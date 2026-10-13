//! The remote's headless outputs that copy home monitors (the sequence measured in #3): each
//! monitor rule is declared first, then its output is created, so it is born in its place;
//! then Hyprland's own monitor list is checked against what was asked. Headless outputs are
//! runtime-only: a reload drops the rules (sent again), a restart drops the outputs (made again).
//!
//! The copies sit right of the remote's own monitors, side by side in home's left-to-right
//! order: each one's place follows from the copies before it, never from which was made first.

use std::io;
use std::time::Duration;

use crate::display::REMOTE_CURSOR;
use crate::display::wire::OutputCopy;
use crate::hypr::{self, Hypr, Monitor};

/// The name of output `index`: `NAVETTE-1` for the leftmost.
pub fn name(index: usize) -> String {
    format!("NAVETTE-{}", index + 1)
}

/// A copy made by Navette, whichever: `NAVETTE-<n>`.
pub fn is_copy(name: &str) -> bool {
    name.strip_prefix("NAVETTE-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The `hl.monitor` rule for copy `index`, at `x` on the remote's layout.
pub fn rule(index: usize, copy: &OutputCopy, x: i32) -> String {
    format!(
        "eval hl.monitor({{ output = \"{}\", mode = \"{}x{}@{}\", position = \"{x}x0\", \
         scale = {}, transform = {} }})",
        name(index),
        copy.width,
        copy.height,
        decimal(copy.refresh_mhz),
        decimal(copy.scale_milli),
        copy.transform,
    )
}

/// Thousandths written as Hyprland reads them: `165`, `59.94`, `1.25`.
pub fn decimal(milli: u32) -> String {
    let text = format!("{}.{:03}", milli / 1000, milli % 1000);
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The width a monitor takes on the layout: its mode divided by its scale, turned a quarter
/// for an odd transform.
fn logical_width(width: u32, height: u32, transform: i32, scale: f32) -> i32 {
    let w = if transform % 2 == 1 { height } else { width };
    (w as f32 / scale.max(0.1)).round() as i32
}

/// Where copy `index` goes: right of every output that is not a copy, so the remote's own
/// screens keep their place, after the copies left of it, each at the scale Hyprland chose for
/// it when it is known (`scales`, by index), else at the one asked.
fn place(monitors: &[Monitor], copies: &[OutputCopy], scales: &[Option<f32>], index: usize) -> i32 {
    let edge = monitors
        .iter()
        .filter(|m| !is_copy(&m.name))
        .map(|m| m.x + logical_width(m.width, m.height, m.transform, m.scale))
        .max()
        .unwrap_or(0);
    let before: i32 = copies[..index]
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let asked = c.scale_milli as f32 / 1000.0;
            let scale = scales.get(i).copied().flatten().unwrap_or(asked);
            logical_width(c.width, c.height, i32::from(c.transform), scale)
        })
        .sum();
    edge + before
}

/// Makes copy `index` of `copies` and checks it; gives the scale Hyprland chose, which may not
/// be the one asked (it rounds a fractional scale so the logical size is whole). On any
/// failure, nothing of it is left behind.
pub fn create(
    hypr: &dyn Hypr,
    copies: &[OutputCopy],
    scales: &[Option<f32>],
    index: usize,
) -> io::Result<f32> {
    let (name, copy) = (name(index), &copies[index]);
    let monitors = hypr::monitors(hypr)?;
    if monitors.iter().any(|m| m.name == name) {
        // Left by an agent that did not stop cleanly, or of another size.
        hypr::dispatch(hypr, &format!("output remove {name}"))?;
    }
    hypr::dispatch(
        hypr,
        &rule(index, copy, place(&monitors, copies, scales, index)),
    )?;
    hypr::dispatch(hypr, &format!("output create headless {name}"))?;
    match check(hypr, &name, copy).and_then(|scale| hide_cursor(hypr).map(|()| scale)) {
        Ok(scale) => Ok(scale),
        Err(e) => {
            let _ = remove(hypr, index);
            Err(e)
        }
    }
}

/// Copy `index`'s scale if it is there as asked, so a stream can start again on it untouched.
pub fn live(hypr: &dyn Hypr, copy: &OutputCopy, index: usize) -> io::Result<Option<f32>> {
    let name = name(index);
    Ok(hypr::monitors(hypr)?
        .into_iter()
        .find(|m| m.name == name && matches(m, copy))
        .map(|m| m.scale))
}

fn matches(m: &Monitor, copy: &OutputCopy) -> bool {
    (m.width, m.height, m.transform) == (copy.width, copy.height, copy.transform as i32)
}

/// Hyprland applies a mode asynchronously: the copy is checked for half a second.
fn check(hypr: &dyn Hypr, name: &str, copy: &OutputCopy) -> io::Result<f32> {
    let mut seen = None;
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        let monitors = hypr::monitors(hypr)?;
        seen = monitors.into_iter().find(|m| m.name == name);
        if let Some(m) = &seen
            && matches(m, copy)
        {
            return Ok(m.scale);
        }
    }
    Err(io::Error::other(match seen {
        Some(m) => format!(
            "{name} is {}x{} transform {}, not the {}x{} transform {} asked",
            m.width, m.height, m.transform, copy.width, copy.height, copy.transform
        ),
        None => format!("{name} did not appear"),
    }))
}

