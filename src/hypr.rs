//! The Hyprland port: what Navette asks of the home compositor, behind a trait so the logic
//! around it is tested with a fake.
//!
//! The real implementation talks to the instance's sockets directly, as `hyprctl` does: the
//! command socket (`.socket.sock`) takes one request per connection and answers until it closes
//! it; the event socket (`.socket2.sock`) streams one `name>>data` line per event. Changes are
//! runtime Lua (`eval`), never written into the user's config.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use hypr_ipc::Monitor;

/// A command Hyprland did not take.
#[derive(Debug)]
pub enum HyprError {
    /// The socket could not be reached or broke mid-request.
    Io(io::Error),
    /// Hyprland answered with something other than `ok`: its error text.
    Refused(String),
}

impl fmt::Display for HyprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HyprError::Io(e) => write!(f, "Hyprland IPC: {e}"),
            HyprError::Refused(answer) => write!(f, "Hyprland refused: {answer}"),
        }
    }
}

impl std::error::Error for HyprError {}

impl From<io::Error> for HyprError {
    fn from(e: io::Error) -> Self {
        HyprError::Io(e)
    }
}

/// The home compositor, as Navette uses it.
pub trait Hyprland {
    /// Runs a Lua chunk at runtime (`hyprctl eval`).
    fn eval(&mut self, lua: &str) -> Result<(), HyprError>;
}

/// An event from `.socket2.sock` that Navette acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The config was reloaded: every runtime rule is gone.
    ConfigReloaded,
    MonitorAdded(String),
    MonitorRemoved(String),
    /// Anything else, ignored.
    Other,
}

impl Event {
    /// Parses one line of the event socket, without its newline.
    pub fn parse(line: &str) -> Event {
        let (name, data) = line.split_once(">>").unwrap_or((line, ""));
        match name {
            "configreloaded" => Event::ConfigReloaded,
            "monitoradded" => Event::MonitorAdded(data.to_owned()),
            "monitorremoved" => Event::MonitorRemoved(data.to_owned()),
            _ => Event::Other,
        }
    }
}

/// A running Hyprland instance, reached through its sockets.
#[derive(Clone, Debug)]
pub struct HyprIpc {
    dir: PathBuf,
}

/// Hyprland 0.56.2 reads a request 1023 bytes at a time, on its main thread, and takes a shorter
/// read as the end (`hyprCtlFDTick` in `src/debug/HyprCtl.cpp`). A request of 1023 bytes or more
/// can be cut, and one whose length is a multiple of 1023 blocks the compositor until the client
/// gives up (measured: no answer in 8 s at 1023 and 2046 bytes). It also stops at a NUL byte.
const MAX_REQUEST: usize = 1022;

/// Long enough for a busy compositor, short enough that a hung one does not hang Navette.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

impl HyprIpc {
    /// The instance whose sockets are in `dir` (`$XDG_RUNTIME_DIR/hypr/<signature>`).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The instance this process runs under, from `XDG_RUNTIME_DIR` and
    /// `HYPRLAND_INSTANCE_SIGNATURE`.
    pub fn from_env() -> io::Result<Self> {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| io::Error::other(format!("{name} is not set: is Hyprland running?")))
        };
        let runtime = PathBuf::from(var("XDG_RUNTIME_DIR")?);
        Ok(Self::new(
            runtime
                .join("hypr")
                .join(var("HYPRLAND_INSTANCE_SIGNATURE")?),
        ))
    }

    /// Sends one request to the command socket and returns the whole answer.
    ///
    /// No `[flags]/` prefix: Hyprland looks for flags only before the first space, so a `/`
    /// in an `eval` chunk is never taken for the separator.
    pub fn request(&self, request: &str) -> io::Result<String> {
        if request.len() > MAX_REQUEST || request.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a Hyprland request must be at most {MAX_REQUEST} bytes with no NUL; this one is {}",
                    request.len()
                ),
            ));
        }
        let mut stream = UnixStream::connect(self.dir.join(".socket.sock"))?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
        stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
        stream.write_all(request.as_bytes())?;
        let mut answer = String::new();
        stream.read_to_string(&mut answer)?;
        Ok(answer)
    }

    /// Subscribes to the event socket.
    pub fn events(&self) -> io::Result<Events> {
        let stream = UnixStream::connect(self.dir.join(".socket2.sock"))?;
        Ok(Events {
            lines: BufReader::new(stream),
        })
    }
}

