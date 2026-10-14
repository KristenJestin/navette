//! Files on the clipboard. On the copying side, what a copy offers: the files the user copied,
//! directories walked, links and special files left out and named, read again only at paste
//! time. On the pasting side, which names of the peer's are accepted.
//!
//! The walk adapts gliff's `crates/gliff-transport/src/clipboard/files.rs` (MIT, Copyright (c)
//! 2026 Kevin McConnell; the notice is in full in `src/clipboard.rs`), without its 4 GiB cap:
//! a copy of any size is offered.

use std::io;
use std::os::unix::fs::OpenOptionsExt;
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
/// and distinct; no more entries and name bytes than an offer carries. Whatever their total
/// size: the free space is checked at paste time.
pub fn validate(files: &[ClipboardFile]) -> Result<(), String> {
    if files.len() > MAX_FILE_ENTRIES {
        return Err(format!("more than {MAX_FILE_ENTRIES} files"));
    }
    let names: usize = files.iter().map(|f| f.path.len()).sum();
    if names > MAX_FILE_PATH_BYTES {
        return Err(format!("names longer than {MAX_FILE_PATH_BYTES} bytes"));
    }
    let mut seen = std::collections::HashSet::new();
    for f in files {
        let path =
            safe_relative_path(&f.path).ok_or_else(|| format!("unsafe name {:?}", f.path))?;
        if !seen.insert(path) || (f.dir && f.size != 0) {
            return Err(format!("bad entry {:?}", f.path));
        }
    }
    Ok(())
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

fn invalid(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
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
    fn a_name_with_dot_dot_or_an_absolute_path_is_refused() {
        for bad in ["../evil", "/etc/passwd", "a/../../b", "", "./x"] {
            let entry = ClipboardFile {
                path: bad.into(),
                size: 1,
                dir: false,
            };
            assert!(validate(&[entry]).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn a_link_put_in_place_of_a_copied_file_is_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let copied = tmp.path().join("notes.txt");
        std::fs::write(tmp.path().join("secret"), b"no").unwrap();
        symlink(tmp.path().join("secret"), &copied).unwrap();
        assert!(open_copied(&copied).is_err());
    }
}
