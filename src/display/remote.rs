//! The agent's side of the display: on home's order, make the headless output and stream it;
//! take it away when home says stop or the link drops. While a mode is active, a reload gets
//! the monitor rule back and a Hyprland restart (or a vanished output) gets the output back.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use gliff_proto::{ClientCaps, ClientMsg};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use super::output;
use super::stream::{self, Source};
use crate::display::wire::{Display, Incoming, Msg, Outbox, OutputCopy, Peer};
use crate::hypr::{self, Hypr};

/// How often a lost output is made again.
const RETRY: Duration = Duration::from_secs(1);

/// Starts the picture path for an output (the port behind which tests put a fake). The stream
/// runs until the returned guard is dropped; `ended` says why if it stops on its own.
pub trait Streams: Send + Sync {
    fn start(
        &self,
        source: Source,
        caps: ClientCaps,
        outbox: Outbox,
        feedback: UnboundedReceiver<ClientMsg>,
        ended: oneshot::Sender<io::Error>,
    ) -> io::Result<Box<dyn Send + Sync>>;
}

/// The real picture path: capture, encode, send.
pub struct LiveStreams;

impl Streams for LiveStreams {
    fn start(
        &self,
        source: Source,
        caps: ClientCaps,
        outbox: Outbox,
        feedback: UnboundedReceiver<ClientMsg>,
        ended: oneshot::Sender<io::Error>,
    ) -> io::Result<Box<dyn Send + Sync>> {
        Ok(Box::new(stream::start(
            source, caps, outbox, feedback, ended,
        )?))
    }
}

/// Serves every link the agent brings up, one after the other.
pub async fn serve(
    mut peers: UnboundedReceiver<Peer>,
    hypr: Arc<dyn Hypr>,
    streams: Arc<dyn Streams>,
    mut events: UnboundedReceiver<hypr::Event>,
) {
    while let Some(peer) = peers.recv().await {
        serve_peer(peer, hypr.clone(), streams.clone(), &mut events).await;
    }
}

/// An active mode: the copy asked for, and its stream when it runs.
struct Active {
    copy: OutputCopy,
    caps: ClientCaps,
    running: Option<Running>,
}

struct Running {
    _stream: Box<dyn Send + Sync>,
    feedback: UnboundedSender<ClientMsg>,
    ended: oneshot::Receiver<io::Error>,
}

/// One link, until it drops; the output never outlives it. Hyprland requests and the stream's
/// start and stop block, so they run off the executor thread.
pub async fn serve_peer(
    mut peer: Peer,
    hypr: Arc<dyn Hypr>,
    streams: Arc<dyn Streams>,
    events: &mut UnboundedReceiver<hypr::Event>,
) {
    let mut active: Option<Active> = None;
    loop {
        let lost = active.as_ref().is_some_and(|a| a.running.is_none());
        tokio::select! {
            msg = peer.inbox.recv() => match msg {
                None => break,
                Some(Incoming::Display(Display::Start { output, caps })) => {
                    stop(&mut active, &hypr).await;
                    let mode = Active { copy: output, caps, running: None };
                    let (mode, result) = bring_up(mode, &hypr, &streams, &peer.outbox).await;
                    let reply = match result {
                        Ok(()) => {
                            active = Some(mode);
                            Display::Started { output: output::NAME.into() }
                        }
                        Err(e) => Display::Failed { reason: e.to_string() },
                    };
                    peer.outbox.send(Msg::Display(reply), Vec::new());
                }
                Some(Incoming::Display(Display::Stop)) => stop(&mut active, &hypr).await,
                Some(Incoming::Feedback(msg)) => {
                    if let Some(running) = active.as_ref().and_then(|a| a.running.as_ref()) {
                        let _ = running.feedback.send(msg);
                    }
                }
                Some(_) => {}
            },
            Some(event) = events.recv() => {
                if let (hypr::Event::ConfigReloaded, Some(mode)) = (event, &active) {
                    let (hypr, copy) = (hypr.clone(), mode.copy.clone());
                    let reapplied =
                        tokio::task::spawn_blocking(move || output::reapply(&*hypr, &copy)).await;
                    if let Ok(Err(e)) = reapplied {
                        eprintln!("navette: could not restore the output after a reload: {e}");
                    }
                }
            }
            Some(reason) = ended(&mut active) => {
                eprintln!("navette: the stream stopped ({reason}); making the output again");
                if let Some(mode) = &mut active {
                    mode.running = None;
                }
            }
            _ = tokio::time::sleep(RETRY), if lost => {
                if let Some(mode) = active.take() {
                    active = Some(bring_up(mode, &hypr, &streams, &peer.outbox).await.0);
                }
            }
        }
    }
    stop(&mut active, &hypr).await;
}

