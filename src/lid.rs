//! The remote laptop's lid while a mode runs, when home's `mode.conf` says `closed_lid = on`.
//! Closed, the laptop's own screen is disabled at runtime and Hyprland moves its workspaces to
//! Navette's outputs; opened, it comes back with the mode it had. The lid does not suspend the
//! laptop meanwhile: the agent holds logind's `handle-lid-switch` inhibitor lock for the mode's
//! duration, which needs no root. When logind or the machine's policy refuses the lock, the
//! agent says so and leaves the lid alone: the laptop's settings are never worked around.
//!
//! The rule that brings the screen back is written to a file before the screen is disabled,
//! and removed once it is back: an agent that crashed puts it back when it starts again.

use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::display::output;
use crate::hypr::{self, Hypr, Monitor, lua_string};

/// logind, as the agent uses it (the port behind which tests put a fake).
pub trait Logind: Send + Sync {
    /// Whether the lid is closed now: logind's `LidClosed`.
    fn lid_closed(&self) -> io::Result<bool>;
    /// Takes the `handle-lid-switch` inhibitor lock, held until the guard is dropped.
    fn inhibit_lid(&self) -> io::Result<Box<dyn Send + Sync>>;
}

/// The laptop's own screen goes by its connector's name, whatever the order of arrival.
const INTERNAL: [&str; 3] = ["eDP-", "LVDS-", "DSI-"];

fn internal(monitors: &[Monitor]) -> Option<&Monitor> {
    monitors
        .iter()
        .find(|m| INTERNAL.iter().any(|prefix| m.name.starts_with(prefix)))
}

/// Where the rule that brings the screen back is kept: the runtime directory, which lives as
/// long as the session's Hyprland and its runtime rules do.
pub fn record_path() -> io::Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime).join("navette").join("lid"))
}

/// Disables the laptop's screen if it is on and one of Navette's outputs is there to take its
/// workspaces, after writing to `record` the rule that brings it back.
pub fn close(hypr: &dyn Hypr, record: &Path) -> io::Result<()> {
    let monitors = hypr::monitors(hypr)?;
    let Some(screen) = internal(&monitors).filter(|m| !m.disabled) else {
        return Ok(());
    };
    // Disabled with no other output on, the laptop would have nowhere to put its windows.
    if !monitors
        .iter()
        .any(|m| output::is_copy(&m.name) && !m.disabled)
    {
        return Ok(());
    }
    write_private(record, &rule(screen))?;
    let disabled = hypr::dispatch(
        hypr,
        &format!(
            "eval hl.monitor({{ output = {}, disabled = true }})",
            lua_string(&screen.name)
        ),
    );
    if disabled.is_err() {
        let _ = std::fs::remove_file(record);
    }
    disabled
}

/// The rule that puts `screen` back as it is now. A rule merges into the one before it, so
/// it has to say `disabled = false` (measured in #36).
fn rule(screen: &Monitor) -> String {
    format!(
        "eval hl.monitor({{ output = {}, mode = \"{}x{}@{}\", position = \"{}x{}\", \
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

/// Writes `text` to `path`, in a directory only this user can enter.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = path.parent().expect("the record sits in a directory");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(path, text)
}

/// Puts the screen back as `record` says, if the agent disabled it, and forgets the record.
pub fn open(hypr: &dyn Hypr, record: &Path) -> io::Result<()> {
    let rule = match std::fs::read_to_string(record) {
        Ok(rule) => rule,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    // Kept when Hyprland cannot take it: the next start tries again.
    hypr::dispatch(hypr, &rule)?;
    std::fs::remove_file(record)
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
            .output()?;
        if !reply.status.success() {
            return Err(io::Error::other(format!(
                "logind's LidClosed: {}",
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
            format!("logind's LidClosed is `{other}`, not a boolean"),
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
            .spawn()?;
        // The command runs once the lock is held, so its word is the proof.
        let mut said = String::new();
        if let Some(stdout) = child.stdout.take() {
            BufReader::new(stdout).read_line(&mut said)?;
        }
        if said.trim() == "held" {
            return Ok(Self(child));
        }
        drop(child.stdin.take());
        let mut why = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut why);
        }
        let status = child.wait()?;
        Err(io::Error::other(format!(
            "logind refused the lid-switch inhibitor ({status}): {}",
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
    }

    impl Logind for FakeLogind {
        fn lid_closed(&self) -> io::Result<bool> {
            self.looked.fetch_add(1, Ordering::SeqCst);
            Ok(self.closed.load(Ordering::SeqCst))
        }

        fn inhibit_lid(&self) -> io::Result<Box<dyn Send + Sync>> {
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
    fn logind_s_boolean_is_read() {
        assert!(boolean("b true\n").unwrap());
        assert!(!boolean("b false\n").unwrap());
        assert!(boolean("s \"yes\"\n").is_err());
    }
}
