//! The link once paired: the remote's agent dials home and keeps a connection, home serves it.
//! Over it, a hello (the link's version), then gliff's framing carrying a heartbeat each way
//! and the connection's services (the clipboard bridge, the display), over the same
//! authenticated stream.
//!
//! The agent reconnects with an exponential backoff whenever the link drops; home keeps the
//! newest connection of its one paired remote and drops the older one, which a laptop that
//! changed network may have left half-open. Once the newer one has said hello, it ends the
//! older connection, and so its service, before it starts the newer one's: a service such as
//! the clipboard bridge is never twice alive on the same machine.

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use bytes::Bytes;
use gliff_proto::clipboard::CHUNK;
use gliff_proto::{ClipboardMsg, ServerMsg};
use gliff_transport::Framed;
use gliff_transport::clipboard::{Outbound, outbound_channel};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinSet;

use crate::audio::{self, codec::MAX_PACKET};
use crate::display::wire::{Incoming, Outbox, Peer};
use crate::frame::{read_frame, write_frame};
use crate::input;
use crate::store::{Role, Store};
use crate::transport::{Accept, Dial, TlsAcceptor, TlsDialer};
use crate::wire::{Clip, LinkMsg, checked};

/// The version of the link's messages; peers of another version refuse each other.
pub const VERSION: u8 = 2;

const HELLO: u8 = 1;

/// Clipboard messages read from the peer and not yet taken by the bridge. The reader never
/// waits on a service: past this many, a message for it is dropped, so a stuck service cannot
/// stall the heartbeats (or another service) nor grow its memory. With offers checked on
/// arrival, a message holds at most about 1 MiB (a chunk, 256 KiB): a few MiB at worst, while
/// a bridge that drains has a handful queued at most (gliff's window is 4 chunks).
const INBOUND: usize = 16;

/// The display's messages read from the peer and not yet taken. Its service routes them at once
/// (frames to the decoder, which has its own bounded queue), so this fills only while it waits
/// on Hyprland or a thread, a second or two: past this, a frame is acked and dropped (a
/// keyframe asked for), feedback is dropped (acks are cumulative), and an order, which must not
/// be lost, ends the link, so both sides give the monitor back.
const DISPLAY_INBOUND: usize = 256;

/// The largest video frame stream: a 4K keyframe is a few MiB.
const MAX_VIDEO_PAYLOAD: u32 = 32 << 20;

/// What a connection's services get: the clipboard messages of the peer and where to put its
/// own; the display's, both ways, for the display service to take.
pub struct Wire {
    pub from_peer: mpsc::Receiver<Clip>,
    pub to_peer: Outbound,
    pub display: Option<Peer>,
    /// The sound's own queues, for whoever takes them.
    pub audio: Option<audio::Wire>,
}

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

