//! What Navette keeps on disk, in its config directory (`$XDG_CONFIG_HOME/navette` by default):
//! this machine's identity and the one machine it is paired with. Every file is written private
//! (0600, in a 0700 directory) and replaced atomically, so a failed write never leaves half a
//! pairing behind.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::identity::{Fingerprint, Identity};

const CERT_FILE: &str = "identity.crt";
const KEY_FILE: &str = "identity.key";
const PAIRING_FILE: &str = "paired.conf";

/// Which side of the link this machine is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The machine with the monitors: it listens, on its LAN address only.
    Home,
    /// The managed laptop: it only dials out to home.
    Remote,
}

/// The machine this one trusts, recorded by `navette pair`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    pub role: Role,
    pub peer: Fingerprint,
    /// Home: the LAN address it listens on. Remote: the home address it dials.
    pub address: SocketAddr,
}

/// Navette's config directory.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `$XDG_CONFIG_HOME/navette`, or `~/.config/navette`.
    pub fn default_dir() -> io::Result<PathBuf> {
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".config"))
                .ok_or_else(|| io::Error::other("neither XDG_CONFIG_HOME nor HOME is set"))?,
        };
        Ok(base.join("navette"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// This machine's identity, made on first use.
    pub fn identity(&self) -> io::Result<Identity> {
        let cert_path = self.dir.join(CERT_FILE);
        let key_path = self.dir.join(KEY_FILE);
        match (read_if_exists(&cert_path)?, read_if_exists(&key_path)?) {
            (Some(cert), Some(key)) => Ok(Identity::from_der(cert, key)),
            // Making a new identity here would silently break the pairing: say so instead.
            (Some(_), None) | (None, Some(_)) => Err(io::Error::other(format!(
                "half an identity in {}: remove {CERT_FILE} and {KEY_FILE}, then pair again",
                self.dir.display()
            ))),
            (None, None) => {
                let identity = Identity::generate()?;
                let (cert, key) = identity.to_der();
                self.write_private(KEY_FILE, key)?;
                self.write_private(CERT_FILE, cert)?;
                Ok(identity)
            }
        }
    }

    /// The recorded pairing, if this machine has been paired.
    pub fn pairing(&self) -> io::Result<Option<Pairing>> {
        let path = self.dir.join(PAIRING_FILE);
        let Some(bytes) = read_if_exists(&path)? else {
            return Ok(None);
        };
        let damaged = || io::Error::other(format!("{} is damaged: pair again", path.display()));
        let text = String::from_utf8(bytes).map_err(|_| damaged())?;
        let (mut role, mut peer, mut address) = (None, None, None);
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "role" => {
                    role = match value {
                        "home" => Some(Role::Home),
                        "remote" => Some(Role::Remote),
                        _ => None,
                    }
                }
                "peer" => peer = Fingerprint::from_hex(value),
                "address" => address = value.parse().ok(),
                _ => {}
            }
        }
        match (role, peer, address) {
            (Some(role), Some(peer), Some(address)) => Ok(Some(Pairing {
                role,
                peer,
                address,
            })),
            _ => Err(damaged()),
        }
    }

    /// Records a pairing, replacing any previous one.
    pub fn save_pairing(&self, pairing: &Pairing) -> io::Result<()> {
        let role = match pairing.role {
            Role::Home => "home",
            Role::Remote => "remote",
        };
        let text = format!(
            "# Written by `navette pair`. The peer is the SHA-256 of its certificate.\n\
             role = {role}\npeer = {}\naddress = {}\n",
            pairing.peer.to_hex(),
            pairing.address
        );
        self.write_private(PAIRING_FILE, text.as_bytes())
    }

    fn write_private(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let tmp = self.dir.join(format!(".{name}.tmp"));
        let _ = fs::remove_file(&tmp);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, self.dir.join(name))
    }
}

fn read_if_exists(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn identity_files_are_private() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("navette"));
        store.identity().unwrap();
        assert_eq!(mode(store.dir()), 0o700);
        assert_eq!(mode(&store.dir().join(KEY_FILE)), 0o600);
        assert_eq!(mode(&store.dir().join(CERT_FILE)), 0o600);
    }

    #[test]
    fn identity_is_made_once_then_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let first = store.identity().unwrap().fingerprint();
        assert_eq!(store.identity().unwrap().fingerprint(), first);
    }

    #[test]
    fn half_an_identity_is_an_error_not_a_new_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        store.identity().unwrap();
        fs::remove_file(tmp.path().join(KEY_FILE)).unwrap();
        assert!(store.identity().is_err());
        assert!(!tmp.path().join(KEY_FILE).exists());
    }

    #[test]
    fn a_pairing_is_saved_private_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        assert_eq!(store.pairing().unwrap(), None);
        let pairing = Pairing {
            role: Role::Remote,
            peer: Identity::generate().unwrap().fingerprint(),
            address: "192.0.2.10:7420".parse().unwrap(),
        };
        store.save_pairing(&pairing).unwrap();
        assert_eq!(store.pairing().unwrap(), Some(pairing));
        assert_eq!(mode(&tmp.path().join(PAIRING_FILE)), 0o600);
    }

    #[test]
    fn a_damaged_pairing_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(PAIRING_FILE), "role = home\n").unwrap();
        assert!(Store::new(tmp.path()).pairing().is_err());
    }
}
