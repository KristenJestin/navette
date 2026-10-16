//! Where Navette stands, as one line of JSON per change, for a bar or a shell: `serve` on the
//! desk and `agent` on the remote machine each keep their state here, computed in one place
//! from its parts, and serve it on `$XDG_RUNTIME_DIR/navette/events.sock` (`docs/events.md`,
//! the contract other programs read).

use crate::mic;

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
        let modes: Vec<String> = self
            .modes
            .iter()
            .map(|m| {
                format!(
                    r#"{{"name":{},"monitors":{}}}"#,
                    json!(m.name),
                    json!(m.monitors)
                )
            })
            .collect();
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
            ("modes", format!("[{}]", modes.join(","))),
            ("mic", json!(self.mic.map(mic::Status::word)).to_string()),
        ];
        let fields: Vec<String> = fields
            .iter()
            .map(|(key, value)| format!("\"{key}\":{value}"))
            .collect();
        format!("{{{}}}", fields.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
