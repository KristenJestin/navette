//! What crosses the link once both sides have said hello: Navette's messages in CBOR, over
//! gliff's framing, so a video frame's bytes go straight from the encoder to the socket and
//! from the socket to the decoder. The heartbeat is one of these messages.
//!
//! The write queue is adapted from gliff-server's `writer.rs` (gliff, MIT, see
//! `LICENSES/gliff-MIT.txt`): a video frame waits alone, and a newer cursor or stream message
//! replaces an older one still queued, so the queue stays short however slow the link.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use gliff_proto::{ClientCaps, ClientMsg, ServerMsg};
use gliff_transport::Framed;
use minicbor::{Decode, Encode};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Everything either side sends after the hello.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Msg {
    #[n(0)]
    Heartbeat,
    #[n(1)]
    Display(#[n(0)] Display),
    /// Remote to home: gliff's stream messages (config, frames, cursor, pings).
    #[n(2)]
    Video(#[n(0)] ServerMsg),
    /// Home to remote: gliff's answers to them (frame acks, keyframe requests, pongs).
    #[n(3)]
    Feedback(#[n(0)] ClientMsg),
}

/// The display's orders and their answers.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Display {
    /// Home to remote: make a headless output that copies this home monitor, and stream it.
    #[n(0)]
    Start {
        #[n(0)]
        output: OutputCopy,
        /// What home decodes.
        #[n(1)]
        caps: ClientCaps,
    },
    /// Home to remote: stop streaming and remove the output.
    #[n(1)]
    Stop,
    /// Remote to home: the output exists, its stream follows.
    #[n(2)]
    Started {
        #[n(0)]
        output: String,
    },
    /// Remote to home: the output could not be made; nothing was left behind.
    #[n(3)]
    Failed {
        #[n(0)]
        reason: String,
    },
}

/// The home monitor a remote output copies: its mode, scale and orientation.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct OutputCopy {
    /// The mode, unrotated, as Hyprland reports it.
    #[n(0)]
    pub width: u32,
    #[n(1)]
    pub height: u32,
    /// Refresh rate in millihertz.
    #[n(2)]
    pub refresh_mhz: u32,
    #[n(3)]
    pub scale_milli: u32,
    /// `wl_output` transform, 0 to 7.
    #[n(4)]
    pub transform: u8,
}

/// A message received, with the payload bytes it declared.
#[derive(Debug)]
pub enum Incoming {
    Display(Display),
    /// A gliff stream message; `main` and `aux` hold a video frame's two streams.
    Video {
        msg: ServerMsg,
        main: Bytes,
        aux: Bytes,
    },
    Feedback(ClientMsg),
}

/// One live connection, as the display sees it. `inbox` ends when the link drops.
pub struct Peer {
    pub outbox: Outbox,
    pub inbox: UnboundedReceiver<Incoming>,
}

#[derive(Default)]
struct Queue {
    packets: VecDeque<(Msg, Vec<Vec<u8>>)>,
    closed: bool,
}

fn is_video(msg: &Msg) -> bool {
    matches!(msg, Msg::Video(ServerMsg::VideoFrame { .. }))
}

impl Queue {
    fn video_ready(&self) -> bool {
        !self.packets.iter().any(|(msg, _)| is_video(msg))
    }

    fn push(&mut self, msg: Msg, payloads: Vec<Vec<u8>>) {
        match &msg {
            Msg::Video(ServerMsg::StreamConfig { .. }) => {
                // A config queued after the last frame is superseded; one before it is not.
                let after_video = self
                    .packets
                    .iter()
                    .rposition(|(msg, _)| is_video(msg))
                    .map_or(0, |i| i + 1);
                let mut index = 0;
                self.packets.retain(|(msg, _)| {
                    let keep = index < after_video
                        || !matches!(msg, Msg::Video(ServerMsg::StreamConfig { .. }));
                    index += 1;
                    keep
                });
            }
            Msg::Video(ServerMsg::CursorShape { .. }) => self.packets.retain(|(msg, _)| {
                !matches!(
                    msg,
                    Msg::Video(ServerMsg::CursorShape { .. } | ServerMsg::CursorPos { .. })
                )
            }),
            Msg::Heartbeat
            | Msg::Video(ServerMsg::CursorPos { .. } | ServerMsg::Pong { .. })
            | Msg::Feedback(ClientMsg::Pong { .. }) => {
                let same = |queued: &Msg| match (queued, &msg) {
                    (Msg::Video(a), Msg::Video(b)) => {
                        std::mem::discriminant(a) == std::mem::discriminant(b)
                    }
                    (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b),
                };
                self.packets.retain(|(queued, _)| !same(queued));
            }
            _ => {}
        }
        self.packets.push_back((msg, payloads));
    }
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    notify: Notify,
    /// How long each write took, for whoever paces a video stream.
    reports: Mutex<Option<UnboundedSender<Duration>>>,
}

/// The sending side of a link: `send` only queues, so a caller never waits on the network.
#[derive(Clone, Default)]
pub struct Outbox(Arc<Shared>);

impl Outbox {
    /// Queues a message and the payloads it declares. Dropped once the link is down.
    pub fn send(&self, msg: Msg, payloads: Vec<Vec<u8>>) {
        let mut queue = self.0.queue.lock().unwrap();
        if queue.closed {
            return;
        }
        queue.push(msg, payloads);
        drop(queue);
        self.0.notify.notify_one();
    }

    /// No video frame waits to be written: the encoder works one frame ahead at most.
    pub fn video_ready(&self) -> bool {
        self.0.queue.lock().unwrap().video_ready()
    }

    pub fn is_closed(&self) -> bool {
        self.0.queue.lock().unwrap().closed
    }

    /// The duration of every write from now on, for the rate controller.
    pub fn write_reports(&self) -> UnboundedReceiver<Duration> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.0.reports.lock().unwrap() = Some(tx);
        rx
    }

    /// What waits to be written, oldest first.
    #[cfg(test)]
    pub fn queued(&self) -> Vec<Msg> {
        let queue = self.0.queue.lock().unwrap();
        queue.packets.iter().map(|(msg, _)| msg.clone()).collect()
    }

    fn close(&self) {
        let mut queue = self.0.queue.lock().unwrap();
        queue.closed = true;
        queue.packets.clear();
        drop(queue);
        *self.0.reports.lock().unwrap() = None;
    }

    async fn next(&self) -> (Msg, Vec<Vec<u8>>) {
        loop {
            if let Some(packet) = self.0.queue.lock().unwrap().packets.pop_front() {
                return packet;
            }
            self.0.notify.notified().await;
        }
    }
}

