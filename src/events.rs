//! Where Navette stands, as one line of JSON per change, for a bar or a shell: `serve` on the
//! desk and `agent` on the remote machine each keep their state here, computed in one place
//! from its parts, and serve it on `$XDG_RUNTIME_DIR/navette/events.sock` (`docs/events.md`,
//! the contract other programs read).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::{oneshot, watch};

use crate::mic::{self, Routing};
use crate::mode;

/// The `modes` array, as `navette mode list --json` prints it too.
pub fn modes_json(modes: &[Defined]) -> String {
    let modes: Vec<String> = modes
        .iter()
        .map(|m| {
            format!(
                r#"{{"name":{},"monitors":{}}}"#,
                serde_json::json!(m.name),
                serde_json::json!(m.monitors)
            )
        })
        .collect();
    format!("[{}]", modes.join(","))
}

/// The contract's version, said on every line: it changes only when a field or a value changes
/// meaning or goes away, never for a new one (`docs/events.md`).
pub const VERSION: u32 = 1;

/// Which machine says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Desk,
    Remote,
}

impl Role {
    pub fn word(self) -> &'static str {
        match self {
            Role::Desk => "desk",
            Role::Remote => "remote",
        }
    }
}

/// Where the link stands, as this machine sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// The desk, with no remote machine linked.
    Waiting,
    /// The remote machine, off the desk's network: it does not dial.
    Away,
    /// The remote machine, on the desk's network, the desk not answering (yet).
    Dialing,
    Linked,
}

/// Where a mode is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The desk: between the order and the remote's answer. The remote: making its outputs.
    Starting,
    Running,
    /// The desk, giving the monitors back.
    Stopping,
}

/// The mode a machine drives, or is starting or stopping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Driven {
    pub phase: Phase,
    /// `None` for the unnamed mode, and on the remote, which is not told the name.
    pub name: Option<String>,
    /// The desk's monitors given, or the remote's outputs made, left to right.
    pub monitors: Vec<String>,
}

/// A mode `mode.conf` defines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Defined {
    /// `None` for the unnamed one.
    pub name: Option<String>,
    /// As `mode.conf` writes them.
    pub monitors: Vec<String>,
}

/// Everything a line says, from which it is computed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parts {
    pub role: Role,
    pub link: Link,
    pub mode: Option<Driven>,
    /// The other machine's label: `remote_label` on the desk; none on the remote, which is not
    /// told the desk's.
    pub peer: Option<String>,
    /// The desk's modes; none on the remote.
    pub modes: Vec<Defined>,
    /// Where the desk's mic goes; the remote does not know it.
    pub mic: Option<mic::Status>,
}

impl Parts {
    /// The desk as `serve` starts: no remote machine, no mode, the mic home.
    pub fn desk() -> Self {
        Self {
            role: Role::Desk,
            link: Link::Waiting,
            mode: None,
            peer: None,
            modes: Vec::new(),
            mic: Some(mic::Status::Off),
        }
    }

    /// The remote machine as `agent` starts: about to dial.
    pub fn remote() -> Self {
        Self {
            role: Role::Remote,
            link: Link::Dialing,
            mode: None,
            peer: None,
            modes: Vec::new(),
            mic: None,
        }
    }

    /// The mode the line names: one giving its monitors back, or one starting or running over
    /// a link that is up.
    fn shown(&self) -> Option<&Driven> {
        self.mode
            .as_ref()
            .filter(|d| d.phase == Phase::Stopping || self.link == Link::Linked)
    }

    /// The `state` value. A new one (the link refusing the other machine's version, to come)
    /// goes here: readers ignore the values they do not know.
    pub fn state(&self) -> &'static str {
        match self.shown().map(|d| d.phase) {
            Some(Phase::Starting) => "starting",
            Some(Phase::Running) => "mode",
            Some(Phase::Stopping) => "stopping",
            None => match self.link {
                Link::Waiting => "waiting",
                Link::Away => "away",
                Link::Dialing => "dialing",
                Link::Linked => "linked",
            },
        }
    }

    /// The line, without its newline.
    pub fn line(&self) -> String {
        use serde_json::json;
        let shown = self.shown();
        // Written field by field, in the contract's order, for a human reading it: a JSON map
        // would sort them.
        let fields = [
            ("v", json!(VERSION).to_string()),
            ("role", json!(self.role.word()).to_string()),
            ("state", json!(self.state()).to_string()),
            ("peer", json!(self.peer).to_string()),
            (
                "mode",
                json!(shown.and_then(|d| d.name.as_deref())).to_string(),
            ),
            (
                "monitors",
                json!(shown.map_or(&[][..], |d| &d.monitors)).to_string(),
            ),
            ("modes", modes_json(&self.modes)),
            ("mic", json!(self.mic.map(mic::Status::word)).to_string()),
        ];
        let fields: Vec<String> = fields
            .iter()
            .map(|(key, value)| format!("\"{key}\":{value}"))
            .collect();
        format!("{{{}}}", fields.join(","))
    }
}

