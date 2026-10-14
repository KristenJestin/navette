//! Where the home mic goes. It serves one machine at a time: the remote machine while a mode
//! runs, from the moment it starts, and home again when it stops; in between, `navette mic
//! toggle|remote|home` (the hotkey, the bar) switches it either way. Switching only starts or
//! stops sending: the remote's source stays its default mic for the whole mode, silent while
//! the mic is home, so a call there keeps its device.
//!
//! `navette serve` holds the mic, so `navette mic` reaches it over a Unix socket beside the
//! mode's, in `$XDG_RUNTIME_DIR/navette`. `navette mic watch` prints where it goes at each
//! change, for a bar to show.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

use crate::audio::mic::HomeMic;
use crate::audio::{Graph, Message, Saved, lock};
use crate::notify::Notifier;

/// Where the mic goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// No mode runs: the mic is home, and stays there.
    Off,
    Home,
    Remote,
    /// A mode runs, but the mic cannot go where it should: `navette mic toggle` says why.
    Failed,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Off => "off",
            Status::Home => "home",
            Status::Remote => "remote",
            Status::Failed => "failed",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        [Status::Off, Status::Home, Status::Remote, Status::Failed]
            .into_iter()
            .find(|s| s.word() == word)
    }

    /// One line of JSON, in the shape of a Waybar custom module (`text`, `alt`, `class`,
    /// `tooltip`), which any bar can read.
    pub fn json(self) -> String {
        let tooltip = match self {
            Status::Off => "No mode is running: the mic is home",
            Status::Home => "The mic is home",
            Status::Remote => "The mic goes to the remote machine",
            Status::Failed => {
                "The mic cannot go to the remote machine: `navette mic toggle` says why"
            }
        };
        let word = self.word();
        format!(r#"{{"text":"{word}","alt":"{word}","class":"{word}","tooltip":"{tooltip}"}}"#)
    }
}

/// What `navette mic` asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    Toggle,
    Remote,
    Home,
    Status,
}

impl Order {
    pub fn word(self) -> &'static str {
        match self {
            Order::Toggle => "toggle",
            Order::Remote => "remote",
            Order::Home => "home",
            Order::Status => "status",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        [Order::Toggle, Order::Remote, Order::Home, Order::Status]
            .into_iter()
            .find(|o| o.word() == word)
    }
}

/// What the control socket serves: the mic, or nothing when home plays no sound.
pub trait Routing: Send + Sync {
    /// Its calls may wait for PipeWire: they are made off the async runtime.
    fn order(&self, order: Order) -> io::Result<Status>;
    fn watch(&self) -> watch::Receiver<Status>;
}

/// The home mic, at home.
pub struct Mic<G: Graph> {
    shared: Arc<Shared<G>>,
    toggle_command: String,
}

/// What the mic's own thread shares with it: it opens the mic again when PipeWire restarts.
struct Shared<G: Graph> {
    graph: G,
    /// Where home's default mic is kept while the mic is away.
    saved: Saved,
    /// Held through a start or a stop, which may wait for the link: they never overlap.
    lifecycle: Mutex<()>,
    /// Held only briefly, so that orders and the checks are answered while the link waits.
    state: Mutex<State<G>>,
    status: watch::Sender<Status>,
    /// Says where the mic went, at each switch.
    notifier: Arc<dyn Notifier>,
}

struct State<G: Graph> {
    /// The audio queue of the current link.
    link: Option<mpsc::Sender<Message>>,
    running: Option<Running<G>>,
    sessions: u32,
}

/// The mic while a mode runs.
struct Running<G: Graph> {
    /// None when it could not start.
    mic: Option<HomeMic<G>>,
    /// Why the mic does not go where it should, while it cannot.
    failure: Option<String>,
    /// Dropped when the mode stops, which ends the checks.
    _checks: Option<std_mpsc::Sender<()>>,
}

/// How often the mic looks for a PipeWire that restarted under it.
const CHECK: Duration = Duration::from_millis(250);

