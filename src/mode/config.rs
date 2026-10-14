//! `mode.conf`: the modes the maintainer defines at home, each naming the home monitors the
//! remote machine takes. Keys before any `[section]` are the default of every mode, and a
//! `monitor` among them is the one unnamed mode, so a file written before modes had names keeps
//! working. A `[name]` section is a named mode: its `monitor`, and any of `release`,
//! `mic_hotkey`, `remote_label`, `home_label`, `remote_bar`, `home_bar` it wants other than the
//! default.
//! `#` starts a comment.

use std::io;
use std::path::{Path, PathBuf};

use crate::corner::{self, Look};
use crate::display::wire::MAX_OUTPUTS;
use crate::hypr::Monitor;
use crate::input::{self, Hotkey};

pub const CONFIG_FILE: &str = "mode.conf";
/// The keys that switch the mic, unless the config names others: free in Omarchy's own
/// bindings.
const MIC_HOTKEY: &str = "SUPER + CTRL + M";
/// A mode's name travels on one short line to `navette serve`.
pub const NAME_LONGEST: usize = 32;
/// A monitor named by its description rather than its output name.
const BY_DESCRIPTION: &str = "desc:";

/// One mode, as the config says it.
#[derive(Debug)]
pub struct Mode {
    /// `None` for the mode of a file with no sections.
    pub name: Option<String>,
    /// Each one an output name, or `desc:` and the start of a monitor's description, as written.
    pub monitors: Vec<String>,
    pub release: Option<Hotkey>,
    /// What the file says that is no longer read, on one line.
    pub notice: Option<String>,
    pub mic_hotkey: String,
    pub look: Look,
}

/// The settings of one section; the first one, before any `[name]`, has no name.
struct Section {
    name: Option<String>,
    settings: Vec<(String, String)>,
}