/// The reason a running stream stopped on its own; pending forever when none runs.
async fn ended(active: &mut Option<Active>) -> Option<io::Error> {
    match active.as_mut().and_then(|a| a.running.as_mut()) {
        Some(running) => (&mut running.ended).await.ok(),
        None => std::future::pending().await,
    }
}

/// Makes the output and starts its stream; on failure, nothing is left behind.
async fn bring_up(
    mut mode: Active,
    hypr: &Arc<dyn Hypr>,
    streams: &Arc<dyn Streams>,
    outbox: &Outbox,
) -> (Active, io::Result<()>) {
    let (hypr, streams, outbox) = (hypr.clone(), streams.clone(), outbox.clone());
    let task = tokio::task::spawn_blocking(move || {
        let result = start_blocking(&mut mode, &*hypr, &*streams, outbox);
        (mode, result)
    });
    task.await.expect("bringing the output up does not panic")
}

fn start_blocking(
    mode: &mut Active,
    hypr: &dyn Hypr,
    streams: &dyn Streams,
    outbox: Outbox,
) -> io::Result<()> {
    output::create(hypr, &mode.copy)?;
    let (feedback, feedback_rx) = mpsc::unbounded_channel();
    let (ended_tx, ended) = oneshot::channel();
    let source = Source {
        output: output::NAME.into(),
        scale: mode.copy.scale_milli as f32 / 1000.0,
    };
    match streams.start(source, mode.caps.clone(), outbox, feedback_rx, ended_tx) {
        Ok(stream) => {
            mode.running = Some(Running {
                _stream: stream,
                feedback,
                ended,
            });
            Ok(())
        }
        Err(e) => {
            let _ = output::remove(hypr);
            Err(e)
        }
    }
}

