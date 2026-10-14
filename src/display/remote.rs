//! The agent's side of the display: on home's order, make the headless outputs and stream
//! each; take them away when home says stop or the link drops. While a mode is active, a
//! reload gets the monitor rules back and a Hyprland restart (or a vanished output) gets an
//! output back.
//!
//! Each output lives on its own: its stream, its injector, its failures and its waits before
//! it is started again. When one has failed [`ATTEMPTS`] times in a row, the whole mode ends:
//! home cannot show a desk with a hole in it.
//!
//! An output's start blocks (Hyprland requests, the encoder's and the capture's start), so it
//! runs as a task while the loop goes on serving the outputs that run: one start at a time,
//! leftmost first, as each output's place depends on the size of the ones left of it.
//!
//! Home's input is injected on an output while its stream runs: the injector starts and stops
//! with the stream, so it always points at the output that is there, and it is gone (every key
//! and button released) before the output is removed.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use gliff_proto::{ClientCaps, ClientMsg};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use super::output;
use super::stream::{self, Source};
use crate::display::wire::{Display, Incoming, MAX_OUTPUTS, Msg, Outbox, OutputCopy, Peer};
use crate::hypr::{self, Hypr};
use crate::input::{self, Inject, Injectors};
use crate::lid::{self, Logind};

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

/// Serves every link the agent brings up, one after the other. What an agent that crashed
/// left goes first: no window must stay on a screen nobody sees, and the laptop's own screen,
/// disabled for a closed lid, comes back.
pub async fn serve(
    mut peers: UnboundedReceiver<Peer>,
    ports: Ports,
    mut events: UnboundedReceiver<hypr::Event>,
) {
    let (hypr, record) = (ports.hypr.clone(), ports.lid_record.clone());
    let left = tokio::task::spawn_blocking(move || {
        // The laptop's screen first: the copies may be the only outputs on.
        (
            restore(&*hypr, record.as_deref()),
            output::remove_all(&*hypr),
        )
    })
    .await;
    if let Ok((opened, removed)) = left {
        said_restored(opened);
        match removed {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                eprintln!("navette: could not remove a leftover remote output: {e}");
            }
            _ => {}
        }
    }
    while let Some(peer) = peers.recv().await {
        serve_peer(peer, &ports, &mut events).await;
    }
}

/// What the agent drives: Hyprland, the picture path, the input injection and logind, with
/// the file that brings the laptop's screen back (none without a runtime directory) and how
/// often the lid is read.
#[derive(Clone)]
pub struct Ports {
    pub hypr: Arc<dyn Hypr>,
    pub streams: Arc<dyn Streams>,
    pub injectors: Arc<dyn Injectors>,
    pub logind: Arc<dyn Logind>,
    pub lid_record: Option<std::path::PathBuf>,
    pub lid_poll: Duration,
}

/// Puts the laptop's screen back if a record says the agent disabled it.
fn restore(hypr: &dyn Hypr, record: Option<&std::path::Path>) -> io::Result<()> {
    record.map_or(Ok(()), |record| lid::open(hypr, record))
}

fn said_restored(restored: io::Result<()>) {
    match restored {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            eprintln!("navette: could not bring the laptop's screen back: {e}");
        }
        _ => {}
    }
}

/// [`restore`], off the executor: at a start, and on Hyprland's events with no lid watched, so
/// a restore that failed is tried again.
async fn restore_now(ports: &Ports) {
    let (hypr, record) = (ports.hypr.clone(), ports.lid_record.clone());
    if let Ok(restored) =
        tokio::task::spawn_blocking(move || restore(&*hypr, record.as_deref())).await
    {
        said_restored(restored);
    }
}

/// What home said of its input, kept for the next injector: its keymap, each surface's size
/// and its locks.
#[derive(Default)]
struct Home {
    keymap: Option<String>,
    areas: [Option<input::Event>; MAX_OUTPUTS as usize],
    locked: Option<input::Event>,
}

/// An active mode: the copies asked for, in home's order, and each one's stream.
struct Active {
    copies: Vec<OutputCopy>,
    /// The scale Hyprland chose for each output made, which places the ones right of it.
    scales: Vec<Option<f32>>,
    caps: ClientCaps,
    outputs: Vec<Output>,
    told_no_input: bool,
    /// Until every output has run once: home waits for its answer, and a failure ends it.
    starting: bool,
    /// The output being started, and its start.
    up: Option<(usize, tokio::task::JoinHandle<io::Result<Running>>)>,
    /// With `closed_lid = on`: the lid's watch.
    lid: Option<lid::Watch>,
    /// `closed_lid = on` was asked, said back in `Started`.
    closed_lid: bool,
}

struct Output {
    /// Failures in a row: streams that would not start, or stopped before [`STEADY`].
    failures: u32,
    stream: Stream,
}

impl Active {
    /// Gives output `index` its stream, its injector getting what home said of its surface
    /// and its locks, up to now.
    fn run(&mut self, index: usize, running: Running, home: &Home, outbox: &Outbox) {
        match &running.input {
            Ok(input) => {
                for event in [&home.areas[index], &home.locked].into_iter().flatten() {
                    input.send(event);
                }
            }
            Err(reason) if !self.told_no_input => {
                self.told_no_input = true;
                outbox.send(Msg::NoInput(reason.clone()), Vec::new());
            }
            Err(_) => {}
        }
        self.scales[index] = Some(running.scale);
        self.outputs[index].stream = Stream::Running(running);
    }

    /// Starts output `index`'s stream and injector as a task, on the output if it is still
    /// there as asked, else on a new one. A panic in the encoder's or the capture's start is a
    /// failure like another.
    fn bring_up(&mut self, index: usize, ports: &Ports, home: &Home, outbox: &Outbox) {
        let (copies, scales, caps, ports) = (
            self.copies.clone(),
            self.scales.clone(),
            self.caps.clone(),
            ports.clone(),
        );
        let outbox = outbox.on(index as u8);
        let keymap = home.keymap.clone();
        self.outputs[index].stream = Stream::Starting;
        self.up = Some((
            index,
            tokio::task::spawn_blocking(move || {
                start_blocking(
                    (&copies, &scales),
                    index,
                    caps,
                    &ports,
                    keymap.as_deref(),
                    outbox,
                )
            }),
        ));
    }

    fn running(&self, index: usize) -> Option<&Running> {
        match &self.outputs.get(index)?.stream {
            Stream::Running(running) => Some(running),
            Stream::Lost(_) | Stream::Starting => None,
        }
    }

    /// The earliest instant an output waits for to start again, while none is starting.
    fn retry_at(&self) -> Option<tokio::time::Instant> {
        if self.starting || self.up.is_some() {
            return None;
        }
        self.outputs
            .iter()
            .filter_map(|o| match o.stream {
                Stream::Lost(at) => Some(at),
                Stream::Running(_) | Stream::Starting => None,
            })
            .min()
    }
}

