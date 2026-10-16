//! The link once paired: the remote's agent dials home and keeps a connection, home serves it.
//! Over it, a hello (the link's version), then gliff's framing carrying a heartbeat each way
//! and the connection's services (the clipboard bridge, the display), over the same
//! authenticated stream. The link stays up between two modes, so that a start is quick, but
//! carries nothing of the user's then: no copy, no sound, no mic, no input.
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
use std::time::{Duration, Instant};

use bytes::Bytes;
use gliff_proto::clipboard::CHUNK;
use gliff_proto::{ClipboardMsg, ServerMsg};
use gliff_transport::Framed;
use gliff_transport::clipboard::{Outbound, outbound_channel};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::audio::{self, codec::MAX_PACKET};
use crate::display::wire::{Cursor, Incoming, MAX_CURSOR, MAX_OUTPUTS, Outbox, Peer, Pointer};
use crate::frame::{read_frame, write_frame};
use crate::input;
use crate::network::{HomeNetwork, RECHECK, Tables};
use crate::store::{Role, Store};
use crate::transport::{Accept, Dial, TlsAcceptor, TlsDialer};
use crate::wire::{Clip, LinkMsg, checked, output_id};

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

/// While the picture keeps the link busy, the clipboard still gets one message in this many
/// frames: its offers, requests and acks, and a transfer's chunks, never wait past gliff's idle
/// timeout.
const FRAMES_PER_CLIP: usize = 8;

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
    /// Whether a mode runs, as the display says it: the link stays up between modes so that a
    /// start is quick, but nothing of the user's crosses outside one.
    pub mode: watch::Receiver<bool>,
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
    /// Capped at a few seconds: a refused dial to a LAN address costs nothing, and home may be
    /// back at any moment, so the agent must not be found asleep for half a minute.
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(3))
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
    /// The agent is not on home's network: it does not dial.
    Away,
    /// The agent is back on home's network: it dials again.
    Back,
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
    let (pointer_tx, pointer) = watch::channel(Vec::new());
    let input = input::Wire::default();
    let (mode, in_mode) = watch::channel(false);
    let sound_mode = mode.subscribe();
    let display = Peer {
        outbox: outbox.clone(),
        inbox: display_rx,
        pointer,
        input: input.clone(),
        mode,
    };
    let (sound, sound_end) = audio::wire();
    let service = async {
        attach(Wire {
            from_peer,
            to_peer,
            display: Some(display),
            audio: Some(sound),
            mode: in_mode,
        })
        .await;
        std::future::pending::<Infallible>().await
    };
    let result = tokio::select! {
        result = send(
            Framed::new(writer),
            heartbeat.interval,
            outbound_rx,
            (sound_end.from_audio, sound_mode),
            &outbox,
            &input.to_peer,
        ) => result,
        result = receive(
            Framed::new(reader),
            heartbeat.timeout,
            inbound_tx,
            sound_end.to_audio,
            (display_tx, pointer_tx),
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
    (mut sound, mode): (mpsc::Receiver<audio::Message>, watch::Receiver<bool>),
    display: &Outbox,
    input: &input::Queue,
) -> io::Result<Infallible> {
    let mut tick = tokio::time::interval(interval);
    // A long chunk write must not be followed by a burst of heartbeats.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut open = true;
    let mut sound_open = true;
    let mut frames = 0;
    loop {
        tokio::select! {
            // Input first: a key must not wait behind a video frame already queued.
            biased;
            Some((output, event)) = input.next() => {
                writer
                    .write_msg(&LinkMsg::Input(event, output_id(output)))
                    .await
                    .map_err(io_error)?
            }
            msg = sound.recv(), if sound_open => match msg {
                // What the remote plays, or the home mic hears, never crosses outside a mode,
                // whatever the other side asked.
                Some(msg) if msg.is_frame() && !in_mode(&mode) => {}
                Some(msg) => {
                    let packet = match &msg {
                        audio::Message::Frame { packet, .. }
                        | audio::Message::MicFrame { packet, .. } => &packet[..],
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
            // The picture waits once it had enough turns in a row with the clipboard waiting.
            (output, msg, payloads) = display.next(),
                if frames < FRAMES_PER_CLIP || outbound.is_empty() =>
            {
                let parts: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
                let begin = Instant::now();
                writer
                    .write_msg_with_payloads(&LinkMsg::from((output, msg)), &parts)
                    .await
                    .map_err(io_error)?;
                display.report_write(output, begin.elapsed());
                frames += 1;
            }
            // The clipboard last: a file's chunk waits for the picture, the sound and the input,
            // and gliff's window (four chunks in flight) keeps a transfer from piling up more.
            clip = outbound.recv(), if open => match clip {
                Some((msg, payload)) => {
                    writer
                        .write_msg_with_payloads(&LinkMsg::from(msg), &[&payload])
                        .await
                        .map_err(io_error)?;
                    frames = 0;
                }
                None => open = false,
            },
        }
    }
}

/// Whether the display says a mode runs; with no display, none does.
pub fn in_mode(mode: &watch::Receiver<bool>) -> bool {
    *mode.borrow() && mode.has_changed().is_ok()
}

/// The output a message names; one past the most a mode streams ends the link.
fn output_of(id: Option<u8>) -> io::Result<u8> {
    match id.unwrap_or(0) {
        output if output < MAX_OUTPUTS => Ok(output),
        output => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a message for output {output}, past the {MAX_OUTPUTS} a mode streams"),
        )),
    }
}

/// An output's pointer, the ones before it made if none came yet.
fn pointer_of(pointers: &mut Vec<Pointer>, output: u8) -> &mut Pointer {
    let index = usize::from(output);
    if pointers.len() <= index {
        pointers.resize(index + 1, Pointer::default());
    }
    &mut pointers[index]
}

async fn receive<R: AsyncRead + Unpin>(
    mut reader: Framed<R>,
    timeout: Duration,
    inbound: mpsc::Sender<Clip>,
    sound: audio::ToAudio,
    (display, pointer): (mpsc::Sender<Incoming>, watch::Sender<Vec<Pointer>>),
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
            LinkMsg::AudioStart { .. }
            | LinkMsg::AudioStop
            | LinkMsg::AudioFrame { .. }
            | LinkMsg::MicStart { .. }
            | LinkMsg::MicStop
            | LinkMsg::MicFrame { .. }
            | LinkMsg::AudioVolume { .. } => {
                let sound_msg = audio_received(&mut reader, timeout, msg).await?;
                // Like the clipboard's, a frame the audio thread is not ready for is dropped; an
                // order is not.
                let _ = sound.send(sound_msg);
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
            LinkMsg::Video(msg, output) => {
                let output = output_of(output)?;
                let (main, aux) = match &msg {
                    ServerMsg::VideoFrame {
                        data_len, aux_len, ..
                    } => (*data_len, *aux_len),
                    ServerMsg::CursorShape { argb_len, .. } => (*argb_len, 0),
                    _ => (0, 0),
                };
                const MAX_CURSOR_BYTES: u32 = MAX_CURSOR * MAX_CURSOR * 4;
                let cursor = matches!(msg, ServerMsg::CursorShape { .. });
                if main.max(aux) > MAX_VIDEO_PAYLOAD || (cursor && main > MAX_CURSOR_BYTES) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "a video frame or a cursor larger than any screen's",
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
                if let ServerMsg::CursorPos { visible, .. } = msg {
                    pointer.send_if_modified(|pointers| {
                        let pointer = pointer_of(pointers, output);
                        std::mem::replace(&mut pointer.visible, visible) != visible
                    });
                    continue;
                }
                if let ServerMsg::CursorShape {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    ..
                } = msg
                {
                    let shape = Cursor {
                        width,
                        height,
                        hot_x,
                        hot_y,
                        argb: main,
                    };
                    pointer
                        .send_modify(|pointers| pointer_of(pointers, output).shape = Some(shape));
                    continue;
                }
                let video = Incoming::Video {
                    output,
                    msg,
                    main,
                    aux,
                };
                if let Err(mpsc::error::TrySendError::Full(Incoming::Video { msg, .. })) =
                    display.try_send(video)
                {
                    outbox.on(output).refuse_frame(&msg);
                }
                continue;
            }
            LinkMsg::Input(input::Event::Keymap(text), _) if text.len() > input::MAX_KEYMAP => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a keymap larger than any keyboard's",
                ));
            }
            // Never waits: the queue's own policy keeps it bounded and no key held.
            LinkMsg::Input(event, output) => {
                input.on(output_of(output)?).push(event);
                continue;
            }
            LinkMsg::Feedback(msg, output) => {
                let output = output_of(output)?;
                let _ = display.try_send(Incoming::Feedback { output, msg });
                continue;
            }
            LinkMsg::NoInput { reason } => {
                let _ = display.try_send(Incoming::NoInput(reason));
                continue;
            }
            LinkMsg::NoLid { reason } => {
                let _ = display.try_send(Incoming::NoLid(reason));
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
        LinkMsg::AudioVolume {
            session,
            left,
            right,
            muted,
        } => audio::Message::Volume {
            session,
            level: audio::Level { left, right, muted },
        },
        LinkMsg::MicStart { session } => audio::Message::MicStart { session },
        LinkMsg::MicFrame {
            session,
            seq,
            data_len,
        } if data_len as usize <= MAX_PACKET => audio::Message::MicFrame {
            session,
            seq,
            packet: tokio::time::timeout(timeout, reader.read_payload(data_len))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the peer went silent"))?
                .map_err(io_error)?,
        },
        LinkMsg::AudioFrame { .. } | LinkMsg::MicFrame { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "an audio packet larger than Opus makes",
            ));
        }
        LinkMsg::MicStop => audio::Message::MicStop,
        _ => audio::Message::Stop,
    })
}

