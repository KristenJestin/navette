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
    parse_uri_list, resolve_mime,
};
use gliff_proto::{ClipboardFile, ClipboardItem, ClipboardMsg};
use gliff_transport::clipboard::files::{
    LocalFiles, Spool, fetch_files, list_files, open_source, retire, write_body,
};
use gliff_transport::clipboard::progress::{Jobs, Progress, State, describe_files, describe_mime};
use gliff_transport::clipboard::{Event, ReadSource, Side, Transfers, WriteSink};
use hypr_input::{Clipboard, ClipboardEvent, Target};
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe;
use tokio::sync::OnceCell;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::LocalSet;

use crate::link::Wire;
use crate::wire::{MAX_MIME_LEN, MAX_MIME_TYPES};

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
}

#[derive(Default)]
struct RemoteOffer {
    serial: u32,
    files: Vec<ClipboardFile>,
    /// The files once spooled for a paste; filled at most once per offer.
    spooled: Rc<OnceCell<Vec<PathBuf>>>,
    spool: Option<Rc<Spool>>,
}

/// The two ends of one link's clipboard: the peer's messages in, the compositor's events in.
pub struct Bridge<C: Compositor> {
    transfers: Transfers,
    jobs: Jobs,
    compositor: OffThread<C>,
    /// Where the files of a peer's offer are spooled for a paste; none without a home folder.
    spool_base: Option<PathBuf>,
    local: Rc<RefCell<LocalOffer>>,
    remote: Rc<RefCell<RemoteOffer>>,
    /// Bumped per selection so a slow offer build for a stale one is dropped. It is the
    /// link's, not the bridge's: a later bridge on the same link never reuses a serial the
    /// peer may still request.
    selection_gen: Rc<Cell<u64>>,
}