impl<G: Graph + Sync> Mic<G> {
    /// `toggle_command` is what the hotkey runs ([`toggle_command`]).
    pub fn new(
        graph: G,
        saved: Saved,
        toggle_command: String,
        notifier: Arc<dyn Notifier>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                graph,
                saved,
                lifecycle: Mutex::new(()),
                state: Mutex::new(State {
                    link: None,
                    running: None,
                    sessions: 0,
                }),
                status: watch::Sender::new(Status::Off),
                notifier,
            }),
            toggle_command,
        }
    }

    /// A link came up: the mic goes over it from the next mode on.
    pub fn attach(&self, link: mpsc::Sender<Message>) {
        lock(&self.shared.state).link = Some(link);
    }

    /// A mode started: the remote's source becomes its default mic, and the mic goes there.
    /// When it cannot, the mode runs without it, and the mic says why until the mode stops.
    pub fn start(&self) -> io::Result<()> {
        let shared = &self.shared;
        let _lifecycle = lock(&shared.lifecycle);
        let started = {
            let mut state = lock(&shared.state);
            if state.running.is_some() {
                return Ok(());
            }
            state.sessions = state.sessions.wrapping_add(1);
            let link = state.link.clone().filter(|link| !link.is_closed());
            link.map(|link| (link, state.sessions))
        };
        let (running, result) = match self.open(started) {
            Ok(running) => (running, Ok(())),
            Err(e) => {
                let running = Running {
                    mic: None,
                    failure: Some(format!("the mic could not go to the remote machine: {e}")),
                    _checks: None,
                };
                (running, Err(e))
            }
        };
        let mut state = lock(&shared.state);
        state.running = Some(running);
        shared.publish(&state);
        result
    }

    /// Tells the remote and opens the mic, the state's lock not held: the link may make it
    /// wait.
    fn open(&self, link: Option<(mpsc::Sender<Message>, u32)>) -> io::Result<Running<G>> {
        let shared = &self.shared;
        let (link, session) =
            link.ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "the link is down"))?;
        send(&link, Message::MicStart { session })?;
        let started = HomeMic::start(
            shared.graph.clone(),
            shared.saved.clone(),
            link.clone(),
            session,
        );
        let mic = match started {
            Ok(mic) => mic,
            Err(e) => {
                let _ = send(&link, Message::MicStop);
                return Err(e);
            }
        };
        let (checks, ended) = std_mpsc::channel();
        let checked = shared.clone();
        std::thread::Builder::new()
            .name("navette-mic".into())
            .spawn(move || checked.check(&ended))?;
        Ok(Running {
            mic: Some(mic),
            failure: None,
            _checks: Some(checks),
        })
    }

    /// The mode stopped: the mic comes home, and the remote puts its default mic back.
    pub fn stop(&self) {
        let shared = &self.shared;
        let _lifecycle = lock(&shared.lifecycle);
        let (running, link) = {
            let mut state = lock(&shared.state);
            let Some(running) = state.running.take() else {
                return;
            };
            shared.publish(&state);
            (running, state.link.clone())
        };
        // Closed first, so that no frame follows the stop.
        let Some(mic) = running.mic else {
            // It never went: the remote was told when it failed.
            return;
        };
        drop(mic);
        if let Some(link) = link {
            // A link already down took the remote's source with it.
            let _ = send(&link, Message::MicStop);
        }
    }
}

impl<G: Graph> Shared<G> {
    /// Tells the watchers where the mic goes, if that changed.
    fn publish(&self, state: &State<G>) {
        let now = match &state.running {
            None => Status::Off,
            Some(running) if running.failure.is_some() => Status::Failed,
            Some(running) if running.mic.as_ref().is_some_and(HomeMic::remote) => Status::Remote,
            Some(_) => Status::Home,
        };
        let mut before = now;
        self.status.send_if_modified(|status| {
            before = *status;
            *status = now;
            before != now
        });
        // What the user sees is where the mic is: a mode starting with the mic home, or
        // stopping with it home, moves nothing.
        if Self::place(before) != Self::place(now) {
            match now {
                Status::Remote => self.notifier.show("Mic → remote", ""),
                Status::Failed => {
                    let failure = state.running.as_ref().and_then(|r| r.failure.as_deref());
                    self.notifier
                        .show("Mic failed", failure.unwrap_or_default());
                }
                Status::Home | Status::Off => self.notifier.show("Mic → home", ""),
            }
        }
    }

    /// A switch that did not happen: the user pressed a key and nothing on screen says why,
    /// so the notification does.
    fn refused(&self, why: io::Error) -> io::Error {
        self.notifier.show("Mic not switched", &why.to_string());
        why
    }

    /// Where the user would say the mic is: no mode means home.
    fn place(status: Status) -> Status {
        match status {
            Status::Off => Status::Home,
            other => other,
        }
    }

