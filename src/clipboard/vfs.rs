//! The virtual folder (FUSE) where the files of the peer's copies show, the way remote desktop
//! clients do it. A paste of files gets their paths here at once, and nothing crosses: a file's
//! bytes are fetched from the peer only when an application reads it, as it reads it. A file
//! manager reads the file list of every new copy, to know whether it can paste: that read moves
//! no byte now.
//!
//! Each copy of the peer is a folder named by its serial, holding the copied files and folders.
//! The first read of a file asks the bridge to fetch it whole, in order, into a cache file of
//! the receiving folder (unlinked at once, so it goes with its last handle); each read waits
//! for its range, so an application may read in any order. A copy that a newer one replaced
//! stays while something of it is open, and while its transfer is under way: from the first
//! read of one of its files until it is cancelled, or until every file read is whole and
//! nothing of it was opened for [`LINGER`], since a file manager copying a folder opens its
//! files one after the other, with nothing open between two.
//!
//! The kernel's requests run on one thread of `fuser`'s, which never waits: a read whose bytes
//! are not here yet waits on a thread of its own, so a stuck read holds up neither the other
//! reads nor the link.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gliff_proto::ClipboardFile;
use gliff_proto::clipboard::safe_relative_path;

use super::progress::Headline;
use tokio::sync::mpsc::UnboundedSender;

/// The folder's own inode.
const ROOT: u64 = 1;

/// How long the kernel keeps what it was told of a name or a file.
const TTL: Duration = Duration::from_secs(1);

/// How long a replaced copy whose files were read stays once nothing of it is open: the next
/// file of a folder being copied comes well within it.
pub const LINGER: Duration = Duration::from_secs(30);

/// What the folder asks of the bridge.
#[derive(Debug)]
pub enum Asked {
    /// An application reads file `index` of the peer's copy `serial`, whose files hold
    /// `total` bytes: fetch it into `content`.
    Fetch {
        serial: u32,
        index: u32,
        total: u64,
        /// What names the copy in its bar.
        headline: Option<Headline>,
        content: Arc<Content>,
    },
    /// The copy `serial` went: a newer one replaced it, and its transfer is over.
    Gone { serial: u32 },
}

/// What a name in the folder is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attr {
    pub ino: u64,
    pub dir: bool,
    pub size: u64,
    /// When the copy it is part of came.
    pub made: SystemTime,
}

/// The bytes of one file of a copy, as they come.
#[derive(Debug)]
pub struct Content {
    /// Its size in the offer.
    pub size: u64,
    file: File,
    have: Mutex<Have>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct Have {
    got: u64,
    /// Set once the fetch is over: whole, or the errno its readers get.
    end: Option<Result<(), i32>>,
}

impl Content {
    /// A file of `size` bytes, its cache in `dir`.
    fn new(dir: &Path, size: u64) -> io::Result<Self> {
        let file = cache_file(dir)?;
        Ok(Self {
            size,
            file,
            have: Mutex::default(),
            changed: Condvar::new(),
        })
    }

    /// Writes `bytes` at `offset`, the next ones fetched; the readers waiting for them go on.
    /// Fails once the fetch is over: its reader closed it, or the user stopped it. Blocks.
    pub fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()> {
        if self.over() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "nobody reads it",
            ));
        }
        self.file.write_all_at(bytes, offset)?;
        self.have.lock().unwrap().got = offset + bytes.len() as u64;
        self.changed.notify_all();
        Ok(())
    }

    /// The fetch is over, whole or not: a failure is the errno its readers get. The first end
    /// stays.
    pub fn end(&self, how: Result<(), i32>) {
        let mut have = self.have.lock().unwrap();
        if have.end.is_none() {
            have.end = Some(if how.is_ok() && have.got != self.size {
                Err(libc::EIO)
            } else {
                how
            });
        }
        drop(have);
        self.changed.notify_all();
    }

    /// Whether its fetch is over, whole or not.
    pub fn over(&self) -> bool {
        self.have.lock().unwrap().end.is_some()
    }

    /// Whether its fetch ended with all its bytes.
    pub fn whole(&self) -> bool {
        self.have.lock().unwrap().end == Some(Ok(()))
    }

    /// The bytes still to come.
    pub fn left(&self) -> u64 {
        self.size.saturating_sub(self.have.lock().unwrap().got)
    }

    /// Whether all its bytes are here, or its fetch is over.
    fn complete(&self) -> bool {
        let have = self.have.lock().unwrap();
        have.got >= self.size || have.end.is_some()
    }

    /// Whether the bytes up to `end` are here; `None` while they are still to come.
    fn ready(&self, end: u64) -> Option<Result<(), i32>> {
        let have = self.have.lock().unwrap();
        if have.got >= end {
            return Some(Ok(()));
        }
        have.end.map(|how| how.and(Err(libc::EIO)))
    }

    /// Waits for the bytes up to `end`.
    fn wait(&self, end: u64) -> Result<(), i32> {
        let mut have = self.have.lock().unwrap();
        loop {
            if have.got >= end {
                return Ok(());
            }
            if let Some(how) = have.end {
                return how.and(Err(libc::EIO));
            }
            have = self.changed.wait(have).unwrap();
        }
    }

    fn read_at(&self, offset: u64, len: u64) -> Result<Vec<u8>, i32> {
        let mut bytes = vec![0; len as usize];
        self.file
            .read_exact_at(&mut bytes, offset)
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
        Ok(bytes)
    }
}

