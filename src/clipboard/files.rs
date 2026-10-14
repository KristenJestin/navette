//! Files on the clipboard. On the copying side, what a copy offers: the files the user copied,
//! directories walked, links and special files left out and named, read again only at paste
//! time. On the pasting side, where the peer's files go: a `Navette` folder in the download
//! directory, filled through a hidden folder of its own so a cancelled or failed paste leaves
//! no half-written file, and never outside it or over a file already there, whatever names the
//! peer sends.
//!
//! The walk adapts gliff's `crates/gliff-transport/src/clipboard/files.rs` (MIT, Copyright (c)
//! 2026 Kevin McConnell; the notice is in full in `src/clipboard.rs`), without its 4 GiB cap:
//! a copy of any size is offered.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use gliff_proto::ClipboardFile;
use gliff_proto::clipboard::{MAX_FILE_ENTRIES, MAX_FILE_PATH_BYTES, safe_relative_path};
use gliff_transport::clipboard::files::LocalFiles;

/// What a copy of files offers the peer, and what it leaves out.
#[derive(Debug, Default)]
pub struct Copied {
    pub files: LocalFiles,
    /// Links and special files (sockets, pipes, devices), by their path in the offer.
    pub skipped: Vec<String>,
}

/// Walks the copied paths. Directories are listed recursively; a link is never followed, so a
/// loop cannot run the walk forever and a copy never reaches past what was copied. Fails when
/// there are more entries, or longer names, than an offer carries: a huge selection is not
/// offered at all rather than offered in part.
pub fn list(roots: &[PathBuf]) -> io::Result<Copied> {
    let mut out = Copied::default();
    for root in roots {
        let name = root
            .file_name()
            .ok_or_else(|| invalid(format!("{} has no file name", root.display())))?;
        walk(root, Path::new(name), &mut out)?;
    }
    validate(&out.files.entries).map_err(invalid)?;
    Ok(out)
}

fn walk(path: &Path, rel: &Path, out: &mut Copied) -> io::Result<()> {
    let rel_str = rel
        .to_str()
        .ok_or_else(|| invalid(format!("{} is not UTF-8", rel.display())))?
        .to_string();
    let meta = std::fs::symlink_metadata(path)?;
    let kind = meta.file_type();
    if !kind.is_dir() && !kind.is_file() {
        out.skipped.push(rel_str);
        return Ok(());
    }
    if out.files.entries.len() >= MAX_FILE_ENTRIES {
        return Err(invalid(format!(
            "more than {MAX_FILE_ENTRIES} files copied"
        )));
    }
    out.files.entries.push(ClipboardFile {
        path: rel_str,
        size: if kind.is_dir() { 0 } else { meta.len() },
        dir: kind.is_dir(),
    });
    out.files.paths.push(path.to_path_buf());
    if kind.is_dir() {
        let mut children: Vec<_> = std::fs::read_dir(path)?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<io::Result<_>>()?;
        children.sort();
        for child in children {
            walk(&path.join(&child), &rel.join(&child), out)?;
        }
    }
    Ok(())
}

/// Whether the entries of a peer's offer are acceptable: every path relative, without `..`,
/// and distinct, none at the top named like a staging folder (the sweep of stale ones would
/// remove it from the inbox); no more entries and name bytes than an offer carries. Whatever
/// their total size, as long as it can be counted: the free space is checked at paste time.
pub fn validate(files: &[ClipboardFile]) -> Result<(), String> {
    if files.len() > MAX_FILE_ENTRIES {
        return Err(format!("more than {MAX_FILE_ENTRIES} files"));
    }
    let names: usize = files.iter().map(|f| f.path.len()).sum();
    if names > MAX_FILE_PATH_BYTES {
        return Err(format!("names longer than {MAX_FILE_PATH_BYTES} bytes"));
    }
    if total(files).is_none() {
        return Err("sizes past what a count holds".into());
    }
    let mut seen = std::collections::HashSet::new();
    for f in files {
        let path =
            safe_relative_path(&f.path).ok_or_else(|| format!("unsafe name {:?}", f.path))?;
        let top = path.components().next().map(|c| c.as_os_str().as_bytes());
        let staging = top.is_some_and(|top| top.starts_with(STAGING.as_bytes()));
        if staging || !seen.insert(path) || (f.dir && f.size != 0) {
            return Err(format!("bad entry {:?}", f.path));
        }
    }
    Ok(())
}