    /// Opens the mic again whenever PipeWire restarted, until the mode stops.
    fn check(&self, ended: &std_mpsc::Receiver<()>) {
        while let Err(std_mpsc::RecvTimeoutError::Timeout) = ended.recv_timeout(CHECK) {
            let mut state = lock(&self.state);
            let Some(Running {
                mic: Some(mic),
                failure,
                ..
            }) = &mut state.running
            else {
                return;
            };
            match mic.recover() {
                Ok(()) => {
                    if failure.take().is_some() {
                        eprintln!("navette: mic: opened again");
                    }
                }
                Err(e) => {
                    if failure.is_none() {
                        eprintln!("navette: mic: could not open it again: {e}");
                    }
                    *failure = Some(format!(
                        "the mic went with PipeWire and could not be opened again: {e}"
                    ));
                }
            }
            self.publish(&state);
        }
    }
}

/// Waits for room on the link: an order must not be lost, as a frame may be.
fn send(link: &mpsc::Sender<Message>, message: Message) -> io::Result<()> {
    link.blocking_send(message)
        .map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "the link is down"))
}

impl<G: Graph + Sync> Routing for Mic<G> {
    fn order(&self, order: Order) -> io::Result<Status> {
        let shared = &self.shared;
        // Read without a lock: it is all there.
        if order == Order::Status {
            return Ok(*shared.status.borrow());
        }
        let mut state = lock(&shared.state);
        let current = *shared.status.borrow();
        let Some(running) = &mut state.running else {
            return Err(shared.refused(io::Error::other(
                "no mode is running: the mic is home until one starts",
            )));
        };
        if let Some(failure) = &running.failure {
            return Err(shared.refused(io::Error::other(failure.clone())));
        }
        let to = match (order, current) {
            (Order::Status, _) => return Ok(current),
            (Order::Remote, _) | (Order::Toggle, Status::Home) => Status::Remote,
            (Order::Home, _) | (Order::Toggle, _) => Status::Home,
        };
        if let Some(mic) = &mut running.mic {
            mic.send_to(to == Status::Remote)
                .map_err(|e| shared.refused(e))?;
        }
        shared.publish(&state);
        Ok(to)
    }

    fn watch(&self) -> watch::Receiver<Status> {
        self.shared.status.subscribe()
    }
}

/// Home plays no sound (`navette serve` without `--audio`): the mic is never sent.
pub struct NoSound(watch::Sender<Status>);

impl Default for NoSound {
    fn default() -> Self {
        Self(watch::Sender::new(Status::Off))
    }
}

impl Routing for NoSound {
    fn order(&self, order: Order) -> io::Result<Status> {
        match order {
            Order::Status => Ok(Status::Off),
            _ => Err(io::Error::other(
                "home plays no sound: start `navette serve --audio` to send the mic",
            )),
        }
    }

    fn watch(&self) -> watch::Receiver<Status> {
        self.0.subscribe()
    }
}

/// The shell command that toggles the mic with this very binary: what the hotkey runs.
pub fn toggle_command(navette: &Path) -> String {
    let path = navette.to_string_lossy().replace('\'', r"'\''");
    format!("'{path}' mic toggle")
}

/// `$XDG_RUNTIME_DIR/navette/mic.sock`.
pub fn control_path() -> io::Result<PathBuf> {
    Ok(crate::mode::control_path()?.with_file_name("mic.sock"))
}

/// How long, and how much, `navette serve` reads of an order.
const ORDER_READ: Duration = Duration::from_secs(5);
const ORDER_LONGEST: u64 = 16;
/// How long `navette mic` waits for an answer: switching never waits for PipeWire, but a mode
/// starting or stopping holds the mic meanwhile.
const ANSWER: Duration = Duration::from_secs(15);
/// How often `navette mic watch` looks for a `navette serve` that is not there.
const RETRY: Duration = Duration::from_secs(2);

/// Answers each `navette mic` connection: one order and its answer (`ok <status>` or
/// `error <why>`), or `watch` and a line at each change, the current one first.
pub async fn serve(listener: UnixListener, mic: Arc<dyn Routing>) {
    while let Ok((stream, _)) = listener.accept().await {
        let mic = mic.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            let mut read = BufReader::new(tokio::io::AsyncReadExt::take(read, ORDER_LONGEST));
            match tokio::time::timeout(ORDER_READ, read.read_line(&mut line)).await {
                Ok(Ok(_)) if line.ends_with('\n') => {}
                _ => return,
            }
            let word = line.trim();
            if word == "watch" {
                let mut changes = mic.watch();
                let mut byte = [0; 1];
                loop {
                    let status = *changes.borrow_and_update();
                    let line = format!("{}\n", status.word());
                    if write.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                    // A watcher says nothing more: anything it does, closing included, ends it,
                    // rather than at the next change, which may be hours away.
                    tokio::select! {
                        changed = changes.changed() => if changed.is_err() {
                            return;
                        },
                        _ = tokio::io::AsyncReadExt::read(&mut read, &mut byte) => return,
                    }
                }
            }
            let Some(order) = Order::parse(word) else {
                return;
            };
            let answer = tokio::task::spawn_blocking(move || mic.order(order))
                .await
                .unwrap_or_else(|e| Err(io::Error::other(e.to_string())));
            let text = match answer {
                Ok(status) => format!("ok {}\n", status.word()),
                Err(e) => format!("error {e}\n"),
            };
            let _ = write.write_all(text.as_bytes()).await;
        });
    }
}