/// Stops the stream first, then removes the output it captured.
async fn stop(active: &mut Option<Active>, hypr: &Arc<dyn Hypr>) {
    let Some(mode) = active.take() else {
        return;
    };
    let hypr = hypr.clone();
    let removed = tokio::task::spawn_blocking(move || {
        drop(mode.running);
        output::remove(&*hypr)
    })
    .await;
    if let Ok(Err(e)) = removed {
        eprintln!("navette: could not remove the remote output: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use gliff_proto::{ChromaMode, Codec, ServerMsg};

    use super::*;
    use crate::hypr::fake::{FakeHypr, FakeMonitor};

    /// Streams that send one config message, and can be ended from the test.
    #[derive(Default)]
    struct FakeStreams {
        ends: Mutex<Vec<oneshot::Sender<io::Error>>>,
        started: Mutex<Vec<String>>,
        refuse: Mutex<bool>,
    }

    impl Streams for FakeStreams {
        fn start(
            &self,
            source: Source,
            _caps: ClientCaps,
            outbox: Outbox,
            _feedback: UnboundedReceiver<ClientMsg>,
            ended: oneshot::Sender<io::Error>,
        ) -> io::Result<Box<dyn Send + Sync>> {
            if *self.refuse.lock().unwrap() {
                return Err(io::Error::other("no encoder"));
            }
            self.started.lock().unwrap().push(source.output);
            self.ends.lock().unwrap().push(ended);
            outbox.send(Msg::Video(ServerMsg::Ping { t: 1 }), Vec::new());
            Ok(Box::new(()))
        }
    }

    impl FakeStreams {
        fn end_last(&self) {
            let end = self.ends.lock().unwrap().pop().unwrap();
            let _ = end.send(io::Error::other("capture stopped"));
        }
    }

    fn copy() -> OutputCopy {
        OutputCopy {
            width: 1920,
            height: 1080,
            refresh_mhz: 165_000,
            scale_milli: 1000,
            transform: 0,
        }
    }

    fn start() -> Incoming {
        Incoming::Display(Display::Start {
            output: copy(),
            caps: ClientCaps {
                codecs: vec![Codec::H264],
                max_width: 3840,
                max_height: 2160,
                chroma: vec![ChromaMode::Single420],
                features: Vec::new(),
            },
        })
    }

    struct Agent {
        inbox: UnboundedSender<Incoming>,
        outbox: Outbox,
        events: UnboundedSender<hypr::Event>,
        hypr: Arc<FakeHypr>,
        streams: Arc<FakeStreams>,
        task: tokio::task::JoinHandle<()>,
    }

    fn agent() -> Agent {
        let hypr = Arc::new(FakeHypr::with(vec![FakeMonitor::new(
            "eDP-1", 1920, 1200, 0,
        )]));
        let streams = Arc::new(FakeStreams::default());
        let (inbox, inbox_rx) = mpsc::unbounded_channel();
        let (events, mut events_rx) = mpsc::unbounded_channel();
        let outbox = Outbox::default();
        let peer = Peer {
            outbox: outbox.clone(),
            inbox: inbox_rx,
        };
        let task = {
            let (hypr, streams) = (hypr.clone(), streams.clone());
            tokio::spawn(async move {
                serve_peer(peer, hypr, streams, &mut events_rx).await;
            })
        };
        Agent {
            inbox,
            outbox,
            events,
            hypr,
            streams,
            task,
        }
    }

    /// Lets the agent handle what was sent, then waits until `done` holds.
    async fn until(done: impl Fn() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the agent never got there");
    }

    #[tokio::test]
    async fn starting_makes_the_output_and_streams_it() {
        let agent = agent();
        agent.inbox.send(start()).unwrap();
        until(|| agent.hypr.monitor(output::NAME).is_some()).await;
        until(|| agent.streams.started.lock().unwrap().len() == 1).await;
        assert_eq!(agent.streams.started.lock().unwrap()[0], output::NAME);
        until(|| agent.outbox.queued().len() == 2).await;
        assert_eq!(
            agent.outbox.queued()[1],
            Msg::Display(Display::Started {
                output: output::NAME.into()
            })
        );
    }

    #[tokio::test]
    async fn stopping_the_mode_removes_the_remote_output() {
        let agent = agent();
        agent.inbox.send(start()).unwrap();
        until(|| agent.hypr.monitor(output::NAME).is_some()).await;
        agent.inbox.send(Incoming::Display(Display::Stop)).unwrap();
        until(|| agent.hypr.monitor(output::NAME).is_none()).await;
        assert_eq!(
            agent.hypr.changes().last().unwrap(),
            "output remove NAVETTE-1"
        );
    }

    #[tokio::test]
    async fn a_dropped_link_removes_the_remote_output() {
        let agent = agent();
        agent.inbox.send(start()).unwrap();
        until(|| agent.hypr.monitor(output::NAME).is_some()).await;
        drop(agent.inbox);
        agent.task.await.unwrap();
        assert!(agent.hypr.monitor(output::NAME).is_none());
    }

    #[tokio::test]
    async fn a_stream_that_cannot_start_leaves_no_output_and_says_why() {
        let agent = agent();
        *agent.streams.refuse.lock().unwrap() = true;
        agent.inbox.send(start()).unwrap();
        until(|| !agent.outbox.queued().is_empty()).await;
        assert_eq!(
            agent.outbox.queued(),
            [Msg::Display(Display::Failed {
                reason: "no encoder".into()
            })]
        );
        assert!(agent.hypr.monitor(output::NAME).is_none());
        assert_eq!(
            agent.hypr.changes().last().unwrap(),
            "output remove NAVETTE-1"
        );
    }

    #[tokio::test]
    async fn the_output_comes_back_after_a_hyprland_restart() {
        let agent = agent();
        agent.inbox.send(start()).unwrap();
        until(|| agent.streams.started.lock().unwrap().len() == 1).await;

        // Hyprland exits: the capture stops, the output is gone with it.
        agent.hypr.set_down(true, Vec::new());
        agent.streams.end_last();
        tokio::time::sleep(RETRY * 2).await;
        assert!(agent.hypr.monitor(output::NAME).is_none());

        // A new Hyprland starts with only the laptop's screen.
        agent
            .hypr
            .set_down(false, vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        until(|| agent.streams.started.lock().unwrap().len() == 2).await;
        assert_eq!(agent.hypr.monitor(output::NAME).unwrap().width, 1920);
    }

    #[tokio::test]
    async fn a_reload_gets_the_monitor_rule_back() {
        let agent = agent();
        agent.inbox.send(start()).unwrap();
        until(|| agent.hypr.monitor(output::NAME).is_some()).await;
        agent.hypr.reload();
        assert_eq!(agent.hypr.monitor(output::NAME).unwrap().scale, 2.0);
        agent.events.send(hypr::Event::ConfigReloaded).unwrap();
        until(|| agent.hypr.monitor(output::NAME).unwrap().scale == 1.0).await;
    }
}
