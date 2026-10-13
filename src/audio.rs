//! The remote machine's sound, heard at home mixed with home sound.
//!
//! On the remote machine, a virtual sink named for Navette becomes the default output while
//! home asks for the sound; what the apps play into it is Opus-encoded and sent home, and the
//! previous default comes back when home stops, when the link drops or when the agent ends. At
//! home, a playback stream that PipeWire mixes like any other application plays it, through a
//! small jitter buffer, with its own volume in the mixer.
//!
//! PipeWire is reached through [`Graph`]; tests drive a fake one.
//!
//! The audio messages are top-level variants of [`LinkMsg`](crate::wire::LinkMsg):
//! - `AudioStart` (home → remote), with a session number: make the sink the default output and
//!   send its sound under that number.
//! - `AudioStop` (home → remote): put the previous default back and stop sending.
//! - `AudioFrame` (remote → home), with the session and a sequence number: one Opus packet
//!   follows it.
//!
//! Home's mic goes the other way ([`mic`]): `MicStart` and `MicStop` (home → remote) open and
//! close the remote's virtual source, the default mic while a mode runs, and `MicFrame` (home
//! → remote) carries what the home mic hears while it goes to the remote machine.

#[cfg(test)]
pub(crate) mod allocations;
pub mod codec;
#[cfg(test)]
pub mod fake;
pub mod jitter;
pub mod mic;
pub mod pipewire;

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc;

use bytes::{Bytes, BytesMut};

use codec::{Decoder, Encoder, FRAME_SAMPLES, MAX_PACKET};
use jitter::Jitter;

/// The remote's virtual sink.
pub const SINK: &str = "navette-home-speakers";
const SINK_DESCRIPTION: &str = "Home speakers (Navette)";
/// Home's playback stream.
pub const PLAYBACK: &str = "navette-remote-sound";
const PLAYBACK_DESCRIPTION: &str = "Remote machine (Navette)";

/// Home starts playing once 30 ms are buffered, and keeps at most 120 ms.
const JITTER_TARGET: usize = 3 * FRAME_SAMPLES;
const JITTER_MAX: usize = 12 * FRAME_SAMPLES;

/// How often the remote checks that its sink is still there: PipeWire may restart under it.
const CHECK: Duration = Duration::from_millis(250);

/// The audio messages; on the link, they are top-level variants of
/// [`LinkMsg`](crate::wire::LinkMsg).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Home numbers each time it asks, so that frames still on the way from the time before
    /// are told apart.
    Start {
        session: u32,
    },
    Stop,
    Frame {
        session: u32,
        seq: u32,
        packet: Bytes,
    },
    /// Home's mic, the other way (`mic`): the remote's source becomes the default mic for
    /// the session.
    MicStart {
        session: u32,
    },
    MicStop,
    /// Home → remote: one Opus packet of what the home mic hears.
    MicFrame {
        session: u32,
        seq: u32,
        packet: Bytes,
    },
}

/// Audio frames waiting between the link and the audio thread, each way: 64 frames are 640 ms
/// of sound. The link's reader never waits on the audio thread, nor the sink's callback on the
/// link: past this many, a frame is dropped, and the other side fills the gap. Orders (start,
/// stop) are few, and never dropped: a lost stop would leave the remote's default on Navette.
pub const QUEUE: usize = 64;

impl Message {
    fn is_frame(&self) -> bool {
        matches!(self, Message::Frame { .. } | Message::MicFrame { .. })
    }
}

/// What the audio thread of one link gets: the peer's audio messages, and where to put its own.
/// The first ends when the link drops.
pub struct Wire {
    pub from_peer: Inbox,
    pub to_peer: mpsc::Sender<Message>,
}

/// The peer's audio messages, in the order they came, for the audio thread.
pub struct Inbox {
    messages: std::sync::mpsc::Receiver<Message>,
    /// Frames sent and not yet taken.
    frames: Arc<AtomicUsize>,
}

