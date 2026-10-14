//! The clipboard bridge: copy on one machine, paste on the other, both ways, text and files.
//! One bridge runs per link, on each side, however many monitors a mode will give the remote
//! machine: during the trial two sessions to the same machine replaced each other's offers on
//! its one clipboard, and every paste failed with "the peer aborted the transfer".
//!
//! The transfer engine is gliff's (`gliff-transport`): lazy (nothing moves until an
//! application pastes), typed, chunked, files included. The compositor side is
//! `ext-data-control` through `hypr-input`, on both machines since Navette has no GTK. This
//! file adapts gliff's `crates/gliff-server/src/clipboard.rs`, so it keeps its notice:
//!
//! > MIT License. Copyright (c) 2026 Kevin McConnell
//! >
//! > Permission is hereby granted, free of charge, to any person obtaining a copy of this
//! > software and associated documentation files (the "Software"), to deal in the Software
//! > without restriction, including without limitation the rights to use, copy, modify, merge,
//! > publish, distribute, sublicense, and/or sell copies of the Software, and to permit
//! > persons to whom the Software is furnished to do so, subject to the following conditions:
//! > the above copyright notice and this permission notice shall be included in all copies or
//! > substantial portions of the Software. The Software is provided "as is", without warranty
//! > of any kind.
//!
//! The home machine is gliff's client side of the engine (odd request ids) and the remote
//! machine its server side (even ones), so two transfers in opposite directions never share an
//! id. Everything here runs on the link's thread, in a `LocalSet` of the bridge's own, so the
//! end of the bridge ends every transfer and paste it started; the compositor calls that block
//! run on other threads, so they never hold up the link's heartbeats.

pub mod files;
pub mod progress;

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gliff_proto::clipboard::{
    MAX_ITEM, file_list_body, forwardable_mimes, is_file_mime, local_mimes_for_offer,
    parse_uri_list, resolve_mime, top_level,
};
use gliff_proto::{ClipboardFile, ClipboardItem, ClipboardMsg};
use gliff_transport::clipboard::files::{LocalFiles, write_body};
use gliff_transport::clipboard::progress::{
    Jobs, Meter, Progress, State, describe_files, describe_mime,
};
use gliff_transport::clipboard::{Event, ReadSource, Side, TransferError, Transfers, WriteSink};
use hypr_input::{Clipboard, ClipboardEvent, Target};
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe;
use tokio::sync::OnceCell;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::LocalSet;

use crate::link::Wire;
use crate::notify::Notifier;
use crate::wire::{MAX_MIME_LEN, MAX_MIME_TYPES};
use files::{Inbox, Staging, human_bytes};
use progress::{Crossing, Served, Shown, Stop};

/// Longest `text/uri-list` read to build an offer.
const URI_LIST_MAX: usize = 1024 * 1024;

/// How long to wait before looking for the compositor again when it is not there, or went.
const COMPOSITOR_RETRY: Duration = Duration::from_secs(5);

/// The machine's clipboard, as the bridge needs it (a port: tests use a fake). Starting it,
/// [`Compositor::receive`] and dropping it may block: the bridge does them on other threads.
pub trait Compositor: Send + Sync + 'static {
    /// Become the selection, offering these mime types; each paste arrives as
    /// [`Selected::Paste`]. An empty list withdraws our selection. Must not block.
    fn offer(&self, mime_types: Vec<String>);
    /// A pipe that yields the current selection in `mime_type`, then EOF.
    fn receive(&self, mime_type: String) -> io::Result<OwnedFd>;
}

/// The bridge's compositor. Its last owner on the link's thread drops it on a thread of its
/// own, since dropping the real one joins its Wayland thread.
struct OffThread<C: Compositor>(Option<Arc<C>>);

impl<C: Compositor> OffThread<C> {
    fn get(&self) -> &Arc<C> {
        self.0.as_ref().expect("only taken on drop")
    }
}

impl<C: Compositor> Drop for OffThread<C> {
    fn drop(&mut self) {
        if let Some(compositor) = self.0.take() {
            std::thread::spawn(move || drop(compositor));
        }
    }
}

/// [`Compositor::receive`], on a blocking thread.
async fn receive<C: Compositor>(
    compositor: Arc<C>,
    mime_type: String,
) -> io::Result<pipe::Receiver> {
    tokio::task::spawn_blocking(move || compositor.receive(mime_type))
        .await
        .map_err(io::Error::other)?
        .and_then(pipe::Receiver::from_owned_fd)
}

/// What the compositor reports.
#[derive(Debug)]
pub enum Selected {
    /// The selection changed to something offering these mime types.
    Selection { mime_types: Vec<String> },
    /// An application pastes our offer in `mime_type`: write the item to `fd`, then close it.
    Paste { mime_type: String, fd: OwnedFd },
}

#[derive(Default)]
struct LocalOffer {
    /// Serial of the offer this selection was (or will be) sent as; a request against a
    /// replaced offer is refused.
    serial: u32,
    /// Mime types the compositor's current selection advertises.
    mime_types: Vec<String>,
    files: LocalFiles,
    /// The links and special files the copy left out; named once, at the first paste.
    skipped: Vec<String>,
}

#[derive(Default)]
struct RemoteOffer {
    serial: u32,
    files: Vec<ClipboardFile>,
    /// Where the files are once pasted; filled at most once per offer, so a second paste of
    /// the same copy points at the same files.
    received: Rc<OnceCell<Vec<PathBuf>>>,
}

/// The two ends of one link's clipboard: the peer's messages in, the compositor's events in.
pub struct Bridge<C: Compositor> {
    transfers: Transfers,
    jobs: Jobs,
    compositor: OffThread<C>,
    /// Where the files of a peer's offer go when pasted; none without a home folder.
    inbox: Option<Inbox>,
    local: Rc<RefCell<LocalOffer>>,
    remote: Rc<RefCell<RemoteOffer>>,
    /// Bumped per selection so a slow offer build for a stale one is dropped. It is the
    /// link's, not the bridge's: a later bridge on the same link never reuses a serial the
    /// peer may still request.
    selection_gen: Rc<Cell<u64>>,
    progress: progress::Progress,
    notifier: Arc<dyn Notifier>,
    /// The number of the last paste of files on this machine.
    pastes: Cell<u32>,
}

impl<C: Compositor> Bridge<C> {
    pub fn new(
        transfers: Transfers,
        jobs: Jobs,
        compositor: Arc<C>,
        inbox: Option<Inbox>,
        serials: Rc<Cell<u64>>,
        shown: &Shown,
    ) -> Self {
        Self {
            transfers,
            jobs,
            compositor: OffThread(Some(compositor)),
            inbox,
            local: Rc::default(),
            remote: Rc::default(),
            selection_gen: serials,
            progress: progress::Progress::new(shown.bar.clone()),
            notifier: shown.notifier.clone(),
            pastes: Cell::new(0),
        }
    }

    /// Stops every file crossing, both ways.
    pub fn cancel(&self) {
        if !self.progress.cancel().is_empty() {
            self.notifier.show("Paste cancelled", "");
        }
    }

    /// A message from the peer.
    pub fn on_peer_msg(&self, msg: ClipboardMsg, payload: Bytes) {
        match self.transfers.on_msg(msg, payload) {
            Some(Event::Offer {
                serial,
                mime_types,
                files,
            }) => self.on_remote_offer(serial, mime_types, files),
            Some(Event::Request { id, serial, item }) => self.on_request(id, serial, item),
            None => {}
        }
    }

    /// An event from the compositor's clipboard.
    pub fn on_compositor_event(&self, event: Selected) {
        match event {
            Selected::Selection { mime_types } => self.on_selection(mime_types),
            Selected::Paste { mime_type, fd } => self.on_paste(mime_type, fd),
        }
    }

    fn on_remote_offer(&self, serial: u32, mime_types: Vec<String>, files: Vec<ClipboardFile>) {
        let advertise = local_mimes_for_offer(&mime_types, !files.is_empty());
        *self.remote.borrow_mut() = RemoteOffer {
            serial,
            files,
            ..RemoteOffer::default()
        };
        self.compositor.get().offer(advertise);
    }

