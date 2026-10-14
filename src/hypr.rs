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
    /// A window opened or moved, a workspace moved, or a monitor shows another workspace: a
    /// window may now be on a covered monitor.
    Placed,
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
            "openwindow" | "movewindowv2" | "moveworkspacev2" | "workspacev2" => Event::Placed,
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

/// Gives the keyboard to the home window that had it last, among those shown on a monitor not
/// `covered` (Hyprland's focus history), without moving the pointer: its address, or
/// `None` when there is no such window.
pub fn focus_last_window(hypr: &dyn Hypr, covered: &[String]) -> io::Result<Option<String>> {
    let json = |request: &str| -> io::Result<serde_json::Value> {
        serde_json::from_str(&hypr.request(request)?).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Hyprland's `{request}`: {e}"),
            )
        })
    };
    let monitors = json("j/monitors all")?;
    let windows = json("j/clients")?;
    // Shown: on the workspace its monitor shows now. A window on another workspace would make
    // Hyprland switch to it.
    let shown_at_home = |window: &serde_json::Value| {
        monitors.as_array().into_iter().flatten().any(|m| {
            m["id"] == window["monitor"]
                && !covered.iter().any(|c| m["name"] == c.as_str())
                && m["activeWorkspace"]["id"] == window["workspace"]["id"]
        })
    };
    let last = windows
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["mapped"] == true && w["hidden"] == false && shown_at_home(w))
        .filter_map(|w| Some((w["focusHistoryID"].as_i64()?, w["address"].as_str()?)))
        .filter(|(history, _)| *history >= 0)
        .min();
    let Some((_, address)) = last else {
        return Ok(None);
    };
    // Hyprland's focus warps the pointer onto the window; `no_warps` keeps it over the monitor
    // for that one dispatch, in the same chunk, and is then put back as it was.
    let no_warps = json("j/getoption cursor:no_warps")?;
    let was = no_warps["int"].as_i64().map(|v| v != 0);
    let was = was.or(no_warps["bool"].as_bool()).unwrap_or(false);
    dispatch(
        hypr,
        &format!(
            "eval hl.config({{ cursor = {{ no_warps = true }} }}) \
             hl.dispatch(hl.dsp.focus({{ window = {} }})) \
             hl.config({{ cursor = {{ no_warps = {was} }} }})",
            lua_string(&format!("address:{address}"))
        ),
    )?;
    Ok(Some(address.to_string()))
}

