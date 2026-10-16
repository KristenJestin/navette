//! The remote laptop's lid while a mode runs: it can be closed and opened at any moment.
//! Closed, the laptop's own screen is disabled at runtime and Hyprland moves its workspaces to
//! Navette's outputs; opened, it comes back with the mode it had. The lid does not suspend the
//! laptop meanwhile: the agent holds logind's `handle-lid-switch` inhibitor lock for the mode's
//! duration, which needs no root. When logind or the machine's policy refuses the lock, or the
//! lid cannot be read, the agent says so and leaves the lid alone: the laptop's settings are
//! never worked around.
//!
//! The lid is watched on a thread of its own ([`Watch`]): logind and Hyprland answer it there,
//! never in the loop that carries home's input. logind's `LidClosed` is read once a second:
//! `busctl monitor` is refused to a user (measured in #36), and a read cannot miss a change.
//!
//! The rules that bring the screen back are written to a file before the screen is disabled,
//! and removed once it is back: whenever the file is there, the agent puts the screen back as
//! soon as it can, after a crash included.

use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crate::display::output;
use crate::hypr::{Hypr, Monitor, lua_string};

/// logind, as the agent uses it (the port behind which tests put a fake).
pub trait Logind: Send + Sync {
    /// Whether the lid is closed now: logind's `LidClosed`.
    fn lid_closed(&self) -> io::Result<bool>;
    /// Takes the `handle-lid-switch` inhibitor lock, held until the guard is dropped.
    fn inhibit_lid(&self) -> io::Result<Box<dyn Send + Sync>>;
}

/// How often the lid is read while a mode runs: the lock keeps the
/// laptop up meanwhile, so a second is soon enough.
pub const POLL: Duration = Duration::from_secs(1);

/// Reads of the lid failed in a row before the lock is let go: holding it would keep the
/// laptop up for a lid nobody watches.
const FAILURES: u32 = 5;

/// The laptop's own screens go by their connector's name, whatever the order of arrival; a
/// laptop with two panels has both disabled and both brought back.
const INTERNAL: [&str; 3] = ["eDP-", "LVDS-", "DSI-"];

fn is_internal(m: &Monitor) -> bool {
    INTERNAL.iter().any(|prefix| m.name.starts_with(prefix))
}

/// What a record holds: one rule per screen, nothing else.
const RULE: &str = "eval hl.monitor(";

/// Where the rules that bring the screens back are kept: the runtime directory, which lives as
/// long as the session's Hyprland and its runtime rules do. `None` without one.
pub fn record_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(runtime).join("navette").join("lid"))
}

/// Disables the laptop's screens that are on, if one of Navette's outputs is there to take
/// their workspaces, after writing to `record` the rules that bring them back.
pub fn close(hypr: &dyn Hypr, record: &Path) -> io::Result<()> {
    let monitors = crate::hypr::monitors(hypr)?;
    let on: Vec<&Monitor> = monitors
        .iter()
        .filter(|m| is_internal(m) && !m.disabled)
        .collect();
    // Disabled with no other output on, the laptop would have nowhere to put its windows.
    let copy_on = monitors
        .iter()
        .any(|m| output::is_copy(&m.name) && !m.disabled);
    if on.is_empty() || !copy_on {
        return Ok(());
    }
    // A screen disabled already (the user's, or ours) keeps the rule recorded for it.
    let mut rules: Vec<String> = read(record)?
        .into_iter()
        .filter(|kept| !on.iter().any(|m| kept.contains(&lua_string(&m.name))))
        .collect();
    rules.extend(on.iter().map(|m| rule(m)));
    write_private(record, &rules.join("\n"))?;
    for screen in on {
        crate::hypr::dispatch(
            hypr,
            &format!(
                "eval hl.monitor({{ output = {}, disabled = true }})",
                lua_string(&screen.name)
            ),
        )?;
    }
    Ok(())
}

