//! The link once paired: the remote's agent dials home and keeps an idle connection, home
//! serves it. Over it, for now, only a hello (the link's version) and a heartbeat each way;
//! the orders come with the next issues, over the same authenticated stream.
//!
//! The agent reconnects with an exponential backoff whenever the link drops; home keeps the
//! newest connection of its one paired remote and drops the older one, which a laptop that
//! changed network may have left half-open.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinSet;

use crate::frame::{read_frame, write_frame};
use crate::store::{Role, Store};
use crate::transport::{Accept, Dial, Stream, TlsAcceptor, TlsDialer};

/// The version of the link's messages; peers of another version refuse each other.
pub const VERSION: u8 = 1;

const HELLO: u8 = 1;
const HEARTBEAT: u8 = 2;

/// How often each side shows it is alive, and how long a silence ends the link.
#[derive(Clone, Copy, Debug)]
pub struct Heartbeat {
    pub interval: Duration,
    pub timeout: Duration,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(30),
        }
    }
}

/// The agent's wait between two attempts: doubled after each failure up to `max`, back to
/// `first` once a link has been up.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new(first: Duration, max: Duration) -> Self {
        Self {
            first,
            max,
            next: first,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }

    pub fn reset(&mut self) {
        self.next = self.first;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(30))
    }
}

/// What the link went through, for the log. Never carries a key or a certificate.
#[derive(Debug)]
pub enum Event {
    Connected,
    Disconnected(io::Error),
    /// A connection home refused, or one the agent could not open.
    Failed(io::Error),
    Retrying(Duration),
}

/// Both sides say hello first; a peer of another version is refused.
pub async fn hello<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    heartbeat: Heartbeat,
) -> io::Result<()> {
    write_frame(stream, &[HELLO, VERSION]).await?;
    let frame = tokio::time::timeout(heartbeat.timeout, read_frame(stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no hello from the peer"))??;
    match frame.as_slice() {
        [HELLO, VERSION] => Ok(()),
        [HELLO, other] => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the peer speaks link version {other}, this machine {VERSION}: update both"),
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no hello from the peer",
        )),
    }
}

/// Keeps a link alive after its hello, and returns what ended it.
pub async fn idle<S: AsyncRead + AsyncWrite>(stream: S, heartbeat: Heartbeat) -> io::Error {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let result = tokio::select! {
        result = send_heartbeats(&mut writer, heartbeat.interval) => result,
        result = receive_heartbeats(&mut reader, heartbeat.timeout) => result,
    };
    match result {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

async fn send_heartbeats<W: AsyncWrite + Unpin>(
    writer: &mut W,
    interval: Duration,
) -> io::Result<Infallible> {
    let mut tick = tokio::time::interval(interval);
    loop {
        tick.tick().await;
        write_frame(writer, &[HEARTBEAT]).await?;
    }
}

async fn receive_heartbeats<R: AsyncRead + Unpin>(
    reader: &mut R,
    timeout: Duration,
) -> io::Result<Infallible> {
    loop {
        let frame = tokio::time::timeout(timeout, read_frame(reader))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the peer went silent"))??;
        if frame != [HEARTBEAT] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "an unexpected message on the link",
            ));
        }
    }
}

/// `navette agent`: dials home, keeps the link up, and dials again whenever it drops. Never
/// returns.
pub async fn agent<D: Dial>(
    dialer: D,
    heartbeat: Heartbeat,
    mut backoff: Backoff,
    events: UnboundedSender<Event>,
) {
    loop {
        match dialer.dial().await {
            Ok(mut stream) => match hello(&mut stream, heartbeat).await {
                Ok(()) => {
                    backoff.reset();
                    let _ = events.send(Event::Connected);
                    let _ = events.send(Event::Disconnected(idle(stream, heartbeat).await));
                }
                Err(e) => {
                    let _ = events.send(Event::Failed(e));
                }
            },
            Err(e) => {
                let _ = events.send(Event::Failed(e));
            }
        }
        let delay = backoff.next_delay();
        let _ = events.send(Event::Retrying(delay));
        tokio::time::sleep(delay).await;
    }
}