/// Sends to the `home` monitor every home window that is on a workspace of a `covered` monitor,
/// where the cover would hide it; says what it moved (`window <address>`, `workspace <name>`).
///
/// A workspace the covered monitor does not show goes home whole, and so does the one it shows
/// when it is not the one `seen` there at the previous call: a workspace moved there whole.
/// (Hyprland says the monitor shows it before it says it moved, so the events cannot tell.)
/// The user's grouping is kept, and a workspace not shown stays out of sight. The
/// windows of the workspace it shows go home one by one, to the workspace `home` shows: moving
/// that workspace would make `home` switch to it and take the focus (measured on Hyprland
/// 0.56.2), hiding what it showed for one window dragged or opened there. Only the window that
/// has the keyboard keeps it, and brings the pointer home. Special workspaces stay: Hyprland
/// shows them over whichever monitor asks.
pub fn send_home(
    hypr: &dyn Hypr,
    covered: &[String],
    home: &str,
    seen: &mut Vec<(String, i64)>,
) -> io::Result<Vec<String>> {
    let json = |request: &str| -> io::Result<serde_json::Value> {
        serde_json::from_str(&hypr.request(request)?).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Hyprland's `{request}`: {e}"),
            )
        })
    };
    let monitors = json("j/monitors all")?;
    let workspaces = json("j/workspaces")?;
    let windows = json("j/clients")?;
    let list = |v: &serde_json::Value| v.as_array().cloned().unwrap_or_default();
    let is_covered = |name: &serde_json::Value| covered.iter().any(|c| name == c.as_str());
    let shown: Vec<(String, i64)> = list(&monitors)
        .iter()
        .filter(|m| is_covered(&m["name"]))
        .filter_map(|m| {
            let name = m["name"].as_str()?.to_owned();
            Some((name, m["activeWorkspace"]["id"].as_i64()?))
        })
        .collect();
    let shows = |id: i64| shown.iter().find(|(_, shows)| *shows == id);
    // Nothing seen yet, on the first call: what the monitors show is taken as seen.
    let moved_there =
        |(monitor, id): &(String, i64)| seen.iter().any(|(m, was)| m == monitor && was != id);
    let mut sent = Vec::new();
    for workspace in list(&workspaces) {
        let (Some(id), Some(name)) = (workspace["id"].as_i64(), workspace["name"].as_str()) else {
            continue;
        };
        if !is_covered(&workspace["monitor"]) || name.starts_with("special:") {
            continue;
        }
        let on_it: Vec<(String, bool)> = list(&windows)
            .iter()
            .filter(|w| w["workspace"]["id"] == id)
            .filter_map(|w| {
                let address = w["address"].as_str()?.to_owned();
                Some((address, w["focusHistoryID"] == 0))
            })
            .collect();
        if on_it.is_empty() {
            continue;
        }
        if shows(id).is_none_or(moved_there) {
            dispatch(
                hypr,
                &format!(
                    "eval hl.dispatch(hl.dsp.workspace.move({{ workspace = {}, monitor = {} }}))",
                    lua_string(&format!("name:{name}")),
                    lua_string(home)
                ),
            )?;
            sent.push(format!("workspace {name}"));
            continue;
        }
        for (address, has_keyboard) in on_it {
            let window = lua_string(&format!("address:{address}"));
            dispatch(
                hypr,
                &format!(
                    "eval hl.dispatch(hl.dsp.window.move({{ monitor = {}, follow = false, \
                     window = {window} }}))",
                    lua_string(home)
                ),
            )?;
            // Hyprland put the pointer on the covered monitor with the window that has the
            // keyboard (a move it followed, a drag): the focus warps it back home with it.
            if has_keyboard {
                dispatch(
                    hypr,
                    &format!("eval hl.dispatch(hl.dsp.focus({{ window = {window} }}))"),
                )?;
            }
            sent.push(format!("window {address}"));
        }
    }
    *seen = shown;
    Ok(sent)
}

/// A workspace with windows on a monitor a mode takes, as the mode starts.
#[derive(Debug)]
pub struct Held {
    pub name: String,
    pub monitor: String,
    /// The monitor shows it.
    pub shown: bool,
}

/// The workspaces with windows on the `given` monitors, those they show last; special
/// workspaces left out, as Hyprland shows them over whichever monitor asks.
pub fn held_workspaces(hypr: &dyn Hypr, given: &[String]) -> io::Result<Vec<Held>> {
    let json = |request: &str| -> io::Result<serde_json::Value> {
        serde_json::from_str(&hypr.request(request)?).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Hyprland's `{request}`: {e}"),
            )
        })
    };
    let monitors = json("j/monitors all")?;
    let workspaces = json("j/workspaces")?;
    let list = |v: &serde_json::Value| v.as_array().cloned().unwrap_or_default();
    let shows = |monitor: &str, id: &serde_json::Value| {
        list(&monitors)
            .iter()
            .any(|m| m["name"] == monitor && m["activeWorkspace"]["id"] == *id)
    };
    let mut held: Vec<Held> = list(&workspaces)
        .iter()
        .filter(|w| w["windows"].as_u64().is_some_and(|n| n > 0))
        .filter_map(|w| {
            let name = w["name"].as_str()?;
            let monitor = w["monitor"].as_str()?;
            (given.iter().any(|g| g == monitor) && !name.starts_with("special:")).then(|| Held {
                name: name.to_owned(),
                monitor: monitor.to_owned(),
                shown: shows(monitor, &w["id"]),
            })
        })
        .collect();
    held.sort_by_key(|h| h.shown);
    Ok(held)
}

/// The name of the workspace `monitor` shows.
pub fn shown_workspace(hypr: &dyn Hypr, monitor: &str) -> io::Result<Option<String>> {
    let reply = hypr.request("j/monitors all")?;
    let monitors: serde_json::Value = serde_json::from_str(&reply).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Hyprland's monitor list: {e}"),
        )
    })?;
    Ok(monitors
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["name"] == monitor)
        .and_then(|m| m["activeWorkspace"]["name"].as_str())
        .map(str::to_owned))
}

