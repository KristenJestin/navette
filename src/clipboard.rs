//! The clipboard bridge: copy on one machine, paste on the other, both ways, text and files.
//! One bridge runs per link, on each side, however many monitors a mode will give the remote
//! machine: during the trial two sessions to the same machine replaced each other's offers on
//! its one clipboard, and every paste failed with "the peer aborted the transfer". It bridges
//! only while a mode runs: between two modes, a copy stays on its machine, and the peer's
//! requests are refused, on both sides, whatever the other side asks.
//!
//! The transfer engine is gliff's (`gliff-transport`): lazy (nothing moves until an
//! application pastes), typed, chunked, files included. The peer's files show in a virtual
//! folder ([`vfs`]) and cross only when an application reads them; where it cannot be mounted,
//! a paste fetches them all first. The compositor side is
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
pub mod vfs;

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::hash::BuildHasher;
use std::io;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gliff_proto::clipboard::{
    MAX_ITEM, file_list_body, forwardable_mimes, is_file_mime, is_text_mime, local_mimes_for_offer,
    parse_uri_list, resolve_mime, top_level,
};
use gliff_proto::{ClipboardFile, ClipboardItem, ClipboardMsg};
use gliff_transport::clipboard::files::{LocalFiles, write_body};
use gliff_transport::clipboard::progress::{Jobs, Meter, Progress, State, describe_mime};
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

/// How many copies of files made before the current one still serve their files: the peer may
/// be reading them through its virtual folder.
const OLDER_SERVED: usize = 8;

/// How often the virtual folder lets go of the replaced copies whose transfer is over.
const TIDY: Duration = Duration::from_secs(5);

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

/// What a machine's bridges keep from one link to the next. A reconnect (the lid closed and
/// opened, a network blip) offers each machine's copy again, and an application that reads
/// every new selection would fetch its files again: so the machine that copied offers an
/// unchanged copy of files under the serial it had, and the machine that fetched it answers
/// with the files it already has, while they are in the receiving folder. That folder is
/// emptied when a link ends, so across links only the serial is kept; within one, this is what
/// keeps a copy offered again from being fetched twice.
#[derive(Clone)]
pub struct Kept {
    /// The serials of this machine's offers. From a random start, so that a restart does not
    /// give a new copy the serial of one the peer kept.
    serials: Rc<Cell<u64>>,
    /// The last copy of files offered: its serial, its files, and when they last changed.
    sent: Rc<RefCell<Option<(u32, Unchanged)>>>,
    /// The last copy of files fetched: its serial, its files, and where they went.
    fetched: Rc<RefCell<Option<(u32, Fetched)>>>,
}

type Fetched = (Vec<ClipboardFile>, Vec<PathBuf>);

type Unchanged = (
    Vec<PathBuf>,
    Vec<ClipboardFile>,
    Vec<Option<std::time::SystemTime>>,
);

impl Default for Kept {
    fn default() -> Self {
        let start = std::collections::hash_map::RandomState::new().hash_one(()) as u32;
        Self {
            serials: Rc::new(Cell::new(start.into())),
            sent: Rc::default(),
            fetched: Rc::default(),
        }
    }
}

impl Kept {
    /// The serial to offer `copied` as: the one it had when it is the last copy of files
    /// offered, unchanged; else `new`.
    fn serial_of(&self, copied: &files::Copied, new: u32) -> u32 {
        if copied.files.entries.is_empty() {
            return new;
        }
        let this = (
            copied.files.paths.clone(),
            copied.files.entries.clone(),
            copied.modified.clone(),
        );
        let mut sent = self.sent.borrow_mut();
        match &*sent {
            Some((serial, last)) if *last == this => *serial,
            _ => {
                *sent = Some((new, this));
                new
            }
        }
    }

    /// Where the files of the peer's copy `serial` are, when this machine fetched it already
    /// and they are all still there.
    fn received(&self, serial: u32, files: &[ClipboardFile]) -> Option<Vec<PathBuf>> {
        let fetched = self.fetched.borrow();
        let (fetched_serial, (fetched_files, paths)) = fetched.as_ref()?;
        let here = paths.iter().all(|p| p.symlink_metadata().is_ok());
        (*fetched_serial == serial && fetched_files == files && here).then(|| paths.clone())
    }
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
    /// The copies of files made before, by serial, the latest last: their files are still
    /// served, not their other types.
    older: std::collections::VecDeque<(u32, LocalFiles)>,
}

#[derive(Default)]
struct RemoteOffer {
    serial: u32,
    files: Vec<ClipboardFile>,
    /// Where the files are once pasted; filled at most once per offer, so a second paste of
    /// the same copy points at the same files.
    received: Rc<OnceCell<Vec<PathBuf>>>,
    /// The mime types it is offered as on this machine.
    advertised: Vec<String>,
    /// Whether it is still this machine's selection: no copy here replaced it.
    selected: bool,
    /// Whether it was offered again once its files were in.
    offered_again: bool,
    /// Its pastes on this machine, stopped when a newer copy replaces it.
    pastes: Vec<Crossing>,
    /// Where its files show in the virtual folder, when it is mounted.
    in_folder: Option<Vec<PathBuf>>,
}

/// The two ends of one link's clipboard: the peer's messages in, the compositor's events in.
pub struct Bridge<C: Compositor> {
    transfers: Transfers,
    jobs: Jobs,
    compositor: OffThread<C>,
    /// Where the files of a peer's offer go when pasted: a temporary folder.
    inbox: Option<Inbox>,
    local: Rc<RefCell<LocalOffer>>,
    remote: Rc<RefCell<RemoteOffer>>,
    /// Bumped per selection so a slow offer build for a stale one is dropped. It is the
    /// machine's, not the bridge's: a later bridge never gives another copy a serial the peer
    /// may still request.
    selection_gen: Rc<Cell<u64>>,
    kept: Kept,
    progress: progress::Progress,
    notifier: Arc<dyn Notifier>,
    /// The number of the last paste of files on this machine.
    pastes: Cell<u32>,
    /// The copy whose files the peer last asked for: its crossing is logged once.
    serving: Cell<Option<u32>>,
    /// The virtual folder of the peer's files, when it is mounted.
    folder: Option<Arc<vfs::Folder>>,
    /// The reads of each copy read through the folder.
    reads: Rc<RefCell<std::collections::HashMap<u32, Reads>>>,
    /// The files fetched now for the folder, of every copy: room is kept for all of them.
    fetching: Rc<RefCell<Vec<Arc<vfs::Content>>>>,
}

/// The reads of one copy of the peer's through the virtual folder.
#[derive(Default)]
struct Reads {
    /// Its fetches running.
    running: usize,
    /// The bytes fetched of all its files, from the first read: what its bar shows.
    done: Meter,
    /// The copy went from the folder: these reads go with their last fetch.
    gone: bool,
    /// The user was told a read of it failed: once is enough, the next ones fail alike.
    told: bool,
}

impl<C: Compositor> Bridge<C> {
    pub fn new(
        transfers: Transfers,
        jobs: Jobs,
        compositor: Arc<C>,
        inbox: Option<Inbox>,
        kept: Kept,
        shown: &Shown,
    ) -> Self {
        Self {
            transfers,
            jobs,
            compositor: OffThread(Some(compositor)),
            inbox,
            local: Rc::default(),
            remote: Rc::default(),
            selection_gen: kept.serials.clone(),
            kept,
            progress: progress::Progress::new(shown.bar.clone()),
            notifier: shown.notifier.clone(),
            pastes: Cell::new(0),
            serving: Cell::new(None),
            folder: None,
            reads: Rc::default(),
            fetching: Rc::default(),
        }
    }

    /// The peer's copy goes with the mode: no application here pastes it after. A copy made
    /// here since stays.
    fn withdraw(&self) {
        let mut remote = self.remote.borrow_mut();
        if std::mem::take(&mut remote.selected) {
            self.compositor.get().offer(Vec::new());
        }
    }

