//! `navette`: the work-from-home mode. From the home desk (Hyprland, several monitors), it
//! reserves monitors for the work laptop and shows its desktop on them fullscreen, with a mic
//! toggle. `navette` runs at home; `navette agent` runs on the laptop. Nothing is implemented
//! yet: see the issues.

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("navette: nothing is implemented yet (see the issues)");
    ExitCode::from(2)
}