enum Stream {
    Running(Running),
    /// Lost; started again at this instant, kept across loop turns.
    Lost(tokio::time::Instant),
    Starting,
}

/// What a running mode's tasks bring to the loop.
enum Landed {
    /// A running stream stopped on its own, and why.
    Ended(usize, io::Error),
    /// An output's start is done.
    Up(usize, io::Result<Running>),
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
    /// The scale Hyprland chose for the output.
    scale: f32,
}

/// One link, until it drops; the outputs never outlive it. Hyprland requests and the streams'
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
        let retry_at = active.as_ref().and_then(Active::retry_at);
        let had_mode = active.is_some();
        tokio::select! {
            msg = peer.inbox.recv() => match msg {
                None => break,
                Some(Incoming::Display(Display::Start { output, caps, others, closed_lid })) => {
                    stop(&mut active, ports).await;
                    restore_now(ports).await;
                    let mut copies = vec![output];
                    copies.extend(others.unwrap_or_default());
                    match start(copies, caps) {
                        Ok(mut mode) => {
                            if closed_lid == Some(true) {
                                mode.closed_lid = true;
                                mode.lid = watch_lid(ports, &peer.outbox);
                            }
                            mode.bring_up(0, ports, &home, &peer.outbox);
                            active = Some(mode);
                        }
                        Err(reason) => peer
                            .outbox
                            .send(Msg::Display(Display::Failed { reason }), Vec::new()),
                    }
                }
                Some(Incoming::Display(Display::Stop)) => stop(&mut active, ports).await,
                Some(Incoming::Feedback { output, msg }) => {
                    if let Some(running) = active.as_ref().and_then(|a| a.running(output.into())) {
                        let _ = running.feedback.send(msg);
                    }
                }
                Some(_) => {}
            },
            event = peer.input.from_peer.next(), if input_open => match event {
                // A compositor's keymap is compiled whole; an `include` would have this
                // machine's xkbcommon read files of its own for home.
                Some((_, input::Event::Keymap(text))) if text.contains("include") => {
                    eprintln!("navette: home's keymap includes files: refused, the last one stays");
                }
                Some((output, event)) => {
                    let index = usize::from(output);
                    match &event {
                        input::Event::Keymap(text) => home.keymap = Some(text.clone()),
                        input::Event::Area { .. } => home.areas[index] = Some(event.clone()),
                        input::Event::Locked(_) => home.locked = Some(event.clone()),
                        _ => {}
                    }
                    let Some(mode) = &active else { continue };
                    // Home's keymap is every injector's; the rest is for the output named.
                    let every = matches!(event, input::Event::Keymap(_));
                    for (i, o) in mode.outputs.iter().enumerate() {
                        if let Stream::Running(Running { input: Ok(input), .. }) = &o.stream
                            && (every || i == index)
                        {
                            input.send(&event);
                        }
                    }
                }
                None => input_open = false,
            },
            Some(event) = events.recv() => {
                // A reload, or a script on the lid, may have turned the laptop's screen on;
                // with no lid watched, a restore that failed may now go through.
                match active.as_ref().and_then(|a| a.lid.as_ref()) {
                    Some(watch) => watch.look(),
                    None => restore_now(ports).await,
                }
                if let (hypr::Event::ConfigReloaded, Some(mode)) = (event, &active) {
                    let (hypr, copies, scales) =
                        (hypr.clone(), mode.copies.clone(), mode.scales.clone());
                    let reapplied = tokio::task::spawn_blocking(move || {
                        output::reapply(&*hypr, &copies, &scales)
                    })
                    .await;
                    if let Ok(Err(e)) = reapplied {
                        eprintln!("navette: could not restore the outputs after a reload: {e}");
                    }
                }
            }
            landed = landed(&mut active) => match landed {
                Landed::Ended(index, reason) => {
                    let Some(mode) = &mut active else { continue };
                    let lost = std::mem::replace(
                        &mut mode.outputs[index].stream,
                        Stream::Lost(tokio::time::Instant::now()),
                    );
                    if let Stream::Running(running) = lost {
                        if running.since.elapsed() >= STEADY {
                            mode.outputs[index].failures = 0;
                        }
                        // Its thread is joined: not on the executor.
                        let _ = tokio::task::spawn_blocking(move || drop(running)).await;
                    }
                    eprintln!("navette: the stream of {} stopped ({reason})", output::name(index));
                    failed(&mut active, index, reason, ports, &peer.outbox).await;
                }
                Landed::Up(index, started) => {
                    let Some(mode) = &mut active else { continue };
                    match started {
                        Ok(running) => {
                            mode.run(index, running, &home, &peer.outbox);
                            let next = index + 1;
                            if mode.starting && next < mode.copies.len() {
                                mode.bring_up(next, ports, &home, &peer.outbox);
                            } else if mode.starting {
                                mode.starting = false;
                                let names: Vec<_> =
                                    (0..mode.copies.len()).map(output::name).collect();
                                let started = Display::Started {
                                    output: names[0].clone(),
                                    others: (names.len() > 1).then(|| names[1..].to_vec()),
                                    closed_lid: mode.closed_lid.then_some(true),
                                };
                                peer.outbox.send(Msg::Display(started), Vec::new());
                            }
                        }
                        Err(e) if mode.starting => {
                            stop(&mut active, ports).await;
                            let reason = Display::Failed { reason: start_failed(index, e) };
                            peer.outbox.send(Msg::Display(reason), Vec::new());
                        }
                        Err(e) => {
                            mode.outputs[index].stream = Stream::Lost(tokio::time::Instant::now());
                            eprintln!(
                                "navette: the stream of {} could not start again ({e})",
                                output::name(index)
                            );
                            failed(&mut active, index, e, ports, &peer.outbox).await;
                        }
                    }
                }
            },
            () = sleep_until(retry_at), if retry_at.is_some() => {
                let Some(mode) = &mut active else { continue };
                let now = tokio::time::Instant::now();
                // Leftmost first: the others wait for it to land.
                let due = (0..mode.outputs.len())
                    .find(|&i| matches!(mode.outputs[i].stream, Stream::Lost(at) if at <= now));
                if let Some(index) = due {
                    mode.bring_up(index, ports, &home, &peer.outbox);
                }
            }
        }
        // However it ended, the sizes said were its surfaces'. The next mode's are said before
        // its start, so they are kept until a mode ends.
        if had_mode && active.is_none() {
            home.areas = Default::default();
        }
    }
    stop(&mut active, ports).await;
}

