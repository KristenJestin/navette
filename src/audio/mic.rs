//! Home's mic, heard on the remote machine: the mirror of the remote's sound.
//!
//! While a mode runs, the remote machine has a virtual source named for Navette, made its
//! default mic; what home sends plays into it through a small jitter buffer, and it is silent
//! while nothing comes (the mic is home). At home, the mic is captured with the echo of the
//! home speakers cancelled, Opus-encoded and sent while the mic goes to the remote machine;
//! meanwhile home's default mic is a silent source, so that home apps do not hear it.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use super::codec::Decoder;
use super::jitter::Jitter;
use super::{
    Device, Graph, JITTER_CEILING, JITTER_FLOOR, Message, Saved, Switch, Undecoded, encoding, lock,
    repair_one,
};

/// The remote's virtual source.
pub const SOURCE: &str = "navette-home-mic";
const SOURCE_DESCRIPTION: &str = "Home mic (Navette)";
/// Home's echo-cancelled mic.
pub const MIC: &str = "navette-mic";
const MIC_DESCRIPTION: &str = "Mic to the remote machine (Navette)";

/// The remote's source for one session. Dropping it puts the previous default mic back, then
/// removes the source.
pub struct Voice<G: Graph> {
    // Field order is drop order: the apps move straight to the previous default.
    switch: Switch<G>,
    node: G::Node,
    jitter: Arc<Mutex<Jitter>>,
    session: u32,
    decoder: Decoder,
    undecoded: Undecoded,
}

impl<G: Graph> Voice<G> {
    pub fn start(graph: G, saved: Saved, session: u32) -> io::Result<Self> {
        let switch = Switch::save(graph, saved, Device::Source, SOURCE)?;
        let jitter = Arc::new(Mutex::new(Jitter::new(JITTER_FLOOR, JITTER_CEILING)));
        let node = switch
            .graph
            .open_source(SOURCE, SOURCE_DESCRIPTION, jitter.clone())?;
        switch.switch()?;
        Ok(Self {
            switch,
            node,
            jitter,
            session,
            decoder: Decoder::new()?,
            undecoded: Undecoded::new("mic"),
        })
    }

    /// Takes a new session, which home started without stopping the last: the source stays the
    /// default mic.
    pub fn restart(&mut self, session: u32) -> io::Result<()> {
        self.decoder = Decoder::new()?;
        self.undecoded = Undecoded::new("mic");
        self.session = session;
        Ok(())
    }

    /// A frame of the mic; one of an earlier session, still on its way, is dropped.
    pub fn received(&mut self, session: u32, seq: u32, packet: &[u8]) -> io::Result<()> {
        if session != self.session {
            return Ok(());
        }
        match self.decoder.decode(seq, packet) {
            Ok(pcm) => lock(&self.jitter).push(&pcm),
            Err(e) => return self.undecoded.failed(e),
        }
        Ok(())
    }

    /// Opens the source again if PipeWire restarted under it, and makes it the default again.
    pub fn recover(&mut self) -> io::Result<()> {
        if !self.switch.graph.is_gone(&self.node) {
            return Ok(());
        }
        self.node =
            self.switch
                .graph
                .open_source(SOURCE, SOURCE_DESCRIPTION, self.jitter.clone())?;
        self.switch.switch()
    }
}

/// While the mic goes to the remote machine, home's default mic: a source of Navette's that
/// stays silent, so that home apps no longer hear the mic.
pub const AWAY: &str = "navette-mic-away";
const AWAY_DESCRIPTION: &str = "Mic away (Navette)";

/// Home's mic while a mode runs. Its echo-cancelled capture stays open for the whole mode; while
/// it goes to the remote machine, it is sent there, and home's apps hear [`AWAY`]. Dropping it
/// gives home its mic back, then closes the echo canceller.
pub struct HomeMic<G: Graph> {
    // Field order is drop order: the apps get their mic back first.
    away: Option<Away<G>>,
    capture: G::Node,
    graph: G,
    saved: Saved,
    /// The mic captured, by name.
    mic: String,
    sending: Sending,
}