impl<C: Compositor> Bridge<C> {
    pub fn new(
        transfers: Transfers,
        jobs: Jobs,
        compositor: Arc<C>,
        spool_base: Option<PathBuf>,
        serials: Rc<Cell<u64>>,
    ) -> Self {
        Self {
            transfers,
            jobs,
            compositor: OffThread(Some(compositor)),
            spool_base,
            local: Rc::default(),
            remote: Rc::default(),
            selection_gen: serials,
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
        let previous = std::mem::replace(
            &mut *self.remote.borrow_mut(),
            RemoteOffer {
                serial,
                files,
                ..RemoteOffer::default()
            },
        );
        if let Some(spool) = previous.spool {
            retire(spool);
        }
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
                let Some(path) = self.local.borrow().files.path_for(&item).map(PathBuf::from)
                else {
                    self.transfers.refuse(id);
                    return;
                };
                let transfers = self.transfers.clone();
                tokio::task::spawn_local(async move {
                    match open_source(&path).await {
                        Ok(src) => transfers.serve(id, src),
                        Err(e) => {
                            log(format_args!("cannot open a copied file: {e}"));
                            transfers.refuse(id);
                        }
                    }
                });
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
            let files = match uri_list {
                Some(mime) => match receive(compositor, mime).await {
                    Ok(rx) => local_files_from_pipe(rx).await,
                    Err(e) => {
                        log(format_args!("cannot read the copied file list: {e}"));
                        LocalFiles::default()
                    }
                },
                None => LocalFiles::default(),
            };
            if selection_gen.get() != generation {
                return; // superseded while we were listing
            }
            let entries = files.entries.clone();
            local.borrow_mut().files = files;
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
            let (serial, files, spooled, spool) = {
                let mut remote = self.remote.borrow_mut();
                if remote.files.is_empty() {
                    return;
                }
                if remote.spool.is_none() {
                    let Some(base) = &self.spool_base else {
                        log(format_args!(
                            "no cache folder for pasted files: $HOME is not set"
                        ));
                        return;
                    };
                    match Spool::create_in(base) {
                        Ok(s) => remote.spool = Some(Rc::new(s)),
                        Err(e) => {
                            log(format_args!("cannot make a folder for pasted files: {e}"));
                            return;
                        }
                    }
                }
                (
                    remote.serial,
                    remote.files.clone(),
                    remote.spooled.clone(),
                    remote.spool.clone().expect("just created"),
                )
            };
            let (label, total) = describe_files(&files);
            self.jobs.run(label, total, |meter| async move {
                let paths = spooled
                    .get_or_try_init(|| fetch_files(&transfers, &files, serial, &spool, &meter))
                    .await?;
                let body = file_list_body(&mime_type, paths).unwrap_or_default();
                // An application that closed its end early is not the transfer's failure.
                let _ = write_body(WriteSink(target), body).await;
                Ok(())
            });
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

/// Read the compositor's `text/uri-list` and list the files it names.
async fn local_files_from_pipe(mut rx: pipe::Receiver) -> LocalFiles {
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
        return LocalFiles::default();
    }
    let paths = parse_uri_list(&String::from_utf8_lossy(&body));
    if paths.is_empty() {
        return LocalFiles::default();
    }
    match tokio::task::spawn_blocking(move || list_files(&paths)).await {
        Ok(Ok(files)) => files,
        Ok(Err(e)) => {
            log(format_args!("the copied files are not offered: {e}"));
            LocalFiles::default()
        }
        Err(_) => LocalFiles::default(),
    }
}

/// Runs one link's bridge until the link's clipboard channel closes (`true`) or the compositor
/// side does (`false`). Takes the wire by reference so a new compositor connection can pick
/// the same link up again; `serials` is the link's. Whatever the bridge started (pastes,
/// transfers, spools kept for their grace) ends with it, even when this future is dropped.
pub async fn run<C: Compositor>(
    side: Side,
    wire: &mut Wire,
    compositor: Arc<C>,
    mut events: UnboundedReceiver<Selected>,
    spool_base: Option<PathBuf>,
    serials: Rc<Cell<u64>>,
) -> bool {
    // Every task the bridge and gliff's engine spawn lands in this set, and dies with it.
    let tasks = LocalSet::new();
    tasks
        .run_until(async {
            let bridge = Bridge::new(
                Transfers::new(wire.to_peer.clone(), side),
                Jobs::new(report),
                compositor,
                spool_base,
                serials,
            );
            loop {
                tokio::select! {
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

/// Only what the user must know: a paste that failed. It names no file or content.
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
pub async fn attach<C, S>(side: Side, start: S, mut wire: Wire)
where
    C: Compositor,
    S: Fn(UnboundedSender<Selected>) -> io::Result<C> + Clone + Send + 'static,
{
    let serials = Rc::new(Cell::new(0));
    let spool = spool_base();
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
                    spool.clone(),
                    serials.clone(),
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

/// `$XDG_CACHE_HOME/navette/clipboard`: on disk, not in the RAM-backed runtime folder, since
/// a pasted file can be large, and never in the shared temporary folder.
fn spool_base() -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(cache.join("navette").join("clipboard"))
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
    use crate::link::{self, Heartbeat};
    use crate::wire::LinkMsg;

    const TEXT: &str = "text/plain;charset=utf-8";
    const URI_LIST: &str = "text/uri-list";

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
            let (reader, writer) = std::io::pipe().unwrap();
            self.events
                .send(Selected::Paste {
                    mime_type: mime_type.into(),
                    fd: writer.into(),
                })
                .unwrap();
            let mut reader = pipe::Receiver::from_owned_fd(reader.into()).unwrap();
            let mut data = Vec::new();
            within("the paste", reader.read_to_end(&mut data))
                .await
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
    fn desk(spool: &std::path::Path) -> (Machine, Machine) {
        let (home_end, remote_end) = tokio::io::duplex(64 * 1024);
        let home = machine(Side::Client, home_end, spool.join("home"));
        let remote = machine(Side::Server, remote_end, spool.join("remote"));
        (home, remote)
    }

    fn machine(side: Side, stream: tokio::io::DuplexStream, spool: std::path::PathBuf) -> Machine {
        let clipboard = Arc::new(Fake::default());
        let (events, selected) = mpsc::unbounded_channel();
        let bridge = clipboard.clone();
        let link = tokio::task::spawn_local(async move {
            let service = |mut wire| async move {
                let serials = Rc::default();
                super::run(side, &mut wire, bridge, selected, Some(spool), serials).await;
            };
            link::run(stream, heartbeat(), service).await;
        });
        Machine {
            clipboard,
            events,
            link,
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
                assert_eq!(paths[0].file_name().unwrap(), "report.bin");
                assert!(
                    paths[0].starts_with(tmp.path()),
                    "spooled under the test folder"
                );
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
                    Some(tmp.path().into()),
                    Rc::default(),
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
                    |wire| super::attach(Side::Client, hypr(missing), wire)
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
                let spool = tmp.path().to_path_buf();
                tokio::task::spawn_local(link::run(home_end, eager(), |mut wire| async move {
                    let serials = Rc::default();
                    super::run(
                        Side::Client,
                        &mut wire,
                        bridge,
                        selected,
                        Some(spool),
                        serials,
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
                    super::attach(Side::Client, slow, wire)
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
                    super::attach(Side::Client, gone, wire)
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
                    super::attach(Side::Client, missing, wire)
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

    #[tokio::test]
    async fn the_pasted_files_are_removed_when_the_link_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                for (n, name) in ["first.txt", "second.txt"].into_iter().enumerate() {
                    std::fs::write(copied.join(name), name).unwrap();
                    let list = format!("file://{}\r\n", copied.join(name).display());
                    home.copy(&[(URI_LIST, list.as_bytes())]);
                    remote.offered(n + 1).await;
                    remote.paste(URI_LIST).await;
                }
                let spooled = tmp.path().join("remote");
                assert_eq!(std::fs::read_dir(&spooled).unwrap().count(), 2);

                home.link.abort();
                within("the spools removed", async {
                    while std::fs::read_dir(&spooled).unwrap().count() > 0 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await;
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
                        Some(tmp.path().into()),
                        Rc::clone(&serials),
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
