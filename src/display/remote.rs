//! The agent's side of the display: on home's order, make the headless output and stream it;
//! take it away when home says stop or the link drops. While a mode is active, a reload gets
//! the monitor rule back and a Hyprland restart (or a vanished output) gets the output back.
//!
//! Home's input is injected on that output while its stream runs: the injector starts and
//! stops with the stream, so it always points at the output that is there, and it is gone
//! (every key and button released) before the output is removed.

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
use crate::input::{self, Inject, Injectors};

/// The first wait before a lost stream is started again, doubled after each failure up to
/// [`LONGEST_WAIT`].
const RETRY: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(1)
};
const LONGEST_WAIT: Duration = Duration::from_secs(30);
/// Failures in a row before home is told the mode cannot go on: about a minute of waits, long
/// enough for Hyprland to restart.
const ATTEMPTS: u32 = 6;
/// A stream that ran this long was not failing: the count starts again.
const STEADY: Duration = Duration::from_secs(30);

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

/// Serves every link the agent brings up, one after the other. An output left by an agent that
/// crashed goes first: no window must stay on a screen nobody sees.
pub async fn serve(
    mut peers: UnboundedReceiver<Peer>,
    hypr: Arc<dyn Hypr>,
    streams: Arc<dyn Streams>,
    injectors: Arc<dyn Injectors>,
    mut events: UnboundedReceiver<hypr::Event>,
) {
    let removed = {
        let hypr = hypr.clone();
        tokio::task::spawn_blocking(move || output::remove(&*hypr)).await
    };
    match removed {
        Ok(Err(e)) if e.kind() != io::ErrorKind::NotFound => {
            eprintln!("navette: could not remove a leftover remote output: {e}");
        }
        _ => {}
    }
    while let Some(peer) = peers.recv().await {
        let ports = Ports {
            hypr: hypr.clone(),
            streams: streams.clone(),
            injectors: injectors.clone(),
        };
        serve_peer(peer, &ports, &mut events).await;
    }
}

/// What the agent drives: Hyprland, the picture path and the input injection.
#[derive(Clone)]
pub struct Ports {
    pub hypr: Arc<dyn Hypr>,
    pub streams: Arc<dyn Streams>,
    pub injectors: Arc<dyn Injectors>,
}

/// What home said of its input, kept for the next injector: its keymap, its surface's size
/// and its locks.
#[derive(Default)]
struct Home {
    keymap: Option<String>,
    area: Option<input::Event>,
    locked: Option<input::Event>,
}

/// An active mode: the copy asked for, and its stream when it runs.
struct Active {
    copy: OutputCopy,
    caps: ClientCaps,
    /// Failures in a row: streams that would not start, or stopped before [`STEADY`].
    failures: u32,
    told_no_input: bool,
    stream: Stream,
}

impl Active {
    fn run(&mut self, running: Running, outbox: &Outbox) {
        if let Err(reason) = &running.input
            && !self.told_no_input
        {
            self.told_no_input = true;
            outbox.send(Msg::NoInput(reason.clone()), Vec::new());
        }
        self.stream = Stream::Running(running);
    }
}

enum Stream {
    Running(Running),
    /// Lost; started again at this instant, kept across loop turns.
    Lost(tokio::time::Instant),
}

struct Running {
    // Field order is drop order: the capture, then the injector, both before the output goes
    // (Hyprland 0.56 aborts when a cursor capture outlives its monitor).
    _stream: Box<dyn Send + Sync>,
    /// Without input, the picture still runs: home is told why, once a mode.
    input: Result<Box<dyn Inject>, String>,
    feedback: UnboundedSender<ClientMsg>,
    ended: oneshot::Receiver<io::Error>,
    since: tokio::time::Instant,
}

