//! The remote's headless output that copies a home monitor (the sequence measured in #3): the
//! monitor rule is declared first, then the output is created, so it is born in its place;
//! then Hyprland's own monitor list is checked against what was asked. Headless outputs are
//! runtime-only: a reload drops the rule (sent again), a restart drops the output (made again).

use std::io;
use std::time::Duration;

use crate::display::REMOTE_CURSOR;
use crate::display::wire::OutputCopy;
use crate::hypr::{self, Hypr, Monitor};

/// The remote output's name. One work monitor for now; several come with #10.
pub const NAME: &str = "NAVETTE-1";

/// The `hl.monitor` rule for the copy, at `x` on the remote's layout.
pub fn rule(copy: &OutputCopy, x: i32) -> String {
    format!(
        "eval hl.monitor({{ output = \"{NAME}\", mode = \"{}x{}@{}\", position = \"{x}x0\", \
         scale = {}, transform = {} }})",
        copy.width,
        copy.height,
        decimal(copy.refresh_mhz),
        decimal(copy.scale_milli),
        copy.transform,
    )
}

/// Thousandths written as Hyprland reads them: `165`, `59.94`, `1.25`.
fn decimal(milli: u32) -> String {
    let text = format!("{}.{:03}", milli / 1000, milli % 1000);
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The size a monitor takes on the layout: its mode divided by its scale, turned a quarter
/// for an odd transform.
fn logical_width(m: &Monitor) -> i32 {
    let (w, h) = (m.width as f32, m.height as f32);
    let w = if m.transform % 2 == 1 { h } else { w };
    (w / m.scale.max(0.1)).round() as i32
}

/// Where the copy goes: right of every other output, so the remote's own screen keeps its place.
fn right_edge(monitors: &[Monitor]) -> i32 {
    monitors
        .iter()
        .filter(|m| m.name != NAME)
        .map(|m| m.x + logical_width(m))
        .max()
        .unwrap_or(0)
}

/// Makes the copy and checks it; gives the scale Hyprland chose, which may not be the one asked
/// (it rounds a fractional scale so the logical size is whole). On any failure, nothing is left
/// behind.
pub fn create(hypr: &dyn Hypr, copy: &OutputCopy) -> io::Result<f32> {
    let monitors = hypr::monitors(hypr)?;
    if monitors.iter().any(|m| m.name == NAME) {
        // Left by an agent that did not stop cleanly.
        hypr::dispatch(hypr, &format!("output remove {NAME}"))?;
    }
    hypr::dispatch(hypr, &rule(copy, right_edge(&monitors)))?;
    hypr::dispatch(hypr, &format!("output create headless {NAME}"))?;
    match check(hypr, copy).and_then(|scale| hide_cursor(hypr).map(|()| scale)) {
        Ok(scale) => Ok(scale),
        Err(e) => {
            let _ = remove(hypr);
            Err(e)
        }
    }
}

/// The copy's scale if it is there as asked, so a stream can start again on it untouched.
pub fn live(hypr: &dyn Hypr, copy: &OutputCopy) -> io::Result<Option<f32>> {
    Ok(hypr::monitors(hypr)?
        .into_iter()
        .find(|m| m.name == NAME && matches(m, copy))
        .map(|m| m.scale))
}

fn matches(m: &Monitor, copy: &OutputCopy) -> bool {
    (m.width, m.height, m.transform) == (copy.width, copy.height, copy.transform as i32)
}

/// Hyprland applies a mode asynchronously: the copy is checked for half a second.
fn check(hypr: &dyn Hypr, copy: &OutputCopy) -> io::Result<f32> {
    let mut seen = None;
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        let monitors = hypr::monitors(hypr)?;
        seen = monitors.into_iter().find(|m| m.name == NAME);
        if let Some(m) = &seen
            && matches(m, copy)
        {
            return Ok(m.scale);
        }
    }
    Err(io::Error::other(match seen {
        Some(m) => format!(
            "the remote output is {}x{} transform {}, not the {}x{} transform {} asked",
            m.width, m.height, m.transform, copy.width, copy.height, copy.transform
        ),
        None => "the remote output did not appear".to_string(),
    }))
}

/// Declares the rule again, and hides the cursor again, after a `hyprctl reload` dropped both.
pub fn reapply(hypr: &dyn Hypr, copy: &OutputCopy) -> io::Result<()> {
    let monitors = hypr::monitors(hypr)?;
    hypr::dispatch(hypr, &rule(copy, right_edge(&monitors)))?;
    hide_cursor(hypr)
}