impl Hyprland for HyprIpc {
    fn eval(&mut self, lua: &str) -> Result<(), HyprError> {
        match self.request(&format!("eval {lua}"))?.trim_end() {
            "ok" => Ok(()),
            answer => Err(HyprError::Refused(answer.to_owned())),
        }
    }
}

/// The event stream; it ends when Hyprland exits. Reconnecting to the next instance and calling
/// [`crate::desk::Desk::reapply`] is the mode runner's job (#6).
pub struct Events {
    lines: BufReader<UnixStream>,
}

impl Iterator for Events {
    type Item = io::Result<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut line = String::new();
        match self.lines.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(Ok(Event::parse(line.trim_end_matches('\n')))),
            Err(e) => Some(Err(e)),
        }
    }
}

/// `s` as a Lua string literal.
pub fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\000"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// The request port of the display (#6): the whole command socket, for the monitor list and
// the remote's outputs. It finds the running instance on every request, so after a Hyprland
// restart the next request reaches the new one.

/// Hyprland's command socket: one request, one reply.
pub trait Hypr: Send + Sync {
    fn request(&self, command: &str) -> io::Result<String>;
}

/// Every monitor, disabled ones included.
pub fn monitors(hypr: &dyn Hypr) -> io::Result<Vec<Monitor>> {
    let reply = hypr.request("j/monitors all")?;
    serde_json::from_str(&reply).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Hyprland's monitor list: {e}"),
        )
    })
}

/// A request whose reply must be `ok`: Hyprland has no other error channel.
pub fn dispatch(hypr: &dyn Hypr, command: &str) -> io::Result<()> {
    let reply = hypr.request(command)?;
    match reply.trim() {
        "ok" => Ok(()),
        other => Err(io::Error::other(format!(
            "Hyprland refused `{command}`: {other}"
        ))),
    }
}

/// The running Hyprland: the one this process was started in while it answers, else the
/// newest that does. `hypr_ipc::Instance::discover` keeps to the inherited signature, which
/// names a dead instance once Hyprland has restarted.
pub fn instance() -> io::Result<hypr_ipc::Instance> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    let pinned = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok();
    find_instance(std::path::Path::new(&runtime_dir), pinned.as_deref())
}

fn find_instance(
    runtime_dir: &std::path::Path,
    pinned: Option<&str>,
) -> io::Result<hypr_ipc::Instance> {
    pinned
        .and_then(|sig| hypr_ipc::Instance::discover_in(runtime_dir, Some(sig)).ok())
        .filter(|i| UnixStream::connect(i.socket_path()).is_ok())
        .map_or_else(|| hypr_ipc::Instance::discover_in(runtime_dir, None), Ok)
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("no running Hyprland: {e}")))
}

/// The running Hyprland of this session.
#[derive(Clone, Debug, Default)]
pub struct Live;

impl Hypr for Live {
    fn request(&self, command: &str) -> io::Result<String> {
        let instance = instance()?;
        instance.request(command).map_err(|e| match e {
            hypr_ipc::Error::Io(e) => e,
            other => io::Error::other(other.to_string()),
        })
    }
}

/// Follows the event socket (`.socket2.sock`) of whichever Hyprland runs, on its own thread,
/// across restarts. Ends when `events` is dropped.
pub fn watch_events(events: tokio::sync::mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || {
        while !events.is_closed() {
            let socket = instance()
                .map(|i| i.dir.join(".socket2.sock"))
                .and_then(std::os::unix::net::UnixStream::connect);
            let Ok(socket) = socket else {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            };
            for line in io::BufReader::new(socket).lines().map_while(Result::ok) {
                let event = Event::parse(&line);
                if event != Event::Other && events.send(event).is_err() {
                    return;
                }
            }
        }
    });
}

/// [`Hyprland`] over a [`Hypr`], so the desk recorder reaches the same instance.
pub struct Eval(pub Arc<dyn Hypr>);

impl Hyprland for Eval {
    fn eval(&mut self, lua: &str) -> Result<(), HyprError> {
        match self.0.request(&format!("eval {lua}"))?.trim_end() {
            "ok" => Ok(()),
            answer => Err(HyprError::Refused(answer.to_owned())),
        }
    }
}

#[cfg(test)]
pub mod fake {
    //! A Hyprland that keeps its monitors in memory and answers the requests Navette sends,
    //! the way Hyprland 0.56 does (measured in #3).

