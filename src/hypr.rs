//! Hyprland, reached over its IPC socket behind a port, so the logic around it is tested with a
//! fake. The live side goes through gliff's `hypr-ipc` and finds the running instance again on
//! every request: after a Hyprland restart the next request reaches the new one.
//!
//! Thread #9 builds the full desk-change port; this one holds only what the display needs.

use std::io;
use std::time::Duration;

pub use hypr_ipc::Monitor;

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

/// The running Hyprland of this session.
#[derive(Clone, Debug, Default)]
pub struct Live;

impl Hypr for Live {
    fn request(&self, command: &str) -> io::Result<String> {
        let instance = hypr_ipc::Instance::discover(None).map_err(|e| {
            io::Error::new(io::ErrorKind::NotFound, format!("no running Hyprland: {e}"))
        })?;
        instance.request(command).map_err(|e| match e {
            hypr_ipc::Error::Io(e) => e,
            other => io::Error::other(other.to_string()),
        })
    }
}

/// What Hyprland tells on its event socket that the display cares about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// `hyprctl reload`: every runtime monitor rule is gone.
    ConfigReloaded,
}

/// Follows the event socket (`.socket2.sock`) of whichever Hyprland runs, on its own thread,
/// across restarts. Ends when `events` is dropped.
pub fn watch_events(events: tokio::sync::mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        while !events.is_closed() {
            let socket = hypr_ipc::Instance::discover(None)
                .map(|i| i.dir.join(".socket2.sock"))
                .and_then(|path| Ok(std::os::unix::net::UnixStream::connect(path)?));
            let Ok(socket) = socket else {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            };
            for line in io::BufReader::new(socket).lines().map_while(Result::ok) {
                if line.starts_with("configreloaded>>")
                    && events.send(Event::ConfigReloaded).is_err()
                {
                    return;
                }
            }
        }
    });
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
        down: bool,
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

        fn apply_rule(state: &mut State, rule: FakeMonitor) {
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
                            r#"{{"id":{id},"name":"{}","width":{},"height":{},"refreshRate":{},"x":{},"y":{},"scale":{},"transform":{},"focused":false,"disabled":false}}"#,
                            m.name, m.width, m.height, m.refresh, m.x, m.y, m.scale, m.transform
                        )
                    })
                    .collect();
                return Ok(format!("[{}]", list.join(",")));
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
    use super::fake::{FakeHypr, FakeMonitor};
    use super::*;

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
