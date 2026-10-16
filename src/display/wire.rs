//! The display's messages on the link: its orders and answers, gliff's stream messages and
//! their feedback. The link (`crate::link`) carries them as top-level `LinkMsg` variants, a
//! video frame's bytes going straight from the encoder to the socket and from the socket to the
//! decoder.
//!
//! The write queue is adapted from gliff-server's `writer.rs` (gliff, MIT, see
//! `LICENSES/gliff-MIT.txt`): a video frame waits alone, and a newer cursor or stream message
//! replaces an older one still queued, so the queue stays short however slow the link.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use gliff_proto::{ClientCaps, ClientMsg, ServerMsg};
use minicbor::{Decode, Encode};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Notify, watch};

/// The most outputs a mode streams: an output id from 0 to `MAX_OUTPUTS - 1`, and a message
/// for another one ends the link.
pub const MAX_OUTPUTS: u8 = 8;

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

/// What the display sends. A stream's messages (`Video`, `Feedback`) are for the output of the
/// [`Outbox`] they go through.
#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Display(Display),
    /// Remote to home: gliff's stream messages (config, frames, cursor, pings).
    Video(ServerMsg),
    /// Home to remote: gliff's answers to them (frame acks, keyframe requests, pongs).
    Feedback(ClientMsg),
    /// Remote to home: the picture goes on, but home's input cannot be injected (the reason).
    NoInput(String),
    /// Remote to home: the mode goes on, but the lid is left alone (the reason).
    NoLid(String),
    /// Home to remote, once the link is up: how the desk's user calls it (`local_label`).
    Desk(String),
}

/// The display's orders and their answers.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Display {
    /// Home to remote: make a headless output that copies this home monitor, and stream it.
    #[n(0)]
    Start {
        /// Output 0, the leftmost.
        #[n(0)]
        output: OutputCopy,
        /// What home decodes.
        #[n(1)]
        caps: ClientCaps,
        /// The other monitors, outputs 1, 2…, in home's left-to-right order.
        #[n(2)]
        others: Option<Vec<OutputCopy>>,
        /// Always `Some(true)`: an agent of the release before keeps its lid (`crate::lid`)
        /// only when asked. An agent now keeps it during every mode, and ignores this.
        #[n(3)]
        closed_lid: Option<bool>,
        /// Which mode this is, said back in `Started` and `Failed` so that home can tell the
        /// answer of a mode it already stopped (a switch) from the new one's. An agent that
        /// does not know it ignores it and never says it back.
        #[n(4)]
        generation: Option<u32>,
    },
    /// Home to remote: stop streaming and remove the output.
    #[n(1)]
    Stop,
    /// Remote to home: the output exists, its stream follows.
    #[n(2)]
    Started {
        #[n(0)]
        output: String,
        /// The names of outputs 1, 2…; an agent that knows only one output never sends them.
        #[n(1)]
        others: Option<Vec<String>>,
        /// `Some(true)`: the agent keeps its lid during the mode. One that does not never says.
        #[n(2)]
        closed_lid: Option<bool>,
        /// The `generation` of the `Start` it answers, when it had one.
        #[n(3)]
        generation: Option<u32>,
    },
    /// Remote to home: the output could not be made, or the mode ended on the remote; nothing
    /// was left behind.
    #[n(3)]
    Failed {
        #[n(0)]
        reason: String,
        /// The `generation` of the `Start` of the mode that failed, when it had one.
        #[n(1)]
        generation: Option<u32>,
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
        output: u8,
        msg: ServerMsg,
        main: Bytes,
        aux: Bytes,
    },
    Feedback {
        output: u8,
        msg: ClientMsg,
    },
    NoInput(String),
    NoLid(String),
    Desk(String),
}

/// One live connection, as the display and its input see it. `inbox` ends when the link drops.
pub struct Peer {
    pub outbox: Outbox,
    pub inbox: mpsc::Receiver<Incoming>,
    /// Remote to home, each output's at its id (none yet: the default); it ends with the inbox.
    pub pointer: watch::Receiver<Vec<Pointer>>,
    pub input: crate::input::Wire,
    /// Whether a mode runs on this link, which this machine's display says: nothing else
    /// crosses outside one (see [`Wire::mode`](crate::link::Wire::mode)).
    pub mode: watch::Sender<bool>,
}

/// A message queued, with the output it is for and its payloads.
type Packet = (u8, Msg, Vec<Vec<u8>>);

#[derive(Default)]
struct Queue {
    packets: VecDeque<Packet>,
    closed: bool,
}