/// One link, until it drops; the output never outlives it. Hyprland requests and the stream's
/// start and stop block, so they run off the executor thread.
pub async fn serve_peer(
    mut peer: Peer,
    ports: &Ports,
    events: &mut UnboundedReceiver<hypr::Event>,
) {
    let hypr = &ports.hypr;
    let mut active: Option<Active> = None;
    let mut home = Home::default();
    let mut input_open = true;
    loop {
        let retry_at = match &active {
            Some(Active {
                stream: Stream::Lost(at),
                ..
            }) => Some(*at),
            _ => None,
        };
        tokio::select! {
            msg = peer.inbox.recv() => match msg {
                None => break,
                Some(Incoming::Display(Display::Start { output, caps })) => {
                    stop(&mut active, hypr).await;
                    let started = bring_up(&output, &caps, ports, &home, &peer.outbox).await;
                    let reply = match started {
                        Ok(running) => {
                            let mut mode = Active {
                                copy: output,
                                caps,
                                failures: 0,
                                told_no_input: false,
                                stream: Stream::Lost(tokio::time::Instant::now()),
                            };
                            mode.run(running, &peer.outbox);
                            active = Some(mode);
                            Display::Started { output: output::NAME.into() }
                        }
                        Err(e) => {
                            remove(hypr).await;
                            Display::Failed { reason: e.to_string() }
                        }
                    };
                    peer.outbox.send(Msg::Display(reply), Vec::new());
                }
                Some(Incoming::Display(Display::Stop)) => stop(&mut active, hypr).await,
                Some(Incoming::Feedback(msg)) => {
                    if let Some(Active { stream: Stream::Running(running), .. }) = &active {
                        let _ = running.feedback.send(msg);
                    }
                }
                Some(_) => {}
            },
            event = peer.input.from_peer.next(), if input_open => match event {
                // A compositor's keymap is compiled whole; an `include` would have this
                // machine's xkbcommon read files of its own for home.
                Some(input::Event::Keymap(text)) if text.contains("include") => {
                    eprintln!("navette: home's keymap includes files: refused, the last one stays");
                }
                Some(event) => {
                    match &event {
                        input::Event::Keymap(text) => home.keymap = Some(text.clone()),
                        input::Event::Area { .. } => home.area = Some(event.clone()),
                        input::Event::Locked(_) => home.locked = Some(event.clone()),
                        _ => {}
                    }
                    if let Some(Active { stream: Stream::Running(running), .. }) = &active
                        && let Ok(input) = &running.input
                    {
                        input.send(&event);
                    }
                }
                None => input_open = false,
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
                let Some(mode) = &mut active else { continue };
                let lost = std::mem::replace(&mut mode.stream, Stream::Lost(tokio::time::Instant::now()));
                if let Stream::Running(running) = lost {
                    if running.since.elapsed() >= STEADY {
                        mode.failures = 0;
                    }
                    // Its thread is joined: not on the executor.
                    let _ = tokio::task::spawn_blocking(move || drop(running)).await;
                }
                eprintln!("navette: the stream stopped ({reason})");
                failed(&mut active, reason, hypr, &peer.outbox).await;
            }
            () = sleep_until(retry_at), if retry_at.is_some() => {
                let Some(mode) = &mut active else { continue };
                match bring_up(&mode.copy, &mode.caps, ports, &home, &peer.outbox).await {
                    Ok(running) => mode.run(running, &peer.outbox),
                    Err(e) => {
                        eprintln!("navette: the stream could not start again ({e})");
                        failed(&mut active, e, hypr, &peer.outbox).await;
                    }
                }
            }
        }
    }
    stop(&mut active, hypr).await;
}

async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Counts one more failure: waits longer before the next attempt, or gives up and tells home.
async fn failed(
    active: &mut Option<Active>,
    reason: io::Error,
    hypr: &Arc<dyn Hypr>,
    outbox: &Outbox,
) {
    let Some(mode) = active else { return };
    mode.failures += 1;
    if mode.failures >= ATTEMPTS {
        let reason = format!(
            "the remote picture failed {} times in a row, the last: {reason}",
            mode.failures
        );
        eprintln!("navette: {reason}; the mode ends");
        stop(active, hypr).await;
        outbox.send(Msg::Display(Display::Failed { reason }), Vec::new());
        return;
    }
    let wait = RETRY
        .saturating_mul(1 << (mode.failures - 1).min(16))
        .min(LONGEST_WAIT);
    mode.stream = Stream::Lost(tokio::time::Instant::now() + wait);
}

