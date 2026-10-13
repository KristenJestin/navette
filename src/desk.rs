//! What a mode changes on the home desk, recorded so it can always be undone.
//!
//! Each change is a pair of runtime Lua chunks: one that applies it, one that undoes it. The undo
//! chunks are written to a private file under `$XDG_RUNTIME_DIR` before the change is applied,
//! so a run that crashes leaves behind exactly what the next start must undo.
//!
//! Hyprland 0.56.2 has no way to remove a runtime rule: `hl.window_rule` and `hl.layer_rule`
//! only add one, or update the rule that already has the same `name`. So a change is a *named*
//! rule, and undoing it calls the same function again with `enabled = false`. Calling it again
//! with its effects would append them a second time to the disabled rule, so every run names
//! its rules afresh ([`Desk::rule_name`]). Workspace rules are left out on purpose: Hyprland
//! merges one into the user's own rule for the same workspace, which nothing can undo cleanly.
//!
//! A config reload clears every rule, the user's and Navette's, and reloads the user's config:
//! [`Desk::handle`] then applies the changes again, or rolls everything back if it cannot.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hypr::{Event, HyprError, Hyprland, lua_string};

/// One change to the desk, and how to undo it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    apply: String,
    undo: String,
}

impl Change {
    /// A named window rule. `fields` is the rest of the Lua table, e.g.
    /// `match = { workspace = "2" }, monitor = "HOME-1"`; every string in it must go through
    /// [`lua_string`].
    pub fn window_rule(name: &str, fields: &str) -> Self {
        Self::named_rule("hl.window_rule", name, fields)
    }

    /// A named layer rule, `fields` as for [`Change::window_rule`].
    pub fn layer_rule(name: &str, fields: &str) -> Self {
        Self::named_rule("hl.layer_rule", name, fields)
    }

    fn named_rule(function: &str, name: &str, fields: &str) -> Self {
        let name = lua_string(name);
        Self {
            apply: format!("{function}({{ name = {name}, {fields} }})"),
            undo: format!("{function}({{ name = {name}, enabled = false }})"),
        }
    }
}

/// Why the desk could not be changed or restored.
#[derive(Debug)]
pub enum DeskError {
    Hypr(HyprError),
    /// The record under `$XDG_RUNTIME_DIR` could not be read or written.
    Record(io::Error),
    /// Another Navette holds the desk.
    Busy,
}

impl fmt::Display for DeskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeskError::Hypr(e) => e.fmt(f),
            DeskError::Record(e) => write!(f, "the desk record: {e}"),
            DeskError::Busy => write!(f, "another Navette is already changing this desk"),
        }
    }
}

impl std::error::Error for DeskError {}

impl From<HyprError> for DeskError {
    fn from(e: HyprError) -> Self {
        DeskError::Hypr(e)
    }
}

/// The changes applied to the home desk, in order.
pub struct Desk<H: Hyprland> {
    hypr: H,
    record: PathBuf,
    run: String,
    changes: Vec<Change>,
    /// Held while this desk lives, so a second Navette cannot take its rules for leftovers.
    _lock: Option<fs::File>,
}

