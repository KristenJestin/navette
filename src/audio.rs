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
//! The audio messages, one block of the link's message types:
//! - `START` (home → remote): make the sink the default output and send its sound.
//! - `STOP` (home → remote): put the previous default back and stop sending.
//! - `FRAME` (remote → home): a big-endian `u32` sequence number, then one Opus packet.

pub mod codec;
#[cfg(test)]
pub mod fake;
pub mod jitter;
pub mod pipewire;

use std::io;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tokio::sync::mpsc;

use crate::link::Peer;
use codec::{Decoder, Encoder, FRAME_SAMPLES};
use jitter::Jitter;

const START: u8 = 0x30;
const STOP: u8 = 0x31;
const FRAME: u8 = 0x32;

/// The remote's virtual sink.
pub const SINK: &str = "navette-home-speakers";
const SINK_DESCRIPTION: &str = "Home speakers (Navette)";
/// Home's playback stream.
pub const PLAYBACK: &str = "navette-remote-sound";
const PLAYBACK_DESCRIPTION: &str = "Remote machine (Navette)";

/// Home starts playing once 30 ms are buffered, and keeps at most 120 ms.
const JITTER_TARGET: usize = 3 * FRAME_SAMPLES;
const JITTER_MAX: usize = 12 * FRAME_SAMPLES;

#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Start,
    Stop,
    Frame { seq: u32, packet: Vec<u8> },
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Message::Start => vec![START],
            Message::Stop => vec![STOP],
            Message::Frame { seq, packet } => {
                let mut frame = Vec::with_capacity(5 + packet.len());
                frame.push(FRAME);
                frame.extend_from_slice(&seq.to_be_bytes());
                frame.extend_from_slice(packet);
                frame
            }
        }
    }

    pub fn decode(frame: &[u8]) -> io::Result<Self> {
        match frame {
            [START] => Ok(Message::Start),
            [STOP] => Ok(Message::Stop),
            [FRAME, a, b, c, d, packet @ ..] => Ok(Message::Frame {
                seq: u32::from_be_bytes([*a, *b, *c, *d]),
                packet: packet.to_vec(),
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an audio message",
            )),
        }
    }
}

/// The configured default sink, as PipeWire's `default` metadata holds it: kept as is, to be
/// put back as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultSink(pub String);

impl DefaultSink {
    pub fn named(node: &str) -> Self {
        Self(format!("{{ \"name\": \"{node}\" }}"))
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

    /// The default sink the user configured, if any.
    fn default_sink(&self) -> io::Result<Option<DefaultSink>>;
    /// Configures the default sink; `None` clears it, leaving the choice to the session manager.
    fn set_default_sink(&self, sink: Option<&DefaultSink>) -> io::Result<()>;
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
}

/// The remote's sink while home asks for the sound. Dropping it puts the previous default
/// back, then removes the sink.
pub struct RemoteSink<G: Graph> {
    graph: G,
    previous: Option<DefaultSink>,
    _node: G::Node,
}

impl<G: Graph> RemoteSink<G> {
    pub fn start(graph: G, on_sound: OnSound) -> io::Result<Self> {
        let ours = DefaultSink::named(SINK);
        // Left on our own sink by an agent that did not end cleanly: nothing to put back.
        let previous = graph.default_sink()?.filter(|sink| *sink != ours);
        let node = graph.open_sink(SINK, SINK_DESCRIPTION, on_sound)?;
        let sink = Self {
            graph,
            previous,
            _node: node,
        };
        sink.graph.set_default_sink(Some(&ours))?;
        Ok(sink)
    }
}

impl<G: Graph> Drop for RemoteSink<G> {
    fn drop(&mut self) {
        if let Err(e) = self.graph.set_default_sink(self.previous.as_ref()) {
            eprintln!("navette: audio: could not put the previous default sink back: {e}");
        }
    }
}

/// Encodes the sink's sound and sends it home, on PipeWire's thread.
fn sender(home: mpsc::Sender<Vec<u8>>) -> io::Result<OnSound> {
    let mut encoder = Encoder::new()?;
    let mut seq = 0u32;
    Ok(Box::new(move |pcm| match encoder.push(pcm) {
        Ok(packets) => {
            for packet in packets {
                let frame = Message::Frame { seq, packet };
                seq = seq.wrapping_add(1);
                // Late sound is worse than lost sound: a full link drops the frame, and home
                // fills the gap.
                let _ = home.try_send(frame.encode());
            }
        }
        Err(e) => eprintln!("navette: audio: {e}"),
    }))
}

/// The remote's side of one link, on a thread of its own: follows home's orders until the
/// link drops, then puts the previous default sink back. Joining the thread waits for that.
pub fn remote<G: Graph>(graph: G, mut peer: Peer) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("navette-audio".into())
        .spawn(move || {
            let mut sink = None;
            while let Some(frame) = peer.received.blocking_recv() {
                match Message::decode(&frame) {
                    Ok(Message::Start) if sink.is_none() => {
                        match sender(peer.send.clone())
                            .and_then(|on_sound| RemoteSink::start(graph.clone(), on_sound))
                        {
                            Ok(started) => sink = Some(started),
                            Err(e) => eprintln!("navette: audio: could not start the sink: {e}"),
                        }
                    }
                    Ok(Message::Stop) => sink = None,
                    Ok(_) => {}
                    Err(e) => eprintln!("navette: audio: {e}"),
                }
            }
        })
}