/// How many lines a reader may have waiting: one that falls further behind is let go, so a
/// stalled bar never holds up `serve` or `agent`. It connects again and starts from the state.
pub const QUEUE: usize = 32;
/// How long a reader has to take a line, after which it is let go too.
const WRITE: Duration = Duration::from_secs(5);
/// The longest command line: `start` and a mode's name.
const COMMAND_LONGEST: u64 = 64;

/// The state, the one place it is kept: each change to its parts makes a line, sent to every
/// reader, unless the line is the one already said.
pub struct Events {
    inner: Mutex<Inner>,
}

struct Inner {
    parts: Parts,
    line: Arc<str>,
    readers: Vec<mpsc::Sender<Arc<str>>>,
}

impl Events {
    pub fn new(parts: Parts) -> Arc<Self> {
        let line = parts.line().into();
        Arc::new(Self {
            inner: Mutex::new(Inner {
                parts,
                line,
                readers: Vec::new(),
            }),
        })
    }

    pub fn parts(&self) -> Parts {
        self.inner.lock().unwrap().parts.clone()
    }

    /// Changes the parts; a new line goes to every reader, and a reader whose queue is full is
    /// let go. Never waits on a reader.
    pub fn update(&self, change: impl FnOnce(&mut Parts)) {
        let mut inner = self.inner.lock().unwrap();
        change(&mut inner.parts);
        let line: Arc<str> = inner.parts.line().into();
        if line == inner.line {
            return;
        }
        inner.line = line.clone();
        inner
            .readers
            .retain(|reader| reader.try_send(line.clone()).is_ok());
    }

    /// The current line first, then one per change.
    pub fn subscribe(&self) -> mpsc::Receiver<Arc<str>> {
        let (reader, lines) = mpsc::channel(QUEUE);
        let mut inner = self.inner.lock().unwrap();
        // Under the lock: no change slips between the state and the next line.
        let _ = reader.try_send(inner.line.clone());
        inner.readers.push(reader);
        lines
    }

    /// The readers still there.
    #[cfg(test)]
    fn readers(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.readers.iter().filter(|r| !r.is_closed()).count()
    }
}

/// Keeps the desk's `mic` as the mic says it, `mic watch`'s own source, until it is gone.
pub async fn follow_mic(events: Arc<Events>, mut mic: watch::Receiver<mic::Status>) {
    loop {
        let status = *mic.borrow_and_update();
        events.update(|p| p.mic = Some(status));
        if mic.changed().await.is_err() {
            return;
        }
    }
}

/// The remote machine's link after one of the agent's events.
pub fn link_after(link: Link, event: &crate::link::Event) -> Link {
    use crate::link::Event;
    match event {
        Event::Away => Link::Away,
        Event::Back | Event::Disconnected(_) => Link::Dialing,
        Event::Connected => Link::Linked,
        Event::Failed(_) | Event::Retrying(_) => link,
    }
}

/// What the commands on the socket reach.
pub enum Commands {
    /// `serve`'s orders, as `control.sock` hands them, and its mic, as `mic.sock` does.
    Desk {
        orders: UnboundedSender<mode::Order>,
        mic: Arc<dyn Routing>,
    },
    /// The remote machine drives nothing: every command is refused.
    Remote,
}

/// The refusal of every command on the remote machine.
pub const REMOTE_REFUSES: &str = "error the remote does not drive the mode";

/// `$XDG_RUNTIME_DIR/navette/events.sock`, the same on both machines.
pub fn path() -> io::Result<PathBuf> {
    Ok(mode::control_path()?.with_file_name("events.sock"))
}

/// Listens on `path`, 0600 in a 0700 directory, in place of a socket a crash left there.
pub fn listen(path: &Path) -> io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    let listener = mode::listen(path).map_err(|e| match e.kind() {
        io::ErrorKind::AddrInUse => io::Error::new(
            e.kind(),
            format!(
                "{} is taken: another `navette serve` or `navette agent` runs here",
                path.display()
            ),
        ),
        _ => e,
    })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serves each reader: the state, a line per change, and an answer to each command.
pub async fn serve(listener: UnixListener, events: Arc<Events>, commands: Arc<Commands>) {
    while let Ok((stream, _)) = listener.accept().await {
        let (read, mut write) = stream.into_split();
        let mut lines = events.subscribe();
        let (answers_tx, mut answers) = mpsc::channel(1);
        let commands = commands.clone();
        tokio::spawn(async move {
            // Commands are read and carried out on their own, one after the other, so the
            // state goes on meanwhile; anything that ends the reading ends the reader, a close
            // included, rather than at the next change, which may be hours away.
            let commanded = tokio::spawn(command(read, commands, answers_tx));
            write_out(&mut write, &mut lines, &mut answers).await;
            commanded.abort();
        });
    }
}

/// Writes a reader's state lines and answers as they come, until it is let go: for falling
/// behind, for taking no line within [`WRITE`], or once its commands are read to the end.
async fn write_out<W: tokio::io::AsyncWrite + Unpin>(
    write: &mut W,
    lines: &mut mpsc::Receiver<Arc<str>>,
    answers: &mut mpsc::Receiver<String>,
) {
    loop {
        let text = tokio::select! {
            // A state queued before an answer goes first: a `start` is answered after its
            // `starting` line, as the contract says.
            biased;
            line = lines.recv() => match line {
                Some(line) => line.to_string(),
                // Let go for falling behind.
                None => return,
            },
            answer = answers.recv() => match answer {
                Some(answer) => answer,
                None => return,
            },
        };
        let text = format!("{text}\n");
        let written = write.write_all(text.as_bytes());
        if !matches!(tokio::time::timeout(WRITE, written).await, Ok(Ok(()))) {
            return;
        }
    }
}