/// Why a running stream stopped on its own; pending forever when none runs. A stream that
/// ended without a word ended all the same.
async fn ended(active: &mut Option<Active>) -> Option<io::Error> {
    match active {
        Some(Active {
            stream: Stream::Running(running),
            ..
        }) => Some(
            (&mut running.ended)
                .await
                .unwrap_or_else(|_| io::Error::other("the stream ended")),
        ),
        _ => std::future::pending().await,
    }
}

/// Starts the stream and the injector, on the output if it is still there as asked, else on a
/// new one. A panic in the encoder's or the capture's start is a failure like another.
async fn bring_up(
    copy: &OutputCopy,
    caps: &ClientCaps,
    ports: &Ports,
    home: &Home,
    outbox: &Outbox,
) -> io::Result<Running> {
    let (copy, caps, ports, outbox) = (copy.clone(), caps.clone(), ports.clone(), outbox.clone());
    let keymap = home.keymap.clone();
    let replay: Vec<_> = [&home.area, &home.locked]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    tokio::task::spawn_blocking(move || {
        start_blocking(&copy, caps, &ports, keymap.as_deref(), &replay, outbox)
    })
    .await
    .unwrap_or_else(|e| {
        Err(io::Error::other(format!(
            "the picture's start crashed: {e}"
        )))
    })
}

fn start_blocking(
    copy: &OutputCopy,
    caps: ClientCaps,
    ports: &Ports,
    keymap: Option<&str>,
    replay: &[input::Event],
    outbox: Outbox,
) -> io::Result<Running> {
    let hypr = &*ports.hypr;
    let scale = match output::live(hypr, copy)? {
        Some(scale) => scale,
        None => output::create(hypr, copy)?,
    };
    let (feedback, feedback_rx) = mpsc::unbounded_channel();
    let (ended_tx, ended) = oneshot::channel();
    let source = Source {
        output: output::NAME.into(),
        scale,
    };
    let stream = ports
        .streams
        .start(source, caps, outbox, feedback_rx, ended_tx)?;
    let input = ports
        .injectors
        .start(output::NAME, keymap)
        .inspect(|input| {
            for event in replay {
                input.send(event);
            }
        })
        .map_err(|e| {
            eprintln!(
                "navette: home's input cannot be injected, the picture goes on without it: {e}"
            );
            e.to_string()
        });
    Ok(Running {
        _stream: stream,
        input,
        feedback,
        ended,
        since: tokio::time::Instant::now(),
    })
}

/// Stops the stream first, then removes the output it captured.
async fn stop(active: &mut Option<Active>, hypr: &Arc<dyn Hypr>) {
    let Some(mode) = active.take() else {
        return;
    };
    let hypr = hypr.clone();
    let removed = tokio::task::spawn_blocking(move || {
        drop(mode);
        output::remove(&*hypr)
    })
    .await;
    if let Ok(Err(e)) = removed {
        eprintln!("navette: could not remove the remote output: {e}");
    }
}

