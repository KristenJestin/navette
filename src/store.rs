//! What Navette keeps on disk, in its config directory (`$XDG_CONFIG_HOME/navette` by default):
//! this machine's identity and the one machine it is paired with. Every file is written private
//! (0600, in a 0700 directory) and replaced atomically, so a failed write never leaves half a
//! pairing behind.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::identity::{Fingerprint, Identity};
use crate::network::Mac;

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
    /// Remote: the hardware address of home's router, its default gateway at home; none for a
    /// pairing made before it was recorded, or without one known yet.
    pub gateway: Option<Mac>,
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
        let (mut peer, mut address) = (None, None);
        let mut role = Err("has no `role` key".to_string());
        // Absent from a pairing made before it was recorded; present, it must read.
        let mut gateway = Ok(None);
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "role" => {
                    role = match value {
                        // `home` is what 0.1 wrote, before the desk was called the desk.
                        "desk" | "home" => Ok(Role::Home),
                        "remote" => Ok(Role::Remote),
                        _ => Err(format!("has `role = {value}`, which is not desk or remote")),
                    }
                }
                "peer" => peer = Fingerprint::from_hex(value),
                "address" => address = value.parse().ok(),
                "gateway" => gateway = Mac::parse(value).map(Some).ok_or(()),
                _ => {}
            }
        }
        // A role that does not read is named: it is the key a hand edit gets wrong.
        let role =
            role.map_err(|why| io::Error::other(format!("{} {why}: pair again", path.display())))?;
        match (peer, address, gateway) {
            (Some(peer), Some(address), Ok(gateway)) => Ok(Some(Pairing {
                role,
                peer,
                address,
                gateway,
            })),
            _ => Err(damaged()),
        }
    }

    /// Records a pairing, replacing any previous one.
    pub fn save_pairing(&self, pairing: &Pairing) -> io::Result<()> {
        let role = match pairing.role {
            Role::Home => "desk",
            Role::Remote => "remote",
        };
        let mut text = format!(
            "# Written by `navette pair`. The peer is the SHA-256 of its certificate.\n\
             role = {role}\npeer = {}\naddress = {}\n",
            pairing.peer.to_hex(),
            pairing.address
        );
        if let Some(gateway) = pairing.gateway {
            text.push_str(&format!("gateway = {gateway}\n"));
        }
        self.write_private(PAIRING_FILE, text.as_bytes())
    }

    fn write_private(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        write_private(&self.dir, name, bytes)
    }
}

/// Writes `dir/name` whole or not at all, readable by this user only (0600 in a 0700 directory).
pub(crate) fn write_private(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // `mode` only applies to a directory this call creates; one made earlier (by hand, or
    // by an older version) keeps whatever it had.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let tmp = dir.join(format!(".{name}.tmp"));
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, dir.join(name))
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
            gateway: Mac::parse("02:00:00:00:00:01"),
        };
        store.save_pairing(&pairing).unwrap();
        assert_eq!(store.pairing().unwrap(), Some(pairing));
        assert_eq!(mode(&tmp.path().join(PAIRING_FILE)), 0o600);
    }

    fn pairing_text(role: &str) -> (Fingerprint, String) {
        let peer = Identity::generate().unwrap().fingerprint();
        let text = format!(
            "role = {role}\npeer = {}\naddress = 192.0.2.10:7420\n",
            peer.to_hex()
        );
        (peer, text)
    }

    #[test]
    fn a_new_pairing_at_the_desk_writes_role_desk() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        store
            .save_pairing(&Pairing {
                role: Role::Home,
                peer: Identity::generate().unwrap().fingerprint(),
                address: "192.0.2.10:7420".parse().unwrap(),
                gateway: None,
            })
            .unwrap();
        let text = fs::read_to_string(tmp.path().join(PAIRING_FILE)).unwrap();
        assert!(text.contains("role = desk\n"), "{text}");
        assert!(!text.contains("home"), "{text}");
        assert_eq!(store.pairing().unwrap().unwrap().role, Role::Home);
    }

    #[test]
    fn a_pairing_made_by_0_1_with_role_home_still_loads_as_the_desk() {
        let tmp = tempfile::tempdir().unwrap();
        let (peer, text) = pairing_text("home");
        fs::write(tmp.path().join(PAIRING_FILE), text).unwrap();
        let pairing = Store::new(tmp.path()).pairing().unwrap().unwrap();
        assert_eq!(pairing.role, Role::Home);
        assert_eq!(pairing.peer, peer);
        // Read as it is: nothing is rewritten, so no pairing is redone.
        let text = fs::read_to_string(tmp.path().join(PAIRING_FILE)).unwrap();
        assert!(text.starts_with("role = home\n"), "{text}");
    }

    #[test]
    fn a_pairing_with_role_desk_loads_as_the_desk() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, text) = pairing_text("desk");
        fs::write(tmp.path().join(PAIRING_FILE), text).unwrap();
        let pairing = Store::new(tmp.path()).pairing().unwrap().unwrap();
        assert_eq!(pairing.role, Role::Home);
    }

    #[test]
    fn a_role_that_is_neither_desk_nor_remote_says_which_two_are_read() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, text) = pairing_text("boss");
        fs::write(tmp.path().join(PAIRING_FILE), text).unwrap();
        let error = Store::new(tmp.path()).pairing().unwrap_err().to_string();
        assert!(error.contains("not desk or remote"), "{error}");
    }

    #[test]
    fn a_pairing_made_before_the_gateway_was_recorded_reads_without_one() {
        let tmp = tempfile::tempdir().unwrap();
        let peer = Identity::generate().unwrap().fingerprint();
        fs::write(
            tmp.path().join(PAIRING_FILE),
            format!(
                "role = remote\npeer = {}\naddress = 192.0.2.10:7420\n",
                peer.to_hex()
            ),
        )
        .unwrap();
        let pairing = Store::new(tmp.path()).pairing().unwrap().unwrap();
        assert_eq!(pairing.gateway, None);
        assert_eq!(pairing.peer, peer);
    }

    #[test]
    fn a_damaged_gateway_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let peer = Identity::generate().unwrap().fingerprint();
        fs::write(
            tmp.path().join(PAIRING_FILE),
            format!(
                "role = remote\npeer = {}\naddress = 192.0.2.10:7420\ngateway = 02:00\n",
                peer.to_hex()
            ),
        )
        .unwrap();
        assert!(Store::new(tmp.path()).pairing().is_err());
    }

    #[test]
    fn a_damaged_pairing_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(PAIRING_FILE), "role = home\n").unwrap();
        assert!(Store::new(tmp.path()).pairing().is_err());
    }

    #[test]
    fn an_existing_directory_is_made_private() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("navette");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let store = Store::new(&dir);
        store.identity().unwrap();
        assert_eq!(mode(&dir), 0o700);
    }
}