/// Reads a reader's commands, and hands each one's answer to its writer.
async fn command(read: OwnedReadHalf, commands: Arc<Commands>, answers: mpsc::Sender<String>) {
    let mut read = BufReader::new(read);
    loop {
        let mut line = String::new();
        match (&mut read).take(COMMAND_LONGEST).read_line(&mut line).await {
            Ok(_) if line.ends_with('\n') => {}
            // The end, an endless line, or no text.
            _ => return,
        }
        let answer = answer(&commands, line.trim()).await;
        if answers.send(answer).await.is_err() {
            return;
        }
    }
}

/// One command's answer, by the very handlers of `control.sock` and `mic.sock`.
async fn answer(commands: &Commands, line: &str) -> String {
    let Commands::Desk { orders, mic } = commands else {
        return REMOTE_REFUSES.to_string();
    };
    let unknown = || format!("error unknown command: {line}");
    if let Some(word) = line.strip_prefix("mic ") {
        // `home` is the word of `navette mic` before the desk was called the desk.
        let order = match word.trim() {
            "toggle" => mic::Order::Toggle,
            "remote" => mic::Order::Remote,
            "desk" | "home" => mic::Order::Home,
            _ => return unknown(),
        };
        return mic::answer(mic.clone(), order).await;
    }
    if line == "status" {
        return unknown();
    }
    let (reply, answer) = oneshot::channel();
    match mode::order_of(line, reply) {
        Some(order) => mode::answered(orders, order, answer)
            .await
            .unwrap_or_else(|| "error navette is stopping".to_string()),
        None => unknown(),
    }
}

/// What `navette watch` says while neither `serve` nor `agent` runs here.
pub const OFF: &str = r#"{"v":1,"state":"off"}"#;
/// How often `navette watch` looks for a socket that is not there.
const RETRY: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(2)
};