async fn connect(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(path).await.map_err(|e| {
        io::Error::new(
            e.kind(),
            "`navette serve` is not running here: start it first",
        )
    })
}

/// `navette mic toggle|remote|home|status`: where the mic goes once the order is done.
pub async fn ask(path: &Path, order: Order) -> io::Result<Status> {
    tokio::time::timeout(ANSWER, async {
        let (read, mut write) = connect(path).await?.into_split();
        write
            .write_all(format!("{}\n", order.word()).as_bytes())
            .await?;
        let mut line = String::new();
        BufReader::new(read).read_line(&mut line).await?;
        match line.trim_end().split_once(' ') {
            Some(("ok", word)) => Status::parse(word)
                .ok_or_else(|| io::Error::other("`navette serve` gave no answer")),
            Some(("error", text)) => Err(io::Error::other(text.to_string())),
            _ => Err(io::Error::other("`navette serve` gave no answer")),
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "`navette serve` did not answer in time",
        )
    })?
}

/// `navette mic watch`: hands `each` where the mic goes, now and at every change, for ever.
/// While `navette serve` is not running, the mic is home: `off`, until it is back.
pub async fn watch(path: &Path, mut each: impl FnMut(Status) -> io::Result<()>) -> io::Result<()> {
    let mut last = None;
    let mut tell = |status: Status| -> io::Result<()> {
        if last != Some(status) {
            last = Some(status);
            each(status)?;
        }
        Ok(())
    };
    loop {
        if let Ok(stream) = UnixStream::connect(path).await {
            let (read, mut write) = stream.into_split();
            if write.write_all(b"watch\n").await.is_ok() {
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(status) = Status::parse(line.trim()) {
                        tell(status)?;
                    }
                }
            }
        }
        tell(Status::Off)?;
        tokio::time::sleep(RETRY).await;
    }
}

impl<G: Graph + Sync> crate::mode::Mic for Mic<G> {
    fn start(&self) -> io::Result<()> {
        Mic::start(self)
    }

    fn stop(&self) {
        Mic::stop(self);
    }

    fn toggle_command(&self) -> String {
        self.toggle_command.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::fake::FakeGraph;
    use crate::audio::mic::{AWAY, MIC};
    use crate::audio::{DefaultNode, Saved};
    use crate::notify::Fake;

    /// A mic at home, `desk-mic`, and the remote's end of its link.
    fn mic() -> (
        FakeGraph,
        Arc<Mic<FakeGraph>>,
        mpsc::Receiver<Message>,
        tempfile::TempDir,
    ) {
        let (graph, mic, remote, _notes, tmp) = noted();
        (graph, mic, remote, tmp)
    }

    /// The same, and what it shows the user.
    fn noted() -> (
        FakeGraph,
        Arc<Mic<FakeGraph>>,
        mpsc::Receiver<Message>,
        Arc<Fake>,
        tempfile::TempDir,
    ) {
        let graph = FakeGraph::default();
        graph.state().default_source = Some(DefaultNode::named("desk-mic"));
        let tmp = tempfile::tempdir().unwrap();
        let saved = Saved::in_dir(tmp.path().to_owned());
        let notes = Arc::new(Fake::default());
        let mic = Mic::new(
            graph.clone(),
            saved,
            "navette mic toggle".into(),
            notes.clone(),
        );
        let mic = Arc::new(mic);
        let (link, remote) = mpsc::channel(crate::audio::QUEUE);
        mic.attach(link);
        (graph, mic, remote, notes, tmp)
    }

    /// The name of home's default mic.
    fn home_default(graph: &FakeGraph) -> String {
        let state = graph.state();
        let node = state.default_source.as_ref().unwrap();
        node.name().unwrap().to_owned()
    }

    /// The mic's calls block: off the runtime, as `navette serve` makes them.
    async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        tokio::task::spawn_blocking(f).await.unwrap()
    }

