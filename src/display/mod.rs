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

/// Where a part of a running mode says why it cannot go on (the surface went away, home cannot
/// decode): the mode then ends.
pub type Ended = tokio::sync::mpsc::UnboundedSender<String>;