/// The rule that puts `screen` back as it is now. A rule merges into the one before it, so
/// it has to say `disabled = false` (measured in #36).
fn rule(screen: &Monitor) -> String {
    format!(
        "{RULE}{{ output = {}, mode = \"{}x{}@{}\", position = \"{}x{}\", \
         scale = {}, transform = {}, disabled = false }})",
        lua_string(&screen.name),
        screen.width,
        screen.height,
        output::decimal((screen.refresh_rate * 1000.0).round() as u32),
        screen.x,
        screen.y,
        output::decimal((screen.scale * 1000.0).round() as u32),
        screen.transform,
    )
}

/// The rules in `record`, none when there is no record.
fn read(record: &Path) -> io::Result<Vec<String>> {
    match std::fs::read_to_string(record) {
        Ok(text) => Ok(text.lines().map(str::to_string).collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Writes `text` to `path` whole or not at all, in a directory only this user can enter.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = path.parent().expect("the record sits in a directory");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let next = path.with_extension("next");
    std::fs::write(&next, text)?;
    std::fs::rename(&next, path)
}

/// Puts the screens back as `record` says, if the agent disabled them, and forgets the record.
/// It is kept when Hyprland cannot be reached, for the next try; a rule Hyprland refuses, or
/// one Navette did not write, is dropped, as no later try would do better.
pub fn open(hypr: &dyn Hypr, record: &Path) -> io::Result<()> {
    let rules = read(record)?;
    if rules.is_empty() {
        return Ok(());
    }
    let mut refused = Vec::new();
    for rule in &rules {
        if !rule.starts_with(RULE) {
            refused.push(format!("`{rule}` is not a monitor rule"));
            continue;
        }
        match hypr.request(rule)?.trim() {
            "ok" => {}
            answer => refused.push(format!("Hyprland refused `{rule}`: {answer}")),
        }
    }
    #[cfg(test)]
    std::thread::sleep(Duration::from_millis(100)); // TEMPORARY (#140)
    std::fs::remove_file(record)?;
    match refused.is_empty() {
        true => Ok(()),
        false => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            refused.join("; "),
        )),
    }
}

enum Order {
    Look,
    Stop,
}

/// The lid's watch for one mode, on a thread of its own. It reads the lid once, then takes
/// logind's lock, then keeps the laptop's screens as the lid says; until the lock is held,
/// the mode runs as without the option.
pub struct Watch {
    orders: mpsc::Sender<Order>,
    thread: Option<std::thread::JoinHandle<Option<Box<dyn Send + Sync>>>>,
}

impl Watch {
    /// `told` hears why the lid is left alone, when it is.
    pub fn start(
        logind: std::sync::Arc<dyn Logind>,
        hypr: std::sync::Arc<dyn Hypr>,
        record: Option<PathBuf>,
        poll: Duration,
        told: impl Fn(String) + Send + 'static,
    ) -> io::Result<Watch> {
        let (orders, heard) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("navette-lid".into())
            .spawn(move || watch(&*logind, &*hypr, record, poll, heard, told))?;
        Ok(Watch {
            orders,
            thread: Some(thread),
        })
    }

    /// Reads the lid now: a reload, or a script on the lid, may have turned a screen on.
    pub fn look(&self) {
        let _ = self.orders.send(Order::Look);
    }

    /// Ends the watch and gives logind's lock, if it was held, to be dropped once the screen
    /// is back and the copies are gone. Blocks until the thread is done.
    pub fn stop(mut self) -> Option<Box<dyn Send + Sync>> {
        let _ = self.orders.send(Order::Stop);
        self.thread.take()?.join().ok().flatten()
    }
}