/// Starts the lid's watch; what keeps it from working is said here and at home.
fn watch_lid(ports: &Ports, outbox: &Outbox) -> Option<lid::Watch> {
    let outbox = outbox.clone();
    let told = move |why: String| {
        eprintln!("navette: closed_lid: {why}; the lid does what it always does");
        outbox.send(Msg::NoLid(why), Vec::new());
    };
    match lid::Watch::start(
        ports.logind.clone(),
        ports.hypr.clone(),
        ports.lid_record.clone(),
        ports.lid_poll,
        told,
    ) {
        Ok(watch) => Some(watch),
        Err(e) => {
            eprintln!("navette: closed_lid: the lid cannot be watched: {e}");
            None
        }
    }
}

async fn sleep_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// A mode for these copies, none of them started yet.
fn start(copies: Vec<OutputCopy>, caps: ClientCaps) -> Result<Active, String> {
    if copies.len() > usize::from(MAX_OUTPUTS) {
        return Err(format!(
            "{} monitors asked, {MAX_OUTPUTS} at most",
            copies.len()
        ));
    }
    let now = tokio::time::Instant::now();
    Ok(Active {
        outputs: (0..copies.len())
            .map(|_| Output {
                failures: 0,
                stream: Stream::Lost(now),
            })
            .collect(),
        scales: vec![None; copies.len()],
        copies,
        caps,
        told_no_input: false,
        starting: true,
        up: None,
        lid: None,
        closed_lid: false,
    })
}

/// Why the mode could not start: output `index` would not.
fn start_failed(index: usize, e: io::Error) -> String {
    let name = output::name(index);
    // A GPU's encoder may take fewer sessions than there are outputs.
    if index > 0 {
        format!(
            "{name} could not stream, though the outputs left of it did \
             (the remote machine's encoder may not take one more): {e}"
        )
    } else {
        format!("{name}: {e}")
    }
}

/// Counts one more failure of output `index`: waits longer before its next attempt, or gives
/// up on the mode and tells home.
async fn failed(
    active: &mut Option<Active>,
    index: usize,
    reason: io::Error,
    ports: &Ports,
    outbox: &Outbox,
) {
    let Some(mode) = active else { return };
    let output = &mut mode.outputs[index];
    output.failures += 1;
    if output.failures >= ATTEMPTS {
        let reason = format!(
            "the picture of {} failed {} times in a row, the last: {reason}",
            output::name(index),
            output.failures
        );
        eprintln!("navette: {reason}; the mode ends");
        stop(active, ports).await;
        outbox.send(Msg::Display(Display::Failed { reason }), Vec::new());
        return;
    }
    let wait = RETRY
        .saturating_mul(1 << (output.failures - 1).min(16))
        .min(LONGEST_WAIT);
    output.stream = Stream::Lost(tokio::time::Instant::now() + wait);
}

/// The next thing a mode's tasks bring: an output's start done, or the first running stream
/// to stop on its own; pending forever when there is no mode. A stream that ended without a
/// word ended all the same.
async fn landed(active: &mut Option<Active>) -> Landed {
    let Some(mode) = active else {
        return std::future::pending().await;
    };
    std::future::poll_fn(|cx| {
        if let Some((index, start)) = &mut mode.up
            && let Poll::Ready(result) = Pin::new(start).poll(cx)
        {
            let index = *index;
            mode.up = None;
            let started = result.unwrap_or_else(|e| {
                Err(io::Error::other(format!(
                    "the picture's start crashed: {e}"
                )))
            });
            return Poll::Ready(Landed::Up(index, started));
        }
        for (index, output) in mode.outputs.iter_mut().enumerate() {
            if let Stream::Running(running) = &mut output.stream
                && let Poll::Ready(result) = Pin::new(&mut running.ended).poll(cx)
            {
                let reason = result.unwrap_or_else(|_| io::Error::other("the stream ended"));
                return Poll::Ready(Landed::Ended(index, reason));
            }
        }
        Poll::Pending
    })
    .await
}

fn start_blocking(
    (copies, scales): (&[OutputCopy], &[Option<f32>]),
    index: usize,
    caps: ClientCaps,
    ports: &Ports,
    keymap: Option<&str>,
    outbox: Outbox,
) -> io::Result<Running> {
    let hypr = &*ports.hypr;
    let name = output::name(index);
    let scale = match output::live(hypr, &copies[index], index)? {
        Some(scale) => scale,
        None => output::create(hypr, copies, scales, index)?,
    };
    let (feedback, feedback_rx) = mpsc::unbounded_channel();
    let (ended_tx, ended) = oneshot::channel();
    let source = Source {
        output: name.clone(),
        scale,
    };
    let stream = ports
        .streams
        .start(source, caps, outbox, feedback_rx, ended_tx)?;
    let input = ports.injectors.start(&name, keymap).map_err(|e| {
        eprintln!(
            "navette: home's input cannot be injected on {name}, the picture goes on \
                 without it: {e}"
        );
        e.to_string()
    });
    Ok(Running {
        _stream: stream,
        input,
        feedback,
        ended,
        since: tokio::time::Instant::now(),
        scale,
    })
}

