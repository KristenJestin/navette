//! One `navette agent` per remote machine. Home keeps the newest connection of its paired
//! remote and drops the older one, so two agents would drop each other's link, dial again, and
//! do it forever. The agent takes this lock before it dials and holds it for its whole life:
//! the kernel releases it when the process ends, a crash included, so no stale file is ever
//! left to clean up.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// Held by the running agent; dropping it (or the process ending) lets the next one start.
#[derive(Debug)]
pub struct AgentLock {
    _file: File,
}

/// `$XDG_RUNTIME_DIR/navette/agent.lock`.
pub fn agent_lock_path() -> io::Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime).join("navette").join("agent.lock"))
}

impl AgentLock {
    /// Takes the lock on `path`, in a directory only this user can enter. Fails with
    /// [`io::ErrorKind::AddrInUse`] while another agent holds it.
    pub fn take(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

        let dir = path.parent().unwrap_or(Path::new("."));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another navette agent is already running \
                 (systemctl --user status navette)",
            )),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_second_lock_is_refused_while_the_first_is_held_and_says_why() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("navette").join("agent.lock");
        let first = AgentLock::take(&path).unwrap();
        let refused = AgentLock::take(&path).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::AddrInUse);
        assert!(refused.to_string().contains("systemctl --user status"));
        drop(first);
        AgentLock::take(&path).unwrap();
    }

    #[test]
    fn the_lock_sits_in_a_private_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("navette").join("agent.lock");
        let _lock = AgentLock::take(&path).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
    }
}