impl Drop for Watch {
    /// Dropped without [`Watch::stop`] (a panic in the agent's loop): the thread brings the
    /// screen back itself, then lets the lock go.
    fn drop(&mut self) {
        let (orders, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.orders, orders));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watch(
    logind: &dyn Logind,
    hypr: &dyn Hypr,
    record: Option<PathBuf>,
    poll: Duration,
    heard: mpsc::Receiver<Order>,
    told: impl Fn(String),
) -> Option<Box<dyn Send + Sync>> {
    let Some(record) = record else {
        told("XDG_RUNTIME_DIR is not set, so nothing can bring the laptop's screen back".into());
        return None;
    };
    // Read once before the lock: a lid that cannot be read is not worth holding the laptop up.
    if let Err(e) = logind.lid_closed() {
        told(format!("the lid cannot be read: {e}"));
        return None;
    }
    let lock = match logind.inhibit_lid() {
        Ok(lock) => lock,
        Err(e) => {
            told(format!(
                "the laptop cannot be kept up with its lid closed: {e}"
            ));
            return None;
        }
    };
    let (mut failures, mut said) = (0, false);
    loop {
        let kept = match logind.lid_closed() {
            Ok(closed) => {
                failures = 0;
                // Open, the record is tried whoever left it: a restore that failed before.
                match closed {
                    true => close(hypr, &record),
                    false => open(hypr, &record),
                }
            }
            Err(e) => {
                failures += 1;
                if failures >= FAILURES {
                    let _ = open(hypr, &record);
                    told(format!(
                        "the lid could not be read {FAILURES} times in a row, the laptop's \
                         lock is let go: {e}"
                    ));
                    return None;
                }
                Ok(())
            }
        };
        match kept {
            Err(e) if !said => {
                said = true;
                eprintln!("navette: lid: {e}");
            }
            Err(_) => {}
            Ok(()) => said = false,
        }
        match heard.recv_timeout(poll) {
            Ok(Order::Look) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Order::Stop) => return Some(lock),
            Err(RecvTimeoutError::Disconnected) => {
                if let Err(e) = open(hypr, &record) {
                    eprintln!("navette: could not bring the laptop's screen back: {e}");
                }
                return Some(lock);
            }
        }
    }
}

/// logind on the system bus, through systemd's own tools.
pub struct LiveLogind;

impl Logind for LiveLogind {
    fn lid_closed(&self) -> io::Result<bool> {
        let reply = Command::new("busctl")
            .args([
                "get-property",
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "LidClosed",
            ])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| io::Error::new(e.kind(), format!("busctl: {e}")))?;
        if !reply.status.success() {
            return Err(io::Error::other(format!(
                "busctl get-property LidClosed: {}",
                String::from_utf8_lossy(&reply.stderr).trim()
            )));
        }
        boolean(&String::from_utf8_lossy(&reply.stdout))
    }

    fn inhibit_lid(&self) -> io::Result<Box<dyn Send + Sync>> {
        Ok(Box::new(Inhibitor::take()?))
    }
}

/// `busctl get-property`'s answer for a boolean: `b true`.
fn boolean(reply: &str) -> io::Result<bool> {
    match reply.trim() {
        "b true" => Ok(true),
        "b false" => Ok(false),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("busctl: logind's LidClosed is `{other}`, not a boolean"),
        )),
    }
}

/// `systemd-inhibit` holding the lock while its command runs: a `cat` on a pipe from the
/// agent, which ends when the agent closes it or dies, so a crash releases the lock too.
struct Inhibitor(Child);

impl Inhibitor {
    fn take() -> io::Result<Self> {
        let mut child = Command::new("systemd-inhibit")
            .args([
                "--what=handle-lid-switch",
                "--who=Navette",
                "--why=A mode runs: the laptop works with its lid closed",
                "--mode=block",
                // A refusal is final: nothing asks for a password instead.
                "--no-ask-password",
                "sh",
                "-c",
                "echo held && exec cat >/dev/null",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("systemd-inhibit: {e}")))?;
        // The command runs once the lock is held, so its word is the proof.
        let mut said = String::new();
        let read = match child.stdout.take() {
            Some(stdout) => BufReader::new(stdout).read_line(&mut said).map(drop),
            None => Ok(()),
        };
        if read.is_ok() && said.trim() == "held" {
            return Ok(Self(child));
        }
        // Whatever went wrong, the child is ended and reaped.
        drop(child.stdin.take());
        let _ = child.kill();
        let mut why = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut why);
        }
        let status = child.wait();
        read?;
        Err(io::Error::other(format!(
            "systemd-inhibit: logind refused the lid-switch lock ({}): {}",
            status.map_or_else(|e| e.to_string(), |s| s.to_string()),
            why.trim()
        )))
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.wait();
    }
}