fn is_video(msg: &Msg) -> bool {
    matches!(msg, Msg::Video(ServerMsg::VideoFrame { .. }))
}

/// Each output's stream has its own rules: one frame of its own waiting, its own newest cursor.
impl Queue {
    fn video_ready(&self, output: u8) -> bool {
        !self
            .packets
            .iter()
            .any(|(o, msg, _)| *o == output && is_video(msg))
    }

    fn push(&mut self, output: u8, msg: Msg, payloads: Vec<Vec<u8>>) {
        match &msg {
            Msg::Video(ServerMsg::StreamConfig { .. }) => {
                // A config queued after the last frame is superseded; one before it is not.
                let after_video = self
                    .packets
                    .iter()
                    .rposition(|(o, msg, _)| *o == output && is_video(msg))
                    .map_or(0, |i| i + 1);
                let mut index = 0;
                self.packets.retain(|(o, msg, _)| {
                    let keep = index < after_video
                        || *o != output
                        || !matches!(msg, Msg::Video(ServerMsg::StreamConfig { .. }));
                    index += 1;
                    keep
                });
            }
            // Unlike gliff's, a newer image leaves the visibility queued: home reads them apart.
            Msg::Video(
                ServerMsg::CursorShape { .. }
                | ServerMsg::CursorPos { .. }
                | ServerMsg::Pong { .. },
            )
            | Msg::Feedback(ClientMsg::Pong { .. }) => {
                let same = |queued: &Msg| match (queued, &msg) {
                    (Msg::Video(a), Msg::Video(b)) => {
                        std::mem::discriminant(a) == std::mem::discriminant(b)
                    }
                    (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b),
                };
                self.packets
                    .retain(|(o, queued, _)| *o != output || !same(queued));
            }
            _ => {}
        }
        self.packets.push_back((output, msg, payloads));
    }
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    notify: Notify,
    /// How long each write of an output's took, for whoever paces its video stream.
    reports: Mutex<HashMap<u8, UnboundedSender<Duration>>>,
}

/// The sending side of a link: `send` only queues, so a caller never waits on the network. Its
/// messages are for output 0; [`Outbox::on`] gives the same link for another output.
#[derive(Clone, Default)]
pub struct Outbox {
    shared: Arc<Shared>,
    output: u8,
}

impl Outbox {
    /// The same link, its stream messages for `output`.
    pub fn on(&self, output: u8) -> Self {
        Self {
            shared: self.shared.clone(),
            output,
        }
    }

    /// The output its stream messages are for.
    pub fn output(&self) -> u8 {
        self.output
    }

    /// Queues a message and the payloads it declares. Dropped once the link is down.
    pub fn send(&self, msg: Msg, payloads: Vec<Vec<u8>>) {
        let mut queue = self.shared.queue.lock().unwrap();
        if queue.closed {
            return;
        }
        queue.push(self.output, msg, payloads);
        drop(queue);
        self.shared.notify.notify_one();
    }

    /// No video frame of this output waits to be written: its encoder works one frame ahead
    /// at most.
    pub fn video_ready(&self) -> bool {
        self.shared.queue.lock().unwrap().video_ready(self.output)
    }

    pub fn is_closed(&self) -> bool {
        self.shared.queue.lock().unwrap().closed
    }