    fn on_request(&self, id: u32, serial: u32, item: ClipboardItem) {
        if serial != self.local.borrow().serial {
            // The selection moved on since the peer saw the offer: refuse, so its paste fails
            // on its own, rather than serve data it did not ask for.
            self.transfers.refuse(id);
            return;
        }
        match item {
            ClipboardItem::Mime(requested) => {
                let local = self.local.borrow();
                let Some(actual) = resolve_mime(&requested, &local.mime_types) else {
                    self.transfers.refuse(id);
                    return;
                };
                let (compositor, actual) = (self.compositor.get().clone(), actual.to_string());
                let transfers = self.transfers.clone();
                tokio::task::spawn_local(async move {
                    match receive(compositor, actual).await {
                        Ok(rx) => transfers.serve(id, ReadSource(rx)),
                        Err(e) => {
                            log(format_args!("cannot read the selection: {e}"));
                            transfers.refuse(id);
                        }
                    }
                });
            }
            ClipboardItem::File(_) => {
                let (path, total, skipped) = {
                    let mut local = self.local.borrow_mut();
                    let Some(path) = local.files.path_for(&item).map(PathBuf::from) else {
                        self.transfers.refuse(id);
                        return;
                    };
                    let total = local.files.entries.iter().map(|f| f.size).sum();
                    (path, total, std::mem::take(&mut local.skipped))
                };
                if !skipped.is_empty() {
                    self.notifier
                        .show("Not sent: links and special files", &skipped.join(", "));
                }
                let (done, stopped, run) = self.progress.serving(serial, total);
                let crossing = Crossing::Serve(serial);
                match files::open_copied(&path) {
                    Ok(file) => self.transfers.serve(
                        id,
                        Served {
                            source: ReadSource(file),
                            done,
                            stopped,
                            progress: self.progress.clone(),
                            crossing,
                            run,
                            total,
                            finished: false,
                        },
                    ),
                    Err(e) => {
                        log(format_args!("cannot open a copied file: {e}"));
                        self.progress.end(crossing, run);
                        self.transfers.refuse(id);
                    }
                }
            }
        }
    }

    fn on_selection(&self, mime_types: Vec<String>) {
        let generation = self.selection_gen.get() + 1;
        self.selection_gen.set(generation);
        {
            let mut local = self.local.borrow_mut();
            local.serial = generation as u32;
            local.mime_types = mime_types.clone();
            local.files = LocalFiles::default();
            local.skipped = Vec::new();
        }
        let mut offered = forwardable_mimes(&mime_types);
        // What the peer would refuse; the text types come first, so text survives.
        offered.retain(|m| m.len() <= MAX_MIME_LEN);
        offered.truncate(MAX_MIME_TYPES);
        // Read the file list as whichever file-list type the selection advertises; the parser
        // handles both bodies.
        let uri_list = mime_types.iter().find(|m| is_file_mime(m)).cloned();
        let compositor = self.compositor.get().clone();
        let transfers = self.transfers.clone();
        let local = self.local.clone();
        let selection_gen = self.selection_gen.clone();
        tokio::task::spawn_local(async move {
            let copied = match uri_list {
                Some(mime) => match receive(compositor, mime).await {
                    Ok(rx) => local_files_from_pipe(rx).await,
                    Err(e) => {
                        log(format_args!("cannot read the copied file list: {e}"));
                        files::Copied::default()
                    }
                },
                None => files::Copied::default(),
            };
            if selection_gen.get() != generation {
                return; // superseded while we were listing
            }
            let entries = copied.files.entries.clone();
            {
                let mut local = local.borrow_mut();
                local.files = copied.files;
                local.skipped = copied.skipped;
            }
            let _ = transfers
                .send(ClipboardMsg::Offer {
                    serial: generation as u32,
                    mime_types: offered,
                    files: entries,
                })
                .await;
        });
    }

    fn on_paste(&self, mime_type: String, fd: OwnedFd) {
        let target = match pipe::Sender::from_owned_fd(fd) {
            Ok(tx) => tx,
            Err(e) => {
                log(format_args!("a paste target is not a pipe: {e}"));
                return;
            }
        };
        let transfers = self.transfers.clone();
        if is_file_mime(&mime_type) {
            let (serial, files, received) = {
                let remote = self.remote.borrow();
                if remote.files.is_empty() {
                    return;
                }
                (remote.serial, remote.files.clone(), remote.received.clone())
            };
            let Some(inbox) = self.inbox.clone() else {
                log(format_args!("no folder for pasted files: $HOME is not set"));
                return;
            };
            let (label, total) = describe_files(&files);
            let total = total.unwrap_or(0);
            let paste = Crossing::Paste(self.pastes.get().wrapping_add(1));
            self.pastes.set(self.pastes.get().wrapping_add(1));
            let done = Meter::default();
            let (meter, progress, notifier) =
                (done.clone(), self.progress.clone(), self.notifier.clone());
            // Filled before the task first runs: nothing runs before this call returns.
            let run = Rc::new(Cell::new(0));
            let its_run = run.clone();
            let task = tokio::task::spawn_local(async move {
                let mut fetched = false;
                let result = received
                    .get_or_try_init(|| {
                        fetched = true;
                        receive_files(&transfers, &files, serial, &inbox, &meter)
                    })
                    .await;
                progress.end(paste, its_run.get());
                match result {
                    Ok(paths) => {
                        if fetched {
                            notifier.show(
                                "Files pasted",
                                &format!(
                                    "{label}, {}, in {}",
                                    human_bytes(total),
                                    inbox.dir.display()
                                ),
                            );
                        }
                        let body = file_list_body(&mime_type, paths).unwrap_or_default();
                        // An application that closed its end early is not the paste's failure.
                        let _ = write_body(WriteSink(target), body).await;
                    }
                    Err(e) => {
                        log(format_args!("a paste of files failed: {e}"));
                        notifier.show("Paste failed", &why(&e));
                    }
                }
            });
            run.set(
                self.progress
                    .start(paste, total, done, Stop::Task(task.abort_handle())),
            );
        } else {
            let serial = self.remote.borrow().serial;
            self.jobs
                .run(describe_mime(&mime_type), None, |meter| async move {
                    transfers
                        .fetch(
                            ClipboardItem::Mime(mime_type),
                            serial,
                            meter.wrap(WriteSink(target)),
                            Some(MAX_ITEM),
                        )
                        .await
                        .map(|_| ())
                });
        }
    }
}

/// Fetches every entry of the peer's offer into a staging folder of the inbox, then moves the
/// top-level ones into the inbox; their paths there. Nothing is written when the offer is
/// refused or the disk has no room for it, and nothing stays when a fetch fails or the paste is
/// dropped.
async fn receive_files(
    transfers: &Transfers,
    files: &[ClipboardFile],
    serial: u32,
    inbox: &Inbox,
    meter: &Meter,
) -> Result<Vec<PathBuf>, TransferError> {
    files::validate(files).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    inbox.check_room(files.iter().map(|f| f.size).sum())?;
    let staging = Staging::create_in(inbox)?;
    for (i, f) in files.iter().enumerate() {
        let dest = staging.path_of(&f.path)?;
        if f.dir {
            tokio::fs::create_dir_all(&dest).await?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
            .await?;
        let got = transfers
            .fetch(
                ClipboardItem::File(i as u32),
                serial,
                meter.wrap(WriteSink(file)),
                Some(f.size),
            )
            .await?;
        if got != f.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: got {got} of {} bytes", f.path, f.size),
            )
            .into());
        }
    }
    Ok(staging.finish(top_level(files).map(|f| f.path.as_str()))?)
}

/// What the user reads of a paste that failed.
fn why(e: &TransferError) -> String {
    match e {
        TransferError::Aborted => "stopped on the other machine".into(),
        e => e.to_string(),
    }
}

/// Read the compositor's `text/uri-list` and list the files it names.
async fn local_files_from_pipe(mut rx: pipe::Receiver) -> files::Copied {
    let mut body = Vec::new();
    let read = tokio::time::timeout(
        Duration::from_secs(5),
        (&mut rx)
            .take(URI_LIST_MAX as u64 + 1)
            .read_to_end(&mut body),
    )
    .await;
    if !matches!(read, Ok(Ok(_))) || body.len() > URI_LIST_MAX {
        log(format_args!(
            "the copied file list is unreadable or too large"
        ));
        return files::Copied::default();
    }
    let paths = parse_uri_list(&String::from_utf8_lossy(&body));
    if paths.is_empty() {
        return files::Copied::default();
    }
    match tokio::task::spawn_blocking(move || files::list(&paths)).await {
        Ok(Ok(copied)) => copied,
        Ok(Err(e)) => {
            log(format_args!("the copied files are not offered: {e}"));
            files::Copied::default()
        }
        Err(_) => files::Copied::default(),
    }
}

