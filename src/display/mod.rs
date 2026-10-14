//! The picture path for a mode: on the remote, a headless output copying a home monitor,
//! captured, encoded and streamed; at home, decoded and shown on that monitor.

pub mod decode;
pub mod output;
// gliff's copy, kept as is: it was written for a binary crate, where this lint does not fire.
#[allow(clippy::new_without_default)]
pub mod rate;
pub mod remote;
pub mod stream;
pub mod surface;
pub mod wire;

/// Whether the remote cursor's image and visibility reach home's pointer: off. A headless
/// output has no cursor plane (aquamarine's headless backend), so the remote's Hyprland draws
/// its cursor into every frame, and the capture copies it whatever it asks: a second pointer,
/// lagging behind home's. So while a mode runs the remote hides its cursor (`cursor:invisible`)
/// whenever it is over a copy, which also blanks the image its cursor capture gives, and home
/// shows its own default arrow.
/// Turned on, the remote leaves its cursor alone and home draws the remote's image again (#7).
pub const REMOTE_CURSOR: bool = false;

/// Where a part of a running mode says why it cannot go on (the surface went away, home cannot
/// decode): the mode then ends.
pub type Ended = tokio::sync::mpsc::UnboundedSender<String>;

/// Where the keyboard goes, as the surface tells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyboard {
    /// The release hotkey gave it back: home then gives it to the home window that had it last.
    Home,
    /// A surface took it again, the pointer having come back after a release.
    Remote,
}

/// Where the surface says the keyboard went.
pub type Released = tokio::sync::mpsc::UnboundedSender<Keyboard>;