struct Playback<G: Graph> {
    _node: G::Node,
    jitter: Arc<Mutex<Jitter>>,
    decoder: Decoder,
}

/// Home's side: asks the remote for its sound and plays it. Its calls wait for PipeWire and
/// the link: they are made from an audio thread, never from the async runtime.
pub struct Home<G: Graph> {
    graph: G,
    remote: mpsc::Sender<Vec<u8>>,
    playback: Option<Playback<G>>,
}

impl<G: Graph> Home<G> {
    pub fn new(graph: G, remote: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            graph,
            remote,
            playback: None,
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
        self.playback = Some(Playback {
            _node: node,
            jitter,
            decoder: Decoder::new()?,
        });
        self.order(Message::Start)
    }

    pub fn stop(&mut self) -> io::Result<()> {
        self.playback = None;
        self.order(Message::Stop)
    }

    fn order(&self, message: Message) -> io::Result<()> {
        self.remote
            .blocking_send(message.encode())
            .map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "the link is down"))
    }

    /// A message from the remote; its sound is ignored unless home is playing.
    pub fn received(&mut self, frame: &[u8]) -> io::Result<()> {
        if let Message::Frame { seq, packet } = Message::decode(frame)?
            && let Some(playback) = &mut self.playback
        {
            let pcm = playback.decoder.decode(seq, &packet)?;
            playback.jitter.lock().unwrap().push(&pcm);
        }
        Ok(())
    }
}