/// Runs one link's bridge until the link's clipboard channel closes (`true`) or the compositor
/// side does (`false`). Takes the wire by reference so a new compositor connection can pick
/// the same link up again; `serials` is the link's. Whatever the bridge started (pastes and
/// transfers, their half-written files) ends with it, even when this future is dropped.
pub async fn run<C: Compositor>(
    side: Side,
    wire: &mut Wire,
    compositor: Arc<C>,
    mut events: UnboundedReceiver<Selected>,
    inbox: Option<Inbox>,
    serials: Rc<Cell<u64>>,
    shown: Shown,
) -> bool {
    // Every task the bridge and gliff's engine spawn lands in this set, and dies with it.
    let tasks = LocalSet::new();
    tasks
        .run_until(async {
            let bridge = Bridge::new(
                Transfers::new(wire.to_peer.clone(), side),
                Jobs::new(report),
                compositor,
                inbox,
                serials,
                &shown,
            );
            let mut cancel = shown.cancel.clone();
            // A click before this bridge stops nothing of it.
            cancel.borrow_and_update();
            let mut cancel_open = true;
            loop {
                tokio::select! {
                    changed = cancel.changed(), if cancel_open => match changed {
                        Ok(()) => bridge.cancel(),
                        Err(_) => cancel_open = false,
                    },
                    clip = wire.from_peer.recv() => match clip {
                        Some((msg, payload)) => bridge.on_peer_msg(msg, payload),
                        None => return true,
                    },
                    event = events.recv() => match event {
                        Some(event) => bridge.on_compositor_event(event),
                        None => return false,
                    },
                }
            }
        })
        .await
}

/// Only what the user must know: a paste of text or a picture that failed. It names no
/// content; a paste of files says more, in a notification.
fn report(_job: u32, progress: Progress) {
    if let State::Failed(why) = progress.state {
        log(format_args!("a paste failed: {why}"));
    }
}

fn log(message: std::fmt::Arguments) {
    eprintln!("navette: clipboard: {message}");
}

/// The Wayland clipboard of the machine, through `ext-data-control`.
pub struct Hypr(Clipboard);

impl Compositor for Hypr {
    fn offer(&self, mime_types: Vec<String>) {
        self.0.offer(mime_types);
    }

    fn receive(&self, mime_type: String) -> io::Result<OwnedFd> {
        self.0.receive(mime_type).map_err(io::Error::other)
    }
}

/// Starts [`Hypr`] on `target`'s compositor, reporting its events to the bridge. Blocks until
/// the compositor answers.
pub fn hypr(
    target: Target,
) -> impl Fn(UnboundedSender<Selected>) -> io::Result<Hypr> + Clone + Send + 'static {
    move |events| {
        let sink = Box::new(move |event| {
            let _ = events.send(match event {
                ClipboardEvent::Selection { mime_types } => Selected::Selection { mime_types },
                ClipboardEvent::Paste { mime_type, fd } => Selected::Paste { mime_type, fd },
            });
        });
        Clipboard::start(target.clone(), sink)
            .map(Hypr)
            .map_err(io::Error::other)
    }
}

/// The service of a link on this machine: the clipboard bridge on the compositor that `start`
/// connects to (on a blocking thread). It looks for the compositor again when it is not there
/// yet or went away, since the agent can start before the session does; the link stays up
/// meanwhile, and the peer's messages are drained: its requests refused, its offers dropped.
/// Needs a `LocalSet`.
pub async fn attach<C, S>(side: Side, start: S, mut wire: Wire, shown: Shown)
where
    C: Compositor,
    S: Fn(UnboundedSender<Selected>) -> io::Result<C> + Clone + Send + 'static,
{
    let serials = Rc::new(Cell::new(0));
    let inbox = Inbox::of_this_user();
    let mut warned = false;
    loop {
        let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let start = start.clone();
        let starting = async {
            tokio::task::spawn_blocking(move || start(events_tx))
                .await
                .map_err(io::Error::other)?
        };
        let Some(started) = draining(&mut wire, starting).await else {
            return;
        };
        match started {
            Ok(compositor) => {
                warned = false;
                let compositor = Arc::new(compositor);
                if run(
                    side,
                    &mut wire,
                    compositor,
                    events,
                    inbox.clone(),
                    serials.clone(),
                    shown.clone(),
                )
                .await
                {
                    return;
                }
                log(format_args!("the compositor's clipboard went away"));
            }
            Err(e) => {
                if !std::mem::replace(&mut warned, true) {
                    log(format_args!("no compositor clipboard yet: {e}"));
                }
            }
        }
        if draining(&mut wire, tokio::time::sleep(COMPOSITOR_RETRY))
            .await
            .is_none()
        {
            return;
        }
    }
}