/// Carries the link after its hello: heartbeats every `interval`, messages both ways, and
/// gives up after `timeout` without anything from the peer. The peer's messages go to the
/// `Peer` handed to `peers`; returns what ended the link.
pub async fn carry<S>(
    stream: S,
    interval: Duration,
    timeout: Duration,
    peers: Option<&UnboundedSender<Peer>>,
) -> io::Error
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    let outbox = Outbox::default();
    let (inbox_tx, inbox) = mpsc::unbounded_channel();
    if let Some(peers) = peers {
        let _ = peers.send(Peer {
            outbox: outbox.clone(),
            inbox,
        });
    }
    let error = tokio::select! {
        e = write_loop(Framed::new(writer), &outbox, interval) => e,
        e = read_loop(Framed::new(reader), inbox_tx, timeout) => e,
    };
    outbox.close();
    error
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut framed: Framed<W>,
    outbox: &Outbox,
    interval: Duration,
) -> io::Error {
    let mut tick = tokio::time::interval(interval);
    loop {
        let (msg, payloads) = tokio::select! {
            packet = outbox.next() => packet,
            _ = tick.tick() => (Msg::Heartbeat, Vec::new()),
        };
        let parts: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
        let begin = Instant::now();
        if let Err(e) = framed.write_msg_with_payloads(&msg, &parts).await {
            return io_error(e);
        }
        let reports = outbox.0.reports.lock().unwrap();
        if let Some(reports) = reports.as_ref() {
            let _ = reports.send(begin.elapsed());
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut framed: Framed<R>,
    inbox: UnboundedSender<Incoming>,
    timeout: Duration,
) -> io::Error {
    loop {
        // The whole message, payloads included: a peer gone in the middle of a frame would
        // otherwise hold the link until TCP gives up, many minutes later.
        let incoming = match tokio::time::timeout(timeout, read_one(&mut framed)).await {
            Err(_) => return io::Error::new(io::ErrorKind::TimedOut, "the peer went silent"),
            Ok(Err(e)) => return io_error(e),
            Ok(Ok(None)) => continue,
            Ok(Ok(Some(incoming))) => incoming,
        };
        // Nobody listening (an idle link): the message is dropped, the link stays up.
        let _ = inbox.send(incoming);
    }
}

/// One message and its payloads; `None` for a heartbeat.
async fn read_one<R: AsyncRead + Unpin>(
    framed: &mut Framed<R>,
) -> Result<Option<Incoming>, gliff_transport::Error> {
    Ok(Some(match framed.read_msg::<Msg>().await? {
        Msg::Heartbeat => return Ok(None),
        Msg::Display(display) => Incoming::Display(display),
        Msg::Feedback(feedback) => Incoming::Feedback(feedback),
        Msg::Video(msg) => {
            let (main, aux) = match &msg {
                ServerMsg::VideoFrame {
                    data_len, aux_len, ..
                } => (*data_len, *aux_len),
                _ => (0, 0),
            };
            let main = framed.read_payload(main).await?;
            let aux = framed.read_payload(aux).await?;
            Incoming::Video { msg, main, aux }
        }
    }))
}

fn io_error(e: gliff_transport::Error) -> io::Error {
    match e {
        gliff_transport::Error::Io(e) => e,
        gliff_transport::Error::Closed => {
            io::Error::new(io::ErrorKind::UnexpectedEof, "the peer closed the link")
        }
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use gliff_proto::{ChromaMode, Codec};

    use super::*;

    fn frame(id: u64, len: usize) -> (Msg, Vec<Vec<u8>>) {
        (
            Msg::Video(ServerMsg::VideoFrame {
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

    fn caps() -> ClientCaps {
        ClientCaps {
            codecs: vec![Codec::H264],
            max_width: 3840,
            max_height: 2160,
            chroma: vec![ChromaMode::Single420],
            features: Vec::new(),
        }
    }

    async fn next(peer: &mut Peer) -> Incoming {
        tokio::time::timeout(Duration::from_secs(5), peer.inbox.recv())
            .await
            .expect("a message in time")
            .expect("the link is up")
    }

    #[tokio::test]
    async fn orders_and_a_large_video_frame_cross_the_link() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (peers, mut new_peers) = mpsc::unbounded_channel();
        let ms = Duration::from_millis;
        tokio::spawn(async move {
            let peers = peers;
            tokio::join!(
                carry(a, ms(20), ms(500), Some(&peers)),
                carry(b, ms(20), ms(500), Some(&peers)),
            )
        });
        let home = new_peers.recv().await.unwrap();
        let mut remote = new_peers.recv().await.unwrap();

        let output = OutputCopy {
            width: 2560,
            height: 1440,
            refresh_mhz: 165_000,
            scale_milli: 1000,
            transform: 1,
        };
        let start = Display::Start {
            output: output.clone(),
            caps: caps(),
        };
        home.outbox.send(Msg::Display(start.clone()), Vec::new());
        // Two megabytes: far above the pairing frames' limit, one keyframe of a busy screen.
        let (msg, payloads) = frame(0, 2 << 20);
        home.outbox.send(msg, payloads);

        match next(&mut remote).await {
            Incoming::Display(got) => assert_eq!(got, start),
            other => panic!("expected the start order, got {other:?}"),
        }
        match next(&mut remote).await {
            Incoming::Video { main, aux, .. } => {
                assert_eq!(main.len(), 2 << 20);
                assert!(main.iter().all(|&b| b == 7));
                assert_eq!(&aux[..], &[9, 9, 9]);
            }
            other => panic!("expected a video frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn heartbeats_keep_an_idle_link_up_and_silence_ends_it() {
        let ms = Duration::from_millis;
        let (a, b) = tokio::io::duplex(1024);
        let both = async {
            tokio::select! {
                e = carry(a, ms(20), ms(200), None) => e,
                e = carry(b, ms(20), ms(200), None) => e,
            }
        };
        assert!(tokio::time::timeout(ms(2000), both).await.is_err());

        let (a, _silent) = tokio::io::duplex(1024);
        let error = carry(a, ms(20), ms(200), None).await;
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn a_peer_gone_in_the_middle_of_a_frame_ends_the_link_in_time() {
        let ms = Duration::from_millis;
        let (a, b) = tokio::io::duplex(64 * 1024);
        // The frame's header says 100 kB follow; the peer sends 10 and is heard no more.
        let (msg, _) = frame(0, 100_000);
        let body = minicbor::to_vec(&msg).unwrap();
        let mut bytes = (body.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&100_003u32.to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(&[7; 10]);
        let mut peer = b;
        tokio::io::AsyncWriteExt::write_all(&mut peer, &bytes)
            .await
            .unwrap();
        let error = tokio::time::timeout(ms(2000), carry(a, ms(20), ms(200), None))
            .await
            .expect("the link ends within its timeout");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn the_queue_holds_one_frame_and_the_newest_cursor() {
        let mut queue = Queue::default();
        let (msg, payloads) = frame(1, 4);
        queue.push(msg, payloads);
        assert!(!queue.video_ready());
        for id in 0..100 {
            queue.push(Msg::Heartbeat, Vec::new());
            queue.push(
                Msg::Video(ServerMsg::CursorPos {
                    x: id as f64,
                    y: 0.0,
                    shape_id: 0,
                    visible: true,
                }),
                Vec::new(),
            );
            queue.push(Msg::Feedback(ClientMsg::Pong { t: id }), Vec::new());
        }
        assert_eq!(queue.packets.len(), 4);
        assert!(matches!(
            queue.packets[2].0,
            Msg::Video(ServerMsg::CursorPos { x: 99.0, .. })
        ));
    }

    #[test]
    fn a_closed_outbox_drops_what_is_sent() {
        let outbox = Outbox::default();
        outbox.close();
        outbox.send(Msg::Heartbeat, Vec::new());
        assert!(outbox.is_closed());
        assert!(outbox.0.queue.lock().unwrap().packets.is_empty());
    }
}