/// Where the capture's frames go, and their numbering, kept across a PipeWire restart.
#[derive(Clone)]
struct Sending {
    remote_machine: mpsc::Sender<Message>,
    session: u32,
    seq: Arc<AtomicU32>,
    failures: Arc<AtomicU64>,
    /// Read by the capture's callback, on PipeWire's thread.
    remote: Arc<AtomicBool>,
}

/// Home's default mic switched to the silent source, put back when dropped.
struct Away<G: Graph> {
    // Field order is drop order, as for the remote's source.
    switch: Switch<G>,
    node: G::Node,
}

impl<G: Graph> HomeMic<G> {
    /// Opens the mic home's apps have now and sends it to the remote machine, its frames to
    /// `remote_machine` under `session`.
    pub fn start(
        graph: G,
        saved: Saved,
        remote_machine: mpsc::Sender<Message>,
        session: u32,
    ) -> io::Result<Self> {
        // Named, not followed: the default goes silent while the mic is away.
        let mic = graph
            .mic_in_use()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home has no mic"))?;
        let sending = Sending {
            remote_machine,
            session,
            seq: Arc::default(),
            failures: Arc::default(),
            remote: Arc::default(),
        };
        let capture = open_capture(&graph, &mic, &sending)?;
        let mut home_mic = Self {
            away: None,
            capture,
            graph,
            saved,
            mic,
            sending,
        };
        home_mic.send_to(true)?;
        Ok(home_mic)
    }