impl Inbox {
    pub fn recv(&self) -> Result<Message, std::sync::mpsc::RecvError> {
        self.messages.recv().map(|m| self.taken(m))
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Message, RecvTimeoutError> {
        self.messages.recv_timeout(timeout).map(|m| self.taken(m))
    }

    pub fn try_recv(&self) -> Result<Message, std::sync::mpsc::TryRecvError> {
        self.messages.try_recv().map(|m| self.taken(m))
    }

    fn taken(&self, message: Message) -> Message {
        if message.is_frame() {
            self.frames.fetch_sub(1, Ordering::Relaxed);
        }
        message
    }
}

/// The link's way into an [`Inbox`]: an order always goes in, a frame only while fewer than
/// [`QUEUE`] wait.
pub struct ToAudio {
    messages: std::sync::mpsc::Sender<Message>,
    frames: Arc<AtomicUsize>,
}

impl ToAudio {
    /// Fails once the audio thread is gone; a frame dropped for a full queue is no failure.
    pub fn send(&self, message: Message) -> Result<(), std::sync::mpsc::SendError<Message>> {
        if message.is_frame() && self.frames.fetch_add(1, Ordering::Relaxed) >= QUEUE {
            self.frames.fetch_sub(1, Ordering::Relaxed);
            return Ok(());
        }
        self.messages.send(message)
    }
}

/// The link's end of a [`Wire`].
pub struct LinkEnd {
    pub to_audio: ToAudio,
    pub from_audio: mpsc::Receiver<Message>,
}

pub fn wire() -> (Wire, LinkEnd) {
    let (messages, received) = std::sync::mpsc::channel();
    let frames = Arc::new(AtomicUsize::new(0));
    let (to_peer, from_audio) = mpsc::channel(QUEUE);
    (
        Wire {
            from_peer: Inbox {
                messages: received,
                frames: frames.clone(),
            },
            to_peer,
        },
        LinkEnd {
            to_audio: ToAudio { messages, frames },
            from_audio,
        },
    )
}

/// A lock that a panic elsewhere does not turn into a panic here: the buffers it guards stay
/// usable, and a panic in a PipeWire callback would cross its C code.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One of the machine's two defaults: its output, or its mic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    Sink,
    Source,
}

impl Device {
    fn saved_file(self) -> &'static str {
        match self {
            Device::Sink => "previous-default-sink",
            Device::Source => "previous-default-source",
        }
    }
}

/// A configured default, as PipeWire's `default` metadata holds it: kept as is, to be put
/// back as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultNode(pub String);

impl DefaultNode {
    pub fn named(node: &str) -> Self {
        Self(format!("{{ \"name\": \"{node}\" }}"))
    }

    /// The node it names. The value is JSON (`{ "name": "x" }`) or the relaxed SPA-JSON that
    /// PipeWire's own config writes (`{ name = x }`).
    pub fn name(&self) -> Option<&str> {
        let after_key = &self.0[self.0.find("name")? + "name".len()..];
        let after_key = after_key
            .strip_prefix('"')
            .unwrap_or(after_key)
            .trim_start();
        let value = after_key
            .strip_prefix(':')
            .or_else(|| after_key.strip_prefix('='))?
            .trim_start();
        let name = match value.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next()?,
            None => value
                .split(|c: char| c.is_whitespace() || c == ',' || c == '}')
                .next()?,
        };
        Some(name).filter(|name| !name.is_empty())
    }
}

/// The files where the remote keeps the defaults it replaced until they are put back, so
/// that an agent that died before putting them back finds them on its next start.
#[derive(Clone, Debug)]
pub struct Saved {
    dir: PathBuf,
}