impl<H: Hyprland> Desk<H> {
    /// `$XDG_RUNTIME_DIR/navette/desk`: gone at logout, like the runtime rules it lists.
    pub fn default_record() -> io::Result<PathBuf> {
        std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(|dir| PathBuf::from(dir).join("navette").join("desk"))
            .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))
    }

    /// Opens the desk, first undoing whatever a previous run left in `record`. Fails with
    /// [`DeskError::Busy`] while another Navette holds it.
    pub fn start(hypr: H, record: impl Into<PathBuf>) -> Result<Self, DeskError> {
        let record = record.into();
        let lock = lock(&record)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut desk = Self {
            hypr,
            record,
            run: format!("{nanos:x}"),
            changes: Vec::new(),
            _lock: Some(lock),
        };
        let leftovers = read_record(&desk.record).map_err(DeskError::Record)?;
        let remaining = desk.undo_all(leftovers);
        desk.finish_undo(remaining)?;
        Ok(desk)
    }

    /// The name of a rule for this run: `navette-<base>-<run>`.
    pub fn rule_name(&self, base: &str) -> String {
        format!("navette-{base}-{}", self.run)
    }

    /// Applies one change. It is recorded first, so a crash cannot leak it; if Hyprland refuses
    /// it, it is undone and forgotten.
    pub fn apply(&mut self, change: Change) -> Result<(), DeskError> {
        self.changes.push(change);
        if let Err(e) = self.save() {
            self.changes.pop();
            return Err(e);
        }
        let change = self.changes.last().expect("just pushed");
        if let Err(e) = self.hypr.eval(&change.apply) {
            let change = self.changes.pop().expect("just pushed");
            let _ = self.hypr.eval(&change.undo);
            self.save()?;
            return Err(e.into());
        }
        Ok(())
    }

    /// Reacts to a compositor event: after a config reload, applies every change again, in
    /// order. If one is refused, rolls the whole desk back and returns the refusal.
    pub fn handle(&mut self, event: &Event) -> Result<(), DeskError> {
        match event {
            Event::ConfigReloaded => self.reapply(),
            _ => Ok(()),
        }
    }

    /// Applies every change again, once Hyprland has dropped them (a reload, a restart).
    pub fn reapply(&mut self) -> Result<(), DeskError> {
        let failed = self
            .changes
            .iter()
            .find_map(|change| self.hypr.eval(&change.apply).err());
        match failed {
            None => Ok(()),
            Some(e) => {
                self.stop()?;
                Err(e.into())
            }
        }
    }

    /// Undoes every change, the last one first. What could not reach Hyprland stays recorded,
    /// for the next start to try again; what Hyprland refuses is dropped.
    pub fn stop(&mut self) -> Result<(), DeskError> {
        let undos = self.changes.drain(..).map(|c| c.undo).collect();
        let remaining = self.undo_all(undos);
        self.finish_undo(remaining)
    }

    /// Runs `undos` last first; returns the ones that could not reach Hyprland, in record order,
    /// with the first such error. A refused undo is logged and dropped: Hyprland will not take
    /// it later either (a garbled line), and keeping it would fail every start.
    fn undo_all(&mut self, undos: Vec<String>) -> (Vec<String>, Option<HyprError>) {
        let mut remaining = Vec::new();
        let mut first_error = None;
        for undo in undos.into_iter().rev() {
            match self.hypr.eval(&undo) {
                Ok(()) => {}
                Err(HyprError::Refused(answer)) => {
                    eprintln!("navette: dropping an undo Hyprland refused ({answer}): {undo}");
                }
                Err(e) => {
                    first_error.get_or_insert(e);
                    remaining.push(undo);
                }
            }
        }
        remaining.reverse();
        (remaining, first_error)
    }

    fn finish_undo(
        &self,
        (remaining, error): (Vec<String>, Option<HyprError>),
    ) -> Result<(), DeskError> {
        write_record(&self.record, &remaining).map_err(DeskError::Record)?;
        error.map_or(Ok(()), |e| Err(e.into()))
    }

    fn save(&self) -> Result<(), DeskError> {
        let undos: Vec<String> = self.changes.iter().map(|c| c.undo.clone()).collect();
        write_record(&self.record, &undos).map_err(DeskError::Record)
    }
}

impl<H: Hyprland> Drop for Desk<H> {
    /// Best effort, for the exits that do not call [`Desk::stop`]: an error, a panic.
    fn drop(&mut self) {
        if !self.changes.is_empty()
            && let Err(e) = self.stop()
        {
            eprintln!("navette: could not restore the desk: {e}");
        }
    }
}

/// Takes `<record>.lock`; the kernel releases it when the process ends, crash included.
fn lock(record: &Path) -> Result<fs::File, DeskError> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let dir = record.parent().unwrap_or(Path::new("."));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(DeskError::Record)?;
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(record.with_extension("lock"))
        .map_err(DeskError::Record)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(DeskError::Busy),
        Err(fs::TryLockError::Error(e)) => Err(DeskError::Record(e)),
    }
}