    /// Stops the transfer whose bar is `transfer`, whichever way it crosses.
    pub fn cancel(&self, transfer: u64) {
        match self.progress.cancel(transfer) {
            Some(Crossing::Paste(_)) => self.notifier.show("Paste cancelled", ""),
            Some(Crossing::Read(serial)) => {
                log(format_args!("offer {serial}: its reads stopped"));
                if let Some(folder) = &self.folder {
                    folder.stop(serial);
                }
                self.notifier.show("Paste cancelled", "");
            }
            _ => {}
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
        log(format_args!(
            "offer {serial} received: {} files, {} types",
            files.len(),
            mime_types.len()
        ));
        let advertise = local_mimes_for_offer(&mime_types, !files.is_empty());
        // A fetch of the copy it replaces would only compete with its own for the link.
        let (replaced, pastes) = {
            let mut remote = self.remote.borrow_mut();
            (remote.serial, std::mem::take(&mut remote.pastes))
        };
        if pastes
            .into_iter()
            .fold(false, |any, p| self.progress.stop(p) | any)
        {
            log(format_args!(
                "offer {replaced}: its paste stopped, offer {serial} replaced it"
            ));
        }
        // In the virtual folder, the copies it replaces stay while they are read.
        let valid = files::validate(&files).is_ok();
        let in_folder = self.folder.as_ref().and_then(|folder| {
            // A copy refused shows nothing, but still replaces those before it.
            let shown = folder.offer(serial, if valid { &files } else { &[] });
            valid.then_some(shown)
        });
        let received = match in_folder {
            Some(_) => None,
            None => self.kept.received(serial, &files),
        };
        // Only the files of the current offer are kept: a paste of it in progress copies from
        // them until it is replaced. The same copy offered again points at the same files.
        if received.is_none()
            && self.folder.is_none()
            && let Some(inbox) = &self.inbox
        {
            inbox.empty();
        }
        if received.is_some() {
            log(format_args!(
                "offer {serial}: the copy fetched before, its files are here already"
            ));
        }
        *self.remote.borrow_mut() = RemoteOffer {
            serial,
            files,
            received: Rc::new(OnceCell::new_with(received)),
            advertised: advertise.clone(),
            selected: true,
            in_folder,
            ..RemoteOffer::default()
        };
        self.compositor.get().offer(advertise);
    }

    fn on_request(&self, id: u32, serial: u32, item: ClipboardItem) {
        let current = self.local.borrow().serial;
        let older = matches!(item, ClipboardItem::File(_))
            && serial != current
            && self.local.borrow().older.iter().any(|(s, _)| *s == serial);
        if serial != current && !older {
            // The selection moved on since the peer saw the offer: refuse, so its paste fails
            // on its own, rather than serve data it did not ask for.
            log(format_args!(
                "offer {serial}: a request refused, this machine copied again (offer {current})"
            ));
            self.transfers.refuse(id);
            return;
        }
        match item {
            ClipboardItem::Mime(requested) => {
                let local = self.local.borrow();
                let Some(actual) = resolve_mime(&requested, &local.mime_types) else {
                    log(format_args!(
                        "offer {serial}: {requested} refused, not in the copy"
                    ));
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
            ClipboardItem::File(index) => {
                let (path, size, total, skipped) = {
                    let mut local = self.local.borrow_mut();
                    let copied = match older {
                        false => &local.files,
                        true => &local.older.iter().find(|(s, _)| *s == serial).unwrap().1,
                    };
                    let Some(path) = copied.path_for(&item).map(PathBuf::from) else {
                        log(format_args!(
                            "offer {serial}: file {index} refused, not in the copy"
                        ));
                        self.transfers.refuse(id);
                        return;
                    };
                    let size = copied.entries[index as usize].size;
                    // Listed and validated: the sum is counted.
                    let total = files::total(&copied.entries).unwrap_or(u64::MAX);
                    let skipped = match older {
                        false => std::mem::take(&mut local.skipped),
                        true => Vec::new(),
                    };
                    (path, size, total, skipped)
                };
                if !skipped.is_empty() {
                    self.notifier
                        .show("Not sent: links and special files", &skipped.join(", "));
                }
                let Some((done, stopped, run)) = self.progress.serving(serial, total) else {
                    // The user stopped this copy's paste: its next files are not served.
                    log(format_args!(
                        "offer {serial}: file {index} refused, its paste was stopped"
                    ));
                    self.transfers.refuse(id);
                    return;
                };
                if self.serving.replace(Some(serial)) != Some(serial) {
                    log(format_args!(
                        "offer {serial}: the peer pastes it, serving its files ({})",
                        human_bytes(total)
                    ));
                }
                let crossing = Crossing::Serve(serial);
                match files::open_copied(&path) {
                    Ok(file) => self.transfers.serve(
                        id,
                        Served {
                            source: ReadSource(file),
                            size,
                            read: 0,
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
        self.remote.borrow_mut().selected = false;
        {
            let mut local = self.local.borrow_mut();
            let files = std::mem::take(&mut local.files);
            if !files.entries.is_empty() {
                let serial = local.serial;
                local.older.retain(|(s, _)| *s != serial);
                local.older.push_back((serial, files));
                if local.older.len() > OLDER_SERVED {
                    local.older.pop_front();
                }
            }
            local.serial = generation as u32;
            local.mime_types = mime_types.clone();
            local.skipped = Vec::new();
        }
        let mut offered = forwardable_mimes(&mime_types);
        // What the peer would refuse, and what is no data but an application's own; the text
        // types come first, so text survives.
        offered.retain(|m| m.len() <= MAX_MIME_LEN && standard(m));
        offered.truncate(MAX_MIME_TYPES);
        // Read the file list as whichever file-list type the selection advertises; the parser
        // handles both bodies.
        let uri_list = mime_types.iter().find(|m| is_file_mime(m)).cloned();
        let compositor = self.compositor.get().clone();
        let transfers = self.transfers.clone();
        let local = self.local.clone();
        let selection_gen = self.selection_gen.clone();
        let (kept, progress) = (self.kept.clone(), self.progress.clone());
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
                log(format_args!(
                    "a copy here was replaced before it was sent, so only the newer one is"
                ));
                return;
            }
            let serial = kept.serial_of(&copied, generation as u32);
            // Copied again: what the user stopped of it may cross again.
            progress.resume(serial);
            let entries = copied.files.entries.clone();
            log(format_args!(
                "offer {serial} sent: {} files, {} types",
                entries.len(),
                offered.len()
            ));
            {
                let mut local = local.borrow_mut();
                local.serial = serial;
                local.files = copied.files;
                local.skipped = copied.skipped;
            }
            let _ = transfers
                .send(ClipboardMsg::Offer {
                    serial,
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
        // The text of a copy of files names the files: an application that pastes by reading
        // the text gets them like one that reads the list.
        let of_files = is_file_mime(&mime_type)
            || (is_text_type(&mime_type) && !self.remote.borrow().files.is_empty());
        if of_files {
            let (serial, files, received) = {
                let remote = self.remote.borrow();
                if remote.files.is_empty() {
                    log(format_args!(
                        "offer {}: {mime_type} asked, but it holds no files",
                        remote.serial
                    ));
                    return;
                }
                (remote.serial, remote.files.clone(), remote.received.clone())
            };
            if let Some(paths) = self.remote.borrow().in_folder.clone() {
                // Nothing crosses: each file comes when an application reads it.
                log(format_args!(
                    "offer {serial}: pasted as {mime_type}, from the virtual folder"
                ));
                let body = files_body(&mime_type, &paths);
                tokio::task::spawn_local(write_body(WriteSink(target), body));
                return;
            }
            log(format_args!("offer {serial}: pasted as {mime_type}"));
            let Some(inbox) = self.inbox.clone() else {
                log(format_args!("no folder for pasted files"));
                return;
            };
            if let Err(e) = files::validate(&files) {
                log(format_args!("a paste of files is refused: {e}"));
                self.notifier.show("Paste failed", &e);
                return;
            }
            // Validated: gliff's sum of the sizes cannot overflow.
            let total = files::total(&files).unwrap_or(0);
            let paste = Crossing::Paste(self.pastes.get().wrapping_add(1));
            self.pastes.set(self.pastes.get().wrapping_add(1));
            self.remote.borrow_mut().pastes.push(paste);
            let done = Meter::default();
            let (meter, progress, notifier) =
                (done.clone(), self.progress.clone(), self.notifier.clone());
            let (remote, compositor) = (self.remote.clone(), self.compositor.get().clone());
            let kept = self.kept.clone();
            // Filled before the task first runs: nothing runs before this call returns.
            let run = Rc::new(Cell::new(0));
            let its_run = run.clone();
            let task = tokio::task::spawn_local(async move {
                let mut fetched = false;
                let started = std::time::Instant::now();
                let result = received
                    .get_or_try_init(|| {
                        fetched = true;
                        log(format_args!(
                            "offer {serial}: fetching {} files ({})",
                            files.len(),
                            human_bytes(total)
                        ));
                        receive_files(&transfers, &files, serial, &inbox, &meter)
                    })
                    .await;
                progress.end(paste, its_run.get());
                match result {
                    Ok(paths) => {
                        // No notification: a file manager fetches the files of a copy as
                        // soon as it is offered, reading it as a paste does.
                        if fetched {
                            *kept.fetched.borrow_mut() =
                                Some((serial, (files.clone(), paths.clone())));
                            log(format_args!(
                                "offer {serial}: {} files pasted in {} ms",
                                files.len(),
                                started.elapsed().as_millis()
                            ));
                        }
                        let body = files_body(&mime_type, paths);
                        // An application that closed its end early is not the paste's failure.
                        if write_body(WriteSink(target), body).await.is_err() {
                            offer_again(&remote, serial, &*compositor);
                        }
                    }
                    Err(e) => {
                        log(format_args!(
                            "offer {serial}: the paste of its files failed: {e}"
                        ));
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
            log(format_args!("offer {serial}: pasted as {mime_type}"));
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

impl<C: Compositor> Bridge<C> {
    /// What the virtual folder asks: a file read, to fetch, or a copy gone, whose fetches stop.
    pub fn on_asked(&self, asked: vfs::Asked) {
        match asked {
            vfs::Asked::Fetch {
                serial,
                index,
                total,
                content,
            } => self.fetch_read(serial, index, total, content),
            // Its transfer is over: a fetch still running has its bytes, and ends on its own
            // at the peer's end.
            vfs::Asked::Gone { serial } => {
                let mut reads = self.reads.borrow_mut();
                match reads.get_mut(&serial) {
                    Some(read) if read.running > 0 => read.gone = true,
                    _ => drop(reads.remove(&serial)),
                }
            }
        }
    }

    /// Fetches file `index` of the peer's copy `serial` into `content`, for the applications
    /// that read it: one transfer per copy, whose bar counts the bytes of all its files.
    fn fetch_read(&self, serial: u32, index: u32, total: u64, content: Arc<vfs::Content>) {
        // Asked before a cancel the bridge saw first: the copy's reads are stopped.
        if content.over() {
            return;
        }
        let size = content.size;
        let coming: u64 = self.fetching.borrow().iter().map(|c| c.left()).sum();
        if let Some(inbox) = &self.inbox
            && let Err(e) = inbox.check_cache_room(size.saturating_add(coming))
        {
            log(format_args!("offer {serial}: file {index} not read: {e}"));
            self.notifier.show("Paste failed", &e.to_string());
            content.end(Err(libc::ENOSPC));
            if let Some(folder) = &self.folder {
                folder.forget(serial, index, &content);
            }
            return;
        }
        self.fetching.borrow_mut().push(content.clone());
        let (first, done) = {
            let mut reads = self.reads.borrow_mut();
            let reads = reads.entry(serial).or_default();
            reads.running += 1;
            (reads.running == 1, reads.done.clone())
        };
        let (meter, tasks, run) = self.progress.reading(serial, total, done);
        if first {
            log(format_args!(
                "offer {serial}: an application reads its files, fetching them as read ({})",
                human_bytes(total)
            ));
        }
        let transfers = self.transfers.clone();
        let (progress, notifier, reads) = (
            self.progress.clone(),
            self.notifier.clone(),
            self.reads.clone(),
        );
        let (folder, fetching) = (self.folder.clone(), self.fetching.clone());
        let task = tokio::task::spawn_local(async move {
            // Ends the read and the transfer however the fetch ends, stopped included.
            let ending = ReadEnding {
                content: content.clone(),
                serial,
                index,
                run,
                progress,
                reads,
                folder,
                fetching,
            };
            let sink = meter.wrap(ContentSink {
                content: content.clone(),
                offset: 0,
            });
            let started = std::time::Instant::now();
            let fetched = transfers
                .fetch(ClipboardItem::File(index), serial, sink, Some(size))
                .await;
            match fetched {
                Ok(got) if got == size => {
                    content.end(Ok(()));
                    log(format_args!(
                        "offer {serial}: file {index} read in {} ms",
                        started.elapsed().as_millis()
                    ));
                }
                Ok(got) => {
                    content.end(Err(libc::EIO));
                    log(format_args!(
                        "offer {serial}: file {index}: got {got} of {size} bytes"
                    ));
                }
                // Its reader closed it before its end: the peer is told to stop.
                Err(_) if content.over() => log(format_args!(
                    "offer {serial}: file {index} closed before its end, its fetch stopped"
                )),
                Err(e) => {
                    content.end(Err(libc::EIO));
                    log(format_args!("offer {serial}: file {index} not read: {e}"));
                    let told = told_once(&ending.reads, serial);
                    if !told {
                        notifier.show("Paste failed", &why(&e));
                    }
                }
            }
        });
        tasks.borrow_mut().push(task.abort_handle());
    }
}

/// Whether the user was told already that a read of the copy `serial` failed; they are now.
fn told_once(reads: &RefCell<std::collections::HashMap<u32, Reads>>, serial: u32) -> bool {
    let mut reads = reads.borrow_mut();
    reads
        .get_mut(&serial)
        .is_some_and(|r| std::mem::replace(&mut r.told, true))
}

/// Ends a read of the virtual folder when its fetch ends or is dropped: its readers get what
/// came, or fail, and a file not fetched whole is fetched again at its next read; the transfer
/// of its copy ends with its last fetch.
struct ReadEnding {
    content: Arc<vfs::Content>,
    serial: u32,
    index: u32,
    run: u64,
    progress: progress::Progress,
    reads: Rc<RefCell<std::collections::HashMap<u32, Reads>>>,
    folder: Option<Arc<vfs::Folder>>,
    fetching: Rc<RefCell<Vec<Arc<vfs::Content>>>>,
}

impl Drop for ReadEnding {
    fn drop(&mut self) {
        // A fetch dropped before its end: stopped by the user, its copy gone, or the mode.
        self.content.end(Err(libc::ECANCELED));
        let content = &self.content;
        self.fetching
            .borrow_mut()
            .retain(|c| !Arc::ptr_eq(c, content));
        if !content.whole()
            && let Some(folder) = &self.folder
        {
            folder.forget(self.serial, self.index, content);
        }
        let mut all = self.reads.borrow_mut();
        let Some(reads) = all.get_mut(&self.serial) else {
            return;
        };
        reads.running -= 1;
        if reads.running == 0 {
            self.progress.end(Crossing::Read(self.serial), self.run);
            if reads.gone {
                all.remove(&self.serial);
            }
        }
    }
}

/// Where a file read through the virtual folder is fetched: its cache, written beside the
/// link's thread.
struct ContentSink {
    content: Arc<vfs::Content>,
    offset: u64,
}

impl gliff_transport::clipboard::ChunkSink for ContentSink {
    async fn write(&mut self, chunk: Bytes) -> io::Result<()> {
        let (content, offset) = (self.content.clone(), self.offset);
        self.offset += chunk.len() as u64;
        tokio::task::spawn_blocking(move || content.write_at(&chunk, offset))
            .await
            .map_err(io::Error::other)?
    }

    async fn finish(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Whether `mime_type` is one of the text types an application reads a text copy as.
fn is_text_type(mime_type: &str) -> bool {
    is_text_mime(mime_type) || mime_type.starts_with("text/plain;")
}

/// What a paste of a copy of files reads as `mime_type`: the file list, or for a text type
/// the same paths, one per line, with no final newline so that a terminal does not run them.
fn files_body(mime_type: &str, paths: &[PathBuf]) -> String {
    if is_text_type(mime_type) {
        let lines: Vec<_> = paths.iter().map(|p| p.to_string_lossy()).collect();
        return lines.join("\n");
    }
    file_list_body(mime_type, paths).unwrap_or_default()
}

/// Whether any application may read `mime_type`. An application can put types of its own on
/// the clipboard for its other instances (a token, its process id): on the other machine they
/// name nothing, or something else, so they stay on their machine. File lists cross as the
/// offer's files.
fn standard(mime_type: &str) -> bool {
    matches!(
        mime_type,
        "text/plain" | "UTF8_STRING" | "STRING" | "TEXT" | "text/html"
    ) || mime_type.starts_with("text/plain;")
        || mime_type.starts_with("image/")
}

/// The application that pasted the peer's offer `serial` stopped waiting before its files were
/// in: a file manager reads the clipboard when it changes, and gives up after a few seconds,
/// which a paste over the LAN outlasts. The offer goes on the clipboard
/// again, so the file manager reads it once more and gets the files at once. Only once, and
/// only while it is still the selection here: a copy made here since is never replaced.
fn offer_again(remote: &RefCell<RemoteOffer>, serial: u32, compositor: &impl Compositor) {
    let mut remote = remote.borrow_mut();
    if remote.serial != serial || !remote.selected || remote.offered_again {
        return;
    }
    remote.offered_again = true;
    log(format_args!(
        "offer {serial}: the application stopped waiting for its files, offering them again"
    ));
    compositor.offer(remote.advertised.clone());
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
    inbox.check_room(files::total(files).unwrap_or(u64::MAX))?;
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
        log(format_args!(
            "the copied file list names no local file, so no file is sent"
        ));
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

/// Serves one link's clipboard until the link's clipboard channel closes (`true`) or the
/// compositor side does (`false`): a bridge for each mode, and nothing crossing between two.
/// Takes the wire by reference so a new compositor connection can pick the same link up again;
/// `kept` is the machine's. Whatever a bridge started (pastes and transfers, their half-written
/// files) ends with it, even when this future is dropped.
pub async fn run<C: Compositor>(
    side: Side,
    wire: &mut Wire,
    compositor: Arc<C>,
    mut events: UnboundedReceiver<Selected>,
    inbox: Option<Inbox>,
    kept: Kept,
    shown: Shown,
) -> bool {
    // Dropping the compositor may block: never on the link's thread.
    let compositor = OffThread(Some(compositor));
    loop {
        if let Some(ended) = quiet(wire, &mut events).await {
            return matches!(ended, Ended::Link);
        }
        log(format_args!("a mode runs: copies cross"));
        let ended = bridge(
            side,
            wire,
            compositor.get().clone(),
            &mut events,
            inbox.clone(),
            kept.clone(),
            shown.clone(),
        )
        .await;
        match ended {
            Ended::Mode => log(format_args!("no mode runs: copies stay on their machine")),
            ended => return matches!(ended, Ended::Link),
        }
    }
}

/// What ended a link's bridge.
enum Ended {
    Link,
    Compositor,
    Mode,
}

/// Until the display says a mode runs (`on`), or that none does.
async fn mode_is(mode: &mut tokio::sync::watch::Receiver<bool>, on: bool) {
    while crate::link::in_mode(mode) != on {
        if mode.changed().await.is_err() && on {
            // No display: no mode will start on this link.
            std::future::pending::<()>().await;
        }
    }
}

/// Outside a mode: a copy here stays here, a copy the peer offers anyway is not put on the
/// clipboard, and its requests are refused. A copy made now is not offered when the mode
/// starts: only the next one is. `None` once a mode runs.
async fn quiet(wire: &mut Wire, events: &mut UnboundedReceiver<Selected>) -> Option<Ended> {
    loop {
        tokio::select! {
            biased;
            () = mode_is(&mut wire.mode, true) => return None,
            clip = wire.from_peer.recv() => match clip {
                Some((ClipboardMsg::Request { id, serial, .. }, _)) => {
                    log(format_args!("offer {serial}: a request refused, no mode runs"));
                    let _ = wire.to_peer.send((ClipboardMsg::Abort { id }, Bytes::new())).await;
                }
                Some((ClipboardMsg::Offer { serial, .. }, _)) => {
                    log(format_args!("offer {serial} not taken, no mode runs"));
                }
                Some(_) => {}
                None => return Some(Ended::Link),
            },
            event = events.recv() => match event {
                Some(Selected::Selection { .. }) => {
                    log(format_args!("a copy here stays here, no mode runs"));
                }
                // Nothing of the peer's is on the clipboard: the paste reads nothing.
                Some(Selected::Paste { .. }) => {}
                None => return Some(Ended::Compositor),
            },
        }
    }
}

/// A mode's bridge, until the mode stops, the link ends or the compositor goes.
async fn bridge<C: Compositor>(
    side: Side,
    wire: &mut Wire,
    compositor: Arc<C>,
    events: &mut UnboundedReceiver<Selected>,
    inbox: Option<Inbox>,
    kept: Kept,
    shown: Shown,
) -> Ended {
    // Every task the bridge and gliff's engine spawn lands in this set, and dies with it.
    let tasks = LocalSet::new();
    // The received files go with the mode, however it ends: the mode stops, the link drops, or
    // this future is dropped. Only a compositor that went away leaves them, since the link and
    // the offer they belong to are still up.
    let mut leaving = Emptying(inbox.clone());
    // Declared after the tasks: dropped first, so its reads fail before their fetches go.
    let (mounted, mut asked) = mount(inbox.as_ref()).await.unzip();
    tasks
        .run_until(async {
            let mut bridge = Bridge::new(
                Transfers::new(wire.to_peer.clone(), side),
                Jobs::new(report),
                compositor,
                inbox,
                kept,
                &shown,
            );
            bridge.folder = mounted.as_ref().map(|m| m.folder.clone());
            let bridge = bridge;
            let mut cancel = shown.cancel.clone();
            // A click before this bridge stops nothing of it.
            cancel.borrow_and_update();
            let mut cancel_open = true;
            let folder = mounted.as_ref().map(|m| m.folder.clone());
            let mut tidy = tokio::time::interval(TIDY);
            loop {
                tokio::select! {
                    // A stop is seen before what the peer sent after it.
                    biased;
                    () = mode_is(&mut wire.mode, false) => {
                        bridge.withdraw();
                        return Ended::Mode;
                    }
                    changed = cancel.changed(), if cancel_open => match changed {
                        Ok(()) => bridge.cancel(cancel.borrow_and_update().transfer),
                        Err(_) => cancel_open = false,
                    },
                    clip = wire.from_peer.recv() => match clip {
                        Some((msg, payload)) => bridge.on_peer_msg(msg, payload),
                        None => return Ended::Link,
                    },
                    Some(asked) = next_asked(&mut asked) => bridge.on_asked(asked),
                    now = tidy.tick(), if folder.is_some() => {
                        folder.as_ref().unwrap().tidy(now.into_std());
                    }
                    event = events.recv() => match event {
                        Some(event) => bridge.on_compositor_event(event),
                        None => {
                            leaving.0 = None;
                            return Ended::Compositor;
                        }
                    },
                }
            }
        })
        .await
}

/// The virtual folder of a mode's bridge, mounted. Dropped, its reads fail, then it is
/// unmounted, beside the link's thread.
struct Mounted {
    folder: Arc<vfs::Folder>,
    handle: Option<Box<dyn Send>>,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        self.folder.close();
        vfs::unmount_later(self.handle.take());
    }
}

/// What the virtual folder asks next; never anything without one.
async fn next_asked(asked: &mut Option<UnboundedReceiver<vfs::Asked>>) -> Option<vfs::Asked> {
    match asked {
        Some(asked) => asked.recv().await,
        None => std::future::pending().await,
    }
}

/// Mounts the virtual folder of `inbox`, if it has one, on a blocking thread; with what it
/// asks of the bridge. When it cannot be mounted, the bridge fetches the files of a paste
/// first, as without one, and says so once.
async fn mount(inbox: Option<&Inbox>) -> Option<(Mounted, UnboundedReceiver<vfs::Asked>)> {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let inbox = inbox?.clone();
    let mounter = inbox.mount.clone()?;
    let (asked, rx) = tokio::sync::mpsc::unbounded_channel();
    let folder = vfs::Folder::new(mounter.path().to_owned(), inbox.cache.clone(), asked);
    let mounting = folder.clone();
    let mounted = tokio::task::spawn_blocking(move || {
        inbox.prepare()?;
        inbox.prepare_cache()?;
        mounter.mount(mounting)
    })
    .await
    .map_err(io::Error::other)
    .flatten();
    match mounted {
        Ok(handle) => {
            log(format_args!("the virtual folder is mounted"));
            let handle = Some(handle);
            Some((Mounted { folder, handle }, rx))
        }
        Err(e) => {
            if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log(format_args!(
                    "no virtual folder ({e}): a paste of files fetches them all first"
                ));
            }
            None
        }
    }
}

/// Empties the folder of the files received over a link when dropped.
struct Emptying(Option<Inbox>);

impl Drop for Emptying {
    fn drop(&mut self) {
        if let Some(inbox) = &self.0 {
            inbox.empty();
        }
    }
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
/// `kept` is the machine's, from one link to the next. Needs a `LocalSet`.
pub async fn attach<C, S>(side: Side, start: S, mut wire: Wire, kept: Kept, shown: Shown)
where
    C: Compositor,
    S: Fn(UnboundedSender<Selected>) -> io::Result<C> + Clone + Send + 'static,
{
    let inbox = Inbox::of_this_user();
    // Left behind by a process that died before it emptied its folder, or unmounted its own.
    inbox.empty();
    inbox.empty_cache();
    if let Some(mount) = inbox.mount.clone() {
        let _ = tokio::task::spawn_blocking(move || mount.clean()).await;
    }
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
                    Some(inbox.clone()),
                    kept.clone(),
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
    use gliff_transport::Framed;
    use gliff_transport::clipboard::outbound_channel;
    use tokio::net::unix::pipe;
    use tokio::sync::mpsc;
    use tokio::task::LocalSet;

    use super::*;
    use crate::clipboard::progress::{Bar, CADENCE, Cancel};
    use crate::clipboard::vfs;
    use crate::link::{self, Heartbeat};
    use crate::notify;
    use crate::wire::LinkMsg;

    const TEXT: &str = "text/plain;charset=utf-8";
    const URI_LIST: &str = "text/uri-list";

    /// What a machine showed of the files crossing, and its cancel.
    #[derive(Clone)]
    struct Seen {
        bars: Arc<Mutex<Vec<Vec<Bar>>>>,
        notes: Arc<notify::Fake>,
        cancel: Arc<tokio::sync::watch::Sender<Cancel>>,
    }

    impl Seen {
        fn new() -> Self {
            Self {
                bars: Arc::default(),
                notes: Arc::default(),
                cancel: Arc::new(tokio::sync::watch::channel(Cancel::default()).0),
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

        /// The user clicks the bar of `transfer`.
        fn click(&self, transfer: u64) {
            self.cancel.send_modify(|cancel| {
                cancel.clicks += 1;
                cancel.transfer = transfer;
            });
        }

        fn bars(&self) -> Vec<Vec<Bar>> {
            self.bars.lock().unwrap().clone()
        }

        /// Whether the last bars shown are none.
        fn no_bar(&self) -> bool {
            self.bars().last().is_some_and(Vec::is_empty)
        }

        /// How many notifications shown so far have a summary that starts with `start`.
        fn times_told(&self, start: &str) -> usize {
            let shown = self.notes.shown();
            shown.iter().filter(|(s, _)| s.starts_with(start)).count()
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

    /// Whether a mode runs on each new link, and what the display of the link up now says.
    type ModeSaid = (
        Cell<bool>,
        RefCell<Option<tokio::sync::watch::Sender<bool>>>,
    );

    /// One machine of a test desk: its clipboard and the way to act on it.
    struct Machine {
        clipboard: Arc<Fake>,
        /// The way to its bridge of the connection up now.
        events: Rc<RefCell<mpsc::UnboundedSender<Selected>>>,
        seen: Seen,
        /// Its display's end of the link, once it is up.
        display: tokio::sync::Mutex<mpsc::UnboundedReceiver<crate::display::wire::Peer>>,
        /// Its end of the link: aborting it drops the link.
        link: tokio::task::JoinHandle<()>,
        /// Whether a mode runs, as its display says it on each link: from the link's start
        /// unless the test says otherwise, so that what crosses during a mode is tested alone.
        mode: Rc<ModeSaid>,
    }

    impl Machine {
        /// The user copies.
        fn copy(&self, items: &[(&str, &[u8])]) {
            let mime_types = self.clipboard.select(items);
            self.send(Selected::Selection { mime_types });
        }

        fn send(&self, event: Selected) {
            self.events.borrow().send(event).unwrap();
        }

        /// An application pastes `mime_type`; what it reads.
        async fn paste(&self, mime_type: &str) -> Vec<u8> {
            self.paste_for(mime_type, Duration::from_secs(10)).await
        }

        /// The same, within `limit`.
        async fn paste_for(&self, mime_type: &str, limit: Duration) -> Vec<u8> {
            let (reader, writer) = std::io::pipe().unwrap();
            self.send(Selected::Paste {
                mime_type: mime_type.into(),
                fd: writer.into(),
            });
            let mut reader = pipe::Receiver::from_owned_fd(reader.into()).unwrap();
            let mut data = Vec::new();
            tokio::time::timeout(limit, reader.read_to_end(&mut data))
                .await
                .expect("the paste in time")
                .unwrap();
            data
        }

        /// An application asks for `mime_type` and stops waiting before any answer, like a
        /// file manager whose clipboard read gives up after two seconds.
        fn paste_and_give_up(&self, mime_type: &str) {
            let (reader, writer) = std::io::pipe().unwrap();
            drop(reader);
            self.send(Selected::Paste {
                mime_type: mime_type.into(),
                fd: writer.into(),
            });
        }

        /// A mode starts on its link (`true`) or stops.
        fn mode(&self, on: bool) {
            self.mode.0.set(on);
            if let Some(mode) = &*self.mode.1.borrow() {
                mode.send_replace(on);
            }
        }

        /// Its link is up, once more.
        async fn connected(&self) {
            within("the link", self.display.lock().await.recv()).await;
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
    /// A mode runs on it from the start.
    fn desk(folders: &std::path::Path) -> (Machine, Machine) {
        let (home_end, remote_end) = tokio::io::duplex(64 * 1024);
        let home = machine(Side::Client, home_end, Inbox::new(folders.join("home")));
        let remote = machine(Side::Server, remote_end, Inbox::new(folders.join("remote")));
        (home, remote)
    }

    /// The same, with the link up and no mode.
    fn quiet_desk(folders: &std::path::Path) -> (Machine, Machine) {
        let (home, remote) = desk(folders);
        home.mode(false);
        remote.mode(false);
        (home, remote)
    }

    /// A machine and the raw end of its link, where a test plays the peer; no mode runs.
    fn alone(side: Side, folder: &std::path::Path) -> (Machine, Framed<tokio::io::DuplexStream>) {
        let (end, peer) = tokio::io::duplex(64 * 1024);
        let machine = machine(side, end, Inbox::new(folder.into()));
        machine.mode(false);
        (machine, Framed::new(peer))
    }

    /// The next clipboard message the machine sent the peer.
    async fn next_clip(peer: &mut Framed<tokio::io::DuplexStream>) -> LinkMsg {
        within("a clipboard message", async {
            loop {
                let msg = peer.read_msg::<LinkMsg>().await.unwrap();
                if let LinkMsg::ClipboardData { data_len, .. } = &msg {
                    peer.read_payload(*data_len).await.unwrap();
                }
                if msg.clone().into_clipboard().is_some() {
                    return msg;
                }
            }
        })
        .await
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
        // What the user does before the link is up waits for its first bridge.
        let (events, first) = mpsc::unbounded_channel();
        let (events, first) = (Rc::new(RefCell::new(events)), RefCell::new(Some(first)));
        let (displays, display) = mpsc::unbounded_channel();
        let seen = Seen::new();
        let kept = Kept::default();
        let mode = Rc::new((Cell::new(true), RefCell::new(None)));
        let (bridge, shown, its_events) = (clipboard.clone(), seen.shown(), events.clone());
        let its_mode = mode.clone();
        let service: Service = Rc::new(move |mut wire: Wire| {
            let peer = wire.display.take().unwrap();
            peer.mode.send_replace(its_mode.0.get());
            *its_mode.1.borrow_mut() = Some(peer.mode.clone());
            let _ = displays.send(peer);
            let selected = first.borrow_mut().take().unwrap_or_else(|| {
                let (to_bridge, selected) = mpsc::unbounded_channel();
                *its_events.borrow_mut() = to_bridge;
                selected
            });
            let (bridge, inbox, shown) = (bridge.clone(), inbox.clone(), shown.clone());
            let kept = kept.clone();
            Box::pin(async move {
                let inbox = Some(inbox);
                super::run(side, &mut wire, bridge, selected, inbox, kept, shown).await;
            })
        });
        Machine {
            clipboard,
            events,
            seen,
            display: tokio::sync::Mutex::new(display),
            link: link(service),
            mode,
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

    const HTML: &str = "text/html";

    /// Long enough for a copy to be offered, were it offered: a newer copy made before
    /// replaces it unsent.
    async fn settled() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// A copy made with no mode sends nothing: the first clipboard message the peer gets is
    /// the offer of the copy made once the mode started.
    async fn a_copy_with_no_mode_sends_nothing(side: Side) {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (machine, mut peer) = alone(side, tmp.path());
                machine.copy(&[(HTML, b"<p>salaries</p>")]);
                settled().await;
                machine.mode(true);
                machine.copy(&[(TEXT, b"meeting at ten")]);
                let LinkMsg::ClipboardOffer { mime_types, .. } = next_clip(&mut peer).await else {
                    panic!("an offer first");
                };
                assert!(
                    mime_types.iter().any(|m| m == TEXT) && !mime_types.iter().any(|m| m == HTML),
                    "{mime_types:?}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn with_the_link_up_and_no_mode_a_copy_at_home_sends_nothing() {
        a_copy_with_no_mode_sends_nothing(Side::Client).await;
    }

    #[tokio::test]
    async fn with_the_link_up_and_no_mode_a_copy_on_the_remote_sends_nothing() {
        a_copy_with_no_mode_sends_nothing(Side::Server).await;
    }

    #[tokio::test]
    async fn after_mode_start_a_copy_crosses() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = quiet_desk(tmp.path());
                home.connected().await;
                home.mode(true);
                remote.mode(true);
                home.copy(&[(TEXT, b"meeting at ten")]);
                remote.offered(1).await;
                assert_eq!(remote.paste(TEXT).await, b"meeting at ten");
                remote.copy(&[(TEXT, b"room 4")]);
                home.offered(1).await;
                assert_eq!(home.paste(TEXT).await, b"room 4");
            })
            .await;
    }

    #[tokio::test]
    async fn a_copy_made_before_mode_start_does_not_cross_at_start() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = quiet_desk(tmp.path());
                home.connected().await;
                home.copy(&[(HTML, b"<p>salaries</p>")]);
                settled().await;
                home.mode(true);
                remote.mode(true);
                home.copy(&[(TEXT, b"meeting at ten")]);
                let offer = remote.offered(1).await;
                assert!(!offer.iter().any(|mime| mime == HTML), "{offer:?}");
                assert_eq!(remote.paste(TEXT).await, b"meeting at ten");
                assert_eq!(remote.clipboard.offered.lock().unwrap().len(), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn after_mode_stop_a_copy_no_longer_crosses() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                home.copy(&[(TEXT, b"meeting at ten")]);
                remote.offered(1).await;
                home.mode(false);
                remote.mode(false);
                // What the remote held of home's copy goes with the mode.
                assert_eq!(remote.offered(2).await, Vec::<String>::new());
                home.copy(&[(HTML, b"<p>salaries</p>")]);
                settled().await;
                home.mode(true);
                remote.mode(true);
                home.copy(&[(TEXT, b"room 4")]);
                let offer = remote.offered(3).await;
                assert!(!offer.iter().any(|mime| mime == HTML), "{offer:?}");
                assert_eq!(remote.paste(TEXT).await, b"room 4");
            })
            .await;
    }

    #[tokio::test]
    async fn the_mode_s_stop_takes_the_peer_s_copy_off_the_clipboard() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                remote.copy(&[(TEXT, b"meeting at ten")]);
                home.offered(1).await;
                home.mode(false);
                assert_eq!(home.offered(2).await, Vec::<String>::new());
            })
            .await;
    }

    #[tokio::test]
    async fn the_mode_s_stop_leaves_a_copy_made_here_since_on_the_clipboard() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, mut peer) = alone(Side::Client, tmp.path());
                home.mode(true);
                peer.write_msg(&LinkMsg::ClipboardOffer {
                    serial: 7,
                    mime_types: vec![TEXT.into()],
                    files: Vec::new(),
                })
                .await
                .unwrap();
                home.offered(1).await;
                home.copy(&[(TEXT, b"mine")]);
                let LinkMsg::ClipboardOffer { serial, .. } = next_clip(&mut peer).await else {
                    panic!("the offer of the copy");
                };
                home.mode(false);
                // Refused, the request shows the stop handled.
                peer.write_msg(&LinkMsg::ClipboardRequest {
                    id: 1,
                    serial,
                    item: ClipboardItem::Mime(TEXT.into()),
                })
                .await
                .unwrap();
                assert_eq!(
                    next_clip(&mut peer).await,
                    LinkMsg::ClipboardAbort { id: 1 }
                );
                assert_eq!(home.clipboard.offered.lock().unwrap().len(), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn outside_a_mode_the_remote_refuses_what_home_asks_and_offers() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (remote, mut home) = alone(Side::Server, tmp.path());
                remote.mode(true);
                remote.copy(&[(TEXT, b"meeting at ten")]);
                let LinkMsg::ClipboardOffer { serial, .. } = next_clip(&mut home).await else {
                    panic!("an offer first");
                };
                remote.mode(false);
                // A home that asks anyway: its offer is not put on the clipboard, and its
                // request is refused, in that order.
                home.write_msg(&LinkMsg::ClipboardOffer {
                    serial: 9,
                    mime_types: vec![TEXT.into()],
                    files: Vec::new(),
                })
                .await
                .unwrap();
                home.write_msg(&LinkMsg::ClipboardRequest {
                    id: 1,
                    serial,
                    item: ClipboardItem::Mime(TEXT.into()),
                })
                .await
                .unwrap();
                assert_eq!(
                    next_clip(&mut home).await,
                    LinkMsg::ClipboardAbort { id: 1 }
                );
                assert!(remote.clipboard.offered.lock().unwrap().is_empty());
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

    /// Home copies `name` of `copied` (made with `content`); the remote pastes it. The paths it
    /// read.
    async fn copy_and_paste(
        home: &Machine,
        remote: &Machine,
        copied: &std::path::Path,
        name: &str,
        content: &[u8],
        offers: usize,
    ) -> Vec<std::path::PathBuf> {
        std::fs::write(copied.join(name), content).unwrap();
        let list = uri_list_of(&[copied.join(name)]);
        home.copy(&[(URI_LIST, list.as_bytes())]);
        remote.offered(offers).await;
        parse_uri_list(&String::from_utf8(remote.paste(URI_LIST).await).unwrap())
    }

    #[tokio::test]
    async fn a_newer_copy_removes_the_files_of_the_one_it_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        let folder = tmp.path().join("remote");
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                let first = copy_and_paste(&home, &remote, &copied, "first.bin", b"one", 1).await;
                assert_eq!(first, [folder.join("first.bin")]);
                assert!(first[0].exists());

                // Copied again, not pasted yet: the old files are already gone.
                let list = uri_list_of(&[copied.join("second.bin")]);
                std::fs::write(copied.join("second.bin"), b"two").unwrap();
                home.copy(&[(URI_LIST, list.as_bytes())]);
                remote.offered(2).await;
                assert!(tree(&folder).is_empty(), "{:?}", tree(&folder));

                let pasted = String::from_utf8(remote.paste(URI_LIST).await).unwrap();
                assert_eq!(parse_uri_list(&pasted), [folder.join("second.bin")]);
                assert_eq!(
                    tree(&folder),
                    [("second.bin".into(), Some(b"two".to_vec()))]
                );
            })
            .await;
    }

    #[tokio::test]
    async fn the_same_copy_offered_again_keeps_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        let folder = tmp.path().join("remote");
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                let first = copy_and_paste(&home, &remote, &copied, "report.bin", b"data", 1).await;
                // The same files copied again: the same offer, and no second fetch.
                let list = uri_list_of(&[copied.join("report.bin")]);
                home.copy(&[(URI_LIST, list.as_bytes())]);
                remote.offered(2).await;
                assert_eq!(
                    tree(&folder),
                    [("report.bin".into(), Some(b"data".to_vec()))]
                );
                let again = String::from_utf8(remote.paste(URI_LIST).await).unwrap();
                assert_eq!(parse_uri_list(&again), first);
            })
            .await;
    }

    #[tokio::test]
    async fn the_files_of_the_current_copy_stay_until_it_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        let folder = tmp.path().join("remote");
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                let first = copy_and_paste(&home, &remote, &copied, "report.bin", b"data", 1).await;
                // Pasted twice, one after the other: the application copies from there when it
                // likes.
                tokio::time::sleep(Duration::from_millis(50)).await;
                let again = String::from_utf8(remote.paste(URI_LIST).await).unwrap();
                assert_eq!(parse_uri_list(&again), first);
                assert_eq!(std::fs::read(&first[0]).unwrap(), b"data");
                assert_eq!(tree(&folder).len(), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn an_application_s_private_types_stay_on_its_machine() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                home.copy(&[
                    (TEXT, b"note"),
                    ("text/html", b"<b>note</b>"),
                    ("image/png", b"\x89PNG"),
                    ("application/x-example-token", b"pid 4242"),
                ]);
                let offer = remote.offered(1).await;
                for standard in [TEXT, "text/plain", "UTF8_STRING", "text/html", "image/png"] {
                    assert!(offer.iter().any(|m| m == standard), "{standard}: {offer:?}");
                }
                assert!(
                    !offer.iter().any(|m| m == "application/x-example-token"),
                    "{offer:?}"
                );
            })
            .await;
    }

    /// A link between the two machines, cut when the cable is dropped.
    struct Cable([tokio::task::JoinHandle<()>; 2]);

    impl Drop for Cable {
        fn drop(&mut self) {
            self.0.iter().for_each(|carry| carry.abort());
        }
    }

    /// Plugs a new cable between the ends each machine's link takes in turn.
    fn plug(
        home: &mpsc::UnboundedSender<tokio::io::DuplexStream>,
        remote: &mpsc::UnboundedSender<tokio::io::DuplexStream>,
    ) -> Cable {
        let (home_end, home_line) = tokio::io::duplex(64 * 1024);
        let (remote_line, remote_end) = tokio::io::duplex(64 * 1024);
        let (home_read, home_write) = tokio::io::split(home_line);
        let (remote_read, remote_write) = tokio::io::split(remote_line);
        home.send(home_end).unwrap();
        remote.send(remote_end).unwrap();
        Cable([
            tokio::task::spawn_local(carry(home_read, remote_write)),
            tokio::task::spawn_local(carry(remote_read, home_write)),
        ])
    }

    /// A machine whose link comes up again over each end it is given.
    fn machine_over(
        side: Side,
        inbox: Inbox,
        mut ends: mpsc::UnboundedReceiver<tokio::io::DuplexStream>,
    ) -> Machine {
        machine_on(side, inbox, |service| {
            tokio::task::spawn_local(async move {
                while let Some(end) = ends.recv().await {
                    let service = service.clone();
                    link::run(end, heartbeat(), move |wire| service(wire)).await;
                }
            })
        })
    }

    /// Home and the remote machine, each keeping what it keeps from one link to the next, and
    /// the way to plug a new link between them. Each machine's receiving folder is `home` or
    /// `remote` in `folders`.
    fn reconnecting_desk(folders: &std::path::Path) -> (Machine, Machine, impl Fn() -> Cable) {
        let (home_ends, ends) = mpsc::unbounded_channel();
        let home = machine_over(Side::Client, Inbox::new(folders.join("home")), ends);
        let (remote_ends, ends) = mpsc::unbounded_channel();
        let remote = machine_over(Side::Server, Inbox::new(folders.join("remote")), ends);
        (home, remote, move || plug(&home_ends, &remote_ends))
    }

    /// The remote copies `list` again on a new link, and home pastes it; what it read.
    async fn after_a_reconnect(
        home: &Machine,
        remote: &Machine,
        cable: Cable,
        plug: impl Fn() -> Cable,
        list: &str,
    ) -> String {
        // The lid closes and opens: a new link, and the remote's compositor tells its new
        // bridge the selection it still holds.
        drop(cable);
        let _cable = plug();
        remote.connected().await;
        remote.copy(&[(URI_LIST, list.as_bytes())]);
        home.offered(2).await;
        String::from_utf8(home.paste(URI_LIST).await).unwrap()
    }

    #[tokio::test]
    async fn the_end_of_the_link_empties_the_receiving_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        std::fs::write(copied.join("report.bin"), b"figures").unwrap();
        let list = uri_list_of(&[copied.join("report.bin")]);
        let folder = tmp.path().join("home");
        LocalSet::new()
            .run_until(async {
                let (home, remote, plug) = reconnecting_desk(tmp.path());
                let cable = plug();
                remote.connected().await;
                remote.copy(&[(URI_LIST, list.as_bytes())]);
                home.offered(1).await;
                home.paste(URI_LIST).await;
                assert_eq!(
                    tree(&folder),
                    [("report.bin".into(), Some(b"figures".to_vec()))]
                );

                // The mode stops, or the lid closes: the files are not kept for the next link.
                drop(cable);
                within("the folder emptied", async {
                    while !tree(&folder).is_empty() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
                assert!(folder.is_dir());
            })
            .await;
    }

    #[tokio::test]
    async fn the_mode_s_stop_empties_the_receiving_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        std::fs::write(copied.join("report.bin"), b"figures").unwrap();
        let list = uri_list_of(&[copied.join("report.bin")]);
        let folder = tmp.path().join("home");
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                remote.copy(&[(URI_LIST, list.as_bytes())]);
                home.offered(1).await;
                home.paste(URI_LIST).await;
                assert_eq!(
                    tree(&folder),
                    [("report.bin".into(), Some(b"figures".to_vec()))]
                );

                home.mode(false);
                within("the folder emptied", async {
                    while !tree(&folder).is_empty() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
                assert!(folder.is_dir());
            })
            .await;
    }

    #[tokio::test]
    async fn a_reconnect_fetches_the_copy_again_into_the_emptied_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        std::fs::write(copied.join("report.bin"), b"figures").unwrap();
        let list = uri_list_of(&[copied.join("report.bin")]);
        let folder = tmp.path().join("home");
        LocalSet::new()
            .run_until(async {
                let (home, remote, plug) = reconnecting_desk(tmp.path());
                let cable = plug();
                remote.connected().await;
                remote.copy(&[(URI_LIST, list.as_bytes())]);
                home.offered(1).await;
                home.paste(URI_LIST).await;

                let again = after_a_reconnect(&home, &remote, cable, plug, &list).await;
                assert_eq!(parse_uri_list(&again), [folder.join("report.bin")]);
                assert_eq!(
                    tree(&folder),
                    [("report.bin".into(), Some(b"figures".to_vec()))]
                );
            })
            .await;
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
                    Kept::default(),
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
                    |wire| {
                        super::attach(Side::Client, hypr(missing), wire, Kept::default(), quiet())
                    }
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
                    let kept = Kept::default();
                    super::run(
                        Side::Client,
                        &mut wire,
                        bridge,
                        selected,
                        Some(inbox),
                        kept,
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
                    super::attach(Side::Client, slow, wire, Kept::default(), quiet())
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
                    super::attach(Side::Client, gone, wire, Kept::default(), quiet())
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
                    super::attach(Side::Client, missing, wire, Kept::default(), quiet())
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
                remote.send(Selected::Paste {
                    mime_type: TEXT.into(),
                    fd: writer.into(),
                });
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
        let list = uri_list_of(&[copied.join("report.bin")]);
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                home.copy(&[(URI_LIST, list.as_bytes())]);
                remote.offered(1).await;
                // A new offer empties the folder, so the name is taken after it.
                std::fs::create_dir_all(&folder).unwrap();
                std::fs::write(folder.join("report.bin"), b"mine").unwrap();
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
                    free: |_| Ok(1024),
                    ..Inbox::new(folder.clone())
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
                    Kept::default(),
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

    #[tokio::test]
    async fn a_paste_of_files_whose_sizes_overflow_fails_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("inbox");
        LocalSet::new()
            .run_until(async {
                let (out, _sent) = outbound_channel();
                let seen = Seen::new();
                let bridge = Bridge::new(
                    Transfers::new(out, Side::Client),
                    Jobs::new(|_, _| {}),
                    Arc::new(Fake::default()),
                    Some(Inbox::new(folder.clone())),
                    Kept::default(),
                    &seen.shown(),
                );
                // As a peer would send it past the link's check: two files of 2^63 bytes.
                let half = |path: &str| ClipboardFile {
                    path: path.into(),
                    size: 1 << 63,
                    dir: false,
                };
                let offer = ClipboardMsg::Offer {
                    serial: 1,
                    mime_types: Vec::new(),
                    files: vec![half("a.bin"), half("b.bin")],
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
                assert!(tree(&folder).is_empty(), "{:?}", tree(&folder));
                seen.told("Paste failed").await;
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
            while !seen.no_bar() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        let bars = seen.bars();
        assert!(
            bars.iter().flatten().all(|bar| bar.total == total),
            "{bars:?}"
        );
        assert!(bars.iter().any(|bars| !bars.is_empty()), "{bars:?}");
    }

    /// Home copies the files `names` of `tmp/copied` the way a file manager does: the file
    /// lists and the text types, all naming home's paths.
    fn copy_files_with_text(home: &Machine, copied: &std::path::Path, names: &[&str]) {
        let paths: Vec<_> = names.iter().map(|n| copied.join(n)).collect();
        let list = uri_list_of(&paths);
        let text = paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        home.copy(&[
            (URI_LIST, list.as_bytes()),
            (TEXT, text.as_bytes()),
            ("text/plain", text.as_bytes()),
            ("UTF8_STRING", text.as_bytes()),
            ("STRING", text.as_bytes()),
            ("TEXT", text.as_bytes()),
        ]);
    }

    #[tokio::test]
    async fn the_text_of_a_copy_of_files_names_the_receiving_machine_s_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        std::fs::write(copied.join("a.txt"), b"first").unwrap();
        std::fs::write(copied.join("b.txt"), b"second").unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                copy_files_with_text(&home, &copied, &["a.txt", "b.txt"]);
                let offer = remote.offered(1).await;
                for text in [TEXT, "text/plain", "UTF8_STRING", "STRING", "TEXT"] {
                    assert!(offer.iter().any(|m| m == text), "{text}: {offer:?}");
                }
                // Only the text types are read, as a file manager does when it pastes: they
                // fetch the files the same way the file list does.
                let text = String::from_utf8(remote.paste(TEXT).await).unwrap();
                let here = tmp.path().join("remote");
                let expected = format!(
                    "{}\n{}",
                    here.join("a.txt").display(),
                    here.join("b.txt").display()
                );
                assert_eq!(text, expected);
                assert_eq!(std::fs::read(here.join("a.txt")).unwrap(), b"first");
                assert_eq!(std::fs::read(here.join("b.txt")).unwrap(), b"second");
                // Every text type gives the same paths, from the files already in.
                for other in ["text/plain", "UTF8_STRING", "STRING", "TEXT"] {
                    assert_eq!(remote.paste(other).await, expected.as_bytes(), "{other}");
                }
                // And the file list names the same files.
                let list = String::from_utf8(remote.paste(URI_LIST).await).unwrap();
                assert_eq!(
                    parse_uri_list(&list),
                    [here.join("a.txt"), here.join("b.txt")]
                );
            })
            .await;
    }

    #[tokio::test]
    async fn the_text_of_a_copy_of_files_made_on_the_remote_names_home_s_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        std::fs::write(copied.join("a.txt"), b"first").unwrap();
        let list = uri_list_of(&[copied.join("a.txt")]);
        let text = copied.join("a.txt").display().to_string();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                remote.copy(&[(URI_LIST, list.as_bytes()), (TEXT, text.as_bytes())]);
                home.offered(1).await;
                let pasted = String::from_utf8(home.paste(TEXT).await).unwrap();
                assert_eq!(pasted, tmp.path().join("home/a.txt").display().to_string());
            })
            .await;
    }

    #[tokio::test]
    async fn a_text_copy_without_files_is_unchanged_by_the_files_rule() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                home.copy(&[(TEXT, b"/not/a/file/copy")]);
                remote.offered(1).await;
                assert_eq!(remote.paste(TEXT).await, b"/not/a/file/copy");
            })
            .await;
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
    async fn files_fetched_with_no_paste_behind_them_send_no_files_pasted_notification() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("report.bin");
        std::fs::write(&copied, b"figures").unwrap();
        let list = uri_list_of(std::slice::from_ref(&copied));
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                remote.copy(&[(URI_LIST, list.as_bytes())]);
                home.offered(1).await;
                // A file manager reads the copy's files as soon as it is offered, on its own.
                home.paste(URI_LIST).await;
                the_bar_went(&home.seen, 7).await;
                assert_eq!(home.seen.notes.summaries(), Vec::<String>::new());
            })
            .await;
    }

    /// A paste reads the copy as the fetch of a file manager does: nothing tells them apart,
    /// so a paste says nothing either; the bar shows the files crossing.
    #[tokio::test]
    async fn a_paste_that_fetches_files_sends_no_files_pasted_notification_either() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("report.bin");
        std::fs::write(&copied, b"figures").unwrap();
        let list = uri_list_of(std::slice::from_ref(&copied));
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                let pasted = paste_on_the_remote(&home, &remote, &list).await;
                assert_eq!(
                    parse_uri_list(&pasted),
                    [tmp.path().join("remote/report.bin")]
                );
                the_bar_went(&remote.seen, 7).await;
                remote.paste(URI_LIST).await;
                assert_eq!(remote.seen.notes.summaries(), Vec::<String>::new());
            })
            .await;
    }