#[cfg(test)]
pub mod fake {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    /// A lid, and a logind that counts its locks.
    #[derive(Default)]
    pub struct FakeLogind {
        pub closed: AtomicBool,
        /// Why the lock is refused, when it is.
        pub refuse: std::sync::Mutex<Option<String>>,
        /// Locks taken, and those still held.
        pub taken: AtomicUsize,
        pub held: Arc<AtomicUsize>,
        /// Times the lid was looked at.
        pub looked: AtomicUsize,
        /// `LidClosed` cannot be read.
        pub unreadable: AtomicBool,
        /// How long each answer takes.
        pub slow: std::sync::Mutex<std::time::Duration>,
    }

    struct Lock(Arc<AtomicUsize>);

    impl Drop for Lock {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl FakeLogind {
        pub fn close(&self, closed: bool) {
            self.closed.store(closed, Ordering::SeqCst);
        }

        pub fn held(&self) -> usize {
            self.held.load(Ordering::SeqCst)
        }

        pub fn taken(&self) -> usize {
            self.taken.load(Ordering::SeqCst)
        }

        pub fn looked(&self) -> usize {
            self.looked.load(Ordering::SeqCst)
        }

        pub fn unreadable(&self, unreadable: bool) {
            self.unreadable.store(unreadable, Ordering::SeqCst);
        }
    }

    impl Logind for FakeLogind {
        fn lid_closed(&self) -> io::Result<bool> {
            std::thread::sleep(*self.slow.lock().unwrap());
            self.looked.fetch_add(1, Ordering::SeqCst);
            if self.unreadable.load(Ordering::SeqCst) {
                return Err(io::Error::other("busctl: No such file or directory"));
            }
            Ok(self.closed.load(Ordering::SeqCst))
        }

        fn inhibit_lid(&self) -> io::Result<Box<dyn Send + Sync>> {
            std::thread::sleep(*self.slow.lock().unwrap());
            if let Some(why) = self.refuse.lock().unwrap().clone() {
                return Err(io::Error::other(why));
            }
            self.taken.fetch_add(1, Ordering::SeqCst);
            self.held.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Lock(self.held.clone())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypr::fake::{FakeHypr, FakeMonitor};

    fn laptop() -> FakeMonitor {
        FakeMonitor {
            refresh: 59.997,
            scale: 1.25,
            ..FakeMonitor::new("eDP-1", 1920, 1200, 0)
        }
    }

    fn with_a_copy() -> FakeHypr {
        FakeHypr::with(vec![
            laptop(),
            FakeMonitor::new("NAVETTE-1", 2560, 1440, 1536),
        ])
    }

    #[test]
    fn closing_disables_the_laptop_screen_and_opening_brings_it_back_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("navette").join("lid");
        let hypr = with_a_copy();
        close(&hypr, &record).unwrap();
        assert!(hypr.monitor("eDP-1").unwrap().disabled);
        assert!(record.exists());
        open(&hypr, &record).unwrap();
        assert_eq!(hypr.monitor("eDP-1").unwrap(), laptop());
        assert!(!record.exists());
        assert_eq!(
            hypr.changes(),
            [
                "eval hl.monitor({ output = \"eDP-1\", disabled = true })",
                "eval hl.monitor({ output = \"eDP-1\", mode = \"1920x1200@59.997\", \
                 position = \"0x0\", scale = 1.25, transform = 0, disabled = false })",
            ]
        );
    }

