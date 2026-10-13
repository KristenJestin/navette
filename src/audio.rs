//! The remote machine's sound, heard at home mixed with home sound.
//!
//! On the remote machine, a virtual sink named for Navette becomes the default output while
//! home asks for the sound; what the apps play into it is Opus-encoded and sent home, and the
//! previous default comes back when home stops, when the link drops or when the agent ends. At
//! home, a playback stream that PipeWire mixes like any other application plays it, through a
//! small jitter buffer. The remote's volume and mute act once, there: the remote's sink sends
//! its sound at full scale and tells home its level when it changes.
//!
//! PipeWire is reached through [`Graph`]; tests drive a fake one.
//!
//! The audio messages are top-level variants of [`LinkMsg`](crate::wire::LinkMsg):
//! - `AudioStart` (home → remote), with a session number: make the sink the default output and
//!   send its sound under that number.
//! - `AudioStop` (home → remote): put the previous default back and stop sending.
//! - `AudioFrame` (remote → home), with the session and a sequence number: one Opus packet
//!   follows it.
//! - `AudioVolume` (remote → home), with the session, the linear gain of each channel and the
//!   mute: sent when the sink's volume or mute changes, applied to the playback.
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
use std::time::{Duration, Instant};

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

/// The jitter buffers keep 10 ms beyond what the playback takes in a cycle, more when the
/// link falls behind, up to 100 ms.
const JITTER_FLOOR: usize = FRAME_SAMPLES;
const JITTER_CEILING: usize = 10 * FRAME_SAMPLES;

/// How often the remote checks that its sink is still there: PipeWire may restart under it.
const CHECK: Duration = Duration::from_millis(250);

/// How often the remote looks for a change of its sink's volume while the sink runs: a held
/// volume key moves it many times a second, and home follows within this.
const LEVEL_POLL: Duration = Duration::from_millis(50);

/// How often each side tells how the remote's sound fares, beside the video's stats: which
/// kind of event a crackle was.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// Interleaved samples in a millisecond.
const MS: usize = FRAME_SAMPLES / 10;

/// What a mixer says of a stream: a linear gain for each channel (what the sound is multiplied
/// by, not the cubic scale of a volume slider) and whether it is muted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub left: f32,
    pub right: f32,
    pub muted: bool,
}

impl Level {
    /// Untouched sound: what a stream starts with.
    pub const UNITY: Self = Self {
        left: 1.0,
        right: 1.0,
        muted: false,
    };
}

/// The audio messages; on the link, they are top-level variants of
/// [`LinkMsg`](crate::wire::LinkMsg).
#[derive(Debug, Clone, PartialEq)]
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
    /// Remote → home: the sink's volume and mute as the remote's mixer has them, said when they
    /// change; home applies them to its playback, which is the only place they act.
    Volume {
        session: u32,
        level: Level,
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
    /// Frames dropped for a full queue.
    dropped: Arc<AtomicU64>,
}

impl Inbox {
    /// Frames dropped so far because the audio thread had [`QUEUE`] waiting.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

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
    dropped: Arc<AtomicU64>,
}

