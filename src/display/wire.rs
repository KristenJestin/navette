//! The display's messages on the link: its orders and answers, gliff's stream messages and
//! their feedback. The link (`crate::link`) carries them as top-level `LinkMsg` variants, a
//! video frame's bytes going straight from the encoder to the socket and from the socket to the
//! decoder.
//!
//! The write queue is adapted from gliff-server's `writer.rs` (gliff, MIT, see
//! `LICENSES/gliff-MIT.txt`): a video frame waits alone, and a newer cursor or stream message
//! replaces an older one still queued, so the queue stays short however slow the link.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use gliff_proto::{ClientCaps, ClientMsg, ServerMsg};
use minicbor::{Decode, Encode};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Notify, watch};

/// The largest cursor taken, as gliff's client, in pixels a side: a cursor image declaring
/// more ends the link.
pub const MAX_CURSOR: u32 = 1024;

/// The remote cursor's image: little-endian ARGB8888 in the output's pixels, its hotspot in
/// them too.
#[derive(Clone, Debug, PartialEq)]
pub struct Cursor {
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub argb: Bytes,
}

/// The remote pointer as last heard. Only the newest counts, so it never waits behind the
/// pictures, nor is it dropped with them when the display falls behind.
#[derive(Clone, Debug, PartialEq)]
pub struct Pointer {
    /// `None` until the remote sends an image.
    pub shape: Option<Cursor>,
    pub visible: bool,
}

impl Default for Pointer {
    fn default() -> Self {
        Self {
            shape: None,
            visible: true,
        }
    }
}

/// What the display sends.
#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Display(Display),
    /// Remote to home: gliff's stream messages (config, frames, cursor, pings).
    Video(ServerMsg),
    /// Home to remote: gliff's answers to them (frame acks, keyframe requests, pongs).
    Feedback(ClientMsg),
    /// Remote to home: the picture goes on, but home's input cannot be injected (the reason).
    NoInput(String),
}

/// The display's orders and their answers.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Display {
    /// Home to remote: make a headless output that copies this home monitor, and stream it.
    #[n(0)]
    Start {
        #[n(0)]
        output: OutputCopy,
        /// What home decodes.
        #[n(1)]
        caps: ClientCaps,
    },
    /// Home to remote: stop streaming and remove the output.
    #[n(1)]
    Stop,
    /// Remote to home: the output exists, its stream follows.
    #[n(2)]
    Started {
        #[n(0)]
        output: String,
    },
    /// Remote to home: the output could not be made; nothing was left behind.
    #[n(3)]
    Failed {
        #[n(0)]
        reason: String,
    },
}

/// The home monitor a remote output copies: its mode, scale and orientation.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct OutputCopy {
    /// The mode, unrotated, as Hyprland reports it.
    #[n(0)]
    pub width: u32,
    #[n(1)]
    pub height: u32,
    /// Refresh rate in millihertz.
    #[n(2)]
    pub refresh_mhz: u32,
    #[n(3)]
    pub scale_milli: u32,
    /// `wl_output` transform, 0 to 7.
    #[n(4)]
    pub transform: u8,
}

/// A message received, with the payload bytes it declared.
#[derive(Debug)]
pub enum Incoming {
    Display(Display),
    /// A gliff stream message; `main` and `aux` hold a video frame's two streams.
    Video {
        msg: ServerMsg,
        main: Bytes,
        aux: Bytes,
    },
    Feedback(ClientMsg),
    NoInput(String),
}

/// One live connection, as the display and its input see it. `inbox` ends when the link drops.
pub struct Peer {
    pub outbox: Outbox,
    pub inbox: mpsc::Receiver<Incoming>,
    /// Remote to home; it ends with the inbox.
    pub pointer: watch::Receiver<Pointer>,
    pub input: crate::input::Wire,
}

#[derive(Default)]
struct Queue {
    packets: VecDeque<(Msg, Vec<Vec<u8>>)>,
    closed: bool,
}

fn is_video(msg: &Msg) -> bool {
    matches!(msg, Msg::Video(ServerMsg::VideoFrame { .. }))
}

impl Queue {
    fn video_ready(&self) -> bool {
        !self.packets.iter().any(|(msg, _)| is_video(msg))
    }

    fn push(&mut self, msg: Msg, payloads: Vec<Vec<u8>>) {
        match &msg {
            Msg::Video(ServerMsg::StreamConfig { .. }) => {
                // A config queued after the last frame is superseded; one before it is not.
                let after_video = self
                    .packets
                    .iter()
                    .rposition(|(msg, _)| is_video(msg))
                    .map_or(0, |i| i + 1);
                let mut index = 0;
                self.packets.retain(|(msg, _)| {
                    let keep = index < after_video
                        || !matches!(msg, Msg::Video(ServerMsg::StreamConfig { .. }));
                    index += 1;
                    keep
                });
            }
            Msg::Video(ServerMsg::CursorShape { .. }) => self.packets.retain(|(msg, _)| {
                !matches!(
                    msg,
                    Msg::Video(ServerMsg::CursorShape { .. } | ServerMsg::CursorPos { .. })
                )
            }),
            Msg::Video(ServerMsg::CursorPos { .. } | ServerMsg::Pong { .. })
            | Msg::Feedback(ClientMsg::Pong { .. }) => {
                let same = |queued: &Msg| match (queued, &msg) {
                    (Msg::Video(a), Msg::Video(b)) => {
                        std::mem::discriminant(a) == std::mem::discriminant(b)
                    }
                    (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b),
                };
                self.packets.retain(|(queued, _)| !same(queued));
            }
            _ => {}
        }
        self.packets.push_back((msg, payloads));
    }
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    notify: Notify,
    /// How long each write took, for whoever paces a video stream.
    reports: Mutex<Option<UnboundedSender<Duration>>>,
}