    #[test]
    fn the_record_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("navette").join("lid");
        close(&with_a_copy(), &record).unwrap();
        let mode = std::fs::metadata(record.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn closing_again_changes_nothing_and_keeps_the_first_record() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("lid");
        let hypr = with_a_copy();
        close(&hypr, &record).unwrap();
        let first = std::fs::read_to_string(&record).unwrap();
        close(&hypr, &record).unwrap();
        assert_eq!(hypr.changes().len(), 1);
        assert_eq!(std::fs::read_to_string(&record).unwrap(), first);
    }

    #[test]
    fn a_screen_on_again_after_a_reload_is_recorded_as_the_reload_left_it() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("lid");
        let hypr = with_a_copy();
        close(&hypr, &record).unwrap();
        hypr.reload();
        hypr.request(
            "eval hl.monitor({ output = \"eDP-1\", mode = \"1920x1200@60\", \
             position = \"0x0\", scale = 1.5, transform = 0 })",
        )
        .unwrap();
        close(&hypr, &record).unwrap();
        assert!(hypr.monitor("eDP-1").unwrap().disabled);
        assert!(
            std::fs::read_to_string(&record)
                .unwrap()
                .contains("scale = 1.5")
        );
    }

    #[test]
    fn the_screen_stays_on_while_no_copy_can_take_its_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("lid");
        // A Hyprland just restarted: the copies are not back yet.
        let hypr = FakeHypr::with(vec![laptop()]);
        close(&hypr, &record).unwrap();
        assert!(!hypr.monitor("eDP-1").unwrap().disabled);
        assert!(!record.exists());
        assert_eq!(hypr.changes(), [] as [&str; 0]);
    }