/// The undo chunks in `path`, one escaped chunk per line.
fn read_record(path: &Path) -> io::Result<Vec<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text.lines().map(unescape).collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Replaces the record atomically (0600, in a 0700 directory); an empty one is removed.
fn write_record(path: &Path, undos: &[String]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    if undos.is_empty() {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    for undo in undos {
        writeln!(file, "{}", escape(undo))?;
    }
    file.sync_all()?;
    fs::rename(&tmp, path)
}

fn escape(chunk: &str) -> String {
    chunk
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unescape(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some('n')) => {
                out.push('\n');
                chars.next();
            }
            ('\\', Some('r')) => {
                out.push('\r');
                chars.next();
            }
            ('\\', Some('\\')) => {
                out.push('\\');
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    /// A Hyprland that keeps named rules and whether each is enabled, as 0.56.2 does.
    #[derive(Clone, Default)]
    struct FakeHyprland(Rc<RefCell<FakeState>>);

    #[derive(Default)]
    struct FakeState {
        rules: BTreeMap<String, bool>,
        /// Applying a chunk containing this is refused; undoing never is.
        refuse: Option<String>,
        unreachable: bool,
    }

    impl FakeHyprland {
        /// A desk with one rule of the user's own.
        fn with_user_rule() -> Self {
            let fake = Self::default();
            fake.0.borrow_mut().rules.insert("user-rule".into(), true);
            fake
        }

        fn enabled_rules(&self) -> Vec<String> {
            let state = self.0.borrow();
            state
                .rules
                .iter()
                .filter(|(_, on)| **on)
                .map(|(n, _)| n.clone())
                .collect()
        }

        /// `hyprctl reload`: every rule goes, the user's config brings its own back.
        fn reload(&self) {
            let mut state = self.0.borrow_mut();
            state.rules.clear();
            state.rules.insert("user-rule".into(), true);
        }

        fn refuse(&self, pattern: Option<&str>) {
            self.0.borrow_mut().refuse = pattern.map(str::to_owned);
        }

        fn set_unreachable(&self, unreachable: bool) {
            self.0.borrow_mut().unreachable = unreachable;
        }
    }

    impl Hyprland for FakeHyprland {
        fn eval(&mut self, lua: &str) -> Result<(), HyprError> {
            let mut state = self.0.borrow_mut();
            if state.unreachable {
                return Err(HyprError::Io(io::ErrorKind::NotFound.into()));
            }
            let undo = lua.contains("enabled = false");
            if !undo && state.refuse.as_deref().is_some_and(|p| lua.contains(p)) {
                return Err(HyprError::Refused("error: refused".into()));
            }
            let Some(name) = lua
                .split_once("name = \"")
                .and_then(|(_, rest)| rest.split_once('"'))
                .map(|(name, _)| name.to_owned())
            else {
                return Err(HyprError::Refused("error: not a rule".into()));
            };
            state.rules.insert(name, !undo);
            Ok(())
        }
    }

    fn keep_home_off(desk: &Desk<FakeHyprland>, base: &str) -> Change {
        Change::window_rule(
            &desk.rule_name(base),
            r#"match = { workspace = "2" }, monitor = "HOME-1""#,
        )
    }

    /// What a crash leaves: no stop, no Drop; the kernel releases the lock.
    fn crash(desk: Desk<FakeHyprland>) {
        let mut desk = std::mem::ManuallyDrop::new(desk);
        desk._lock.take();
    }

    fn record_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("navette").join("desk")
    }

    #[test]
    fn start_then_stop_leaves_the_desk_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHyprland::with_user_rule();
        let before = fake.enabled_rules();

        let mut desk = Desk::start(fake.clone(), record_in(&dir)).unwrap();
        desk.apply(keep_home_off(&desk, "windows")).unwrap();
        desk.apply(keep_home_off(&desk, "layers")).unwrap();
        assert_eq!(fake.enabled_rules().len(), before.len() + 2);
        desk.stop().unwrap();

        assert_eq!(fake.enabled_rules(), before);
        assert!(!record_in(&dir).exists());
    }

    #[test]
    fn a_reload_event_reapplies_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHyprland::with_user_rule();
        let mut desk = Desk::start(fake.clone(), record_in(&dir)).unwrap();
        desk.apply(keep_home_off(&desk, "windows")).unwrap();
        let applied = fake.enabled_rules();

        fake.reload();
        assert_ne!(fake.enabled_rules(), applied);
        desk.handle(&Event::ConfigReloaded).unwrap();

        assert_eq!(fake.enabled_rules(), applied);
    }

    #[test]
    fn a_reload_that_cannot_reapply_rolls_the_desk_back() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHyprland::with_user_rule();
        let before = fake.enabled_rules();
        let mut desk = Desk::start(fake.clone(), record_in(&dir)).unwrap();
        desk.apply(keep_home_off(&desk, "windows")).unwrap();
        desk.apply(keep_home_off(&desk, "layers")).unwrap();

        fake.reload();
        fake.refuse(Some("layers"));
        assert!(desk.handle(&Event::ConfigReloaded).is_err());

        assert_eq!(fake.enabled_rules(), before);
        assert!(!record_in(&dir).exists());
    }

    #[test]
    fn other_events_change_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHyprland::with_user_rule();
        let mut desk = Desk::start(fake.clone(), record_in(&dir)).unwrap();
        desk.apply(keep_home_off(&desk, "windows")).unwrap();
        let applied = fake.enabled_rules();

        fake.set_unreachable(true);
        desk.handle(&Event::MonitorAdded("DP-9".into())).unwrap();
        desk.handle(&Event::Other).unwrap();
        fake.set_unreachable(false);

        assert_eq!(fake.enabled_rules(), applied);
    }

    #[test]
    fn a_new_start_cleans_what_a_crashed_run_left() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHyprland::with_user_rule();
        let before = fake.enabled_rules();
        {
            let mut crashed = Desk::start(fake.clone(), record_in(&dir)).unwrap();
            crashed.apply(keep_home_off(&crashed, "windows")).unwrap();
            crashed.apply(keep_home_off(&crashed, "layers")).unwrap();
            crash(crashed);
        }
        assert_ne!(fake.enabled_rules(), before);

        let _desk = Desk::start(fake.clone(), record_in(&dir)).unwrap();

        assert_eq!(fake.enabled_rules(), before);
        assert!(!record_in(&dir).exists());
    }

    #[test]
    fn a_refused_change_is_neither_applied_nor_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHyprland::with_user_rule();
        let before = fake.enabled_rules();
        let mut desk = Desk::start(fake.clone(), record_in(&dir)).unwrap();

        fake.refuse(Some("monitor"));
        assert!(desk.apply(keep_home_off(&desk, "windows")).is_err());

        assert_eq!(fake.enabled_rules(), before);
        assert!(!record_in(&dir).exists());
    }

    #[test]
    fn a_stop_hyprland_cannot_hear_is_finished_by_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHyprland::with_user_rule();
        let before = fake.enabled_rules();
        let mut desk = Desk::start(fake.clone(), record_in(&dir)).unwrap();
        desk.apply(keep_home_off(&desk, "windows")).unwrap();

        fake.set_unreachable(true);
        assert!(desk.stop().is_err());
        assert!(record_in(&dir).exists());
        fake.set_unreachable(false);
        drop(desk);

        Desk::start(fake.clone(), record_in(&dir)).unwrap();
        assert_eq!(fake.enabled_rules(), before);
        assert!(!record_in(&dir).exists());
    }

    #[test]
    fn each_run_names_its_rules_afresh() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let first = Desk::start(FakeHyprland::default(), record_in(&dir)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let second = Desk::start(FakeHyprland::default(), record_in(&other)).unwrap();
        assert!(first.rule_name("windows").starts_with("navette-windows-"));
        assert_ne!(first.rule_name("windows"), second.rule_name("windows"));
    }

    #[test]
    fn the_record_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut desk = Desk::start(FakeHyprland::default(), record_in(&dir)).unwrap();
        desk.apply(keep_home_off(&desk, "windows")).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&record_in(&dir)), 0o600);
        assert_eq!(mode(record_in(&dir).parent().unwrap()), 0o700);
    }

    #[test]
    fn rules_are_named_and_undone_by_disabling_them() {
        let change = Change::window_rule("navette-x", r#"monitor = "HOME-1""#);
        assert_eq!(
            change.apply,
            r#"hl.window_rule({ name = "navette-x", monitor = "HOME-1" })"#
        );
        assert_eq!(
            change.undo,
            r#"hl.window_rule({ name = "navette-x", enabled = false })"#
        );
        assert_eq!(
            Change::layer_rule("navette-y", "no_anim = true").undo,
            r#"hl.layer_rule({ name = "navette-y", enabled = false })"#
        );
    }

    #[test]
    fn a_corrupt_record_does_not_block_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHyprland::with_user_rule();
        let before = fake.enabled_rules();
        fs::create_dir_all(record_in(&dir).parent().unwrap()).unwrap();
        fs::write(record_in(&dir), "garbled \\x line\n").unwrap();

        let mut desk = Desk::start(fake.clone(), record_in(&dir)).unwrap();

        assert_eq!(fake.enabled_rules(), before);
        assert!(!record_in(&dir).exists());
        desk.apply(keep_home_off(&desk, "windows")).unwrap();
    }

    #[test]
    fn a_record_from_before_a_hyprland_restart_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let old = FakeHyprland::with_user_rule();
        let mut crashed = Desk::start(old.clone(), record_in(&dir)).unwrap();
        crashed.apply(keep_home_off(&crashed, "windows")).unwrap();
        crash(crashed);

        // Hyprland restarted: a new instance, with only the user's config.
        let restarted = FakeHyprland::with_user_rule();
        let before = restarted.enabled_rules();
        Desk::start(restarted.clone(), record_in(&dir)).unwrap();

        assert_eq!(restarted.enabled_rules(), before);
        assert!(!record_in(&dir).exists());
    }

    #[test]
    fn a_change_that_cannot_be_recorded_is_not_applied_nor_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHyprland::with_user_rule();
        let before = fake.enabled_rules();
        let mut desk = Desk::start(fake.clone(), record_in(&dir)).unwrap();
        let parent = record_in(&dir).parent().unwrap().to_owned();
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();

        let result = desk.apply(keep_home_off(&desk, "windows"));
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(matches!(result, Err(DeskError::Record(_))));
        assert!(desk.changes.is_empty());
        assert_eq!(fake.enabled_rules(), before);
    }

    #[test]
    fn dropping_the_desk_undoes_its_changes() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHyprland::with_user_rule();
        let before = fake.enabled_rules();
        {
            let mut desk = Desk::start(fake.clone(), record_in(&dir)).unwrap();
            desk.apply(keep_home_off(&desk, "windows")).unwrap();
        }
        assert_eq!(fake.enabled_rules(), before);
        assert!(!record_in(&dir).exists());
    }

    #[test]
    fn a_second_navette_does_not_undo_the_first_ones_rules() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeHyprland::with_user_rule();
        let mut first = Desk::start(fake.clone(), record_in(&dir)).unwrap();
        first.apply(keep_home_off(&first, "windows")).unwrap();
        let applied = fake.enabled_rules();

        let second = Desk::start(fake.clone(), record_in(&dir));

        assert!(second.is_err());
        assert_eq!(fake.enabled_rules(), applied);
        assert!(record_in(&dir).exists());
        drop(first);
        Desk::start(fake.clone(), record_in(&dir)).unwrap();
    }

    #[test]
    fn multi_line_chunks_survive_the_record() {
        for chunk in [
            "a\nb",
            "cr\r\nlf",
            "back\\slash",
            "\\n literally",
            "\\r too",
            "",
        ] {
            assert_eq!(unescape(&escape(chunk)), chunk);
            assert!(!escape(chunk).contains(['\n', '\r']));
        }
    }
}