impl Section {
    fn get(&self, key: &str) -> Option<&str> {
        self.settings
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

struct File {
    path: PathBuf,
    sections: Vec<Section>,
}

fn name_chars(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Whether `name` can name a mode.
pub fn is_name(name: &str) -> bool {
    name_chars(name) && name.len() <= NAME_LONGEST
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl File {
    fn read(config_dir: &Path) -> io::Result<File> {
        let path = config_dir.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no mode configured: write `monitor = <output name>` in {}",
                        path.display()
                    ),
                ));
            }
            Err(e) => return Err(e),
        };
        let mut sections = vec![Section {
            name: None,
            settings: Vec::new(),
        }];
        for line in text.lines() {
            let line = line.split_once('#').map_or(line, |(kept, _)| kept).trim();
            if let Some(header) = line.strip_prefix('[') {
                let name = header
                    .strip_suffix(']')
                    .ok_or_else(|| invalid(format!("{}: `{line}` has no `]`", path.display())))?
                    .trim();
                if !name_chars(name) {
                    return Err(invalid(format!(
                        "{}: `[{name}]` is not a mode name: letters, digits, `-`, `_` and `.`",
                        path.display()
                    )));
                }
                if name.len() > NAME_LONGEST {
                    return Err(invalid(format!(
                        "{}: the mode name `{name}` is too long, {NAME_LONGEST} characters at most",
                        path.display()
                    )));
                }
                if sections.iter().any(|s| s.name.as_deref() == Some(name)) {
                    return Err(invalid(format!(
                        "{} names the mode `{name}` twice",
                        path.display()
                    )));
                }
                sections.push(Section {
                    name: Some(name.to_string()),
                    settings: Vec::new(),
                });
            } else if let Some((key, value)) = line.split_once('=') {
                let last = sections.last_mut().expect("the default section is first");
                last.settings
                    .push((key.trim().to_string(), value.trim().to_string()));
            }
        }
        Ok(File { path, sections })
    }

    /// The names of the named modes.
    fn names(&self) -> Vec<&str> {
        self.sections
            .iter()
            .filter_map(|s| s.name.as_deref())
            .collect()
    }

    /// The mode in section `index`: its own settings first, then the defaults.
    fn mode(&self, index: usize) -> io::Result<Mode> {
        let section = &self.sections[index];
        let path = self.path.display();
        let which = section
            .name
            .as_ref()
            .map_or(String::new(), |name| format!(": mode `{name}`"));
        let value = |key: &str| section.get(key).or_else(|| self.sections[0].get(key));
        let monitors: Vec<String> = section
            .get("monitor")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
        if monitors.is_empty() {
            return Err(invalid(format!(
                "{path}{which} names no monitor: `monitor = <output name>`, or several: \
                 `monitor = DP-1, DP-2`"
            )));
        }
        if let Some(twice) = monitors
            .iter()
            .enumerate()
            .find_map(|(i, name)| monitors[..i].contains(name).then_some(name))
        {
            return Err(invalid(format!("{path} names {twice} twice")));
        }
        if let Some(bare) = monitors.iter().find(|m| *m == BY_DESCRIPTION) {
            return Err(invalid(format!(
                "{path}: `{bare}` needs the start of a description after it"
            )));
        }
        if monitors.len() > usize::from(MAX_OUTPUTS) {
            return Err(invalid(format!(
                "{path} names {} monitors, {MAX_OUTPUTS} at most",
                monitors.len()
            )));
        }
        let spec = value("release").unwrap_or(input::DEFAULT_HOTKEY);
        let release =
            Hotkey::parse(spec).map_err(|e| invalid(format!("{path}: `release = {spec}`: {e}")))?;
        // The lid works during every mode now; a file written for the option still starts.
        let notice = value("closed_lid").map(|_| {
            format!(
                "{path}: `closed_lid` is no longer read, the remote laptop can be closed and \
                 opened during every mode: the line can go"
            )
        });
        let mic_hotkey = value("mic_hotkey")
            .filter(|keys| !keys.is_empty())
            .unwrap_or(MIC_HOTKEY)
            .to_string();
        let fallback = Look::default();
        let label = |key: &str, default: String| match value(key).filter(|v| !v.is_empty()) {
            None => Ok(default),
            Some(label) => corner::check_label(label)
                .map(|()| label.to_string())
                .map_err(|e| {
                    invalid(format!(
                        "{path}: `{key} = {label}`: {e}; nothing was changed"
                    ))
                }),
        };
        let remote_bar = match value("remote_bar") {
            None => fallback.remote_bar,
            Some(bar) => bar.parse::<u16>().map(i32::from).map_err(|_| {
                invalid(format!(
                    "{path}: `remote_bar = {bar}`: a height in pixels, 0 or more; \
                     nothing was changed"
                ))
            })?,
        };
        let home_bar = match value("home_bar") {
            None => fallback.home_bar,
            Some("auto") => None,
            Some(bar) => Some(bar.parse::<u16>().map(i32::from).map_err(|_| {
                invalid(format!(
                    "{path}: `home_bar = {bar}`: `auto`, or a height in pixels, 0 or more; \
                     nothing was changed"
                ))
            })?),
        };
        let look = Look {
            remote_label: label("remote_label", fallback.remote_label)?,
            home_label: label("home_label", fallback.home_label)?,
            remote_bar,
            home_bar,
        };
        Ok(Mode {
            name: section.name.clone(),
            monitors,
            release,
            notice,
            mic_hotkey,
            look,
        })
    }
}

impl Mode {
    /// The mode called `name`; with no name, the unnamed mode, or the only named one.
    pub fn read(config_dir: &Path, name: Option<&str>) -> io::Result<Mode> {
        let file = File::read(config_dir)?;
        let named = file.names();
        let index = match name {
            Some(name) => file
                .sections
                .iter()
                .position(|s| s.name.as_deref() == Some(name))
                .ok_or_else(|| {
                    let there = if named.is_empty() {
                        "the file defines no named mode".to_string()
                    } else {
                        format!("there are: {}", named.join(", "))
                    };
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no mode `{name}` in {} ({there})", file.path.display()),
                    )
                })?,
            None if file.sections[0].get("monitor").is_some() || named.is_empty() => 0,
            None if named.len() == 1 => 1,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} defines several modes ({}): `navette mode start <name>`",
                        file.path.display(),
                        named.join(", ")
                    ),
                ));
            }
        };
        file.mode(index)
    }
}