    #[tokio::test]
    async fn files_that_arrive_after_the_application_stopped_waiting_are_offered_again() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("report.bin");
        std::fs::write(&copied, b"figures").unwrap();
        let list = uri_list_of(std::slice::from_ref(&copied));
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                remote.copy(&[(URI_LIST, list.as_bytes())]);
                let first = home.offered(1).await;
                home.paste_and_give_up(URI_LIST);
                the_bar_went(&home.seen, 7).await;
                // The clipboard changes again, so a file manager that watches it reads it again,
                // and this time gets the files at once.
                assert_eq!(home.offered(2).await, first);
                let pasted = String::from_utf8(home.paste(URI_LIST).await).unwrap();
                assert_eq!(
                    parse_uri_list(&pasted),
                    [tmp.path().join("home/report.bin")]
                );
                assert_eq!(home.clipboard.offered.lock().unwrap().len(), 2);
            })
            .await;
    }

    #[tokio::test]
    async fn files_that_arrive_after_a_new_copy_here_never_replace_it() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("report.bin");
        std::fs::write(&copied, b"figures").unwrap();
        let list = uri_list_of(std::slice::from_ref(&copied));
        LocalSet::new()
            .run_until(async {
                let (home, remote) = desk(tmp.path());
                remote.copy(&[(URI_LIST, list.as_bytes())]);
                home.offered(1).await;
                home.paste_and_give_up(URI_LIST);
                home.copy(&[(TEXT, b"a note")]);
                the_bar_went(&home.seen, 7).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert_eq!(home.clipboard.offered.lock().unwrap().len(), 1);
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

    /// A bridge pasting the peer's offer 1, a file of four chunks, with one of them written in
    /// its staging folder.
    struct LargePaste {
        /// The peer's end of the link.
        peer: mpsc::Sender<(ClipboardMsg, Bytes)>,
        sent: mpsc::Receiver<(ClipboardMsg, Bytes)>,
        seen: Seen,
        /// The application's end of the paste.
        reader: pipe::Receiver,
        /// The transfer of the file.
        id: u32,
        /// The bridge runs as long as its compositor does.
        _compositor: mpsc::UnboundedSender<Selected>,
    }

    async fn a_large_paste_under_way(folder: &std::path::Path) -> LargePaste {
        let (peer, from_peer) = mpsc::channel(16);
        let (to_peer, mut sent) = outbound_channel();
        let (mode, in_mode) = tokio::sync::watch::channel(true);
        let mut wire = Wire {
            from_peer,
            to_peer,
            display: None,
            audio: None,
            mode: in_mode,
        };
        let (events, selected) = mpsc::unbounded_channel();
        let (seen, clipboard) = (Seen::new(), Arc::new(Fake::default()));
        let (shown, bridge, inbox) = (seen.shown(), clipboard.clone(), folder.to_path_buf());
        tokio::task::spawn_local(async move {
            let _mode = mode;
            let inbox = Some(Inbox::new(inbox));
            let kept = Kept::default();
            super::run(
                Side::Server,
                &mut wire,
                bridge,
                selected,
                inbox,
                kept,
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
        assert_eq!(tree(folder).len(), 2, "the staging folder and its file");
        LargePaste {
            peer,
            sent,
            seen,
            reader: pipe::Receiver::from_owned_fd(reader.into()).unwrap(),
            id,
            _compositor: events,
        }
    }

    /// The paste of `paste` ends with nothing for the application, and the peer is told to stop.
    async fn the_paste_stopped(paste: &mut LargePaste, folder: &std::path::Path) {
        assert_eq!(
            next_sent(&mut paste.sent, |m| matches!(m, ClipboardMsg::Abort { .. })).await,
            ClipboardMsg::Abort { id: paste.id },
            "the peer is told to stop",
        );
        let mut pasted = Vec::new();
        within("the paste", paste.reader.read_to_end(&mut pasted))
            .await
            .unwrap();
        assert!(pasted.is_empty());
        assert!(tree(folder).is_empty(), "{:?}", tree(folder));
    }

    #[tokio::test]
    async fn a_cancel_stops_a_paste_and_leaves_no_half_written_file() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("inbox");
        LocalSet::new()
            .run_until(async {
                let mut paste = a_large_paste_under_way(&folder).await;
                paste.seen.click(Crossing::Paste(1).id());
                the_paste_stopped(&mut paste, &folder).await;
                paste.seen.told("Paste cancelled").await;
                assert!(paste.seen.no_bar());
            })
            .await;
    }

    #[tokio::test]
    async fn a_new_copy_during_a_large_fetch_stops_it_and_leaves_no_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("inbox");
        LocalSet::new()
            .run_until(async {
                let mut paste = a_large_paste_under_way(&folder).await;
                let newer = ClipboardMsg::Offer {
                    serial: 2,
                    mime_types: vec![TEXT.into()],
                    files: Vec::new(),
                };
                paste.peer.send((newer, Bytes::new())).await.unwrap();
                the_paste_stopped(&mut paste, &folder).await;
                within("the bar gone", async {
                    while !paste.seen.no_bar() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
                // The user stopped nothing: nothing is said.
                assert!(paste.seen.notes.shown().is_empty());
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
                    Kept::default(),
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
                bridge.cancel(Crossing::Serve(serial).id());
                let ack = ClipboardMsg::Ack {
                    id: 2,
                    received: CHUNK as u64,
                };
                bridge.on_peer_msg(ack, Bytes::new());
                assert_eq!(
                    next_sent(&mut sent, |m| !matches!(m, ClipboardMsg::Data { .. })).await,
                    ClipboardMsg::Abort { id: 2 },
                );
                assert!(seen.no_bar());
                // Nothing this machine pasted was stopped: it says nothing.
                assert!(seen.notes.shown().is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn the_same_files_copied_again_after_a_cancel_are_served_again() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("report.bin");
        std::fs::write(&copied, vec![9; 16 * CHUNK]).unwrap();
        let list = uri_list_of(std::slice::from_ref(&copied));
        LocalSet::new()
            .run_until(async {
                let (out, mut sent) = outbound_channel();
                let clipboard = Arc::new(Fake::default());
                let bridge = Bridge::new(
                    Transfers::new(out, Side::Client),
                    Jobs::new(|_, _| {}),
                    clipboard.clone(),
                    None,
                    Kept::default(),
                    &quiet(),
                );
                let copy = async |sent: &mut mpsc::Receiver<(ClipboardMsg, Bytes)>| {
                    let mime_types = clipboard.select(&[(URI_LIST, list.as_bytes())]);
                    bridge.on_compositor_event(Selected::Selection { mime_types });
                    let offer = next_sent(sent, |m| matches!(m, ClipboardMsg::Offer { .. }));
                    let ClipboardMsg::Offer { serial, .. } = offer.await else {
                        unreachable!()
                    };
                    serial
                };
                let serial = copy(&mut sent).await;
                let request = |id| ClipboardMsg::Request {
                    id,
                    serial,
                    item: ClipboardItem::File(0),
                };
                bridge.on_peer_msg(request(2), Bytes::new());
                next_sent(&mut sent, |m| matches!(m, ClipboardMsg::Data { .. })).await;
                // The user stops it while it crosses, then copies the same files again.
                bridge.cancel(Crossing::Serve(serial).id());
                assert_eq!(copy(&mut sent).await, serial, "the same copy, unchanged");
                bridge.on_peer_msg(request(4), Bytes::new());
                let served = next_sent(&mut sent, |m| {
                    matches!(
                        m,
                        ClipboardMsg::Data { id: 4, .. } | ClipboardMsg::Abort { id: 4 }
                    )
                })
                .await;
                assert!(
                    matches!(served, ClipboardMsg::Data { id: 4, .. }),
                    "{served:?}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn a_cancel_between_two_files_refuses_the_next_one() {
        let tmp = tempfile::tempdir().unwrap();
        let (first, second) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let list = uri_list_of(&[first, second]);
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
                    Kept::default(),
                    &seen.shown(),
                );
                let mime_types = clipboard.select(&[(URI_LIST, list.as_bytes())]);
                bridge.on_compositor_event(Selected::Selection { mime_types });
                let ClipboardMsg::Offer { serial, .. } =
                    next_sent(&mut sent, |m| matches!(m, ClipboardMsg::Offer { .. })).await
                else {
                    unreachable!()
                };
                let request = |id, file| ClipboardMsg::Request {
                    id,
                    serial,
                    item: ClipboardItem::File(file),
                };
                bridge.on_peer_msg(request(2, 0), Bytes::new());
                next_sent(&mut sent, |m| {
                    matches!(m, ClipboardMsg::Data { done: true, .. })
                })
                .await;
                // The first file is served whole; the user clicks before the peer asks for the
                // second.
                bridge.cancel(Crossing::Serve(serial).id());
                bridge.on_peer_msg(request(4, 1), Bytes::new());
                assert_eq!(
                    next_sent(&mut sent, |_| true).await,
                    ClipboardMsg::Abort { id: 4 },
                );
                tokio::time::sleep(CADENCE * 2).await;
                assert!(seen.no_bar());
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
                let (_mode, in_mode) = tokio::sync::watch::channel(true);
                let mut wire = Wire {
                    from_peer: inbound,
                    to_peer,
                    display: None,
                    audio: None,
                    mode: in_mode,
                };
                let kept = Kept::default();
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
                        kept.clone(),
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
    /// A mount that is none: the test reads the folder the way the kernel would.
    struct FakeMount {
        path: std::path::PathBuf,
        folders: Mutex<Vec<Arc<vfs::Folder>>>,
        /// The folders mounted now.
        mounted: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// A mount, until it is dropped.
    struct Unmounts(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for Unmounts {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl std::fmt::Debug for FakeMount {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.debug_tuple("FakeMount").field(&self.path).finish()
        }
    }

    impl vfs::Mounter for FakeMount {
        fn path(&self) -> &std::path::Path {
            &self.path
        }

        fn mount(&self, folder: Arc<vfs::Folder>) -> io::Result<Box<dyn Send>> {
            self.folders.lock().unwrap().push(folder);
            self.mounted
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(Unmounts(self.mounted.clone())))
        }

        fn clean(&self) {}
    }

    impl FakeMount {
        /// The folder mounted last, once it is.
        async fn folder(&self) -> Arc<vfs::Folder> {
            within("the mount", async {
                loop {
                    if let Some(folder) = self.folders.lock().unwrap().last() {
                        return folder.clone();
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
        }

        fn mounted(&self) -> usize {
            self.mounted.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// A receiving folder `inbox` in `folder`, with a virtual folder `files` beside it.
    fn reading(folder: &std::path::Path) -> (Inbox, Arc<FakeMount>) {
        let mount = Arc::new(FakeMount {
            path: folder.join("files"),
            folders: Mutex::default(),
            mounted: Arc::default(),
        });
        let inbox = Inbox {
            mount: Some(mount.clone()),
            ..Inbox::new(folder.join("inbox"))
        };
        (inbox, mount)
    }

    /// A machine that reads the peer's files through a virtual folder, and the raw end of its
    /// link, where a test plays the peer; a mode runs.
    fn alone_reading(
        side: Side,
        folder: &std::path::Path,
    ) -> (Machine, Framed<tokio::io::DuplexStream>, Arc<FakeMount>) {
        let (end, peer) = tokio::io::duplex(64 * 1024);
        let (inbox, mount) = reading(folder);
        let machine = machine(side, end, inbox);
        (machine, Framed::new(peer), mount)
    }

    /// Home and the remote machine, each reading the other's files through a virtual folder.
    fn desk_reading(
        folders: &std::path::Path,
    ) -> (Machine, Machine, Arc<FakeMount>, Arc<FakeMount>) {
        let (home_end, remote_end) = tokio::io::duplex(64 * 1024);
        let (home_inbox, home_mount) = reading(&folders.join("home"));
        let (remote_inbox, remote_mount) = reading(&folders.join("remote"));
        let home = machine(Side::Client, home_end, home_inbox);
        let remote = machine(Side::Server, remote_end, remote_inbox);
        (home, remote, home_mount, remote_mount)
    }

    /// An application reads `path` of `folder` whole, as the kernel asks for it.
    async fn read_file(folder: Arc<vfs::Folder>, path: std::path::PathBuf) -> Result<Vec<u8>, i32> {
        let rel = path.strip_prefix(folder.path()).map_err(|_| libc::ENOENT)?;
        let mut ino = 1;
        for name in rel {
            ino = folder.lookup(ino, name)?.ino;
        }
        folder.open(ino)?;
        let mut bytes = Vec::new();
        let read = loop {
            let (tx, rx) = tokio::sync::oneshot::channel();
            folder.read(ino, bytes.len() as u64, 128 * 1024, move |read| {
                let _ = tx.send(read);
            });
            match rx.await {
                Ok(Ok(piece)) if piece.is_empty() => break Ok(()),
                Ok(Ok(piece)) => bytes.extend(piece),
                Ok(Err(e)) => break Err(e),
                Err(_) => break Err(libc::EIO),
            }
        };
        folder.release(ino);
        read.map(|()| bytes)
    }

    /// The peer's next request, past the acks; its id and what it asks.
    async fn next_request(peer: &mut Framed<tokio::io::DuplexStream>) -> (u32, ClipboardItem) {
        loop {
            match next_clip(peer).await {
                LinkMsg::ClipboardRequest { id, item, .. } => return (id, item),
                LinkMsg::ClipboardAck { .. } => {}
                msg => panic!("a request, not {msg:?}"),
            }
        }
    }

    /// The peer's next message that is not an ack.
    async fn next_not_ack(peer: &mut Framed<tokio::io::DuplexStream>) -> LinkMsg {
        loop {
            match next_clip(peer).await {
                LinkMsg::ClipboardAck { .. } => {}
                msg => return msg,
            }
        }
    }

    /// Nothing more crosses from the machine to the peer, but acks.
    async fn nothing_more(peer: &mut Framed<tokio::io::DuplexStream>) {
        let more = tokio::time::timeout(Duration::from_millis(300), next_not_ack(peer)).await;
        assert!(more.is_err(), "{more:?}");
    }

    /// The peer sends `bytes` of its transfer `id` at `offset`, and its end when `done`.
    async fn send_data(
        peer: &mut Framed<tokio::io::DuplexStream>,
        id: u32,
        offset: u64,
        bytes: &[u8],
        done: bool,
    ) {
        let data = LinkMsg::ClipboardData {
            id,
            offset,
            data_len: bytes.len() as u32,
            done: false,
        };
        peer.write_msg_with_payloads(&data, &[bytes]).await.unwrap();
        if done {
            let end = LinkMsg::ClipboardData {
                id,
                offset: offset + bytes.len() as u64,
                data_len: 0,
                done: true,
            };
            peer.write_msg(&end).await.unwrap();
        }
    }

    /// The peer sends the rest of its transfer `id` of `bytes`, from `offset`, a chunk at a
    /// time, then its end.
    async fn send_rest(
        peer: &mut Framed<tokio::io::DuplexStream>,
        id: u32,
        bytes: &[u8],
        offset: usize,
    ) {
        let pieces: Vec<_> = bytes[offset..].chunks(CHUNK).collect();
        for (n, piece) in pieces.iter().enumerate() {
            let at = (offset + n * CHUNK) as u64;
            send_data(peer, id, at, piece, n + 1 == pieces.len()).await;
        }
    }

    /// The peer offers one file, `name`, of `size` bytes, as copy `serial`; the machine pastes
    /// its file list: the path it reads.
    async fn offer_a_file(
        machine: &Machine,
        peer: &mut Framed<tokio::io::DuplexStream>,
        serial: u32,
        name: &str,
        size: u64,
        offers: usize,
    ) -> std::path::PathBuf {
        let offer = LinkMsg::ClipboardOffer {
            serial,
            mime_types: Vec::new(),
            files: vec![ClipboardFile {
                path: name.into(),
                size,
                dir: false,
            }],
        };
        peer.write_msg(&offer).await.unwrap();
        machine.offered(offers).await;
        let pasted = machine.paste(URI_LIST).await;
        let paths = parse_uri_list(&String::from_utf8(pasted).unwrap());
        assert_eq!(paths.len(), 1);
        paths[0].clone()
    }

    /// Bytes that tell their offsets apart.
    fn bytes_of(size: usize, seed: u8) -> Vec<u8> {
        (0..size).map(|i| (i % 251) as u8 ^ seed).collect()
    }

    #[tokio::test]
    async fn copying_files_sends_no_file_bytes_over_the_link() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("big.bin");
        std::fs::write(&copied, vec![1; 4 * CHUNK]).unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, mut peer) = alone(Side::Client, &tmp.path().join("inbox"));
                home.mode(true);
                home.copy(&[(URI_LIST, uri_list_of(&[copied]).as_bytes())]);
                let LinkMsg::ClipboardOffer { files, .. } = next_clip(&mut peer).await else {
                    panic!("an offer");
                };
                assert_eq!(files.len(), 1);
                nothing_more(&mut peer).await;
            })
            .await;
    }

    #[tokio::test]
    async fn reading_the_list_sends_no_file_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (remote, mut home, mount) = alone_reading(Side::Server, tmp.path());
                let offer = LinkMsg::ClipboardOffer {
                    serial: 4,
                    mime_types: vec![TEXT.into()],
                    files: vec![
                        ClipboardFile {
                            path: "a.txt".into(),
                            size: 5,
                            dir: false,
                        },
                        ClipboardFile {
                            path: "big.bin".into(),
                            size: 3 * CHUNK as u64,
                            dir: false,
                        },
                    ],
                };
                home.write_msg(&offer).await.unwrap();
                remote.offered(1).await;
                // A file manager reads the list, in each type, and gets it at once.
                let pasted = remote.paste_for(URI_LIST, Duration::from_millis(500)).await;
                let paths = parse_uri_list(&String::from_utf8(pasted).unwrap());
                let folder = mount.folder().await;
                let copy = folder.path().join("4");
                assert_eq!(paths, [copy.join("a.txt"), copy.join("big.bin")]);
                let text = remote.paste_for(TEXT, Duration::from_millis(500)).await;
                assert_eq!(
                    String::from_utf8(text).unwrap(),
                    format!("{}\n{}", paths[0].display(), paths[1].display())
                );
                nothing_more(&mut home).await;
                assert!(remote.seen.bars().iter().all(Vec::is_empty));
            })
            .await;
    }

    #[tokio::test]
    async fn reading_a_file_fetches_exactly_its_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let content = bytes_of(CHUNK + 1000, 3);
        LocalSet::new()
            .run_until(async {
                let (remote, mut home, mount) = alone_reading(Side::Server, tmp.path());
                let offer = LinkMsg::ClipboardOffer {
                    serial: 4,
                    mime_types: Vec::new(),
                    files: vec![
                        ClipboardFile {
                            path: "a.txt".into(),
                            size: 5,
                            dir: false,
                        },
                        ClipboardFile {
                            path: "b.bin".into(),
                            size: content.len() as u64,
                            dir: false,
                        },
                    ],
                };
                home.write_msg(&offer).await.unwrap();
                remote.offered(1).await;
                let pasted = remote.paste(URI_LIST).await;
                let paths = parse_uri_list(&String::from_utf8(pasted).unwrap());
                let read =
                    tokio::task::spawn_local(read_file(mount.folder().await, paths[1].clone()));
                let (id, item) = next_request(&mut home).await;
                assert_eq!(item, ClipboardItem::File(1), "that file alone");
                send_rest(&mut home, id, &content, 0).await;
                assert!(within("the read", read).await.unwrap().unwrap() == content);
                nothing_more(&mut home).await;
            })
            .await;
    }

    #[tokio::test]
    async fn a_newer_copy_during_a_transfer_does_not_stop_it() {
        let tmp = tempfile::tempdir().unwrap();
        let content = bytes_of(4 * CHUNK, 7);
        LocalSet::new()
            .run_until(async {
                let (home, mut remote, mount) = alone_reading(Side::Client, tmp.path());
                let size = content.len() as u64;
                let path = offer_a_file(&home, &mut remote, 6, "big.bin", size, 1).await;
                let read = tokio::task::spawn_local(read_file(mount.folder().await, path));
                let (id, _) = next_request(&mut remote).await;
                send_data(&mut remote, id, 0, &content[..CHUNK], false).await;
                // Something else is copied on the remote meanwhile.
                let newer = LinkMsg::ClipboardOffer {
                    serial: 8,
                    mime_types: vec![TEXT.into()],
                    files: Vec::new(),
                };
                remote.write_msg(&newer).await.unwrap();
                home.offered(2).await;
                send_rest(&mut remote, id, &content, CHUNK).await;
                assert!(within("the read", read).await.unwrap().unwrap() == content);
                nothing_more(&mut remote).await;
            })
            .await;
    }

    /// Home reads two files of two copies of the remote's, each one chunk in: the two reads,
    /// their transfers, and the peer's end.
    async fn two_reads_under_way(
        home: &Machine,
        remote: &mut Framed<tokio::io::DuplexStream>,
        mount: &FakeMount,
        first: &[u8],
        second: &[u8],
    ) -> [(tokio::task::JoinHandle<Result<Vec<u8>, i32>>, u32); 2] {
        let a = offer_a_file(home, remote, 6, "a.bin", first.len() as u64, 1).await;
        let read_a = tokio::task::spawn_local(read_file(mount.folder().await, a));
        let (a_id, _) = next_request(remote).await;
        send_data(remote, a_id, 0, &first[..CHUNK], false).await;
        let b = offer_a_file(home, remote, 7, "b.bin", second.len() as u64, 2).await;
        let read_b = tokio::task::spawn_local(read_file(mount.folder().await, b));
        let (b_id, _) = next_request(remote).await;
        send_data(remote, b_id, 0, &second[..CHUNK], false).await;
        [(read_a, a_id), (read_b, b_id)]
    }

    /// The bars of the two transfers of copies 6 and 7, once home shows both.
    async fn two_bars(seen: &Seen) -> Vec<Bar> {
        let ids = [Crossing::Read(6).id(), Crossing::Read(7).id()];
        within("two bars", async {
            loop {
                if let Some(bars) = seen.bars().last()
                    && bars.iter().map(|b| b.id).eq(ids)
                    && bars.iter().all(|b| b.done >= CHUNK as u64)
                {
                    return bars.clone();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
    }

    #[tokio::test]
    async fn two_transfers_at_once_both_complete_each_with_its_own_bar() {
        let tmp = tempfile::tempdir().unwrap();
        let (first, second) = (bytes_of(2 * CHUNK, 1), bytes_of(3 * CHUNK, 2));
        LocalSet::new()
            .run_until(async {
                let (home, mut remote, mount) = alone_reading(Side::Client, tmp.path());
                let [(read_a, a), (read_b, b)] =
                    two_reads_under_way(&home, &mut remote, &mount, &first, &second).await;
                let bars = two_bars(&home.seen).await;
                assert_eq!(
                    (bars[0].total, bars[1].total),
                    (2 * CHUNK as u64, 3 * CHUNK as u64)
                );
                send_rest(&mut remote, b, &second, CHUNK).await;
                send_rest(&mut remote, a, &first, CHUNK).await;
                assert!(within("the first read", read_a).await.unwrap().unwrap() == first);
                assert!(within("the second read", read_b).await.unwrap().unwrap() == second);
                within("the bars gone", async {
                    while !home.seen.no_bar() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
            })
            .await;
    }

    #[tokio::test]
    async fn cancel_stops_one_transfer_and_leaves_the_other() {
        let tmp = tempfile::tempdir().unwrap();
        let (first, second) = (bytes_of(2 * CHUNK, 1), bytes_of(2 * CHUNK, 2));
        LocalSet::new()
            .run_until(async {
                let (home, mut remote, mount) = alone_reading(Side::Client, tmp.path());
                let [(read_a, a), (read_b, b)] =
                    two_reads_under_way(&home, &mut remote, &mount, &first, &second).await;
                two_bars(&home.seen).await;
                home.seen.click(Crossing::Read(6).id());
                assert_eq!(
                    next_not_ack(&mut remote).await,
                    LinkMsg::ClipboardAbort { id: a },
                    "the remote is told to stop serving the first"
                );
                assert_eq!(
                    within("the first read", read_a).await.unwrap(),
                    Err(libc::ECANCELED)
                );
                home.seen.told("Paste cancelled").await;
                send_rest(&mut remote, b, &second, CHUNK).await;
                assert!(within("the second read", read_b).await.unwrap().unwrap() == second);
            })
            .await;
    }

    #[tokio::test]
    async fn mode_stop_ends_every_transfer_and_unmounts() {
        let tmp = tempfile::tempdir().unwrap();
        let content = bytes_of(2 * CHUNK, 5);
        LocalSet::new()
            .run_until(async {
                let (home, mut remote, mount) = alone_reading(Side::Client, tmp.path());
                let size = content.len() as u64;
                let path = offer_a_file(&home, &mut remote, 6, "a.bin", size, 1).await;
                let read = tokio::task::spawn_local(read_file(mount.folder().await, path));
                let (id, _) = next_request(&mut remote).await;
                send_data(&mut remote, id, 0, &content[..CHUNK], false).await;
                assert_eq!(mount.mounted(), 1);
                // The remote's mode stops as well: its bridge, and what it serves, end with it.
                home.mode(false);
                assert_eq!(within("the read", read).await.unwrap(), Err(libc::EIO));
                within("the unmount", async {
                    while mount.mounted() != 0 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
            })
            .await;
    }

    async fn a_file_crosses_through_the_virtual_folder(copy_at_home: bool) {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("report.bin");
        let content = bytes_of(300_000, 9);
        std::fs::write(&copied, &content).unwrap();
        let list = uri_list_of(std::slice::from_ref(&copied));
        LocalSet::new()
            .run_until(async {
                let (home, remote, home_mount, remote_mount) = desk_reading(tmp.path());
                let (source, target, mount) = if copy_at_home {
                    (&home, &remote, &remote_mount)
                } else {
                    (&remote, &home, &home_mount)
                };
                source.copy(&[(URI_LIST, list.as_bytes())]);
                target.offered(1).await;
                let pasted = target.paste(URI_LIST).await;
                let paths = parse_uri_list(&String::from_utf8(pasted).unwrap());
                let folder = mount.folder().await;
                assert_eq!(paths.len(), 1);
                assert!(paths[0].starts_with(folder.path()), "{paths:?}");
                let read = read_file(folder, paths[0].clone()).await.unwrap();
                assert!(read == content);
            })
            .await;
    }

    #[tokio::test]
    async fn a_file_copied_at_home_is_read_on_the_remote_through_the_virtual_folder() {
        a_file_crosses_through_the_virtual_folder(true).await;
    }

    #[tokio::test]
    async fn a_file_copied_on_the_remote_is_read_at_home_through_the_virtual_folder() {
        a_file_crosses_through_the_virtual_folder(false).await;
    }

    #[tokio::test]
    async fn the_files_of_an_older_copy_are_still_served_after_a_newer_one() {
        let tmp = tempfile::tempdir().unwrap();
        let (older, newer) = (tmp.path().join("older.bin"), tmp.path().join("newer.txt"));
        let content = bytes_of(100_000, 4);
        std::fs::write(&older, &content).unwrap();
        std::fs::write(&newer, b"newer").unwrap();
        LocalSet::new()
            .run_until(async {
                let (home, remote, _, remote_mount) = desk_reading(tmp.path());
                home.copy(&[(URI_LIST, uri_list_of(&[older]).as_bytes())]);
                remote.offered(1).await;
                let pasted = remote.paste(URI_LIST).await;
                let paths = parse_uri_list(&String::from_utf8(pasted).unwrap());
                let folder = remote_mount.folder().await;
                // The file manager opened it, then something else is copied at home.
                let rel = paths[0].strip_prefix(folder.path()).unwrap();
                let dir = folder.lookup(1, rel.iter().next().unwrap()).unwrap();
                let ino = folder
                    .lookup(dir.ino, rel.iter().nth(1).unwrap())
                    .unwrap()
                    .ino;
                folder.open(ino).unwrap();
                home.copy(&[(URI_LIST, uri_list_of(&[newer]).as_bytes())]);
                remote.offered(2).await;
                let read = read_file(folder.clone(), paths[0].clone()).await.unwrap();
                assert!(read == content);
                folder.release(ino);
            })
            .await;
    }
    /// The peer offers the folder `photos` of `count` files of `size` bytes as copy `serial`;
    /// the machine pastes it: the folder's path.
    async fn offer_a_folder(
        machine: &Machine,
        peer: &mut Framed<tokio::io::DuplexStream>,
        serial: u32,
        count: usize,
        size: u64,
    ) -> std::path::PathBuf {
        let mut files = vec![ClipboardFile {
            path: "photos".into(),
            size: 0,
            dir: true,
        }];
        files.extend((0..count).map(|n| ClipboardFile {
            path: format!("photos/{n}.bin"),
            size,
            dir: false,
        }));
        let offer = LinkMsg::ClipboardOffer {
            serial,
            mime_types: Vec::new(),
            files,
        };
        peer.write_msg(&offer).await.unwrap();
        machine.offered(1).await;
        let pasted = machine.paste(URI_LIST).await;
        parse_uri_list(&String::from_utf8(pasted).unwrap())[0].clone()
    }

    #[tokio::test]
    async fn a_newer_copy_between_two_files_of_a_folder_pasted_stops_none_of_them() {
        let tmp = tempfile::tempdir().unwrap();
        let contents: Vec<_> = (0..3).map(|n| bytes_of(CHUNK + 10, n)).collect();
        LocalSet::new()
            .run_until(async {
                let (home, mut remote, mount) = alone_reading(Side::Client, tmp.path());
                let size = contents[0].len() as u64;
                let photos = offer_a_folder(&home, &mut remote, 6, 3, size).await;
                let folder = mount.folder().await;
                // A file manager copies the folder, one file after the other.
                for (n, content) in contents.iter().enumerate() {
                    let path = photos.join(format!("{n}.bin"));
                    let read = tokio::task::spawn_local(read_file(folder.clone(), path));
                    let (id, item) = next_request(&mut remote).await;
                    assert_eq!(item, ClipboardItem::File(n as u32 + 1));
                    send_rest(&mut remote, id, content, 0).await;
                    assert!(within("a file", read).await.unwrap().unwrap() == *content);
                    if n == 0 {
                        // Something else is copied on the remote meanwhile.
                        let newer = LinkMsg::ClipboardOffer {
                            serial: 8,
                            mime_types: vec![TEXT.into()],
                            files: Vec::new(),
                        };
                        remote.write_msg(&newer).await.unwrap();
                        home.offered(2).await;
                    }
                }
            })
            .await;
    }

    /// An application opens `path` of `folder`, reads its first `size` bytes, and closes it,
    /// as a file manager that looks at what a file holds.
    async fn sniff(folder: Arc<vfs::Folder>, path: std::path::PathBuf, size: u32) -> Vec<u8> {
        let rel = path.strip_prefix(folder.path()).unwrap();
        let ino = rel
            .iter()
            .fold(1, |ino, name| folder.lookup(ino, name).unwrap().ino);
        folder.open(ino).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        folder.read(ino, 0, size, move |read| {
            let _ = tx.send(read);
        });
        let read = rx.await.unwrap().unwrap();
        folder.release(ino);
        read
    }

    #[tokio::test]
    async fn a_file_closed_before_its_bytes_are_in_stops_its_fetch_and_drops_them() {
        let tmp = tempfile::tempdir().unwrap();
        let content = bytes_of(4 * CHUNK, 3);
        LocalSet::new()
            .run_until(async {
                let (home, mut remote, mount) = alone_reading(Side::Client, tmp.path());
                let size = content.len() as u64;
                let path = offer_a_file(&home, &mut remote, 6, "big.bin", size, 1).await;
                let folder = mount.folder().await;
                let sniffed = tokio::task::spawn_local(sniff(folder.clone(), path.clone(), 4096));
                let (id, _) = next_request(&mut remote).await;
                send_data(&mut remote, id, 0, &content[..CHUNK], false).await;
                assert_eq!(within("the sniff", sniffed).await.unwrap(), content[..4096]);
                // The next chunk finds nobody reading.
                send_data(
                    &mut remote,
                    id,
                    CHUNK as u64,
                    &content[CHUNK..2 * CHUNK],
                    false,
                )
                .await;
                assert_eq!(
                    next_not_ack(&mut remote).await,
                    LinkMsg::ClipboardAbort { id },
                    "the remote is told to stop serving it"
                );
                assert_eq!(home.seen.times_told("Paste failed"), 0);
                // A later read starts it again.
                let read = tokio::task::spawn_local(read_file(folder, path));
                let (again, _) = next_request(&mut remote).await;
                send_rest(&mut remote, again, &content, 0).await;
                assert!(within("the read", read).await.unwrap().unwrap() == content);
            })
            .await;
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_nothing_and_a_later_read_fetches_again() {
        let tmp = tempfile::tempdir().unwrap();
        let content = bytes_of(2 * CHUNK, 8);
        LocalSet::new()
            .run_until(async {
                let (home, mut remote, mount) = alone_reading(Side::Client, tmp.path());
                let size = content.len() as u64;
                let path = offer_a_file(&home, &mut remote, 6, "a.bin", size, 1).await;
                let folder = mount.folder().await;
                let ino = folder.lookup(1, "6".as_ref()).unwrap().ino;
                let ino = folder.lookup(ino, "a.bin".as_ref()).unwrap().ino;
                folder.open(ino).unwrap();
                // A read of the second chunk waits for it.
                let (tx, waiting) = tokio::sync::oneshot::channel();
                folder.read(ino, CHUNK as u64, 4096, move |read| {
                    let _ = tx.send(read);
                });
                let (id, _) = next_request(&mut remote).await;
                send_data(&mut remote, id, 0, &content[..CHUNK], false).await;
                // The remote stops in the middle of it.
                remote
                    .write_msg(&LinkMsg::ClipboardAbort { id })
                    .await
                    .unwrap();
                assert_eq!(within("the read", waiting).await.unwrap(), Err(libc::EIO));
                let read = tokio::task::spawn_local(read_file(folder.clone(), path));
                let (again, _) = next_request(&mut remote).await;
                send_rest(&mut remote, again, &content, 0).await;
                assert!(within("the read again", read).await.unwrap().unwrap() == content);
                folder.release(ino);
            })
            .await;
    }

    #[tokio::test]
    async fn a_read_needs_room_for_the_fetches_under_way_as_well() {
        let tmp = tempfile::tempdir().unwrap();
        let file = |path: &str| ClipboardFile {
            path: path.into(),
            size: 2 * CHUNK as u64,
            dir: false,
        };
        LocalSet::new()
            .run_until(async {
                let (end, peer) = tokio::io::duplex(64 * 1024);
                let (inbox, mount) = reading(tmp.path());
                // Room for one of the two files, and a half.
                let inbox = Inbox {
                    free: |_| Ok(3 * CHUNK as u64),
                    ..inbox
                };
                let (home, mut remote) = (machine(Side::Client, end, inbox), Framed::new(peer));
                let offer = LinkMsg::ClipboardOffer {
                    serial: 6,
                    mime_types: Vec::new(),
                    files: vec![file("a.bin"), file("b.bin")],
                };
                remote.write_msg(&offer).await.unwrap();
                home.offered(1).await;
                let pasted = home.paste(URI_LIST).await;
                let paths = parse_uri_list(&String::from_utf8(pasted).unwrap());
                let folder = mount.folder().await;
                let _a = tokio::task::spawn_local(read_file(folder.clone(), paths[0].clone()));
                next_request(&mut remote).await;
                let b = read_file(folder, paths[1].clone()).await;
                assert_eq!(b, Err(libc::ENOSPC));
                home.seen.told("Paste failed").await;
            })
            .await;
    }

    #[tokio::test]
    async fn a_file_read_is_kept_in_the_cache_folder_not_the_runtime_one() {
        let tmp = tempfile::tempdir().unwrap();
        let content = bytes_of(CHUNK, 2);
        LocalSet::new()
            .run_until(async {
                let (end, peer) = tokio::io::duplex(64 * 1024);
                let (inbox, mount) = reading(tmp.path());
                // The runtime folder has no room for it; the cache folder on disk has.
                let inbox = Inbox {
                    cache: tmp.path().join("cache"),
                    free: |dir| Ok(if dir.ends_with("cache") { 1 << 40 } else { 0 }),
                    ..inbox
                };
                let (home, mut remote) = (machine(Side::Client, end, inbox), Framed::new(peer));
                let size = content.len() as u64;
                let path = offer_a_file(&home, &mut remote, 6, "a.bin", size, 1).await;
                let read = tokio::task::spawn_local(read_file(mount.folder().await, path));
                let (id, _) = next_request(&mut remote).await;
                send_rest(&mut remote, id, &content, 0).await;
                assert!(within("the read", read).await.unwrap().unwrap() == content);
                assert!(tmp.path().join("cache").is_dir());
            })
            .await;
    }

    #[tokio::test]
    async fn a_read_asked_just_before_a_cancel_starts_no_new_transfer() {
        let tmp = tempfile::tempdir().unwrap();
        let first = bytes_of(2 * CHUNK, 1);
        LocalSet::new()
            .run_until(async {
                let (home, mut remote, mount) = alone_reading(Side::Client, tmp.path());
                let photos = offer_a_folder(&home, &mut remote, 6, 2, first.len() as u64).await;
                let folder = mount.folder().await;
                let read_a =
                    tokio::task::spawn_local(read_file(folder.clone(), photos.join("0.bin")));
                let (a, _) = next_request(&mut remote).await;
                send_data(&mut remote, a, 0, &first[..CHUNK], false).await;
                within("the bar", async {
                    while home.seen.no_bar() || home.seen.bars().is_empty() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
                // The next file is read, and the bar clicked, before the bridge sees either.
                let b = photos.join("1.bin");
                let rel = b.strip_prefix(folder.path()).unwrap();
                let ino = rel
                    .iter()
                    .fold(1, |ino, name| folder.lookup(ino, name).unwrap().ino);
                let (tx, read_b) = tokio::sync::oneshot::channel();
                folder.read(ino, 0, 4096, move |read| {
                    let _ = tx.send(read);
                });
                home.seen.click(Crossing::Read(6).id());
                assert_eq!(
                    next_not_ack(&mut remote).await,
                    LinkMsg::ClipboardAbort { id: a }
                );
                assert_eq!(
                    within("the first read", read_a).await.unwrap(),
                    Err(libc::ECANCELED)
                );
                assert_eq!(
                    within("the second read", read_b).await.unwrap(),
                    Err(libc::ECANCELED)
                );
                nothing_more(&mut remote).await;
            })
            .await;
    }

    #[tokio::test]
    async fn a_copy_gone_from_the_folder_leaves_no_reads_behind() {
        LocalSet::new()
            .run_until(async {
                let (out, _sent) = outbound_channel();
                let bridge = Bridge::new(
                    Transfers::new(out, Side::Client),
                    Jobs::new(|_, _| {}),
                    Arc::new(Fake::default()),
                    None,
                    Kept::default(),
                    &quiet(),
                );
                // Its last file read whole, its fetch over.
                bridge.reads.borrow_mut().insert(6, Reads::default());
                bridge.on_asked(vfs::Asked::Gone { serial: 6 });
                assert!(bridge.reads.borrow().is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn once_the_peer_stops_serving_a_copy_its_later_reads_say_so_once() {
        let tmp = tempfile::tempdir().unwrap();
        LocalSet::new()
            .run_until(async {
                let (remote, mut home, mount) = alone_reading(Side::Server, tmp.path());
                let path = offer_a_file(&remote, &mut home, 6, "a.bin", 2 * CHUNK as u64, 1).await;
                let folder = mount.folder().await;
                // Home's user cancelled its serving: each request is refused from now on.
                for _ in 0..3 {
                    let read = tokio::task::spawn_local(read_file(folder.clone(), path.clone()));
                    let (id, _) = next_request(&mut home).await;
                    home.write_msg(&LinkMsg::ClipboardAbort { id })
                        .await
                        .unwrap();
                    assert_eq!(within("the read", read).await.unwrap(), Err(libc::EIO));
                }
                remote.seen.told("Paste failed").await;
                assert_eq!(remote.seen.times_told("Paste failed"), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn the_bar_of_a_copy_counts_the_bytes_read_of_all_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (first, second) = (bytes_of(2 * CHUNK, 1), bytes_of(2 * CHUNK, 2));
        LocalSet::new()
            .run_until(async {
                let (home, mut remote, mount) = alone_reading(Side::Client, tmp.path());
                let file = |path: &str| ClipboardFile {
                    path: path.into(),
                    size: 2 * CHUNK as u64,
                    dir: false,
                };
                let offer = LinkMsg::ClipboardOffer {
                    serial: 6,
                    mime_types: Vec::new(),
                    files: vec![file("a.bin"), file("b.bin")],
                };
                remote.write_msg(&offer).await.unwrap();
                home.offered(1).await;
                let pasted = home.paste(URI_LIST).await;
                let paths = parse_uri_list(&String::from_utf8(pasted).unwrap());
                let folder = mount.folder().await;
                let read_a = tokio::task::spawn_local(read_file(folder.clone(), paths[0].clone()));
                let (a, _) = next_request(&mut remote).await;
                send_rest(&mut remote, a, &first, 0).await;
                assert!(within("the first file", read_a).await.unwrap().unwrap() == first);
                let read_b = tokio::task::spawn_local(read_file(folder, paths[1].clone()));
                let (b, _) = next_request(&mut remote).await;
                send_data(&mut remote, b, 0, &second[..CHUNK], false).await;
                let id = Crossing::Read(6).id();
                let three_of_four = Bar {
                    id,
                    done: 3 * CHUNK as u64,
                    total: 4 * CHUNK as u64,
                };
                within("the bar at three chunks of four", async {
                    while home.seen.bars().last() != Some(&vec![three_of_four]) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
                send_rest(&mut remote, b, &second, CHUNK).await;
                assert!(within("the second file", read_b).await.unwrap().unwrap() == second);
            })
            .await;
    }
    /// What the hand check of the virtual folder measures, between two paired instances over
    /// a loopback link, the remote reading through a real FUSE mount in a temporary folder:
    /// the copy and the read of its file list move nothing, then a read fetches the file whole,
    /// a copy of text in the middle stopping nothing. 2 GB under `target/` by default;
    /// `NAVETTE_BENCH_BYTES` sets another size. Needs FUSE and `fusermount3`.
    #[tokio::test]
    #[ignore = "a measure, a minute long, 2 GB on disk, and a real FUSE mount"]
    async fn a_large_file_read_through_a_real_mount_between_two_paired_instances() {
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
                let mount = tmp.path().join("files");
                let inbox = Inbox {
                    mount: Some(Arc::new(vfs::Fuse::at(mount.clone()))),
                    ..Inbox::new(tmp.path().join("r"))
                };
                let dialer = link::remote_dialer(&desk.remote).unwrap();
                let remote = machine_on(Side::Server, inbox, |service| {
                    tokio::task::spawn_local(link::agent(
                        dialer,
                        Heartbeat::default(),
                        link::Backoff::default(),
                        events,
                        move |wire| service(wire),
                    ))
                });
                remote.connected().await;
                let list = uri_list_of(std::slice::from_ref(&copied));
                home.copy(&[(URI_LIST, list.as_bytes())]);
                remote.offered(1).await;
                let asked = std::time::Instant::now();
                let pasted = remote.paste(URI_LIST).await;
                let answered = asked.elapsed();
                let paths = parse_uri_list(&String::from_utf8(pasted).unwrap());
                assert!(paths[0].starts_with(&mount), "{paths:?}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                let served_before = home.seen.bars().iter().flatten().count();
                let path = paths[0].clone();
                let read = tokio::task::spawn_blocking(move || {
                    let started = std::time::Instant::now();
                    let mut file = std::fs::File::open(path)?;
                    let got = std::io::copy(&mut file, &mut std::io::sink())?;
                    io::Result::Ok((got, started.elapsed()))
                });
                tokio::time::sleep(Duration::from_secs(2)).await;
                home.copy(&[(TEXT, b"meeting at ten")]);
                remote.offered(2).await;
                let in_the_middle = !read.is_finished();
                let (got, elapsed) = read.await.unwrap().unwrap();
                assert_eq!(got, bytes);
                assert_eq!(served_before, 0, "nothing served before the read");
                let rate = bytes as f64 / elapsed.as_secs_f64() / 1e6;
                eprintln!(
                    "list answered in {} ms, nothing served before the read; {bytes} bytes \
                     read through the mount in {:.1} s: {rate:.0} MB/s ({:.0} Mbit/s); a copy \
                     of text in the middle of it: {in_the_middle}",
                    answered.as_millis(),
                    elapsed.as_secs_f64(),
                    rate * 8.0,
                );
            })
            .await;
    }
}