    /// The orders the remote got, frames left out.
    fn orders(remote: &mut mpsc::Receiver<Message>) -> Vec<Message> {
        std::iter::from_fn(|| remote.try_recv().ok())
            .filter(|m| !matches!(m, Message::MicFrame { .. }))
            .collect()
    }

    /// Whether what the home mic hears now is sent.
    fn sends(graph: &FakeGraph, remote: &mut mpsc::Receiver<Message>) -> bool {
        let callback = graph.state().mics[0].1.clone();
        (callback.lock().unwrap())(&vec![0.25; 4 * crate::audio::codec::FRAME_SAMPLES]);
        std::iter::from_fn(|| remote.try_recv().ok()).any(|m| matches!(m, Message::MicFrame { .. }))
    }

    #[tokio::test]
    async fn a_mode_start_sends_the_mic_to_the_remote() {
        let (graph, mic, mut remote, _tmp) = mic();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        assert_eq!(mic.order(Order::Status).unwrap(), Status::Remote);
        assert_eq!(orders(&mut remote), [Message::MicStart { session: 1 }]);
        assert_eq!(graph.state().mics.len(), 1);
        assert!(sends(&graph, &mut remote));
    }

    #[tokio::test]
    async fn toggling_switches_the_mic_both_ways() {
        let (graph, mic, mut remote, _tmp) = mic();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        assert_eq!(orders(&mut remote), [Message::MicStart { session: 1 }]);
        assert_eq!(home_default(&graph), AWAY);
        assert_eq!(mic.order(Order::Toggle).unwrap(), Status::Home);
        assert!(!sends(&graph, &mut remote));
        // Home's apps have the mic again.
        assert_eq!(home_default(&graph), "desk-mic");
        assert_eq!(mic.order(Order::Toggle).unwrap(), Status::Remote);
        assert_eq!(home_default(&graph), AWAY);
        // Switching is no order to the remote: its source stays its default mic.
        assert!(orders(&mut remote).is_empty());
        assert!(sends(&graph, &mut remote));
    }

    #[tokio::test]
    async fn home_and_remote_send_the_mic_one_way_whatever_the_state() {
        let (graph, mic, mut remote, _tmp) = mic();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        assert_eq!(mic.order(Order::Home).unwrap(), Status::Home);
        assert_eq!(mic.order(Order::Home).unwrap(), Status::Home);
        assert!(!sends(&graph, &mut remote));
        assert_eq!(mic.order(Order::Remote).unwrap(), Status::Remote);
        assert_eq!(mic.order(Order::Remote).unwrap(), Status::Remote);
        assert!(sends(&graph, &mut remote));
    }

    #[tokio::test]
    async fn a_mode_stop_brings_the_mic_home_and_closes_the_echo_canceller() {
        let (graph, mic, mut remote, _tmp) = mic();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        let stopped = mic.clone();
        blocking(move || stopped.stop()).await;
        assert_eq!(mic.order(Order::Status).unwrap(), Status::Off);
        assert!(graph.state().mics.is_empty());
        assert_eq!(home_default(&graph), "desk-mic");
        let away = DefaultNode::named(AWAY).0;
        let desk = DefaultNode::named("desk-mic").0;
        assert_eq!(
            graph.state().log,
            [
                format!("mic {MIC} <- desk-mic"),
                format!("source {AWAY}"),
                format!("default source {away}"),
                format!("default source {desk}"),
                format!("remove {AWAY}"),
                format!("remove {MIC}"),
            ]
        );
        assert_eq!(
            orders(&mut remote),
            [Message::MicStart { session: 1 }, Message::MicStop]
        );
    }

    #[tokio::test]
    async fn with_no_mode_the_mic_cannot_be_switched() {
        let (_graph, mic, mut remote, _tmp) = mic();
        for order in [Order::Toggle, Order::Remote, Order::Home] {
            let error = mic.order(order).unwrap_err();
            assert!(error.to_string().contains("no mode is running"), "{error}");
        }
        assert_eq!(mic.order(Order::Status).unwrap(), Status::Off);
        assert!(orders(&mut remote).is_empty());
    }

    #[tokio::test]
    async fn each_mode_numbers_its_session_afresh() {
        let (_graph, mic, mut remote, _tmp) = mic();
        for _ in 0..2 {
            let cycled = mic.clone();
            blocking(move || {
                cycled.start().unwrap();
                cycled.stop();
            })
            .await;
        }
        assert_eq!(
            orders(&mut remote),
            [
                Message::MicStart { session: 1 },
                Message::MicStop,
                Message::MicStart { session: 2 },
                Message::MicStop,
            ]
        );
    }