    /// The duration of every write of this output's from now on, for its rate controller.
    pub fn write_reports(&self) -> UnboundedReceiver<Duration> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.shared.reports.lock().unwrap().insert(self.output, tx);
        rx
    }

    /// What waits to be written, oldest first, whatever its output.
    #[cfg(test)]
    pub fn queued(&self) -> Vec<Msg> {
        let queue = self.shared.queue.lock().unwrap();
        queue
            .packets
            .iter()
            .map(|(_, msg, _)| msg.clone())
            .collect()
    }

    /// The link is down: nothing more is queued, and nothing waits.
    pub(crate) fn close(&self) {
        let mut queue = self.shared.queue.lock().unwrap();
        queue.closed = true;
        queue.packets.clear();
        drop(queue);
        self.shared.reports.lock().unwrap().clear();
    }

    /// The next message to write, oldest first, with its output.
    pub(crate) async fn next(&self) -> Packet {
        loop {
            if let Some(packet) = self.shared.queue.lock().unwrap().packets.pop_front() {
                return packet;
            }
            self.shared.notify.notified().await;
        }
    }

    /// How long a write of `output`'s took, for its rate controller.
    pub(crate) fn report_write(&self, output: u8, took: Duration) {
        if let Some(reports) = self.shared.reports.lock().unwrap().get(&output) {
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
        queue.push(0, msg, payloads);
        assert!(!queue.video_ready(0));
        for id in 0..100 {
            queue.push(
                0,
                Msg::Video(ServerMsg::CursorPos {
                    x: id as f64,
                    y: 0.0,
                    shape_id: 0,
                    visible: true,
                }),
                Vec::new(),
            );
            queue.push(0, Msg::Feedback(ClientMsg::Pong { t: id }), Vec::new());
        }
        assert_eq!(queue.packets.len(), 3);
        assert!(matches!(
            queue.packets[1].1,
            Msg::Video(ServerMsg::CursorPos { x: 99.0, .. })
        ));
    }

    #[test]
    fn a_newer_cursor_image_leaves_the_cursor_s_visibility_queued() {
        let mut queue = Queue::default();
        let hidden = Msg::Video(ServerMsg::CursorPos {
            x: 0.0,
            y: 0.0,
            shape_id: 0,
            visible: false,
        });
        queue.push(0, hidden.clone(), Vec::new());
        for id in 1..=2 {
            let shape = ServerMsg::CursorShape {
                id,
                width: 1,
                height: 1,
                hot_x: 0,
                hot_y: 0,
                argb_len: 4,
            };
            queue.push(0, Msg::Video(shape), vec![vec![0; 4]]);
        }
        let queued: Vec<_> = queue
            .packets
            .iter()
            .map(|(_, msg, _)| msg.clone())
            .collect();
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0], hidden);
        assert!(matches!(
            queued[1],
            Msg::Video(ServerMsg::CursorShape { id: 2, .. })
        ));
    }

    #[test]
    fn a_closed_outbox_drops_what_is_sent() {
        let outbox = Outbox::default();
        outbox.close();
        outbox.send(Msg::Feedback(ClientMsg::RequestKeyframe), Vec::new());
        assert!(outbox.is_closed());
        assert!(outbox.shared.queue.lock().unwrap().packets.is_empty());
    }

    fn cursor_at(x: f64) -> Msg {
        Msg::Video(ServerMsg::CursorPos {
            x,
            y: 0.0,
            shape_id: 0,
            visible: true,
        })
    }

    #[test]
    fn two_outputs_queue_a_frame_each_and_keep_their_own_cursor() {
        let outbox = Outbox::default();
        let (left, right) = (outbox.on(0), outbox.on(1));
        let (msg, payloads) = frame(1, 4);
        left.send(msg, payloads);
        // Output 0's frame waiting does not hold output 1's encoder back.
        assert!(!left.video_ready());
        assert!(right.video_ready());
        let (msg, payloads) = frame(1, 4);
        right.send(msg, payloads);
        assert!(!right.video_ready());
        left.send(cursor_at(1.0), Vec::new());
        right.send(cursor_at(2.0), Vec::new());
        left.send(cursor_at(3.0), Vec::new());
        let queued = outbox.shared.queue.lock().unwrap();
        let outputs: Vec<_> = queued
            .packets
            .iter()
            .map(|(o, msg, _)| (*o, msg.clone()))
            .collect();
        assert_eq!(outputs.len(), 4, "{outputs:?}");
        assert_eq!(outputs[2], (1, cursor_at(2.0)));
        assert_eq!(outputs[3], (0, cursor_at(3.0)));
    }

    #[test]
    fn a_stream_config_supersedes_only_its_own_output_s() {
        let mut queue = Queue::default();
        let config = |width| {
            Msg::Video(ServerMsg::StreamConfig {
                codec: gliff_proto::Codec::H264,
                chroma: gliff_proto::ChromaMode::Single420,
                pipeline: None,
                width,
                height: 1080,
                scale_milli: 1000,
                extradata: vec![],
                aux_extradata: None,
                view_width: width,
                view_height: 1080,
                fps_cap: 60,
            })
        };
        queue.push(0, config(1920), Vec::new());
        queue.push(1, config(2560), Vec::new());
        queue.push(0, config(1280), Vec::new());
        let queued: Vec<_> = queue
            .packets
            .iter()
            .map(|(o, msg, _)| (*o, msg.clone()))
            .collect();
        assert_eq!(queued, [(1, config(2560)), (0, config(1280))]);
    }

    #[tokio::test]
    async fn each_output_s_writes_go_to_its_own_rate_controller() {
        let outbox = Outbox::default();
        let mut left = outbox.on(0).write_reports();
        let mut right = outbox.on(1).write_reports();
        outbox.report_write(1, Duration::from_millis(7));
        assert_eq!(right.recv().await, Some(Duration::from_millis(7)));
        assert!(left.try_recv().is_err());
    }
}