    #[test]
    fn a_screen_the_user_disabled_is_left_to_them() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("lid");
        let hypr = FakeHypr::with(vec![
            FakeMonitor {
                disabled: true,
                ..laptop()
            },
            FakeMonitor::new("NAVETTE-1", 2560, 1440, 1536),
        ]);
        close(&hypr, &record).unwrap();
        open(&hypr, &record).unwrap();
        assert!(hypr.monitor("eDP-1").unwrap().disabled);
        assert_eq!(hypr.changes(), [] as [&str; 0]);
    }

    #[test]
    fn with_no_laptop_screen_nothing_is_done() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("lid");
        let hypr = FakeHypr::with(vec![
            FakeMonitor::new("DP-1", 1920, 1080, 0),
            FakeMonitor::new("NAVETTE-1", 2560, 1440, 1920),
        ]);
        close(&hypr, &record).unwrap();
        assert_eq!(hypr.changes(), [] as [&str; 0]);
        assert!(!hypr.monitor("DP-1").unwrap().disabled);
    }

    #[test]
    fn opening_with_nothing_recorded_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let hypr = with_a_copy();
        open(&hypr, &dir.path().join("lid")).unwrap();
        assert_eq!(hypr.changes(), [] as [&str; 0]);
    }

    #[test]
    fn a_record_hyprland_could_not_take_is_kept_for_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("lid");
        let hypr = with_a_copy();
        close(&hypr, &record).unwrap();
        hypr.set_down(true, Vec::new());
        assert!(open(&hypr, &record).is_err());
        assert!(record.exists());
    }

    #[test]
    fn a_record_hyprland_refuses_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("lid");
        std::fs::write(
            &record,
            "eval hl.monitor({ output = \"eDP-1\", mode = \"nonsense\" })",
        )
        .unwrap();
        let error = open(&with_a_copy(), &record).unwrap_err();
        assert!(error.to_string().contains("refused"), "{error}");
        assert!(!record.exists());
    }

    #[test]
    fn a_record_that_is_not_a_monitor_rule_is_dropped_unsent() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("lid");
        std::fs::write(&record, "output remove NAVETTE-1").unwrap();
        let hypr = with_a_copy();
        assert!(open(&hypr, &record).is_err());
        assert!(!record.exists());
        assert_eq!(hypr.changes(), [] as [&str; 0]);
    }

    #[test]
    fn the_record_is_written_whole() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("lid");
        close(&with_a_copy(), &record).unwrap();
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["lid"]);
    }

    #[test]
    fn both_panels_of_a_laptop_with_two_are_disabled_and_brought_back() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("lid");
        let second = FakeMonitor::new("eDP-2", 1920, 1080, 0);
        let hypr = FakeHypr::with(vec![
            laptop(),
            FakeMonitor {
                y: 1200,
                ..second.clone()
            },
            FakeMonitor::new("NAVETTE-1", 2560, 1440, 1536),
        ]);
        close(&hypr, &record).unwrap();
        assert!(hypr.monitor("eDP-1").unwrap().disabled);
        assert!(hypr.monitor("eDP-2").unwrap().disabled);
        open(&hypr, &record).unwrap();
        assert_eq!(hypr.monitor("eDP-1").unwrap(), laptop());
        assert_eq!(
            hypr.monitor("eDP-2").unwrap(),
            FakeMonitor { y: 1200, ..second }
        );
    }

    use fake::FakeLogind;
    use std::sync::{Arc, Mutex};

    /// A watch on `logind`, what it was told kept.
    fn watching(
        logind: &Arc<FakeLogind>,
        hypr: &Arc<FakeHypr>,
        record: Option<PathBuf>,
    ) -> (Watch, Arc<Mutex<Vec<String>>>) {
        let told = Arc::new(Mutex::new(Vec::new()));
        let heard = told.clone();
        let watch = Watch::start(
            logind.clone(),
            hypr.clone(),
            record,
            Duration::from_millis(10),
            move |why| heard.lock().unwrap().push(why),
        )
        .unwrap();
        (watch, told)
    }

    fn until(done: impl Fn() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the watch never got there");
    }

    #[test]
    fn a_lid_that_cannot_be_read_is_reported_and_no_lock_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let logind = Arc::new(FakeLogind::default());
        logind.unreadable(true);
        let (watch, told) = watching(
            &logind,
            &Arc::new(with_a_copy()),
            Some(dir.path().join("lid")),
        );
        until(|| !told.lock().unwrap().is_empty());
        assert!(
            told.lock().unwrap()[0].contains("busctl"),
            "{:?}",
            told.lock().unwrap()
        );
        assert!(watch.stop().is_none());
        assert_eq!(logind.taken(), 0);
    }

    #[test]
    fn a_lid_that_stops_being_readable_lets_the_lock_go_and_the_screen_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let hypr = Arc::new(with_a_copy());
        let logind = Arc::new(FakeLogind::default());
        logind.close(true);
        let (watch, told) = watching(&logind, &hypr, Some(dir.path().join("lid")));
        until(|| hypr.monitor("eDP-1").unwrap().disabled);
        assert_eq!(logind.held(), 1);
        logind.unreadable(true);
        until(|| logind.held() == 0);
        assert!(!hypr.monitor("eDP-1").unwrap().disabled);
        assert!(told.lock().unwrap()[0].contains("times in a row"));
        assert!(watch.stop().is_none());
    }

    #[test]
    fn without_a_runtime_dir_the_lid_is_left_alone_and_said_why() {
        let logind = Arc::new(FakeLogind::default());
        let (watch, told) = watching(&logind, &Arc::new(with_a_copy()), None);
        until(|| !told.lock().unwrap().is_empty());
        assert!(told.lock().unwrap()[0].contains("XDG_RUNTIME_DIR"));
        drop(watch);
        assert_eq!((logind.taken(), logind.looked()), (0, 0));
    }

    #[test]
    fn logind_s_boolean_is_read() {
        assert!(boolean("b true\n").unwrap());
        assert!(!boolean("b false\n").unwrap());
        assert!(boolean("s \"yes\"\n").is_err());
    }
}