/// A cache file in `dir`, unlinked at once: nobody else opens it, and it goes with its last
/// handle, whatever ends.
fn cache_file(dir: &Path) -> io::Result<File> {
    let pid = std::process::id();
    for n in 0u32.. {
        let path = dir.join(format!(".navette-read-{pid}-{n}"));
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path);
        match opened {
            Ok(file) => {
                std::fs::remove_file(&path)?;
                return Ok(file);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// The folder's tree of copies, and the bytes read of them.
pub struct Folder {
    /// Where it is mounted.
    path: PathBuf,
    /// Where the bytes fetched are kept.
    cache: PathBuf,
    tree: Mutex<Tree>,
    asked: UnboundedSender<Asked>,
}

struct Tree {
    nodes: HashMap<u64, Node>,
    offers: BTreeMap<u32, Offer>,
    next: u64,
    /// The mode ended: every read fails.
    closed: bool,
    made: SystemTime,
}

struct Node {
    /// The copy it is part of; none for the folder itself.
    serial: Option<u32>,
    kind: Kind,
}

enum Kind {
    Dir(BTreeMap<OsString, u64>),
    File { index: u32, size: u64 },
}

struct Offer {
    files: Vec<ClipboardFile>,
    /// Its folder's inode, and every inode under it.
    inodes: Vec<u64>,
    /// Still the peer's copy: no newer one replaced it.
    current: bool,
    /// Its files and folders open now.
    open: usize,
    /// How many times each of its files is open now, by index.
    opened: HashMap<u32, usize>,
    /// When one of them was last opened or closed.
    touched: Instant,
    /// One of its files was read: its transfer started.
    read: bool,
    /// The user stopped its reads.
    stopped: bool,
    contents: HashMap<u32, Arc<Content>>,
    made: SystemTime,
}

impl Folder {
    /// A folder mounted at `path`, keeping the bytes fetched in `cache`, asking the bridge
    /// through `asked`.
    pub fn new(path: PathBuf, cache: PathBuf, asked: UnboundedSender<Asked>) -> Arc<Self> {
        let root = Node {
            serial: None,
            kind: Kind::Dir(BTreeMap::new()),
        };
        Arc::new(Self {
            path,
            cache,
            tree: Mutex::new(Tree {
                nodes: HashMap::from([(ROOT, root)]),
                offers: BTreeMap::new(),
                next: ROOT + 1,
                closed: false,
                made: SystemTime::now(),
            }),
            asked,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The peer's copy `serial` shows, with `files` (validated); the paths of its top-level
    /// entries. The copies it replaces go once none of their files is open; the same copy
    /// offered again keeps its files and what was read of them.
    pub fn offer(&self, serial: u32, files: &[ClipboardFile]) -> Vec<PathBuf> {
        let mut tree = self.tree.lock().unwrap();
        let same = tree
            .offers
            .get(&serial)
            .is_some_and(|o| !o.stopped && o.files == files);
        if !same {
            tree.remove(serial);
            tree.add(serial, files);
        }
        let replaced: Vec<u32> = tree
            .offers
            .keys()
            .copied()
            .filter(|s| *s != serial)
            .collect();
        let now = Instant::now();
        for older in replaced {
            tree.offers.get_mut(&older).unwrap().current = false;
            self.prune(&mut tree, older, now);
        }
        tree.offers.get_mut(&serial).unwrap().current = true;
        let dir = self.path.join(serial.to_string());
        gliff_proto::clipboard::top_level(files)
            .map(|f| dir.join(&f.path))
            .collect()
    }

    /// Removes the copy `serial` when it was replaced and its transfer is over at `now`.
    fn prune(&self, tree: &mut Tree, serial: u32, now: Instant) {
        if tree.offers.get(&serial).is_some_and(|o| o.over(now)) {
            tree.remove(serial);
            let _ = self.asked.send(Asked::Gone { serial });
        }
    }

    /// Removes the replaced copies whose transfer is over at `now`: called now and then.
    pub fn tidy(&self, now: Instant) {
        let mut tree = self.tree.lock().unwrap();
        let serials: Vec<u32> = tree.offers.keys().copied().collect();
        for serial in serials {
            self.prune(&mut tree, serial, now);
        }
    }

    /// The name `name` in the folder `parent`.
    pub fn lookup(&self, parent: u64, name: &OsStr) -> Result<Attr, i32> {
        let tree = self.tree.lock().unwrap();
        let Kind::Dir(children) = &tree.node(parent)?.kind else {
            return Err(libc::ENOTDIR);
        };
        let ino = *children.get(name).ok_or(libc::ENOENT)?;
        tree.attr(ino)
    }

    pub fn attr(&self, ino: u64) -> Result<Attr, i32> {
        self.tree.lock().unwrap().attr(ino)
    }

    /// What the folder `ino` holds: each entry's inode, whether it is a folder, its name.
    pub fn list(&self, ino: u64) -> Result<Vec<(u64, bool, OsString)>, i32> {
        let tree = self.tree.lock().unwrap();
        let Kind::Dir(children) = &tree.node(ino)?.kind else {
            return Err(libc::ENOTDIR);
        };
        Ok(children
            .iter()
            .map(|(name, ino)| {
                let dir = matches!(tree.nodes[ino].kind, Kind::Dir(_));
                (*ino, dir, name.clone())
            })
            .collect())
    }

    /// An application opens the file `ino`: its copy stays until it is closed.
    pub fn open(&self, ino: u64) -> Result<(), i32> {
        let mut tree = self.tree.lock().unwrap();
        if tree.closed {
            return Err(libc::EIO);
        }
        let node = tree.node(ino)?;
        let (&Kind::File { index, .. }, Some(serial)) = (&node.kind, node.serial) else {
            return Err(libc::EISDIR);
        };
        let offer = tree.offers.get_mut(&serial).ok_or(libc::ENOENT)?;
        offer.open += 1;
        *offer.opened.entry(index).or_default() += 1;
        offer.touched = Instant::now();
        Ok(())
    }

    /// An application opens the folder `ino`: its copy stays until it is closed.
    pub fn open_dir(&self, ino: u64) -> Result<(), i32> {
        let mut tree = self.tree.lock().unwrap();
        let node = tree.node(ino)?;
        let (Kind::Dir(_), serial) = (&node.kind, node.serial) else {
            return Err(libc::ENOTDIR);
        };
        if let Some(offer) = serial.and_then(|s| tree.offers.get_mut(&s)) {
            offer.open += 1;
            offer.touched = Instant::now();
        }
        Ok(())
    }

    /// An application closed the file or folder `ino`. A file closed by its last reader
    /// before all its bytes came stops its fetch and drops what came: a file manager that
    /// looks at the first bytes of each file costs a few chunks, not the files. A later read
    /// fetches it again.
    pub fn release(&self, ino: u64) {
        let mut tree = self.tree.lock().unwrap();
        let Some(node) = tree.nodes.get(&ino) else {
            return;
        };
        let Some(serial) = node.serial else { return };
        let index = match node.kind {
            Kind::File { index, .. } => Some(index),
            Kind::Dir(_) => None,
        };
        let now = Instant::now();
        if let Some(offer) = tree.offers.get_mut(&serial) {
            offer.open = offer.open.saturating_sub(1);
            offer.touched = now;
            if let Some(index) = index
                && let Some(opened) = offer.opened.get_mut(&index)
            {
                *opened -= 1;
                if *opened == 0 {
                    offer.opened.remove(&index);
                    if let Some(content) = offer.contents.get(&index)
                        && !content.complete()
                    {
                        content.end(Err(libc::ECANCELED));
                        offer.contents.remove(&index);
                    }
                }
            }
        }
        self.prune(&mut tree, serial, now);
    }

    /// Reads `size` bytes at `offset` of the file `ino`, and hands them to `reply`: at once
    /// when they are here, else from a thread of its own once they are. The first read of a
    /// file asks the bridge to fetch it.
    pub fn read(
        &self,
        ino: u64,
        offset: u64,
        size: u32,
        reply: impl FnOnce(Result<Vec<u8>, i32>) + Send + 'static,
    ) {
        let content = match self.content(ino) {
            Ok(content) => content,
            Err(e) => return reply(Err(e)),
        };
        let end = offset.saturating_add(size.into()).min(content.size);
        if offset >= end {
            return reply(Ok(Vec::new()));
        }
        if let Some(ready) = content.ready(end) {
            return reply(ready.and_then(|()| content.read_at(offset, end - offset)));
        }
        let waiting = std::thread::Builder::new()
            .name("navette-read".into())
            .spawn(move || {
                let read = content
                    .wait(end)
                    .and_then(|()| content.read_at(offset, end - offset));
                reply(read);
            });
        if let Err(e) = waiting {
            eprintln!("navette: clipboard: a read cannot wait for its bytes: {e}");
        }
    }

    /// The bytes of the file `ino`, asked of the bridge at the first read.
    fn content(&self, ino: u64) -> Result<Arc<Content>, i32> {
        let mut tree = self.tree.lock().unwrap();
        if tree.closed {
            return Err(libc::EIO);
        }
        let node = tree.node(ino)?;
        let (&Kind::File { index, size }, Some(serial)) = (&node.kind, node.serial) else {
            return Err(libc::EISDIR);
        };
        let offer = tree.offers.get_mut(&serial).ok_or(libc::ENOENT)?;
        if offer.stopped {
            return Err(libc::ECANCELED);
        }
        if let Some(content) = offer.contents.get(&index) {
            return Ok(content.clone());
        }
        let content = Content::new(&self.cache, size).map_err(|e| {
            eprintln!("navette: clipboard: no room for the bytes read: {e}");
            libc::EIO
        })?;
        let content = Arc::new(content);
        offer.contents.insert(index, content.clone());
        offer.read = true;
        let total = crate::clipboard::files::total(&offer.files).unwrap_or(u64::MAX);
        let asked = Asked::Fetch {
            serial,
            index,
            total,
            headline: crate::clipboard::files::headline(&offer.files),
            content: content.clone(),
        };
        if self.asked.send(asked).is_err() {
            content.end(Err(libc::EIO));
        }
        Ok(content)
    }

    /// The fetch of `content`, file `index` of the copy `serial`, ended without all its bytes:
    /// what came of it goes, and the next read fetches it again.
    pub fn forget(&self, serial: u32, index: u32, content: &Arc<Content>) {
        let mut tree = self.tree.lock().unwrap();
        if let Some(offer) = tree.offers.get_mut(&serial)
            && offer
                .contents
                .get(&index)
                .is_some_and(|c| Arc::ptr_eq(c, content))
        {
            offer.contents.remove(&index);
        }
    }

    /// The user stopped the reads of the copy `serial`: those waiting fail, and the next ones.
    pub fn stop(&self, serial: u32) {
        let mut tree = self.tree.lock().unwrap();
        if let Some(offer) = tree.offers.get_mut(&serial) {
            offer.stopped = true;
            for content in std::mem::take(&mut offer.contents).into_values() {
                content.end(Err(libc::ECANCELED));
            }
        }
    }

    /// The mode ended: every read fails, those waiting and those to come.
    pub fn close(&self) {
        let mut tree = self.tree.lock().unwrap();
        tree.closed = true;
        for offer in std::mem::take(&mut tree.offers).into_values() {
            for content in offer.contents.into_values() {
                content.end(Err(libc::EIO));
            }
        }
    }
}

impl Offer {
    /// Whether it may go at `now`: replaced, nothing of it open, and its transfer over:
    /// none, cancelled, or every file read whole and nothing opened for [`LINGER`].
    fn over(&self, now: Instant) -> bool {
        !self.current
            && self.open == 0
            && (!self.read
                || self.stopped
                || (self.contents.values().all(|c| c.complete())
                    && now.saturating_duration_since(self.touched) >= LINGER))
    }
}

impl Tree {
    fn node(&self, ino: u64) -> Result<&Node, i32> {
        self.nodes.get(&ino).ok_or(libc::ENOENT)
    }

    fn attr(&self, ino: u64) -> Result<Attr, i32> {
        let node = self.node(ino)?;
        let made = node
            .serial
            .and_then(|s| self.offers.get(&s))
            .map_or(self.made, |o| o.made);
        let (dir, size) = match node.kind {
            Kind::Dir(_) => (true, 0),
            Kind::File { size, .. } => (false, size),
        };
        Ok(Attr {
            ino,
            dir,
            size,
            made,
        })
    }

    fn inode(&mut self, serial: u32, kind: Kind) -> u64 {
        let ino = self.next;
        self.next += 1;
        let serial = Some(serial);
        self.nodes.insert(ino, Node { serial, kind });
        ino
    }

    /// The folder `name` in `parent`, made when it is not there.
    fn dir_in(&mut self, serial: u32, parent: u64, name: &OsStr) -> u64 {
        if let Some(Kind::Dir(children)) = self.nodes.get(&parent).map(|n| &n.kind)
            && let Some(ino) = children.get(name)
        {
            return *ino;
        }
        let ino = self.inode(serial, Kind::Dir(BTreeMap::new()));
        self.link(parent, name, ino);
        ino
    }

    fn link(&mut self, parent: u64, name: &OsStr, ino: u64) {
        if let Some(Node {
            kind: Kind::Dir(children),
            ..
        }) = self.nodes.get_mut(&parent)
        {
            children.insert(name.to_owned(), ino);
        }
    }

    fn add(&mut self, serial: u32, files: &[ClipboardFile]) {
        let first = self.next;
        let top = self.dir_in(serial, ROOT, OsStr::new(&serial.to_string()));
        for (index, f) in files.iter().enumerate() {
            let Some(path) = safe_relative_path(&f.path) else {
                continue;
            };
            let names: Vec<_> = path
                .components()
                .filter_map(|c| match c {
                    Component::Normal(name) => Some(name.to_owned()),
                    _ => None,
                })
                .collect();
            let Some((last, parents)) = names.split_last() else {
                continue;
            };
            let parent = parents
                .iter()
                .fold(top, |parent, name| self.dir_in(serial, parent, name));
            if f.dir {
                self.dir_in(serial, parent, last);
            } else {
                let kind = Kind::File {
                    index: index as u32,
                    size: f.size,
                };
                let ino = self.inode(serial, kind);
                self.link(parent, last, ino);
            }
        }
        let offer = Offer {
            files: files.to_vec(),
            inodes: (first..self.next).collect(),
            current: true,
            open: 0,
            opened: HashMap::new(),
            touched: Instant::now(),
            read: false,
            stopped: false,
            contents: HashMap::new(),
            made: SystemTime::now(),
        };
        self.offers.insert(serial, offer);
    }

    fn remove(&mut self, serial: u32) {
        let Some(offer) = self.offers.remove(&serial) else {
            return;
        };
        for ino in offer.inodes {
            self.nodes.remove(&ino);
        }
        if let Some(Node {
            kind: Kind::Dir(children),
            ..
        }) = self.nodes.get_mut(&ROOT)
        {
            children.remove(OsStr::new(&serial.to_string()));
        }
        for content in offer.contents.into_values() {
            content.end(Err(libc::ECANCELED));
        }
    }
}

/// How the folder is mounted: by FUSE, or by a fake in the tests.
pub trait Mounter: Send + Sync + std::fmt::Debug {
    /// Where it mounts.
    fn path(&self) -> &Path;
    /// Mounts `folder`, until the handle is dropped. Blocks, and so does dropping the handle.
    fn mount(&self, folder: Arc<Folder>) -> io::Result<Box<dyn Send>>;
    /// Unmounts what a process that died left mounted there. Blocks.
    fn clean(&self);
}

/// The kernel's FUSE, through `fusermount3`: no root, no system change.
#[derive(Debug)]
pub struct Fuse {
    path: PathBuf,
}

/// One mount or unmount at a time: an unmount still running never takes the next mount.
static MOUNTING: Mutex<()> = Mutex::new(());

impl Fuse {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Mounter for Fuse {
    fn path(&self) -> &Path {
        &self.path
    }

    fn mount(&self, folder: Arc<Folder>) -> io::Result<Box<dyn Send>> {
        let _one = MOUNTING.lock().unwrap();
        unmount_stale(&self.path);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.path)?;
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            fuser::MountOption::FSName("navette".into()),
            fuser::MountOption::RO,
            fuser::MountOption::NoSuid,
            fuser::MountOption::NoDev,
            fuser::MountOption::DefaultPermissions,
        ];
        let session = fuser::spawn_mount(Kernel(folder), &self.path, &config)?;
        Ok(Box::new(Mounted(Some(session))))
    }

    fn clean(&self) {
        let _one = MOUNTING.lock().unwrap();
        unmount_stale(&self.path);
    }
}

/// The unmounts running, beside the link's thread.
static UNMOUNTING: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

/// Drops the mount `handle` on a thread of its own: an unmount runs `fusermount3`.
pub fn unmount_later(handle: Option<Box<dyn Send>>) {
    let Some(handle) = handle else { return };
    let mut unmounting = UNMOUNTING.lock().unwrap();
    unmounting.retain(|thread| !thread.is_finished());
    match std::thread::Builder::new()
        .name("navette-unmount".into())
        .spawn(move || drop(handle))
    {
        Ok(thread) => unmounting.push(thread),
        Err(e) => eprintln!("navette: clipboard: the virtual folder stays mounted: {e}"),
    }
}

/// Waits for the unmounts running, before the process ends: a mount left behind would hold
/// its folder until the next start. Blocks.
pub fn unmounted() {
    let threads = std::mem::take(&mut *UNMOUNTING.lock().unwrap());
    for thread in threads {
        let _ = thread.join();
    }
}

/// A mount, undone when dropped.
struct Mounted(Option<fuser::BackgroundSession>);

impl Drop for Mounted {
    fn drop(&mut self) {
        let _one = MOUNTING.lock().unwrap();
        // Lazily: an application that holds a file open keeps it, failing, until it closes it.
        drop(self.0.take());
    }
}

/// Unmounts `path` when something is mounted there: a mount left by a process that died.
fn unmount_stale(path: &Path) {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return;
    };
    if !mounts.lines().any(|line| mount_point(line) == Some(path)) {
        return;
    }
    let done = std::process::Command::new("fusermount3")
        .args(["-u", "-q", "-z", "--"])
        .arg(path)
        .status();
    match done {
        Ok(status) if status.success() => {
            eprintln!("navette: clipboard: a stale virtual folder was unmounted");
        }
        Ok(status) => eprintln!("navette: clipboard: a stale virtual folder stays: {status}"),
        Err(e) => eprintln!("navette: clipboard: a stale virtual folder stays: {e}"),
    }
}

/// The mount point of a line of `/proc/self/mountinfo`: its fifth field, with `\ooo` escapes.
fn mount_point(line: &str) -> Option<&Path> {
    let field = line.split(' ').nth(4)?;
    // Paths with an escape (a space, a tab…) never match ours.
    (!field.contains('\\')).then(|| Path::new(field))
}

/// The folder as the kernel asks for it.
struct Kernel(Arc<Folder>);

fn errno(e: i32) -> fuser::Errno {
    fuser::Errno::from_i32(e)
}

fn file_attr(attr: Attr) -> fuser::FileAttr {
    // SAFETY: no arguments, no failure.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let (kind, perm, nlink) = if attr.dir {
        (fuser::FileType::Directory, 0o755, 2)
    } else {
        (fuser::FileType::RegularFile, 0o644, 1)
    };
    fuser::FileAttr {
        ino: fuser::INodeNo(attr.ino),
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: attr.made,
        mtime: attr.made,
        ctime: attr.made,
        crtime: attr.made,
        kind,
        perm,
        nlink,
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

impl fuser::Filesystem for Kernel {
    fn lookup(
        &self,
        _req: &fuser::Request,
        parent: fuser::INodeNo,
        name: &OsStr,
        reply: fuser::ReplyEntry,
    ) {
        match self.0.lookup(parent.0, name) {
            Ok(attr) => reply.entry(&TTL, &file_attr(attr), fuser::Generation(0)),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn getattr(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: fuser::ReplyAttr,
    ) {
        match self.0.attr(ino.0) {
            Ok(attr) => reply.attr(&TTL, &file_attr(attr)),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn readdir(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        mut reply: fuser::ReplyDirectory,
    ) {
        let entries = match self.0.list(ino.0) {
            Ok(entries) => entries,
            Err(e) => return reply.error(errno(e)),
        };
        let dots = [
            (ino.0, true, OsString::from(".")),
            (ROOT, true, "..".into()),
        ];
        let all = dots.into_iter().chain(entries).enumerate();
        for (n, (ino, dir, name)) in all.skip(offset as usize) {
            let kind = if dir {
                fuser::FileType::Directory
            } else {
                fuser::FileType::RegularFile
            };
            if reply.add(fuser::INodeNo(ino), n as u64 + 1, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _flags: fuser::OpenFlags,
        reply: fuser::ReplyOpen,
    ) {
        match self.0.open(ino.0) {
            // A file's bytes never change: what the kernel keeps of them stays good.
            Ok(()) => reply.opened(fuser::FileHandle(0), fuser::FopenFlags::FOPEN_KEEP_CACHE),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn opendir(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _flags: fuser::OpenFlags,
        reply: fuser::ReplyOpen,
    ) {
        match self.0.open_dir(ino.0) {
            Ok(()) => reply.opened(fuser::FileHandle(0), fuser::FopenFlags::empty()),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn releasedir(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        reply: fuser::ReplyEmpty,
    ) {
        self.0.release(ino.0);
        reply.ok();
    }

    fn read(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: fuser::ReplyData,
    ) {
        self.0.read(ino.0, offset, size, move |read| match read {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno(e)),
        });
    }

    fn release(
        &self,
        _req: &fuser::Request,
        ino: fuser::INodeNo,
        _fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        self.0.release(ino.0);
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::{self, UnboundedReceiver};

    use super::*;

    fn file(path: &str, size: u64) -> ClipboardFile {
        ClipboardFile {
            path: path.into(),
            size,
            dir: false,
        }
    }

    fn dir(path: &str) -> ClipboardFile {
        ClipboardFile {
            path: path.into(),
            size: 0,
            dir: true,
        }
    }

    fn folder(cache: &Path) -> (Arc<Folder>, UnboundedReceiver<Asked>) {
        let (asked, rx) = mpsc::unbounded_channel();
        (Folder::new("/mnt/files".into(), cache.into(), asked), rx)
    }

    /// The inode at `path` under the folder.
    fn at(folder: &Folder, path: &str) -> Result<Attr, i32> {
        Path::new(path)
            .iter()
            .try_fold(folder.attr(ROOT)?, |attr, name| {
                folder.lookup(attr.ino, name)
            })
    }

    /// Reads `size` bytes at `offset` of `ino`, waiting up to a second.
    fn read(folder: &Folder, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let (tx, rx) = std::sync::mpsc::channel();
        folder.read(ino, offset, size, move |read| tx.send(read).unwrap());
        rx.recv_timeout(Duration::from_secs(1)).expect("an answer")
    }

    #[test]
    fn an_offer_shows_its_tree_at_once_and_asks_for_no_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let (folder, mut asked) = folder(tmp.path());
        let files = [dir("photos"), file("photos/a.jpg", 3), file("notes.txt", 5)];
        let tops = folder.offer(7, &files);
        assert_eq!(
            tops,
            [
                PathBuf::from("/mnt/files/7/photos"),
                PathBuf::from("/mnt/files/7/notes.txt")
            ]
        );
        let photos = at(&folder, "7/photos").unwrap();
        assert!(photos.dir);
        let names: Vec<_> = folder
            .list(photos.ino)
            .unwrap()
            .into_iter()
            .map(|e| e.2)
            .collect();
        assert_eq!(names, ["a.jpg"]);
        assert_eq!(at(&folder, "7/notes.txt").unwrap().size, 5);
        let notes = at(&folder, "7/notes.txt").unwrap().ino;
        folder.open(notes).unwrap();
        assert!(asked.try_recv().is_err(), "opening reads nothing");
    }

    #[test]
    fn a_read_fetches_its_file_once_and_waits_for_its_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let (folder, mut asked) = folder(tmp.path());
        folder.offer(7, &[file("a.txt", 4), file("b.txt", 6)]);
        let b = at(&folder, "7/b.txt").unwrap().ino;
        let (tx, rx) = std::sync::mpsc::channel();
        folder.read(b, 2, 3, move |read| tx.send(read).unwrap());
        let Ok(Asked::Fetch {
            serial: 7,
            index: 1,
            total: 10,
            content,
            ..
        }) = asked.try_recv()
        else {
            panic!("file 1 of copy 7 asked");
        };
        content.write_at(b"he", 0).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err(), "waits");
        content.write_at(b"llo!", 2).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap(),
            b"llo"
        );
        content.end(Ok(()));
        assert_eq!(read(&folder, b, 0, 4096).unwrap(), b"hello!");
        assert!(asked.try_recv().is_err(), "fetched once");
    }

    #[test]
    fn a_fetch_that_fails_fails_its_readers() {
        let tmp = tempfile::tempdir().unwrap();
        let (folder, mut asked) = folder(tmp.path());
        folder.offer(7, &[file("a.txt", 4)]);
        let a = at(&folder, "7/a.txt").unwrap().ino;
        let (tx, rx) = std::sync::mpsc::channel();
        folder.read(a, 0, 4, move |read| tx.send(read).unwrap());
        let Ok(Asked::Fetch { content, .. }) = asked.try_recv() else {
            panic!("a fetch");
        };
        content.write_at(b"ab", 0).unwrap();
        // The peer's file was shorter than its offer said.
        content.end(Ok(()));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(libc::EIO)
        );
    }

    #[test]
    fn a_replaced_copy_stays_while_a_file_of_it_is_open() {
        let tmp = tempfile::tempdir().unwrap();
        let (folder, mut asked) = folder(tmp.path());
        folder.offer(7, &[file("a.txt", 4)]);
        let a = at(&folder, "7/a.txt").unwrap().ino;
        folder.open(a).unwrap();
        folder.offer(8, &[file("b.txt", 4)]);
        assert!(at(&folder, "7/a.txt").is_ok(), "still read");
        assert!(at(&folder, "8/b.txt").is_ok());
        assert!(asked.try_recv().is_err());
        folder.release(a);
        assert_eq!(at(&folder, "7").err(), Some(libc::ENOENT));
        assert!(matches!(asked.try_recv(), Ok(Asked::Gone { serial: 7 })));
        // A copy replaced with nothing open goes at once.
        folder.offer(9, &[]);
        assert_eq!(at(&folder, "8").err(), Some(libc::ENOENT));
    }

    #[test]
    fn a_replaced_copy_stays_while_a_folder_of_it_is_open() {
        let tmp = tempfile::tempdir().unwrap();
        let (folder, _asked) = folder(tmp.path());
        folder.offer(7, &[dir("d"), file("d/a.txt", 4)]);
        let d = at(&folder, "7/d").unwrap().ino;
        folder.open_dir(d).unwrap();
        folder.offer(8, &[]);
        assert!(at(&folder, "7/d/a.txt").is_ok());
        folder.release(d);
        assert_eq!(at(&folder, "7").err(), Some(libc::ENOENT));
    }

    type Replaced = (Arc<Folder>, UnboundedReceiver<Asked>, tempfile::TempDir);

    /// Copy 7 of files of `sizes`, its first file read whole and closed, then replaced.
    fn replaced_after_a_first_file(sizes: &[u64]) -> Replaced {
        let tmp = tempfile::tempdir().unwrap();
        let (folder, mut asked) = folder(tmp.path());
        let files: Vec<_> = (0..sizes.len())
            .map(|n| file(&format!("{n}.bin"), sizes[n]))
            .collect();
        folder.offer(7, &files);
        let first = at(&folder, "7/0.bin").unwrap().ino;
        folder.open(first).unwrap();
        folder.read(first, 0, 4096, |_| {});
        let Ok(Asked::Fetch { content, .. }) = asked.try_recv() else {
            panic!("a fetch");
        };
        content.write_at(&vec![1; sizes[0] as usize], 0).unwrap();
        content.end(Ok(()));
        folder.release(first);
        folder.offer(8, &[]);
        (folder, asked, tmp)
    }

    #[test]
    fn a_replaced_copy_being_read_stays_between_two_files() {
        let (folder, mut asked, _tmp) = replaced_after_a_first_file(&[4, 4]);
        let next = at(&folder, "7/1.bin").unwrap().ino;
        folder.open(next).unwrap();
        folder.read(next, 0, 4, |_| {});
        assert!(matches!(
            asked.try_recv(),
            Ok(Asked::Fetch {
                serial: 7,
                index: 1,
                ..
            })
        ));
    }

    #[test]
    fn a_replaced_copy_read_whole_goes_once_nothing_of_it_was_opened_for_a_while() {
        let (folder, mut asked, _tmp) = replaced_after_a_first_file(&[4, 4]);
        let now = Instant::now();
        folder.tidy(now + LINGER / 2);
        assert!(at(&folder, "7/1.bin").is_ok());
        assert!(asked.try_recv().is_err());
        folder.tidy(now + LINGER);
        assert_eq!(at(&folder, "7").err(), Some(libc::ENOENT));
        assert!(matches!(asked.try_recv(), Ok(Asked::Gone { serial: 7 })));
    }

    #[test]
    fn a_replaced_copy_whose_reads_were_cancelled_goes_once_closed() {
        let (folder, _asked, _tmp) = replaced_after_a_first_file(&[4, 4]);
        folder.stop(7);
        folder.tidy(Instant::now());
        assert_eq!(at(&folder, "7").err(), Some(libc::ENOENT));
    }

    #[test]
    fn the_same_copy_offered_again_keeps_what_was_read() {
        let tmp = tempfile::tempdir().unwrap();
        let (folder, mut asked) = folder(tmp.path());
        let files = [file("a.txt", 2)];
        folder.offer(7, &files);
        let a = at(&folder, "7/a.txt").unwrap().ino;
        folder.read(a, 0, 2, |_| {});
        let Ok(Asked::Fetch { content, .. }) = asked.try_recv() else {
            panic!("a fetch");
        };
        content.write_at(b"ok", 0).unwrap();
        content.end(Ok(()));
        folder.offer(7, &files);
        assert_eq!(read(&folder, a, 0, 2).unwrap(), b"ok");
        assert!(asked.try_recv().is_err());
    }

    #[test]
    fn a_stopped_copy_fails_its_reads_waiting_and_to_come() {
        let tmp = tempfile::tempdir().unwrap();
        let (folder, _asked) = folder(tmp.path());
        folder.offer(7, &[file("a.txt", 4)]);
        let a = at(&folder, "7/a.txt").unwrap().ino;
        let (tx, rx) = std::sync::mpsc::channel();
        folder.read(a, 0, 4, move |read| tx.send(read).unwrap());
        folder.stop(7);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(libc::ECANCELED)
        );
        assert_eq!(read(&folder, a, 0, 4), Err(libc::ECANCELED));
    }

    #[test]
    fn a_closed_folder_fails_every_read() {
        let tmp = tempfile::tempdir().unwrap();
        let (folder, _asked) = folder(tmp.path());
        folder.offer(7, &[file("a.txt", 4)]);
        let a = at(&folder, "7/a.txt").unwrap().ino;
        let (tx, rx) = std::sync::mpsc::channel();
        folder.read(a, 0, 4, move |read| tx.send(read).unwrap());
        folder.close();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(libc::EIO)
        );
        assert_eq!(read(&folder, a, 0, 4), Err(libc::EIO));
    }

    #[test]
    fn the_cache_leaves_no_file_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let (folder, _asked) = folder(tmp.path());
        folder.offer(7, &[file("a.txt", 4)]);
        let a = at(&folder, "7/a.txt").unwrap().ino;
        folder.read(a, 0, 4, |_| {});
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    /// A real mount, through the kernel and `fusermount3`, in a temporary folder: run by hand
    /// (`cargo test -- --ignored`) on a machine with FUSE, since a test runner may have none.
    #[test]
    #[ignore = "needs FUSE and fusermount3"]
    fn a_real_mount_reads_a_file_through_the_kernel() {
        let tmp = tempfile::tempdir().unwrap();
        let (cache, mount) = (tmp.path().join("cache"), tmp.path().join("files"));
        std::fs::create_dir(&cache).unwrap();
        let (asked, mut wanted) = mpsc::unbounded_channel();
        let folder = Folder::new(mount.clone(), cache, asked);
        let fuse = Fuse::at(mount.clone());
        let mounted = fuse.mount(folder.clone()).expect("a FUSE mount");
        let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let tops = folder.offer(3, &[dir("d"), file("d/big.bin", content.len() as u64)]);
        assert_eq!(tops, [mount.join("3/d")]);
        let listed: Vec<_> = std::fs::read_dir(mount.join("3/d"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(listed, ["big.bin"]);
        assert!(wanted.try_recv().is_err(), "listing reads nothing");
        let served = content.clone();
        let peer = std::thread::spawn(move || {
            let Some(Asked::Fetch { content, .. }) = wanted.blocking_recv() else {
                panic!("a fetch");
            };
            for (n, chunk) in served.chunks(65536).enumerate() {
                content.write_at(chunk, n as u64 * 65536).unwrap();
            }
            content.end(Ok(()));
        });
        assert!(std::fs::read(mount.join("3/d/big.bin")).unwrap() == content);
        peer.join().unwrap();
        drop(mounted);
        let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        assert!(!mounts.lines().any(|l| mount_point(l) == Some(&*mount)));
    }
}
