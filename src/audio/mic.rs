//! Home's mic, heard on the remote machine: the mirror of the remote's sound.
//!
//! While a mode runs, the remote machine has a virtual source named for Navette, made its
//! default mic; what home sends plays into it through a small jitter buffer, and it is silent
//! while nothing comes (the mic is home). At home, the mic is captured with the echo of the
//! home speakers cancelled, Opus-encoded and sent while the mic goes to the remote machine.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use super::codec::Decoder;
use super::jitter::Jitter;
use super::{Device, Graph, JITTER_MAX, JITTER_TARGET, Message, Saved, Switch, encoding, lock};

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
}

impl<G: Graph> Voice<G> {
    pub fn start(graph: G, saved: Saved, session: u32) -> io::Result<Self> {
        let switch = Switch::save(graph, saved, Device::Source, SOURCE)?;
        let jitter = Arc::new(Mutex::new(Jitter::new(JITTER_TARGET, JITTER_MAX)));
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
        })
    }

    /// A frame of the mic; one of an earlier session, still on its way, is dropped.
    pub fn received(&mut self, session: u32, seq: u32, packet: &[u8]) -> io::Result<()> {
        if session != self.session {
            return Ok(());
        }
        let pcm = self.decoder.decode(seq, packet)?;
        lock(&self.jitter).push(&pcm);
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

/// Home's mic while a mode runs: it is sent to the remote machine while `remote` holds, and
/// the echo canceller goes when it drops.
pub struct HomeMic<G: Graph> {
    _node: G::Node,
    failures: Arc<AtomicU64>,
}

impl<G: Graph> HomeMic<G> {
    /// Opens the mic; its frames go to `remote_machine` under `session`.
    pub fn start(
        graph: &G,
        remote_machine: mpsc::Sender<Message>,
        session: u32,
        remote: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let failures = Arc::new(AtomicU64::new(0));
        let mut send = encoding(
            remote_machine,
            session,
            Arc::new(AtomicU32::new(0)),
            failures.clone(),
            |session, seq, packet| Message::MicFrame {
                session,
                seq,
                packet,
            },
        )?;
        let node = graph.open_mic(
            MIC,
            MIC_DESCRIPTION,
            Box::new(move |pcm| {
                // The mic home is nothing sent: the remote's source plays silence.
                if remote.load(Ordering::Relaxed) {
                    send(pcm);
                }
            }),
        )?;
        Ok(Self {
            _node: node,
            failures,
        })
    }
}

impl<G: Graph> Drop for HomeMic<G> {
    fn drop(&mut self) {
        let failures = self.failures.load(Ordering::Relaxed);
        if failures > 0 {
            eprintln!("navette: mic: {failures} cycles of sound could not be encoded");
        }
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

        let to_remote = home_peer.to_peer.clone();
        let mic = HomeMic::start(&home_graph, to_remote, 3, Arc::new(AtomicBool::new(true)));
        let _mic = mic.unwrap();
        speak(&home_graph, 20);
        assert!(
            heard(&remote_graph).await,
            "the mic never reached the source"
        );
    }

    #[test]
    fn with_the_mic_home_nothing_is_sent() {
        let graph = FakeGraph::default();
        let (to_remote, mut frames) = mpsc::channel(super::super::QUEUE);
        let remote = Arc::new(AtomicBool::new(false));
        let _mic = HomeMic::start(&graph, to_remote, 1, remote.clone()).unwrap();
        speak(&graph, 4);
        assert!(frames.try_recv().is_err());

        remote.store(true, Ordering::Relaxed);
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