/// The sending side of a link: `send` only queues, so a caller never waits on the network.
#[derive(Clone, Default)]
pub struct Outbox(Arc<Shared>);

impl Outbox {
    /// Queues a message and the payloads it declares. Dropped once the link is down.
    pub fn send(&self, msg: Msg, payloads: Vec<Vec<u8>>) {
        let mut queue = self.0.queue.lock().unwrap();
        if queue.closed {
            return;
        }
        queue.push(msg, payloads);
        drop(queue);
        self.0.notify.notify_one();
    }

    /// No video frame waits to be written: the encoder works one frame ahead at most.
    pub fn video_ready(&self) -> bool {
        self.0.queue.lock().unwrap().video_ready()
    }

    pub fn is_closed(&self) -> bool {
        self.0.queue.lock().unwrap().closed
    }

    /// The duration of every write from now on, for the rate controller.
    pub fn write_reports(&self) -> UnboundedReceiver<Duration> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.0.reports.lock().unwrap() = Some(tx);
        rx
    }

    /// What waits to be written, oldest first.
    #[cfg(test)]
    pub fn queued(&self) -> Vec<Msg> {
        let queue = self.0.queue.lock().unwrap();
        queue.packets.iter().map(|(msg, _)| msg.clone()).collect()
    }

    /// The link is down: nothing more is queued, and nothing waits.
    pub(crate) fn close(&self) {
        let mut queue = self.0.queue.lock().unwrap();
        queue.closed = true;
        queue.packets.clear();
        drop(queue);
        *self.0.reports.lock().unwrap() = None;
    }

    /// The next message to write, oldest first.
    pub(crate) async fn next(&self) -> (Msg, Vec<Vec<u8>>) {
        loop {
            if let Some(packet) = self.0.queue.lock().unwrap().packets.pop_front() {
                return packet;
            }
            self.0.notify.notified().await;
        }
    }

    /// How long a write took, for the rate controller.
    pub(crate) fn report_write(&self, took: Duration) {
        if let Some(reports) = self.0.reports.lock().unwrap().as_ref() {
            let _ = reports.send(took);
        }
    }

    /// Home could not take a frame (the display fell behind): it is acked all the same, the
    /// remote pacing on acks, and a keyframe is asked for, the next frames depending on it.
    pub(crate) fn refuse_frame(&self, msg: &ServerMsg) {
        if let ServerMsg::VideoFrame { frame_id, .. } = msg {
            let decoded_at_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            self.send(
                Msg::Feedback(ClientMsg::FrameAck {
                    frame_id: *frame_id,
                    decoded_at_ms,
                }),
                Vec::new(),
            );
            self.send(Msg::Feedback(ClientMsg::RequestKeyframe), Vec::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u64, len: usize) -> (Msg, Vec<Vec<u8>>) {
        (
            Msg::Video(ServerMsg::VideoFrame {
                frame_id: id,
                pts_us: 0,
                keyframe: id == 0,
                damage: Vec::new(),
                data_len: len as u32,
                aux_len: 3,
            }),
            vec![vec![7; len], vec![9; 3]],
        )
    }

    #[test]
    fn the_queue_holds_one_frame_and_the_newest_cursor() {
        let mut queue = Queue::default();
        let (msg, payloads) = frame(1, 4);
        queue.push(msg, payloads);
        assert!(!queue.video_ready());
        for id in 0..100 {
            queue.push(
                Msg::Video(ServerMsg::CursorPos {
                    x: id as f64,
                    y: 0.0,
                    shape_id: 0,
                    visible: true,
                }),
                Vec::new(),
            );
            queue.push(Msg::Feedback(ClientMsg::Pong { t: id }), Vec::new());
        }
        assert_eq!(queue.packets.len(), 3);
        assert!(matches!(
            queue.packets[1].0,
            Msg::Video(ServerMsg::CursorPos { x: 99.0, .. })
        ));
    }

    #[test]
    fn a_closed_outbox_drops_what_is_sent() {
        let outbox = Outbox::default();
        outbox.close();
        outbox.send(Msg::Feedback(ClientMsg::RequestKeyframe), Vec::new());
        assert!(outbox.is_closed());
        assert!(outbox.0.queue.lock().unwrap().packets.is_empty());
    }
}