/// `navette watch`: hands `each` the state line, then one per change, for ever; [`OFF`] while
/// nothing serves the socket, until it is back. With `once`, the current line only.
pub async fn watch(
    path: &Path,
    once: bool,
    mut each: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<()> {
    let mut last: Option<String> = None;
    let mut tell = |line: &str| -> io::Result<()> {
        if last.as_deref() != Some(line) {
            last = Some(line.to_string());
            each(line)?;
        }
        Ok(())
    };
    loop {
        // Never written to: the socket stays open both ways for as long as it reads.
        let mut heard = false;
        if let Ok(stream) = UnixStream::connect(path).await {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // A state line; an answer to a command starts with `ok` or `error`.
                if !line.starts_with('{') {
                    continue;
                }
                heard = true;
                tell(&line)?;
                if once {
                    return Ok(());
                }
            }
        }
        // Let go after a state, for falling behind or by a stop: connect again at once, and
        // say `off` only if nothing answers.
        if heard && !once {
            continue;
        }
        tell(OFF)?;
        if once {
            return Ok(());
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// A state line as a Waybar custom module's (`text`, `alt`, `class`, `tooltip`); none for a
/// line that is not one. Unknown fields are ignored, and an unknown state passes as it is.
pub fn waybar(line: &str) -> Option<String> {
    use serde_json::{Value, json};
    let line: Value = serde_json::from_str(line).ok()?;
    let state = line.get("state")?.as_str()?;
    let text_of = |key: &str| line.get(key).and_then(Value::as_str);
    let desk = text_of("role") == Some("desk");
    let monitors = line
        .get("monitors")
        .and_then(Value::as_array)
        .map(|m| {
            m.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let other = match (text_of("peer"), desk) {
        (Some(peer), _) => peer.to_string(),
        (None, true) => "the remote machine".to_string(),
        (None, false) => "the desk".to_string(),
    };
    let title = match text_of("mode") {
        Some(name) => format!("mode {name}"),
        None => "the mode".to_string(),
    };
    let (text, tooltip) = match state {
        "off" => (state, "Navette is not running here".to_string()),
        "waiting" => (state, format!("Waiting for {other}")),
        "away" => (
            state,
            "Not on the desk's network: not dialing it".to_string(),
        ),
        "dialing" => (
            state,
            "Dialing the desk, which does not answer yet".to_string(),
        ),
        "linked" => (
            state,
            format!("{} is linked, no mode runs", capital(&other)),
        ),
        "starting" if desk => (state, format!("Starting {title} on {monitors}")),
        "starting" => (state, format!("Making {monitors} for the desk")),
        "mode" if desk => {
            let mut tooltip = format!("{} runs on {monitors}", capital(&title));
            if let Some(mic) = text_of("mic") {
                tooltip.push_str(&format!("; mic: {mic}"));
            }
            (text_of("mode").unwrap_or(state), tooltip)
        }
        "mode" => (state, format!("The desk drives {monitors}")),
        "stopping" => (state, format!("Giving {monitors} back")),
        // A state of a later version: shown as it is.
        _ => (state, state.to_string()),
    };
    Some(format!(
        r#"{{"text":{},"alt":{},"class":{},"tooltip":{}}}"#,
        json!(text),
        json!(state),
        json!(state),
        json!(tooltip)
    ))
}

fn capital(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// A state line as a word, for a human: its state.
pub fn word(line: &str) -> Option<String> {
    let line: serde_json::Value = serde_json::from_str(line).ok()?;
    Some(line.get("state")?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
    use tokio::net::unix::OwnedWriteHalf;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::mic::NoSound;

    fn office() -> Driven {
        Driven {
            phase: Phase::Running,
            name: Some("office".into()),
            monitors: vec!["DP-2".into()],
        }
    }

    fn desk_in(link: Link, mode: Option<Driven>) -> Parts {
        Parts {
            link,
            mode,
            peer: Some("FAR".into()),
            modes: vec![Defined {
                name: Some("office".into()),
                monitors: vec!["DP-2".into()],
            }],
            mic: Some(mic::Status::Remote),
            ..Parts::desk()
        }
    }

    #[test]
    fn the_desk_running_a_mode_says_the_issue_s_example() {
        assert_eq!(
            desk_in(Link::Linked, Some(office())).line(),
            r#"{"v":1,"role":"desk","state":"mode","peer":"FAR","mode":"office","monitors":["DP-2"],"modes":[{"name":"office","monitors":["DP-2"]}],"mic":"remote"}"#
        );
    }

    #[test]
    fn each_desk_state_is_named() {
        let phase = |phase| Some(Driven { phase, ..office() });
        let said = |parts: Parts| parts.state();
        assert_eq!(said(desk_in(Link::Waiting, None)), "waiting");
        assert_eq!(said(desk_in(Link::Linked, None)), "linked");
        assert_eq!(
            said(desk_in(Link::Linked, phase(Phase::Starting))),
            "starting"
        );
        assert_eq!(said(desk_in(Link::Linked, phase(Phase::Running))), "mode");
        assert_eq!(
            said(desk_in(Link::Linked, phase(Phase::Stopping))),
            "stopping"
        );
        // A drop gives the monitors back: still stopping, though the link is gone.
        assert_eq!(
            said(desk_in(Link::Waiting, phase(Phase::Stopping))),
            "stopping"
        );
    }

    #[test]
    fn each_remote_state_is_named() {
        let remote = |link, mode| Parts {
            link,
            mode,
            ..Parts::remote()
        };
        let made = |phase| {
            Some(Driven {
                phase,
                name: None,
                monitors: vec!["NAVETTE-1".into()],
            })
        };
        assert_eq!(remote(Link::Away, None).state(), "away");
        assert_eq!(remote(Link::Dialing, None).state(), "dialing");
        assert_eq!(remote(Link::Linked, None).state(), "linked");
        assert_eq!(
            remote(Link::Linked, made(Phase::Starting)).state(),
            "starting"
        );
        assert_eq!(remote(Link::Linked, made(Phase::Running)).state(), "mode");
        // The link went before the outputs: no mode is driven any more.
        assert_eq!(
            remote(Link::Dialing, made(Phase::Running)).state(),
            "dialing"
        );
    }

    #[test]
    fn with_no_mode_shown_the_mode_is_null_and_the_monitors_empty() {
        let dropped = desk_in(
            Link::Waiting,
            Some(Driven {
                phase: Phase::Starting,
                ..office()
            }),
        );
        let line: serde_json::Value = serde_json::from_str(&dropped.line()).unwrap();
        assert_eq!(line["state"], "waiting");
        assert_eq!(line["mode"], serde_json::Value::Null);
        assert_eq!(line["monitors"], serde_json::json!([]));
    }

    #[test]
    fn the_remote_says_null_for_what_it_is_not_told() {
        let parts = Parts {
            link: Link::Linked,
            mode: Some(Driven {
                phase: Phase::Running,
                name: None,
                monitors: vec!["NAVETTE-1".into(), "NAVETTE-2".into()],
            }),
            ..Parts::remote()
        };
        assert_eq!(
            parts.line(),
            r#"{"v":1,"role":"remote","state":"mode","peer":null,"mode":null,"monitors":["NAVETTE-1","NAVETTE-2"],"modes":[],"mic":null}"#
        );
    }

    #[test]
    fn the_unnamed_mode_and_odd_labels_are_valid_json() {
        let parts = Parts {
            peer: Some("A \"quoted\" \\ label".into()),
            modes: vec![Defined {
                name: None,
                monitors: vec!["desc:Invented Display 27".into()],
            }],
            ..Parts::desk()
        };
        let line: serde_json::Value = serde_json::from_str(&parts.line()).unwrap();
        assert_eq!(line["peer"], "A \"quoted\" \\ label");
        assert_eq!(
            line["modes"],
            serde_json::json!([{"name": null, "monitors": ["desc:Invented Display 27"]}])
        );
        assert_eq!(line["mic"], "off");
    }

    fn linked_desk() -> Parts {
        desk_in(Link::Linked, None)
    }

    /// `events.sock` served from `events` in a temporary directory.
    fn served(events: Arc<Events>, commands: Commands) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navette").join("events.sock");
        let listener = listen(&path).unwrap();
        tokio::spawn(serve(listener, events, Arc::new(commands)));
        (dir, path)
    }

    struct Reader {
        lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        write: OwnedWriteHalf,
    }

    impl Reader {
        async fn connect(path: &Path) -> Self {
            let (read, write) = UnixStream::connect(path).await.unwrap().into_split();
            Self {
                lines: BufReader::new(read).lines(),
                write,
            }
        }

        async fn next(&mut self) -> String {
            tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
                .await
                .expect("a line in time")
                .unwrap()
                .expect("the stream goes on")
        }

        async fn state(&mut self) -> String {
            let line: serde_json::Value = serde_json::from_str(&self.next().await).unwrap();
            line["state"].as_str().unwrap().to_string()
        }

        async fn say(&mut self, command: &str) {
            self.write
                .write_all(format!("{command}\n").as_bytes())
                .await
                .unwrap();
        }

        /// Nothing more within a moment.
        async fn quiet(&mut self) {
            let next = tokio::time::timeout(Duration::from_millis(200), self.lines.next_line());
            assert!(next.await.is_err(), "a line without a change");
        }
    }

    fn remote_commands() -> Commands {
        Commands::Remote
    }

    /// A desk whose `serve` answers each order with what it was, and plays no sound.
    fn desk_commands() -> (Commands, UnboundedReceiver<String>) {
        let (orders, mut taken) = mpsc::unbounded_channel();
        let (said, heard) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(order) = taken.recv().await {
                let (what, reply) = match order {
                    mode::Order::Start(name, reply) => (format!("start {name:?}"), reply),
                    mode::Order::Stop(reply) => ("stop".to_string(), reply),
                    mode::Order::Status(reply) => ("status".to_string(), reply),
                };
                let _ = said.send(what.clone());
                let _ = reply.send(Ok(format!("did {what}")));
            }
        });
        let mic: Arc<dyn Routing> = Arc::new(NoSound::default());
        (Commands::Desk { orders, mic }, heard)
    }

    #[tokio::test]
    async fn the_first_line_is_the_current_state_on_each_side() {
        let desk = Events::new(linked_desk());
        let (_dir, path) = served(desk.clone(), desk_commands().0);
        let mut reader = Reader::connect(&path).await;
        assert_eq!(reader.next().await, linked_desk().line());

        let away = Parts {
            link: Link::Away,
            ..Parts::remote()
        };
        let remote = Events::new(away.clone());
        let (_dir, path) = served(remote, remote_commands());
        let mut reader = Reader::connect(&path).await;
        assert_eq!(reader.next().await, away.line());
    }

    #[tokio::test]
    async fn each_change_gives_one_line_and_no_line_comes_without_one() {
        let events = Events::new(Parts::desk());
        let (_dir, path) = served(events.clone(), desk_commands().0);
        let mut reader = Reader::connect(&path).await;
        assert_eq!(reader.state().await, "waiting");
        events.update(|p| p.link = Link::Linked);
        assert_eq!(reader.state().await, "linked");
        // Said again as it is: nothing new.
        events.update(|p| p.link = Link::Linked);
        events.update(|_| {});
        reader.quiet().await;
        events.update(|p| p.mic = Some(mic::Status::Remote));
        let line: serde_json::Value = serde_json::from_str(&reader.next().await).unwrap();
        assert_eq!(line["mic"], "remote");
    }

    #[tokio::test]
    async fn two_readers_get_the_same_lines() {
        let events = Events::new(Parts::desk());
        let (_dir, path) = served(events.clone(), desk_commands().0);
        let mut first = Reader::connect(&path).await;
        let mut second = Reader::connect(&path).await;
        assert_eq!(first.state().await, "waiting");
        assert_eq!(second.state().await, "waiting");
        events.update(|p| p.link = Link::Linked);
        events.update(|p| p.link = Link::Waiting);
        for reader in [&mut first, &mut second] {
            assert_eq!(reader.state().await, "linked");
            assert_eq!(reader.state().await, "waiting");
        }
    }

    #[tokio::test]
    async fn a_command_answers_and_the_stream_goes_on() {
        let events = Events::new(linked_desk());
        let (commands, mut heard) = desk_commands();
        let (_dir, path) = served(events.clone(), commands);
        let mut reader = Reader::connect(&path).await;
        assert_eq!(reader.state().await, "linked");

        reader.say("start office").await;
        assert_eq!(reader.next().await, r#"ok did start Some("office")"#);
        reader.say("start").await;
        assert_eq!(reader.next().await, "ok did start None");
        reader.say("stop").await;
        assert_eq!(reader.next().await, "ok did stop");
        // The mic answers as `mic.sock` does: here, home plays no sound.
        reader.say("mic toggle").await;
        let answer = reader.next().await;
        assert!(
            answer.starts_with("error ") && answer.contains("--audio"),
            "{answer}"
        );
        for word in ["remote", "desk", "home"] {
            reader.say(&format!("mic {word}")).await;
            assert!(reader.next().await.starts_with("error "));
        }
        assert_eq!(
            std::iter::from_fn(|| heard.try_recv().ok()).collect::<Vec<_>>(),
            [r#"start Some("office")"#, "start None", "stop"]
        );

        events.update(|p| p.link = Link::Waiting);
        assert_eq!(reader.state().await, "waiting");
    }

    #[tokio::test]
    async fn an_unknown_command_is_answered_and_the_stream_goes_on() {
        let events = Events::new(linked_desk());
        let (commands, mut heard) = desk_commands();
        let (_dir, path) = served(events.clone(), commands);
        let mut reader = Reader::connect(&path).await;
        reader.next().await;
        // `status` is `control.sock`'s: the stream's first line says it.
        for command in ["status", "mic status", "start bad/name", "jump"] {
            reader.say(command).await;
            let answer = reader.next().await;
            assert!(answer.starts_with("error unknown command"), "{answer}");
        }
        assert!(heard.try_recv().is_err());
        events.update(|p| p.link = Link::Waiting);
        assert_eq!(reader.state().await, "waiting");
    }

    #[tokio::test]
    async fn a_command_on_the_remote_is_refused() {
        let events = Events::new(Parts::remote());
        let (_dir, path) = served(events.clone(), remote_commands());
        let mut reader = Reader::connect(&path).await;
        assert_eq!(reader.state().await, "dialing");
        for command in [
            "start",
            "start office",
            "stop",
            "mic toggle",
            "mic desk",
            "jump",
        ] {
            reader.say(command).await;
            assert_eq!(reader.next().await, REMOTE_REFUSES);
        }
        events.update(|p| p.link = Link::Linked);
        assert_eq!(reader.state().await, "linked");
    }

    #[tokio::test]
    async fn an_endless_command_line_ends_that_reader_only() {
        let events = Events::new(linked_desk());
        let (_dir, path) = served(events.clone(), desk_commands().0);
        let mut endless = Reader::connect(&path).await;
        let mut other = Reader::connect(&path).await;
        endless.next().await;
        other.next().await;
        endless.say(&"x".repeat(1000)).await;
        let ended = tokio::time::timeout(Duration::from_secs(5), endless.lines.next_line()).await;
        assert!(matches!(ended, Ok(Ok(None)) | Ok(Err(_))), "{ended:?}");
        events.update(|p| p.link = Link::Waiting);
        assert_eq!(other.state().await, "waiting");
    }

    #[tokio::test]
    async fn a_reader_closing_mid_stream_is_let_go() {
        let events = Events::new(Parts::desk());
        let (_dir, path) = served(events.clone(), desk_commands().0);
        let mut gone = Reader::connect(&path).await;
        let mut staying = Reader::connect(&path).await;
        gone.next().await;
        staying.next().await;
        assert_eq!(events.readers(), 2);
        drop(gone);
        // Noticed without waiting for a change, which may be hours away.
        tokio::time::timeout(Duration::from_secs(5), async {
            while events.readers() > 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the reader let go");
        events.update(|p| p.link = Link::Linked);
        assert_eq!(staying.state().await, "linked");
    }

    #[test]
    fn a_reader_that_falls_behind_is_let_go_without_holding_anyone_up() {
        let events = Events::new(Parts::desk());
        let mut stalled = events.subscribe();
        let mut reading = events.subscribe();
        assert!(reading.try_recv().is_ok());
        for i in 0..QUEUE * 2 {
            // Never waits: a stalled reader holds up neither `serve` nor the other readers.
            events.update(|p| p.peer = Some(format!("PEER-{i}")));
            assert!(reading.try_recv().is_ok());
        }
        // What it had queued, the first line included, then the end: it connects again.
        let kept = std::iter::from_fn(|| stalled.try_recv().ok()).count();
        assert_eq!(kept, QUEUE);
        assert!(matches!(
            stalled.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        assert_eq!(events.readers(), 1);
    }

    #[tokio::test]
    async fn a_socket_left_by_a_crash_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navette").join("events.sock");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let events = Events::new(linked_desk());
        tokio::spawn(serve(
            listen(&path).unwrap(),
            events,
            Arc::new(Commands::Remote),
        ));
        let mut reader = Reader::connect(&path).await;
        assert_eq!(reader.state().await, "linked");
    }

    #[tokio::test]
    async fn a_second_one_running_is_not_replaced() {
        let events = Events::new(linked_desk());
        let (_dir, path) = served(events, Commands::Remote);
        let error = listen(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(error.to_string().contains("events.sock"), "{error}");
    }

    #[tokio::test]
    async fn the_socket_is_the_user_s_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = served(Events::new(Parts::desk()), Commands::Remote);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[tokio::test]
    async fn a_mic_switch_gives_one_line() {
        let events = Events::new(linked_desk());
        let (_dir, path) = served(events.clone(), desk_commands().0);
        let mut reader = Reader::connect(&path).await;
        assert_eq!(reader.next().await, linked_desk().line());
        let (mic, status) = watch::channel(mic::Status::Remote);
        tokio::spawn(follow_mic(events.clone(), status));
        // Where it stands when followed is said at once: already so here.
        reader.quiet().await;
        mic.send(mic::Status::Home).unwrap();
        let line: serde_json::Value = serde_json::from_str(&reader.next().await).unwrap();
        assert_eq!(line["mic"], "home");
        assert_eq!(line["state"], "linked");
        mic.send(mic::Status::Failed).unwrap();
        let line: serde_json::Value = serde_json::from_str(&reader.next().await).unwrap();
        assert_eq!(line["mic"], "failed");
        reader.quiet().await;
    }

    #[test]
    fn the_agent_s_events_move_its_link() {
        use crate::link::Event;
        let error = || io::Error::other("invented");
        let after = |link, event| link_after(link, &event);
        assert_eq!(after(Link::Dialing, Event::Away), Link::Away);
        assert_eq!(after(Link::Away, Event::Back), Link::Dialing);
        assert_eq!(after(Link::Dialing, Event::Connected), Link::Linked);
        assert_eq!(
            after(Link::Linked, Event::Disconnected(error())),
            Link::Dialing
        );
        // A dial that failed, and the wait before the next, change nothing.
        assert_eq!(after(Link::Dialing, Event::Failed(error())), Link::Dialing);
        assert_eq!(
            after(Link::Dialing, Event::Retrying(Duration::from_secs(1))),
            Link::Dialing
        );
        // What the agent could not save of its first link: it is linked all the same.
        assert_eq!(after(Link::Linked, Event::Failed(error())), Link::Linked);
    }

    fn waybar_of(line: &str) -> serde_json::Value {
        serde_json::from_str(&waybar(line).unwrap()).unwrap()
    }

    #[test]
    fn waybar_maps_each_state() {
        let desk = |state: &str, mode: &str| {
            format!(
                r#"{{"v":1,"role":"desk","state":"{state}","peer":"FAR","mode":{mode},"monitors":["DP-2"],"modes":[],"mic":"remote"}}"#
            )
        };
        let remote = |state: &str| {
            format!(
                r#"{{"v":1,"role":"remote","state":"{state}","peer":null,"mode":null,"monitors":[],"modes":[],"mic":null}}"#
            )
        };
        let cases = [
            (OFF.to_string(), "off", "Navette is not running here"),
            (desk("waiting", "null"), "waiting", "Waiting for FAR"),
            (
                desk("linked", "null"),
                "linked",
                "FAR is linked, no mode runs",
            ),
            (
                desk("starting", r#""office""#),
                "starting",
                "Starting mode office on DP-2",
            ),
            (
                desk("mode", r#""office""#),
                "office",
                "Mode office runs on DP-2; mic: remote",
            ),
            (
                desk("mode", "null"),
                "mode",
                "The mode runs on DP-2; mic: remote",
            ),
            (
                desk("stopping", r#""office""#),
                "stopping",
                "Giving DP-2 back",
            ),
            (
                remote("away"),
                "away",
                "Not on the desk's network: not dialing it",
            ),
            (
                remote("dialing"),
                "dialing",
                "Dialing the desk, which does not answer yet",
            ),
            (
                remote("linked"),
                "linked",
                "The desk is linked, no mode runs",
            ),
        ];
        for (line, text, tooltip) in cases {
            let module = waybar_of(&line);
            let state = serde_json::from_str::<serde_json::Value>(&line).unwrap()["state"].clone();
            assert_eq!(module["text"], text, "{line}");
            assert_eq!(module["alt"], state, "{line}");
            assert_eq!(module["class"], state, "{line}");
            assert_eq!(module["tooltip"], tooltip, "{line}");
        }
        let starting = remote("starting").replace("[]", r#"["NAVETTE-1"]"#);
        assert_eq!(
            waybar_of(&starting)["tooltip"],
            "Making NAVETTE-1 for the desk"
        );
        let driven = remote("mode").replace("[]", r#"["NAVETTE-1","NAVETTE-2"]"#);
        assert_eq!(waybar_of(&driven)["text"], "mode");
        assert_eq!(
            waybar_of(&driven)["tooltip"],
            "The desk drives NAVETTE-1, NAVETTE-2"
        );
    }

    #[test]
    fn an_unknown_field_or_value_does_not_break_watch_s_parser() {
        let line = r#"{"v":1,"role":"desk","state":"incompatible","peer":"FAR","peer_version":"9.9.9","new":{"a":[1]},"mode":null,"monitors":[],"modes":[],"mic":"off"}"#;
        let module = waybar_of(line);
        assert_eq!(module["text"], "incompatible");
        assert_eq!(module["class"], "incompatible");
        assert_eq!(word(line).unwrap(), "incompatible");
        let mut known = Parts::desk().line();
        known.insert_str(1, r#""later":true,"#);
        assert_eq!(waybar_of(&known)["class"], "waiting");
        assert_eq!(word(&known).unwrap(), "waiting");
        assert_eq!(waybar("not json"), None);
        assert_eq!(waybar("ok DP-2 is back home"), None);
    }

    /// `watch` on `path`, its lines handed over as they come.
    fn watching(
        path: &Path,
        once: bool,
    ) -> (
        tokio::task::JoinHandle<io::Result<()>>,
        UnboundedReceiver<String>,
    ) {
        let (heard, hears) = mpsc::unbounded_channel();
        let path = path.to_path_buf();
        let task = tokio::spawn(async move {
            watch(&path, once, move |line| {
                let _ = heard.send(line.to_string());
                Ok(())
            })
            .await
        });
        (task, hears)
    }

    async fn heard(hears: &mut UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(5), hears.recv())
            .await
            .expect("a line in time")
            .unwrap()
    }

    #[tokio::test]
    async fn watch_prints_off_with_no_socket_and_picks_the_stream_up_when_it_appears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navette").join("events.sock");
        let (_task, mut hears) = watching(&path, false);
        assert_eq!(heard(&mut hears).await, OFF);
        // Retrying says nothing new.
        tokio::time::sleep(RETRY * 3).await;
        assert!(hears.try_recv().is_err());

        let events = Events::new(linked_desk());
        let listener = listen(&path).unwrap();
        let served = tokio::spawn(serve(listener, events.clone(), Arc::new(Commands::Remote)));
        assert_eq!(heard(&mut hears).await, linked_desk().line());
        events.update(|p| p.link = Link::Waiting);
        assert_eq!(heard(&mut hears).await, events.parts().line());

        // It goes away: off, then back.
        served.abort();
        let _ = served.await;
        drop(events);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(heard(&mut hears).await, OFF);
        let events = Events::new(Parts::remote());
        tokio::spawn(serve(
            listen(&path).unwrap(),
            events,
            Arc::new(Commands::Remote),
        ));
        assert_eq!(heard(&mut hears).await, Parts::remote().line());
    }

    #[tokio::test]
    async fn watch_relays_a_line_with_an_unknown_field_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sock");
        let listener = listen(&path).unwrap();
        let later = r#"{"v":1,"role":"desk","state":"later","peer":null,"surprise":[1,{"x":2}]}"#;
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(format!("{later}\n").as_bytes())
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let (_task, mut hears) = watching(&path, false);
        assert_eq!(heard(&mut hears).await, later);
    }

    #[tokio::test]
    async fn watch_once_prints_the_current_line_or_off_and_returns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navette").join("events.sock");
        let (task, mut hears) = watching(&path, true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("done at once")
            .unwrap()
            .unwrap();
        assert_eq!(heard(&mut hears).await, OFF);
        assert!(hears.recv().await.is_none());

        let (_dir, path) = served(Events::new(linked_desk()), Commands::Remote);
        let (task, mut hears) = watching(&path, true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("done at once")
            .unwrap()
            .unwrap();
        assert_eq!(heard(&mut hears).await, linked_desk().line());
        assert!(hears.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_state_line_queued_before_an_answer_is_written_first() {
        // Many times over: an unordered choice would pass some of them.
        for _ in 0..64 {
            let (mut write, read) = tokio::io::duplex(4096);
            let (line_tx, mut lines) = mpsc::channel::<Arc<str>>(QUEUE);
            let (answer_tx, mut answers) = mpsc::channel(1);
            line_tx
                .try_send(Arc::from("{\"state\":\"starting\"}"))
                .unwrap();
            answer_tx.try_send("ok started".to_string()).unwrap();
            // The hub keeps its end; the reader's commands are read to the end.
            drop(answer_tx);
            write_out(&mut write, &mut lines, &mut answers).await;
            drop(line_tx);
            drop(write);
            let mut said = BufReader::new(read).lines();
            assert_eq!(
                said.next_line().await.unwrap().unwrap(),
                "{\"state\":\"starting\"}"
            );
            assert_eq!(said.next_line().await.unwrap().unwrap(), "ok started");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_that_takes_no_line_within_the_write_delay_is_let_go() {
        // Room for a few bytes, never read.
        let (mut write, _unread) = tokio::io::duplex(8);
        let (line_tx, mut lines) = mpsc::channel::<Arc<str>>(QUEUE);
        let (_answer_tx, mut answers) = mpsc::channel(1);
        line_tx.try_send(Parts::desk().line().into()).unwrap();
        let begun = tokio::time::Instant::now();
        tokio::time::timeout(WRITE * 2, write_out(&mut write, &mut lines, &mut answers))
            .await
            .expect("let go");
        assert!(
            begun.elapsed() >= WRITE,
            "let go after {:?}",
            begun.elapsed()
        );
    }

    #[tokio::test]
    async fn watch_dropped_by_the_socket_reconnects_at_once_without_saying_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sock");
        let listener = listen(&path).unwrap();
        let (first, second) = (linked_desk().line(), Parts::desk().line());
        let lines = [first.clone(), second.clone()];
        tokio::spawn(async move {
            // The first connection is let go after its line, as a reader that fell behind.
            let mut kept = Vec::new();
            for line in lines {
                let (mut stream, _) = listener.accept().await.unwrap();
                stream
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .unwrap();
                kept.push(stream);
                kept.remove(0);
            }
            std::future::pending::<()>().await;
        });
        let (_task, mut hears) = watching(&path, false);
        assert_eq!(heard(&mut hears).await, first);
        assert_eq!(heard(&mut hears).await, second);
        tokio::time::sleep(RETRY * 3).await;
        assert!(hears.try_recv().is_err());
    }
}