/// Waits for the start under way, brings the laptop's screen back, stops every stream, then
/// removes the outputs they captured, shows the cursor, and releases logind's lock last.
async fn stop(active: &mut Option<Active>, ports: &Ports) {
    let Some(mut mode) = active.take() else {
        return;
    };
    // A start cannot be cut short: removed now, its output would be made again after.
    let starting = match mode.up.take() {
        Some((_, start)) => start.await.ok(),
        None => None,
    };
    let (hypr, record) = (ports.hypr.clone(), ports.lid_record.clone());
    let watch = mode.lid.take();
    let undone = tokio::task::spawn_blocking(move || {
        drop(starting);
        let lock = watch.and_then(lid::Watch::stop);
        // Before the copies go: with the screen still disabled, no output would be on.
        let opened = restore(&*hypr, record.as_deref());
        drop(mode);
        let removed = output::remove_all(&*hypr);
        drop(lock);
        (opened, removed)
    })
    .await;
    if let Ok((opened, removed)) = undone {
        said_restored(opened);
        if let Err(e) = removed {
            eprintln!("navette: could not remove the remote outputs: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use gliff_proto::{ChromaMode, Codec, ServerMsg};

    use super::*;
    use crate::hypr::fake::{FakeHypr, FakeMonitor};
    use crate::lid::fake::FakeLogind;

    const FIRST: &str = "NAVETTE-1";
    const SECOND: &str = "NAVETTE-2";

    /// Streams that send one config message, and can be ended from the test. Each one says,
    /// when dropped, on which thread and whether its output was still there.
    struct FakeStreams {
        hypr: Arc<FakeHypr>,
        ends: Mutex<Vec<(String, oneshot::Sender<io::Error>)>>,
        started: Mutex<Vec<(String, f32)>>,
        refuse: Mutex<bool>,
        /// Refuses the stream of this output only: an encoder out of sessions.
        refuse_on: Mutex<Option<String>>,
        /// Takes this long to start the stream of this output: an encoder slow to open.
        slow_on: Mutex<Option<(String, Duration)>>,
        /// Each stream's feedback from home.
        feedback: Mutex<Vec<(String, UnboundedReceiver<ClientMsg>)>>,
        panic: Mutex<bool>,
        dropped: Arc<Mutex<Vec<Dropped>>>,
    }

    #[derive(Debug)]
    struct Dropped {
        thread: std::thread::ThreadId,
        output: String,
        output_there: bool,
    }

    struct FakeStream {
        hypr: Arc<FakeHypr>,
        output: String,
        dropped: Arc<Mutex<Vec<Dropped>>>,
    }

    impl Drop for FakeStream {
        fn drop(&mut self) {
            self.dropped.lock().unwrap().push(Dropped {
                thread: std::thread::current().id(),
                output: self.output.clone(),
                output_there: self.hypr.monitor(&self.output).is_some(),
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
                refuse_on: Mutex::default(),
                slow_on: Mutex::default(),
                feedback: Mutex::default(),
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
            feedback: UnboundedReceiver<ClientMsg>,
            ended: oneshot::Sender<io::Error>,
        ) -> io::Result<Box<dyn Send + Sync>> {
            if *self.panic.lock().unwrap() {
                panic!("the encoder library crashed");
            }
            let slow = self.slow_on.lock().unwrap().clone();
            if let Some((output, wait)) = slow
                && output == source.output
            {
                std::thread::sleep(wait);
            }
            if *self.refuse.lock().unwrap()
                || self.refuse_on.lock().unwrap().as_ref() == Some(&source.output)
            {
                return Err(io::Error::other("no encoder"));
            }
            self.started
                .lock()
                .unwrap()
                .push((source.output.clone(), source.scale));
            self.ends
                .lock()
                .unwrap()
                .push((source.output.clone(), ended));
            self.feedback
                .lock()
                .unwrap()
                .push((source.output.clone(), feedback));
            outbox.send(Msg::Video(ServerMsg::Ping { t: 1 }), Vec::new());
            Ok(Box::new(FakeStream {
                hypr: self.hypr.clone(),
                output: source.output,
                dropped: self.dropped.clone(),
            }))
        }
    }

    impl FakeStreams {
        fn drop_last(&self) {
            drop(self.ends.lock().unwrap().pop().unwrap());
        }

        fn end_last(&self) {
            let (_, end) = self.ends.lock().unwrap().pop().unwrap();
            let _ = end.send(io::Error::other("capture stopped"));
        }

        /// Ends the newest stream of `output`.
        fn end(&self, output: &str) {
            let mut ends = self.ends.lock().unwrap();
            let at = ends.iter().rposition(|(o, _)| o == output).unwrap();
            let (_, end) = ends.remove(at);
            let _ = end.send(io::Error::other("capture stopped"));
        }

        fn started_on(&self, output: &str) -> usize {
            let started = self.started.lock().unwrap();
            started.iter().filter(|(o, _)| o == output).count()
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
        start_of(Vec::new())
    }

    /// A start of `copy()`, then `others` right of it.
    fn start_of(others: Vec<OutputCopy>) -> Incoming {
        Incoming::Display(Display::Start {
            output: copy(),
            caps: ClientCaps {
                codecs: vec![Codec::H264],
                max_width: 3840,
                max_height: 2160,
                chroma: vec![ChromaMode::Single420],
                features: Vec::new(),
            },
            others: (!others.is_empty()).then_some(others),
            closed_lid: None,
        })
    }

    /// A start with `closed_lid = on`.
    fn start_with_the_lid() -> Incoming {
        let Incoming::Display(Display::Start {
            output,
            caps,
            others,
            ..
        }) = start()
        else {
            unreachable!()
        };
        Incoming::Display(Display::Start {
            output,
            caps,
            others,
            closed_lid: Some(true),
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
            let output = self.started.lock().unwrap()[self.index].output.clone();
            let there = self.hypr.monitor(&output).is_some();
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
        logind: Arc<FakeLogind>,
        record: std::path::PathBuf,
        _dir: tempfile::TempDir,
        task: tokio::task::JoinHandle<()>,
    }

    /// The lid's poll in tests: soon, so a change is seen without an event.
    const LID_POLL: Duration = Duration::from_millis(20);

    fn agent() -> Agent {
        agent_polling(LID_POLL)
    }

    /// An agent reading the lid every `poll`.
    fn agent_polling(poll: Duration) -> Agent {
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
        let logind = Arc::new(FakeLogind::default());
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("navette").join("lid");
        let ports = Ports {
            hypr: hypr.clone(),
            streams: streams.clone(),
            injectors: Arc::new(FakeInjectors {
                hypr: hypr.clone(),
                started: injected.clone(),
                refuse: refuse_input.clone(),
            }),
            logind: logind.clone(),
            lid_record: Some(record.clone()),
            lid_poll: poll,
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
            logind,
            record,
            _dir: dir,
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
        assert_eq!(output, FIRST);
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
        assert!(hypr.monitor(FIRST).is_none());
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
                    output: FIRST.into(),
                    others: None,
                    closed_lid: None,
                }))
        );
        // The stream starts again after a stop of its own: still a picture, no second word.
        agent.streams.end_last();
        until(|| agent.streams.count() == 2).await;
        assert!(failed(&agent.outbox).is_none());
        assert_eq!(no_input(&agent.outbox).len(), 1);
        assert!(agent.hypr.monitor(FIRST).is_some());
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
        until(|| agent.hypr.monitor(FIRST).is_some()).await;
        until(|| agent.streams.count() == 1).await;
        assert_eq!(agent.streams.started.lock().unwrap()[0].0, FIRST);
        until(|| agent.outbox.queued().len() == 2).await;
        assert_eq!(
            agent.outbox.queued()[1],
            Msg::Display(Display::Started {
                output: FIRST.into(),
                others: None,
                closed_lid: None,
            })
        );
    }

    #[tokio::test]
    async fn stopping_the_mode_removes_the_remote_output() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.hypr.monitor(FIRST).is_some()).await;
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| agent.hypr.monitor(FIRST).is_none()).await;
        assert_eq!(
            agent.hypr.changes().last().unwrap(),
            "output remove NAVETTE-1"
        );
    }

    #[tokio::test]
    async fn the_remote_cursor_is_hidden_while_the_mode_runs_and_back_when_it_stops() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        assert!(agent.hypr.cursor_invisible());
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| agent.hypr.monitor(FIRST).is_none()).await;
        assert!(!agent.hypr.cursor_invisible());
    }

    #[tokio::test]
    async fn a_dropped_link_shows_the_remote_cursor_again() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        drop(agent.inbox);
        agent.task.await.unwrap();
        assert!(!agent.hypr.cursor_invisible());
    }

    #[tokio::test]
    async fn the_remote_cursor_is_hidden_again_after_a_hyprland_restart() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        agent.hypr.set_down(true, Vec::new());
        agent.streams.end_last();
        tokio::time::sleep(RETRY * 2).await;
        agent
            .hypr
            .set_down(false, vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        until(|| agent.streams.count() == 2).await;
        assert!(agent.hypr.cursor_invisible());
    }

    #[tokio::test]
    async fn a_dropped_link_removes_the_remote_output() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.hypr.monitor(FIRST).is_some()).await;
        drop(agent.inbox);
        agent.task.await.unwrap();
        assert!(agent.hypr.monitor(FIRST).is_none());
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
                reason: "NAVETTE-1: no encoder".into()
            })]
        );
        assert!(agent.hypr.monitor(FIRST).is_none());
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
        assert!(agent.hypr.monitor(FIRST).is_none());

        // A new Hyprland starts with only the laptop's screen.
        agent
            .hypr
            .set_down(false, vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        until(|| agent.streams.count() == 2).await;
        assert_eq!(agent.hypr.monitor(FIRST).unwrap().width, 1920);
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
        until(|| agent.hypr.monitor(FIRST).is_none()).await;
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

    /// The ports of an agent that starts on `hypr`, its lid record at `record` (none there by
    /// default).
    fn starting_agent(hypr: &Arc<FakeHypr>, record: Option<&std::path::Path>) -> Ports {
        Ports {
            hypr: hypr.clone(),
            streams: Arc::new(FakeStreams::new(hypr.clone())),
            injectors: Arc::new(FakeInjectors {
                hypr: hypr.clone(),
                started: Arc::default(),
                refuse: Arc::default(),
            }),
            logind: Arc::new(FakeLogind::default()),
            lid_record: record.map(std::path::Path::to_path_buf),
            lid_poll: LID_POLL,
        }
    }

    #[tokio::test]
    async fn a_leftover_output_is_removed_when_the_agent_starts() {
        let hypr = Arc::new(FakeHypr::with(vec![
            FakeMonitor::new("eDP-1", 1920, 1200, 0),
            FakeMonitor::new(FIRST, 1920, 1080, 1920),
        ]));
        let (_peers, peers_rx) = mpsc::unbounded_channel();
        let (_events, events_rx) = mpsc::unbounded_channel();
        tokio::spawn(serve(peers_rx, starting_agent(&hypr, None), events_rx));
        until(|| hypr.monitor(FIRST).is_none()).await;
    }

    #[tokio::test]
    async fn the_cursor_hidden_by_an_agent_that_crashed_is_shown_when_the_agent_starts() {
        let hypr = Arc::new(FakeHypr::with(vec![
            FakeMonitor::new("eDP-1", 1920, 1200, 0),
            FakeMonitor::new(FIRST, 1920, 1080, 1920),
        ]));
        hypr.request("eval hl.config({ cursor = { invisible = true } })")
            .unwrap();
        let (_peers, peers_rx) = mpsc::unbounded_channel();
        let (_events, events_rx) = mpsc::unbounded_channel();
        tokio::spawn(serve(peers_rx, starting_agent(&hypr, None), events_rx));
        until(|| !hypr.cursor_invisible()).await;
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
        assert!(agent.hypr.monitor(FIRST).is_none());
        *agent.streams.panic.lock().unwrap() = false;
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
    }

    #[tokio::test]
    async fn a_reload_gets_the_monitor_rule_back() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.hypr.monitor(FIRST).is_some()).await;
        agent.hypr.reload();
        assert_eq!(agent.hypr.monitor(FIRST).unwrap().scale, 2.0);
        agent.events.send(hypr::Event::ConfigReloaded).unwrap();
        until(|| agent.hypr.monitor(FIRST).unwrap().scale == 1.0).await;
    }

    fn portrait() -> OutputCopy {
        OutputCopy {
            width: 2560,
            height: 1440,
            refresh_mhz: 60_000,
            scale_milli: 1000,
            transform: 1,
        }
    }

    async fn two(agent: &Agent) {
        agent.inbox.try_send(start_of(vec![portrait()])).unwrap();
        until(|| agent.streams.count() == 2).await;
    }

    #[tokio::test]
    async fn two_monitors_make_two_outputs_side_by_side_each_with_its_stream() {
        let agent = agent();
        two(&agent).await;
        let first = agent.hypr.monitor(FIRST).unwrap();
        let second = agent.hypr.monitor(SECOND).unwrap();
        assert_eq!((first.x, first.width, first.transform), (1920, 1920, 0));
        assert_eq!((second.x, second.width, second.transform), (3840, 2560, 1));
        until(|| agent.outbox.queued().len() == 3).await;
        assert_eq!(
            agent.outbox.queued()[2],
            Msg::Display(Display::Started {
                output: FIRST.into(),
                others: Some(vec![SECOND.into()]),
                closed_lid: None,
            })
        );
    }

    #[tokio::test]
    async fn home_s_input_goes_to_the_output_it_names_and_its_keymap_to_both() {
        let agent = agent();
        two(&agent).await;
        until(|| agent.injected(1).is_some()).await;
        let keymap = input::Event::Keymap("home keymap".into());
        agent.input.push(keymap.clone());
        until(|| agent.injected(1).unwrap().2 == [keymap.clone()]).await;
        agent.input.on(1).push(key(true));
        until(|| agent.injected(1).unwrap().2.len() == 2).await;
        let (first, _, on_first) = agent.injected(0).unwrap();
        let (second, _, on_second) = agent.injected(1).unwrap();
        assert_eq!((first.as_str(), second.as_str()), (FIRST, SECOND));
        assert_eq!(on_first, std::slice::from_ref(&keymap));
        assert_eq!(on_second, [keymap, key(true)]);
    }

    #[tokio::test]
    async fn home_s_acks_go_to_the_stream_of_their_output() {
        let agent = agent();
        two(&agent).await;
        for output in [1, 7] {
            agent
                .inbox
                .try_send(Incoming::Feedback {
                    output,
                    msg: ClientMsg::RequestKeyframe,
                })
                .unwrap();
        }
        let heard = |name: &str| {
            let mut feedback = agent.streams.feedback.lock().unwrap();
            let (_, rx) = feedback.iter_mut().find(|(o, _)| o == name).unwrap();
            rx.try_recv().ok()
        };
        until(|| heard(SECOND).is_some()).await;
        // Output 7 runs no stream: its ack goes nowhere, and nothing else hears it.
        assert_eq!(heard(FIRST), None);
        assert!(!agent.task.is_finished());
    }

    #[tokio::test]
    async fn a_second_output_that_cannot_stream_says_so_and_leaves_nothing() {
        let agent = agent();
        *agent.streams.refuse_on.lock().unwrap() = Some(SECOND.into());
        agent.inbox.try_send(start_of(vec![portrait()])).unwrap();
        until(|| failed(&agent.outbox).is_some()).await;
        let reason = failed(&agent.outbox).unwrap();
        assert!(
            reason.contains(SECOND) && reason.contains("encoder") && reason.contains("no encoder"),
            "{reason}"
        );
        assert!(agent.hypr.monitor(FIRST).is_none());
        assert!(agent.hypr.monitor(SECOND).is_none());
        // The first output hid the laptop's cursor: shown again with the outputs gone.
        assert!(!agent.hypr.cursor_invisible());
        // The first stream went before its output.
        let dropped = agent.streams.dropped.lock().unwrap();
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].output_there);
    }

    #[tokio::test]
    async fn one_lost_stream_starts_again_alone_on_its_own_output() {
        let agent = agent();
        two(&agent).await;
        agent.streams.end(SECOND);
        until(|| agent.streams.started_on(SECOND) == 2).await;
        assert_eq!(agent.streams.started_on(FIRST), 1);
        assert_eq!(created(&agent.hypr), 2);
        let dropped = agent.streams.dropped.lock().unwrap();
        assert!(dropped.iter().all(|d| d.output == SECOND), "{dropped:?}");
    }

    #[tokio::test]
    async fn both_outputs_come_back_after_a_hyprland_restart() {
        let agent = agent();
        two(&agent).await;
        agent.hypr.set_down(true, Vec::new());
        agent.streams.end(FIRST);
        agent.streams.end(SECOND);
        tokio::time::sleep(RETRY * 2).await;
        agent
            .hypr
            .set_down(false, vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        until(|| agent.streams.count() == 4).await;
        assert_eq!(agent.hypr.monitor(SECOND).unwrap().x, 3840);
    }

    /// A narrow third copy, so no two outputs of `three` have the same size.
    fn narrow() -> OutputCopy {
        OutputCopy {
            width: 1280,
            height: 720,
            refresh_mhz: 60_000,
            scale_milli: 1000,
            transform: 0,
        }
    }

    async fn three(agent: &Agent) {
        let others = vec![portrait(), narrow()];
        agent.inbox.try_send(start_of(others)).unwrap();
        until(|| agent.streams.count() == 3).await;
    }

    /// The outputs the mode made, as the user's config sees them: name, place, size, turn,
    /// from the left.
    fn outputs(agent: &Agent) -> Vec<(String, i32, u32, i32)> {
        let mut outputs: Vec<_> = ["NAVETTE-1", "NAVETTE-2", "NAVETTE-3", "NAVETTE-4"]
            .into_iter()
            .filter_map(|n| agent.hypr.monitor(n))
            .map(|m| (m.name, m.x, m.width, m.transform))
            .collect();
        outputs.sort_by_key(|o| o.1);
        outputs
    }

    fn the_desk_of_three() -> Vec<(String, i32, u32, i32)> {
        vec![
            ("NAVETTE-1".into(), 1920, 1920, 0),
            ("NAVETTE-2".into(), 3840, 2560, 1),
            ("NAVETTE-3".into(), 5280, 1280, 0),
        ]
    }

    #[tokio::test]
    async fn the_outputs_are_named_from_left_to_right() {
        let agent = agent();
        three(&agent).await;
        // The first monitor from home's left is NAVETTE-1, the next NAVETTE-2, and so on:
        // the name follows the place, so the n-th from the left is the same name.
        assert_eq!(outputs(&agent), the_desk_of_three());
    }

    #[tokio::test]
    async fn the_names_are_the_same_on_every_start() {
        let agent = agent();
        three(&agent).await;
        let first = outputs(&agent);
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| outputs(&agent).is_empty()).await;

        // Another mode in between, of two monitors, must leave nothing in the next one's names.
        agent.inbox.try_send(start_of(vec![portrait()])).unwrap();
        until(|| outputs(&agent).len() == 2).await;
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| outputs(&agent).is_empty()).await;

        agent
            .inbox
            .try_send(start_of(vec![portrait(), narrow()]))
            .unwrap();
        // The streams are counted since the agent began: 3, then 2, then 3 again.
        until(|| agent.streams.count() == 8).await;
        assert_eq!(outputs(&agent), first);
        assert_eq!(first, the_desk_of_three());
    }

    #[tokio::test]
    async fn the_names_survive_a_hyprland_restart_on_the_remote() {
        let agent = agent();
        three(&agent).await;
        agent.hypr.set_down(true, Vec::new());
        // The streams end in the reverse order: the outputs must come back by name, not by
        // who asks first.
        for output in ["NAVETTE-3", "NAVETTE-2", "NAVETTE-1"] {
            agent.streams.end(output);
        }
        tokio::time::sleep(RETRY * 2).await;
        assert!(outputs(&agent).is_empty());
        agent
            .hypr
            .set_down(false, vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        until(|| agent.streams.count() == 6).await;
        assert_eq!(outputs(&agent), the_desk_of_three());
    }

    #[tokio::test]
    async fn one_output_that_keeps_failing_ends_the_whole_mode() {
        let agent = agent();
        two(&agent).await;
        *agent.streams.refuse_on.lock().unwrap() = Some(SECOND.into());
        agent.streams.end(SECOND);
        until(|| failed(&agent.outbox).is_some()).await;
        assert!(failed(&agent.outbox).unwrap().contains(SECOND));
        until(|| agent.hypr.monitor(FIRST).is_none()).await;
        assert!(agent.hypr.monitor(SECOND).is_none());
        assert!(!agent.task.is_finished());
    }

    #[tokio::test]
    async fn stopping_releases_every_input_and_stream_before_any_output_goes() {
        let agent = agent();
        two(&agent).await;
        until(|| agent.injected(1).is_some()).await;
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| agent.hypr.monitor(SECOND).is_none()).await;
        assert_eq!(
            (agent.dropped(0), agent.dropped(1)),
            (Some(true), Some(true))
        );
        let dropped = agent.streams.dropped.lock().unwrap();
        assert_eq!(dropped.len(), 2);
        assert!(dropped.iter().all(|d| d.output_there), "{dropped:?}");
        assert!(agent.hypr.monitor(FIRST).is_none());
    }

    #[tokio::test]
    async fn a_dropped_link_removes_every_output() {
        let agent = agent();
        two(&agent).await;
        drop(agent.inbox);
        agent.task.await.unwrap();
        assert!(agent.hypr.monitor(FIRST).is_none());
        assert!(agent.hypr.monitor(SECOND).is_none());
    }

    #[tokio::test]
    async fn a_reload_gets_every_monitor_rule_back() {
        let agent = agent();
        two(&agent).await;
        agent.hypr.reload();
        agent.events.send(hypr::Event::ConfigReloaded).unwrap();
        until(|| {
            agent.hypr.monitor(FIRST).unwrap().scale == 1.0
                && agent.hypr.monitor(SECOND).unwrap().scale == 1.0
        })
        .await;
    }

    #[tokio::test]
    async fn every_output_left_by_an_agent_that_crashed_is_removed_when_it_starts() {
        let hypr = Arc::new(FakeHypr::with(vec![
            FakeMonitor::new("eDP-1", 1920, 1200, 0),
            FakeMonitor::new(FIRST, 1920, 1080, 1920),
            FakeMonitor::new(SECOND, 1920, 1080, 3840),
        ]));
        let (_peers, peers_rx) = mpsc::unbounded_channel();
        let (_events, events_rx) = mpsc::unbounded_channel();
        tokio::spawn(serve(peers_rx, starting_agent(&hypr, None), events_rx));
        until(|| hypr.monitor(FIRST).is_none() && hypr.monitor(SECOND).is_none()).await;
    }

    fn laptop_disabled(agent: &Agent) -> bool {
        agent.hypr.monitor("eDP-1").unwrap().disabled
    }

    /// A mode with `closed_lid = on`, its output streaming.
    async fn running_with_the_lid(agent: &Agent) {
        agent.inbox.try_send(start_with_the_lid()).unwrap();
        until(|| agent.streams.count() == 1).await;
    }

    #[tokio::test]
    async fn a_lid_closed_during_a_mode_disables_the_laptop_screen_and_opened_brings_it_back() {
        let agent = agent();
        running_with_the_lid(&agent).await;
        assert!(!laptop_disabled(&agent));
        agent.logind.close(true);
        until(|| laptop_disabled(&agent)).await;
        // Navette's output goes on.
        assert!(agent.hypr.monitor(FIRST).is_some_and(|m| !m.disabled));
        agent.logind.close(false);
        until(|| !laptop_disabled(&agent)).await;
        assert_eq!(
            agent.hypr.monitor("eDP-1").unwrap(),
            FakeMonitor::new("eDP-1", 1920, 1200, 0)
        );
        assert!(!agent.record.exists());
    }

    #[tokio::test]
    async fn the_lock_is_taken_with_the_mode_and_released_when_it_stops() {
        let agent = agent();
        running_with_the_lid(&agent).await;
        assert_eq!((agent.logind.taken(), agent.logind.held()), (1, 1));
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| agent.logind.held() == 0).await;
    }

    #[tokio::test]
    async fn stopping_the_mode_with_the_lid_closed_brings_the_laptop_screen_back() {
        let agent = agent();
        running_with_the_lid(&agent).await;
        agent.logind.close(true);
        until(|| laptop_disabled(&agent)).await;
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| agent.hypr.monitor(FIRST).is_none()).await;
        assert!(!laptop_disabled(&agent));
        assert_eq!(agent.logind.held(), 0);
        // The screen came back before the copy went: some output was on all along.
        let changes = agent.hypr.changes();
        let back = changes.iter().position(|c| c.contains("disabled = false"));
        let gone = changes.iter().position(|c| c == "output remove NAVETTE-1");
        assert!(back.unwrap() < gone.unwrap(), "{changes:?}");
    }

    #[tokio::test]
    async fn a_dropped_link_brings_the_laptop_screen_back() {
        let agent = agent();
        running_with_the_lid(&agent).await;
        agent.logind.close(true);
        until(|| laptop_disabled(&agent)).await;
        let Agent {
            inbox,
            task,
            hypr,
            logind,
            record,
            _dir,
            ..
        } = agent;
        drop(inbox);
        task.await.unwrap();
        assert!(!hypr.monitor("eDP-1").unwrap().disabled);
        assert_eq!(logind.held(), 0);
        assert!(!record.exists());
    }

    #[tokio::test]
    async fn a_laptop_screen_left_disabled_by_an_agent_that_crashed_comes_back_when_it_starts() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("lid");
        let hypr = Arc::new(FakeHypr::with(vec![
            FakeMonitor::new("eDP-1", 1920, 1200, 0),
            FakeMonitor::new(FIRST, 1920, 1080, 1920),
        ]));
        crate::lid::close(&*hypr, &record).unwrap();
        assert!(hypr.monitor("eDP-1").unwrap().disabled);
        let (_peers, peers_rx) = mpsc::unbounded_channel();
        let (_events, events_rx) = mpsc::unbounded_channel();
        tokio::spawn(serve(
            peers_rx,
            starting_agent(&hypr, Some(&record)),
            events_rx,
        ));
        until(|| hypr.monitor(FIRST).is_none()).await;
        until(|| !hypr.monitor("eDP-1").unwrap().disabled).await;
        assert!(!record.exists());
    }

    #[tokio::test]
    async fn a_refused_lock_is_reported_and_the_lid_is_left_alone() {
        let agent = agent();
        *agent.logind.refuse.lock().unwrap() = Some("Access denied".into());
        agent.logind.close(true);
        running_with_the_lid(&agent).await;
        // The mode runs all the same, home is told, and the lid is read only before the lock.
        until(|| !no_lid(&agent.outbox).is_empty()).await;
        assert!(no_lid(&agent.outbox)[0].contains("Access denied"));
        tokio::time::sleep(LID_POLL * 5).await;
        assert_eq!(agent.logind.looked(), 1);
        assert!(!laptop_disabled(&agent));
        assert!(failed(&agent.outbox).is_none());
        assert_eq!(agent.hypr.monitor(FIRST).map(|m| m.disabled), Some(false));
    }

    #[tokio::test]
    async fn with_the_option_off_the_lid_changes_nothing() {
        let agent = agent();
        agent.logind.close(true);
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        tokio::time::sleep(LID_POLL * 5).await;
        assert_eq!((agent.logind.taken(), agent.logind.looked()), (0, 0));
        assert!(!laptop_disabled(&agent));
    }

    #[tokio::test]
    async fn a_reload_with_the_lid_closed_disables_the_laptop_screen_again() {
        // No poll comes in the test's time: what follows the reload is the event's doing.
        let agent = agent_polling(Duration::from_secs(3600));
        agent.logind.close(true);
        running_with_the_lid(&agent).await;
        // Hyprland says the copy came; the screen goes once it is there to take its windows.
        agent
            .events
            .send(hypr::Event::MonitorAdded(FIRST.into()))
            .unwrap();
        until(|| laptop_disabled(&agent)).await;
        agent.hypr.reload();
        assert!(!laptop_disabled(&agent));
        agent.events.send(hypr::Event::ConfigReloaded).unwrap();
        until(|| laptop_disabled(&agent)).await;
        agent.logind.close(false);
        agent
            .events
            .send(hypr::Event::MonitorAdded("DP-9".into()))
            .unwrap();
        until(|| !laptop_disabled(&agent)).await;
    }

    fn no_lid(outbox: &Outbox) -> Vec<String> {
        outbox
            .queued()
            .into_iter()
            .filter_map(|msg| match msg {
                Msg::NoLid(reason) => Some(reason),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn the_agent_says_it_knows_closed_lid_when_its_outputs_are_up() {
        let agent = agent();
        running_with_the_lid(&agent).await;
        until(|| agent.outbox.queued().len() == 2).await;
        assert_eq!(
            agent.outbox.queued()[1],
            Msg::Display(Display::Started {
                output: FIRST.into(),
                others: None,
                closed_lid: Some(true),
            })
        );
    }

    /// The mode stops while Hyprland cannot be reached; it is back, the screen still disabled.
    async fn a_restore_that_failed_at_stop(agent: &Agent) {
        running_with_the_lid(agent).await;
        agent.logind.close(true);
        until(|| laptop_disabled(agent)).await;
        agent.hypr.set_down(true, Vec::new());
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| agent.logind.held() == 0).await;
        assert!(agent.record.exists());
        let disabled = FakeMonitor {
            disabled: true,
            ..FakeMonitor::new("eDP-1", 1920, 1200, 0)
        };
        agent.hypr.set_down(false, vec![disabled]);
    }

    #[tokio::test]
    async fn a_restore_that_failed_at_stop_goes_through_at_hyprland_s_next_event() {
        let agent = agent();
        a_restore_that_failed_at_stop(&agent).await;
        agent
            .events
            .send(hypr::Event::MonitorAdded("DP-9".into()))
            .unwrap();
        until(|| !laptop_disabled(&agent)).await;
        assert!(!agent.record.exists());
    }

    #[tokio::test]
    async fn a_restore_that_failed_at_stop_goes_through_at_the_next_start() {
        let agent = agent();
        a_restore_that_failed_at_stop(&agent).await;
        agent.logind.close(false);
        agent.inbox.try_send(start()).unwrap();
        until(|| !laptop_disabled(&agent)).await;
        assert!(!agent.record.exists());
    }

    #[tokio::test]
    async fn a_slow_logind_holds_neither_the_start_nor_home_s_input() {
        let agent = agent();
        // Each answer takes longer than the whole start may.
        let slow = Duration::from_millis(1500);
        *agent.logind.slow.lock().unwrap() = slow;
        let asked = tokio::time::Instant::now();
        agent.inbox.try_send(start_with_the_lid()).unwrap();
        until(|| agent.outbox.queued().len() == 2).await;
        assert!(asked.elapsed() < slow, "started in {:?}", asked.elapsed());
        assert_eq!(agent.logind.held(), 0);
        let sent = tokio::time::Instant::now();
        agent.input.push(key(true));
        until(|| agent.injected(0).unwrap().2 == [key(true)]).await;
        assert!(sent.elapsed() < slow, "injected in {:?}", sent.elapsed());
        until(|| agent.logind.held() == 1).await;
    }

    #[tokio::test]
    async fn the_laptop_screen_is_disabled_again_after_a_hyprland_restart_once_its_copy_is_back() {
        let agent = agent();
        running_with_the_lid(&agent).await;
        agent.logind.close(true);
        until(|| laptop_disabled(&agent)).await;
        agent.hypr.set_down(true, Vec::new());
        agent.streams.end_last();
        tokio::time::sleep(RETRY * 2).await;
        agent
            .hypr
            .set_down(false, vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        until(|| agent.streams.count() == 2).await;
        until(|| laptop_disabled(&agent)).await;
        assert!(agent.hypr.monitor(FIRST).is_some_and(|m| !m.disabled));
    }

    #[tokio::test]
    async fn a_reload_hides_the_remote_cursor_again() {
        let agent = agent();
        agent.inbox.try_send(start()).unwrap();
        until(|| agent.streams.count() == 1).await;
        agent.hypr.reload();
        assert!(!agent.hypr.cursor_invisible());
        agent.events.send(hypr::Event::ConfigReloaded).unwrap();
        until(|| agent.hypr.cursor_invisible()).await;
    }

    #[tokio::test]
    async fn the_second_output_goes_right_of_the_size_hyprland_chose_for_the_first() {
        let agent = agent();
        agent.hypr.choose_scale(1.2);
        two(&agent).await;
        // 1920 at 1.2 is 1600 wide.
        assert_eq!(agent.hypr.monitor(SECOND).unwrap().x, 1920 + 1600);
    }

    #[tokio::test]
    async fn input_to_the_first_output_keeps_flowing_while_the_second_starts_again() {
        let agent = agent();
        two(&agent).await;
        until(|| agent.injected(1).is_some()).await;
        *agent.streams.slow_on.lock().unwrap() = Some((SECOND.into(), Duration::from_secs(2)));
        agent.streams.end(SECOND);
        // The second stream's start is under way, for two seconds.
        tokio::time::sleep(RETRY * 3).await;
        let sent = tokio::time::Instant::now();
        agent.input.push(key(true));
        until(|| agent.injected(0).unwrap().2.contains(&key(true))).await;
        assert!(
            sent.elapsed() < Duration::from_millis(500),
            "{:?}",
            sent.elapsed()
        );
        until(|| agent.streams.started_on(SECOND) == 2).await;
    }

    #[tokio::test]
    async fn a_stop_during_a_start_again_waits_for_it_and_leaves_no_output() {
        let agent = agent();
        two(&agent).await;
        *agent.streams.slow_on.lock().unwrap() = Some((SECOND.into(), Duration::from_millis(300)));
        agent
            .hypr
            .request(&format!("output remove {SECOND}"))
            .unwrap();
        agent.streams.end(SECOND);
        tokio::time::sleep(RETRY * 3).await;
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| agent.streams.started_on(SECOND) == 2).await;
        until(|| agent.streams.dropped.lock().unwrap().len() == 3).await;
        assert!(agent.hypr.monitor(FIRST).is_none());
        assert!(agent.hypr.monitor(SECOND).is_none());
        assert!(!agent.hypr.cursor_invisible());
        // The first is the stream whose output went away; the stop's come after.
        let dropped = agent.streams.dropped.lock().unwrap();
        assert!(dropped[1..].iter().all(|d| d.output_there), "{dropped:?}");
    }

    #[tokio::test]
    async fn the_size_of_a_surface_of_the_last_mode_is_not_given_to_the_next() {
        let agent = agent();
        let area = |width| input::Event::Area { width, height: 720 };
        agent.input.push(area(1280));
        agent.input.on(1).push(area(1600));
        tokio::time::sleep(Duration::from_millis(50)).await;
        two(&agent).await;
        until(|| agent.injected(1).is_some_and(|i| i.2 == [area(1600)])).await;
        agent
            .inbox
            .try_send(Incoming::Display(Display::Stop))
            .unwrap();
        until(|| agent.hypr.monitor(FIRST).is_none()).await;
        // The next mode's first surface says its size; its second has not yet.
        agent.input.push(area(1920));
        tokio::time::sleep(Duration::from_millis(50)).await;
        two(&agent).await;
        until(|| agent.streams.started_on(SECOND) == 2).await;
        until(|| agent.injected(3).is_some()).await;
        assert_eq!(agent.injected(2).unwrap().2, [area(1920)]);
        assert_eq!(agent.injected(3).unwrap().2, []);
    }
}