async fn remove(hypr: &Arc<dyn Hypr>) {
    let hypr = hypr.clone();
    if let Ok(Err(e)) = tokio::task::spawn_blocking(move || output::remove(&*hypr)).await {
        eprintln!("navette: could not remove the remote output: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use gliff_proto::{ChromaMode, Codec, ServerMsg};

    use super::*;
    use crate::hypr::fake::{FakeHypr, FakeMonitor};

    /// Streams that send one config message, and can be ended from the test. Each one says,
    /// when dropped, on which thread and whether its output was still there.
    struct FakeStreams {
        hypr: Arc<FakeHypr>,
        ends: Mutex<Vec<oneshot::Sender<io::Error>>>,
        started: Mutex<Vec<(String, f32)>>,
        refuse: Mutex<bool>,
        panic: Mutex<bool>,
        dropped: Arc<Mutex<Vec<Dropped>>>,
    }

    #[derive(Debug)]
    struct Dropped {
        thread: std::thread::ThreadId,
        output_there: bool,
    }

    struct FakeStream {
        hypr: Arc<FakeHypr>,
        dropped: Arc<Mutex<Vec<Dropped>>>,
    }

    impl Drop for FakeStream {
        fn drop(&mut self) {
            self.dropped.lock().unwrap().push(Dropped {
                thread: std::thread::current().id(),
                output_there: self.hypr.monitor(output::NAME).is_some(),
            });
        }
    }

    impl FakeStreams {
        fn new(hypr: Arc<FakeHypr>) -> Self {
            Self {
                hypr,
                ends: Mutex::default(),
                started: Mutex::default(),
                refuse: Mutex::default(),
                panic: Mutex::default(),
                dropped: Arc::default(),
            }
        }
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
            if *self.panic.lock().unwrap() {
                panic!("the encoder library crashed");
            }
            if *self.refuse.lock().unwrap() {
                return Err(io::Error::other("no encoder"));
            }
            self.started
                .lock()
                .unwrap()
                .push((source.output, source.scale));
            self.ends.lock().unwrap().push(ended);
            outbox.send(Msg::Video(ServerMsg::Ping { t: 1 }), Vec::new());
            Ok(Box::new(FakeStream {
                hypr: self.hypr.clone(),
                dropped: self.dropped.clone(),
            }))
        }
    }

    impl FakeStreams {
        fn drop_last(&self) {
            drop(self.ends.lock().unwrap().pop().unwrap());
        }

        fn end_last(&self) {
            let end = self.ends.lock().unwrap().pop().unwrap();
            let _ = end.send(io::Error::other("capture stopped"));
        }

        fn count(&self) -> usize {
            self.started.lock().unwrap().len()
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

    /// Injectors that record what each one got, and when it went whether its output was there.
    struct FakeInjectors {
        hypr: Arc<FakeHypr>,
        started: Arc<Mutex<Vec<Injected>>>,
        /// No virtual keyboard to be had.
        refuse: Arc<Mutex<bool>>,
    }

    #[derive(Debug, Default)]
    struct Injected {
        output: String,
        keymap: Option<String>,
        events: Vec<input::Event>,
        /// Dropped, with its output still there.
        dropped: Option<bool>,
    }

    struct FakeInject {
        hypr: Arc<FakeHypr>,
        started: Arc<Mutex<Vec<Injected>>>,
        index: usize,
    }

    impl Injectors for FakeInjectors {
        fn start(&self, output: &str, keymap: Option<&str>) -> io::Result<Box<dyn Inject>> {
            if *self.refuse.lock().unwrap() {
                return Err(io::Error::other("no virtual keyboard"));
            }
            let mut started = self.started.lock().unwrap();
            started.push(Injected {
                output: output.into(),
                keymap: keymap.map(str::to_string),
                ..Default::default()
            });
            Ok(Box::new(FakeInject {
                hypr: self.hypr.clone(),
                started: self.started.clone(),
                index: started.len() - 1,
            }))
        }
    }

    impl Inject for FakeInject {
        fn send(&self, event: &input::Event) {
            self.started.lock().unwrap()[self.index]
                .events
                .push(event.clone());
        }
    }

    impl Drop for FakeInject {
        fn drop(&mut self) {
            let there = self.hypr.monitor(output::NAME).is_some();
            self.started.lock().unwrap()[self.index].dropped = Some(there);
        }
    }

    struct Agent {
        inbox: mpsc::Sender<Incoming>,
        outbox: Outbox,
        input: input::Queue,
        events: UnboundedSender<hypr::Event>,
        hypr: Arc<FakeHypr>,
        streams: Arc<FakeStreams>,
        injected: Arc<Mutex<Vec<Injected>>>,
        refuse_input: Arc<Mutex<bool>>,
        task: tokio::task::JoinHandle<()>,
    }

    fn agent() -> Agent {
        let hypr = Arc::new(FakeHypr::with(vec![FakeMonitor::new(
            "eDP-1", 1920, 1200, 0,
        )]));
        let streams = Arc::new(FakeStreams::new(hypr.clone()));
        let injected = Arc::new(Mutex::new(Vec::new()));
        let refuse_input = Arc::new(Mutex::new(false));
        let (inbox, inbox_rx) = mpsc::channel(64);
        let (events, mut events_rx) = mpsc::unbounded_channel();
        let outbox = Outbox::default();
        let wire = input::Wire::default();
        let peer = Peer {
            outbox: outbox.clone(),
            inbox: inbox_rx,
            pointer: tokio::sync::watch::channel(Default::default()).1,
            input: wire.clone(),
        };
        let ports = Ports {
            hypr: hypr.clone(),
            streams: streams.clone(),
            injectors: Arc::new(FakeInjectors {
                hypr: hypr.clone(),
                started: injected.clone(),
                refuse: refuse_input.clone(),
            }),
        };
        let task = tokio::spawn(async move {
            serve_peer(peer, &ports, &mut events_rx).await;
        });
        Agent {
            inbox,
            outbox,
            input: wire.from_peer,
            events,
            hypr,
            streams,
            injected,
            refuse_input,
            task,
        }
    }

    impl Agent {
        fn injected(&self, index: usize) -> Option<(String, Option<String>, Vec<input::Event>)> {
            let injected = self.injected.lock().unwrap();
            injected
                .get(index)
                .map(|i| (i.output.clone(), i.keymap.clone(), i.events.clone()))
        }

        fn dropped(&self, index: usize) -> Option<bool> {
            self.injected.lock().unwrap().get(index)?.dropped
        }
    }

    const KEY_T: u32 = 20;

    fn key(pressed: bool) -> input::Event {
        input::Event::Key {
            code: KEY_T,
            pressed,
        }
    }

    #[tokio::test]
    async fn home_s_input_is_injected_on_the_remote_output_with_home_s_keymap() {
        let agent = agent();
        // Home's surface says its keymap and size before the remote output exists.
        agent.input.push(input::Event::Keymap("home keymap".into()));
        let area = input::Event::Area {
            width: 1280,
            height: 720,
        };
        agent.input.push(area.clone());
        tokio::time::sleep(Duration::from_millis(50)).await;
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.injected(0).is_some()).await;
        agent.input.push(key(true));
        until(|| agent.injected(0).unwrap().2.len() == 2).await;
        let (output, keymap, events) = agent.injected(0).unwrap();
        assert_eq!(output, output::NAME);
        assert_eq!(keymap.as_deref(), Some("home keymap"));
        assert_eq!(events, [area, key(true)]);
    }

    #[tokio::test]
    async fn a_dropped_link_releases_everything_before_the_output_goes() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.injected(0).is_some()).await;
        agent.input.push(key(true));
        until(|| agent.injected(0).unwrap().2.len() == 1).await;
        let Agent {
            inbox,
            task,
            hypr,
            injected,
            ..
        } = agent;
        drop(inbox);
        task.await.unwrap();
        // Dropping the injector releases what it holds; the output was still there for it.
        assert_eq!(injected.lock().unwrap()[0].dropped, Some(true));
        assert!(hypr.monitor(output::NAME).is_none());
    }

    #[tokio::test]
    async fn stopping_the_mode_releases_everything_before_the_output_goes() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.injected(0).is_some()).await;
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| agent.dropped(0).is_some()).await;
        assert_eq!(agent.dropped(0), Some(true));
    }

    fn no_input(outbox: &Outbox) -> Vec<String> {
        outbox
            .queued()
            .into_iter()
            .filter_map(|msg| match msg {
                Msg::NoInput(reason) => Some(reason),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn an_injector_that_cannot_start_leaves_the_picture_and_tells_home_once() {
        let agent = agent();
        *agent.refuse_input.lock().unwrap() = true;
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        until(|| no_input(&agent.outbox).len() == 1).await;
        assert!(no_input(&agent.outbox)[0].contains("no virtual keyboard"));
        assert!(
            agent
                .outbox
                .queued()
                .contains(&Msg::Display(Display::Started {
                    output: output::NAME.into()
                }))
        );
        // The stream starts again after a stop of its own: still a picture, no second word.
        agent.streams.end_last();
        until(|| agent.streams.count() == 2).await;
        assert!(failed(&agent.outbox).is_none());
        assert_eq!(no_input(&agent.outbox).len(), 1);
        assert!(agent.hypr.monitor(output::NAME).is_some());
    }

    #[tokio::test]
    async fn the_injector_starts_again_with_the_stream_and_keeps_home_s_keymap_and_locks() {
        let agent = agent();
        let caps = input::Event::Locked(vec!["Lock".into()]);
        agent.input.push(input::Event::Keymap("home keymap".into()));
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.injected(0).is_some()).await;
        agent.input.push(caps.clone());
        until(|| agent.injected(0).unwrap().2.contains(&caps)).await;
        agent.streams.end_last();
        until(|| agent.injected(1).is_some()).await;
        assert_eq!(agent.dropped(0), Some(true));
        assert_eq!(agent.injected(1).unwrap().1.as_deref(), Some("home keymap"));
        until(|| agent.injected(1).unwrap().2 == [caps.clone()]).await;
        // The new one gets what follows; the old one, gone, nothing.
        agent.input.push(key(false));
        until(|| agent.injected(1).unwrap().2 == [caps.clone(), key(false)]).await;
        assert!(!agent.injected(0).unwrap().2.contains(&key(false)));
    }

    #[tokio::test]
    async fn a_keymap_that_includes_files_is_refused() {
        let agent = agent();
        let home = input::Event::Keymap("home keymap".into());
        let including = r#"xkb_keymap { xkb_symbols { include "pc+us" }; };"#;
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.injected(0).is_some()).await;
        agent.input.push(home.clone());
        // Queued together, the newer keymap would replace it before the agent reads it.
        until(|| agent.injected(0).unwrap().2 == [home.clone()]).await;
        agent.input.push(input::Event::Keymap(including.into()));
        agent.input.push(key(true));
        until(|| agent.injected(0).unwrap().2 == [home.clone(), key(true)]).await;
        // Nor is it kept for the next injector.
        agent.streams.end_last();
        until(|| agent.injected(1).is_some()).await;
        assert_eq!(agent.injected(1).unwrap().1.as_deref(), Some("home keymap"));
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
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.hypr.monitor(output::NAME).is_some()).await;
        until(|| agent.streams.count() == 1).await;
        assert_eq!(agent.streams.started.lock().unwrap()[0].0, output::NAME);
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
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.hypr.monitor(output::NAME).is_some()).await;
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| agent.hypr.monitor(output::NAME).is_none()).await;
        assert_eq!(
            agent.hypr.changes().last().unwrap(),
            "output remove NAVETTE-1"
        );
    }

    #[tokio::test]
    async fn a_dropped_link_removes_the_remote_output() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.hypr.monitor(output::NAME).is_some()).await;
        drop(agent.inbox);
        agent.task.await.unwrap();
        assert!(agent.hypr.monitor(output::NAME).is_none());
    }

    #[tokio::test]
    async fn a_stream_that_cannot_start_leaves_no_output_and_says_why() {
        let agent = agent();
        *agent.streams.refuse.lock().unwrap() = true;
        agent.inbox.try_send(start()).unwrap();
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
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;

        // Hyprland exits: the capture stops, the output is gone with it.
        agent.hypr.set_down(true, Vec::new());
        agent.streams.end_last();
        tokio::time::sleep(RETRY * 2).await;
        assert!(agent.hypr.monitor(output::NAME).is_none());

        // A new Hyprland starts with only the laptop's screen.
        agent
            .hypr
            .set_down(false, vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        until(|| agent.streams.count() == 2).await;
        assert_eq!(agent.hypr.monitor(output::NAME).unwrap().width, 1920);
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_a_reason_is_started_again() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        agent.streams.drop_last();
        until(|| agent.streams.count() == 2).await;
        assert!(!agent.task.is_finished());
    }

    fn created(hypr: &FakeHypr) -> usize {
        hypr.changes()
            .iter()
            .filter(|c| c.starts_with("output create"))
            .count()
    }

    fn failed(outbox: &Outbox) -> Option<String> {
        outbox.queued().into_iter().find_map(|msg| match msg {
            Msg::Display(Display::Failed { reason }) => Some(reason),
            _ => None,
        })
    }

    #[tokio::test]
    async fn a_lost_stream_starts_again_on_the_same_output() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        agent.streams.end_last();
        until(|| agent.streams.count() == 2).await;
        // The output stayed: the windows on it were not sent to the laptop's screen and back.
        assert_eq!(created(&agent.hypr), 1);
    }

    #[tokio::test]
    async fn a_stream_that_keeps_failing_tells_home_and_removes_the_output() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        *agent.streams.refuse.lock().unwrap() = true;
        agent.streams.end_last();
        until(|| failed(&agent.outbox).is_some()).await;
        assert!(
            failed(&agent.outbox).unwrap().contains("no encoder"),
            "{:?}",
            failed(&agent.outbox)
        );
        until(|| agent.hypr.monitor(output::NAME).is_none()).await;
        assert_eq!(created(&agent.hypr), 1);
        assert!(!agent.task.is_finished());
    }

    #[tokio::test]
    async fn a_storm_of_monitor_events_does_not_hold_the_retry_back() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        agent.streams.end_last();
        let storm = async {
            loop {
                let _ = agent.events.send(hypr::Event::MonitorAdded("DP-9".into()));
                tokio::time::sleep(RETRY / 4).await;
            }
        };
        tokio::select! {
            () = storm => {}
            () = until(|| agent.streams.count() == 2) => {}
        }
    }

    #[tokio::test]
    async fn a_leftover_output_is_removed_when_the_agent_starts() {
        let hypr = Arc::new(FakeHypr::with(vec![
            FakeMonitor::new("eDP-1", 1920, 1200, 0),
            FakeMonitor::new(output::NAME, 1920, 1080, 1920),
        ]));
        let (_peers, peers_rx) = mpsc::unbounded_channel();
        let (_events, events_rx) = mpsc::unbounded_channel();
        let streams = Arc::new(FakeStreams::new(hypr.clone()));
        let injectors = Arc::new(FakeInjectors {
            hypr: hypr.clone(),
            started: Arc::default(),
            refuse: Arc::default(),
        });
        tokio::spawn(serve(peers_rx, hypr.clone(), streams, injectors, events_rx));
        until(|| hypr.monitor(output::NAME).is_none()).await;
    }

    #[tokio::test]
    async fn the_stream_gets_the_scale_hyprland_chose() {
        let agent = agent();
        agent.hypr.choose_scale(1.2);
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        assert_eq!(agent.streams.started.lock().unwrap()[0].1, 1.2);
    }

    #[tokio::test]
    async fn a_stream_is_stopped_off_the_executor_before_its_output_goes() {
        let executor = std::thread::current().id();
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        agent.streams.end_last();
        until(|| agent.streams.dropped.lock().unwrap().len() == 1).await;
        until(|| agent.streams.count() == 2).await;
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| agent.streams.dropped.lock().unwrap().len() == 2).await;
        let dropped = agent.streams.dropped.lock().unwrap();
        assert!(dropped.iter().all(|d| d.thread != executor), "{dropped:?}");
        assert!(dropped[1].output_there, "{dropped:?}");
    }

    #[tokio::test]
    async fn a_stream_start_that_panics_is_a_failure_and_the_agent_goes_on() {
        let agent = agent();
        *agent.streams.panic.lock().unwrap() = true;
        agent.inbox.try_send(start()).unwrap();
        until(|| failed(&agent.outbox).is_some()).await;
        assert!(agent.hypr.monitor(output::NAME).is_none());
        *agent.streams.panic.lock().unwrap() = false;
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
    }

    #[tokio::test]
    async fn a_reload_gets_the_monitor_rule_back() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.hypr.monitor(output::NAME).is_some()).await;
        agent.hypr.reload();
        assert_eq!(agent.hypr.monitor(output::NAME).unwrap().scale, 2.0);
        agent.events.send(hypr::Event::ConfigReloaded).unwrap();
        until(|| agent.hypr.monitor(output::NAME).unwrap().scale == 1.0).await;
    }
}