/// Every mode, one line each: its name and its monitors, or what is wrong with it.
pub fn list(config_dir: &Path) -> io::Result<String> {
    let file = File::read(config_dir)?;
    let mut lines = Vec::new();
    for (index, section) in file.sections.iter().enumerate() {
        let name = match &section.name {
            Some(name) => name.as_str(),
            // A file with no default mode lists the named ones only.
            None if section.get("monitor").is_none() => continue,
            None => "(default)",
        };
        let what = match file.mode(index) {
            Ok(mode) => mode.monitors.join(", "),
            Err(e) => e.to_string(),
        };
        lines.push(format!("{name:<10} {what}"));
    }
    if lines.is_empty() {
        // No mode at all: the error says what to write.
        file.mode(0)?;
    }
    Ok(lines.join("\n"))
}

/// The home monitors the specs name, in the order given: by output name, or by the start of the
/// description (Hyprland's `desc:`), so a change of connector order breaks nothing. A monitor
/// that is not there, a description that fits two, or one monitor named twice, changes nothing.
pub fn resolve(specs: &[String], monitors: &[Monitor]) -> io::Result<Vec<Monitor>> {
    let there = || {
        monitors
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut found: Vec<Monitor> = Vec::new();
    for spec in specs {
        let fits: Vec<&Monitor> = match spec.strip_prefix(BY_DESCRIPTION) {
            Some(start) => monitors
                .iter()
                .filter(|m| !m.disabled && m.description.starts_with(start.trim()))
                .collect(),
            None => monitors
                .iter()
                .filter(|m| m.name == *spec && !m.disabled)
                .collect(),
        };
        let monitor = match fits[..] {
            [] => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no monitor {spec} at home (there are: {}); nothing was changed",
                        there()
                    ),
                ));
            }
            [one] => one,
            _ => {
                let names: Vec<_> = fits.iter().map(|m| m.name.as_str()).collect();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{spec} fits {}: name more of the description; nothing was changed",
                        names.join(" and ")
                    ),
                ));
            }
        };
        if found.iter().any(|m| m.name == monitor.name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} twice; nothing was changed", monitor.name),
            ));
        }
        found.push(monitor.clone());
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, description: &str) -> Monitor {
        serde_json::from_value(serde_json::json!({
            "id": 0, "name": name, "description": description, "width": 1920, "height": 1080,
        }))
        .unwrap()
    }

    fn written(text: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), text).unwrap();
        dir
    }

    fn mode(text: &str, name: Option<&str>) -> io::Result<Mode> {
        Mode::read(written(text).path(), name)
    }

    #[test]
    fn a_file_without_sections_is_one_unnamed_mode() {
        let mode = mode("# my desk\nmonitor = DP-2, DP-3   # right\n", None).unwrap();
        assert_eq!(mode.name, None);
        assert_eq!(mode.monitors, ["DP-2", "DP-3"]);
        assert_eq!(mode.notice, None);
        assert_eq!(mode.mic_hotkey, "SUPER + CTRL + M");
        // There is no other mode to name.
        let error = self::mode("monitor = DP-2\n", Some("office")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("no mode `office`"), "{error}");
    }

    #[test]
    fn sections_are_named_modes_each_with_its_own_monitors() {
        let text = "[office]\nmonitor = DP-1, DP-2\n\n[travel]\nmonitor = desc:Dell U27\n";
        let office = mode(text, Some("office")).unwrap();
        assert_eq!(office.name.as_deref(), Some("office"));
        assert_eq!(office.monitors, ["DP-1", "DP-2"]);
        let travel = mode(text, Some("travel")).unwrap();
        assert_eq!(travel.monitors, ["desc:Dell U27"]);
    }

    #[test]
    fn the_keys_before_any_section_are_the_default_of_every_mode() {
        let text = "release = ctrl+alt+q\nmic_hotkey = SUPER + F9\n\
                    [office]\nmonitor = DP-1\n\
                    [travel]\nmonitor = DP-2\nrelease = none\n\
                    mic_hotkey = SUPER + F10\n";
        let office = mode(text, Some("office")).unwrap();
        assert_eq!(office.release, Hotkey::parse("ctrl+alt+q").unwrap());
        assert_eq!(office.mic_hotkey, "SUPER + F9");
        let travel = mode(text, Some("travel")).unwrap();
        assert_eq!(travel.release, None);
        assert_eq!(travel.mic_hotkey, "SUPER + F10");
    }

    #[test]
    fn the_corner_tags_are_neutral_by_default() {
        let look = mode("monitor = DP-2\n", None).unwrap().look;
        assert_eq!(look, Look::default());
        assert_eq!(look.remote_label, "REMOTE");
        assert_eq!(look.home_label, "HOME");
    }

    #[test]
    fn with_no_key_both_corners_sit_at_the_top() {
        let look = mode("monitor = DP-2\n", None).unwrap().look;
        assert_eq!(look.remote_bar, 0);
        assert_eq!(look.home_bar, Some(0));
    }

    #[test]
    fn a_mode_names_its_own_corner_labels() {
        let text = "remote_label = FAR\nhome_label = NEAR\nremote_bar = 30\n\
                    [office]\nmonitor = DP-1\n\
                    [travel]\nmonitor = DP-2\nremote_label = AWAY\nremote_bar = 0\n";
        let office = mode(text, Some("office")).unwrap().look;
        assert_eq!(
            (office.remote_label.as_str(), office.home_label.as_str()),
            ("FAR", "NEAR")
        );
        assert_eq!(office.remote_bar, 30);
        let travel = mode(text, Some("travel")).unwrap().look;
        assert_eq!(
            (travel.remote_label.as_str(), travel.home_label.as_str()),
            ("AWAY", "NEAR")
        );
        assert_eq!(travel.remote_bar, 0);
        // The longest label there is.
        assert!(mode("monitor = DP-2\nhome_label = ABCDEFGHIJKL\n", None).is_ok());
    }

    #[test]
    fn a_label_the_corner_cannot_draw_fails_clearly_and_changes_nothing() {
        for (line, wants) in [
            ("remote_label = Café", "remote_label"),
            ("home_label = tab\there", "home_label"),
            ("home_label = ABCDEFGHIJKLM", "12 characters"),
            ("remote_label = 日本", "printable ASCII"),
        ] {
            let error = mode(&format!("monitor = DP-2\n{line}\n"), None).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{line}");
            let error = error.to_string();
            assert!(error.contains(wants), "{line}: {error}");
            assert!(error.contains("nothing was changed"), "{line}: {error}");
        }
    }

    #[test]
    fn the_remote_corner_starts_below_the_bar_height_the_mode_sets() {
        assert_eq!(
            mode("monitor = DP-2\nremote_bar = 48\n", None)
                .unwrap()
                .look
                .remote_bar,
            48
        );
        for bad in ["-1", "tall", "2.5"] {
            let error = mode(&format!("monitor = DP-2\nremote_bar = {bad}\n"), None)
                .unwrap_err()
                .to_string();
            assert!(error.contains("remote_bar"), "{bad}: {error}");
            assert!(error.contains("nothing was changed"), "{bad}: {error}");
        }
    }

    #[test]
    fn remote_bar_26_home_bar_auto_and_home_bar_12_still_place_the_corners_as_before() {
        let text = "home_bar = auto\nremote_bar = 26\n[office]\nmonitor = DP-1\n\
                    [travel]\nmonitor = DP-2\nhome_bar = 0\n\
                    [desk]\nmonitor = DP-3\nhome_bar = 12\n";
        let office = mode(text, Some("office")).unwrap().look;
        assert_eq!((office.remote_bar, office.home_bar), (26, None));
        assert_eq!(mode(text, Some("travel")).unwrap().look.home_bar, Some(0));
        assert_eq!(mode(text, Some("desk")).unwrap().look.home_bar, Some(12));
    }

    #[test]
    fn a_home_bar_that_is_neither_auto_nor_pixels_fails_clearly() {
        for bad in ["-1", "tall", "2.5", "Auto"] {
            let error = mode(&format!("monitor = DP-2\nhome_bar = {bad}\n"), None)
                .unwrap_err()
                .to_string();
            assert!(error.contains("home_bar"), "{bad}: {error}");
            assert!(error.contains("auto"), "{bad}: {error}");
            assert!(error.contains("nothing was changed"), "{bad}: {error}");
        }
    }

    #[test]
    fn the_unnamed_mode_stays_next_to_the_named_ones() {
        let text = "monitor = DP-1\n[travel]\nmonitor = DP-2\n";
        assert_eq!(mode(text, None).unwrap().monitors, ["DP-1"]);
        assert_eq!(mode(text, Some("travel")).unwrap().monitors, ["DP-2"]);
        // A default `monitor` is the unnamed mode's, not one a named mode inherits.
        let error = mode("monitor = DP-1\n[travel]\nrelease = none\n", Some("travel"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("`travel` names no monitor"), "{error}");
    }

    #[test]
    fn with_no_name_the_only_named_mode_starts() {
        let text = "[office]\nmonitor = DP-1\n";
        assert_eq!(mode(text, None).unwrap().name.as_deref(), Some("office"));
    }

    #[test]
    fn with_no_name_and_several_named_modes_the_names_are_listed() {
        let text = "[office]\nmonitor = DP-1\n[travel]\nmonitor = DP-2\n";
        let error = mode(text, None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let error = error.to_string();
        assert!(error.contains("office, travel"), "{error}");
        assert!(error.contains("navette mode start <name>"), "{error}");
    }

    #[test]
    fn an_unknown_mode_name_lists_the_known_ones() {
        let text = "[office]\nmonitor = DP-1\n[travel]\nmonitor = DP-2\n";
        let error = mode(text, Some("holiday")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let error = error.to_string();
        assert!(error.contains("no mode `holiday`"), "{error}");
        assert!(error.contains("office, travel"), "{error}");
    }

    #[test]
    fn a_missing_file_says_where_to_write_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let error = Mode::read(dir.path(), None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("monitor = <output name>"));
    }

    #[test]
    fn a_file_with_no_monitor_at_all_fails_clearly() {
        let error = mode("release = none\n", None).unwrap_err().to_string();
        assert!(error.contains("names no monitor"), "{error}");
    }

    #[test]
    fn bad_sections_fail_clearly() {
        for (text, wants) in [
            ("[office\nmonitor = DP-1\n", "office"),
            ("[]\nmonitor = DP-1\n", "[]"),
            ("[two words]\nmonitor = DP-1\n", "two words"),
            ("[a]\nmonitor = DP-1\n[a]\nmonitor = DP-2\n", "`a` twice"),
            (
                &format!("[{}]\nmonitor = DP-1\n", "n".repeat(33)),
                "32 characters",
            ),
        ] {
            let error = mode(text, None).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{text}");
            assert!(error.to_string().contains(wants), "{text}: {error}");
        }
    }

    #[test]
    fn monitors_are_read_each_once_up_to_the_most_a_mode_has() {
        let twice = mode("monitor = DP-3, DP-2, DP-3\n", None).unwrap_err();
        assert!(twice.to_string().contains("DP-3 twice"), "{twice}");
        assert!(mode("monitor = ,\n", None).is_err());
        let trailing = mode("monitor = DP-3 , DP-2,   # the two right ones\n", None).unwrap();
        assert_eq!(trailing.monitors, ["DP-3", "DP-2"]);
        let nine = (1..=9).map(|i| format!("DP-{i}")).collect::<Vec<_>>();
        let error = mode(&format!("monitor = {}\n", nine.join(", ")), None).unwrap_err();
        assert!(
            error.to_string().contains("9 monitors, 8 at most"),
            "{error}"
        );
        let error = mode("monitor = desc:\n", None).unwrap_err();
        assert!(error.to_string().contains("desc:"), "{error}");
    }

    #[test]
    fn a_file_that_still_names_closed_lid_is_read_with_a_one_line_notice() {
        for value in ["on", "off", "yes please"] {
            let text = format!("monitor = DP-2\nclosed_lid = {value}\n");
            let mode = mode(&text, None).unwrap();
            assert_eq!(mode.monitors, ["DP-2"]);
            let notice = mode.notice.expect("a notice");
            assert!(notice.contains("closed_lid"), "{notice}");
            assert!(!notice.contains('\n'), "{notice}");
        }
        // A named mode gets it from the defaults as well as from its own section.
        let text = "closed_lid = on\n[office]\nmonitor = DP-1\n[travel]\nmonitor = DP-2\n";
        assert!(mode(text, Some("office")).unwrap().notice.is_some());
        let text = "[office]\nmonitor = DP-1\nclosed_lid = on\n[travel]\nmonitor = DP-2\n";
        assert!(mode(text, Some("office")).unwrap().notice.is_some());
        assert!(mode(text, Some("travel")).unwrap().notice.is_none());
    }

    #[test]
    fn a_release_hotkey_with_no_known_key_is_refused() {
        let error = mode("monitor = DP-2\nrelease = shift+nokey\n", None).unwrap_err();
        assert!(error.to_string().contains("nokey"), "{error}");
    }

    #[test]
    fn an_empty_release_hotkey_means_none() {
        assert_eq!(
            mode("monitor = DP-2\nrelease =\n", None).unwrap().release,
            None
        );
    }

    #[test]
    fn the_list_names_each_mode_and_its_monitors() {
        let dir =
            written("monitor = DP-1\n[office]\nmonitor = DP-2, DP-3\n[broken]\nrelease = none\n");
        let list = list(dir.path()).unwrap();
        let lines: Vec<_> = list.lines().collect();
        assert_eq!(lines[0], "(default)  DP-1");
        assert_eq!(lines[1], "office     DP-2, DP-3");
        assert!(lines[2].starts_with("broken     "), "{list}");
        assert!(lines[2].contains("names no monitor"), "{list}");
    }

    #[test]
    fn the_list_of_a_file_with_no_mode_says_what_to_write() {
        let dir = written("release = none\n");
        let error = list(dir.path()).unwrap_err().to_string();
        assert!(error.contains("names no monitor"), "{error}");
    }

    #[test]
    fn a_monitor_is_found_by_output_name_or_by_the_start_of_its_description() {
        let monitors = [
            monitor("DP-1", "Dell Inc. DELL U2723QE 7XYZ123"),
            monitor("DP-2", "LG Electronics LG ULTRAWIDE 0123"),
        ];
        let found = |specs: &[&str]| {
            let specs: Vec<String> = specs.iter().map(|s| s.to_string()).collect();
            resolve(&specs, &monitors).map(|m| m.into_iter().map(|m| m.name).collect::<Vec<_>>())
        };
        assert_eq!(found(&["DP-2"]).unwrap(), ["DP-2"]);
        assert_eq!(found(&["desc:Dell Inc. DELL U2723QE"]).unwrap(), ["DP-1"]);
        assert_eq!(found(&["desc:LG", "DP-1"]).unwrap(), ["DP-2", "DP-1"]);
    }

    #[test]
    fn a_description_that_matches_no_monitor_or_two_changes_nothing() {
        let monitors = [
            monitor("DP-1", "Dell Inc. DELL U2723QE 7XYZ123"),
            monitor("DP-2", "Dell Inc. DELL U2723QE 7XYZ456"),
        ];
        let specs = |s: &str| vec![s.to_string()];
        let none = resolve(&specs("desc:Iiyama"), &monitors).unwrap_err();
        assert_eq!(none.kind(), io::ErrorKind::NotFound);
        let none = none.to_string();
        assert!(
            none.contains("desc:Iiyama") && none.contains("DP-1, DP-2"),
            "{none}"
        );
        let two = resolve(&specs("desc:Dell Inc."), &monitors).unwrap_err();
        let two = two.to_string();
        assert!(two.contains("DP-1") && two.contains("DP-2"), "{two}");
        assert!(two.contains("more of the description"), "{two}");
        // Told apart by the serial.
        let one = resolve(&specs("desc:Dell Inc. DELL U2723QE 7XYZ456"), &monitors).unwrap();
        assert_eq!(one[0].name, "DP-2");
    }

    #[test]
    fn a_monitor_named_twice_by_name_and_description_is_refused() {
        let monitors = [monitor("DP-1", "Dell Inc. DELL U2723QE 7XYZ123")];
        let specs = ["DP-1".to_string(), "desc:Dell".to_string()];
        let error = resolve(&specs, &monitors).unwrap_err().to_string();
        assert!(error.contains("DP-1 twice"), "{error}");
    }

    #[test]
    fn a_disabled_monitor_is_not_there() {
        let mut off = monitor("DP-1", "Dell Inc. DELL U2723QE 7XYZ123");
        off.disabled = true;
        let error = resolve(&["DP-1".to_string()], &[off]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