    #[tokio::test]
    async fn a_mic_that_cannot_open_stays_home_and_tells_the_remote() {
        let (graph, mic, mut remote, _tmp) = mic();
        graph.state().refuse_mics = true;
        let started = mic.clone();
        assert!(blocking(move || started.start()).await.is_err());
        // The mode runs without it, and says why.
        assert_eq!(mic.order(Order::Status).unwrap(), Status::Failed);
        let error = mic.order(Order::Toggle).unwrap_err().to_string();
        assert!(
            error.contains("could not go to the remote machine")
                && error.contains("no echo canceller"),
            "{error}"
        );
        let stopped = mic.clone();
        blocking(move || stopped.stop()).await;
        assert_eq!(mic.order(Order::Status).unwrap(), Status::Off);
        assert_eq!(
            orders(&mut remote),
            [Message::MicStart { session: 1 }, Message::MicStop]
        );
    }

    #[tokio::test]
    async fn each_switch_shows_where_the_mic_went() {
        let (_graph, mic, _remote, notes, _tmp) = noted();
        // The mode starts: the mic goes to the remote machine.
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        assert_eq!(notes.summaries(), ["Mic → remote"]);
        mic.order(Order::Toggle).unwrap();
        mic.order(Order::Toggle).unwrap();
        mic.order(Order::Home).unwrap();
        assert_eq!(
            notes.summaries(),
            ["Mic → remote", "Mic → home", "Mic → remote", "Mic → home"]
        );
        // The mode stops with the mic already home: nothing moved, nothing to say.
        let stopped = mic.clone();
        blocking(move || stopped.stop()).await;
        assert_eq!(notes.summaries().len(), 4);
    }

    #[tokio::test]
    async fn stopping_the_mode_with_the_mic_remote_shows_it_came_home() {
        let (_graph, mic, _remote, notes, _tmp) = noted();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        let stopped = mic.clone();
        blocking(move || stopped.stop()).await;
        assert_eq!(notes.summaries(), ["Mic → remote", "Mic → home"]);
    }

    #[tokio::test]
    async fn an_order_that_changes_nothing_shows_nothing() {
        let (_graph, mic, _remote, notes, _tmp) = noted();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        mic.order(Order::Remote).unwrap();
        mic.order(Order::Status).unwrap();
        assert_eq!(notes.summaries(), ["Mic → remote"]);
    }

    #[tokio::test]
    async fn a_switch_that_fails_says_it_failed() {
        let (graph, mic, _remote, notes, _tmp) = noted();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        mic.order(Order::Home).unwrap();
        // Going back to the remote needs PipeWire: it refuses.
        graph.state().refuse_defaults = true;
        assert!(mic.order(Order::Remote).is_err());
        let shown = notes.shown();
        assert_eq!(shown.len(), 3, "{shown:?}");
        assert_eq!(shown[2].0, "Mic not switched");
        assert!(!shown[2].1.is_empty(), "{shown:?}");
    }

    #[tokio::test]
    async fn a_mic_that_cannot_start_shows_why_and_a_switch_says_it_too() {
        let (graph, mic, _remote, notes, _tmp) = noted();
        graph.state().refuse_mics = true;
        let started = mic.clone();
        assert!(blocking(move || started.start()).await.is_err());
        let shown = notes.shown();
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert_eq!(shown[0].0, "Mic failed");
        assert!(shown[0].1.contains("no echo canceller"), "{shown:?}");
        assert!(mic.order(Order::Toggle).is_err());
        assert_eq!(notes.summaries(), ["Mic failed", "Mic not switched"]);
        // The mode stops: the mic is home, and the failure is replaced.
        let stopped = mic.clone();
        blocking(move || stopped.stop()).await;
        assert_eq!(notes.summaries().last().unwrap(), "Mic → home");
    }

    #[tokio::test]
    async fn a_switch_with_no_mode_says_so() {
        let (_graph, mic, _remote, notes, _tmp) = noted();
        assert!(mic.order(Order::Toggle).is_err());
        let shown = notes.shown();
        assert_eq!(shown[0].0, "Mic not switched");
        assert!(shown[0].1.contains("no mode is running"), "{shown:?}");
        mic.order(Order::Status).unwrap();
        assert_eq!(notes.shown().len(), 1);
    }

    #[tokio::test]
    async fn a_pipewire_restart_that_the_mic_survives_shows_nothing_more() {
        let (graph, mic, _remote, notes, _tmp) = noted();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        graph.restart();
        tokio::time::sleep(CHECK * 4).await;
        assert_eq!(notes.summaries(), ["Mic → remote"]);
    }

