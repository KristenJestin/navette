//! `mode.conf`: the modes the maintainer defines at home, each naming the home monitors the
//! remote machine takes. Keys before any `[section]` are the default of every mode, and a
//! `monitor` among them is the one unnamed mode, so a file written before modes had names keeps
//! working. A `[name]` section is a named mode: its `monitor`, and any of `release`, `mic`,
//! `mic_hotkey`, `remote_label`, `local_label`, `remote_bar`, `local_bar`, `local_above` it wants
//! other than the default. `here_notice` is about the link, so only the keys before any section
//! set it.
//! `#` starts a comment.

use std::io;
use std::path::{Path, PathBuf};

use crate::corner::{self, Look};
use crate::display::wire::MAX_OUTPUTS;
use crate::events::Defined;
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
/// The layer namespaces drawn above the remote's picture unless the config names others: the
/// selection overlays of slurp and hyprpicker, tools the user starts on purpose.
const LOCAL_ABOVE: &str = "selection, hyprpicker";

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
    /// `mic = follow`: the mic goes to the side that has the keyboard.
    pub mic_follows: bool,
    /// `here_notice = off` turns off the notification that the remote machine is here. It is
    /// about the link, not about a mode: only the keys before any section set it.
    pub here_notice: bool,
    /// The layer namespaces of home's own tools drawn above the remote's picture.
    pub local_above: Vec<String>,
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

    /// `new` in the mode of section `index`, or `old`, its former name, which still works: the
    /// section's own setting first, then the defaults. Each place that names `old` adds to
    /// `said`, and where both names are present `new` is the one used.
    fn renamed(&self, index: usize, new: &str, old: &str, said: &mut Vec<String>) -> Option<&str> {
        let mut found = None;
        let mut places = vec![index];
        if index != 0 {
            places.push(0);
        }
        for place in places {
            let section = &self.sections[place];
            match (section.get(new), section.get(old)) {
                (Some(_), Some(_)) => {
                    said.push(format!(
                        "`{old}` is ignored, `{new}` is used: the line can go"
                    ));
                }
                (None, Some(_)) => {
                    said.push(format!("`{old}` is now `{new}`: rename the line"));
                }
                _ => {}
            }
            found = found.or_else(|| section.get(new).or_else(|| section.get(old)));
        }
        found
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
        // What the file says that is no longer read, said once at `mode start`.
        let mut said: Vec<String> = Vec::new();
        // The lid works during every mode now; a file written for the option still starts.
        if value("closed_lid").is_some() {
            said.push(
                "`closed_lid` is no longer read, the remote laptop can be closed and opened \
                 during every mode: the line can go"
                    .into(),
            );
        }
        let local_label = self.renamed(index, "local_label", "home_label", &mut said);
        let local_bar = self.renamed(index, "local_bar", "home_bar", &mut said);
        let notice = (!said.is_empty()).then(|| format!("{path}: {}", said.join("; ")));
        let mic_hotkey = value("mic_hotkey")
            .filter(|keys| !keys.is_empty())
            .unwrap_or(MIC_HOTKEY)
            .to_string();
        let mic_follows = match value("mic").unwrap_or_default() {
            "" | "fixed" => false,
            "follow" => true,
            other => {
                return Err(invalid(format!(
                    "{path}: `mic = {other}`: `fixed` or `follow`; nothing was changed"
                )));
            }
        };
        let here_notice = match self.sections[0].get("here_notice").unwrap_or_default() {
            "" | "on" => true,
            "off" => false,
            other => {
                return Err(invalid(format!(
                    "{path}: `here_notice = {other}`: `on` or `off`; nothing was changed"
                )));
            }
        };
        let local_above = match value("local_above").unwrap_or(LOCAL_ABOVE) {
            "none" => Vec::new(),
            list => list
                .split(',')
                .map(str::trim)
                .filter(|namespace| !namespace.is_empty())
                .map(str::to_string)
                .collect(),
        };
        let fallback = Look::default();
        let label = |key: &str, found: Option<&str>, default: String| match found
            .filter(|v| !v.is_empty())
        {
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
        let home_bar = match local_bar {
            None => fallback.home_bar,
            Some("auto") => None,
            Some(bar) => Some(bar.parse::<u16>().map(i32::from).map_err(|_| {
                invalid(format!(
                    "{path}: `local_bar = {bar}`: `auto`, or a height in pixels, 0 or more; \
                     nothing was changed"
                ))
            })?),
        };
        let look = Look {
            remote_label: label("remote_label", value("remote_label"), fallback.remote_label)?,
            home_label: label("local_label", local_label, fallback.home_label)?,
            remote_bar,
            home_bar,
        };
        Ok(Mode {
            name: section.name.clone(),
            monitors,
            release,
            notice,
            mic_hotkey,
            mic_follows,
            here_notice,
            local_above,
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

/// The modes the file defines, as the events and `navette mode list --json` say them: a mode
/// the file gets wrong is left out, as it cannot start.
pub fn defined(config_dir: &Path) -> io::Result<Vec<Defined>> {
    let file = File::read(config_dir)?;
    Ok(file
        .sections
        .iter()
        .enumerate()
        // A file with no default mode has the named ones only.
        .filter(|(_, section)| section.name.is_some() || section.get("monitor").is_some())
        .filter_map(|(index, _)| file.mode(index).ok())
        .map(|mode| Defined {
            name: mode.name,
            monitors: mode.monitors,
        })
        .collect())
}

/// The remote machine's label, `remote_label` before any section, as the events say it.
pub fn remote_label(config_dir: &Path) -> String {
    File::read(config_dir)
        .ok()
        .and_then(|file| file.sections[0].get("remote_label").map(str::to_string))
        .unwrap_or_else(|| Look::default().remote_label)
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
        assert_eq!(look.home_label, "LOCAL");
    }

    #[test]
    fn with_no_key_both_corners_sit_at_the_top() {
        let look = mode("monitor = DP-2\n", None).unwrap().look;
        assert_eq!(look.remote_bar, 0);
        assert_eq!(look.home_bar, Some(0));
    }

    #[test]
    fn a_mode_names_its_own_corner_labels() {
        let text = "remote_label = FAR\nlocal_label = NEAR\nremote_bar = 30\n\
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
        assert!(mode("monitor = DP-2\nlocal_label = ABCDEFGHIJKL\n", None).is_ok());
    }

    #[test]
    fn a_label_the_corner_cannot_draw_fails_clearly_and_changes_nothing() {
        for (line, wants) in [
            ("remote_label = Café", "remote_label"),
            ("local_label = tab\there", "local_label"),
            ("local_label = ABCDEFGHIJKLM", "12 characters"),
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
    fn remote_bar_26_local_bar_auto_and_local_bar_12_still_place_the_corners_as_before() {
        let text = "local_bar = auto\nremote_bar = 26\n[office]\nmonitor = DP-1\n\
                    [travel]\nmonitor = DP-2\nlocal_bar = 0\n\
                    [desk]\nmonitor = DP-3\nlocal_bar = 12\n";
        let office = mode(text, Some("office")).unwrap().look;
        assert_eq!((office.remote_bar, office.home_bar), (26, None));
        assert_eq!(mode(text, Some("travel")).unwrap().look.home_bar, Some(0));
        assert_eq!(mode(text, Some("desk")).unwrap().look.home_bar, Some(12));
    }

    #[test]
    fn a_local_bar_that_is_neither_auto_nor_pixels_fails_clearly() {
        for bad in ["-1", "tall", "2.5", "Auto"] {
            let error = mode(&format!("monitor = DP-2\nlocal_bar = {bad}\n"), None)
                .unwrap_err()
                .to_string();
            assert!(error.contains("local_bar"), "{bad}: {error}");
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
    fn local_label_and_local_bar_are_read_globally_and_per_section() {
        let text = "local_label = NEAR\nlocal_bar = auto\n\
                    [office]\nmonitor = DP-1\n\
                    [travel]\nmonitor = DP-2\nlocal_label = HERE\nlocal_bar = 12\n";
        let office = mode(text, Some("office")).unwrap();
        assert_eq!(office.look.home_label, "NEAR");
        assert_eq!(office.look.home_bar, None);
        assert_eq!(office.notice, None);
        let travel = mode(text, Some("travel")).unwrap();
        assert_eq!(travel.look.home_label, "HERE");
        assert_eq!(travel.look.home_bar, Some(12));
        assert_eq!(travel.notice, None);
    }

    #[test]
    fn a_file_that_still_names_home_label_or_home_bar_is_read_with_a_one_line_notice() {
        let mode_ = mode("monitor = DP-2\nhome_label = NEAR\n", None).unwrap();
        assert_eq!(mode_.look.home_label, "NEAR");
        let notice = mode_.notice.expect("a notice");
        assert!(
            notice.contains("home_label") && notice.contains("local_label"),
            "{notice}"
        );
        assert!(!notice.contains('\n'), "{notice}");
        let mode_ = mode("monitor = DP-2\nhome_bar = auto\n", None).unwrap();
        assert_eq!(mode_.look.home_bar, None);
        let notice = mode_.notice.expect("a notice");
        assert!(
            notice.contains("home_bar") && notice.contains("local_bar"),
            "{notice}"
        );
        // Both old keys: still one line, naming both.
        let mode_ = mode("monitor = DP-2\nhome_label = A\nhome_bar = 3\n", None).unwrap();
        let notice = mode_.notice.expect("a notice");
        assert!(
            notice.contains("local_label") && notice.contains("local_bar"),
            "{notice}"
        );
        assert!(!notice.contains('\n'), "{notice}");
        // The old name is read from the defaults and from a section, like the new one.
        let text = "home_label = NEAR\n[office]\nmonitor = DP-1\n\
                    [travel]\nmonitor = DP-2\nhome_bar = 7\n";
        let office = mode(text, Some("office")).unwrap();
        assert_eq!(office.look.home_label, "NEAR");
        assert!(office.notice.is_some());
        let travel = mode(text, Some("travel")).unwrap();
        assert_eq!(travel.look.home_bar, Some(7));
        assert_eq!(travel.look.home_label, "NEAR");
    }

    #[test]
    fn with_both_the_old_and_the_new_key_the_new_one_wins_and_the_notice_says_so() {
        let text = "monitor = DP-2\nhome_label = OLD\nlocal_label = NEW\n\
                    home_bar = 5\nlocal_bar = auto\n";
        let mode_ = mode(text, None).unwrap();
        assert_eq!(mode_.look.home_label, "NEW");
        assert_eq!(mode_.look.home_bar, None);
        let notice = mode_.notice.expect("a notice");
        assert!(
            notice.contains("local_label") && notice.contains("local_bar"),
            "{notice}"
        );
        assert!(notice.contains("ignored"), "{notice}");
        assert!(!notice.contains('\n'), "{notice}");
        // The order in the file does not matter.
        let text = "monitor = DP-2\nlocal_label = NEW\nhome_label = OLD\n";
        assert_eq!(mode(text, None).unwrap().look.home_label, "NEW");
    }

    #[test]
    fn a_section_s_old_key_still_beats_a_default_new_key() {
        let text = "local_label = DEFAULT\n[office]\nmonitor = DP-1\nhome_label = OWN\n";
        let office = mode(text, Some("office")).unwrap();
        assert_eq!(office.look.home_label, "OWN");
        assert!(office.notice.is_some());
    }

    #[test]
    fn a_closed_lid_notice_and_a_renamed_key_notice_share_one_line() {
        let mode_ = mode("monitor = DP-2\nclosed_lid = on\nhome_label = A\n", None).unwrap();
        let notice = mode_.notice.expect("a notice");
        assert!(
            notice.contains("closed_lid") && notice.contains("local_label"),
            "{notice}"
        );
        assert!(!notice.contains('\n'), "{notice}");
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
    fn the_mic_stays_fixed_by_default() {
        assert!(!mode("monitor = DP-2\n", None).unwrap().mic_follows);
        assert!(!mode("monitor = DP-2\nmic =\n", None).unwrap().mic_follows);
    }

    #[test]
    fn mic_fixed_or_follow_is_read_globally_and_per_section() {
        let text = "mic = follow\n\
                    [office]\nmonitor = DP-1\n\
                    [call]\nmonitor = DP-2\nmic = fixed\n";
        assert!(mode(text, Some("office")).unwrap().mic_follows);
        assert!(!mode(text, Some("call")).unwrap().mic_follows);
        let text = "[office]\nmonitor = DP-1\nmic = follow\n";
        assert!(mode(text, Some("office")).unwrap().mic_follows);
    }

    #[test]
    fn an_unknown_mic_value_is_refused_naming_the_key() {
        let error = mode("monitor = DP-2\nmic = always\n", None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let text = error.to_string();
        assert!(text.contains("`mic = always`"), "{text}");
        assert!(text.contains("`fixed` or `follow`"), "{text}");
    }

    #[test]
    fn here_notice_is_on_by_default() {
        assert!(mode("monitor = DP-2\n", None).unwrap().here_notice);
        assert!(
            mode("monitor = DP-2\nhere_notice =\n", None)
                .unwrap()
                .here_notice
        );
        assert!(
            mode("monitor = DP-2\nhere_notice = on\n", None)
                .unwrap()
                .here_notice
        );
    }

    #[test]
    fn here_notice_off_is_read() {
        assert!(
            !mode("monitor = DP-2\nhere_notice = off\n", None)
                .unwrap()
                .here_notice
        );
    }

    #[test]
    fn here_notice_is_read_globally_only() {
        let text = "[office]\nmonitor = DP-2\nhere_notice = off\n";
        assert!(mode(text, Some("office")).unwrap().here_notice);
        let text = "here_notice = off\n[office]\nmonitor = DP-2\nhere_notice = on\n";
        assert!(!mode(text, Some("office")).unwrap().here_notice);
    }

    #[test]
    fn an_unknown_here_notice_value_is_refused_naming_the_key() {
        let error = mode("monitor = DP-2\nhere_notice = sometimes\n", None).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("`here_notice = sometimes`"), "{text}");
        assert!(text.contains("`on` or `off`"), "{text}");
    }

    #[test]
    fn local_above_defaults_to_the_selection_overlays_of_slurp_and_hyprpicker() {
        let mode = mode("monitor = DP-2\n", None).unwrap();
        assert_eq!(mode.local_above, ["selection", "hyprpicker"]);
    }

    #[test]
    fn local_above_is_a_list_read_globally_and_per_section() {
        let text = "local_above = selection, invented-picker\n\
                    [office]\nmonitor = DP-1\n\
                    [call]\nmonitor = DP-2\nlocal_above = invented-shot\n";
        assert_eq!(
            mode(text, Some("office")).unwrap().local_above,
            ["selection", "invented-picker"]
        );
        assert_eq!(
            mode(text, Some("call")).unwrap().local_above,
            ["invented-shot"]
        );
    }

    #[test]
    fn local_above_none_lists_nothing() {
        let none = mode("monitor = DP-2\nlocal_above = none\n", None).unwrap();
        assert!(none.local_above.is_empty());
        let empty = mode("monitor = DP-2\nlocal_above =\n", None).unwrap();
        assert!(empty.local_above.is_empty());
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
            resolve(&specs, &monitors, &Names::default())
                .map(|m| m.into_iter().map(|m| m.name).collect::<Vec<_>>())
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
        let none = resolve(&specs("desc:Iiyama"), &monitors, &Names::default()).unwrap_err();
        assert_eq!(none.kind(), io::ErrorKind::NotFound);
        let none = none.to_string();
        assert!(
            none.contains("desc:Iiyama") && none.contains("DP-1, DP-2"),
            "{none}"
        );
        let two = resolve(&specs("desc:Dell Inc."), &monitors, &Names::default()).unwrap_err();
        let two = two.to_string();
        assert!(two.contains("DP-1") && two.contains("DP-2"), "{two}");
        assert!(two.contains("more of the description"), "{two}");
        // Told apart by the serial.
        let one = resolve(
            &specs("desc:Dell Inc. DELL U2723QE 7XYZ456"),
            &monitors,
            &Names::default(),
        )
        .unwrap();
        assert_eq!(one[0].name, "DP-2");
    }

    #[test]
    fn a_monitor_that_is_not_there_is_missing_on_the_desk_by_its_label() {
        let monitors = [monitor("DP-1", "Dell Inc. DELL U2723QE 7XYZ123")];
        let specs = ["DP-3".to_string()];
        let default = resolve(&specs, &monitors, &Names::default()).unwrap_err();
        assert!(
            default
                .to_string()
                .contains("no monitor DP-3 on the desk ("),
            "{default}"
        );
        let labelled = resolve(&specs, &monitors, &Names::new(Some("NEAR"), None)).unwrap_err();
        assert!(
            labelled.to_string().contains("no monitor DP-3 on NEAR ("),
            "{labelled}"
        );
    }

    #[test]
    fn a_mode_names_the_machines_by_its_labels_else_desk_and_remote() {
        let none = mode("monitor = DP-2\n", None).unwrap();
        assert_eq!(none.names, Names::default());
        let text = "remote_label = FAR\nlocal_label = NEAR\n\
                    [office]\nmonitor = DP-1\n\
                    [travel]\nmonitor = DP-2\nremote_label = AWAY\n\
                    [bare]\nmonitor = DP-3\nlocal_label =\n";
        let office = mode(text, Some("office")).unwrap();
        assert_eq!(office.names, Names::new(Some("NEAR"), Some("FAR")));
        let travel = mode(text, Some("travel")).unwrap();
        assert_eq!(travel.names, Names::new(Some("NEAR"), Some("AWAY")));
        // An empty label is none: the corner says LOCAL, the sentences say the desk.
        let bare = mode(text, Some("bare")).unwrap();
        assert_eq!(bare.names.desk, "desk");
    }

    #[test]
    fn the_old_home_label_still_names_the_desk_in_the_sentences() {
        let mode_ = mode("monitor = DP-2\nhome_label = NEAR\n", None).unwrap();
        assert_eq!(mode_.names.desk, "NEAR");
    }

    #[test]
    fn a_label_that_is_the_default_tag_still_counts_as_set() {
        // `LOCAL` is what the corner says with no label; set by hand, it is the user's word.
        let mode_ = mode(
            "monitor = DP-2\nlocal_label = LOCAL\nremote_label = REMOTE\n",
            None,
        )
        .unwrap();
        assert_eq!(mode_.names, Names::new(Some("LOCAL"), Some("REMOTE")));
    }

    #[test]
    fn the_desk_label_the_link_carries_is_the_one_before_any_section() {
        let dir = written("local_label = NEAR\n[office]\nmonitor = DP-1\nlocal_label = HERE\n");
        assert_eq!(desk_label(dir.path()), Some("NEAR".to_string()));
        let dir = written("home_label = OLD\n[office]\nmonitor = DP-1\n");
        assert_eq!(desk_label(dir.path()), Some("OLD".to_string()));
        let dir = written("[office]\nmonitor = DP-1\nlocal_label = HERE\n");
        assert_eq!(desk_label(dir.path()), None);
        assert_eq!(desk_label(tempfile::tempdir().unwrap().path()), None);
        // A label the corner could not draw is not sent either: the mode would not start.
        let dir = written("local_label = Café\n");
        assert_eq!(desk_label(dir.path()), None);
    }

    #[test]
    fn a_monitor_named_twice_by_name_and_description_is_refused() {
        let monitors = [monitor("DP-1", "Dell Inc. DELL U2723QE 7XYZ123")];
        let specs = ["DP-1".to_string(), "desc:Dell".to_string()];
        let error = resolve(&specs, &monitors, &Names::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("DP-1 twice"), "{error}");
    }

    #[test]
    fn a_disabled_monitor_is_not_there() {
        let mut off = monitor("DP-1", "Dell Inc. DELL U2723QE 7XYZ123");
        off.disabled = true;
        let error = resolve(&["DP-1".to_string()], &[off], &Names::default()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn the_modes_for_json_name_the_unnamed_one_null_and_leave_the_broken_ones_out() {
        let dir = written(
            "monitor = DP-1\n[office]\nmonitor = DP-1, desc:Invented Display\n\
             [broken]\nrelease = none\n[travel]\nmonitor = DP-2\n",
        );
        let named = |name: Option<&str>, monitors: &[&str]| Defined {
            name: name.map(str::to_string),
            monitors: monitors.iter().map(|m| m.to_string()).collect(),
        };
        assert_eq!(
            defined(dir.path()).unwrap(),
            [
                named(None, &["DP-1"]),
                named(Some("office"), &["DP-1", "desc:Invented Display"]),
                named(Some("travel"), &["DP-2"]),
            ]
        );
        // With no default monitor, there is no unnamed mode.
        let dir = written("release = none\n[office]\nmonitor = DP-1\n");
        assert_eq!(
            defined(dir.path()).unwrap(),
            [named(Some("office"), &["DP-1"])]
        );
        let none = tempfile::tempdir().unwrap();
        assert_eq!(
            defined(none.path()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn the_remote_label_is_the_one_before_any_section_or_the_corner_s() {
        let dir = written("remote_label = FAR\n[office]\nmonitor = DP-1\nremote_label = AWAY\n");
        assert_eq!(remote_label(dir.path()), "FAR");
        let dir = written("[office]\nmonitor = DP-1\nremote_label = AWAY\n");
        assert_eq!(remote_label(dir.path()), "REMOTE");
        assert_eq!(remote_label(tempfile::tempdir().unwrap().path()), "REMOTE");
    }
}