/// The bytes the entries hold, none past what a count holds: a peer can name any sizes.
pub fn total(files: &[ClipboardFile]) -> Option<u64> {
    files
        .iter()
        .try_fold(0u64, |sum, f| sum.checked_add(f.size))
}

/// Opens a copied file to serve it, at paste time: only a regular file, and never through a
/// link put in its place since the copy (nor a pipe that would block the read).
pub fn open_copied(path: &Path) -> io::Result<tokio::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid(format!("{} is no longer a file", path.display())));
    }
    Ok(tokio::fs::File::from_std(file))
}

/// Where pasted files go: the `Navette` folder of the download directory.
#[derive(Clone, Debug)]
pub struct Inbox {
    pub dir: PathBuf,
    /// The bytes free for the user on the disk that holds a folder.
    pub free: fn(&Path) -> io::Result<u64>,
}

impl Inbox {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            free: free_space,
        }
    }

    /// The machine's own: `Navette` in `$XDG_DOWNLOAD_DIR` (from the environment or
    /// `user-dirs.dirs`), else in `~/Downloads`. None without a home folder.
    pub fn of_this_user() -> Option<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty())?);
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|c| !c.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let downloads = match std::env::var_os("XDG_DOWNLOAD_DIR").filter(|d| !d.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => download_dir(&home, &config),
        };
        Some(Self::new(downloads.join("Navette")))
    }

    /// Room for `bytes` more, or why not.
    pub fn check_room(&self, bytes: u64) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let free = (self.free)(&self.dir)?;
        if free < bytes {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!("{} needed, {} free", human_bytes(bytes), human_bytes(free)),
            ));
        }
        Ok(())
    }
}

/// The download directory `user-dirs.dirs` names, else `~/Downloads`.
fn download_dir(home: &Path, config: &Path) -> PathBuf {
    let named = std::fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|dirs| {
            dirs.lines().find_map(|line| {
                let value = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=")?;
                let value = value.trim_matches('"');
                let path = match value.strip_prefix("$HOME") {
                    Some(rest) => home.join(rest.trim_start_matches('/')),
                    None => PathBuf::from(value),
                };
                path.is_absolute().then_some(path)
            })
        });
    named.unwrap_or_else(|| home.join("Downloads"))
}