/// Keeps a link alive after its hello, running `attach` beside the heartbeats, and returns
/// what ended the link. The service ending (or failing to start) does not end the link.
pub async fn run<S, F, Fut>(stream: S, heartbeat: Heartbeat, attach: F) -> io::Error
where
    S: AsyncRead + AsyncWrite,
    F: FnOnce(Wire) -> Fut,
    Fut: Future<Output = ()>,
{
    let (reader, writer) = tokio::io::split(stream);
    let (inbound_tx, from_peer) = mpsc::channel(INBOUND);
    let (to_peer, outbound_rx) = outbound_channel();
    let outbox = Outbox::default();
    let (display_tx, display_rx) = mpsc::channel(DISPLAY_INBOUND);
    let input = input::Wire::default();
    let display = Peer {
        outbox: outbox.clone(),
        inbox: display_rx,
        input: input.clone(),
    };
    let (sound, sound_end) = audio::wire();
    let service = async {
        attach(Wire {
            from_peer,
            to_peer,
            display: Some(display),
            audio: Some(sound),
        })
        .await;
        std::future::pending::<Infallible>().await
    };
    let result = tokio::select! {
        result = send(
            Framed::new(writer),
            heartbeat.interval,
            outbound_rx,
            sound_end.from_audio,
            &outbox,
            &input.to_peer,
        ) => result,
        result = receive(
            Framed::new(reader),
            heartbeat.timeout,
            inbound_tx,
            sound_end.to_audio,
            display_tx,
            &outbox,
            &input.from_peer,
        ) => result,
        never = service => match never {},
    };
    outbox.close();
    input.close();
    match result {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

fn io_error(e: gliff_transport::Error) -> io::Error {
    match e {
        gliff_transport::Error::Io(e) => e,
        gliff_transport::Error::Closed => io::ErrorKind::UnexpectedEof.into(),
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

async fn send<W: AsyncWrite + Unpin>(
    mut writer: Framed<W>,
    interval: Duration,
    mut outbound: mpsc::Receiver<Clip>,
    mut sound: mpsc::Receiver<audio::Message>,
    display: &Outbox,
    input: &input::Queue,
) -> io::Result<Infallible> {
    let mut tick = tokio::time::interval(interval);
    // A long chunk write must not be followed by a burst of heartbeats.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut open = true;
    let mut sound_open = true;
    loop {
        tokio::select! {
            // Input first: a key must not wait behind a video frame already queued.
            biased;
            Some(event) = input.next() => {
                writer.write_msg(&LinkMsg::Input(event)).await.map_err(io_error)?
            }
            msg = sound.recv(), if sound_open => match msg {
                Some(msg) => {
                    let packet = match &msg {
                        audio::Message::Frame { packet, .. } => &packet[..],
                        _ => &[],
                    };
                    writer
                        .write_msg_with_payloads(&LinkMsg::from(&msg), &[packet])
                        .await
                        .map_err(io_error)?
                }
                None => sound_open = false,
            },
            _ = tick.tick() => writer.write_msg(&LinkMsg::Heartbeat).await.map_err(io_error)?,
            clip = outbound.recv(), if open => match clip {
                Some((msg, payload)) => writer
                    .write_msg_with_payloads(&LinkMsg::from(msg), &[&payload])
                    .await
                    .map_err(io_error)?,
                None => open = false,
            },
            (msg, payloads) = display.next() => {
                let parts: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
                let begin = Instant::now();
                writer
                    .write_msg_with_payloads(&LinkMsg::from(msg), &parts)
                    .await
                    .map_err(io_error)?;
                display.report_write(begin.elapsed());
            }
        }
    }
}

async fn receive<R: AsyncRead + Unpin>(
    mut reader: Framed<R>,
    timeout: Duration,
    inbound: mpsc::Sender<Clip>,
    sound: SyncSender<audio::Message>,
    display: mpsc::Sender<Incoming>,
    outbox: &Outbox,
    input: &input::Queue,
) -> io::Result<Infallible> {
    let silent = || io::Error::new(io::ErrorKind::TimedOut, "the peer went silent");
    loop {
        let msg = tokio::time::timeout(timeout, reader.read_msg::<LinkMsg>())
            .await
            .map_err(|_| silent())?
            .map_err(io_error)?;
        let clip = match msg {
            LinkMsg::AudioStart { .. } | LinkMsg::AudioStop | LinkMsg::AudioFrame { .. } => {
                let sound_msg = audio_received(&mut reader, timeout, msg).await?;
                // Like the clipboard's, a sound the audio thread is not ready for is dropped.
                let _ = sound.try_send(sound_msg);
                continue;
            }
            LinkMsg::Display(order) => {
                if let Err(mpsc::error::TrySendError::Full(_)) =
                    display.try_send(Incoming::Display(order))
                {
                    return Err(io::Error::other(
                        "the display fell behind with an order waiting",
                    ));
                }
                continue;
            }
            LinkMsg::Video(msg) => {
                let (main, aux) = match &msg {
                    ServerMsg::VideoFrame {
                        data_len, aux_len, ..
                    } => (*data_len, *aux_len),
                    _ => (0, 0),
                };
                if main.max(aux) > MAX_VIDEO_PAYLOAD {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "a video frame larger than any screen's",
                    ));
                }
                // The whole frame, payloads included, within one timeout: a peer gone in the
                // middle of it must not hold the link until TCP gives up.
                let (main, aux) = tokio::time::timeout(timeout, async {
                    Ok::<_, gliff_transport::Error>((
                        reader.read_payload(main).await?,
                        reader.read_payload(aux).await?,
                    ))
                })
                .await
                .map_err(|_| silent())?
                .map_err(io_error)?;
                let video = Incoming::Video { msg, main, aux };
                if let Err(mpsc::error::TrySendError::Full(Incoming::Video { msg, .. })) =
                    display.try_send(video)
                {
                    outbox.refuse_frame(&msg);
                }
                continue;
            }
            LinkMsg::Input(input::Event::Keymap(text)) if text.len() > input::MAX_KEYMAP => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a keymap larger than any keyboard's",
                ));
            }
            // Never waits: the queue's own policy keeps it bounded and no key held.
            LinkMsg::Input(event) => {
                input.push(event);
                continue;
            }
            LinkMsg::Feedback(msg) => {
                let _ = display.try_send(Incoming::Feedback(msg));
                continue;
            }
            msg => msg,
        };
        let Some(clip) = clip.into_clipboard().map(checked) else {
            continue;
        };
        let payload = match &clip {
            ClipboardMsg::Data { data_len, .. } if *data_len as usize <= CHUNK => {
                tokio::time::timeout(timeout, reader.read_payload(*data_len))
                    .await
                    .map_err(|_| silent())?
                    .map_err(io_error)?
            }
            ClipboardMsg::Data { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a clipboard chunk larger than the agreed size",
                ));
            }
            _ => Bytes::new(),
        };
        // A service that went away or fell behind leaves the link up: the message is dropped,
        // and its transfer fails on its own (gliff's idle timeout).
        let _ = inbound.try_send((clip, payload));
    }
}

