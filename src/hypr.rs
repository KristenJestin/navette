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
use std::time::Duration;

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
    pub fn request(&self, request: &str) -> io::Result<String> {
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

/// The event stream; it ends when Hyprland exits.
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