/// Declares every rule again, and hides the cursor again, after a `hyprctl reload` dropped
/// them.
pub fn reapply(hypr: &dyn Hypr, copies: &[OutputCopy], scales: &[Option<f32>]) -> io::Result<()> {
    let monitors = hypr::monitors(hypr)?;
    for (index, copy) in copies.iter().enumerate() {
        hypr::dispatch(
            hypr,
            &rule(index, copy, place(&monitors, copies, scales, index)),
        )?;
    }
    hide_cursor(hypr)
}

/// Removes copy `index`, the others staying; one already gone (Hyprland restarted) is not an
/// error. The cursor stays hidden: the mode goes on.
pub fn remove(hypr: &dyn Hypr, index: usize) -> io::Result<()> {
    match hypr::dispatch(hypr, &format!("output remove {}", name(index))) {
        Err(e) if e.to_string().contains("output not found") => Ok(()),
        other => other,
    }
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

/// Shows the cursor and removes every copy there is: the mode's, or those an agent that
/// crashed left, however many. The cursor is shown even with no copy left: an agent that
/// crashed may have left it hidden. One copy that will not go does not keep the others.
pub fn remove_all(hypr: &dyn Hypr) -> io::Result<()> {
    let mut result = cursor_invisible(hypr, false);
    for m in hypr::monitors(hypr)? {
        if is_copy(&m.name)
            && let Err(e) = hypr::dispatch(hypr, &format!("output remove {}", m.name))
        {
            result = result.and(Err(e));
        }
    }
    result
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
        create(&hypr, &[copy()], &[], 0).unwrap();
        assert_eq!(
            hypr.changes(),
            [
                "eval hl.monitor({ output = \"NAVETTE-1\", mode = \"2560x1440@59.94\", \
                 position = \"1920x0\", scale = 1.25, transform = 1 })",
                "output create headless NAVETTE-1",
                "eval hl.config({ cursor = { invisible = true } })",
            ]
        );
        let made = hypr.monitor("NAVETTE-1").unwrap();
        assert_eq!((made.width, made.height, made.transform), (2560, 1440, 1));
    }

    #[test]
    fn the_copy_goes_right_of_a_rotated_and_scaled_screen() {
        let mut portrait = FakeMonitor::new("eDP-1", 2400, 1600, 100);
        portrait.transform = 3;
        portrait.scale = 2.0;
        let hypr = FakeHypr::with(vec![portrait]);
        create(&hypr, &[copy()], &[], 0).unwrap();
        // 1600 wide once turned, 800 at scale 2, from x = 100.
        assert!(hypr.changes()[0].contains("position = \"900x0\""));
    }

    #[test]
    fn a_leftover_copy_is_removed_first() {
        let hypr = FakeHypr::with(vec![
            FakeMonitor::new("eDP-1", 1920, 1080, 0),
            FakeMonitor::new("NAVETTE-1", 1280, 720, 1920),
        ]);
        create(&hypr, &[copy()], &[], 0).unwrap();
        assert_eq!(hypr.changes()[0], "output remove NAVETTE-1");
        assert_eq!(hypr.monitor("NAVETTE-1").unwrap().width, 2560);
    }

    #[test]
    fn the_scale_is_the_one_hyprland_chose() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        hypr.choose_scale(1.2);
        assert_eq!(create(&hypr, &[copy()], &[], 0).unwrap(), 1.2);
        assert_eq!(live(&hypr, &copy(), 0).unwrap(), Some(1.2));
    }

    #[test]
    fn a_copy_of_another_size_is_not_live() {
        let hypr = FakeHypr::with(vec![
            FakeMonitor::new("eDP-1", 1920, 1080, 0),
            FakeMonitor::new("NAVETTE-1", 1280, 720, 1920),
        ]);
        assert_eq!(live(&hypr, &copy(), 0).unwrap(), None);
        assert_eq!(live(&FakeHypr::default(), &copy(), 0).unwrap(), None);
    }

    #[test]
    fn removing_a_copy_already_gone_is_fine() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1080, 0)]);
        remove(&hypr, 0).unwrap();
    }

    #[test]
    fn the_remote_cursor_is_hidden_while_the_copy_is_there() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        let copies = [copy(), copy()];
        create(&hypr, &copies, &[], 0).unwrap();
        create(&hypr, &copies, &[], 1).unwrap();
        assert!(hypr.cursor_invisible());
        // One copy made again: the other is still there, the cursor stays hidden.
        remove(&hypr, 1).unwrap();
        assert!(hypr.cursor_invisible());
        remove_all(&hypr).unwrap();
        assert!(!hypr.cursor_invisible());
    }

    #[test]
    fn a_reload_hides_the_remote_cursor_again() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        create(&hypr, &[copy()], &[], 0).unwrap();
        hypr.reload();
        reapply(&hypr, &[copy()], &[]).unwrap();
        assert!(hypr.cursor_invisible());
    }

    #[test]
    fn the_cursor_comes_back_even_when_the_copy_is_already_gone() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1080, 0)]);
        hypr.request("eval hl.config({ cursor = { invisible = true } })")
            .unwrap();
        remove_all(&hypr).unwrap();
        assert!(!hypr.cursor_invisible());
    }

    #[test]
    fn whole_and_fractional_values_are_written_plainly() {
        assert_eq!(decimal(165_000), "165");
        assert_eq!(decimal(59_940), "59.94");
        assert_eq!(decimal(1_000), "1");
        assert_eq!(decimal(1_250), "1.25");
    }

    fn landscape(width: u32, height: u32, scale_milli: u32) -> OutputCopy {
        OutputCopy {
            width,
            height,
            refresh_mhz: 60_000,
            scale_milli,
            transform: 0,
        }
    }

    /// Home's desk of the issue, its first monitor kept home: a portrait one between two
    /// landscape ones given to the remote.
    fn three() -> Vec<OutputCopy> {
        let mut portrait = landscape(2560, 1440, 1000);
        portrait.transform = 1;
        vec![
            landscape(1920, 1080, 1000),
            portrait,
            landscape(2560, 1440, 1250),
        ]
    }

    #[test]
    fn the_copies_sit_side_by_side_in_home_s_order_whichever_is_made_first() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        let copies = three();
        for index in [2, 0, 1] {
            create(&hypr, &copies, &[], index).unwrap();
        }
        let placed: Vec<_> = (0..3)
            .map(|i| {
                let m = hypr.monitor(&name(i)).unwrap();
                (m.x, m.width, m.height, m.transform)
            })
            .collect();
        // The portrait copy is 1440 wide once turned; the last starts right of it.
        assert_eq!(
            placed,
            [
                (1920, 1920, 1080, 0),
                (3840, 2560, 1440, 1),
                (5280, 2560, 1440, 0)
            ]
        );
    }

    #[test]
    fn a_copy_made_again_takes_its_place_back_between_the_others() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        let copies = three();
        for index in 0..3 {
            create(&hypr, &copies, &[], index).unwrap();
        }
        remove(&hypr, 1).unwrap();
        create(&hypr, &copies, &[], 1).unwrap();
        assert_eq!(hypr.monitor("NAVETTE-2").unwrap().x, 3840);
    }

    #[test]
    fn a_reload_gets_every_copy_s_rule_back_in_its_place() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        let copies = three();
        for index in 0..3 {
            create(&hypr, &copies, &[], index).unwrap();
        }
        hypr.reload();
        reapply(&hypr, &copies, &[]).unwrap();
        let rules: Vec<_> = hypr
            .changes()
            .into_iter()
            .rev()
            .filter(|c| c.contains("hl.monitor"))
            .take(3)
            .collect();
        assert!(rules[0].contains("NAVETTE-3") && rules[0].contains("\"5280x0\""));
        assert!(rules[2].contains("NAVETTE-1") && rules[2].contains("\"1920x0\""));
        assert_eq!(hypr.monitor("NAVETTE-3").unwrap().scale, 1.25);
    }

    #[test]
    fn every_copy_left_by_an_agent_that_crashed_is_removed() {
        let hypr = FakeHypr::with(vec![
            FakeMonitor::new("eDP-1", 1920, 1200, 0),
            FakeMonitor::new("NAVETTE-1", 1920, 1080, 1920),
            FakeMonitor::new("NAVETTE-3", 1920, 1080, 3840),
            FakeMonitor::new("NAVETTE-OTHER", 1920, 1080, 5760),
        ]);
        remove_all(&hypr).unwrap();
        let left: Vec<_> = ["eDP-1", "NAVETTE-1", "NAVETTE-3", "NAVETTE-OTHER"]
            .into_iter()
            .filter(|n| hypr.monitor(n).is_some())
            .collect();
        assert_eq!(left, ["eDP-1", "NAVETTE-OTHER"]);
    }

    #[test]
    fn a_copy_goes_right_of_the_size_hyprland_chose_for_the_one_before() {
        let hypr = FakeHypr::with(vec![FakeMonitor::new("eDP-1", 1920, 1200, 0)]);
        let copies = [landscape(2560, 1440, 1250), landscape(1920, 1080, 1000)];
        hypr.choose_scale(1.2);
        let first = create(&hypr, &copies, &[], 0).unwrap();
        // 2560 at 1.2 is 2133 wide, not the 2048 of the 1.25 asked.
        create(&hypr, &copies, &[Some(first)], 1).unwrap();
        assert_eq!(hypr.monitor("NAVETTE-2").unwrap().x, 1920 + 2133);
        hypr.reload();
        reapply(&hypr, &copies, &[Some(first), Some(1.2)]).unwrap();
        let second = hypr
            .changes()
            .into_iter()
            .rfind(|c| c.contains("NAVETTE-2"))
            .unwrap();
        assert!(second.contains("\"4053x0\""), "{second}");
    }
}