    /// Sends the mic to the remote machine, home's apps hearing silence meanwhile, or keeps it
    /// home.
    pub fn send_to(&mut self, remote: bool) -> io::Result<()> {
        if !remote {
            self.sending.remote.store(false, Ordering::Relaxed);
            self.away = None;
            return Ok(());
        }
        if self.away.is_none() {
            let switch =
                Switch::save(self.graph.clone(), self.saved.clone(), Device::Source, AWAY)?;
            let node = open_away(&self.graph)?;
            switch.switch()?;
            self.away = Some(Away { switch, node });
        }
        self.sending.remote.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Whether the mic goes to the remote machine.
    pub fn remote(&self) -> bool {
        self.sending.remote.load(Ordering::Relaxed)
    }

    /// Opens again what went with PipeWire when it restarted: the capture, with its echo
    /// canceller, and the silent source, made the default again.
    pub fn recover(&mut self) -> io::Result<()> {
        if self.graph.is_gone(&self.capture) {
            self.capture = open_capture(&self.graph, &self.mic, &self.sending)?;
        }
        if let Some(away) = &mut self.away
            && self.graph.is_gone(&away.node)
        {
            away.node = open_away(&self.graph)?;
            away.switch.switch()?;
        }
        Ok(())
    }
}

fn open_capture<G: Graph>(graph: &G, mic: &str, sending: &Sending) -> io::Result<G::Node> {
    let mut send = encoding(
        sending.remote_machine.clone(),
        sending.session,
        sending.seq.clone(),
        sending.failures.clone(),
        // Not reported: the remote's mic fills the gap, and only the remote's sound is counted.
        Arc::default(),
        |session, seq, packet| Message::MicFrame {
            session,
            seq,
            packet,
        },
    )?;
    let remote = sending.remote.clone();
    graph.open_mic(
        MIC,
        MIC_DESCRIPTION,
        mic,
        Box::new(move |pcm| {
            // The mic home is nothing sent: the remote's source plays silence.
            if remote.load(Ordering::Relaxed) {
                send(pcm);
            }
        }),
    )
}

/// A source that nothing plays into: silence.
fn open_away<G: Graph>(graph: &G) -> io::Result<G::Node> {
    let silence = Arc::new(Mutex::new(Jitter::new(JITTER_FLOOR, JITTER_CEILING)));
    graph.open_source(AWAY, AWAY_DESCRIPTION, silence)
}

impl<G: Graph> Drop for HomeMic<G> {
    fn drop(&mut self) {
        let failures = self.sending.failures.load(Ordering::Relaxed);
        if failures > 0 {
            eprintln!("navette: mic: {failures} cycles of sound could not be encoded");
        }
    }
}

/// Gives home back the mic a `navette serve` that died left on [`AWAY`], unless the user chose
/// another since.
pub fn repair_home<G: Graph>(graph: &G, saved: &Saved) {
    if let Err(e) = repair_one(graph, saved, Device::Source, AWAY) {
        eprintln!("navette: mic: could not put back the mic a previous run left: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::fake::FakeGraph;
    use super::super::tests::{WAIT, blocking, linked, saved, tone, until};
    use super::super::{DefaultNode, FRAME_SAMPLES, remote};
    use super::*;

    /// What the remote's source plays next, once something loud is in it.
    async fn heard(graph: &FakeGraph) -> bool {
        let jitter = graph.state().sources[0].1.clone();
        tokio::time::timeout(WAIT, async {
            loop {
                let mut out = vec![0.0; 10 * FRAME_SAMPLES];
                jitter.lock().unwrap().pull(&mut out);
                if out.iter().any(|s| s.abs() > 0.1) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok()
    }

    /// The home mic hears `frames` of a tone, a graph quantum at a time.
    fn speak(graph: &FakeGraph, frames: usize) {
        let mic = graph.state().mics[0].1.clone();
        for block in tone(frames).chunks(1024 * super::super::codec::CHANNELS) {
            (mic.lock().unwrap())(block);
        }
    }

    #[test]
    fn the_navette_source_becomes_the_default_mic_and_stop_restores_it() {
        let graph = FakeGraph::with_default("laptop-speakers");
        graph.state().default_source = Some(DefaultNode::named("laptop-mic"));
        let (_tmp, saved) = saved();
        let voice = Voice::start(graph.clone(), saved.clone(), 1).unwrap();
        assert_eq!(
            graph.state().default_source,
            Some(DefaultNode::named(SOURCE))
        );
        // The output is not the mic's to change.
        assert_eq!(
            graph.state().default,
            Some(DefaultNode::named("laptop-speakers"))
        );
        drop(voice);
        let state = graph.state();
        assert_eq!(state.default_source, Some(DefaultNode::named("laptop-mic")));
        assert!(state.sources.is_empty());
        assert_eq!(
            state.log,
            [
                format!("source {SOURCE}"),
                format!("default source {}", DefaultNode::named(SOURCE).0),
                format!("default source {}", DefaultNode::named("laptop-mic").0),
                format!("remove {SOURCE}"),
            ]
        );
    }

    #[test]
    fn the_mic_and_the_sound_keep_their_own_saved_defaults() {
        let graph = FakeGraph::with_default("laptop-speakers");
        graph.state().default_source = Some(DefaultNode::named("laptop-mic"));
        let (_tmp, saved) = saved();
        // An agent that died with both switched.
        std::mem::forget(super::super::RemoteSink::start(
            graph.clone(),
            saved.clone(),
            Box::new(|_| {}),
            Box::new(|_| {}),
        ));
        std::mem::forget(Voice::start(graph.clone(), saved.clone(), 1).unwrap());

        drop(Voice::start(graph.clone(), saved.clone(), 2).unwrap());
        assert_eq!(
            graph.state().default_source,
            Some(DefaultNode::named("laptop-mic"))
        );
        assert_eq!(
            saved.load(Device::Sink).unwrap(),
            Some(Some(DefaultNode::named("laptop-speakers")))
        );
        assert_eq!(saved.load(Device::Source).unwrap(), None);
    }

    #[tokio::test]
    async fn an_agent_that_crashed_is_repaired_as_soon_as_audio_starts_again() {
        let graph = FakeGraph::with_default("laptop-speakers");
        graph.state().default_source = Some(DefaultNode::named("laptop-mic"));
        let (_tmp, saved) = saved();
        std::mem::forget(super::super::RemoteSink::start(
            graph.clone(),
            saved.clone(),
            Box::new(|_| {}),
            Box::new(|_| {}),
        ));
        std::mem::forget(Voice::start(graph.clone(), saved.clone(), 1).unwrap());

        // The next agent: no session asked for yet.
        let (wire, _link) = super::super::wire();
        remote(graph.clone(), saved.clone(), wire, None).unwrap();
        until(&graph, |s| {
            s.default == Some(DefaultNode::named("laptop-speakers"))
                && s.default_source == Some(DefaultNode::named("laptop-mic"))
        })
        .await;
        assert_eq!(saved.load(Device::Sink).unwrap(), None);
        assert_eq!(saved.load(Device::Source).unwrap(), None);
    }

    #[test]
    fn a_default_the_user_changed_since_the_crash_is_left_alone() {
        let graph = FakeGraph::with_default("laptop-speakers");
        let (_tmp, saved) = saved();
        std::mem::forget(super::super::RemoteSink::start(
            graph.clone(),
            saved.clone(),
            Box::new(|_| {}),
            Box::new(|_| {}),
        ));
        graph.state().default = Some(DefaultNode::named("headset"));
        super::super::repair(&graph, &saved);
        assert_eq!(graph.state().default, Some(DefaultNode::named("headset")));
        assert_eq!(saved.load(Device::Sink).unwrap(), None);
    }

    #[tokio::test]
    async fn the_home_mic_is_heard_on_the_remote_source() {
        let home_graph = FakeGraph::default();
        let remote_graph = FakeGraph::default();
        let (_tmp, saved) = saved();
        let (home_peer, remote_peer, _link) = linked().await;
        remote(remote_graph.clone(), saved, remote_peer, None).unwrap();
        home_peer
            .to_peer
            .send(Message::MicStart { session: 3 })
            .await
            .unwrap();
        until(&remote_graph, |s| !s.sources.is_empty()).await;

        home_graph.state().default_source = Some(DefaultNode::named("desk-mic"));
        let (_home_tmp, home_saved) = super::super::tests::saved();
        let to_remote = home_peer.to_peer.clone();
        let _mic = HomeMic::start(home_graph.clone(), home_saved, to_remote, 3).unwrap();
        speak(&home_graph, 20);
        assert!(
            heard(&remote_graph).await,
            "the mic never reached the source"
        );
    }

    /// Home, its mic `desk-mic`, and a mic started on it, sending to the returned queue.
    fn home_mic() -> (
        FakeGraph,
        tempfile::TempDir,
        Saved,
        HomeMic<FakeGraph>,
        mpsc::Receiver<Message>,
    ) {
        let graph = FakeGraph::default();
        graph.state().default_source = Some(DefaultNode::named("desk-mic"));
        let (tmp, saved) = saved();
        let (to_remote, frames) = mpsc::channel(super::super::QUEUE);
        let mic = HomeMic::start(graph.clone(), saved.clone(), to_remote, 1).unwrap();
        (graph, tmp, saved, mic, frames)
    }

    #[test]
    fn while_the_mic_is_remote_home_apps_hear_a_silent_mic() {
        let (graph, _tmp, saved, mut mic, _frames) = home_mic();
        assert_eq!(graph.state().default_source, Some(DefaultNode::named(AWAY)));
        let silence = graph.state().sources[0].1.clone();
        let mut out = vec![1.0; 10 * FRAME_SAMPLES];
        silence.lock().unwrap().pull(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));

        mic.send_to(false).unwrap();
        assert_eq!(
            graph.state().default_source,
            Some(DefaultNode::named("desk-mic"))
        );
        assert!(graph.state().sources.is_empty());
        mic.send_to(true).unwrap();
        assert_eq!(graph.state().default_source, Some(DefaultNode::named(AWAY)));

        drop(mic);
        assert_eq!(
            graph.state().default_source,
            Some(DefaultNode::named("desk-mic"))
        );
        assert!(graph.state().sources.is_empty());
        assert_eq!(saved.load(Device::Source).unwrap(), None);
    }

    #[test]
    fn the_echo_canceller_hears_the_mic_itself_while_home_apps_hear_silence() {
        let (graph, _tmp, _saved, _mic, _frames) = home_mic();
        assert_eq!(
            graph.state().log,
            [
                format!("mic {MIC} <- desk-mic"),
                format!("source {AWAY}"),
                format!("default source {}", DefaultNode::named(AWAY).0),
            ]
        );
    }

    #[test]
    fn a_home_that_crashed_with_the_mic_away_gets_its_mic_back() {
        let (graph, _tmp, saved, mic, _frames) = home_mic();
        std::mem::forget(mic);
        repair_home(&graph, &saved);
        assert_eq!(
            graph.state().default_source,
            Some(DefaultNode::named("desk-mic"))
        );
        assert_eq!(saved.load(Device::Source).unwrap(), None);
    }

    /// The sequence numbers of the frames sent so far.
    fn sent(frames: &mut mpsc::Receiver<Message>) -> Vec<u32> {
        std::iter::from_fn(|| frames.try_recv().ok())
            .filter_map(|m| match m {
                Message::MicFrame { seq, .. } => Some(seq),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_home_mic_comes_back_after_pipewire_restarts() {
        let (graph, _tmp, _saved, mut mic, mut frames) = home_mic();
        speak(&graph, 4);
        let before = *sent(&mut frames).last().unwrap();
        mic.recover().unwrap();
        let opened = graph.state().log.len();
        assert_eq!(opened, 3, "nothing to open again while nothing is gone");

        graph.restart();
        mic.recover().unwrap();
        assert_eq!(
            graph.state().log[opened + 1..],
            [
                format!("mic {MIC} <- desk-mic"),
                format!("source {AWAY}"),
                format!("default source {}", DefaultNode::named(AWAY).0),
            ]
        );
        speak(&graph, 4);
        // Numbered on, or the remote's decoder would take them for late ones.
        assert!(sent(&mut frames).first().unwrap() > &before);
    }

    #[test]
    fn with_the_mic_home_a_restart_opens_only_the_capture_again() {
        let (graph, _tmp, _saved, mut mic, _frames) = home_mic();
        mic.send_to(false).unwrap();
        graph.restart();
        mic.recover().unwrap();
        let state = graph.state();
        assert_eq!(state.log.last().unwrap(), &format!("mic {MIC} <- desk-mic"));
        assert!(state.sources.is_empty());
    }

    #[test]
    fn a_home_with_no_mic_says_so() {
        let (_tmp, saved) = saved();
        let (to_remote, _frames) = mpsc::channel(super::super::QUEUE);
        let error = HomeMic::start(FakeGraph::default(), saved, to_remote, 1)
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn with_the_mic_home_nothing_is_sent() {
        let (graph, _tmp, _saved, mut mic, mut frames) = home_mic();
        mic.send_to(false).unwrap();
        speak(&graph, 4);
        assert!(frames.try_recv().is_err());

        mic.send_to(true).unwrap();
        speak(&graph, 4);
        match frames.try_recv() {
            Ok(Message::MicFrame { session: 1, .. }) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mic_frames_of_another_session_are_dropped() {
        let graph = FakeGraph::default();
        let (_tmp, saved) = saved();
        let mut voice = Voice::start(graph.clone(), saved, 2).unwrap();
        let packets = super::super::codec::tests::packets(
            &mut super::super::codec::Encoder::new().unwrap(),
            &tone(20),
        );
        for (seq, packet) in packets.iter().enumerate() {
            voice.received(1, seq as u32, packet).unwrap();
        }
        let jitter = graph.state().sources[0].1.clone();
        let mut out = vec![0.0; 10 * FRAME_SAMPLES];
        jitter.lock().unwrap().pull(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn a_run_of_broken_mic_packets_is_told_once() {
        let graph = FakeGraph::default();
        let (_tmp, saved) = saved();
        let mut voice = Voice::start(graph, saved, 1).unwrap();
        let broken = super::super::tests::BROKEN;
        assert!(voice.received(1, 0, &broken).is_err());
        assert!(voice.received(1, 1, &broken).is_ok());
        assert!(voice.received(1, 2, &broken).is_ok());
        assert_eq!(voice.undecoded.0, 3);
    }

    #[tokio::test]
    async fn the_link_dropping_restores_the_remote_default_mic() {
        let graph = FakeGraph::default();
        graph.state().default_source = Some(DefaultNode::named("laptop-mic"));
        let (_tmp, saved) = saved();
        let (home_peer, remote_peer, link) = linked().await;
        let audio = remote(graph.clone(), saved, remote_peer, None).unwrap();
        home_peer
            .to_peer
            .send(Message::MicStart { session: 1 })
            .await
            .unwrap();
        until(&graph, |s| !s.sources.is_empty()).await;

        link.abort();
        blocking(move || audio.join().unwrap()).await;
        assert!(graph.state().sources.is_empty());
        assert_eq!(
            graph.state().default_source,
            Some(DefaultNode::named("laptop-mic"))
        );
    }

    #[tokio::test]
    async fn mic_stop_restores_the_remote_default_mic() {
        let graph = FakeGraph::default();
        graph.state().default_source = Some(DefaultNode::named("laptop-mic"));
        let (_tmp, saved) = saved();
        let (wire, link) = super::super::wire();
        remote(graph.clone(), saved, wire, None).unwrap();
        link.to_audio
            .send(Message::MicStart { session: 1 })
            .unwrap();
        until(&graph, |s| !s.sources.is_empty()).await;
        link.to_audio.send(Message::MicStop).unwrap();
        until(&graph, |s| {
            s.sources.is_empty() && s.default_source == Some(DefaultNode::named("laptop-mic"))
        })
        .await;
    }

    #[tokio::test]
    async fn a_new_session_keeps_the_source_the_default_mic() {
        let graph = FakeGraph::default();
        graph.state().default_source = Some(DefaultNode::named("laptop-mic"));
        let (_tmp, saved) = saved();
        let (wire, link) = super::super::wire();
        let _audio = remote(graph.clone(), saved, wire, None).unwrap();
        link.to_audio
            .send(Message::MicStart { session: 1 })
            .unwrap();
        // A home that restarted, say: no stop came first.
        link.to_audio
            .send(Message::MicStart { session: 2 })
            .unwrap();
        let packets = super::super::codec::tests::packets(
            &mut super::super::codec::Encoder::new().unwrap(),
            &tone(20),
        );
        for (seq, packet) in packets.into_iter().enumerate() {
            link.to_audio
                .send(Message::MicFrame {
                    session: 2,
                    seq: seq as u32,
                    packet: packet.into(),
                })
                .unwrap();
        }
        until(&graph, |s| !s.sources.is_empty()).await;
        assert!(heard(&graph).await, "the new session was not heard");
        let log = graph.state().log.clone();
        assert_eq!(
            log,
            [
                format!("source {SOURCE}"),
                format!("default source {}", DefaultNode::named(SOURCE).0),
            ]
        );
    }

    #[tokio::test]
    async fn the_source_comes_back_after_pipewire_restarts() {
        let graph = FakeGraph::default();
        graph.state().default_source = Some(DefaultNode::named("laptop-mic"));
        let (_tmp, saved) = saved();
        let (wire, link) = super::super::wire();
        let audio = remote(graph.clone(), saved, wire, None).unwrap();
        link.to_audio
            .send(Message::MicStart { session: 1 })
            .unwrap();
        until(&graph, |s| !s.sources.is_empty()).await;

        graph.restart();
        graph.state().default_source = Some(DefaultNode::named("webcam-mic"));
        until(&graph, |s| {
            !s.sources.is_empty() && s.default_source == Some(DefaultNode::named(SOURCE))
        })
        .await;

        drop(link);
        blocking(move || audio.join().unwrap()).await;
        assert_eq!(
            graph.state().default_source,
            Some(DefaultNode::named("laptop-mic"))
        );
    }
}
