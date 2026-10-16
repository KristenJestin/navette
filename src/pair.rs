//! `navette pair`: the one-time exchange that makes two machines trust each other.
//!
//! The desk listens on its LAN address and shows a six-digit PIN; the remote dials it and the user
//! types the PIN there. One connection is accepted per pairing session, whatever its outcome,
//! and the session ends after a timeout. Over that plain TCP connection:
//!
//! 1. SPAKE2 (Ed25519 group), the desk as A, the remote as B, the PIN as the password. Each side
//!    sends its SPAKE2 message (`msg_a`, then `msg_b`) and derives the shared key `K`. A peer
//!    without the PIN gets one guess per session, and can test nothing offline.
//! 2. HKDF-SHA256 from `K` gives three HMAC-SHA256 keys: `k_remote`, `k_home`, `k_done`.
//! 3. The remote sends its certificate fingerprint `fp_r` with
//!    `HMAC(k_remote, msg_a ‖ msg_b ‖ fp_r)`. The desk checks it: a wrong PIN stops here.
//! 4. The desk sends `fp_h` with `HMAC(k_home, msg_a ‖ msg_b ‖ fp_r ‖ fp_h)`. The remote checks it,
//!    records the desk, and answers `HMAC(k_done, msg_a ‖ msg_b ‖ fp_r ‖ fp_h)`.
//! 5. The desk checks that answer and only then records the remote.
//!
//! A fingerprint is the SHA-256 of a certificate; the link then pins it (see `transport`). Each
//! side records its peer only after checking a MAC that only the holder of the same PIN could
//! make, so a wrong PIN, a man in the middle or a cut connection leaves nothing trusted on the
//! side that noticed. The one partial case: if the connection drops after step 4, the remote
//! trusts the desk but the desk does not trust the remote, so the link is refused and pairing is run
//! again. Nothing secret crosses: the fingerprints are not secrets, and `K` never leaves memory.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use tokio::net::{TcpListener, TcpStream};

use crate::frame::{read_frame, write_frame};
use crate::identity::Fingerprint;
use crate::network;
use crate::store::{Pairing, Role, Store};

const ID_HOME: &[u8] = b"navette-pair-v1 home";
const ID_REMOTE: &[u8] = b"navette-pair-v1 remote";

/// How long each step of a pairing session may take.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Desk: how long it waits for the remote to dial.
    pub wait: Duration,
    /// Both: how long the exchange may take once connected.
    pub exchange: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            wait: Duration::from_secs(120),
            exchange: Duration::from_secs(30),
        }
    }
}

/// The six-digit PIN of one pairing session.
#[derive(Clone, PartialEq, Eq)]
pub struct Pin(String);

impl Pin {
    pub fn random() -> Self {
        Self(format!("{:06}", rand::rngs::OsRng.gen_range(0..1_000_000)))
    }

    /// Accepts six digits, ignoring the spaces around them.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        (text.len() == 6 && text.bytes().all(|b| b.is_ascii_digit())).then(|| Self(text.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pin(..)")
    }
}

/// The desk's side of one pairing session, on a listener already bound to its LAN address. Records
/// the remote in `store` only if the exchange succeeds.
pub async fn home(
    store: &Store,
    listener: TcpListener,
    pin: &Pin,
    timeouts: Timeouts,
) -> io::Result<()> {
    let identity = store.identity()?;
    let address = listener.local_addr()?;
    let (mut stream, _) = tokio::time::timeout(timeouts.wait, listener.accept())
        .await
        .map_err(|_| timed_out("no remote machine dialed in time"))??;
    // One attempt per session: nobody else may try while this one runs, or after it.
    drop(listener);
    let remote = tokio::time::timeout(
        timeouts.exchange,
        exchange_home(&mut stream, pin, identity.fingerprint()),
    )
    .await
    .map_err(|_| timed_out("the remote machine stopped answering"))??;
    store.save_pairing(&Pairing {
        role: Role::Home,
        peer: remote,
        address,
        gateway: None,
    })
}

/// The remote's side: dials the desk at `address`. Records the desk in `store` only if the exchange
/// succeeds, with `gateway`, the hardware address of the desk's router (see `network`).
pub async fn remote(
    store: &Store,
    address: SocketAddr,
    gateway: Option<network::Mac>,
    pin: &Pin,
    timeouts: Timeouts,
) -> io::Result<()> {
    let identity = store.identity()?;
    let home = tokio::time::timeout(timeouts.exchange, async {
        let mut stream = TcpStream::connect(address).await?;
        exchange_remote(&mut stream, pin, identity.fingerprint()).await
    })
    .await
    .map_err(|_| timed_out("the desk stopped answering"))??;
    store.save_pairing(&Pairing {
        role: Role::Remote,
        peer: home,
        address,
        gateway,
    })
}