/// Home's side of one link, on a thread of its own: with `start`, plays the remote's sound
/// until the link drops.
pub fn home<G: Graph>(graph: G, mut peer: Peer, start: bool) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("navette-audio".into())
        .spawn(move || {
            let mut home = Home::new(graph, peer.send);
            if start && let Err(e) = home.start() {
                eprintln!("navette: audio: could not start the playback: {e}");
            }
            while let Some(frame) = peer.received.blocking_recv() {
                if let Err(e) = home.received(&frame) {
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

    const WAIT: Duration = Duration::from_secs(10);

    /// Two peers over a link in memory, and home's end of the link, to drop it.
    fn linked() -> (Peer, Peer, tokio::task::JoinHandle<io::Error>) {
        let (home_stream, remote_stream) = tokio::io::duplex(64 * 1024);
        let (home, home_wire) = link::peer();
        let (remote, remote_wire) = link::peer();
        let heartbeat = link::Heartbeat::default();
        let home_end = tokio::spawn(link::idle(home_stream, heartbeat, home_wire));
        tokio::spawn(link::idle(remote_stream, heartbeat, remote_wire));
        (home, remote, home_end)
    }

    async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        tokio::time::timeout(WAIT, tokio::task::spawn_blocking(f))
            .await
            .expect("in time")
            .unwrap()
    }

    /// Polls `check` on the fake graph until it holds.
    async fn until(graph: &FakeGraph, check: impl Fn(&fake::State) -> bool) {
        tokio::time::timeout(WAIT, async {
            while !check(&graph.state()) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the graph reached the expected state in time");
    }

    fn tone(frames: usize) -> Vec<f32> {
        (0..frames * codec::FRAME)
            .flat_map(|i| {
                let v = 0.5 * (i as f32 * 440.0 * std::f32::consts::TAU / codec::RATE as f32).sin();
                [v, v]
            })
            .collect()
    }

    #[test]
    fn the_audio_messages_round_trip() {
        for message in [
            Message::Start,
            Message::Stop,
            Message::Frame {
                seq: 0x0102_0304,
                packet: vec![9, 8, 7],
            },
        ] {
            assert_eq!(Message::decode(&message.encode()).unwrap(), message);
        }
        assert!(Message::decode(&[FRAME, 0, 0]).is_err());
        assert!(Message::decode(&[0x02]).is_err());
    }

    #[test]
    fn start_sets_the_default_sink_and_stop_restores_it() {
        let graph = FakeGraph::with_default("speakers");
        let sink = RemoteSink::start(graph.clone(), Box::new(|_| {})).unwrap();
        assert_eq!(graph.state().default, Some(DefaultSink::named(SINK)));
        drop(sink);
        let state = graph.state();
        assert_eq!(state.default, Some(DefaultSink::named("speakers")));
        // Put back before the sink goes, so the apps move straight to the previous one.
        assert_eq!(
            state.log,
            [
                format!("sink {SINK}"),
                format!("default {}", DefaultSink::named(SINK).0),
                format!("default {}", DefaultSink::named("speakers").0),
                format!("remove {SINK}"),
            ]
        );
    }

    #[test]
    fn with_no_default_before_stop_clears_it() {
        let graph = FakeGraph::default();
        drop(RemoteSink::start(graph.clone(), Box::new(|_| {})).unwrap());
        assert_eq!(graph.state().default, None);
    }

    #[test]
    fn a_default_left_on_the_navette_sink_is_not_restored() {
        let graph = FakeGraph::with_default(SINK);
        drop(RemoteSink::start(graph.clone(), Box::new(|_| {})).unwrap());
        assert_eq!(graph.state().default, None);
    }

    #[tokio::test]
    async fn the_remote_sound_plays_at_home() {
        let home_graph = FakeGraph::with_default("home-speakers");
        let remote_graph = FakeGraph::with_default("laptop-speakers");
        let (home_peer, remote_peer, _link) = linked();
        remote(remote_graph.clone(), remote_peer).unwrap();
        home(home_graph.clone(), home_peer, true).unwrap();

        until(&remote_graph, |s| !s.sinks.is_empty()).await;
        assert_eq!(remote_graph.state().default, Some(DefaultSink::named(SINK)));
        // Home's own default output is never touched.
        assert_eq!(
            home_graph.state().default,
            Some(DefaultSink::named("home-speakers"))
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
        let (home_peer, remote_peer, _link) = linked();
        remote(remote_graph.clone(), remote_peer).unwrap();
        let mut home = Home::new(home_graph.clone(), home_peer.send);
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
            Some(DefaultSink::named("laptop-speakers"))
        );
        assert!(home_graph.state().playbacks.is_empty());

        // And again: a mode can start and stop several times on one link.
        blocking(move || home.start().unwrap()).await;
        until(&remote_graph, |s| !s.sinks.is_empty()).await;
    }

    #[tokio::test]
    async fn the_link_dropping_restores_the_remote_default() {
        let remote_graph = FakeGraph::with_default("laptop-speakers");
        let (home_peer, remote_peer, link) = linked();
        let audio = remote(remote_graph.clone(), remote_peer).unwrap();
        home_peer.send.send(Message::Start.encode()).await.unwrap();
        until(&remote_graph, |s| !s.sinks.is_empty()).await;

        link.abort();
        // The thread ends once it has put the default back.
        blocking(move || audio.join().unwrap()).await;
        assert!(remote_graph.state().sinks.is_empty());
        assert_eq!(
            remote_graph.state().default,
            Some(DefaultSink::named("laptop-speakers"))
        );
    }
}