/// `navette serve`: waits for the paired remote and keeps its link up. Never returns; dropping
/// it closes the link.
pub async fn serve<A: Accept>(
    mut acceptor: A,
    heartbeat: Heartbeat,
    events: UnboundedSender<Event>,
) {
    let mut sessions = JoinSet::new();
    loop {
        match acceptor.accept().await {
            Ok(stream) => {
                while sessions.try_join_next().is_some() {}
                sessions.abort_all();
                sessions.spawn(session(stream, heartbeat, events.clone()));
            }
            Err(e) => {
                let _ = events.send(Event::Failed(e));
                // Paces a listener that keeps failing (out of file descriptors, say).
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn session<S: Stream>(mut stream: S, heartbeat: Heartbeat, events: UnboundedSender<Event>) {
    let event = match hello(&mut stream, heartbeat).await {
        Ok(()) => {
            let _ = events.send(Event::Connected);
            Event::Disconnected(idle(stream, heartbeat).await)
        }
        Err(e) => Event::Failed(e),
    };
    let _ = events.send(event);
}

/// The remote's dialer, from its pairing.
pub fn remote_dialer(store: &Store) -> io::Result<TlsDialer> {
    let pairing = paired_as(store, Role::Remote)?;
    TlsDialer::new(&store.identity()?, pairing.peer, pairing.address)
}

/// Home's acceptor, on `listen` or else the address it was paired on.
pub async fn home_acceptor(store: &Store, listen: Option<SocketAddr>) -> io::Result<TlsAcceptor> {
    let pairing = paired_as(store, Role::Home)?;
    let address = listen.unwrap_or(pairing.address);
    TlsAcceptor::bind(&store.identity()?, pairing.peer, address).await
}

fn paired_as(store: &Store, role: Role) -> io::Result<crate::store::Pairing> {
    let not_paired = || {
        io::Error::new(
            io::ErrorKind::NotFound,
            "this machine is not paired: run `navette pair` on both machines first",
        )
    };
    let pairing = store.pairing()?.ok_or_else(not_paired)?;
    if pairing.role != role {
        let command = match pairing.role {
            Role::Home => "navette serve",
            Role::Remote => "navette agent",
        };
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("this machine is paired as the other side: run `{command}` here"),
        ));
    }
    Ok(pairing)
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::identity::Identity;
    use crate::pair::{self, Pin, Timeouts};
    use crate::transport::bind_lan;

    fn fast() -> Heartbeat {
        Heartbeat {
            interval: Duration::from_millis(20),
            timeout: Duration::from_millis(200),
        }
    }

    fn quick_backoff() -> Backoff {
        Backoff::new(Duration::from_millis(10), Duration::from_millis(100))
    }

    async fn next(events: &mut mpsc::UnboundedReceiver<Event>) -> Event {
        tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("an event in time")
            .expect("the sender is alive")
    }

    async fn until_connected(events: &mut mpsc::UnboundedReceiver<Event>) {
        loop {
            if let Event::Connected = next(events).await {
                return;
            }
        }
    }

    struct Desk {
        tmp: tempfile::TempDir,
        home: Store,
        remote: Store,
    }

    /// Two machines paired over loopback, the way `navette pair` does it.
    async fn paired_desk() -> Desk {
        let tmp = tempfile::tempdir().unwrap();
        let desk = Desk {
            home: Store::new(tmp.path().join("home")),
            remote: Store::new(tmp.path().join("remote")),
            tmp,
        };
        let listener = bind_lan("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = listener.local_addr().unwrap();
        let pin = Pin::random();
        let (home, remote) = tokio::join!(
            pair::home(&desk.home, listener, &pin, Timeouts::default()),
            pair::remote(&desk.remote, address, &pin, Timeouts::default()),
        );
        home.unwrap();
        remote.unwrap();
        desk
    }

    #[tokio::test]
    async fn the_link_runs_over_an_in_memory_stream() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let (ha, hb) = tokio::join!(hello(&mut a, fast()), hello(&mut b, fast()));
        ha.unwrap();
        hb.unwrap();
        let both = async {
            tokio::select! {
                e = idle(a, fast()) => e,
                e = idle(b, fast()) => e,
            }
        };
        // Ten heartbeat timeouts later, neither side has dropped the link.
        assert!(
            tokio::time::timeout(fast().timeout * 10, both)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_silent_peer_is_dropped_after_the_heartbeat_timeout() {
        let (a, _silent) = tokio::io::duplex(1024);
        let error = tokio::time::timeout(Duration::from_secs(5), idle(a, fast()))
            .await
            .expect("idle must give up on a silent peer");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn a_peer_of_another_version_is_refused() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let other = async {
            write_frame(&mut b, &[HELLO, VERSION + 1]).await.unwrap();
            b
        };
        let (result, _b) = tokio::join!(hello(&mut a, fast()), other);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn the_backoff_doubles_up_to_its_cap_and_resets() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(5));
        let delays: Vec<u64> = (0..5).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 5, 5]);
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn after_pairing_the_remote_connects_on_its_own() {
        let desk = paired_desk().await;
        let (home_tx, mut home_events) = mpsc::unbounded_channel();
        let (remote_tx, mut remote_events) = mpsc::unbounded_channel();
        let acceptor = home_acceptor(&desk.home, None).await.unwrap();
        let home = tokio::spawn(serve(acceptor, fast(), home_tx));
        let remote = tokio::spawn(agent(
            remote_dialer(&desk.remote).unwrap(),
            fast(),
            quick_backoff(),
            remote_tx,
        ));
        until_connected(&mut home_events).await;
        until_connected(&mut remote_events).await;
        home.abort();
        remote.abort();
    }

    #[tokio::test]
    async fn the_remote_reconnects_after_the_home_side_restarts() {
        let desk = paired_desk().await;
        let (remote_tx, mut remote_events) = mpsc::unbounded_channel();
        let remote = tokio::spawn(agent(
            remote_dialer(&desk.remote).unwrap(),
            fast(),
            quick_backoff(),
            remote_tx,
        ));

        let (home_tx, mut home_events) = mpsc::unbounded_channel();
        let acceptor = home_acceptor(&desk.home, None).await.unwrap();
        let home = tokio::spawn(serve(acceptor, fast(), home_tx));
        until_connected(&mut home_events).await;
        until_connected(&mut remote_events).await;

        home.abort();
        let _ = home.await;
        loop {
            if let Event::Disconnected(_) = next(&mut remote_events).await {
                break;
            }
        }

        let (home_tx, mut home_events) = mpsc::unbounded_channel();
        let acceptor = home_acceptor(&desk.home, None).await.unwrap();
        let home = tokio::spawn(serve(acceptor, fast(), home_tx));
        until_connected(&mut home_events).await;
        until_connected(&mut remote_events).await;
        home.abort();
        remote.abort();
    }

    #[tokio::test]
    async fn each_side_runs_only_the_command_of_its_role() {
        let desk = paired_desk().await;
        let home_as_remote = remote_dialer(&desk.home).err().unwrap();
        assert!(home_as_remote.to_string().contains("navette serve"));
        let remote_as_home = home_acceptor(&desk.remote, None).await.err().unwrap();
        assert!(remote_as_home.to_string().contains("navette agent"));
        let unpaired = Store::new(desk.tmp.path().join("nobody"));
        assert_eq!(
            remote_dialer(&unpaired).err().unwrap().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn an_unpaired_host_cannot_evict_the_real_link() {
        let desk = paired_desk().await;
        let (home_tx, mut home_events) = mpsc::unbounded_channel();
        let (remote_tx, mut remote_events) = mpsc::unbounded_channel();
        let acceptor = home_acceptor(&desk.home, None).await.unwrap();
        let address = acceptor.local_addr().unwrap();
        let home = tokio::spawn(serve(acceptor, fast(), home_tx));
        let remote = tokio::spawn(agent(
            remote_dialer(&desk.remote).unwrap(),
            fast(),
            quick_backoff(),
            remote_tx,
        ));
        until_connected(&mut home_events).await;
        until_connected(&mut remote_events).await;

        // A stranger with its own certificate, and a silent connection: home refuses both.
        let stranger = TlsDialer::new(
            &Identity::generate().unwrap(),
            desk.home.identity().unwrap().fingerprint(),
            address,
        )
        .unwrap();
        let _ = stranger.dial().await;
        let _silent = tokio::net::TcpStream::connect(address).await.unwrap();

        // The real link outlives several heartbeat timeouts, and nothing dropped it.
        let quiet = tokio::time::sleep(fast().timeout * 3);
        tokio::pin!(quiet);
        loop {
            tokio::select! {
                () = &mut quiet => break,
                event = home_events.recv() => assert!(
                    !matches!(event, Some(Event::Disconnected(_))),
                    "the real link was dropped: {event:?}",
                ),
                event = remote_events.recv() => assert!(
                    !matches!(event, Some(Event::Disconnected(_))),
                    "the real link was dropped: {event:?}",
                ),
            }
        }
        home.abort();
        remote.abort();
    }
}