struct Keys {
    remote: Vec<u8>,
    home: Vec<u8>,
    done: Vec<u8>,
}

impl Keys {
    fn derive(shared: &[u8]) -> Self {
        let hkdf = Hkdf::<Sha256>::new(None, shared);
        let expand = |label: &[u8]| {
            let mut key = vec![0u8; 32];
            hkdf.expand(label, &mut key)
                .expect("32 bytes is a valid HKDF length");
            key
        };
        Self {
            remote: expand(b"navette-pair-v1 remote confirm"),
            home: expand(b"navette-pair-v1 home confirm"),
            done: expand(b"navette-pair-v1 done"),
        }
    }
}

fn mac(key: &[u8], parts: &[&[u8]]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    for part in parts {
        mac.update(&(part.len() as u32).to_be_bytes());
        mac.update(part);
    }
    mac
}

/// Splits a `fingerprint ‖ tag` frame.
fn fingerprint_and_tag(frame: &[u8]) -> io::Result<(Fingerprint, &[u8])> {
    let (fingerprint, tag) = frame
        .split_first_chunk::<32>()
        .ok_or_else(|| invalid("a malformed pairing message"))?;
    Ok((Fingerprint::from_bytes(*fingerprint), tag))
}

async fn exchange_home(
    stream: &mut TcpStream,
    pin: &Pin,
    own: Fingerprint,
) -> io::Result<Fingerprint> {
    let (spake, msg_a) = Spake2::<Ed25519Group>::start_a(
        &Password::new(pin.as_str()),
        &SpakeIdentity::new(ID_HOME),
        &SpakeIdentity::new(ID_REMOTE),
    );
    write_frame(stream, &msg_a).await?;
    let msg_b = read_frame(stream).await?;
    let shared = spake
        .finish(&msg_b)
        .map_err(|_| invalid("a malformed pairing message"))?;
    let keys = Keys::derive(&shared);

    let frame = read_frame(stream).await?;
    let (remote, tag) = fingerprint_and_tag(&frame)?;
    mac(&keys.remote, &[&msg_a, &msg_b, remote.as_bytes()])
        .verify_slice(tag)
        .map_err(|_| wrong_pin())?;

    let parts: [&[u8]; 4] = [&msg_a, &msg_b, remote.as_bytes(), own.as_bytes()];
    let tag = mac(&keys.home, &parts).finalize().into_bytes();
    write_frame(stream, &[own.as_bytes().as_slice(), &tag].concat()).await?;

    let done = read_frame(stream).await?;
    mac(&keys.done, &parts)
        .verify_slice(&done)
        .map_err(|_| wrong_pin())?;
    Ok(remote)
}

async fn exchange_remote(
    stream: &mut TcpStream,
    pin: &Pin,
    own: Fingerprint,
) -> io::Result<Fingerprint> {
    let (spake, msg_b) = Spake2::<Ed25519Group>::start_b(
        &Password::new(pin.as_str()),
        &SpakeIdentity::new(ID_HOME),
        &SpakeIdentity::new(ID_REMOTE),
    );
    let msg_a = read_frame(stream).await?;
    write_frame(stream, &msg_b).await?;
    let shared = spake
        .finish(&msg_a)
        .map_err(|_| invalid("a malformed pairing message"))?;
    let keys = Keys::derive(&shared);

    let tag = mac(&keys.remote, &[&msg_a, &msg_b, own.as_bytes()])
        .finalize()
        .into_bytes();
    write_frame(stream, &[own.as_bytes().as_slice(), &tag].concat()).await?;

    // The desk closes without answering when the PIN is wrong.
    let frame = read_frame(stream).await.map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset => wrong_pin(),
        _ => e,
    })?;
    let (home, tag) = fingerprint_and_tag(&frame)?;
    let parts: [&[u8]; 4] = [&msg_a, &msg_b, own.as_bytes(), home.as_bytes()];
    mac(&keys.home, &parts)
        .verify_slice(tag)
        .map_err(|_| wrong_pin())?;

    write_frame(stream, &mac(&keys.done, &parts).finalize().into_bytes()).await?;
    Ok(home)
}