/// An audio message, with the packet that follows a frame.
async fn audio_received<R: AsyncRead + Unpin>(
    reader: &mut Framed<R>,
    timeout: Duration,
    msg: LinkMsg,
) -> io::Result<audio::Message> {
    Ok(match msg {
        LinkMsg::AudioStart { session } => audio::Message::Start { session },
        LinkMsg::AudioFrame {
            session,
            seq,
            data_len,
        } if data_len as usize <= MAX_PACKET => audio::Message::Frame {
            session,
            seq,
            packet: tokio::time::timeout(timeout, reader.read_payload(data_len))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the peer went silent"))?
                .map_err(io_error)?,
        },
        LinkMsg::AudioFrame { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "an audio packet larger than Opus makes",
            ));
        }
        _ => audio::Message::Stop,
    })
}

/// `navette agent`: dials home, keeps the link up, and dials again whenever it drops. `attach`
/// starts the connection's service each time the link is up. Never returns.
pub async fn agent<D, F, Fut>(
    dialer: D,
    heartbeat: Heartbeat,
    mut backoff: Backoff,
    events: UnboundedSender<Event>,
    attach: F,
) where
    D: Dial,
    F: Fn(Wire) -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        match dialer.dial().await {
            Ok(mut stream) => match hello(&mut stream, heartbeat).await {
                Ok(()) => {
                    backoff.reset();
                    let _ = events.send(Event::Connected);
                    let ended = run(stream, heartbeat, &attach).await;
                    let _ = events.send(Event::Disconnected(ended));
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

/// `navette serve`: waits for the paired remote and keeps its link up, running `attach` on
/// each connection. Needs a `LocalSet`, since a service need not be `Send`. Never returns;
/// dropping it closes the link.
pub async fn serve<A, F, Fut>(
    mut acceptor: A,
    heartbeat: Heartbeat,
    events: UnboundedSender<Event>,
    attach: F,
) where
    A: Accept + 'static,
    F: Fn(Wire) -> Fut + Clone + 'static,
    Fut: Future<Output = ()> + 'static,
{
    // The listener runs on its own, so a connection still saying hello holds up neither the
    // next one nor the link that is up.
    let (accepted, mut connections) = mpsc::channel(1);
    let failed = events.clone();
    let mut listener = JoinSet::new();
    listener.spawn_local(async move {
        loop {
            match acceptor.accept().await {
                Ok(stream) => {
                    if accepted.send(stream).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = failed.send(Event::Failed(e));
                    // Paces a listener that keeps failing (out of file descriptors, say).
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
    let mut greetings = JoinSet::new();
    let mut session = JoinSet::new();
    loop {
        tokio::select! {
            Some(mut stream) = connections.recv() => {
                greetings.spawn_local(async move {
                    hello(&mut stream, heartbeat).await.map(|()| stream)
                });
            }
            Some(greeted) = greetings.join_next() => match greeted {
                Ok(Ok(stream)) => {
                    // Only a connection that said hello replaces the link. The older link, with
                    // its service, is gone before the newer one's starts.
                    session.abort_all();
                    while session.join_next().await.is_some() {}
                    let _ = events.send(Event::Connected);
                    let (events, attach) = (events.clone(), attach.clone());
                    session.spawn_local(async move {
                        let ended = run(stream, heartbeat, attach).await;
                        let _ = events.send(Event::Disconnected(ended));
                    });
                }
                Ok(Err(e)) => {
                    let _ = events.send(Event::Failed(e));
                }
                Err(_) => {}
            },
        }
    }
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
pub(crate) mod tests {
    use gliff_proto::{ClientMsg, ClipboardFile};
    use tokio::sync::mpsc;

    use super::*;
    use crate::display::wire::{Display, Msg as DisplayMsg};
    use crate::identity::Identity;
    use crate::pair::{self, Pin, Timeouts};
    use crate::transport::bind_lan;

    fn fast() -> Heartbeat {
        Heartbeat {
            interval: Duration::from_millis(20),
            timeout: Duration::from_millis(200),
        }
    }

    async fn nothing(_: Wire) {}

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
                e = run(a, fast(), nothing) => e,
                e = run(b, fast(), nothing) => e,
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
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), nothing))
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

    #[tokio::test]
    async fn a_clipboard_chunk_larger_than_agreed_ends_the_link() {
        let (a, b) = tokio::io::duplex(2 * CHUNK);
        let mut peer = Framed::new(b);
        let data = vec![0u8; CHUNK + 1];
        peer.write_msg_with_payloads(
            &LinkMsg::ClipboardData {
                id: 1,
                offset: 0,
                data_len: data.len() as u32,
                done: false,
            },
            &[&data],
        )
        .await
        .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), nothing))
            .await
            .expect("the link must give up on an oversized chunk");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_service_that_never_reads_does_not_stop_the_reader() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut peer = Framed::new(b);
        // Far more than the service's queue holds, then silence: a reader that still reads
        // sees the silence and ends the link on time.
        for id in 0..(4 * INBOUND as u32) {
            peer.write_msg(&LinkMsg::ClipboardAck { id, received: 0 })
                .await
                .unwrap();
        }
        let deaf = |wire: Wire| async move {
            let _wire = wire;
            std::future::pending::<()>().await
        };
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), deaf))
            .await
            .expect("the reader must keep reading past a service that does not");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn an_audio_packet_larger_than_opus_makes_ends_the_link() {
        let (a, b) = tokio::io::duplex(2 * MAX_PACKET);
        let mut peer = Framed::new(b);
        let data = vec![0u8; MAX_PACKET + 1];
        peer.write_msg_with_payloads(
            &LinkMsg::AudioFrame {
                session: 1,
                seq: 0,
                data_len: data.len() as u32,
            },
            &[&data],
        )
        .await
        .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), nothing))
            .await
            .expect("the link must give up on an oversized packet");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_sound_that_never_reads_does_not_stop_the_reader() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut peer = Framed::new(b);
        // Far more frames than the sound's queue holds, then silence.
        for seq in 0..(4 * audio::QUEUE as u32) {
            peer.write_msg_with_payloads(
                &LinkMsg::AudioFrame {
                    session: 1,
                    seq,
                    data_len: 3,
                },
                &[&[1, 2, 3]],
            )
            .await
            .unwrap();
        }
        let deaf = |wire: Wire| async move {
            let _wire = wire;
            std::future::pending::<()>().await
        };
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), deaf))
            .await
            .expect("the reader must keep reading past a sound that does not");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    /// What the service gets when the peer sends `offer`.
    async fn offered_to_the_service(offer: LinkMsg) -> Clip {
        let (a, b) = tokio::io::duplex(1024 * 1024);
        let mut peer = Framed::new(b);
        peer.write_msg(&offer).await.unwrap();
        let (got, mut seen) = mpsc::unbounded_channel();
        let service = |mut wire: Wire| async move {
            let _ = got.send(wire.from_peer.recv().await.unwrap());
        };
        tokio::select! {
            _ = run(a, fast(), service) => panic!("the link ended"),
            clip = seen.recv() => clip.unwrap(),
        }
    }

    fn file(path: String) -> ClipboardFile {
        ClipboardFile {
            path,
            size: 1,
            dir: false,
        }
    }

    #[tokio::test]
    async fn an_oversized_offer_reaches_the_service_empty() {
        let empty = ClipboardMsg::Offer {
            serial: 7,
            mime_types: Vec::new(),
            files: Vec::new(),
        };
        let too_many_files = LinkMsg::ClipboardOffer {
            serial: 7,
            mime_types: vec!["text/plain".into()],
            files: (0..20_000).map(|n| file(format!("f{n}"))).collect(),
        };
        assert!(
            offered_to_the_service(too_many_files).await.0 == empty,
            "passed on: too_many_files"
        );
        let too_many_types = LinkMsg::ClipboardOffer {
            serial: 7,
            mime_types: (0..10_000).map(|n| format!("x/{n}")).collect(),
            files: Vec::new(),
        };
        assert!(
            offered_to_the_service(too_many_types).await.0 == empty,
            "passed on: too_many_types"
        );
        let too_long_a_type = LinkMsg::ClipboardOffer {
            serial: 7,
            mime_types: vec!["x".repeat(100_000)],
            files: Vec::new(),
        };
        assert!(
            offered_to_the_service(too_long_a_type).await.0 == empty,
            "passed on: too_long_a_type"
        );
    }

    #[tokio::test]
    async fn a_traversal_path_in_an_offer_is_refused() {
        let traversal = LinkMsg::ClipboardOffer {
            serial: 7,
            mime_types: Vec::new(),
            files: vec![file("../outside".into())],
        };
        let (offer, _) = offered_to_the_service(traversal).await;
        assert!(
            matches!(&offer, ClipboardMsg::Offer { files, .. } if files.is_empty()),
            "{offer:?}"
        );
    }

    /// Home's side of a link that is accepted from a queue of in-memory connections.
    pub(crate) struct Queue(pub(crate) mpsc::UnboundedReceiver<tokio::io::DuplexStream>);

    impl Accept for Queue {
        type Stream = tokio::io::DuplexStream;

        async fn accept(&mut self) -> io::Result<Self::Stream> {
            self.0
                .recv()
                .await
                .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    #[tokio::test]
    async fn a_paired_peer_whose_hello_fails_does_not_kill_the_good_link() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (connections, queue) = mpsc::unbounded_channel();
                let (events, mut home_events) = mpsc::unbounded_channel();
                tokio::task::spawn_local(serve(Queue(queue), fast(), events, nothing));

                let (home_end, mut good) = tokio::io::duplex(1024);
                connections.send(home_end).unwrap();
                hello(&mut good, fast()).await.unwrap();
                until_connected(&mut home_events).await;
                let good = tokio::task::spawn_local(run(good, fast(), nothing));

                let (home_end, mut bad) = tokio::io::duplex(1024);
                connections.send(home_end).unwrap();
                write_frame(&mut bad, &[HELLO, VERSION + 1]).await.unwrap();
                match next(&mut home_events).await {
                    Event::Failed(e) => assert_eq!(e.kind(), io::ErrorKind::InvalidData),
                    other => panic!("the bad hello must fail alone: {other:?}"),
                }
                let quiet = tokio::time::sleep(fast().timeout * 3);
                tokio::pin!(quiet);
                tokio::select! {
                    () = &mut quiet => {}
                    event = home_events.recv() => panic!("the good link moved: {event:?}"),
                }
                assert!(!good.is_finished(), "home dropped the good link");
            })
            .await;
    }

    #[tokio::test]
    async fn a_malformed_message_ends_the_link() {
        // A message this build knows, with a field of the wrong type: not a newer peer's.
        #[derive(minicbor::Encode)]
        enum Malformed {
            #[n(4)]
            ClipboardAck {
                #[n(0)]
                id: String,
                #[n(1)]
                received: u64,
            },
        }
        let (a, b) = tokio::io::duplex(1024);
        let mut peer = Framed::new(b);
        let ack = Malformed::ClipboardAck {
            id: "one".into(),
            received: 0,
        };
        peer.write_msg(&ack).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), nothing))
            .await
            .expect("the link must give up on a malformed message");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
        tokio::task::LocalSet::new()
            .run_until(async {
                let (home_tx, mut home_events) = mpsc::unbounded_channel();
                let (remote_tx, mut remote_events) = mpsc::unbounded_channel();
                let acceptor = home_acceptor(&desk.home, None).await.unwrap();
                let home = tokio::task::spawn_local(serve(acceptor, fast(), home_tx, nothing));
                let remote = tokio::task::spawn_local(agent(
                    remote_dialer(&desk.remote).unwrap(),
                    fast(),
                    quick_backoff(),
                    remote_tx,
                    nothing,
                ));
                until_connected(&mut home_events).await;
                until_connected(&mut remote_events).await;
                home.abort();
                remote.abort();
            })
            .await;
    }

    #[tokio::test]
    async fn the_remote_reconnects_after_the_home_side_restarts() {
        let desk = paired_desk().await;
        tokio::task::LocalSet::new()
            .run_until(async {
                let (remote_tx, mut remote_events) = mpsc::unbounded_channel();
                let remote = tokio::task::spawn_local(agent(
                    remote_dialer(&desk.remote).unwrap(),
                    fast(),
                    quick_backoff(),
                    remote_tx,
                    nothing,
                ));

                let (home_tx, mut home_events) = mpsc::unbounded_channel();
                let acceptor = home_acceptor(&desk.home, None).await.unwrap();
                let home = tokio::task::spawn_local(serve(acceptor, fast(), home_tx, nothing));
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
                let home = tokio::task::spawn_local(serve(acceptor, fast(), home_tx, nothing));
                until_connected(&mut home_events).await;
                until_connected(&mut remote_events).await;
                home.abort();
                remote.abort();
            })
            .await;
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
        tokio::task::LocalSet::new()
            .run_until(async {
                let (home_tx, mut home_events) = mpsc::unbounded_channel();
                let (remote_tx, mut remote_events) = mpsc::unbounded_channel();
                let acceptor = home_acceptor(&desk.home, None).await.unwrap();
                let address = acceptor.local_addr().unwrap();
                let home = tokio::task::spawn_local(serve(acceptor, fast(), home_tx, nothing));
                let remote = tokio::task::spawn_local(agent(
                    remote_dialer(&desk.remote).unwrap(),
                    fast(),
                    quick_backoff(),
                    remote_tx,
                    nothing,
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
            })
            .await;
    }

    /// Two ends of a link, each running; their display parts, and what ended each.
    struct Ends {
        home: Peer,
        remote: Peer,
        home_ended: tokio::task::JoinHandle<io::Error>,
    }

    async fn display_link(capacity: usize) -> Ends {
        let (a, b) = tokio::io::duplex(capacity);
        let attach = |peers: mpsc::UnboundedSender<Peer>| {
            move |mut wire: Wire| {
                let _ = peers.send(wire.display.take().unwrap());
                std::future::ready(())
            }
        };
        let (home_tx, mut home) = mpsc::unbounded_channel();
        let (remote_tx, mut remote) = mpsc::unbounded_channel();
        let home_ended = tokio::spawn(run(a, fast(), attach(home_tx)));
        tokio::spawn(run(b, fast(), attach(remote_tx)));
        Ends {
            home: home.recv().await.unwrap(),
            remote: remote.recv().await.unwrap(),
            home_ended,
        }
    }

    fn video_frame(id: u64, len: usize) -> (DisplayMsg, Vec<Vec<u8>>) {
        (
            DisplayMsg::Video(ServerMsg::VideoFrame {
                frame_id: id,
                pts_us: 0,
                keyframe: id == 0,
                damage: Vec::new(),
                data_len: len as u32,
                aux_len: 3,
            }),
            vec![vec![7; len], vec![9; 3]],
        )
    }

    async fn incoming(peer: &mut Peer) -> Incoming {
        tokio::time::timeout(Duration::from_secs(5), peer.inbox.recv())
            .await
            .expect("a message in time")
            .expect("the link is up")
    }

    #[tokio::test]
    async fn the_display_s_orders_and_a_large_video_frame_cross_the_link() {
        let mut ends = display_link(64 * 1024).await;
        let stop = DisplayMsg::Display(Display::Stop);
        ends.home.outbox.send(stop, Vec::new());
        // Two megabytes: far above the pairing frames' limit, one keyframe of a busy screen.
        let (msg, payloads) = video_frame(0, 2 << 20);
        ends.home.outbox.send(msg, payloads);
        assert!(matches!(
            incoming(&mut ends.remote).await,
            Incoming::Display(Display::Stop)
        ));
        match incoming(&mut ends.remote).await {
            Incoming::Video { main, aux, .. } => {
                assert_eq!(main.len(), 2 << 20);
                assert!(main.iter().all(|&b| b == 7));
                assert_eq!(&aux[..], &[9, 9, 9]);
            }
            other => panic!("expected a video frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_peer_gone_in_the_middle_of_a_frame_ends_the_link_in_time() {
        let (a, mut b) = tokio::io::duplex(64 * 1024);
        // The frame's header says 100 kB follow; the peer sends 10 and is heard no more.
        let (msg, _) = video_frame(0, 100_000);
        let body = minicbor::to_vec(LinkMsg::from(msg)).unwrap();
        let mut bytes = (body.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&100_003u32.to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(&[7; 10]);
        tokio::io::AsyncWriteExt::write_all(&mut b, &bytes)
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), run(a, fast(), nothing))
            .await
            .expect("the link ends within its timeout");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn home_s_input_reaches_the_remote_in_order() {
        let ends = display_link(64 * 1024).await;
        let events = [
            input::Event::Key {
                code: 125,
                pressed: true,
            },
            input::Event::Motion { x: 10.0, y: 20.0 },
            input::Event::ReleaseAll,
        ];
        for event in events.clone() {
            ends.home.input.to_peer.push(event);
        }
        for event in events {
            let got =
                tokio::time::timeout(Duration::from_secs(5), ends.remote.input.from_peer.next())
                    .await
                    .expect("an event in time");
            assert_eq!(got, Some(event));
        }
    }

    #[tokio::test]
    async fn a_dropped_link_ends_the_remote_s_input() {
        let ends = display_link(64 * 1024).await;
        ends.home_ended.abort();
        let _ = ends.home_ended.await;
        let got = tokio::time::timeout(Duration::from_secs(5), ends.remote.input.from_peer.next())
            .await
            .expect("the input ends with the link");
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn an_input_queue_nobody_reads_does_not_stop_the_reader() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut peer = Framed::new(b);
        for code in 0..(4 * input::QUEUE as u32) {
            peer.write_msg(&LinkMsg::Input(input::Event::Key {
                code,
                pressed: true,
            }))
            .await
            .unwrap();
        }
        let deaf = |wire: Wire| async move {
            let _wire = wire;
            std::future::pending::<()>().await
        };
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), deaf))
            .await
            .expect("the reader must keep reading past an input nobody takes");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn a_keymap_larger_than_any_keyboard_s_ends_the_link() {
        let (a, b) = tokio::io::duplex(2 * input::MAX_KEYMAP);
        let mut peer = Framed::new(b);
        let keymap = input::Event::Keymap("x".repeat(input::MAX_KEYMAP + 1));
        peer.write_msg(&LinkMsg::Input(keymap)).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), nothing))
            .await
            .expect("the link must give up on an oversized keymap");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn frames_the_display_cannot_take_are_acked_and_a_keyframe_asked_for() {
        let mut ends = display_link(1 << 20).await;
        // Home's display reads nothing: its queue fills.
        for id in 0..DISPLAY_INBOUND as u64 + 20 {
            let (msg, payloads) = video_frame(id, 16);
            ends.remote.outbox.send(msg, payloads);
        }
        loop {
            match incoming(&mut ends.remote).await {
                Incoming::Feedback(ClientMsg::RequestKeyframe) => break,
                Incoming::Feedback(ClientMsg::FrameAck { .. }) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        // The link stays up for frames; an order that cannot wait ends it.
        assert!(!ends.home_ended.is_finished());
        ends.remote
            .outbox
            .send(DisplayMsg::Display(Display::Stop), Vec::new());
        let error = tokio::time::timeout(Duration::from_secs(2), ends.home_ended)
            .await
            .expect("the link ends")
            .unwrap();
        assert!(error.to_string().contains("fell behind"), "{error}");
        drop(ends.home);
    }
}