    use std::sync::Mutex;

    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    pub struct FakeMonitor {
        pub name: String,
        pub width: u32,
        pub height: u32,
        pub refresh: f64,
        pub x: i32,
        pub y: i32,
        pub scale: f32,
        pub transform: i32,
    }

    impl FakeMonitor {
        pub fn new(name: &str, width: u32, height: u32, x: i32) -> Self {
            Self {
                name: name.into(),
                width,
                height,
                refresh: 60.0,
                x,
                y: 0,
                scale: 1.0,
                transform: 0,
            }
        }
    }

    #[derive(Default)]
    struct State {
        monitors: Vec<FakeMonitor>,
        /// `hl.monitor` rules by output name, as declared.
        rules: Vec<(String, FakeMonitor)>,
        requests: Vec<String>,
        /// `hl.window_rule` tables, as sent.
        window_rules: Vec<String>,
        /// The Lua chunks that bind keys or disable a bind, as sent.
        binds: Vec<String>,
        down: bool,
        /// The scale Hyprland gives a monitor whatever its rule asks (it rounds fractional ones).
        chosen_scale: Option<f32>,
    }

    #[derive(Default)]
    pub struct FakeHypr {
        state: Mutex<State>,
    }

    impl FakeHypr {
        pub fn with(monitors: Vec<FakeMonitor>) -> Self {
            let fake = Self::default();
            fake.state.lock().unwrap().monitors = monitors;
            fake
        }

        /// Requests that change something (not the monitor list).
        pub fn changes(&self) -> Vec<String> {
            let state = self.state.lock().unwrap();
            state
                .requests
                .iter()
                .filter(|r| !r.starts_with("j/"))
                .cloned()
                .collect()
        }

        /// The window rules sent, in order, `enabled = false` ones included.
        pub fn window_rules(&self) -> Vec<String> {
            self.state.lock().unwrap().window_rules.clone()
        }

        pub fn binds(&self) -> Vec<String> {
            self.state.lock().unwrap().binds.clone()
        }

        pub fn monitor(&self, name: &str) -> Option<FakeMonitor> {
            let state = self.state.lock().unwrap();
            state.monitors.iter().find(|m| m.name == name).cloned()
        }

        /// Hyprland exits (`true`) or a new one starts with only `monitors` (`false`).
        pub fn set_down(&self, down: bool, monitors: Vec<FakeMonitor>) {
            let mut state = self.state.lock().unwrap();
            state.down = down;
            state.monitors = monitors;
            state.rules.clear();
        }

        /// Every monitor rule from now on gets `scale`, as Hyprland rounds a fractional one.
        pub fn choose_scale(&self, scale: f32) {
            self.state.lock().unwrap().chosen_scale = Some(scale);
        }

        /// `hyprctl reload`: the runtime rules are gone, the outputs stay, at scale 2.
        pub fn reload(&self) {
            let mut state = self.state.lock().unwrap();
            state.rules.clear();
            for m in &mut state.monitors {
                if m.name.starts_with("NAVETTE") {
                    m.scale = 2.0;
                }
            }
        }

        fn apply_rule(state: &mut State, mut rule: FakeMonitor) {
            rule.scale = state.chosen_scale.unwrap_or(rule.scale);
            if let Some(m) = state.monitors.iter_mut().find(|m| m.name == rule.name) {
                *m = rule.clone();
            }
            state.rules.retain(|(name, _)| *name != rule.name);
            state.rules.push((rule.name.clone(), rule));
        }
    }

    /// The fields of `hl.monitor({ output = "X", mode = "WxH@R", position = "XxY", scale = S,
    /// transform = T })`, as Navette writes it.
    fn parse_rule(lua: &str) -> Option<FakeMonitor> {
        let field = |key: &str| -> Option<String> {
            let start = lua.find(&format!("{key} = "))? + key.len() + 3;
            let rest = &lua[start..];
            let end = rest.find([',', ' ', '}']).unwrap_or(rest.len());
            Some(rest[..end].trim_matches('"').to_string())
        };
        let mode = field("mode")?;
        let (size, refresh) = mode.split_once('@')?;
        let (width, height) = size.split_once('x')?;
        let position = field("position")?;
        let (x, y) = position.split_once('x')?;
        Some(FakeMonitor {
            name: field("output")?,
            width: width.parse().ok()?,
            height: height.parse().ok()?,
            refresh: refresh.parse().ok()?,
            x: x.parse().ok()?,
            y: y.parse().ok()?,
            scale: field("scale")?.parse().ok()?,
            transform: field("transform")?.parse().ok()?,
        })
    }