/// `navette agent`: dials home, keeps the link up, and dials again whenever it drops. `attach`
/// starts the connection's service each time the link is up. Away from home's network, it does
/// not dial, and looks again every `RECHECK`. Never returns.
pub async fn agent<D, T, F, Fut>(
    dialer: D,
    mut network: HomeNetwork<T>,
    heartbeat: Heartbeat,
    mut backoff: Backoff,
    events: UnboundedSender<Event>,
    attach: F,
) where
    D: Dial,
    T: Tables,
    F: Fn(Wire) -> Fut,
    Fut: Future<Output = ()>,
{
    // Said once per change, not at each check.
    let mut away = false;
    loop {
        if !network.at_home() {
            if !away {
                away = true;
                let _ = events.send(Event::Away);
            }
            tokio::time::sleep(RECHECK).await;
            continue;
        }
        if away {
            away = false;
            let _ = events.send(Event::Back);
        }
        match dialer.dial().await {
            Ok(mut stream) => match hello(&mut stream, heartbeat).await {
                Ok(()) => {
                    backoff.reset();
                    if let Err(e) = network.linked() {
                        let _ = events.send(Event::Failed(e));
                    }
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use gliff_proto::{ClientMsg, ClipboardFile};
    use tokio::sync::mpsc;

    use super::*;
    use crate::display::wire::{Display, Msg as DisplayMsg};
    use crate::identity::Identity;
    use crate::network::tests::{FakeTables, HOME_ROUTER, OTHER_ROUTER, mac, paired_remote};
    use crate::pair::{self, Pin, Timeouts};
    use crate::transport::bind_lan;

    fn fast() -> Heartbeat {
        Heartbeat {
            interval: Duration::from_millis(20),
            timeout: Duration::from_millis(200),
        }
    }

    async fn nothing(_: Wire) {}

    /// The gate of a pairing that does not know home's router: it dials wherever it is.
    pub(crate) fn anywhere(store: &Store) -> HomeNetwork<FakeTables> {
        HomeNetwork::new(FakeTables::default(), store.clone()).unwrap()
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

    pub(crate) struct Desk {
        pub(crate) tmp: tempfile::TempDir,
        pub(crate) home: Store,
        pub(crate) remote: Store,
    }

    /// Two machines paired over loopback, the way `navette pair` does it.
    pub(crate) async fn paired_desk() -> Desk {
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
            pair::remote(&desk.remote, address, None, &pin, Timeouts::default()),
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
    async fn a_mic_packet_larger_than_opus_makes_ends_the_link() {
        let (a, b) = tokio::io::duplex(2 * MAX_PACKET);
        let mut peer = Framed::new(b);
        let data = vec![0u8; MAX_PACKET + 1];
        peer.write_msg_with_payloads(
            &LinkMsg::MicFrame {
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

    /// The next sound or display message the link sent.
    async fn next_sound_or_order(peer: &mut Framed<tokio::io::DuplexStream>) -> LinkMsg {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match peer.read_msg::<LinkMsg>().await.unwrap() {
                    msg @ (LinkMsg::AudioFrame { data_len, .. }
                    | LinkMsg::MicFrame { data_len, .. }) => {
                        peer.read_payload(data_len).await.unwrap();
                        return msg;
                    }
                    msg @ LinkMsg::Display(_) => return msg,
                    _ => {}
                }
            }
        })
        .await
        .expect("a message in time")
    }

    #[tokio::test]
    async fn with_the_link_up_and_no_mode_no_sound_crosses_either_way() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut peer = Framed::new(b);
        let (taken, ends) = tokio::sync::oneshot::channel();
        let service = |mut wire: Wire| async move {
            let _ = taken.send((wire.audio.take().unwrap(), wire.display.take().unwrap()));
            std::future::pending::<()>().await
        };
        tokio::spawn(run(a, fast(), service));
        let (sound, display) = ends.await.unwrap();
        let packet = Bytes::from_static(&[1, 2, 3]);
        let frames = |seq| {
            // The remote's sound, and home's mic.
            for frame in [
                audio::Message::Frame {
                    session: 1,
                    seq,
                    packet: packet.clone(),
                },
                audio::Message::MicFrame {
                    session: 1,
                    seq,
                    packet: packet.clone(),
                },
            ] {
                sound.to_peer.try_send(frame).unwrap();
            }
        };
        // The sound goes before an order: the order read, the frames before it were taken.
        let order = || {
            display
                .outbox
                .send(DisplayMsg::Display(Display::Stop), Vec::new())
        };
        let stop = LinkMsg::Display(Display::Stop);

        frames(0);
        order();
        assert_eq!(next_sound_or_order(&mut peer).await, stop);

        display.mode.send_replace(true);
        frames(1);
        assert!(matches!(
            next_sound_or_order(&mut peer).await,
            LinkMsg::AudioFrame { seq: 1, .. }
        ));
        assert!(matches!(
            next_sound_or_order(&mut peer).await,
            LinkMsg::MicFrame { seq: 1, .. }
        ));

        display.mode.send_replace(false);
        frames(2);
        order();
        assert_eq!(next_sound_or_order(&mut peer).await, stop);
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

    #[tokio::test]
    async fn an_order_after_a_flood_of_frames_still_reaches_the_audio_thread() {
        let (a, b) = tokio::io::duplex(1024 * 1024);
        let mut peer = Framed::new(b);
        // The audio thread stalls (PipeWire slow to answer) while the mic keeps sending.
        for seq in 0..(4 * audio::QUEUE as u32) {
            peer.write_msg_with_payloads(
                &LinkMsg::MicFrame {
                    session: 1,
                    seq,
                    data_len: 3,
                },
                &[&[1, 2, 3]],
            )
            .await
            .unwrap();
        }
        peer.write_msg(&LinkMsg::MicStop).await.unwrap();
        peer.write_msg(&LinkMsg::AudioStop).await.unwrap();
        let (taken, sound) = tokio::sync::oneshot::channel();
        let stalled = |mut wire: Wire| async move {
            let _ = taken.send(wire.audio.take().unwrap());
            std::future::pending::<()>().await
        };
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), stalled))
            .await
            .expect("the link ends once the peer goes silent");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let sound = sound.await.unwrap();
        let got: Vec<audio::Message> =
            std::iter::from_fn(|| sound.from_peer.try_recv().ok()).collect();
        let frames = got
            .iter()
            .filter(|m| matches!(m, audio::Message::MicFrame { .. }))
            .count();
        assert!(frames <= audio::QUEUE, "{frames} frames queued");
        assert_eq!(
            got[frames..],
            [audio::Message::MicStop, audio::Message::Stop],
            "the orders were dropped"
        );
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

    /// What "within seconds" means for a redial to a LAN address.
    const A_FEW_SECONDS: Duration = Duration::from_secs(5);

    #[test]
    fn the_backoff_never_exceeds_the_cap() {
        let mut backoff = Backoff::default();
        let cap = backoff.max;
        assert!(cap <= A_FEW_SECONDS, "a LAN redial is cheap: {cap:?}");
        for _ in 0..1000 {
            assert!(backoff.next_delay() <= cap);
        }
    }

    /// A dialer that refuses until `home_is_back`, then answers like home does.
    struct Outage {
        home_is_back: Arc<AtomicBool>,
    }

    impl Dial for Outage {
        type Stream = tokio::io::DuplexStream;

        async fn dial(&self) -> io::Result<Self::Stream> {
            if !self.home_is_back.load(Ordering::SeqCst) {
                return Err(io::ErrorKind::ConnectionRefused.into());
            }
            let (agent_side, mut home_side) = tokio::io::duplex(1024);
            tokio::spawn(async move {
                let _ = hello(&mut home_side, Heartbeat::default()).await;
                // Keeps the link up until the test ends.
                std::future::pending::<()>().await;
            });
            Ok(agent_side)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_agent_redials_within_the_cap_after_a_long_outage() {
        let tmp = tempfile::tempdir().unwrap();
        let home_is_back = Arc::new(AtomicBool::new(false));
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let backoff = Backoff::default();
        let agent = tokio::spawn(agent(
            Outage {
                home_is_back: home_is_back.clone(),
            },
            anywhere(&Store::new(tmp.path())),
            Heartbeat::default(),
            backoff,
            events_tx,
            nothing,
        ));
        // Ten minutes without home: far more failures than it takes to reach the cap.
        tokio::time::sleep(Duration::from_secs(600)).await;
        while events.try_recv().is_ok() {}
        // Home comes back right after a refused dial, the worst moment: the agent has just
        // started its longest wait.
        while !matches!(events.recv().await, Some(Event::Retrying(_))) {}
        home_is_back.store(true, Ordering::SeqCst);
        let back = tokio::time::Instant::now();
        let waited = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if let Event::Connected = events.recv().await.expect("the agent is alive") {
                    return back.elapsed();
                }
            }
        })
        .await
        .expect("the agent dials home again");
        agent.abort();
        assert!(waited <= A_FEW_SECONDS, "dialled again after {waited:?}");
    }

    /// A dialer that answers like home does, and counts its dials.
    #[derive(Clone, Default)]
    struct Counted {
        dials: Arc<AtomicUsize>,
    }

    impl Dial for Counted {
        type Stream = tokio::io::DuplexStream;

        async fn dial(&self) -> io::Result<Self::Stream> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            let (agent_side, mut home_side) = tokio::io::duplex(1024);
            tokio::spawn(async move {
                let _ = hello(&mut home_side, Heartbeat::default()).await;
                std::future::pending::<()>().await;
            });
            Ok(agent_side)
        }
    }

    /// An agent paired with home's router as `gateway`, on `tables`.
    fn gated_agent(
        store: &Store,
        tables: FakeTables,
    ) -> (
        Counted,
        tokio::task::JoinHandle<()>,
        mpsc::UnboundedReceiver<Event>,
    ) {
        let dialer = Counted::default();
        let (events_tx, events) = mpsc::unbounded_channel();
        let agent = tokio::spawn(agent(
            dialer.clone(),
            HomeNetwork::new(tables, store.clone()).unwrap(),
            Heartbeat::default(),
            Backoff::default(),
            events_tx,
            nothing,
        ));
        (dialer, agent, events)
    }

    fn drained(events: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        std::iter::from_fn(|| events.try_recv().ok()).collect()
    }

    /// Away from home for ten minutes: no dial, and one line said, not one per check.
    async fn never_dialed_and_said_once(tables: FakeTables) {
        let (_tmp, store) = paired_remote(Some(HOME_ROUTER));
        let (dialer, agent, mut events) = gated_agent(&store, tables);
        tokio::time::sleep(Duration::from_secs(600)).await;
        agent.abort();
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 0);
        let said = drained(&mut events);
        assert!(matches!(said.as_slice(), [Event::Away]), "said {said:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_agent_dials_behind_home_s_router() {
        let (_tmp, store) = paired_remote(Some(HOME_ROUTER));
        let (_, agent, mut events) = gated_agent(&store, FakeTables::behind(HOME_ROUTER));
        let first = tokio::time::timeout(A_FEW_SECONDS, events.recv()).await;
        agent.abort();
        assert!(matches!(first, Ok(Some(Event::Connected))), "{first:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn behind_another_router_the_agent_never_dials_and_says_so_once() {
        never_dialed_and_said_once(FakeTables::behind(OTHER_ROUTER)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn with_no_router_the_agent_never_dials_and_says_so_once() {
        never_dialed_and_said_once(FakeTables::default()).await;
    }

    #[tokio::test(start_paused = true)]
    async fn with_no_arp_entry_for_the_router_the_agent_never_dials_and_says_so_once() {
        let tables = FakeTables::behind(HOME_ROUTER);
        tables.set(
            &tables.route().unwrap(),
            "IP address       HW type     Flags       HW address            Mask     Device\n",
        );
        never_dialed_and_said_once(tables).await;
    }

    #[tokio::test(start_paused = true)]
    async fn back_on_home_s_network_the_agent_dials_again() {
        let (_tmp, store) = paired_remote(Some(HOME_ROUTER));
        let tables = FakeTables::behind(OTHER_ROUTER);
        let (_, agent, mut events) = gated_agent(&store, tables.clone());
        tokio::time::sleep(Duration::from_secs(600)).await;
        tables.move_behind(HOME_ROUTER);
        let back = tokio::time::Instant::now();
        let said = tokio::time::timeout(RECHECK + A_FEW_SECONDS, async {
            let mut said = Vec::new();
            loop {
                let event = events.recv().await.expect("the agent is alive");
                let connected = matches!(event, Event::Connected);
                said.push(event);
                if connected {
                    return said;
                }
            }
        })
        .await
        .expect("the agent dials home again");
        agent.abort();
        assert!(
            back.elapsed() <= RECHECK,
            "linked after {:?}",
            back.elapsed()
        );
        assert!(
            matches!(
                said.as_slice(),
                [Event::Away, Event::Back, Event::Connected]
            ),
            "said {said:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_old_pairing_learns_home_s_router_at_its_first_link() {
        let (_tmp, store) = paired_remote(None);
        let (_, agent, mut events) = gated_agent(&store, FakeTables::behind(HOME_ROUTER));
        until_connected(&mut events).await;
        agent.abort();
        assert_eq!(
            store.pairing().unwrap().unwrap().gateway,
            Some(mac(HOME_ROUTER))
        );
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
                    anywhere(&desk.remote),
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
                    anywhere(&desk.remote),
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
                    anywhere(&desk.remote),
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
    async fn the_remote_cursor_s_image_crosses_the_link() {
        let mut ends = display_link(64 * 1024).await;
        let shape = ServerMsg::CursorShape {
            id: 1,
            width: 2,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            argb_len: 8,
        };
        ends.remote
            .outbox
            .send(DisplayMsg::Video(shape), vec![vec![5; 8]]);
        let cursor = newest_cursor(&mut ends.home).await;
        assert_eq!((cursor.width, cursor.height), (2, 1));
        assert_eq!(&cursor.argb[..], &[5; 8]);
    }

    /// The remote pointer's image, once one has come.
    async fn newest_cursor(peer: &mut Peer) -> Cursor {
        tokio::time::timeout(
            Duration::from_secs(5),
            peer.pointer
                .wait_for(|pointers| pointers.first().is_some_and(|p| p.shape.is_some())),
        )
        .await
        .expect("a cursor in time")
        .expect("the link is up")[0]
            .shape
            .clone()
            .unwrap()
    }

    #[tokio::test]
    async fn the_remote_hiding_its_cursor_is_heard_at_home() {
        let mut ends = display_link(64 * 1024).await;
        let seen = |visible| {
            DisplayMsg::Video(ServerMsg::CursorPos {
                x: 0.0,
                y: 0.0,
                shape_id: 0,
                visible,
            })
        };
        for visible in [false, true] {
            ends.remote.outbox.send(seen(visible), Vec::new());
            tokio::time::timeout(
                Duration::from_secs(5),
                ends.home.pointer.wait_for(|pointers| {
                    pointers.first().cloned().unwrap_or_default().visible == visible
                }),
            )
            .await
            .expect("the cursor's visibility in time")
            .unwrap();
        }
    }

    #[tokio::test]
    async fn each_output_s_frames_and_cursor_reach_home_apart() {
        let mut ends = display_link(64 * 1024).await;
        let second = ends.remote.outbox.on(1);
        let (msg, payloads) = video_frame(4, 16);
        second.send(msg, payloads);
        let shape = ServerMsg::CursorShape {
            id: 1,
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            argb_len: 4,
        };
        second.send(DisplayMsg::Video(shape), vec![vec![3; 4]]);
        match incoming(&mut ends.home).await {
            Incoming::Video { output, .. } => assert_eq!(output, 1),
            other => panic!("expected a video frame, got {other:?}"),
        }
        let pointers = tokio::time::timeout(
            Duration::from_secs(5),
            ends.home.pointer.wait_for(|pointers| pointers.len() == 2),
        )
        .await
        .expect("output 1's cursor in time")
        .unwrap()
        .clone();
        assert_eq!(pointers[0], Pointer::default());
        assert_eq!(&pointers[1].shape.as_ref().unwrap().argb[..], &[3; 4]);
        // Home's acks, and home's input, go back to output 1.
        ends.home
            .outbox
            .on(1)
            .send(DisplayMsg::Feedback(ClientMsg::RequestKeyframe), Vec::new());
        match incoming(&mut ends.remote).await {
            Incoming::Feedback { output, msg } => {
                assert_eq!((output, msg), (1, ClientMsg::RequestKeyframe))
            }
            other => panic!("expected feedback, got {other:?}"),
        }
        ends.home.input.to_peer.on(1).push(input::Event::ReleaseAll);
        let got = tokio::time::timeout(Duration::from_secs(5), ends.remote.input.from_peer.next())
            .await
            .expect("the event in time");
        assert_eq!(got, Some((1, input::Event::ReleaseAll)));
    }

    #[tokio::test]
    async fn a_message_for_an_output_past_the_most_a_mode_streams_ends_the_link() {
        let (a, b) = tokio::io::duplex(4096);
        let mut peer = Framed::new(b);
        let event = LinkMsg::Input(input::Event::ReleaseAll, Some(MAX_OUTPUTS));
        peer.write_msg(&event).await.unwrap();
        let nothing = |_: Wire| std::future::ready(());
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), nothing))
            .await
            .expect("the link must give up");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn the_remote_cursor_is_never_lost_behind_a_full_display_queue() {
        let mut ends = display_link(1 << 20).await;
        // Home's display reads nothing: its queue fills.
        for id in 0..DISPLAY_INBOUND as u64 + 20 {
            let (msg, payloads) = video_frame(id, 16);
            ends.remote.outbox.send(msg, payloads);
        }
        let shape = |id| ServerMsg::CursorShape {
            id,
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            argb_len: 4,
        };
        ends.remote
            .outbox
            .send(DisplayMsg::Video(shape(1)), vec![vec![1; 4]]);
        assert_eq!(&newest_cursor(&mut ends.home).await.argb[..], &[1; 4]);
        ends.remote
            .outbox
            .send(DisplayMsg::Video(shape(2)), vec![vec![2; 4]]);
        tokio::time::timeout(
            Duration::from_secs(5),
            ends.home.pointer.wait_for(|pointers| {
                pointers
                    .first()
                    .and_then(|p| p.shape.as_ref())
                    .is_some_and(|c| c.argb[..] == [2; 4])
            }),
        )
        .await
        .expect("the newer cursor in time")
        .unwrap();
    }

    #[tokio::test]
    async fn the_remote_s_no_input_notice_crosses_the_link() {
        let mut ends = display_link(64 * 1024).await;
        ends.remote.outbox.send(
            DisplayMsg::NoInput("no virtual keyboard".into()),
            Vec::new(),
        );
        match incoming(&mut ends.home).await {
            Incoming::NoInput(reason) => assert_eq!(reason, "no virtual keyboard"),
            other => panic!("expected the notice, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_remote_s_no_lid_notice_crosses_the_link() {
        let mut ends = display_link(64 * 1024).await;
        ends.remote
            .outbox
            .send(DisplayMsg::NoLid("Access denied".into()), Vec::new());
        match incoming(&mut ends.home).await {
            Incoming::NoLid(reason) => assert_eq!(reason, "Access denied"),
            other => panic!("expected the notice, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_peer_gone_in_the_middle_of_a_frame_ends_the_link_in_time() {
        let (a, mut b) = tokio::io::duplex(64 * 1024);
        // The frame's header says 100 kB follow; the peer sends 10 and is heard no more.
        let (msg, _) = video_frame(0, 100_000);
        let body = minicbor::to_vec(LinkMsg::from((0, msg))).unwrap();
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
            input::Event::Motion {
                x: 10.0,
                y: 20.0,
                delta: None,
            },
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
            assert_eq!(got, Some((0, event)));
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
            peer.write_msg(&LinkMsg::Input(
                input::Event::Key {
                    code,
                    pressed: true,
                },
                None,
            ))
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
        peer.write_msg(&LinkMsg::Input(keymap, None)).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), nothing))
            .await
            .expect("the link must give up on an oversized keymap");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_cursor_larger_than_any_cursor_ends_the_link() {
        let side = MAX_CURSOR;
        let argb_len = side * side * 4 + 4;
        let (a, mut b) = tokio::io::duplex(argb_len as usize + 1024);
        let shape = ServerMsg::CursorShape {
            id: 1,
            width: side,
            height: side + 1,
            hot_x: 0,
            hot_y: 0,
            argb_len,
        };
        // The image comes whole: only its size is wrong.
        let body = minicbor::to_vec(LinkMsg::Video(shape, None)).unwrap();
        let mut bytes = (body.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&argb_len.to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes.resize(bytes.len() + argb_len as usize, 255);
        tokio::io::AsyncWriteExt::write_all(&mut b, &bytes)
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), run(a, fast(), nothing))
            .await
            .expect("the link must give up on an oversized cursor");
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
                Incoming::Feedback {
                    msg: ClientMsg::RequestKeyframe,
                    ..
                } => break,
                Incoming::Feedback {
                    msg: ClientMsg::FrameAck { .. },
                    ..
                } => {}
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

    #[tokio::test]
    async fn a_picture_frame_never_waits_behind_the_clipboard_s_chunks() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (queued, mut ready) = mpsc::unbounded_channel();
        // Everything is queued before the link writes a byte past the duplex's room: chunks of
        // a file transfer first, then one frame.
        tokio::spawn(run(a, fast(), move |wire: Wire| async move {
            for n in 0..6u64 {
                let chunk = ClipboardMsg::Data {
                    id: 1,
                    offset: n * CHUNK as u64,
                    data_len: CHUNK as u32,
                    done: false,
                };
                wire.to_peer
                    .send((chunk, Bytes::from(vec![0; CHUNK])))
                    .await
                    .unwrap();
            }
            let (msg, payloads) = video_frame(0, 16);
            wire.display.as_ref().unwrap().outbox.send(msg, payloads);
            queued.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        ready.recv().await.unwrap();
        let mut peer = Framed::new(b);
        let mut chunks_before = 0;
        loop {
            let msg: LinkMsg = tokio::time::timeout(Duration::from_secs(5), peer.read_msg())
                .await
                .expect("a message in time")
                .unwrap();
            match msg {
                LinkMsg::ClipboardData { data_len, .. } => {
                    peer.read_payload(data_len).await.unwrap();
                    chunks_before += 1;
                }
                LinkMsg::Video(ServerMsg::VideoFrame { .. }, _) => break,
                _ => {}
            }
        }
        // The chunk already on its way when the frame came, at most.
        assert!(chunks_before <= 1, "{chunks_before} chunks went first");
    }

    #[tokio::test]
    async fn the_clipboard_keeps_moving_under_a_saturated_video_stream() {
        // A narrow line: each frame takes several reads, and two outputs refill the queue
        // meanwhile, so a frame always waits when the link picks what to write next.
        let (a, b) = tokio::io::duplex(16 * 1024);
        // The peer here sends no heartbeat: the link must not give up on it meanwhile.
        let patient = Heartbeat {
            interval: Duration::from_millis(20),
            timeout: Duration::from_secs(10),
        };
        tokio::spawn(run(a, patient, move |wire: Wire| async move {
            for n in 0..3u64 {
                let chunk = ClipboardMsg::Data {
                    id: 1,
                    offset: n * 16,
                    data_len: 16,
                    done: false,
                };
                wire.to_peer
                    .send((chunk, Bytes::from(vec![0; 16])))
                    .await
                    .unwrap();
            }
            let outbox = &wire.display.as_ref().unwrap().outbox;
            let mut id = 0;
            loop {
                for output in 0..2 {
                    let outbox = outbox.on(output);
                    if outbox.video_ready() {
                        let (msg, payloads) = video_frame(id, 60_000);
                        outbox.send(msg, payloads);
                        id += 1;
                    }
                }
                tokio::task::yield_now().await;
            }
        }));
        let mut peer = Framed::new(b);
        let mut gaps = Vec::new();
        let mut frames = 0;
        let three_turns = async {
            while gaps.len() < 3 {
                match peer.read_msg().await.unwrap() {
                    LinkMsg::ClipboardData { data_len, .. } => {
                        peer.read_payload(data_len).await.unwrap();
                        gaps.push(std::mem::take(&mut frames));
                    }
                    LinkMsg::Video(
                        ServerMsg::VideoFrame {
                            data_len, aux_len, ..
                        },
                        _,
                    ) => {
                        peer.read_payload(data_len).await.unwrap();
                        peer.read_payload(aux_len).await.unwrap();
                        frames += 1;
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(2), three_turns)
            .await
            .unwrap_or_else(|_| panic!("the clipboard's turn never came: {frames} frames"));
        // The frames still go first: the clipboard gets one message in so many.
        assert!(
            gaps.iter().all(|&gap| gap >= FRAMES_PER_CLIP),
            "frames between clipboard messages: {gaps:?}"
        );
    }
}