impl Saved {
    pub fn in_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `$XDG_STATE_HOME/navette`, or `~/.local/state/navette`.
    pub fn default_dir() -> io::Result<PathBuf> {
        let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".local/state"))
                .ok_or_else(|| io::Error::other("neither XDG_STATE_HOME nor HOME is set"))?,
        };
        Ok(base.join("navette"))
    }

    /// What was saved: nothing, or the default then (`None` when there was none, an empty
    /// file).
    fn load(&self, device: Device) -> io::Result<Option<Option<DefaultNode>>> {
        match std::fs::read_to_string(self.dir.join(device.saved_file())) {
            Ok(value) if value.is_empty() => Ok(Some(None)),
            Ok(value) => Ok(Some(Some(DefaultNode(value)))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn save(&self, device: Device, previous: Option<&DefaultNode>) -> io::Result<()> {
        let value = previous.map_or("", |node| node.0.as_str());
        crate::store::write_private(&self.dir, device.saved_file(), value.as_bytes())
    }

    fn clear(&self, device: Device) -> io::Result<()> {
        match std::fs::remove_file(self.dir.join(device.saved_file())) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// What a sink does with the sound played into it: interleaved stereo at 48 kHz, in blocks of
/// the graph's size. It runs on PipeWire's thread, so it must not block.
pub type OnSound = Box<dyn FnMut(&[f32]) + Send>;

/// One machine's PipeWire graph, as Navette uses it. Its calls wait for PipeWire: they are
/// made from the audio threads, never from the async runtime. The nodes it opens are removed
/// when dropped.
pub trait Graph: Clone + Send + 'static {
    type Node: Send;

    /// The default sink or source the user configured, if any.
    fn default_node(&self, device: Device) -> io::Result<Option<DefaultNode>>;
    /// Configures the default sink or source; `None` clears it, leaving the choice to the
    /// session manager.
    fn set_default_node(&self, device: Device, node: Option<&DefaultNode>) -> io::Result<()>;
    /// A virtual sink: what the machine plays into it goes to `on_sound`.
    fn open_sink(&self, name: &str, description: &str, on_sound: OnSound)
    -> io::Result<Self::Node>;
    /// A playback stream, mixed like any application's, that plays what `jitter` holds.
    fn open_playback(
        &self,
        name: &str,
        description: &str,
        jitter: Arc<Mutex<Jitter>>,
    ) -> io::Result<Self::Node>;
    /// A virtual source, which the machine's apps record from as from a mic: it plays what
    /// `jitter` holds.
    fn open_source(
        &self,
        name: &str,
        description: &str,
        jitter: Arc<Mutex<Jitter>>,
    ) -> io::Result<Self::Node>;
    /// The mic the session manager gives the apps now: the configured default, or its own
    /// pick when there is none.
    fn mic_in_use(&self) -> io::Result<Option<String>>;
    /// The mic named `mic`, with the echo of what the speakers play cancelled: what it hears
    /// goes to `on_sound`. The echo canceller goes with the node.
    fn open_mic(
        &self,
        name: &str,
        description: &str,
        mic: &str,
        on_sound: OnSound,
    ) -> io::Result<Self::Node>;
    /// Whether `node` went with the PipeWire it was opened on (restarted, say).
    fn is_gone(&self, node: &Self::Node) -> bool;
}

/// A default switched to a node of ours. Dropping it puts the previous default back.
struct Switch<G: Graph> {
    graph: G,
    saved: Saved,
    device: Device,
    ours: DefaultNode,
    previous: Option<DefaultNode>,
}

impl<G: Graph> Switch<G> {
    /// Saves the default that `ours` is to replace, before anything changes.
    fn save(graph: G, saved: Saved, device: Device, ours: &str) -> io::Result<Self> {
        let previous = match graph.default_node(device)? {
            // Left on our own node by an agent that died: the user's choice is the one it saved.
            Some(current) if current.name() == Some(ours) => saved.load(device)?.flatten(),
            current => current,
        };
        // Saved before switching, so that a crash from here on still knows what to put back.
        saved.save(device, previous.as_ref())?;
        Ok(Self {
            graph,
            saved,
            device,
            ours: DefaultNode::named(ours),
            previous,
        })
    }

    /// Makes our node the default, again after PipeWire restarts; what goes back at the end
    /// is still the default from before.
    fn switch(&self) -> io::Result<()> {
        self.graph.set_default_node(self.device, Some(&self.ours))
    }
}

impl<G: Graph> Drop for Switch<G> {
    fn drop(&mut self) {
        // Kept saved when it fails, for the next start to put back.
        match self
            .graph
            .set_default_node(self.device, self.previous.as_ref())
        {
            Ok(()) => {
                if let Err(e) = self.saved.clear(self.device) {
                    eprintln!("navette: audio: could not remove the saved default: {e}");
                }
            }
            Err(e) => eprintln!("navette: audio: could not put the previous default back: {e}"),
        }
    }
}

/// Puts back the defaults an agent that died left on Navette's nodes, as soon as audio starts
/// again rather than at the next session; a default the user changed since stays.
pub(crate) fn repair<G: Graph>(graph: &G, saved: &Saved) {
    for (device, ours) in [(Device::Sink, SINK), (Device::Source, mic::SOURCE)] {
        if let Err(e) = repair_one(graph, saved, device, ours) {
            eprintln!("navette: audio: could not put back a default a previous run left: {e}");
        }
    }
}

pub(super) fn repair_one<G: Graph>(
    graph: &G,
    saved: &Saved,
    device: Device,
    ours: &str,
) -> io::Result<()> {
    let Some(previous) = saved.load(device)? else {
        return Ok(());
    };
    if graph
        .default_node(device)?
        .and_then(|c| c.name().map(str::to_owned))
        .as_deref()
        == Some(ours)
    {
        graph.set_default_node(device, previous.as_ref())?;
    }
    saved.clear(device)
}

/// The remote's sink while home asks for the sound. Dropping it puts the previous default
/// back, then removes the sink.
pub struct RemoteSink<G: Graph> {
    // Field order is drop order: the apps move straight to the previous default.
    switch: Switch<G>,
    node: G::Node,
}

impl<G: Graph> RemoteSink<G> {
    pub fn start(graph: G, saved: Saved, on_sound: OnSound) -> io::Result<Self> {
        let switch = Switch::save(graph, saved, Device::Sink, SINK)?;
        let node = switch.graph.open_sink(SINK, SINK_DESCRIPTION, on_sound)?;
        switch.switch()?;
        Ok(Self { switch, node })
    }

    fn is_gone(&self) -> bool {
        self.switch.graph.is_gone(&self.node)
    }

    /// The sink again, once PipeWire has restarted, and the default again.
    fn reopen(&mut self, on_sound: OnSound) -> io::Result<()> {
        self.node = self
            .switch
            .graph
            .open_sink(SINK, SINK_DESCRIPTION, on_sound)?;
        self.switch.switch()
    }
}

/// The block the packets are cut from: a few seconds of sound.
const ARENA: usize = 64 * 1024;

/// Encodes the sink's sound and sends it home, on PipeWire's thread: it only counts its
/// failures, for the audio thread to report.
fn sender(
    home: mpsc::Sender<Message>,
    session: u32,
    seq: Arc<AtomicU32>,
    failures: Arc<AtomicU64>,
) -> io::Result<OnSound> {
    encoding(home, session, seq, failures, |session, seq, packet| {
        Message::Frame {
            session,
            seq,
            packet,
        }
    })
}

/// Encodes what a node hears and sends each frame as `frame` makes it, on PipeWire's thread.
fn encoding(
    to: mpsc::Sender<Message>,
    session: u32,
    seq: Arc<AtomicU32>,
    failures: Arc<AtomicU64>,
    frame: fn(u32, u32, Bytes) -> Message,
) -> io::Result<OnSound> {
    let mut encoder = Encoder::new()?;
    // The packets are cut from one block, so a frame costs no allocation of its own.
    let mut arena = BytesMut::with_capacity(ARENA);
    Ok(Box::new(move |pcm| {
        let sent = encoder.push(pcm, |packet| {
            if arena.capacity() - arena.len() < MAX_PACKET {
                arena.reserve(ARENA);
            }
            arena.extend_from_slice(packet);
            let frame = frame(
                session,
                seq.fetch_add(1, Ordering::Relaxed),
                arena.split().freeze(),
            );
            // Late sound is worse than lost sound: a full link drops the frame, and the other
            // side fills the gap.
            let _ = to.try_send(frame);
        });
        if sent.is_err() {
            failures.fetch_add(1, Ordering::Relaxed);
        }
    }))
}

/// The sink while it runs, and the cycles it could not encode.
struct Capture<G: Graph> {
    sink: RemoteSink<G>,
    home: mpsc::Sender<Message>,
    session: u32,
    /// Shared with the sink's sender, and with the next one after a restart: home would take
    /// frames numbered from 0 again as late.
    seq: Arc<AtomicU32>,
    failures: Arc<AtomicU64>,
}

impl<G: Graph> Capture<G> {
    fn start(
        graph: G,
        saved: Saved,
        home: mpsc::Sender<Message>,
        session: u32,
    ) -> io::Result<Self> {
        let seq = Arc::new(AtomicU32::new(0));
        let failures = Arc::new(AtomicU64::new(0));
        let on_sound = sender(home.clone(), session, seq.clone(), failures.clone())?;
        Ok(Self {
            sink: RemoteSink::start(graph, saved, on_sound)?,
            home,
            session,
            seq,
            failures,
        })
    }

    /// Opens the sink again if PipeWire restarted under it.
    fn recover(&mut self) -> io::Result<()> {
        if !self.sink.is_gone() {
            return Ok(());
        }
        let on_sound = sender(
            self.home.clone(),
            self.session,
            self.seq.clone(),
            self.failures.clone(),
        )?;
        self.sink.reopen(on_sound)
    }
}

impl<G: Graph> Drop for Capture<G> {
    fn drop(&mut self) {
        let failures = self.failures.load(Ordering::Relaxed);
        if failures > 0 {
            eprintln!("navette: audio: {failures} cycles of sound could not be encoded");
        }
    }
}

/// The remote's side of one link, on a thread of its own: follows home's orders until the
/// link drops, then puts the previous default sink back. Joining the thread waits for that;
/// `after`, the previous link's thread, is joined before anything else.
pub fn remote<G: Graph>(
    graph: G,
    saved: Saved,
    wire: Wire,
    after: Option<JoinHandle<()>>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("navette-audio".into())
        .spawn(move || {
            // The previous link's thread first puts the default back, or the two would race.
            if let Some(previous) = after {
                let _ = previous.join();
            }
            repair(&graph, &saved);
            let mut sink = None;
            let mut voice: Option<mic::Voice<G>> = None;
            // Told once per outage, not at each check while PipeWire is still away.
            let mut lost = false;
            let mut voice_lost = false;
            loop {
                match wire.from_peer.recv_timeout(CHECK) {
                    Ok(Message::Start { session }) => {
                        // A new session replaces the one running, if any.
                        drop(sink.take());
                        match Capture::start(
                            graph.clone(),
                            saved.clone(),
                            wire.to_peer.clone(),
                            session,
                        ) {
                            Ok(started) => drop(sink.replace(started)),
                            Err(e) => eprintln!("navette: audio: could not start the sink: {e}"),
                        }
                    }
                    Ok(Message::Stop) => drop(sink.take()),
                    Ok(Message::MicStart { session }) => {
                        drop(voice.take());
                        match mic::Voice::start(graph.clone(), saved.clone(), session) {
                            Ok(started) => voice = Some(started),
                            Err(e) => eprintln!("navette: mic: could not start the source: {e}"),
                        }
                    }
                    Ok(Message::MicStop) => drop(voice.take()),
                    Ok(Message::MicFrame {
                        session,
                        seq,
                        packet,
                    }) => {
                        if let Some(voice) = &mut voice
                            && let Err(e) = voice.received(session, seq, &packet)
                        {
                            eprintln!("navette: mic: {e}");
                        }
                    }
                    Ok(Message::Frame { .. }) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if let Some(voice) = &mut voice {
                    match voice.recover() {
                        Ok(()) => voice_lost = false,
                        Err(e) if !voice_lost => {
                            eprintln!("navette: mic: could not open the source again: {e}");
                            voice_lost = true;
                        }
                        Err(_) => {}
                    }
                }
                if let Some(capture) = &mut sink {
                    match capture.recover() {
                        Ok(()) => lost = false,
                        Err(e) if !lost => {
                            eprintln!("navette: audio: could not open the sink again: {e}");
                            lost = true;
                        }
                        Err(_) => {}
                    }
                }
            }
        })
}

struct Playback<G: Graph> {
    _node: G::Node,
    session: u32,
    jitter: Arc<Mutex<Jitter>>,
    decoder: Decoder,
}

/// Home's side: asks the remote for its sound and plays it. Its calls wait for PipeWire and
/// the link: they are made from an audio thread, never from the async runtime.
pub struct Home<G: Graph> {
    graph: G,
    remote: mpsc::Sender<Message>,
    playback: Option<Playback<G>>,
    sessions: u32,
}

impl<G: Graph> Home<G> {
    pub fn new(graph: G, remote: mpsc::Sender<Message>) -> Self {
        Self {
            graph,
            remote,
            playback: None,
            sessions: 0,
        }
    }

    pub fn start(&mut self) -> io::Result<()> {
        if self.playback.is_some() {
            return Ok(());
        }
        let jitter = Arc::new(Mutex::new(Jitter::new(JITTER_TARGET, JITTER_MAX)));
        let node = self
            .graph
            .open_playback(PLAYBACK, PLAYBACK_DESCRIPTION, jitter.clone())?;
        self.sessions = self.sessions.wrapping_add(1);
        let session = self.sessions;
        self.playback = Some(Playback {
            _node: node,
            session,
            jitter,
            decoder: Decoder::new()?,
        });
        self.order(Message::Start { session })
    }

    pub fn stop(&mut self) -> io::Result<()> {
        self.playback = None;
        self.order(Message::Stop)
    }

    fn order(&self, message: Message) -> io::Result<()> {
        self.remote
            .blocking_send(message)
            .map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "the link is down"))
    }

    /// A message from the remote; its sound is ignored unless home is playing, and plays only
    /// when it belongs to the session home asked for last.
    pub fn received(&mut self, message: Message) -> io::Result<()> {
        if let Message::Frame {
            session,
            seq,
            packet,
        } = message
            && let Some(playback) = &mut self.playback
            // Sent before home stopped: its numbering is not the new session's.
            && session == playback.session
        {
            let pcm = playback.decoder.decode(seq, &packet)?;
            lock(&playback.jitter).push(&pcm);
        }
        Ok(())
    }
}

/// Home's side of one link, on a thread of its own: plays the remote's sound until the link
/// drops.
pub fn home<G: Graph>(graph: G, wire: Wire) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("navette-audio".into())
        .spawn(move || {
            let mut home = Home::new(graph, wire.to_peer);
            if let Err(e) = home.start() {
                eprintln!("navette: audio: could not start the playback: {e}");
            }
            while let Ok(message) = wire.from_peer.recv() {
                if let Err(e) = home.received(message) {
                    eprintln!("navette: audio: {e}");
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::fake::FakeGraph;
    use super::*;
    use crate::link;

    pub(super) const WAIT: Duration = Duration::from_secs(10);

    /// The audio wires of the two ends of a link in memory, and home's end of the link, to
    /// drop it.
    pub(super) async fn linked() -> (Wire, Wire, tokio::task::JoinHandle<io::Error>) {
        let (home_stream, remote_stream) = tokio::io::duplex(64 * 1024);
        let heartbeat = link::Heartbeat::default();
        let end = |stream| {
            let (taken, wire) = tokio::sync::oneshot::channel();
            let link = tokio::spawn(link::run(stream, heartbeat, move |mut wire: link::Wire| {
                let _ = taken.send(wire.audio.take().unwrap());
                // The clipboard's queues stay open, unused.
                async move { std::future::pending::<()>().await }
            }));
            (link, wire)
        };
        let (home_end, home) = end(home_stream);
        let (_remote_end, remote) = end(remote_stream);
        (home.await.unwrap(), remote.await.unwrap(), home_end)
    }

    pub(super) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        tokio::time::timeout(WAIT, tokio::task::spawn_blocking(f))
            .await
            .expect("in time")
            .unwrap()
    }

    /// Polls `check` on the fake graph until it holds.
    pub(super) async fn until(graph: &FakeGraph, check: impl Fn(&fake::State) -> bool) {
        tokio::time::timeout(WAIT, async {
            while !check(&graph.state()) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the graph reached the expected state in time");
    }

    pub(super) fn tone(frames: usize) -> Vec<f32> {
        (0..frames * codec::FRAME)
            .flat_map(|i| {
                let v = 0.5 * (i as f32 * 440.0 * std::f32::consts::TAU / codec::RATE as f32).sin();
                [v, v]
            })
            .collect()
    }

    #[tokio::test]
    async fn the_audio_messages_cross_the_link() {
        let (home, remote, _link) = linked().await;
        let messages = [
            Message::Start { session: 7 },
            Message::Frame {
                session: 7,
                seq: 0x0102_0304,
                packet: Bytes::from_static(&[9, 8, 7]),
            },
            Message::Stop,
            Message::MicStart { session: 8 },
            Message::MicFrame {
                session: 8,
                seq: 0x0506_0708,
                packet: Bytes::from_static(&[6, 5]),
            },
            Message::MicStop,
        ];
        for message in &messages {
            home.to_peer.send(message.clone()).await.unwrap();
        }
        let received = blocking(move || {
            (0..6)
                .map(|_| remote.from_peer.recv_timeout(WAIT).unwrap())
                .collect::<Vec<_>>()
        })
        .await;
        assert_eq!(received, messages);
    }

    /// A saved-default file of its own, in a temporary directory.
    pub(super) fn saved() -> (tempfile::TempDir, Saved) {
        let tmp = tempfile::tempdir().unwrap();
        let saved = Saved::in_dir(tmp.path().join("state"));
        (tmp, saved)
    }

    fn start(graph: &FakeGraph, saved: &Saved) -> RemoteSink<FakeGraph> {
        RemoteSink::start(graph.clone(), saved.clone(), Box::new(|_| {})).unwrap()
    }

    #[test]
    fn a_default_sink_gives_its_name_in_json_or_spa_json() {
        for value in [
            r#"{ "name": "invented-speakers" }"#,
            r#"{"name":"invented-speakers"}"#,
            r#"{ name = "invented-speakers" }"#,
            r#"{ name = invented-speakers }"#,
        ] {
            assert_eq!(
                DefaultNode(value.to_owned()).name(),
                Some("invented-speakers"),
                "{value}"
            );
        }
        assert_eq!(DefaultNode("{}".to_owned()).name(), None);
    }

    #[test]
    fn start_sets_the_default_sink_and_stop_restores_it() {
        let graph = FakeGraph::with_default("speakers");
        let (_tmp, saved) = saved();
        let sink = start(&graph, &saved);
        assert_eq!(graph.state().default, Some(DefaultNode::named(SINK)));
        drop(sink);
        let state = graph.state();
        assert_eq!(state.default, Some(DefaultNode::named("speakers")));
        // Put back before the sink goes, so the apps move straight to the previous one.
        assert_eq!(
            state.log,
            [
                format!("sink {SINK}"),
                format!("default {}", DefaultNode::named(SINK).0),
                format!("default {}", DefaultNode::named("speakers").0),
                format!("remove {SINK}"),
            ]
        );
    }

    #[test]
    fn with_no_default_before_stop_clears_it() {
        let graph = FakeGraph::default();
        let (_tmp, saved) = saved();
        drop(start(&graph, &saved));
        assert_eq!(graph.state().default, None);
    }

    #[test]
    fn a_default_left_on_the_navette_sink_with_nothing_saved_is_cleared() {
        // Written by the session manager in its own form, not ours.
        let graph = FakeGraph::default();
        graph.state().default = Some(DefaultNode(format!(r#"{{"name":"{SINK}"}}"#)));
        let (_tmp, saved) = saved();
        drop(start(&graph, &saved));
        assert_eq!(graph.state().default, None);
    }

    #[test]
    fn the_previous_default_is_saved_until_it_is_put_back() {
        use std::os::unix::fs::PermissionsExt;

        let graph = FakeGraph::with_default("speakers");
        let (_tmp, saved) = saved();
        let sink = start(&graph, &saved);
        let file = saved.dir.join(Device::Sink.saved_file());
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            saved.load(Device::Sink).unwrap(),
            Some(Some(DefaultNode::named("speakers")))
        );
        drop(sink);
        assert!(!file.exists());
    }

    #[test]
    fn after_a_crash_the_saved_default_is_put_back() {
        let graph = FakeGraph::with_default("speakers");
        let (_tmp, saved) = saved();
        // An agent that died while switched: the default is still ours.
        std::mem::forget(start(&graph, &saved));
        assert_eq!(graph.state().default, Some(DefaultNode::named(SINK)));

        drop(start(&graph, &saved));
        assert_eq!(graph.state().default, Some(DefaultNode::named("speakers")));
        assert_eq!(saved.load(Device::Sink).unwrap(), None);
    }

    #[test]
    fn a_default_that_could_not_be_put_back_stays_saved() {
        let graph = FakeGraph::with_default("speakers");
        let (_tmp, saved) = saved();
        let sink = start(&graph, &saved);
        graph.state().refuse_defaults = true;
        drop(sink);
        assert_eq!(
            saved.load(Device::Sink).unwrap(),
            Some(Some(DefaultNode::named("speakers")))
        );
    }

    #[tokio::test]
    async fn the_remote_sound_plays_at_home() {
        let home_graph = FakeGraph::with_default("home-speakers");
        let remote_graph = FakeGraph::with_default("laptop-speakers");
        let (_tmp, saved) = saved();
        let (home_peer, remote_peer, _link) = linked().await;
        remote(remote_graph.clone(), saved.clone(), remote_peer, None).unwrap();
        home(home_graph.clone(), home_peer).unwrap();

        until(&remote_graph, |s| !s.sinks.is_empty()).await;
        assert_eq!(remote_graph.state().default, Some(DefaultNode::named(SINK)));
        // Home's own default output is never touched.
        assert_eq!(
            home_graph.state().default,
            Some(DefaultNode::named("home-speakers"))
        );

        let sink = remote_graph.state().sinks[0].1.clone();
        // A graph quantum's worth at a time, the way PipeWire hands it over.
        for block in tone(20).chunks(1024 * codec::CHANNELS) {
            (sink.lock().unwrap())(block);
        }
        until(&home_graph, |s| !s.playbacks.is_empty()).await;
        let jitter = home_graph.state().playbacks[0].1.clone();
        let heard = tokio::time::timeout(WAIT, async {
            loop {
                let mut out = vec![0.0; 10 * FRAME_SAMPLES];
                jitter.lock().unwrap().pull(&mut out);
                if out.iter().any(|s| s.abs() > 0.1) {
                    return out;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(heard.is_ok(), "the tone never reached home's playback");
    }

    #[tokio::test]
    async fn stopping_at_home_restores_the_remote_default() {
        let home_graph = FakeGraph::default();
        let remote_graph = FakeGraph::with_default("laptop-speakers");
        let (_tmp, saved) = saved();
        let (home_peer, remote_peer, _link) = linked().await;
        remote(remote_graph.clone(), saved.clone(), remote_peer, None).unwrap();
        let mut home = Home::new(home_graph.clone(), home_peer.to_peer);
        // Home's calls block: off the runtime, as in `home`.
        let mut home = blocking(move || {
            home.start().unwrap();
            home
        })
        .await;
        until(&remote_graph, |s| !s.sinks.is_empty()).await;

        let mut home = blocking(move || {
            home.stop().unwrap();
            home
        })
        .await;
        until(&remote_graph, |s| s.sinks.is_empty()).await;
        assert_eq!(
            remote_graph.state().default,
            Some(DefaultNode::named("laptop-speakers"))
        );
        assert!(home_graph.state().playbacks.is_empty());

        // And again: a mode can start and stop several times on one link.
        blocking(move || home.start().unwrap()).await;
        until(&remote_graph, |s| !s.sinks.is_empty()).await;
    }

    #[tokio::test]
    async fn the_link_dropping_restores_the_remote_default() {
        let remote_graph = FakeGraph::with_default("laptop-speakers");
        let (_tmp, saved) = saved();
        let (home_peer, remote_peer, link) = linked().await;
        let audio = remote(remote_graph.clone(), saved.clone(), remote_peer, None).unwrap();
        home_peer
            .to_peer
            .send(Message::Start { session: 1 })
            .await
            .unwrap();
        until(&remote_graph, |s| !s.sinks.is_empty()).await;

        link.abort();
        // The thread ends once it has put the default back.
        blocking(move || audio.join().unwrap()).await;
        assert!(remote_graph.state().sinks.is_empty());
        assert_eq!(
            remote_graph.state().default,
            Some(DefaultNode::named("laptop-speakers"))
        );
    }

    #[test]
    fn late_frames_of_a_stopped_session_do_not_hold_up_the_next_one() {
        let graph = FakeGraph::default();
        let (to_remote, mut orders) = mpsc::channel(8);
        let mut home = Home::new(graph.clone(), to_remote);
        let packets = codec::tests::packets(&mut Encoder::new().unwrap(), &tone(20));
        let session = |orders: &mut mpsc::Receiver<Message>| match orders.try_recv().unwrap() {
            Message::Start { session } => session,
            other => panic!("{other:?}"),
        };
        let frame = |session, seq: u32| Message::Frame {
            session,
            seq,
            packet: Bytes::from(packets[seq as usize % packets.len()].clone()),
        };

        home.start().unwrap();
        let first = session(&mut orders);
        for seq in 100..105 {
            home.received(frame(first, seq)).unwrap();
        }
        home.stop().unwrap();
        orders.try_recv().unwrap();
        home.start().unwrap();
        let second = session(&mut orders);
        assert_ne!(first, second);
        // Still on its way when home stopped.
        home.received(frame(first, 105)).unwrap();
        for seq in 0..10 {
            home.received(frame(second, seq)).unwrap();
        }
        let jitter = graph.state().playbacks[0].1.clone();
        let mut out = vec![0.0; 10 * FRAME_SAMPLES];
        jitter.lock().unwrap().pull(&mut out);
        assert!(
            out.iter().any(|s| s.abs() > 0.1),
            "the new session was not played"
        );
    }

    #[tokio::test]
    async fn the_sink_comes_back_after_pipewire_restarts() {
        let graph = FakeGraph::with_default("laptop-speakers");
        let (_tmp, saved) = saved();
        let (wire, link) = super::wire();
        let audio = remote(graph.clone(), saved.clone(), wire, None).unwrap();
        link.to_audio.send(Message::Start { session: 1 }).unwrap();
        until(&graph, |s| !s.sinks.is_empty()).await;

        // PipeWire restarts: the sink goes, and the session manager falls back to another
        // output.
        graph.restart();
        graph.state().default = Some(DefaultNode::named("hdmi-out"));
        until(&graph, |s| {
            !s.sinks.is_empty() && s.default == Some(DefaultNode::named(SINK))
        })
        .await;
        assert_eq!(
            saved.load(Device::Sink).unwrap(),
            Some(Some(DefaultNode::named("laptop-speakers")))
        );

        // The user's choice from before the session comes back, not the fallback.
        drop(link);
        blocking(move || audio.join().unwrap()).await;
        assert_eq!(
            graph.state().default,
            Some(DefaultNode::named("laptop-speakers"))
        );
        assert_eq!(saved.load(Device::Sink).unwrap(), None);
    }

    #[test]
    fn the_frames_keep_their_numbers_after_pipewire_restarts() {
        let graph = FakeGraph::with_default("laptop-speakers");
        let (_tmp, saved) = saved();
        let (to_home, mut frames) = mpsc::channel(QUEUE);
        let mut capture = Capture::start(graph.clone(), saved, to_home, 1).unwrap();
        let play = |graph: &FakeGraph| {
            let sink = graph.state().sinks[0].1.clone();
            (sink.lock().unwrap())(&tone(2));
        };
        play(&graph);
        graph.restart();
        capture.recover().unwrap();
        play(&graph);

        let seqs: Vec<u32> = std::iter::from_fn(|| frames.try_recv().ok())
            .map(|message| match message {
                Message::Frame { seq, .. } => seq,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(seqs, (0..4).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn a_new_link_waits_for_the_previous_one_to_put_the_default_back() {
        let remote_graph = FakeGraph::with_default("laptop-speakers");
        let (_tmp, saved) = saved();
        let (home_peer, remote_peer, link) = linked().await;
        let first = remote(remote_graph.clone(), saved.clone(), remote_peer, None).unwrap();
        home_peer
            .to_peer
            .send(Message::Start { session: 1 })
            .await
            .unwrap();
        until(&remote_graph, |s| !s.sinks.is_empty()).await;

        // The link drops and comes back at once, while the first thread is still putting the
        // default back.
        remote_graph.state().slow_defaults = Duration::from_millis(200);
        link.abort();
        let (home_peer, remote_peer, link) = linked().await;
        let second = remote(
            remote_graph.clone(),
            saved.clone(),
            remote_peer,
            Some(first),
        )
        .unwrap();
        home_peer
            .to_peer
            .send(Message::Start { session: 1 })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        until(&remote_graph, |s| {
            s.default == Some(DefaultNode::named(SINK))
        })
        .await;
        // Long enough for a late put-back of the first link to land, if it were still coming.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(remote_graph.state().default, Some(DefaultNode::named(SINK)));
        // And the second link's save is not wiped by the first one's clean-up.
        assert_eq!(
            saved.load(Device::Sink).unwrap(),
            Some(Some(DefaultNode::named("laptop-speakers")))
        );

        link.abort();
        blocking(move || second.join().unwrap()).await;
        assert_eq!(
            remote_graph.state().default,
            Some(DefaultNode::named("laptop-speakers"))
        );
    }
}