impl ToAudio {
    /// Fails once the audio thread is gone; a frame dropped for a full queue is no failure.
    pub fn send(&self, message: Message) -> Result<(), std::sync::mpsc::SendError<Message>> {
        if message.is_frame() && self.frames.fetch_add(1, Ordering::Relaxed) >= QUEUE {
            self.frames.fetch_sub(1, Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
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
    let dropped = Arc::new(AtomicU64::new(0));
    let (to_peer, from_audio) = mpsc::channel(QUEUE);
    (
        Wire {
            from_peer: Inbox {
                messages: received,
                frames: frames.clone(),
                dropped: dropped.clone(),
            },
            to_peer,
        },
        LinkEnd {
            to_audio: ToAudio {
                messages,
                frames,
                dropped,
            },
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

/// Told each time a sink's mixer sets its volume or mute, with the level then. It runs on
/// PipeWire's loop, so it must not block, and it may be told the same level again.
pub type OnLevel = Box<dyn FnMut(Level) + Send>;

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
    /// A virtual sink: what the machine plays into it goes to `on_sound` at full scale, whatever
    /// its volume and mute, which stay on the sink for the machine's mixer to read and change
    /// and are told to `on_level`.
    fn open_sink(
        &self,
        name: &str,
        description: &str,
        on_sound: OnSound,
        on_level: OnLevel,
    ) -> io::Result<Self::Node>;
    /// Sets the volume and mute of a playback stream of ours, as an application's mixer would.
    fn set_level(&self, node: &Self::Node, level: Level) -> io::Result<()>;
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
    pub fn start(graph: G, saved: Saved, on_sound: OnSound, on_level: OnLevel) -> io::Result<Self> {
        let switch = Switch::save(graph, saved, Device::Sink, SINK)?;
        let node = switch
            .graph
            .open_sink(SINK, SINK_DESCRIPTION, on_sound, on_level)?;
        switch.switch()?;
        Ok(Self { switch, node })
    }

    fn is_gone(&self) -> bool {
        self.switch.graph.is_gone(&self.node)
    }

    /// The sink again, once PipeWire has restarted, and the default again.
    fn reopen(&mut self, on_sound: OnSound, on_level: OnLevel) -> io::Result<()> {
        self.node = self
            .switch
            .graph
            .open_sink(SINK, SINK_DESCRIPTION, on_sound, on_level)?;
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
    dropped: Arc<AtomicU64>,
) -> io::Result<OnSound> {
    encoding(
        home,
        session,
        seq,
        failures,
        dropped,
        |session, seq, packet| Message::Frame {
            session,
            seq,
            packet,
        },
    )
}

/// Encodes what a node hears and sends each frame as `frame` makes it, on PipeWire's thread;
/// the frames the link had no room for are counted in `dropped`.
fn encoding(
    to: mpsc::Sender<Message>,
    session: u32,
    seq: Arc<AtomicU32>,
    failures: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
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
            if to.try_send(frame).is_err() {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        });
        if sent.is_err() {
            failures.fetch_add(1, Ordering::Relaxed);
        }
    }))
}

/// A sink's level as its mixer last set it: written on PipeWire's loop, read by the audio
/// thread, which tells home when it differs from what home was told.
#[derive(Clone, Default)]
struct Latest(Arc<Mutex<Option<Level>>>);

impl Latest {
    fn writer(&self) -> OnLevel {
        let latest = self.0.clone();
        Box::new(move |level| *lock(&latest) = Some(level))
    }

    fn get(&self) -> Option<Level> {
        *lock(&self.0)
    }
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
    /// Frames the link had no room for, and how many of them were told already.
    dropped: Arc<AtomicU64>,
    told: u64,
    /// The sink's level as its mixer last set it, written on PipeWire's loop; and the one home
    /// was last told.
    level: Latest,
    told_level: Option<Level>,
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
        let dropped = Arc::new(AtomicU64::new(0));
        let on_sound = sender(
            home.clone(),
            session,
            seq.clone(),
            failures.clone(),
            dropped.clone(),
        )?;
        let level = Latest::default();
        Ok(Self {
            sink: RemoteSink::start(graph, saved, on_sound, level.writer())?,
            home,
            session,
            seq,
            failures,
            dropped,
            told: 0,
            level,
            told_level: None,
        })
    }

    /// The frames dropped since the last report, if any: the remote's part of the periodic
    /// stats, said only when there is something to say.
    fn report(&mut self) -> Option<String> {
        let dropped = self.dropped.load(Ordering::Relaxed);
        let new = dropped - self.told;
        self.told = dropped;
        (new > 0).then(|| format!("{new} frames dropped, the link had no room"))
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
            self.dropped.clone(),
        )?;
        self.sink.reopen(on_sound, self.level.writer())
    }