    impl Hypr for FakeHypr {
        fn request(&self, command: &str) -> io::Result<String> {
            let mut state = self.state.lock().unwrap();
            if state.down {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no running Hyprland",
                ));
            }
            state.requests.push(command.to_string());
            if command == "j/monitors all" {
                let list: Vec<String> = state
                    .monitors
                    .iter()
                    .enumerate()
                    .map(|(id, m)| {
                        format!(
                            r#"{{"id":{id},"name":"{}","width":{},"height":{},"refreshRate":{},"x":{},"y":{},"scale":{},"transform":{},"focused":false,"disabled":false,"activeWorkspace":{{"id":{},"name":"{}"}}}}"#,
                            m.name, m.width, m.height, m.refresh, m.x, m.y, m.scale, m.transform, id + 1, id + 1
                        )
                    })
                    .collect();
                return Ok(format!("[{}]", list.join(",")));
            }
            if let Some(rule) = command.strip_prefix("eval hl.window_rule(") {
                state
                    .window_rules
                    .push(rule.trim_end_matches(')').to_string());
                return Ok("ok".into());
            }
            if let Some(lua) = command.strip_prefix("eval ")
                && (lua.contains("hl.bind(") || lua.contains(":set_enabled(false)"))
            {
                state.binds.push(lua.to_string());
                return Ok("ok".into());
            }
            if let Some(lua) = command.strip_prefix("eval ") {
                return Ok(match parse_rule(lua) {
                    Some(rule) => {
                        Self::apply_rule(&mut state, rule);
                        "ok".into()
                    }
                    None => "error: hl.monitor: error applying field".into(),
                });
            }
            if let Some(name) = command.strip_prefix("output create headless ") {
                if state.monitors.iter().any(|m| m.name == name) {
                    return Ok("Name already taken".into());
                }
                let monitor = match state.rules.iter().find(|(n, _)| n == name) {
                    Some((_, rule)) => rule.clone(),
                    // Born at scale 2, at an automatic place.
                    None => FakeMonitor {
                        scale: 2.0,
                        ..FakeMonitor::new(name, 1920, 1080, 10_000)
                    },
                };
                state.monitors.push(monitor);
                return Ok("ok".into());
            }
            if let Some(name) = command.strip_prefix("output remove ") {
                let before = state.monitors.len();
                state.monitors.retain(|m| m.name != name);
                return Ok(if state.monitors.len() < before {
                    "ok".into()
                } else {
                    "output not found".into()
                });
            }
            Ok("unknown request".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    #[test]
    fn events_are_parsed_by_name() {
        assert_eq!(Event::parse("configreloaded>>"), Event::ConfigReloaded);
        assert_eq!(
            Event::parse("monitoradded>>DP-2"),
            Event::MonitorAdded("DP-2".into())
        );
        assert_eq!(
            Event::parse("monitorremoved>>HDMI-A-1"),
            Event::MonitorRemoved("HDMI-A-1".into())
        );
        assert_eq!(Event::parse("monitoraddedv2>>3,DP-2,desc"), Event::Other);
        assert_eq!(Event::parse("workspace>>2"), Event::Other);
        assert_eq!(Event::parse("garbage"), Event::Other);
    }

    #[test]
    fn lua_strings_are_quoted_and_escaped() {
        assert_eq!(lua_string("HOME-1"), r#""HOME-1""#);
        assert_eq!(lua_string("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
        // `\0` followed by a digit would read as another code: `\01` is byte 1.
        assert_eq!(lua_string("\u{0}1"), r#""\0001""#);
    }

    /// A stand-in for Hyprland's command socket: answers each connection with `answer` and
    /// hands back what it was sent.
    fn fake_command_socket(
        dir: &std::path::Path,
        answer: &'static str,
    ) -> thread::JoinHandle<String> {
        let listener = UnixListener::bind(dir.join(".socket.sock")).unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            stream.write_all(answer.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        })
    }

    #[test]
    fn eval_sends_the_chunk_and_accepts_ok() {
        let dir = tempfile::tempdir().unwrap();
        let server = fake_command_socket(dir.path(), "ok");
        HyprIpc::new(dir.path())
            .eval("hl.window_rule({ name = \"x\", enabled = false })")
            .unwrap();
        assert_eq!(
            server.join().unwrap(),
            "eval hl.window_rule({ name = \"x\", enabled = false })"
        );
    }

    #[test]
    fn a_slash_in_the_chunk_is_sent_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let server = fake_command_socket(dir.path(), "ok");
        let chunk = r#"hl.window_rule({ name = "a/b", match = { class = "^x/y$" } })"#;
        HyprIpc::new(dir.path()).eval(chunk).unwrap();
        assert_eq!(server.join().unwrap(), format!("eval {chunk}"));
    }

    #[test]
    fn a_request_hyprland_cannot_read_in_one_go_is_refused_before_sending() {
        let dir = tempfile::tempdir().unwrap();
        let long = format!("hl.window_rule({{ name = \"{}\" }})", "x".repeat(1100));
        let err = HyprIpc::new(dir.path()).request(&format!("eval {long}"));
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        // Exactly one full read: Hyprland waits for a next read that never comes.
        let err = HyprIpc::new(dir.path()).request(&"x".repeat(1023));
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        let err = HyprIpc::new(dir.path()).request("eval nul\0byte");
        assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn eval_reports_what_hyprland_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _server =
            fake_command_socket(dir.path(), "error: hl.window_rule: unknown field 'nope'");
        match HyprIpc::new(dir.path()).eval("hl.window_rule({ nope = 1 })") {
            Err(HyprError::Refused(answer)) => assert!(answer.contains("unknown field")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn events_are_read_until_hyprland_closes_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join(".socket2.sock")).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"workspace>>1\nconfigreloaded>>\nmonitorremoved>>DP-3\n")
                .unwrap();
        });
        let events: Vec<Event> = HyprIpc::new(dir.path())
            .events()
            .unwrap()
            .map(Result::unwrap)
            .collect();
        server.join().unwrap();
        assert_eq!(
            events,
            [
                Event::Other,
                Event::ConfigReloaded,
                Event::MonitorRemoved("DP-3".into())
            ]
        );
    }
}

#[cfg(test)]
mod port_tests {
    use super::fake::{FakeHypr, FakeMonitor};
    use super::*;