/// Runs `work` while no bridge reads the wire: the peer's requests are refused, so its pastes
/// fail at once, and the rest is dropped. `None` when the link ended first.
async fn draining<T>(wire: &mut Wire, work: impl Future<Output = T>) -> Option<T> {
    tokio::pin!(work);
    loop {
        tokio::select! {
            done = &mut work => return Some(done),
            clip = wire.from_peer.recv() => match clip {
                Some((ClipboardMsg::Request { id, .. }, _)) => {
                    let _ = wire.to_peer.send((ClipboardMsg::Abort { id }, Bytes::new())).await;
                }
                Some(_) => {}
                None => return None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::sync::Mutex;

    use gliff_proto::ClipboardMsg;
    use gliff_proto::clipboard::CHUNK;
    use gliff_transport::clipboard::outbound_channel;
    use tokio::net::unix::pipe;
    use tokio::sync::mpsc;
    use tokio::task::LocalSet;

    use super::*;
    use crate::clipboard::progress::Bar;
    use crate::link::{self, Heartbeat};
    use crate::notify;
    use crate::wire::LinkMsg;

    const TEXT: &str = "text/plain;charset=utf-8";
    const URI_LIST: &str = "text/uri-list";

    /// What a machine showed of the files crossing, and its cancel.
    #[derive(Clone)]
    struct Seen {
        bars: Arc<Mutex<Vec<Option<Bar>>>>,
        notes: Arc<notify::Fake>,
        cancel: Arc<tokio::sync::watch::Sender<u64>>,
    }

    impl Seen {
        fn new() -> Self {
            Self {
                bars: Arc::default(),
                notes: Arc::default(),
                cancel: Arc::new(tokio::sync::watch::channel(0).0),
            }
        }

        fn shown(&self) -> Shown {
            let bars = self.bars.clone();
            Shown {
                bar: Arc::new(move |bar| bars.lock().unwrap().push(bar)),
                notifier: self.notes.clone(),
                cancel: self.cancel.subscribe(),
            }
        }

        /// The user clicks the bar.
        fn click(&self) {
            self.cancel.send_modify(|n| *n += 1);
        }

        fn bars(&self) -> Vec<Option<Bar>> {
            self.bars.lock().unwrap().clone()
        }

        /// Waits for a notification whose summary starts with `start`.
        async fn told(&self, start: &str) -> (String, String) {
            within(start, async {
                loop {
                    let shown = self.notes.shown();
                    if let Some(note) = shown.into_iter().find(|(s, _)| s.starts_with(start)) {
                        return note;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
        }
    }

    fn quiet() -> Shown {
        Seen::new().shown()
    }

    fn heartbeat() -> Heartbeat {
        Heartbeat {
            interval: Duration::from_millis(20),
            timeout: Duration::from_secs(5),
        }
    }

    async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), fut)
            .await
            .unwrap_or_else(|_| panic!("timed out: {what}"))
    }

    /// A machine's clipboard: what is selected on it, and every offer the bridge made on it.
    #[derive(Default)]
    struct Fake {
        selected: Mutex<HashMap<String, Vec<u8>>>,
        offered: Mutex<Vec<Vec<String>>>,
        /// How long `receive` blocks its caller, like a compositor that answers late.
        slow: Mutex<Duration>,
        /// When set, the application that owns the selection writes it and never closes its
        /// end, which is handed over here: a transfer that stays in flight.
        stalled: Mutex<Option<std::sync::mpsc::Sender<std::io::PipeWriter>>>,
    }

    impl Compositor for Fake {
        fn offer(&self, mime_types: Vec<String>) {
            self.offered.lock().unwrap().push(mime_types);
        }

        fn receive(&self, mime_type: String) -> io::Result<OwnedFd> {
            std::thread::sleep(*self.slow.lock().unwrap());
            let data = self
                .selected
                .lock()
                .unwrap()
                .get(&mime_type)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not selected"))?;
            let (reader, mut writer) = std::io::pipe()?;
            let stalled = self.stalled.lock().unwrap().clone();
            // A pipe holds 64 KiB: the application writing into it is another thread.
            std::thread::spawn(move || {
                let _ = writer.write_all(&data);
                if let Some(stalled) = stalled {
                    let _ = stalled.send(writer);
                }
            });
            Ok(reader.into())
        }
    }

    impl Fake {
        fn select(&self, items: &[(&str, &[u8])]) -> Vec<String> {
            *self.selected.lock().unwrap() = items
                .iter()
                .map(|(mime, data)| (mime.to_string(), data.to_vec()))
                .collect();
            items.iter().map(|(mime, _)| mime.to_string()).collect()
        }
    }

    /// One machine of a test desk: its clipboard and the way to act on it.
    struct Machine {
        clipboard: Arc<Fake>,
        events: mpsc::UnboundedSender<Selected>,
        seen: Seen,
        /// Its display's end of the link, once it is up.
        display: tokio::sync::Mutex<mpsc::UnboundedReceiver<crate::display::wire::Peer>>,
        /// Its end of the link: aborting it drops the link.
        link: tokio::task::JoinHandle<()>,
    }

    impl Machine {
        /// The user copies.
        fn copy(&self, items: &[(&str, &[u8])]) {
            let mime_types = self.clipboard.select(items);
            self.events
                .send(Selected::Selection { mime_types })
                .unwrap();
        }

        /// An application pastes `mime_type`; what it reads.
        async fn paste(&self, mime_type: &str) -> Vec<u8> {
            self.paste_for(mime_type, Duration::from_secs(10)).await
        }

        /// The same, within `limit`.
        async fn paste_for(&self, mime_type: &str, limit: Duration) -> Vec<u8> {
            let (reader, writer) = std::io::pipe().unwrap();
            self.events
                .send(Selected::Paste {
                    mime_type: mime_type.into(),
                    fd: writer.into(),
                })
                .unwrap();
            let mut reader = pipe::Receiver::from_owned_fd(reader.into()).unwrap();
            let mut data = Vec::new();
            tokio::time::timeout(limit, reader.read_to_end(&mut data))
                .await
                .expect("the paste in time")
                .unwrap();
            data
        }

        /// The bridge has offered the peer's copy on this machine `count` times.
        async fn offered(&self, count: usize) -> Vec<String> {
            within("the offer", async {
                while self.clipboard.offered.lock().unwrap().len() < count {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            self.clipboard.offered.lock().unwrap()[count - 1].clone()
        }
    }

    /// Home and the remote machine, each with its bridge, over an in-memory link. Spawns on
    /// the current `LocalSet`.
    /// Each machine's receiving folder is `home` or `remote` in `folders`.
    fn desk(folders: &std::path::Path) -> (Machine, Machine) {
        let (home_end, remote_end) = tokio::io::duplex(64 * 1024);
        let home = machine(Side::Client, home_end, Inbox::new(folders.join("home")));
        let remote = machine(Side::Server, remote_end, Inbox::new(folders.join("remote")));
        (home, remote)
    }

    fn machine(side: Side, stream: tokio::io::DuplexStream, inbox: Inbox) -> Machine {
        machine_on(side, inbox, |service| {
            tokio::task::spawn_local(async move {
                link::run(stream, heartbeat(), move |wire| service(wire)).await;
            })
        })
    }

    /// The service a machine runs on each connection of its link: its clipboard bridge, its
    /// display's end handed out.
    type Service = Rc<dyn Fn(Wire) -> std::pin::Pin<Box<dyn Future<Output = ()>>>>;

    /// A machine whose link `link` starts with the service it runs.
    fn machine_on(
        side: Side,
        inbox: Inbox,
        link: impl FnOnce(Service) -> tokio::task::JoinHandle<()>,
    ) -> Machine {
        let clipboard = Arc::new(Fake::default());
        let (events, selected) = mpsc::unbounded_channel();
        let selected = Rc::new(RefCell::new(Some(selected)));
        let (displays, display) = mpsc::unbounded_channel();
        let seen = Seen::new();
        let (bridge, shown) = (clipboard.clone(), seen.shown());
        let service: Service = Rc::new(move |mut wire: Wire| {
            let _ = displays.send(wire.display.take().unwrap());
            let selected = selected.borrow_mut().take().expect("one connection");
            let (bridge, inbox, shown) = (bridge.clone(), inbox.clone(), shown.clone());
            Box::pin(async move {
                let serials = Rc::default();
                let inbox = Some(inbox);
                super::run(side, &mut wire, bridge, selected, inbox, serials, shown).await;
            })
        });
        Machine {
            clipboard,
            events,
            seen,
            display: tokio::sync::Mutex::new(display),
            link: link(service),
        }
    }

    async fn text_crosses(copy_at_home: bool) {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                let (source, target) = if copy_at_home {
                    (&home, &remote)
                } else {
                    (&remote, &home)
                };
                source.copy(&[(TEXT, b"meeting at ten")]);
                let offer = target.offered(1).await;
                assert!(offer.iter().any(|mime| mime == TEXT), "{offer:?}");
                assert_eq!(target.paste(TEXT).await, b"meeting at ten");
            })
            .await;
    }

    #[tokio::test]
    async fn text_copied_at_home_is_pasted_on_the_remote() {
        text_crosses(true).await;
    }

    #[tokio::test]
    async fn text_copied_on_the_remote_is_pasted_at_home() {
        text_crosses(false).await;
    }

    #[tokio::test]
    async fn a_text_larger_than_a_chunk_crosses_in_pieces() {
        let tmp = tempfile::tempdir().unwrap();
        let text: Vec<u8> = (0..700_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                home.copy(&[(TEXT, &text)]);
                remote.offered(1).await;
                assert!(remote.paste(TEXT).await == text);
            })
            .await;
    }

    #[tokio::test]
    async fn a_new_copy_replaces_the_offer_and_the_paste_gets_the_new_one() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                home.copy(&[(TEXT, b"first")]);
                remote.offered(1).await;
                home.copy(&[(TEXT, b"second")]);
                remote.offered(2).await;
                assert_eq!(remote.paste(TEXT).await, b"second");
            })
            .await;
    }

    async fn file_crosses(copy_at_home: bool) {
        let tmp = tempfile::tempdir().unwrap();
        let source_dir = tmp.path().join("copied");
        std::fs::create_dir(&source_dir).unwrap();
        // Larger than a chunk, so the file takes more than one.
        let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(source_dir.join("report.bin"), &content).unwrap();
        let list = format!("file://{}\r\n", source_dir.join("report.bin").display());
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                let (source, target) = if copy_at_home {
                    (&home, &remote)
                } else {
                    (&remote, &home)
                };
                source.copy(&[(URI_LIST, list.as_bytes())]);
                let offer = target.offered(1).await;
                assert!(offer.iter().any(|mime| mime == URI_LIST), "{offer:?}");
                let pasted = String::from_utf8(target.paste(URI_LIST).await).unwrap();
                let paths = parse_uri_list(&pasted);
                assert_eq!(paths.len(), 1, "{pasted:?}");
                let folder = tmp
                    .path()
                    .join(if copy_at_home { "remote" } else { "home" });
                assert_eq!(paths[0], folder.join("report.bin"));
                assert!(std::fs::read(&paths[0]).unwrap() == content);
            })
            .await;
    }

    #[tokio::test]
    async fn a_file_copied_at_home_is_pasted_on_the_remote() {
        file_crosses(true).await;
    }

    #[tokio::test]
    async fn a_file_copied_on_the_remote_is_pasted_at_home() {
        file_crosses(false).await;
    }

    #[tokio::test]
    async fn a_request_against_a_replaced_offer_is_refused_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (out, mut sent) = outbound_channel();
                let clipboard = Arc::new(Fake::default());
                let bridge = Bridge::new(
                    Transfers::new(out, Side::Client),
                    Jobs::new(|_, _| {}),
                    clipboard.clone(),
                    Some(Inbox::new(tmp.path().into())),
                    Rc::default(),
                    &quiet(),
                );
                let mut next = async || within("a message", sent.recv()).await.unwrap();

                let mime_types = clipboard.select(&[(TEXT, b"old")]);
                bridge.on_compositor_event(Selected::Selection { mime_types });
                let (ClipboardMsg::Offer { serial: old, .. }, _) = next().await else {
                    panic!("an offer first");
                };
                let mime_types = clipboard.select(&[(TEXT, b"new")]);
                bridge.on_compositor_event(Selected::Selection { mime_types });
                let (ClipboardMsg::Offer { serial: new, .. }, _) = next().await else {
                    panic!("the second offer");
                };
                assert_ne!(old, new);

                // The peer asks for the offer it saw, which the new copy replaced.
                let item = ClipboardItem::Mime(TEXT.into());
                bridge.on_peer_msg(
                    ClipboardMsg::Request {
                        id: 2,
                        serial: old,
                        item: item.clone(),
                    },
                    Bytes::new(),
                );
                assert_eq!(next().await.0, ClipboardMsg::Abort { id: 2 });

                // The current offer is still served.
                bridge.on_peer_msg(
                    ClipboardMsg::Request {
                        id: 4,
                        serial: new,
                        item,
                    },
                    Bytes::new(),
                );
                let (msg, payload) = next().await;
                assert!(matches!(msg, ClipboardMsg::Data { id: 4, .. }), "{msg:?}");
                assert_eq!(payload, "new");
            })
            .await;
    }

    #[tokio::test]
    async fn a_paste_against_a_refusing_peer_fails_instead_of_hanging() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                home.copy(&[(TEXT, b"gone")]);
                remote.offered(1).await;
                // The data is no longer there when the paste asks: the peer refuses, and the
                // application reads nothing, then EOF.
                home.clipboard.selected.lock().unwrap().clear();
                assert!(remote.paste(TEXT).await.is_empty());
            })
            .await;
    }

    /// Records when a bridge starts and when it stops.
    struct Live(Rc<RefCell<Vec<&'static str>>>);

    impl Live {
        fn start(log: &Rc<RefCell<Vec<&'static str>>>) -> Self {
            log.borrow_mut().push("start");
            Self(log.clone())
        }
    }

    impl Drop for Live {
        fn drop(&mut self) {
            self.0.borrow_mut().push("stop");
        }
    }

    #[tokio::test]
    async fn a_new_connection_ends_the_old_bridge_before_the_new_one_starts() {
        LocalSet::new()
            .run_until(async {
                let log = Rc::new(RefCell::new(Vec::new()));
                let (connections, queue) = mpsc::unbounded_channel();
                let (events, _log) = mpsc::unbounded_channel();
                let service_log = log.clone();
                tokio::task::spawn_local(link::serve(
                    link::tests::Queue(queue),
                    heartbeat(),
                    events,
                    move |_wire| {
                        let live = Live::start(&service_log);
                        async move {
                            let _live = live;
                            std::future::pending::<()>().await;
                        }
                    },
                ));
                let mut remotes = Vec::new();
                for _ in 0..2 {
                    let (home_end, mut remote_end) = tokio::io::duplex(1024);
                    connections.send(home_end).unwrap();
                    link::hello(&mut remote_end, heartbeat()).await.unwrap();
                    remotes.push(tokio::task::spawn_local(link::run(
                        remote_end,
                        heartbeat(),
                        async |_| {},
                    )));
                    within("the bridge of this connection", async {
                        while log.borrow().iter().filter(|e| **e == "start").count() < remotes.len()
                        {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await;
                }
                // Never two bridges alive at once on the machine.
                assert_eq!(*log.borrow(), ["start", "stop", "start"]);
            })
            .await;
    }

    #[tokio::test]
    async fn the_link_stays_up_while_the_compositor_clipboard_is_missing() {
        LocalSet::new()
            .run_until(async {
                let (home_end, remote_end) = tokio::io::duplex(1024);
                let missing = Target {
                    display: Some("/nonexistent/navette-test-socket".into()),
                    instance: None,
                };
                let ended = tokio::task::spawn_local(link::run(home_end, heartbeat(), {
                    |wire| super::attach(Side::Client, hypr(missing), wire, quiet())
                }));
                let peer =
                    tokio::task::spawn_local(link::run(remote_end, heartbeat(), async |_| {}));
                tokio::time::sleep(heartbeat().interval * 10).await;
                assert!(!ended.is_finished() && !peer.is_finished());
            })
            .await;
    }

    /// Fast heartbeats, and a peer on a thread of its own: it sees the silence of a link
    /// whose thread is blocked, where a peer on the same thread would be blocked too.
    fn eager() -> Heartbeat {
        Heartbeat {
            interval: Duration::from_millis(20),
            timeout: Duration::from_millis(300),
        }
    }

    fn peer_on_its_own_thread(
        stream: tokio::io::DuplexStream,
    ) -> std::sync::mpsc::Receiver<io::Error> {
        let (ended, end) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _ = ended.send(runtime.block_on(link::run(stream, eager(), async |_| {})));
        });
        end
    }

    /// Asserts the peer kept the link up for `long`.
    async fn the_peer_stays(end: &std::sync::mpsc::Receiver<io::Error>, long: Duration) {
        tokio::time::sleep(long).await;
        if let Ok(e) = end.try_recv() {
            panic!("the link went down: {e}");
        }
    }

    #[tokio::test]
    async fn the_heartbeats_flow_while_the_compositor_is_slow_to_hand_over_the_selection() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home_end, remote_end) = tokio::io::duplex(64 * 1024);
                let end = peer_on_its_own_thread(remote_end);
                let clipboard = Arc::new(Fake::default());
                *clipboard.slow.lock().unwrap() = Duration::from_secs(1);
                let (events, selected) = mpsc::unbounded_channel();
                let bridge = clipboard.clone();
                let inbox = Inbox::new(tmp.path().to_path_buf());
                tokio::task::spawn_local(link::run(home_end, eager(), |mut wire| async move {
                    let serials = Rc::default();
                    super::run(
                        Side::Client,
                        &mut wire,
                        bridge,
                        selected,
                        Some(inbox),
                        serials,
                        quiet(),
                    )
                    .await;
                }));
                let mime_types = clipboard.select(&[(URI_LIST, b"file:///nowhere\r\n")]);
                events.send(Selected::Selection { mime_types }).unwrap();
                the_peer_stays(&end, Duration::from_millis(1500)).await;
            })
            .await;
    }

    #[tokio::test]
    async fn the_heartbeats_flow_while_the_compositor_is_slow_to_start() {
        LocalSet::new()
            .run_until(async {
                let (home_end, remote_end) = tokio::io::duplex(64 * 1024);
                let end = peer_on_its_own_thread(remote_end);
                let slow = |_| -> io::Result<Fake> {
                    std::thread::sleep(Duration::from_secs(1));
                    Err(io::Error::other("no compositor"))
                };
                tokio::task::spawn_local(link::run(home_end, eager(), move |wire| {
                    super::attach(Side::Client, slow, wire, quiet())
                }));
                the_peer_stays(&end, Duration::from_millis(1500)).await;
            })
            .await;
    }

    /// A compositor connection that takes its time to close, like the real one joining its
    /// Wayland thread.
    struct SlowToClose;

    impl Compositor for SlowToClose {
        fn offer(&self, _: Vec<String>) {}

        fn receive(&self, _: String) -> io::Result<OwnedFd> {
            Err(io::ErrorKind::NotFound.into())
        }
    }

    impl Drop for SlowToClose {
        fn drop(&mut self) {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    #[tokio::test]
    async fn the_heartbeats_flow_while_the_compositor_is_slow_to_close() {
        LocalSet::new()
            .run_until(async {
                let (home_end, remote_end) = tokio::io::duplex(64 * 1024);
                let end = peer_on_its_own_thread(remote_end);
                // The compositor goes away as soon as it is there: the bridge closes it.
                let gone = |events| {
                    drop(events);
                    Ok(SlowToClose)
                };
                tokio::task::spawn_local(link::run(home_end, eager(), move |wire| {
                    super::attach(Side::Client, gone, wire, quiet())
                }));
                the_peer_stays(&end, Duration::from_millis(1500)).await;
            })
            .await;
    }

    #[tokio::test]
    async fn a_paste_from_the_peer_is_refused_while_the_compositor_is_missing() {
        LocalSet::new()
            .run_until(async {
                let (home_end, remote_end) = tokio::io::duplex(64 * 1024);
                let missing = |_| -> io::Result<Fake> { Err(io::Error::other("no compositor")) };
                tokio::task::spawn_local(link::run(home_end, heartbeat(), move |wire| {
                    super::attach(Side::Client, missing, wire, quiet())
                }));
                let mut peer = gliff_transport::Framed::new(remote_end);
                let request = ClipboardMsg::Request {
                    id: 2,
                    serial: 1,
                    item: ClipboardItem::Mime(TEXT.into()),
                };
                peer.write_msg(&LinkMsg::from(request)).await.unwrap();
                let refused = async {
                    loop {
                        let msg: LinkMsg = peer.read_msg().await.unwrap();
                        if let LinkMsg::ClipboardAbort { id: 2 } = msg {
                            return;
                        }
                    }
                };
                tokio::time::timeout(Duration::from_secs(2), refused)
                    .await
                    .expect("the request refused at once, not left to time out");
            })
            .await;
    }

    #[tokio::test]
    async fn a_paste_in_flight_ends_when_the_link_drops() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                let (stalled, source) = std::sync::mpsc::channel();
                *home.clipboard.stalled.lock().unwrap() = Some(stalled);
                // One whole chunk, so it crosses before the source stalls.
                home.copy(&[(TEXT, &vec![b'a'; CHUNK])]);
                remote.offered(1).await;
                let (reader, writer) = std::io::pipe().unwrap();
                remote
                    .events
                    .send(Selected::Paste {
                        mime_type: TEXT.into(),
                        fd: writer.into(),
                    })
                    .unwrap();
                let mut reader = pipe::Receiver::from_owned_fd(reader.into()).unwrap();
                let mut first = vec![0u8; CHUNK];
                within("the first chunk", reader.read_exact(&mut first))
                    .await
                    .unwrap();

                home.link.abort();
                // The application that pastes gets its end of file at once, not after gliff's
                // idle timeout, and the one that copied sees nobody reads it any more.
                let mut rest = Vec::new();
                tokio::time::timeout(Duration::from_secs(2), reader.read_to_end(&mut rest))
                    .await
                    .expect("the paste ends with the link")
                    .unwrap();
                let mut source = source.recv().unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert_eq!(
                    source.write_all(b"more").unwrap_err().kind(),
                    io::ErrorKind::BrokenPipe,
                    "the copied side is still read",
                );
            })
            .await;
    }

    /// The files copied: a URI list of `paths`.
    fn uri_list_of(paths: &[std::path::PathBuf]) -> String {
        paths
            .iter()
            .map(|p| format!("file://{}\r\n", p.display()))
            .collect()
    }

    /// Every file and folder under `root`, by its path from `root`, with a file's bytes.
    fn tree(root: &std::path::Path) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
        let mut out = Vec::new();
        let mut todo = vec![root.to_path_buf()];
        while let Some(dir) = todo.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                if path.is_dir() {
                    out.push((rel, None));
                    todo.push(path);
                } else {
                    out.push((rel, Some(std::fs::read(&path).unwrap())));
                }
            }
        }
        out.sort();
        out
    }

    #[tokio::test]
    async fn a_directory_tree_and_an_empty_file_cross_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied");
        let project = copied.join("project");
        std::fs::create_dir_all(project.join("src/empty-dir")).unwrap();
        let big: Vec<u8> = (0..700_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(project.join("src/data.bin"), &big).unwrap();
        std::fs::write(project.join("README"), b"read me").unwrap();
        std::fs::write(copied.join("empty.txt"), b"").unwrap();
        let list = uri_list_of(&[project.clone(), copied.join("empty.txt")]);
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                home.copy(&[(URI_LIST, list.as_bytes())]);
                remote.offered(1).await;
                let pasted = String::from_utf8(remote.paste(URI_LIST).await).unwrap();
                let folder = tmp.path().join("remote");
                assert_eq!(
                    parse_uri_list(&pasted),
                    [folder.join("project"), folder.join("empty.txt")]
                );
                assert_eq!(tree(&folder), tree(&copied));
            })
            .await;
    }

    #[tokio::test]
    async fn a_name_already_in_the_receiving_folder_gets_a_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        std::fs::write(copied.join("report.bin"), b"new").unwrap();
        let folder = tmp.path().join("remote");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("report.bin"), b"mine").unwrap();
        let list = uri_list_of(&[copied.join("report.bin")]);
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                home.copy(&[(URI_LIST, list.as_bytes())]);
                remote.offered(1).await;
                let pasted = String::from_utf8(remote.paste(URI_LIST).await).unwrap();
                assert_eq!(parse_uri_list(&pasted), [folder.join("report (1).bin")]);
                assert_eq!(std::fs::read(folder.join("report.bin")).unwrap(), b"mine");
                assert_eq!(
                    std::fs::read(folder.join("report (1).bin")).unwrap(),
                    b"new"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn a_paste_larger_than_the_free_space_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied.bin");
        std::fs::write(&copied, vec![1; 4096]).unwrap();
        let list = uri_list_of(std::slice::from_ref(&copied));
        let folder = tmp.path().join("remote");
        LocalSet::new()
            .run_until(async {
                let (home_end, remote_end) = tokio::io::duplex(64 * 1024);
                let home = machine(Side::Client, home_end, Inbox::new(tmp.path().join("home")));
                let full = Inbox {
                    dir: folder.clone(),
                    free: |_| Ok(1024),
                };
                let remote = machine(Side::Server, remote_end, full);
                home.copy(&[(URI_LIST, list.as_bytes())]);
                remote.offered(1).await;
                assert!(remote.paste(URI_LIST).await.is_empty());
                assert!(tree(&folder).is_empty(), "{:?}", tree(&folder));
                let (_, why) = remote.seen.told("Paste failed").await;
                assert_eq!(why, "io: 4.0 KiB needed, 1.0 KiB free");
            })
            .await;
    }

    #[tokio::test]
    async fn an_offer_naming_a_path_out_of_the_receiving_folder_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("inbox");
        LocalSet::new()
            .run_until(async {
                let (out, _sent) = outbound_channel();
                let clipboard = Arc::new(Fake::default());
                let bridge = Bridge::new(
                    Transfers::new(out, Side::Client),
                    Jobs::new(|_, _| {}),
                    clipboard.clone(),
                    Some(Inbox::new(folder.clone())),
                    Rc::default(),
                    &quiet(),
                );
                // What the link refuses on arrival, as a peer would send it past that check.
                for path in ["../escaped", "/tmp/escaped"] {
                    let offer = ClipboardMsg::Offer {
                        serial: 1,
                        mime_types: Vec::new(),
                        files: vec![ClipboardFile {
                            path: path.into(),
                            size: 1,
                            dir: false,
                        }],
                    };
                    bridge.on_peer_msg(offer, Bytes::new());
                    let (reader, writer) = std::io::pipe().unwrap();
                    bridge.on_compositor_event(Selected::Paste {
                        mime_type: URI_LIST.into(),
                        fd: writer.into(),
                    });
                    let mut reader = pipe::Receiver::from_owned_fd(reader.into()).unwrap();
                    let mut pasted = Vec::new();
                    within("the paste", reader.read_to_end(&mut pasted))
                        .await
                        .unwrap();
                    assert!(pasted.is_empty());
                }
                assert!(!tmp.path().join("escaped").exists());
                assert!(tree(tmp.path()).iter().all(|(p, _)| p.starts_with("inbox")));
            })
            .await;
    }

    /// Home copies `files` (made under `tmp/copied`), the remote pastes them; what it read.
    async fn paste_on_the_remote(home: &Machine, remote: &Machine, list: &str) -> String {
        home.copy(&[(URI_LIST, list.as_bytes())]);
        remote.offered(1).await;
        String::from_utf8(remote.paste(URI_LIST).await).unwrap()
    }

    /// Waits until the bar is gone after showing `total` bytes.
    async fn the_bar_went(seen: &Seen, total: u64) {
        within("the bar gone", async {
            while seen.bars().last() != Some(&None) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        let bars = seen.bars();
        assert!(
            bars.iter().flatten().all(|bar| bar.total == total),
            "{bars:?}"
        );
        assert!(bars.iter().any(Option::is_some), "{bars:?}");
    }

    #[tokio::test]
    async fn the_bar_shows_the_files_crossing_both_ways_and_goes_when_they_are_done() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("report.bin");
        std::fs::write(&copied, vec![3; 1 << 20]).unwrap();
        let list = uri_list_of(std::slice::from_ref(&copied));
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                paste_on_the_remote(&home, &remote, &list).await;
                // Home serves it, the remote writes it: both count it.
                the_bar_went(&home.seen, 1 << 20).await;
                the_bar_went(&remote.seen, 1 << 20).await;
            })
            .await;
    }

    #[tokio::test]
    async fn a_paste_of_files_says_when_they_are_in() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("report.bin");
        std::fs::write(&copied, b"figures").unwrap();
        let list = uri_list_of(std::slice::from_ref(&copied));
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                paste_on_the_remote(&home, &remote, &list).await;
                let (_, body) = remote.seen.told("Files pasted").await;
                assert!(body.starts_with("report.bin, 7 B, in "), "{body}");
                // A second paste of the same copy points at the same files, and says nothing.
                remote.paste(URI_LIST).await;
                assert_eq!(remote.seen.notes.summaries(), ["Files pasted"]);
            })
            .await;
    }

    #[tokio::test]
    async fn the_links_left_out_of_a_copy_are_named_when_it_is_pasted() {
        let tmp = tempfile::tempdir().unwrap();
        let photos = tmp.path().join("photos");
        std::fs::create_dir(&photos).unwrap();
        std::fs::write(photos.join("beach.jpg"), b"sand").unwrap();
        std::os::unix::fs::symlink("/nowhere", photos.join("shortcut")).unwrap();
        let list = uri_list_of(std::slice::from_ref(&photos));
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                let pasted = paste_on_the_remote(&home, &remote, &list).await;
                assert_eq!(parse_uri_list(&pasted).len(), 1);
                let (_, body) = home.seen.told("Not sent").await;
                assert_eq!(body, "photos/shortcut");
                assert!(!tmp.path().join("remote/photos/shortcut").exists());
            })
            .await;
    }

    async fn next_sent(
        sent: &mut mpsc::Receiver<(ClipboardMsg, Bytes)>,
        wanted: impl Fn(&ClipboardMsg) -> bool,
    ) -> ClipboardMsg {
        within("a message to the peer", async {
            loop {
                let (msg, _) = sent.recv().await.unwrap();
                if wanted(&msg) {
                    return msg;
                }
            }
        })
        .await
    }

    #[tokio::test]
    async fn a_cancel_stops_a_paste_and_leaves_no_half_written_file() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("inbox");
        LocalSet::new()
            .run_until(async {
                let (peer, from_peer) = mpsc::channel(16);
                let (to_peer, mut sent) = outbound_channel();
                let mut wire = Wire {
                    from_peer,
                    to_peer,
                    display: None,
                    audio: None,
                };
                let (events, selected) = mpsc::unbounded_channel();
                let (seen, clipboard) = (Seen::new(), Arc::new(Fake::default()));
                let (shown, bridge, inbox) = (seen.shown(), clipboard.clone(), folder.clone());
                tokio::task::spawn_local(async move {
                    let inbox = Some(Inbox::new(inbox));
                    let serials = Rc::default();
                    super::run(
                        Side::Server,
                        &mut wire,
                        bridge,
                        selected,
                        inbox,
                        serials,
                        shown,
                    )
                    .await;
                });
                let offer = ClipboardMsg::Offer {
                    serial: 1,
                    mime_types: Vec::new(),
                    files: vec![ClipboardFile {
                        path: "big.bin".into(),
                        size: 4 * CHUNK as u64,
                        dir: false,
                    }],
                };
                peer.send((offer, Bytes::new())).await.unwrap();
                within("the offer", async {
                    while clipboard.offered.lock().unwrap().is_empty() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
                let (reader, writer) = std::io::pipe().unwrap();
                events
                    .send(Selected::Paste {
                        mime_type: URI_LIST.into(),
                        fd: writer.into(),
                    })
                    .unwrap();
                let ClipboardMsg::Request { id, .. } =
                    next_sent(&mut sent, |m| matches!(m, ClipboardMsg::Request { .. })).await
                else {
                    unreachable!()
                };
                // One chunk of four is written, then the user clicks the bar.
                let chunk = ClipboardMsg::Data {
                    id,
                    offset: 0,
                    data_len: CHUNK as u32,
                    done: false,
                };
                peer.send((chunk, Bytes::from(vec![5; CHUNK])))
                    .await
                    .unwrap();
                next_sent(&mut sent, |m| matches!(m, ClipboardMsg::Ack { .. })).await;
                assert_eq!(tree(&folder).len(), 2, "the staging folder and its file");
                seen.click();

                assert_eq!(
                    next_sent(&mut sent, |m| matches!(m, ClipboardMsg::Abort { .. })).await,
                    ClipboardMsg::Abort { id },
                    "the peer is told to stop",
                );
                let mut reader = pipe::Receiver::from_owned_fd(reader.into()).unwrap();
                let mut pasted = Vec::new();
                within("the paste", reader.read_to_end(&mut pasted))
                    .await
                    .unwrap();
                assert!(pasted.is_empty());
                assert!(tree(&folder).is_empty(), "{:?}", tree(&folder));
                seen.told("Paste cancelled").await;
                assert_eq!(seen.bars().last(), Some(&None));
            })
            .await;
    }

    #[tokio::test]
    async fn a_cancel_stops_serving_a_paste_of_the_peer() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("big.bin");
        std::fs::write(&copied, vec![9; 16 * CHUNK]).unwrap();
        let list = uri_list_of(std::slice::from_ref(&copied));
        LocalSet::new()
            .run_until(async {
                let (out, mut sent) = outbound_channel();
                let seen = Seen::new();
                let clipboard = Arc::new(Fake::default());
                let bridge = Bridge::new(
                    Transfers::new(out, Side::Client),
                    Jobs::new(|_, _| {}),
                    clipboard.clone(),
                    None,
                    Rc::default(),
                    &seen.shown(),
                );
                let mime_types = clipboard.select(&[(URI_LIST, list.as_bytes())]);
                bridge.on_compositor_event(Selected::Selection { mime_types });
                let ClipboardMsg::Offer { serial, .. } =
                    next_sent(&mut sent, |m| matches!(m, ClipboardMsg::Offer { .. })).await
                else {
                    unreachable!()
                };
                let request = ClipboardMsg::Request {
                    id: 2,
                    serial,
                    item: ClipboardItem::File(0),
                };
                bridge.on_peer_msg(request, Bytes::new());
                // The window's chunks go, then the source waits for the peer's acks.
                for _ in 0..4 {
                    next_sent(&mut sent, |m| matches!(m, ClipboardMsg::Data { .. })).await;
                }
                within("the bar", async {
                    while !seen.bars().iter().flatten().any(|bar| bar.done > 0) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
                bridge.cancel();
                let ack = ClipboardMsg::Ack {
                    id: 2,
                    received: CHUNK as u64,
                };
                bridge.on_peer_msg(ack, Bytes::new());
                assert_eq!(
                    next_sent(&mut sent, |m| !matches!(m, ClipboardMsg::Data { .. })).await,
                    ClipboardMsg::Abort { id: 2 },
                );
                assert_eq!(seen.bars().last(), Some(&None));
                // Nothing this machine pasted was stopped: it says nothing.
                assert!(seen.notes.shown().is_empty());
            })
            .await;
    }

    /// Two ends joined by a line of about 500 Mbit/s each way, like a home network.
    fn lan() -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
        let (home, home_line) = tokio::io::duplex(64 * 1024);
        let (remote_line, remote) = tokio::io::duplex(64 * 1024);
        let (home_read, home_write) = tokio::io::split(home_line);
        let (remote_read, remote_write) = tokio::io::split(remote_line);
        tokio::task::spawn_local(carry(home_read, remote_write));
        tokio::task::spawn_local(carry(remote_read, home_write));
        (home, remote)
    }

    /// At most 64 KiB a millisecond.
    async fn carry(
        mut from: impl tokio::io::AsyncRead + Unpin,
        mut to: impl tokio::io::AsyncWrite + Unpin,
    ) {
        use tokio::io::AsyncWriteExt;
        let mut tick = tokio::time::interval(Duration::from_millis(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut buf = vec![0; 64 * 1024];
        loop {
            tick.tick().await;
            match from.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if to.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// What a paste of files did to the picture.
    struct Measured {
        /// From the paste to its last byte in the receiving folder.
        elapsed: Duration,
        /// Each frame's delay from the remote's queue to home's display, while it ran.
        delays: Vec<Duration>,
        pasted: std::path::PathBuf,
    }

    impl Measured {
        /// The delay below which `part` of the frames came.
        fn delay(&self, part: f64) -> Duration {
            let mut delays = self.delays.clone();
            delays.sort();
            delays[((delays.len() - 1) as f64 * part) as usize]
        }
    }

    /// The remote copies `copied` and home pastes it, while the remote streams a 60 kB frame
    /// every 16 ms, as its encoder would: one waiting at most.
    async fn frames_during_a_paste(
        home: &Machine,
        remote: &Machine,
        copied: &std::path::Path,
        limit: Duration,
    ) -> Measured {
        use crate::display::wire::{Incoming, Msg};
        use gliff_proto::ServerMsg;
        use std::time::Instant;

        let mut home_display = home.display.lock().await.recv().await.unwrap();
        let remote_display = remote.display.lock().await.recv().await.unwrap();
        let list = uri_list_of(&[copied.to_path_buf()]);
        remote.copy(&[(URI_LIST, list.as_bytes())]);
        home.offered(1).await;
        let sent = RefCell::new(HashMap::new());
        let delays = RefCell::new(Vec::new());
        let pictures = async {
            let mut tick = tokio::time::interval(Duration::from_millis(16));
            for frame_id in 0u64.. {
                tokio::select! {
                    _ = tick.tick() => if remote_display.outbox.video_ready() {
                        let frame = ServerMsg::VideoFrame {
                            frame_id,
                            pts_us: 0,
                            keyframe: false,
                            damage: Vec::new(),
                            data_len: 60_000,
                            aux_len: 0,
                        };
                        sent.borrow_mut().insert(frame_id, Instant::now());
                        let payloads = vec![vec![7; 60_000], Vec::new()];
                        remote_display.outbox.send(Msg::Video(frame), payloads);
                    },
                    got = home_display.inbox.recv() => {
                        if let Some(Incoming::Video {
                            msg: ServerMsg::VideoFrame { frame_id, .. },
                            ..
                        }) = got
                            && let Some(at) = sent.borrow_mut().remove(&frame_id)
                        {
                            delays.borrow_mut().push(at.elapsed());
                        }
                    },
                }
            }
        };
        let begin = Instant::now();
        let pasted = tokio::select! {
            pasted = home.paste_for(URI_LIST, limit) => pasted,
            () = pictures => unreachable!(),
        };
        let paths = parse_uri_list(&String::from_utf8(pasted).unwrap());
        Measured {
            elapsed: begin.elapsed(),
            delays: delays.into_inner(),
            pasted: paths.into_iter().next().expect("the pasted file"),
        }
    }

    #[tokio::test]
    async fn a_large_transfer_interleaved_with_picture_frames_keeps_them_flowing() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("large.bin");
        let content: Vec<u8> = (0..32u32 << 20).map(|i| (i % 249) as u8).collect();
        std::fs::write(&copied, &content).unwrap();
        LocalSet::new()
            .run_until(async {
                let (home_end, remote_end) = lan();
                let home = machine(Side::Client, home_end, Inbox::new(tmp.path().join("home")));
                let remote = machine(Side::Server, remote_end, Inbox::new(tmp.path().join("r")));
                let measured =
                    frames_during_a_paste(&home, &remote, &copied, Duration::from_secs(30)).await;
                assert!(std::fs::read(&measured.pasted).unwrap() == content);
                // Half a second of transfer at least, so a frame every 16 ms: many of them.
                assert!(
                    measured.delays.len() >= 10,
                    "{} frames",
                    measured.delays.len()
                );
                // A frame waits for the chunk on the line at most, not for the file.
                let worst = measured.delay(1.0);
                assert!(
                    worst < Duration::from_millis(100),
                    "a frame waited {worst:?}"
                );
            })
            .await;
    }

    /// The 2 GB measure of the pull request, over TLS on loopback between two paired
    /// instances: `cargo test --release -- --ignored --nocapture two_gb`. Its files go in
    /// `target/`, on disk; `NAVETTE_BENCH_BYTES` sets another size.
    #[tokio::test]
    #[ignore = "a measure, minutes long and 4 GB on disk"]
    async fn two_gb_over_a_loopback_link_between_two_paired_instances() {
        use std::io::Write;
        let bytes: u64 = std::env::var("NAVETTE_BENCH_BYTES")
            .ok()
            .and_then(|b| b.parse().ok())
            .unwrap_or(2_000_000_000);
        let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let tmp = tempfile::tempdir_in(target).unwrap();
        let copied = tmp.path().join("large.bin");
        {
            let mut file = std::io::BufWriter::new(std::fs::File::create(&copied).unwrap());
            let block: Vec<u8> = (0..8u32 << 20).map(|i| (i % 251) as u8).collect();
            let mut left = bytes;
            while left > 0 {
                let n = left.min(block.len() as u64) as usize;
                file.write_all(&block[..n]).unwrap();
                left -= n as u64;
            }
        }
        let desk = link::tests::paired_desk().await;
        LocalSet::new()
            .run_until(async {
                let (events, _log) = mpsc::unbounded_channel();
                let acceptor = link::home_acceptor(&desk.home, None).await.unwrap();
                let home = machine_on(Side::Client, Inbox::new(tmp.path().join("home")), {
                    let events = events.clone();
                    |service| {
                        let attach = move |wire| service(wire);
                        tokio::task::spawn_local(link::serve(
                            acceptor,
                            Heartbeat::default(),
                            events,
                            attach,
                        ))
                    }
                });
                let dialer = link::remote_dialer(&desk.remote).unwrap();
                let remote =
                    machine_on(Side::Server, Inbox::new(tmp.path().join("r")), |service| {
                        tokio::task::spawn_local(link::agent(
                            dialer,
                            Heartbeat::default(),
                            link::Backoff::default(),
                            events,
                            move |wire| service(wire),
                        ))
                    });
                let m =
                    frames_during_a_paste(&home, &remote, &copied, Duration::from_secs(3600)).await;
                assert_eq!(std::fs::metadata(&m.pasted).unwrap().len(), bytes);
                let rate = bytes as f64 / m.elapsed.as_secs_f64() / 1e6;
                eprintln!(
                    "{bytes} bytes in {:.1} s: {rate:.0} MB/s ({:.0} Mbit/s); {} frames, delay \
                     p50 {:?}, p99 {:?}, max {:?}",
                    m.elapsed.as_secs_f64(),
                    rate * 8.0,
                    m.delays.len(),
                    m.delay(0.5),
                    m.delay(0.99),
                    m.delay(1.0),
                );
            })
            .await;
    }

    #[tokio::test]
    async fn a_new_bridge_on_the_same_link_never_reuses_a_serial() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (_from_peer, inbound) = mpsc::channel(1);
                let (to_peer, mut sent) = outbound_channel();
                let mut wire = Wire {
                    from_peer: inbound,
                    to_peer,
                    display: None,
                    audio: None,
                };
                let serials = Rc::default();
                let clipboard = Arc::new(Fake::default());
                let mut seen = Vec::new();
                // The compositor goes away after one copy, then a new bridge picks the link up.
                for _ in 0..2 {
                    let (events, selected) = mpsc::unbounded_channel();
                    let mime_types = clipboard.select(&[(TEXT, b"note")]);
                    events.send(Selected::Selection { mime_types }).unwrap();
                    let bridge = super::run(
                        Side::Client,
                        &mut wire,
                        clipboard.clone(),
                        selected,
                        Some(Inbox::new(tmp.path().into())),
                        Rc::clone(&serials),
                        quiet(),
                    );
                    tokio::pin!(bridge);
                    let serial = loop {
                        tokio::select! {
                            _ = &mut bridge => panic!("the bridge ended before its offer"),
                            sent = sent.recv() => {
                                if let (ClipboardMsg::Offer { serial, .. }, _) = sent.unwrap() {
                                    break serial;
                                }
                            }
                        }
                    };
                    drop(events);
                    assert!(!bridge.await, "the compositor went, not the link");
                    seen.push(serial);
                }
                assert_ne!(seen[0], seen[1], "the peer may still request the first");
            })
            .await;
    }
}
