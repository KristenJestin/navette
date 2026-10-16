//! The picture path for a mode: on the remote, a headless output copying a desk monitor,
//! captured, encoded and streamed; at the desk, decoded and shown on that monitor.

pub mod corner;
pub mod decode;
pub mod output;
// gliff's copy, kept as is: it was written for a binary crate, where this lint does not fire.
#[allow(clippy::new_without_default)]
pub mod rate;
pub mod remote;
pub mod stream;
pub mod surface;
pub mod wire;

/// Whether the remote cursor's image and visibility reach the desk's pointer: off. A headless
/// output has no cursor plane (aquamarine's headless backend), so the remote's Hyprland draws
/// its cursor into every frame, and the capture copies it whatever it asks: a second pointer,
/// lagging behind the desk's. So while a mode runs the remote hides its cursor (`cursor:invisible`)
/// whenever it is over a copy, which also blanks the image its cursor capture gives, and the desk
/// shows its own default arrow.
/// Turned on, the remote leaves its cursor alone and the desk draws the remote's image again (#7).
pub const REMOTE_CURSOR: bool = false;

/// Where a part of a running mode says why it cannot go on (the surface went away, the desk cannot
/// decode): the mode then ends.
pub type Ended = tokio::sync::mpsc::UnboundedSender<String>;

/// Where the keyboard goes, as the surface tells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyboard {
    /// The release hotkey gave it back: the desk then gives it to the desk window that had it last.
    Home,
    /// A surface took it again, the pointer having come back after a release.
    Remote,
    /// The pointer came over a reserved monitor, the keyboard with it: the corner says nothing,
    /// the mic may follow.
    Came,
    /// The pointer left for a desk monitor, the keyboard with it.
    Left,
}

impl Keyboard {
    /// Whether the keyboard is the remote's now.
    pub fn remote(self) -> bool {
        matches!(self, Keyboard::Remote | Keyboard::Came)
    }
}

/// Where the surface says the keyboard went.
pub type Released = tokio::sync::mpsc::UnboundedSender<Keyboard>;