fn free_space(dir: &Path) -> io::Result<u64> {
    let path = CString::new(dir.as_os_str().as_bytes()).map_err(invalid)?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a statvfs the call fills.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// The hidden folder one paste fills in the inbox. Removed with whatever it holds when dropped:
/// a paste that fails or is cancelled leaves nothing behind.
pub struct Staging {
    dir: PathBuf,
    inbox: PathBuf,
}

impl Staging {
    pub fn create_in(inbox: &Inbox) -> io::Result<Self> {
        std::fs::create_dir_all(&inbox.dir)?;
        sweep_stale(&inbox.dir);
        // Pasted files can be private: the folder is the user's alone while it fills.
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        let pid = std::process::id();
        for n in 0u32.. {
            let dir = inbox.dir.join(format!("{STAGING}{pid}-{n}"));
            match builder.create(&dir) {
                Ok(()) => {
                    return Ok(Self {
                        dir,
                        inbox: inbox.dir.clone(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    /// Where the entry the peer names `path` is written: inside this folder, or nowhere.
    pub fn path_of(&self, path: &str) -> io::Result<PathBuf> {
        safe_relative_path(path)
            .map(|rel| self.dir.join(rel))
            .ok_or_else(|| invalid(format!("unsafe name {path:?}")))
    }

    /// Moves each top-level entry into the inbox under its own name, or the name with a suffix
    /// when one is already there; their paths there. All of them or none: when one cannot be
    /// moved, those already moved come back, to go with this folder.
    pub fn finish<'a>(self, tops: impl Iterator<Item = &'a str>) -> io::Result<Vec<PathBuf>> {
        let mut placed = Vec::new();
        for top in tops {
            let to = self
                .path_of(top)
                .and_then(|from| Ok((place(&from, &self.inbox, OsStr::new(top))?, from)));
            match to {
                Ok(moved) => placed.push(moved),
                Err(e) => return Err(unplace(placed, e)),
            }
        }
        Ok(placed.into_iter().map(|(to, _)| to).collect())
    }
}

/// Moves the entries placed back where they came from, after `e`; what could not be is named
/// in the error the user reads.
fn unplace(placed: Vec<(PathBuf, PathBuf)>, e: io::Error) -> io::Error {
    let stayed: Vec<String> = placed
        .into_iter()
        // Back into the staging folder, the paste's alone: nothing there to overwrite.
        .filter(|(to, from)| std::fs::rename(to, from).is_err())
        .map(|(to, _)| to.display().to_string())
        .collect();
    if stayed.is_empty() {
        return e;
    }
    io::Error::new(
        e.kind(),
        format!("{e}; left in place: {}", stayed.join(", ")),
    )
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            eprintln!("navette: clipboard: a cancelled paste was not removed: {e}");
        }
    }
}

/// The prefix of a staging folder's name, then `<pid>-<n>`.
const STAGING: &str = ".navette-incoming-";

/// Removes the staging folders of processes that no longer exist: a killed one never drops
/// its own.
fn sweep_stale(inbox: &Path) {
    let Ok(entries) = std::fs::read_dir(inbox) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(STAGING))
            .and_then(|n| n.split('-').next())
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !Path::new("/proc").join(pid.to_string()).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Moves `from` into `dir` as `name`, or `name (1)`, `name (2)`… : never over what is there.
fn place(from: &Path, dir: &Path, name: &OsStr) -> io::Result<PathBuf> {
    for n in 0u32.. {
        let to = dir.join(suffixed(name, n));
        match rename_no_replace(from, &to) {
            Ok(()) => return Ok(to),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// `report.pdf`, then `report (1).pdf`, `report (2).pdf`…
fn suffixed(name: &OsStr, n: u32) -> OsString {
    if n == 0 {
        return name.to_owned();
    }
    let path = Path::new(name);
    let stem = path.file_stem().unwrap_or(name);
    let mut out = stem.to_owned();
    out.push(format!(" ({n})"));
    if let Some(ext) = path.extension() {
        out.push(".");
        out.push(ext);
    }
    out
}

fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let (c_from, c_to) = (c_path(from)?, c_path(to)?);
    // SAFETY: two NUL-terminated paths, relative to nothing (they are absolute or the cwd's).
    let done = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            c_from.as_ptr(),
            libc::AT_FDCWD,
            c_to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    let done = if done == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    };
    or_without_noreplace(done, from, to)
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(invalid)
}

/// `done`, or on a filesystem without `RENAME_NOREPLACE` (EINVAL), the same move by other
/// means, never over what is there: a file is linked under its new name then unlinked, a
/// folder replaces an empty one made for it (which fails if anything got in meanwhile).
fn or_without_noreplace(done: io::Result<()>, from: &Path, to: &Path) -> io::Result<()> {
    match done {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {}
        done => return done,
    }
    if !std::fs::symlink_metadata(from)?.is_dir() {
        std::fs::hard_link(from, to)?;
        return std::fs::remove_file(from);
    }
    std::fs::create_dir(to)?;
    std::fs::rename(from, to).map_err(|e| {
        // Only the empty folder made here goes, never what someone put in it.
        let _ = std::fs::remove_dir(to);
        match e.kind() {
            io::ErrorKind::DirectoryNotEmpty => io::ErrorKind::AlreadyExists.into(),
            _ => e,
        }
    })
}

/// Bytes as a short figure: `1.5 GiB`.
pub fn human_bytes(n: u64) -> String {
    gliff_transport::clipboard::progress::human_bytes(n)
}

fn invalid(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn names(copied: &Copied) -> Vec<&str> {
        copied
            .files
            .entries
            .iter()
            .map(|f| f.path.as_str())
            .collect()
    }

    #[test]
    fn a_copied_directory_is_listed_whole_and_its_links_and_pipes_are_left_out() {
        let tmp = tempfile::tempdir().unwrap();
        let photos = tmp.path().join("photos");
        std::fs::create_dir_all(photos.join("2026")).unwrap();
        std::fs::write(photos.join("2026/beach.jpg"), b"sand").unwrap();
        std::fs::write(photos.join("empty.txt"), b"").unwrap();
        symlink("/etc/hostname", photos.join("elsewhere")).unwrap();
        let fifo = CString::new(photos.join("pipe").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let copied = list(std::slice::from_ref(&photos)).unwrap();
        assert_eq!(
            names(&copied),
            [
                "photos",
                "photos/2026",
                "photos/2026/beach.jpg",
                "photos/empty.txt"
            ]
        );
        assert_eq!(copied.files.entries[2].size, 4);
        assert_eq!(copied.files.paths[2], photos.join("2026/beach.jpg"));
        assert_eq!(copied.skipped, ["photos/elsewhere", "photos/pipe"]);
    }

    #[test]
    fn a_copied_link_is_left_out_and_named() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("real.txt"), b"x").unwrap();
        symlink(tmp.path().join("real.txt"), tmp.path().join("link.txt")).unwrap();
        let copied = list(&[tmp.path().join("link.txt"), tmp.path().join("real.txt")]).unwrap();
        assert_eq!(names(&copied), ["real.txt"]);
        assert_eq!(copied.skipped, ["link.txt"]);
    }

    #[test]
    fn a_copy_of_any_size_is_offered() {
        let five_gib = ClipboardFile {
            path: "disk.img".into(),
            size: 5 << 30,
            dir: false,
        };
        assert_eq!(validate(&[five_gib]), Ok(()));
    }

    #[test]
    fn an_offer_whose_sizes_add_up_past_what_a_count_holds_is_refused() {
        let half = |path: &str| ClipboardFile {
            path: path.into(),
            size: 1 << 63,
            dir: false,
        };
        let files = [half("a.bin"), half("b.bin")];
        assert_eq!(total(&files), None);
        assert!(validate(&files).is_err());
    }

    #[test]
    fn a_name_with_dot_dot_or_an_absolute_path_is_refused() {
        for bad in ["../evil", "/etc/passwd", "a/../../b", "", "./x"] {
            let entry = ClipboardFile {
                path: bad.into(),
                size: 1,
                dir: false,
            };
            assert!(validate(&[entry]).is_err(), "{bad:?} accepted");
        }
        let tmp = tempfile::tempdir().unwrap();
        let staging = Staging::create_in(&Inbox::new(tmp.path().into())).unwrap();
        assert!(staging.path_of("../evil").is_err());
        assert!(staging.path_of("/etc/passwd").is_err());
        assert!(staging.path_of("a/b").unwrap().starts_with(&staging.dir));
    }

    #[test]
    fn a_name_the_stale_staging_sweep_would_remove_is_refused() {
        let entry = |path: &str| ClipboardFile {
            path: path.into(),
            size: 0,
            dir: true,
        };
        for bad in [".navette-incoming-1-0", ".navette-incoming-x"] {
            assert!(validate(&[entry(bad)]).is_err(), "{bad:?} accepted");
        }
        // Deeper in a folder, the sweep never sees it.
        let nested = [entry("photos"), entry("photos/.navette-incoming-1-0")];
        assert_eq!(validate(&nested), Ok(()));
    }

    #[test]
    fn a_link_put_in_place_of_a_copied_file_is_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("notes.txt");
        std::fs::write(tmp.path().join("secret"), b"no").unwrap();
        symlink(tmp.path().join("secret"), &copied).unwrap();
        assert!(open_copied(&copied).is_err());
    }

    #[test]
    fn an_existing_name_gets_a_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = Inbox::new(tmp.path().join("Navette"));
        std::fs::create_dir_all(&inbox.dir).unwrap();
        std::fs::write(inbox.dir.join("report.pdf"), b"old").unwrap();
        std::fs::write(inbox.dir.join("report (1).pdf"), b"older").unwrap();
        std::fs::create_dir(inbox.dir.join("photos")).unwrap();

        let staging = Staging::create_in(&inbox).unwrap();
        std::fs::write(staging.path_of("report.pdf").unwrap(), b"new").unwrap();
        std::fs::create_dir(staging.path_of("photos").unwrap()).unwrap();
        let placed = staging
            .finish(["report.pdf", "photos"].into_iter())
            .unwrap();

        assert_eq!(
            placed,
            [
                inbox.dir.join("report (2).pdf"),
                inbox.dir.join("photos (1)")
            ]
        );
        assert_eq!(std::fs::read(inbox.dir.join("report.pdf")).unwrap(), b"old");
        assert_eq!(std::fs::read(&placed[0]).unwrap(), b"new");
        // The staging folder went with the paste.
        assert_eq!(std::fs::read_dir(&inbox.dir).unwrap().count(), 5);
    }

    #[test]
    fn a_paste_whose_later_entry_cannot_be_placed_places_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = Inbox::new(tmp.path().into());
        let staging = Staging::create_in(&inbox).unwrap();
        std::fs::write(staging.path_of("first.txt").unwrap(), b"one").unwrap();
        // The second entry is not in the staging folder: it cannot be moved.
        let failed = staging.finish(["first.txt", "second.txt"].into_iter());
        assert!(failed.is_err());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn without_rename_noreplace_a_file_and_a_folder_move_and_never_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("staged"), tmp.path().join("inbox"));
        std::fs::create_dir_all(from.join("photos")).unwrap();
        std::fs::create_dir(&to).unwrap();
        std::fs::write(from.join("report.pdf"), b"new").unwrap();
        std::fs::write(from.join("photos/beach.jpg"), b"sand").unwrap();
        std::fs::write(to.join("taken.pdf"), b"mine").unwrap();
        std::fs::create_dir(to.join("taken")).unwrap();
        // What renameat2 says on a filesystem without RENAME_NOREPLACE.
        let unsupported = || Err(io::Error::from_raw_os_error(libc::EINVAL));
        let moved =
            |a: &str, b: &str| or_without_noreplace(unsupported(), &from.join(a), &to.join(b));

        moved("report.pdf", "report.pdf").unwrap();
        moved("photos", "photos").unwrap();
        assert_eq!(std::fs::read(to.join("report.pdf")).unwrap(), b"new");
        assert_eq!(std::fs::read(to.join("photos/beach.jpg")).unwrap(), b"sand");
        assert!(!from.join("report.pdf").exists() && !from.join("photos").exists());

        std::fs::write(from.join("again.pdf"), b"again").unwrap();
        std::fs::create_dir(from.join("again")).unwrap();
        let taken = moved("again.pdf", "taken.pdf").unwrap_err();
        assert_eq!(taken.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(to.join("taken.pdf")).unwrap(), b"mine");
        let taken = moved("again", "taken").unwrap_err();
        assert_eq!(taken.kind(), io::ErrorKind::AlreadyExists);
        assert!(from.join("again.pdf").exists() && from.join("again").exists());
    }

    #[test]
    fn a_dropped_staging_folder_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = Inbox::new(tmp.path().into());
        let staging = Staging::create_in(&inbox).unwrap();
        std::fs::write(staging.path_of("half.bin").unwrap(), b"half").unwrap();
        drop(staging);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_download_folder_is_the_one_user_dirs_names() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, config) = (tmp.path().join("home"), tmp.path().join("config"));
        assert_eq!(download_dir(&home, &config), home.join("Downloads"));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("user-dirs.dirs"),
            "# written by xdg-user-dirs-update\nXDG_DOWNLOAD_DIR=\"$HOME/Téléchargements\"\n",
        )
        .unwrap();
        assert_eq!(download_dir(&home, &config), home.join("Téléchargements"));
    }

    #[test]
    fn too_little_room_is_said_before_anything_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let inbox = Inbox {
            dir: tmp.path().join("Navette"),
            free: |_| Ok(1 << 20),
        };
        let error = inbox.check_room(2 << 30).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert!(error.to_string().contains("2.0 GiB needed"), "{error}");
        inbox.check_room(1 << 10).unwrap();
    }
}