    /// Waits for the mic to say `status`.
    async fn until_status(mic: &Mic<FakeGraph>, status: Status) {
        let mut changes = mic.watch();
        tokio::time::timeout(Duration::from_secs(5), changes.wait_for(|s| *s == status))
            .await
            .expect("the status in time")
            .unwrap();
    }

    #[tokio::test]
    async fn a_home_pipewire_restart_opens_the_mic_again() {
        let (graph, mic, mut remote, _tmp) = mic();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        graph.restart();
        tokio::time::timeout(Duration::from_secs(5), async {
            while graph.state().mics.is_empty() || graph.state().sources.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the mic opened again in time");
        assert_eq!(home_default(&graph), AWAY);
        assert!(sends(&graph, &mut remote));
        assert_eq!(mic.order(Order::Status).unwrap(), Status::Remote);
    }

    #[tokio::test]
    async fn a_mic_that_cannot_open_again_says_so_until_it_can() {
        let (graph, mic, _remote, _tmp) = mic();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        graph.state().refuse_mics = true;
        graph.restart();
        until_status(&mic, Status::Failed).await;
        let error = mic.order(Order::Toggle).unwrap_err();
        assert!(error.to_string().contains("no echo canceller"), "{error}");

        graph.state().refuse_mics = false;
        until_status(&mic, Status::Remote).await;
        assert_eq!(mic.order(Order::Toggle).unwrap(), Status::Home);
    }

    /// Fills the link with frames, as a stalled one is: nothing more goes in.
    fn stall(remote: &mpsc::Receiver<Message>, mic: &Mic<FakeGraph>) {
        let link = lock(&mic.shared.state).link.clone().unwrap();
        let frame = || Message::MicFrame {
            session: 0,
            seq: 0,
            packet: Default::default(),
        };
        while link.try_send(frame()).is_ok() {}
        assert_eq!(remote.len(), crate::audio::QUEUE);
    }

    /// The mic answers `order` within half a second.
    async fn answers(mic: &Arc<Mic<FakeGraph>>, order: Order) -> io::Result<Status> {
        let asked = mic.clone();
        let answer = blocking(move || asked.order(order));
        tokio::time::timeout(Duration::from_millis(500), answer)
            .await
            .expect("an answer in time")
    }

    #[tokio::test]
    async fn a_stop_waits_out_a_stalled_link_and_its_order_is_not_lost() {
        let (_graph, mic, mut remote, _tmp) = mic();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        stall(&remote, &mic);
        let stopped = mic.clone();
        let stop = tokio::task::spawn_blocking(move || stopped.stop());
        // Longer than any give-up, and meanwhile the mic still answers.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(answers(&mic, Order::Status).await.unwrap(), Status::Off);
        assert!(answers(&mic, Order::Toggle).await.is_err());

        let mut got = orders(&mut remote);
        stop.await.unwrap();
        got.extend(orders(&mut remote));
        assert_eq!(got, [Message::MicStart { session: 1 }, Message::MicStop]);
    }

    #[tokio::test]
    async fn the_mic_answers_while_a_start_waits_for_the_link() {
        let (_graph, mic, remote, _tmp) = mic();
        stall(&remote, &mic);
        let started = mic.clone();
        let start = tokio::task::spawn_blocking(move || started.start());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(answers(&mic, Order::Status).await.unwrap(), Status::Off);
        assert!(answers(&mic, Order::Toggle).await.is_err());
        drop(remote);
        let _ = start.await;
    }

    #[tokio::test]
    async fn without_a_link_the_mic_cannot_start() {
        let tmp = tempfile::tempdir().unwrap();
        let saved = Saved::in_dir(tmp.path().to_owned());
        let notes = Arc::new(Fake::default());
        let mic = Arc::new(Mic::new(FakeGraph::default(), saved, String::new(), notes));
        let started = mic.clone();
        let error = blocking(move || started.start()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
        assert_eq!(mic.order(Order::Status).unwrap(), Status::Failed);
    }

    #[tokio::test]
    async fn a_new_link_takes_the_next_mode() {
        let (_graph, mic, mut old, _tmp) = mic();
        let (link, mut new) = mpsc::channel(crate::audio::QUEUE);
        mic.attach(link);
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        assert!(orders(&mut old).is_empty());
        assert_eq!(orders(&mut new), [Message::MicStart { session: 1 }]);
    }

    #[test]
    fn the_status_is_a_word_or_a_line_of_json_a_bar_reads() {
        assert_eq!(Status::Remote.word(), "remote");
        assert_eq!(
            Status::Home.json(),
            r#"{"text":"home","alt":"home","class":"home","tooltip":"The mic is home"}"#
        );
        assert_eq!(Status::Failed.word(), "failed");
        for status in [Status::Off, Status::Home, Status::Remote, Status::Failed] {
            let json: serde_json::Value = serde_json::from_str(&status.json()).unwrap();
            assert_eq!(json["alt"], status.word());
        }
    }

    #[test]
    fn the_hotkey_runs_this_binary_whatever_its_path() {
        assert_eq!(
            toggle_command(Path::new("/opt/invented dir/it's/navette")),
            r"'/opt/invented dir/it'\''s/navette' mic toggle"
        );
    }

    /// A control socket served in a temporary directory.
    fn served(mic: Arc<dyn Routing>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mic.sock");
        let listener = crate::mode::listen(&path).unwrap();
        tokio::spawn(serve(listener, mic));
        (dir, path)
    }

    #[tokio::test]
    async fn navette_mic_switches_it_over_the_control_socket() {
        let (_graph, mic, _remote, _tmp) = mic();
        let (_dir, path) = served(mic.clone());
        let error = ask(&path, Order::Toggle).await.unwrap_err();
        assert!(error.to_string().contains("no mode is running"), "{error}");

        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        assert_eq!(ask(&path, Order::Status).await.unwrap(), Status::Remote);
        assert_eq!(ask(&path, Order::Toggle).await.unwrap(), Status::Home);
        assert_eq!(ask(&path, Order::Remote).await.unwrap(), Status::Remote);
        assert_eq!(ask(&path, Order::Home).await.unwrap(), Status::Home);
    }

    #[tokio::test]
    async fn watch_hears_every_change_the_current_one_first() {
        let (_graph, mic, _remote, _tmp) = mic();
        let (_dir, path) = served(mic.clone());
        let (heard, mut hears) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            watch(&path, move |status| {
                let _ = heard.send(status);
                Ok(())
            })
            .await
        });
        let next = async |hears: &mut mpsc::UnboundedReceiver<Status>| {
            tokio::time::timeout(Duration::from_secs(5), hears.recv())
                .await
                .expect("a change in time")
                .unwrap()
        };
        assert_eq!(next(&mut hears).await, Status::Off);
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        assert_eq!(next(&mut hears).await, Status::Remote);
        mic.order(Order::Toggle).unwrap();
        assert_eq!(next(&mut hears).await, Status::Home);
        let stopped = mic.clone();
        blocking(move || stopped.stop()).await;
        assert_eq!(next(&mut hears).await, Status::Off);
    }

    #[tokio::test]
    async fn a_watcher_that_goes_away_is_noticed_without_waiting_for_a_change() {
        let (_graph, mic, _remote, _tmp) = mic();
        let (_dir, path) = served(mic.clone());
        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream.write_all(b"watch\n").await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut line)
            .await
            .unwrap();
        assert_eq!(line, "off\n");
        assert_eq!(mic.shared.status.receiver_count(), 1);

        drop(stream);
        tokio::time::timeout(Duration::from_secs(2), async {
            while mic.shared.status.receiver_count() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the watcher noticed gone");
    }

    #[tokio::test]
    async fn watch_says_off_while_serve_is_not_running_then_follows_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mic.sock");
        let (heard, mut hears) = mpsc::unbounded_channel();
        let watched = path.clone();
        tokio::spawn(async move {
            watch(&watched, move |status| {
                let _ = heard.send(status);
                Ok(())
            })
            .await
        });
        let first = tokio::time::timeout(Duration::from_secs(5), hears.recv()).await;
        assert_eq!(first.unwrap(), Some(Status::Off));

        let (_graph, mic, _remote, _tmp) = mic();
        let started = mic.clone();
        blocking(move || started.start()).await.unwrap();
        let listener = crate::mode::listen(&path).unwrap();
        tokio::spawn(serve(listener, mic));
        // Within one retry.
        let next = tokio::time::timeout(RETRY * 3, hears.recv()).await;
        assert_eq!(next.unwrap(), Some(Status::Remote));
    }

    #[tokio::test]
    async fn without_sound_the_mic_is_off_and_says_why() {
        let (_dir, path) = served(Arc::new(NoSound::default()));
        assert_eq!(ask(&path, Order::Status).await.unwrap(), Status::Off);
        let error = ask(&path, Order::Toggle).await.unwrap_err();
        assert!(error.to_string().contains("--audio"), "{error}");
    }

    #[tokio::test]
    async fn the_cli_says_when_serve_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let error = ask(&dir.path().join("mic.sock"), Order::Toggle)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not running"), "{error}");
    }
}