/// Shows the cursor and removes the copy; one already gone (Hyprland restarted) is not an
/// error. The cursor is shown even then: an agent that crashed may have left it hidden.
pub fn remove(hypr: &dyn Hypr) -> io::Result<()> {
    let shown = cursor_invisible(hypr, false);
    match hypr::dispatch(hypr, &format!("output remove {NAME}")) {
        Err(e) if e.to_string().contains("output not found") => Ok(()),
        other => other,
    }?;
    shown
}

/// Hyprland paints its cursor into the copy's frames (see [`REMOTE_CURSOR`]): hidden while the
/// copy is there.
fn hide_cursor(hypr: &dyn Hypr) -> io::Result<()> {
    if REMOTE_CURSOR {
        return Ok(());
    }
    cursor_invisible(hypr, true)
}

fn cursor_invisible(hypr: &dyn Hypr, invisible: bool) -> io::Result<()> {
    hypr::dispatch(
        hypr,
        &format!("eval hl.config({{ cursor = {{ invisible = {invisible} }} }})"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypr::fake::{FakeHypr, FakeMonitor};

    fn copy() -> OutputCopy {
        OutputCopy {
            width: 2560,
            height: 1440,
            refresh_mhz: 59_940,
            scale_milli: 1250,
            transform: 1,
        }
    }

    #[test]
    fn the_rule_is_declared_before_the_output_is_created() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        create(&hypr, &copy()).unwrap();
        assert_eq!(
            hypr.changes(),
            [
                "eval hl.monitor({ output = \"NAVETTE-1\", mode = \"2560x1440@59.94\", \
                 position = \"1920x0\", scale = 1.25, transform = 1 })",
                "output create headless NAVETTE-1",
                "eval hl.config({ cursor = { invisible = true } })",
            ]
        );
        let made = hypr.monitor(NAME).unwrap();
        assert_eq!((made.width, made.height, made.transform), (2560, 1440, 1));
    }

    #[test]
    fn the_copy_goes_right_of_a_rotated_and_scaled_screen() {
        let mut portrait = FakeMonitor::new("eDP-1", 2400, 1600, 100);
        portrait.transform = 3;
        portrait.scale = 2.0;
        let hypr = FakeHypr::with(vec![portrait]);
        create(&hypr, &copy()).unwrap();
        // 1600 wide once turned, 800 at scale 2, from x = 100.
        assert!(hypr.changes()[0].contains("position = \"900x0\""));
    }

    #[test]
    fn a_leftover_copy_is_removed_first() {
        let hypr = FakeHypr::with(vec![
            FakeMonitor::new("eDP-1", 1920, 1080, 0),
            FakeMonitor::new(NAME, 1280, 720, 1920),
        ]);
        create(&hypr, &copy()).unwrap();
        assert_eq!(hypr.changes()[0], "output remove NAVETTE-1");
        assert_eq!(hypr.monitor(NAME).unwrap().width, 2560);
    }

    #[test]
    fn the_scale_is_the_one_hyprland_chose() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        hypr.choose_scale(1.2);
        assert_eq!(create(&hypr, &copy()).unwrap(), 1.2);
        assert_eq!(live(&hypr, &copy()).unwrap(), Some(1.2));
    }

    #[test]
    fn a_copy_of_another_size_is_not_live() {
        let hypr = FakeHypr::with(vec![
            FakeMonitor::new("eDP-1", 1920, 1080, 0),
            FakeMonitor::new(NAME, 1280, 720, 1920),
        ]);
        assert_eq!(live(&hypr, &copy()).unwrap(), None);
        assert_eq!(live(&FakeHypr::default(), &copy()).unwrap(), None);
    }

    #[test]
    fn removing_a_copy_already_gone_is_fine() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1080, 0)]);
        remove(&hypr).unwrap();
    }

    #[test]
    fn the_remote_cursor_is_hidden_while_the_copy_is_there() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        create(&hypr, &copy()).unwrap();
        assert!(hypr.cursor_invisible());
        remove(&hypr).unwrap();
        assert!(!hypr.cursor_invisible());
    }

    #[test]
    fn a_reload_hides_the_remote_cursor_again() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        create(&hypr, &copy()).unwrap();
        hypr.reload();
        reapply(&hypr, &copy()).unwrap();
        assert!(hypr.cursor_invisible());
    }

    #[test]
    fn the_cursor_comes_back_even_when_the_copy_is_already_gone() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1080, 0)]);
        hypr.request("eval hl.config({ cursor = { invisible = true } })")
            .unwrap();
        remove(&hypr).unwrap();
        assert!(!hypr.cursor_invisible());
    }

    #[test]
    fn whole_and_fractional_values_are_written_plainly() {
        assert_eq!(decimal(165_000), "165");
        assert_eq!(decimal(59_940), "59.94");
        assert_eq!(decimal(1_000), "1");
        assert_eq!(decimal(1_250), "1.25");
    }
}