    /// An instance directory under `runtime/hypr`, its socket answering while the listener lives.
    fn instance_dir(
        runtime: &std::path::Path,
        signature: &str,
    ) -> std::os::unix::net::UnixListener {
        let dir = runtime.join("hypr").join(signature);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::net::UnixListener::bind(dir.join(".socket.sock")).unwrap()
    }

    #[test]
    fn a_signature_from_a_hyprland_since_restarted_finds_the_running_one() {
        let runtime = tempfile::tempdir().unwrap();
        // The Hyprland the agent was started in: its socket stays, nothing answers it.
        drop(instance_dir(runtime.path(), "before_1_1"));
        let _running = instance_dir(runtime.path(), "after_2_2");
        let found = find_instance(runtime.path(), Some("before_1_1")).unwrap();
        assert_eq!(found.signature, "after_2_2");
    }

    #[test]
    fn the_signature_of_a_running_hyprland_is_kept() {
        let runtime = tempfile::tempdir().unwrap();
        let _ours = instance_dir(runtime.path(), "ours_1_1");
        let _nested = instance_dir(runtime.path(), "nested_2_2");
        let found = find_instance(runtime.path(), Some("ours_1_1")).unwrap();
        assert_eq!(found.signature, "ours_1_1");
    }

    #[test]
    fn the_monitor_list_is_read_from_the_json_reply() {
        let hypr = FakeHypr::with(vec![
            FakeMonitor::new("DP-1", 1920, 1080, 0),
            FakeMonitor::new("DP-2", 2560, 1440, 1920),
        ]);
        let list = monitors(&hypr).unwrap();
        let names: Vec<_> = list
            .iter()
            .map(|m| (m.name.as_str(), m.width, m.x))
            .collect();
        assert_eq!(names, [("DP-1", 1920, 0), ("DP-2", 2560, 1920)]);
    }

    #[test]
    fn a_reply_other_than_ok_is_an_error() {
        let hypr = FakeHypr::default();
        let error = dispatch(&hypr, "output remove NOPE").unwrap_err();
        assert!(error.to_string().contains("output not found"));
    }
}