    /// Tells home the sink's level if it changed since it was last told. Told again at the next
    /// call when the link has no room, and not before: an order is never dropped.
    fn tell_level(&mut self) {
        let Some(level) = self
            .level
            .get()
            .filter(|level| self.told_level != Some(*level))
        else {
            return;
        };
        let volume = Message::Volume {
            session: self.session,
            level,
        };
        if self.home.try_send(volume).is_ok() {
            self.told_level = Some(level);
        }
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
            let mut due = Instant::now() + REPORT_EVERY;
            let mut next_check = Instant::now() + CHECK;
            loop {
                let wait = if sink.is_some() { LEVEL_POLL } else { CHECK };
                match wire.from_peer.recv_timeout(wait) {
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
                        let started = match voice.take() {
                            // No flip of the default away and back.
                            Some(mut running) => running.restart(session).map(|()| running),
                            None => mic::Voice::start(graph.clone(), saved.clone(), session),
                        };
                        match started {
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
                    // The volume only goes the other way.
                    Ok(Message::Frame { .. } | Message::Volume { .. })
                    | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if let Some(capture) = &mut sink {
                    capture.tell_level();
                }
                if Instant::now() < next_check {
                    continue;
                }
                next_check = Instant::now() + CHECK;
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
                    if Instant::now() >= due {
                        due = Instant::now() + REPORT_EVERY;
                        if let Some(report) = capture.report() {
                            eprintln!("navette: audio: {report}");
                        }
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
    undecoded: Undecoded,
}

/// The packets of a session that could not be decoded: the first is told, the rest counted and
/// told once the session ends, as a broken stream would otherwise fill the log a line a frame.
struct Undecoded(u64, &'static str);

impl Undecoded {
    fn new(what: &'static str) -> Self {
        Self(0, what)
    }

    fn failed(&mut self, e: io::Error) -> io::Result<()> {
        self.0 += 1;
        if self.0 == 1 { Err(e) } else { Ok(()) }
    }
}

impl Drop for Undecoded {
    fn drop(&mut self) {
        if self.0 > 1 {
            eprintln!(
                "navette: {}: {} packets could not be decoded",
                self.1, self.0
            );
        }
    }
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
        let jitter = Arc::new(Mutex::new(Jitter::new(JITTER_FLOOR, JITTER_CEILING)));
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
            undecoded: Undecoded::new("audio"),
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

    /// How the remote's sound fares, while it plays: what is buffered, and each kind of event
    /// that is heard as a crackle, counted since it started, `dropped` with them.
    pub fn stats(&self, dropped: u64) -> Option<String> {
        let playback = self.playback.as_ref()?;
        let jitter = lock(&playback.jitter);
        let counts = jitter.counts();
        let gaps = playback.decoder.gaps();
        Some(format!(
            "buffer {} ms, target {} ms, rate {:+.3} %; underruns {}, trims {}, concealed {}, \
             lost {}, late {}, dropped {dropped}",
            jitter.buffered() / MS,
            jitter.target() / MS,
            (jitter.rate() - 1.0) * 100.0,
            counts.underruns,
            counts.trims,
            gaps.concealed,
            gaps.lost,
            gaps.late,
        ))
    }

    /// A message from the remote; its sound is ignored unless home is playing, and plays only
    /// when it belongs to the session home asked for last.
    pub fn received(&mut self, message: Message) -> io::Result<()> {
        if let Message::Volume { session, level } = message
            && let Some(playback) = &self.playback
            && session == playback.session
        {
            return self.graph.set_level(&playback._node, level);
        }
        if let Message::Frame {
            session,
            seq,
            packet,
        } = message
            && let Some(playback) = &mut self.playback
            // Sent before home stopped: its numbering is not the new session's.
            && session == playback.session
        {
            match playback.decoder.decode(seq, &packet) {
                Ok(pcm) => lock(&playback.jitter).push(&pcm),
                Err(e) => return playback.undecoded.failed(e),
            }
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
            let mut due = Instant::now() + REPORT_EVERY;
            loop {
                match wire
                    .from_peer
                    .recv_timeout(due.saturating_duration_since(Instant::now()))
                {
                    Ok(message) => {
                        if let Err(e) = home.received(message) {
                            eprintln!("navette: audio: {e}");
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if Instant::now() >= due {
                    due = Instant::now() + REPORT_EVERY;
                    if let Some(stats) = home.stats(wire.from_peer.dropped()) {
                        eprintln!("navette: audio: {stats}");
                    }
                }
            }
            if let Some(stats) = home.stats(wire.from_peer.dropped()) {
                eprintln!("navette: audio: at the end, {stats}");
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
            Message::Volume {
                session: 7,
                level: Level {
                    left: 0.125,
                    right: 0.5,
                    muted: true,
                },
            },
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
            (0..7)
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
        RemoteSink::start(
            graph.clone(),
            saved.clone(),
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .unwrap()
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

    const QUIET: Level = Level {
        left: 0.125,
        right: 0.125,
        muted: false,
    };

    /// The remote and home of a link in memory, both playing: the remote's sink and home's
    /// playback are open.
    async fn playing() -> (
        FakeGraph,
        FakeGraph,
        tempfile::TempDir,
        tokio::task::JoinHandle<io::Error>,
    ) {
        let home_graph = FakeGraph::with_default("home-speakers");
        let remote_graph = FakeGraph::with_default("laptop-speakers");
        let (tmp, saved) = saved();
        let (home_peer, remote_peer, link) = linked().await;
        remote(remote_graph.clone(), saved, remote_peer, None).unwrap();
        home(home_graph.clone(), home_peer).unwrap();
        until(&remote_graph, |s| !s.sinks.is_empty()).await;
        until(&home_graph, |s| !s.playbacks.is_empty()).await;
        (home_graph, remote_graph, tmp, link)
    }

    #[tokio::test]
    async fn the_remote_volume_and_mute_reach_the_home_stream_once() {
        let (home_graph, remote_graph, _tmp, _link) = playing().await;

        remote_graph.mixer_sets(SINK, QUIET);
        // PipeWire tells the same level again for every control it changes.
        remote_graph.mixer_sets(SINK, QUIET);
        until(&home_graph, |s| !s.levels.is_empty()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(home_graph.state().levels, [(PLAYBACK.to_owned(), QUIET)]);

        let muted = Level {
            muted: true,
            ..QUIET
        };
        remote_graph.mixer_sets(SINK, muted);
        until(&home_graph, |s| s.levels.len() == 2).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            home_graph.state().levels,
            [(PLAYBACK.to_owned(), QUIET), (PLAYBACK.to_owned(), muted)]
        );
    }

    #[tokio::test]
    async fn the_sound_sent_is_not_scaled_by_the_remote_volume() {
        let (home_graph, remote_graph, _tmp, _link) = playing().await;
        remote_graph.mixer_sets(SINK, QUIET);

        // The sink hands over the sound at full scale, whatever its volume.
        let sink = remote_graph.state().sinks[0].1.clone();
        for block in tone(20).chunks(1024 * codec::CHANNELS) {
            (sink.lock().unwrap())(block);
        }
        let jitter = home_graph.state().playbacks[0].1.clone();
        let peak = tokio::time::timeout(WAIT, async {
            loop {
                let mut out = vec![0.0; 10 * FRAME_SAMPLES];
                jitter.lock().unwrap().pull(&mut out);
                let peak = out.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
                if peak > 0.1 {
                    return peak;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the tone reached home's playback");
        // The tone is 0.5 at full scale; Opus keeps it within a few percent.
        assert!((0.45..0.55).contains(&peak), "peak {peak}");
    }

    fn capture_with_room(
        graph: &FakeGraph,
        room: usize,
    ) -> (
        Capture<FakeGraph>,
        mpsc::Receiver<Message>,
        tempfile::TempDir,
    ) {
        let (tmp, saved) = saved();
        let (to_home, orders) = mpsc::channel(room);
        let capture = Capture::start(graph.clone(), saved, to_home, 7).unwrap();
        (capture, orders, tmp)
    }

    fn volumes(orders: &mut mpsc::Receiver<Message>) -> Vec<Message> {
        std::iter::from_fn(|| orders.try_recv().ok()).collect()
    }

    #[test]
    fn a_level_is_told_when_it_changes_and_not_when_it_is_repeated() {
        let graph = FakeGraph::with_default("laptop-speakers");
        let (mut capture, mut orders, _tmp) = capture_with_room(&graph, QUEUE);

        capture.tell_level();
        assert_eq!(
            volumes(&mut orders),
            [],
            "nothing to tell before the mixer spoke"
        );

        graph.mixer_sets(SINK, QUIET);
        capture.tell_level();
        capture.tell_level();
        graph.mixer_sets(SINK, QUIET);
        capture.tell_level();
        assert_eq!(
            volumes(&mut orders),
            [Message::Volume {
                session: 7,
                level: QUIET
            }]
        );

        // Back to what it was before: that is a change too.
        graph.mixer_sets(SINK, Level::UNITY);
        capture.tell_level();
        assert_eq!(
            volumes(&mut orders),
            [Message::Volume {
                session: 7,
                level: Level::UNITY
            }]
        );
    }

    #[test]
    fn a_level_that_found_the_link_full_is_told_when_there_is_room() {
        let graph = FakeGraph::with_default("laptop-speakers");
        let (mut capture, mut orders, _tmp) = capture_with_room(&graph, 1);
        // The link is busy with a frame.
        capture
            .home
            .try_send(Message::Frame {
                session: 7,
                seq: 0,
                packet: Bytes::new(),
            })
            .unwrap();

        graph.mixer_sets(SINK, QUIET);
        capture.tell_level();
        assert!(matches!(
            volumes(&mut orders).as_slice(),
            [Message::Frame { .. }]
        ));

        capture.tell_level();
        assert_eq!(
            volumes(&mut orders),
            [Message::Volume {
                session: 7,
                level: QUIET
            }]
        );
        // Told once it got through.
        capture.tell_level();
        assert_eq!(volumes(&mut orders), []);
    }

    #[test]
    fn a_new_session_tells_its_level_again() {
        let graph = FakeGraph::with_default("laptop-speakers");
        let (mut first, mut orders, _tmp) = capture_with_room(&graph, QUEUE);
        graph.mixer_sets(SINK, QUIET);
        first.tell_level();
        assert_eq!(volumes(&mut orders).len(), 1);
        drop(first);

        let (mut second, mut orders, _tmp) = capture_with_room(&graph, QUEUE);
        graph.mixer_sets(SINK, QUIET);
        second.tell_level();
        assert_eq!(
            volumes(&mut orders),
            [Message::Volume {
                session: 7,
                level: QUIET
            }]
        );
    }

    #[test]
    fn the_sink_that_came_back_after_pipewire_restarts_tells_its_level() {
        let graph = FakeGraph::with_default("laptop-speakers");
        let (mut capture, mut orders, _tmp) = capture_with_room(&graph, QUEUE);
        graph.restart();
        capture.recover().unwrap();
        graph.mixer_sets(SINK, QUIET);
        capture.tell_level();
        assert_eq!(volumes(&mut orders).len(), 1);
    }

    #[test]
    fn a_level_applies_to_the_playback_of_its_own_session_only() {
        let graph = FakeGraph::default();
        let (to_remote, mut orders) = mpsc::channel(8);
        let mut home = Home::new(graph.clone(), to_remote);
        let volume = |session| Message::Volume {
            session,
            level: QUIET,
        };

        // Nothing plays yet.
        home.received(volume(1)).unwrap();
        assert!(graph.state().levels.is_empty());

        home.start().unwrap();
        let Message::Start { session } = orders.try_recv().unwrap() else {
            panic!("home asked for the sound");
        };
        // Said by the remote before home stopped and started again.
        home.received(volume(session.wrapping_add(1))).unwrap();
        assert!(graph.state().levels.is_empty());

        home.received(volume(session)).unwrap();
        assert_eq!(graph.state().levels, [(PLAYBACK.to_owned(), QUIET)]);
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

    /// No Opus packet: its header asks for more frames than a packet holds.
    pub(super) const BROKEN: [u8; 3] = [0xff, 0xff, 0xff];

    #[test]
    fn a_run_of_broken_packets_is_told_once() {
        let (to_remote, _orders) = mpsc::channel(8);
        let mut home = Home::new(FakeGraph::default(), to_remote);
        home.start().unwrap();
        let frame = |seq| Message::Frame {
            session: 1,
            seq,
            packet: Bytes::from_static(&BROKEN),
        };
        assert!(home.received(frame(0)).is_err());
        assert!(home.received(frame(1)).is_ok());
        assert!(home.received(frame(2)).is_ok());
        assert_eq!(home.playback.as_ref().unwrap().undecoded.0, 3);
    }

    #[test]
    fn a_frame_dropped_in_a_full_queue_is_counted() {
        let (wire, link) = super::wire();
        let frame = |seq| Message::Frame {
            session: 1,
            seq,
            packet: Bytes::new(),
        };
        for seq in 0..QUEUE as u32 + 3 {
            link.to_audio.send(frame(seq)).unwrap();
        }
        assert_eq!(wire.from_peer.dropped(), 3);
        // Orders are never dropped, nor counted.
        link.to_audio.send(Message::Stop).unwrap();
        assert_eq!(wire.from_peer.dropped(), 3);
    }

    #[test]
    fn a_frame_the_link_cannot_take_is_counted() {
        let graph = FakeGraph::with_default("laptop-speakers");
        let (_tmp, saved) = saved();
        let (to_home, _frames) = mpsc::channel(1);
        let mut capture = Capture::start(graph.clone(), saved, to_home, 1).unwrap();
        let sink = graph.state().sinks[0].1.clone();
        (sink.lock().unwrap())(&tone(4));
        assert_eq!(
            capture.report().as_deref(),
            Some("3 frames dropped, the link had no room")
        );
        // Told once.
        assert_eq!(capture.report(), None);
    }

    #[test]
    fn the_periodic_stats_name_each_kind() {
        let graph = FakeGraph::default();
        let (to_remote, _orders) = mpsc::channel(8);
        let mut home = Home::new(graph.clone(), to_remote);
        assert_eq!(home.stats(0), None, "nothing plays yet");
        home.start().unwrap();
        let packets = codec::tests::packets(&mut Encoder::new().unwrap(), &tone(10));
        for seq in [0u32, 1, 2, 4] {
            home.received(Message::Frame {
                session: 1,
                seq,
                packet: Bytes::from(packets[seq as usize].clone()),
            })
            .unwrap();
        }
        assert_eq!(
            home.stats(2).as_deref(),
            Some(
                "buffer 50 ms, target 10 ms, rate +0.000 %; underruns 0, trims 0, concealed 1, \
                 lost 0, late 0, dropped 2"
            )
        );
    }

    #[test]
    fn home_asks_the_playback_to_catch_up_when_the_remote_runs_ahead() {
        let graph = FakeGraph::default();
        let (to_remote, _orders) = mpsc::channel(8);
        let mut home = Home::new(graph.clone(), to_remote);
        home.start().unwrap();
        let jitter = graph.state().playbacks[0].1.clone();
        let packets = codec::tests::packets(&mut Encoder::new().unwrap(), &tone(10));
        let mut seq = 0u32;
        let mut out = vec![0.0; 2048];
        // The remote's clock runs fast: a frame more than home plays every few cycles.
        for cycle in 0..2000 {
            let frames = if cycle % 4 == 0 { 3 } else { 2 };
            for _ in 0..frames {
                home.received(Message::Frame {
                    session: 1,
                    seq,
                    packet: Bytes::from(packets[seq as usize % packets.len()].clone()),
                })
                .unwrap();
                seq += 1;
            }
            jitter.lock().unwrap().pull(&mut out);
        }
        assert!(jitter.lock().unwrap().rate() > 1.0);
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