/// `monitor` shows the workspace `name`, without the focus: a runtime chunk for
/// [`crate::desk`] and [`dispatch`]. Hyprland makes the workspace when it is missing, but one
/// on another monitor must be moved first: it would be shown there while still on its own.
/// `set_workspace` takes the bare name: `name:` makes it do nothing (both measured on
/// Hyprland 0.56.2).
pub fn show_workspace(monitor: &str, name: &str) -> String {
    format!(
        "local m = hl.get_monitor({}) if m then m:set_workspace({{ workspace = {} }}) end",
        lua_string(monitor),
        lua_string(name)
    )
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
        /// Disabled by a rule: listed by `monitors all`, its mode and place kept.
        pub disabled: bool,
        /// The make, model and serial Hyprland reports.
        pub description: String,
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
                disabled: false,
                description: String::new(),
            }
        }

        pub fn described(self, description: &str) -> Self {
            Self {
                description: description.into(),
                ..self
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
        /// `cursor:invisible`, set at runtime; a reload or a restart sets it back.
        cursor_invisible: bool,
        windows: Vec<FakeWindow>,
        /// `cursor:no_warps`, as set now.
        no_warps: bool,
        /// The windows focused by a dispatch, and whether the pointer was warped onto each.
        focused: Vec<(String, bool)>,
        /// Workspaces beyond the one each monitor shows: id, name, monitor.
        workspaces: Vec<(i64, String, String)>,
        /// The workspace a monitor shows, when not its index + 1.
        shown: Vec<(String, i64)>,
        /// The user's workspace rules: workspace id, the monitor it is bound to.
        bound: Vec<(i64, String)>,
    }

    impl State {
        fn shown(&self, monitor: &str) -> i64 {
            let index = self.monitors.iter().position(|m| m.name == monitor);
            self.shown
                .iter()
                .find(|(m, _)| m == monitor)
                .map(|(_, id)| *id)
                .unwrap_or_else(|| index.map_or(0, |i| i as i64 + 1))
        }

        fn workspace_of(&self, window: &FakeWindow) -> i64 {
            match window.workspace {
                Some(id) => id,
                // A hidden workspace's id is past every monitor's shown one.
                None if window.hidden_workspace => 100,
                None => self.shown(&window.monitor),
            }
        }

        /// Moves a workspace and its windows to `to`. A shown workspace is shown where it goes,
        /// and where it was gets a new one: Hyprland does so when its monitor has the focus.
        fn move_workspace(&mut self, id: i64, name: String, from: String, to: &str) {
            for w in 0..self.windows.len() {
                if self.workspace_of(&self.windows[w]) == id {
                    self.windows[w].workspace = Some(id);
                    self.windows[w].monitor = to.to_string();
                }
            }
            self.workspaces.retain(|(i, _, _)| *i != id);
            self.workspaces.push((id, name, to.to_string()));
            if self.shown(&from) == id && from != to {
                let new = 1000 + self.workspaces.len() as i64;
                self.shown.retain(|(m, _)| *m != from && *m != to);
                self.shown.push((from, new));
                self.shown.push((to.to_string(), id));
            }
        }

        /// Every workspace: id, name, monitor.
        fn all_workspaces(&self) -> Vec<(i64, String, String)> {
            let mut all = self.workspaces.clone();
            let shown = self.monitors.iter().map(|m| (self.shown(&m.name), &m.name));
            let of_windows = self
                .windows
                .iter()
                .map(|w| (self.workspace_of(w), &w.monitor));
            for (id, monitor) in shown.chain(of_windows) {
                if !all.iter().any(|(i, _, _)| *i == id) {
                    all.push((id, id.to_string(), monitor.clone()));
                }
            }
            all
        }
    }

    /// A window, on the active workspace of its monitor unless `hidden_workspace`.
    #[derive(Clone, Debug)]
    pub struct FakeWindow {
        pub address: String,
        pub monitor: String,
        /// 0 for the window that had the keyboard last, then 1, 2…
        pub focus_history: i64,
        pub hidden_workspace: bool,
        /// Its workspace, when not one its monitor's shown or hidden one.
        pub workspace: Option<i64>,
    }

    impl FakeWindow {
        pub fn new(address: &str, monitor: &str, focus_history: i64) -> Self {
            Self {
                address: address.into(),
                monitor: monitor.into(),
                focus_history,
                hidden_workspace: false,
                workspace: None,
            }
        }

        pub fn on_workspace(self, id: i64) -> Self {
            Self {
                workspace: Some(id),
                ..self
            }
        }
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

        pub fn add_window(&self, window: FakeWindow) {
            self.state.lock().unwrap().windows.push(window);
        }

        pub fn add_workspace(&self, id: i64, name: &str, monitor: &str) {
            let mut state = self.state.lock().unwrap();
            state.workspaces.push((id, name.into(), monitor.into()));
        }

        /// `monitor` shows workspace `id`.
        pub fn show(&self, monitor: &str, id: i64) {
            let mut state = self.state.lock().unwrap();
            state.shown.retain(|(m, _)| m != monitor);
            state.shown.push((monitor.into(), id));
        }

        pub fn shown(&self, monitor: &str) -> i64 {
            self.state.lock().unwrap().shown(monitor)
        }

        /// A workspace rule of the user's: workspace `id` lives on `monitor`.
        pub fn bind_workspace(&self, id: i64, monitor: &str) {
            let mut state = self.state.lock().unwrap();
            state.bound.push((id, monitor.into()));
        }

        /// Every workspace with windows, by name, with its monitor.
        pub fn workspaces(&self) -> Vec<(String, String)> {
            let state = self.state.lock().unwrap();
            let mut all: Vec<(String, String)> = state
                .all_workspaces()
                .into_iter()
                .filter(|(id, _, _)| state.windows.iter().any(|w| state.workspace_of(w) == *id))
                .map(|(_, name, monitor)| (name, monitor))
                .collect();
            all.sort();
            all
        }

        /// The monitor and workspace a window is on.
        pub fn place(&self, address: &str) -> (String, i64) {
            let state = self.state.lock().unwrap();
            let window = state.windows.iter().find(|w| w.address == address).unwrap();
            (window.monitor.clone(), state.workspace_of(window))
        }

        pub fn set_no_warps(&self, on: bool) {
            self.state.lock().unwrap().no_warps = on;
        }

        pub fn no_warps(&self) -> bool {
            self.state.lock().unwrap().no_warps
        }

        /// The windows focused by a dispatch, each with whether the pointer was warped.
        pub fn focused(&self) -> Vec<(String, bool)> {
            self.state.lock().unwrap().focused.clone()
        }

        pub fn cursor_invisible(&self) -> bool {
            self.state.lock().unwrap().cursor_invisible
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
            state.cursor_invisible = false;
        }

        /// Every monitor rule from now on gets `scale`, as Hyprland rounds a fractional one.
        pub fn choose_scale(&self, scale: f32) {
            self.state.lock().unwrap().chosen_scale = Some(scale);
        }

        /// `hyprctl reload`: the runtime rules are gone, the outputs stay, at scale 2, and an
        /// output disabled at runtime is on again.
        pub fn reload(&self) {
            let mut state = self.state.lock().unwrap();
            // A workspace rule may take a workspace back to its monitor (Hyprland 0.56.2 was not
            // seen doing it; the fake does, the worse case).
            for (id, monitor) in state.bound.clone() {
                let all = state.all_workspaces();
                if let Some((_, name, from)) = all.into_iter().find(|(i, _, _)| *i == id)
                    && from != monitor
                {
                    state.move_workspace(id, name, from, &monitor);
                }
            }
            state.rules.clear();
            state.cursor_invisible = false;
            for m in &mut state.monitors {
                if m.name.starts_with("NAVETTE") {
                    m.scale = 2.0;
                }
                m.disabled = false;
            }
        }

        /// A rule merges into the output's: one that does not say `disabled = false` leaves a
        /// disabled output disabled (measured in #36).
        fn apply_rule(state: &mut State, mut rule: FakeMonitor, enables: bool) {
            rule.scale = state.chosen_scale.unwrap_or(rule.scale);
            if let Some(m) = state.monitors.iter_mut().find(|m| m.name == rule.name) {
                rule.disabled = m.disabled && !enables;
                *m = rule.clone();
            }
            state.rules.retain(|(name, _)| *name != rule.name);
            state.rules.push((rule.name.clone(), rule));
        }
    }

    /// The string `key = "..."` holds in a Lua table, as Navette writes it.
    fn quoted(lua: &str, key: &str) -> Option<String> {
        let start = lua.find(&format!("{key} = \""))? + key.len() + 4;
        let rest = &lua[start..];
        Some(rest[..rest.find('"')?].to_string())
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
            disabled: false,
            description: String::new(),
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
                            r#"{{"id":{id},"name":"{}","description":"{}","width":{},"height":{},"refreshRate":{},"x":{},"y":{},"scale":{},"transform":{},"focused":false,"disabled":{},"activeWorkspace":{{"id":{},"name":"{}"}}}}"#,
                            m.name, m.description, m.width, m.height, m.refresh, m.x, m.y, m.scale, m.transform,
                            m.disabled, state.shown(&m.name), state.shown(&m.name)
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
                // Keys Hyprland cannot read: invented, as its answer.
                if lua.contains("hl.bind(\"NOT A KEY") {
                    return Ok("error: hl.bind: invalid key".into());
                }
                state.binds.push(lua.to_string());
                return Ok("ok".into());
            }
            if command == "j/workspaces" {
                let list: Vec<String> = state
                    .all_workspaces()
                    .iter()
                    .map(|(id, name, monitor)| {
                        let windows = state
                            .windows
                            .iter()
                            .filter(|w| state.workspace_of(w) == *id)
                            .count();
                        format!(
                            r#"{{"id":{id},"name":"{name}","monitor":"{monitor}","windows":{windows}}}"#
                        )
                    })
                    .collect();
                return Ok(format!("[{}]", list.join(",")));
            }
            if command == "j/clients" {
                let monitors = &state.monitors;
                let list: Vec<String> = state
                    .windows
                    .iter()
                    .filter_map(|w| {
                        let id = monitors.iter().position(|m| m.name == w.monitor)?;
                        let workspace = state.workspace_of(w);
                        Some(format!(
                            r#"{{"address":"{}","mapped":true,"hidden":false,"monitor":{id},"workspace":{{"id":{workspace},"name":"{workspace}"}},"focusHistoryID":{}}}"#,
                            w.address, w.focus_history
                        ))
                    })
                    .collect();
                return Ok(format!("[{}]", list.join(",")));
            }
            if command == "j/getoption cursor:no_warps" {
                return Ok(format!(
                    r#"{{"option":"cursor:no_warps","int":{},"set":true}}"#,
                    state.no_warps as u8
                ));
            }
            if let Some(lua) = command.strip_prefix("eval ")
                && let Some(at) = lua.find("hl.dsp.focus({ window = \"address:")
            {
                // The chunk runs in order: what it sets before the focus is in effect for it.
                let set = |text: &str, now: bool| {
                    text.rfind("no_warps = ")
                        .map_or(now, |i| text[i..].starts_with("no_warps = true"))
                };
                let warped = !set(&lua[..at], state.no_warps);
                state.no_warps = set(lua, state.no_warps);
                let rest = &lua[at + "hl.dsp.focus({ window = \"address:".len()..];
                let address = rest[..rest.find('"').unwrap()].to_string();
                if !state.windows.iter().any(|w| w.address == address) {
                    return Ok("error: hl.focus: window not found".into());
                }
                state.focused.push((address, warped));
                return Ok("ok".into());
            }
            if let Some(lua) = command.strip_prefix("eval hl.dispatch(hl.dsp.window.move(") {
                let (Some(monitor), Some(address)) =
                    (quoted(lua, "monitor"), quoted(lua, "window"))
                else {
                    return Ok("error: hl.window.move: expected a table".into());
                };
                let address = address.trim_start_matches("address:");
                let shown = state.shown(&monitor);
                let Some(window) = state.windows.iter_mut().find(|w| w.address == address) else {
                    return Ok("error: hl.window.move: window not found".into());
                };
                window.monitor = monitor;
                window.workspace = Some(shown);
                return Ok("ok".into());
            }
            // Bare, or behind `if hl.get_workspace(...) then`, which skips a workspace not there.
            if let Some(lua) = command.strip_prefix("eval ")
                && let Some(at) = lua.find("hl.dispatch(hl.dsp.workspace.move(")
            {
                let guarded = lua.starts_with("if hl.get_workspace(");
                let lua = &lua[at..];
                let (Some(name), Some(to)) = (quoted(lua, "workspace"), quoted(lua, "monitor"))
                else {
                    return Ok("error: hl.workspace.move: expected a table".into());
                };
                let name = name.trim_start_matches("name:");
                let all = state.all_workspaces();
                match all.into_iter().find(|(_, n, _)| n == name) {
                    Some((id, name, from)) => state.move_workspace(id, name, from, &to),
                    None if !guarded => {
                        return Ok("warning: =[C]:-1: Workspace not found".into());
                    }
                    None => {}
                }
                // The rest of the chunk, if any, runs next.
                if !lua.contains("local m = hl.get_monitor(") {
                    return Ok("ok".into());
                }
            }
            // `local m = hl.get_monitor("X") if m then m:set_workspace({ workspace = "N" }) end`,
            // maybe after a move: the monitor shows workspace N, made when missing, without the
            // focus. One on another monitor is not moved: shown there all the same, and still
            // listed on its own (measured on Hyprland 0.56.2).
            if let Some(lua) = command.strip_prefix("eval ")
                && let Some(at) = lua.find("local m = hl.get_monitor(")
            {
                let lua = &lua[at..];
                let (Some(monitor), Some(name)) = (
                    lua.split('"').nth(1).map(str::to_string),
                    quoted(lua, "workspace"),
                ) else {
                    return Ok("error: set_workspace: expected a table".into());
                };
                if !state.monitors.iter().any(|m| m.name == monitor) {
                    return Ok("ok".into());
                }
                let all = state.all_workspaces();
                let id = match all.into_iter().find(|(_, n, _)| *n == name) {
                    Some((id, _, _)) => id,
                    None => {
                        let id = name.parse().unwrap_or(-2000);
                        state.workspaces.push((id, name, monitor.clone()));
                        id
                    }
                };
                state.shown.retain(|(m, _)| *m != monitor);
                state.shown.push((monitor, id));
                return Ok("ok".into());
            }
            if let Some(lua) = command.strip_prefix("eval hl.config({ cursor = { invisible = ") {
                state.cursor_invisible = lua.starts_with("true");
                return Ok("ok".into());
            }
            if let Some(name) = command
                .strip_prefix("eval hl.monitor({ output = \"")
                .and_then(|rest| rest.strip_suffix("\", disabled = true })"))
            {
                return Ok(match state.monitors.iter_mut().find(|m| m.name == name) {
                    Some(m) => {
                        m.disabled = true;
                        "ok".into()
                    }
                    None => "error: hl.monitor: no such output".into(),
                });
            }
            if let Some(lua) = command.strip_prefix("eval ") {
                return Ok(match parse_rule(lua) {
                    Some(rule) => {
                        Self::apply_rule(&mut state, rule, lua.contains("disabled = false"));
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
    fn what_can_put_a_window_on_a_monitor_is_parsed() {
        for line in [
            "openwindow>>5a1,7,foot,a title",
            "movewindowv2>>5a1,2,2",
            "workspacev2>>4,4",
        ] {
            assert_eq!(Event::parse(line), Event::Placed, "{line}");
        }
        // Their first versions come with the same change: one sweep is enough.
        assert_eq!(
            Event::parse("moveworkspacev2>>-1337,mail, work,DP-2"),
            Event::Placed
        );
        assert_eq!(Event::parse("movewindow>>5a1,2"), Event::Other);
        assert_eq!(Event::parse("moveworkspace>>2,DP-2"), Event::Other);
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

    use fake::FakeWindow;

    fn home_with_windows(windows: &[FakeWindow]) -> FakeHypr {
        let hypr = FakeHypr::with(vec![
            FakeMonitor::new("DP-1", 1920, 1080, 0),
            FakeMonitor::new("DP-2", 2560, 1440, 1920),
            FakeMonitor::new("DP-3", 1920, 1080, 4480),
        ]);
        for window in windows {
            hypr.add_window(window.clone());
        }
        hypr
    }

    #[test]
    fn the_keyboard_goes_to_the_home_window_that_had_it_last_and_the_pointer_stays() {
        let hypr = home_with_windows(&[
            FakeWindow::new("0xa1", "DP-1", 1),
            FakeWindow::new("0xb2", "DP-3", 0),
            FakeWindow::new("0xc3", "DP-1", 2),
        ]);
        let focused = focus_last_window(&hypr, &["DP-2".to_string()]).unwrap();
        assert_eq!(focused.as_deref(), Some("0xb2"));
        assert_eq!(hypr.focused(), [("0xb2".to_string(), false)]);
        // Put back as it was.
        assert!(!hypr.no_warps());
    }

    #[test]
    fn a_window_under_the_covered_monitor_or_on_a_hidden_workspace_is_passed_over() {
        let hidden = FakeWindow {
            hidden_workspace: true,
            ..FakeWindow::new("0xd4", "DP-1", 1)
        };
        let hypr = home_with_windows(&[
            FakeWindow::new("0xe5", "DP-2", 0),
            hidden,
            FakeWindow::new("0xf6", "DP-3", 2),
        ]);
        assert_eq!(
            focus_last_window(&hypr, &["DP-2".to_string()])
                .unwrap()
                .as_deref(),
            Some("0xf6")
        );
    }

    #[test]
    fn a_window_under_any_of_the_covered_monitors_is_passed_over() {
        let hypr = home_with_windows(&[
            FakeWindow::new("0xa1", "DP-1", 2),
            FakeWindow::new("0xe5", "DP-2", 0),
            FakeWindow::new("0xf6", "DP-3", 1),
        ]);
        let covered = ["DP-2".to_string(), "DP-3".to_string()];
        assert_eq!(
            focus_last_window(&hypr, &covered).unwrap().as_deref(),
            Some("0xa1")
        );
    }

    #[test]
    fn with_no_home_window_shown_the_keyboard_is_left_as_it_is() {
        let hypr = home_with_windows(&[FakeWindow::new("0xe5", "DP-2", 0)]);
        assert_eq!(
            focus_last_window(&hypr, &["DP-2".to_string()]).unwrap(),
            None
        );
        assert_eq!(hypr.changes(), [] as [&str; 0]);
    }

    #[test]
    fn no_warps_set_by_the_user_stays_set() {
        let hypr = home_with_windows(&[FakeWindow::new("0xa1", "DP-1", 0)]);
        hypr.set_no_warps(true);
        focus_last_window(&hypr, &["DP-2".to_string()]).unwrap();
        assert_eq!(hypr.focused(), [("0xa1".to_string(), false)]);
        assert!(hypr.no_warps());
    }

    fn dp2() -> Vec<String> {
        vec!["DP-2".to_string()]
    }

    #[test]
    fn a_window_on_the_workspace_a_covered_monitor_shows_goes_home_without_the_focus() {
        let hypr = home_with_windows(&[
            FakeWindow::new("0xa1", "DP-1", 0),
            FakeWindow::new("0xe5", "DP-2", 1),
        ]);
        let sent = send_home(&hypr, &dp2(), "DP-1", &mut Vec::new()).unwrap();
        assert_eq!(sent, ["window 0xe5"]);
        // On the workspace DP-1 shows, which it still shows: the window is in sight.
        assert_eq!(hypr.place("0xe5"), ("DP-1".to_string(), 1));
        assert_eq!(hypr.shown("DP-1"), 1);
        assert_eq!(hypr.place("0xa1"), ("DP-1".to_string(), 1));
        let changes = hypr.changes();
        assert!(changes[0].contains("follow = false"), "{changes:?}");
    }

    #[test]
    fn the_window_with_the_keyboard_brings_it_home_with_the_pointer() {
        // Moved with the focus following it, or dragged: Hyprland put the pointer on DP-2.
        let hypr = home_with_windows(&[
            FakeWindow::new("0xa1", "DP-1", 1),
            FakeWindow::new("0xe5", "DP-2", 0),
        ]);
        send_home(&hypr, &dp2(), "DP-1", &mut Vec::new()).unwrap();
        assert_eq!(hypr.place("0xe5").0, "DP-1");
        assert_eq!(hypr.focused(), [("0xe5".to_string(), true)]);
    }

    #[test]
    fn a_window_without_the_keyboard_goes_home_without_it() {
        let hypr = home_with_windows(&[
            FakeWindow::new("0xa1", "DP-1", 0),
            FakeWindow::new("0xe5", "DP-2", 1),
        ]);
        send_home(&hypr, &dp2(), "DP-1", &mut Vec::new()).unwrap();
        assert_eq!(hypr.place("0xe5").0, "DP-1");
        assert_eq!(hypr.focused(), []);
    }

    #[test]
    fn a_workspace_moved_whole_onto_a_covered_monitor_goes_home_whole() {
        let hypr = home_with_windows(&[]);
        let mut seen = Vec::new();
        send_home(&hypr, &dp2(), "DP-1", &mut seen).unwrap();
        assert_eq!(seen, [("DP-2".to_string(), 2)]);
        hypr.add_workspace(-1337, "mail", "DP-2");
        hypr.show("DP-2", -1337);
        hypr.add_window(FakeWindow::new("0xa1", "DP-2", 0).on_workspace(-1337));
        hypr.add_window(FakeWindow::new("0xb2", "DP-2", 1).on_workspace(-1337));
        let sent = send_home(&hypr, &dp2(), "DP-1", &mut seen).unwrap();
        assert_eq!(sent, ["workspace mail"]);
        assert_eq!(hypr.place("0xa1"), ("DP-1".to_string(), -1337));
        assert_eq!(hypr.place("0xb2"), ("DP-1".to_string(), -1337));
        assert_eq!(hypr.shown("DP-1"), -1337);
        assert_ne!(hypr.shown("DP-2"), -1337);
    }

    #[test]
    fn a_window_opened_on_a_hidden_workspace_of_a_covered_monitor_goes_home_with_it() {
        // `[workspace 7 silent]`, workspace 7 living on DP-2 but not shown there.
        let hypr = home_with_windows(&[FakeWindow::new("0xa1", "DP-1", 1)]);
        hypr.add_workspace(7, "7", "DP-2");
        hypr.add_window(FakeWindow::new("0xc3", "DP-2", 0).on_workspace(7));
        let sent = send_home(&hypr, &dp2(), "DP-1", &mut Vec::new()).unwrap();
        assert_eq!(sent, ["workspace 7"]);
        assert_eq!(hypr.place("0xc3"), ("DP-1".to_string(), 7));
        // Silent it was, silent it stays: DP-1 still shows what it showed.
        assert_eq!(hypr.shown("DP-1"), 1);
    }

    #[test]
    fn home_windows_empty_and_special_workspaces_stay_where_they_are() {
        let hypr = home_with_windows(&[
            FakeWindow::new("0xa1", "DP-1", 1),
            FakeWindow::new("0xf6", "DP-3", 0),
        ]);
        hypr.add_workspace(-98, "special:scratch", "DP-2");
        hypr.add_window(FakeWindow::new("0xd4", "DP-2", 2).on_workspace(-98));
        hypr.add_workspace(8, "8", "DP-2");
        let sent = send_home(&hypr, &dp2(), "DP-1", &mut Vec::new()).unwrap();
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!(hypr.changes(), [] as [&str; 0]);
        assert_eq!(hypr.place("0xd4"), ("DP-2".to_string(), -98));
    }

    #[test]
    fn windows_under_any_of_the_covered_monitors_go_home() {
        let hypr = home_with_windows(&[
            FakeWindow::new("0xe5", "DP-2", 0),
            FakeWindow::new("0xf6", "DP-3", 1),
        ]);
        let covered = ["DP-2".to_string(), "DP-3".to_string()];
        let mut sent = send_home(&hypr, &covered, "DP-1", &mut Vec::new()).unwrap();
        sent.sort();
        assert_eq!(sent, ["window 0xe5", "window 0xf6"]);
        assert_eq!(hypr.place("0xe5").0, "DP-1");
        assert_eq!(hypr.place("0xf6").0, "DP-1");
    }

    #[test]
    fn a_reply_other_than_ok_is_an_error() {
        let hypr = FakeHypr::default();
        let error = dispatch(&hypr, "output remove NOPE").unwrap_err();
        assert!(error.to_string().contains("output not found"));
    }
}