fn wrong_pin() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "the PIN did not match: nothing was paired",
    )
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn timed_out(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::bind_lan;

    struct Desk {
        _tmp: tempfile::TempDir,
        home: Store,
        remote: Store,
    }

    fn desk() -> Desk {
        let tmp = tempfile::tempdir().unwrap();
        Desk {
            home: Store::new(tmp.path().join("home")),
            remote: Store::new(tmp.path().join("remote")),
            _tmp: tmp,
        }
    }

    async fn loopback() -> TcpListener {
        bind_lan("127.0.0.1:0".parse().unwrap()).await.unwrap()
    }

    fn fast() -> Timeouts {
        Timeouts {
            wait: Duration::from_secs(5),
            exchange: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn two_machines_pair_with_a_pin() {
        let d = desk();
        let listener = loopback().await;
        let address = listener.local_addr().unwrap();
        let pin = Pin::random();
        let (home, remote) = tokio::join!(
            home(&d.home, listener, &pin, fast()),
            remote(&d.remote, address, None, &pin, fast()),
        );
        home.unwrap();
        remote.unwrap();

        let home_side = d.home.pairing().unwrap().unwrap();
        let remote_side = d.remote.pairing().unwrap().unwrap();
        assert_eq!(home_side.role, Role::Home);
        assert_eq!(home_side.peer, d.remote.identity().unwrap().fingerprint());
        assert_eq!(home_side.address, address);
        assert_eq!(remote_side.role, Role::Remote);
        assert_eq!(remote_side.peer, d.home.identity().unwrap().fingerprint());
        assert_eq!(remote_side.address, address);
    }

    #[tokio::test]
    async fn the_remote_records_home_s_router_at_pairing() {
        let d = desk();
        let listener = loopback().await;
        let address = listener.local_addr().unwrap();
        let pin = Pin::random();
        let router = network::Mac::parse("02:00:00:00:00:01");
        let (home, remote) = tokio::join!(
            home(&d.home, listener, &pin, fast()),
            remote(&d.remote, address, router, &pin, fast()),
        );
        home.unwrap();
        remote.unwrap();
        assert_eq!(d.remote.pairing().unwrap().unwrap().gateway, router);
        // The desk's own pairing has no use for it.
        assert_eq!(d.home.pairing().unwrap().unwrap().gateway, None);
    }

    #[tokio::test]
    async fn a_wrong_pin_fails_and_leaves_nothing_trusted() {
        let d = desk();
        let listener = loopback().await;
        let address = listener.local_addr().unwrap();
        let shown = Pin::parse("123456").unwrap();
        let typed = Pin::parse("123457").unwrap();
        let (home, remote) = tokio::join!(
            home(&d.home, listener, &shown, fast()),
            remote(&d.remote, address, None, &typed, fast()),
        );
        assert_eq!(home.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(remote.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(d.home.pairing().unwrap(), None);
        assert_eq!(d.remote.pairing().unwrap(), None);
    }

    #[tokio::test]
    async fn a_pairing_session_takes_one_attempt_only() {
        let d = desk();
        let listener = loopback().await;
        let address = listener.local_addr().unwrap();
        let pin = Pin::random();
        let guess = Pin::parse(if pin.as_str() == "000000" {
            "000001"
        } else {
            "000000"
        })
        .unwrap();
        let (home, first) = tokio::join!(
            home(&d.home, listener, &pin, fast()),
            remote(&d.remote, address, None, &guess, fast()),
        );
        assert!(home.is_err() && first.is_err());
        // The session is over: the right PIN now finds nobody listening.
        let second = remote(&d.remote, address, None, &pin, fast())
            .await
            .unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(d.remote.pairing().unwrap(), None);
    }

    #[tokio::test]
    async fn a_pairing_session_ends_when_nobody_comes() {
        let d = desk();
        let timeouts = Timeouts {
            wait: Duration::from_millis(100),
            ..fast()
        };
        let error = home(&d.home, loopback().await, &Pin::random(), timeouts)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(d.home.pairing().unwrap(), None);
    }

    #[tokio::test]
    async fn a_silent_peer_cannot_hold_a_pairing_session_open() {
        let d = desk();
        let listener = loopback().await;
        let _silent = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let timeouts = Timeouts {
            exchange: Duration::from_millis(100),
            ..fast()
        };
        let error = home(&d.home, listener, &Pin::random(), timeouts)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(d.home.pairing().unwrap(), None);
    }

    #[test]
    fn a_pin_is_six_digits() {
        for _ in 0..100 {
            let pin = Pin::random();
            assert_eq!(Pin::parse(pin.as_str()), Some(pin));
        }
        assert_eq!(Pin::parse(" 042042\n").unwrap().as_str(), "042042");
        for bad in ["12345", "1234567", "12a456", "", "１２３４５６"] {
            assert_eq!(Pin::parse(bad), None, "{bad}");
        }
        assert_eq!(format!("{:?}", Pin::random()), "Pin(..)");
    }
}
