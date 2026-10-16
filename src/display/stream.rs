//! The remote's picture path for one output: capture it (`hypr-capture`), encode it (gliff-vk
//! and VA-API on the GPU, Dual420 as gliff does; gliff-sw on the CPU when the GPU cannot),
//! pace it and adapt the rate to the link, and send it home, with the cursor's image apart.
//!
//! Adapted from gliff-server's `session.rs` (gliff, MIT, see `LICENSES/gliff-MIT.txt`): the
//! capture, pacing, refinement, ladder and fallback logic is gliff's. What Navette drops: the
//! handshake (the display's `Start` order replaces it), input and clipboard (their own modules),
//! resizing to a client window and mirroring (the output copies a home monitor, fixed).

use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gliff_proto::{ChromaMode, ClientCaps, ClientMsg, Codec, Rect, ServerMsg, VideoPipeline};
use gliff_sw::VideoMode;
use gliff_vk::{DmabufPlane, EncodedFrame, Encoder, EncoderSettings, Gpu};
use hypr_capture::{CaptureConfig, CaptureEvent, CapturedFrame, Capturer};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::rate::{Ladder, LinkEstimator, RateController};
use crate::display::wire::{Msg, Outbox};

/// The session-wide fps ceiling; ladder levels cap below it.
const MAX_FPS: u32 = 60;
/// Still-picture refinement: this many re-encodes of an unchanged frame, at least this far
/// apart. All of them run: the encoder lowers its quantiser only after a pass came in under
/// budget, so the first passes are small even when the picture is still soft.
const MAX_REFINES: u8 = 8;
const REFINE_EVERY: Duration = Duration::from_millis(200);
/// A static screen produces no damage and so no first frame; after this long the capture is
/// retried once with full damage.
const FIRST_FRAME_DEADLINE: Duration = Duration::from_millis(700);
/// The starting ladder level: 30 fps, full quality.
const START_LEVEL: usize = 1;

/// What is streamed: an output of this machine's Hyprland, at its scale.
#[derive(Clone, Debug)]
pub struct Source {
    pub output: String,
    pub scale: f32,
}

/// A running stream, on its own thread. Dropping it stops the capture before returning, so
/// the output can be removed right after.
pub struct Stream {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts streaming `source` into `outbox`; `feedback` carries home's acks. Returns once the
/// capture and the encoder are up. `ended` gets the reason if the stream stops on its own
/// (the output or Hyprland went away).
pub fn start(
    source: Source,
    caps: ClientCaps,
    outbox: Outbox,
    feedback: UnboundedReceiver<ClientMsg>,
    ended: tokio::sync::oneshot::Sender<io::Error>,
) -> io::Result<Stream> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("navette-stream".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let result = runtime.block_on(async move {
                let session = match Session::open(source, caps, outbox) {
                    Ok(opened) => {
                        let _ = ready_tx.send(Ok(()));
                        opened
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return Ok(());
                    }
                };
                let (session, captures) = session;
                session.run(captures, feedback, stop_rx).await
            });
            if let Err(e) = result {
                let _ = ended.send(e);
            }
        })?;
    let mut stream = Stream {
        stop: Some(stop_tx),
        thread: Some(thread),
    };
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(stream),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            stream.stop = None;
            Err(io::Error::other("the stream thread ended before starting"))
        }
    }
}

fn other(context: &str, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{context}: {e}"))
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// Events from the capture thread.
enum Incoming {
    Frame(Frame),
    /// The cursor's new image, little-endian ARGB8888.
    Cursor {
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        argb: Vec<u8>,
    },
    /// The remote shows the cursor, or hides it.
    CursorVisible(bool),
    Stopped,
    Error(String),
}

/// A picture to encode: a captured frame, or plain pixels in tests.
enum Frame {
    Captured(CapturedFrame),
    #[cfg(test)]
    Pixels {
        width: u32,
        height: u32,
        bgra: Arc<Vec<u8>>,
    },
}

impl Frame {
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Captured(frame) => (frame.buffer.info.width & !1, frame.buffer.info.height & !1),
            #[cfg(test)]
            Self::Pixels { width, height, .. } => (*width, *height),
        }
    }
}

/// What the session asks of the capture: `hypr-capture` live, a fake in tests.
trait Capture {
    /// Deliver the next frame with damage.
    fn request_frame(&mut self);
    /// Deliver the current picture, damaged or not.
    fn recapture(&mut self);
}

impl Capture for Capturer {
    fn request_frame(&mut self) {
        Capturer::request_frame(self).ok();
    }

    fn recapture(&mut self) {
        Capturer::recapture(self).ok();
    }
}

struct Session {
    outbox: Outbox,
    video: VideoTier,
    source: Source,
    /// The output's size, as captured.
    output: (u32, u32),
    /// Encoded size: the output's, unless larger than the encoder's maximum.
    stream: (u32, u32),
    encoder_max: (u32, u32),
    /// A GPU encode has succeeded, so a later failure is not a broken driver path.
    gpu_encoded: bool,
    caps: ClientCaps,
    link: LinkEstimator,
    ctl: RateController,
    chroma: ChromaMode,
    settings: EncoderSettings,
    encoder: VideoEncoder,
    capturer: Box<dyn Capture>,
    /// The newest captured frame not yet encoded.
    pending: Option<Frame>,
    capture_asked: bool,
    /// The pending frame has already counted as blocked for the rate controller.
    blocked_noted: bool,
    frame_id: u64,
    /// The id of the last cursor image sent.
    cursor_shape_id: u32,
    want_keyframe: bool,
    /// The one commanded cadence: the pace timer and the encoder's frame rate both use it.
    fps_cmd: u32,
    /// Smoothed encode time, for the sustainable-fps clamp.
    encode_us: f64,
    next_send_at: Instant,
    /// The last frame that went out, kept for still-picture refinement.
    last_frame: Option<Frame>,
    refines: u8,
    /// The encoder runs at the refinement rate, not the controller's target.
    refining: bool,
    next_refine_at: Instant,
    recaptured: bool,
    ladder: Ladder,
    /// The full-quality size; the active stream is this scaled by the ladder level.
    base_stream: (u32, u32),
    base_chroma: ChromaMode,
}

impl Session {
    fn open(
        source: Source,
        caps: ClientCaps,
        outbox: Outbox,
    ) -> io::Result<(Self, UnboundedReceiver<Incoming>)> {
        if !caps.codecs.contains(&Codec::H264) {
            return Err(io::Error::other(
                "the desk decodes no codec this machine encodes",
            ));
        }
        let render_node = hypr_capture::render_node(None);
        let (output, chroma_ok) = {
            let monitors = crate::hypr::monitors(&crate::hypr::Live)?;
            let m = monitors
                .iter()
                .find(|m| m.name == source.output)
                .ok_or_else(|| io::Error::other(format!("no output {}", source.output)))?;
            (
                (m.width & !1, m.height & !1),
                caps.chroma.contains(&ChromaMode::Dual420)
                    || caps.chroma.contains(&ChromaMode::Single420),
            )
        };
        if !chroma_ok {
            return Err(io::Error::other(
                "the desk decodes no chroma mode this machine encodes",
            ));
        }
        let video = VideoTier::open(VideoMode::resolve(None), &render_node);
        let (cap_tx, captures) = mpsc::unbounded_channel();
        let capturer = start_capture(&source.output, &render_node, cap_tx)?;
        let session = Self::new(source, caps, outbox, output, video, Box::new(capturer))?;
        Ok((session, captures))
    }

    fn new(
        source: Source,
        caps: ClientCaps,
        outbox: Outbox,
        output: (u32, u32),
        video: VideoTier,
        capturer: Box<dyn Capture>,
    ) -> io::Result<Self> {
        let ladder = Ladder::new(START_LEVEL);
        let fps_cmd = ladder.level().fps_cap.min(MAX_FPS);
        let on_gpu = matches!(video, VideoTier::Gpu(_));
        let start = match VideoStart::open(video, &caps, output, fps_cmd) {
            Ok(start) => start,
            Err(e) if on_gpu => {
                tracing::warn!(error = %e, "cannot start the GPU video pipeline; falling back to the CPU pipeline");
                VideoStart::open(VideoTier::Cpu, &caps, output, fps_cmd)?
            }
            Err(e) => return Err(e),
        };
        eprintln!(
            "navette: streaming {} ({}x{}) encoded on the {}",
            source.output,
            output.0,
            output.1,
            start.video.pipeline().label()
        );
        Ok(Self {
            outbox,
            video: start.video,
            source,
            output,
            stream: start.stream,
            encoder_max: start.encoder_max,
            gpu_encoded: false,
            caps,
            link: LinkEstimator::new(),
            ctl: start.ctl,
            chroma: start.chroma,
            settings: start.settings,
            encoder: start.encoder,
            capturer,
            pending: None,
            capture_asked: false,
            blocked_noted: false,
            frame_id: 0,
            cursor_shape_id: 0,
            want_keyframe: true,
            fps_cmd,
            encode_us: 0.0,
            next_send_at: Instant::now(),
            last_frame: None,
            refines: MAX_REFINES,
            refining: false,
            next_refine_at: Instant::now(),
            recaptured: false,
            ladder,
            base_stream: start.stream,
            base_chroma: start.chroma,
        })
    }

    async fn run(
        mut self,
        mut captures: UnboundedReceiver<Incoming>,
        mut feedback: UnboundedReceiver<ClientMsg>,
        mut stop: tokio::sync::oneshot::Receiver<()>,
    ) -> io::Result<()> {
        let mut write_reports = self.outbox.write_reports();
        // The Pong seeds the round-trip estimate, usually before the first frame ack.
        self.outbox.send(
            Msg::Video(ServerMsg::Ping { t: now_us() / 1000 }),
            Vec::new(),
        );
        self.send_config();
        self.capturer.request_frame();
        self.capture_asked = true;
        let first_frame_deadline =
            tokio::time::Instant::from_std(Instant::now() + FIRST_FRAME_DEADLINE);
        loop {
            tokio::select! {
                _ = &mut stop => return Ok(()),
                msg = feedback.recv() => match msg {
                    Some(msg) => self.on_feedback(msg)?,
                    // The link is down: the controller stops this stream.
                    None => return Ok(()),
                },
                ev = captures.recv() => match ev {
                    Some(Incoming::Frame(frame)) => self.on_frame(frame)?,
                    Some(Incoming::Cursor { width, height, hot_x, hot_y, argb }) => {
                        self.cursor_shape_id += 1;
                        let shape = ServerMsg::CursorShape {
                            id: self.cursor_shape_id,
                            width,
                            height,
                            hot_x,
                            hot_y,
                            argb_len: argb.len() as u32,
                        };
                        self.outbox.send(Msg::Video(shape), vec![argb]);
                    }
                    Some(Incoming::CursorVisible(visible)) => {
                        let seen = ServerMsg::CursorPos {
                            x: 0.0,
                            y: 0.0,
                            shape_id: self.cursor_shape_id,
                            visible,
                        };
                        self.outbox.send(Msg::Video(seen), Vec::new());
                    }
                    Some(Incoming::Stopped) | None => {
                        return Err(io::Error::other("the capture stopped"));
                    }
                    Some(Incoming::Error(e)) => {
                        return Err(io::Error::other(format!("capture error: {e}")));
                    }
                },
                // Every write ends with a report, which also wakes a frame waiting on the writer.
                Some(elapsed) = write_reports.recv() => {
                    // A write longer than a frame interval: the socket, not the byte cap, was
                    // the bottleneck.
                    if elapsed > self.frame_interval() {
                        self.ctl.note_writer_blocked();
                    }
                }
                _ = tokio::time::sleep_until(first_frame_deadline),
                    if self.frame_id == 0 && !self.recaptured =>
                {
                    self.recaptured = true;
                    self.capturer.recapture();
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.refine_wake())),
                    if self.can_refine() => {}
                // The guard must not test the deadline itself, or the arm would disarm exactly
                // while waiting for it.
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(self.next_send_at)),
                    if self.pending.is_some() && self.link_has_room() => {}
            }
            if self.outbox.is_closed() {
                return Ok(());
            }
            self.pump_encoder()?;
        }
    }

    fn on_feedback(&mut self, msg: ClientMsg) -> io::Result<()> {
        match msg {
            ClientMsg::FrameAck { frame_id, .. } => {
                let now = Instant::now();
                self.link.on_ack(now, frame_id);
                if self.ctl.on_ack(now, &mut self.link).is_some() {
                    self.apply_rate();
                }
                if self.pending.is_none() && self.link.inflight_bytes() == 0 {
                    self.ctl.note_idle(now);
                }
                let dual = self.base_chroma == ChromaMode::Dual420;
                let can_drop_aux = self.can_drop_aux();
                let pixels = self.base_stream.0 as u64 * self.base_stream.1 as u64;
                if let Some(level) = self.ladder.consider(
                    now,
                    &self.link,
                    self.ctl.is_slow_start(),
                    self.ctl.congested_recently(now),
                    pixels,
                    dual,
                    can_drop_aux,
                    MAX_FPS,
                ) {
                    tracing::info!(level, fps_cap = self.fps_cap(), "ladder step");
                    if self.apply_stream_params()? {
                        self.send_config();
                    }
                }
            }
            ClientMsg::RequestKeyframe => {
                self.want_keyframe = true;
                // A still screen has no capture pending: answer with the kept frame.
                if self.pending.is_none() && self.last_frame.is_some() {
                    self.refines = 0;
                    self.next_refine_at = Instant::now();
                }
            }
            ClientMsg::Ping { t } => self.outbox.send(
                Msg::Video(ServerMsg::Pong {
                    t,
                    server_now_ms: now_us() / 1000,
                }),
                Vec::new(),
            ),
            ClientMsg::Pong { t } => {
                let rtt_ms = (now_us() / 1000).saturating_sub(t) as f64;
                self.link.seed_rtt(Instant::now(), rtt_ms);
            }
            _ => {}
        }
        Ok(())
    }

    fn on_frame(&mut self, frame: Frame) -> io::Result<()> {
        let size = frame.size();
        if size != self.output {
            // A reload dropped the monitor rule and Hyprland changed the mode before the rule
            // came back: stream what is there.
            tracing::info!(width = size.0, height = size.1, "output resized");
            self.output = size;
            self.base_stream = EncoderSettings::fit_extent(size.0, size.1, self.encoder_max);
            if self.apply_stream_params()? {
                self.send_config();
            }
        }
        self.pending = Some(frame);
        self.capture_asked = false;
        self.blocked_noted = false;
        Ok(())
    }

    /// Encodes and queues the pending frame if the link has room and the pace slot is due,
    /// then asks the capture thread for the next one.
    fn pump_encoder(&mut self) -> io::Result<()> {
        let now = Instant::now();
        // Only a wait on the byte cap is congestion; count it once per frame.
        if self.pending.is_some()
            && !self.blocked_noted
            && self.outbox.video_ready()
            && !self.byte_cap_has_room(now)
        {
            self.ctl.note_blocked_at_cap();
            self.blocked_noted = true;
        }
        if self.link_has_room() && now >= self.next_send_at {
            if let Some(frame) = self.pending.take() {
                if self.matches_output(&frame) {
                    if self.refining {
                        self.refining = false;
                        self.apply_rate();
                    }
                    self.encode_and_send(&frame, false)?;
                    self.last_frame = Some(frame);
                    self.refines = 0;
                    self.next_refine_at = now + REFINE_EVERY;
                }
            } else if now >= self.next_refine_at
                && self.refines < MAX_REFINES
                && let Some(frame) = self.last_frame.take()
                && self.matches_output(&frame)
            {
                // Re-encode the same buffer so a frame that arrived soft under a low budget
                // converges to sharp while nothing changes on screen.
                if !self.refining {
                    self.refining = true;
                    self.apply_rate();
                }
                self.encode_and_send(&frame, true)?;
                self.refines += 1;
                self.next_refine_at = now + REFINE_EVERY;
                self.last_frame = Some(frame);
            }
        }
        // Keep one capture request outstanding even while blocked: a newer frame replaces
        // `pending`, so the frame that finally goes out on a slow link is current.
        if !self.capture_asked {
            self.capturer.request_frame();
            self.capture_asked = true;
        }
        Ok(())
    }

    fn link_has_room(&self) -> bool {
        self.outbox.video_ready() && self.byte_cap_has_room(Instant::now())
    }

    fn byte_cap_has_room(&self, now: Instant) -> bool {
        self.link.inflight_bytes() == 0
            || self.link.inflight_bytes() + self.link.est_frame_bytes()
                <= self.link.cap_bytes(now, self.ctl.is_slow_start())
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps_cmd.max(1) as f64)
    }

    fn matches_output(&self, frame: &Frame) -> bool {
        frame.size() == self.output
    }

    fn refine_wake(&self) -> Instant {
        self.next_refine_at.max(self.next_send_at)
    }

    fn can_refine(&self) -> bool {
        self.pending.is_none()
            && self.last_frame.is_some()
            && self.refines < MAX_REFINES
            && self.link_has_room()
    }

    fn fps_cap(&self) -> u32 {
        self.ladder.level().fps_cap.min(MAX_FPS)
    }

    fn can_drop_aux(&self) -> bool {
        self.caps.chroma.contains(&ChromaMode::Single420)
    }

    fn active_chroma(&self) -> ChromaMode {
        if self.ladder.level().aux || !self.can_drop_aux() {
            self.base_chroma
        } else {
            ChromaMode::Single420
        }
    }

    fn update_fps_cmd(&mut self) {
        let sustainable = if self.encode_us > 0.0 {
            (1e6 / (self.encode_us * 1.1)).clamp(5.0, MAX_FPS as f64) as u32
        } else {
            MAX_FPS
        };
        self.fps_cmd = self.fps_cap().min(sustainable).max(1);
    }

    fn send_config(&mut self) {
        // The scale home divides stream pixels by to reach the remote's logical space.
        let scale = self.source.scale * self.stream.0 as f32 / self.output.0.max(1) as f32;
        self.outbox.send(
            Msg::Video(ServerMsg::StreamConfig {
                codec: Codec::H264,
                chroma: self.chroma,
                pipeline: Some(self.video.pipeline()),
                width: self.stream.0,
                height: self.stream.1,
                scale_milli: (scale * 1000.0).round() as u32,
                // Parameter sets ride in-band on every keyframe.
                extradata: Vec::new(),
                aux_extradata: None,
                view_width: self.base_stream.0,
                view_height: self.base_stream.1,
                fps_cap: self.fps_cap(),
            }),
            Vec::new(),
        );
    }

    /// Derives the active stream from the full-quality size and the ladder level. A change of
    /// size or chroma rebuilds the encoder. False when the encoder rejected the geometry.
    fn apply_stream_params(&mut self) -> io::Result<bool> {
        self.update_fps_cmd();
        let level = self.ladder.level();
        let chroma = self.active_chroma();
        let want = EncoderSettings::fit_extent(
            ((self.base_stream.0 as f32 * level.scale) as u32).max(2) & !1,
            ((self.base_stream.1 as f32 * level.scale) as u32).max(2) & !1,
            self.encoder_max,
        );
        if want == self.stream && chroma == self.chroma {
            self.apply_rate();
            return Ok(true);
        }
        let streams = if chroma == ChromaMode::Dual420 { 2 } else { 1 };
        self.ctl
            .reconfigure(want.0 as u64 * want.1 as u64, streams, MAX_FPS);
        let settings = encoder_settings(
            self.video.is_gpu(),
            want,
            self.ctl.stream_bitrate(),
            self.fps_cmd,
            self.ctl.vbv_ms(self.fps_cmd),
        );
        match VideoEncoder::new(&self.video, &settings, chroma == ChromaMode::Dual420) {
            Ok(encoder) => {
                self.encoder = encoder;
                self.settings = settings;
                self.stream = want;
                self.chroma = chroma;
                self.want_keyframe = true;
                self.encode_us = 0.0;
                self.refines = 0;
                self.refining = false;
                self.next_refine_at = Instant::now();
                Ok(true)
            }
            Err(e) => {
                tracing::warn!(error = %e, "encoder rejected the size; keeping the current one");
                Ok(false)
            }
        }
    }

    fn note_encode_time(&mut self, enc_us: f64) {
        self.encode_us = if self.encode_us == 0.0 {
            enc_us
        } else {
            0.8 * self.encode_us + 0.2 * enc_us
        };
        let before = self.fps_cmd;
        self.update_fps_cmd();
        let moved = (self.fps_cmd as f64 - before as f64).abs() / before.max(1) as f64;
        if moved > 0.15 {
            self.apply_rate();
        } else {
            self.fps_cmd = before;
        }
    }

    /// A refinement pass runs at the controller's ceiling, not its target: the screen is still
    /// and the link idle, and the target only grows on what motion measured, so on a page left
    /// still it stays at its starting value, too low for small text to ever sharpen.
    fn apply_rate(&mut self) {
        let bitrate = if self.refining {
            self.ctl.ceiling_bitrate()
        } else {
            self.ctl.stream_bitrate()
        };
        let vbv = self.ctl.vbv_ms(self.fps_cmd);
        let framerate = encoder_framerate(self.video.is_gpu(), self.refining, self.fps_cmd);
        self.settings.bitrate = bitrate;
        self.settings.framerate = framerate;
        self.settings.vbv_ms = vbv;
        self.encoder.set_rate(bitrate, framerate, vbv);
    }

    /// Encodes `frame` and queues it; returns the coded byte count. `app_limited` marks a send
    /// that does not use the capacity (a refinement pass), so it cannot lower the estimate.
    fn encode_and_send(&mut self, frame: &Frame, app_limited: bool) -> io::Result<usize> {
        let (width, height) = self.stream;
        let key = std::mem::take(&mut self.want_keyframe);
        let t0 = Instant::now();
        let pts_us = now_us();
        let encoded = match self.encoder.encode(frame, self.stream, key) {
            Ok(e) => e,
            Err(e) if matches!(self.video, VideoTier::Gpu(_)) && !self.gpu_encoded => {
                tracing::warn!(error = %e, "the first GPU encode failed; falling back to the CPU pipeline");
                eprintln!("navette: the GPU encode failed ({e}); encoding on the CPU");
                self.video = VideoTier::Cpu;
                self.encoder_max = within_home(gliff_sw::Encoder::MAX_SIZE, &self.caps);
                self.encoder = VideoEncoder::new(
                    &self.video,
                    &self.settings,
                    self.chroma == ChromaMode::Dual420,
                )?;
                self.send_config();
                self.encoder.encode(frame, self.stream, true)?
            }
            Err(e) => return Err(e),
        };
        self.gpu_encoded = true;
        let enc_us = t0.elapsed().as_micros();
        let aux = encoded.aux.unwrap_or_default();
        let msg = ServerMsg::VideoFrame {
            frame_id: self.frame_id,
            pts_us,
            keyframe: encoded.keyframe,
            damage: vec![Rect {
                x: 0,
                y: 0,
                width: width as i32,
                height: height as i32,
            }],
            data_len: encoded.main.len() as u32,
            aux_len: aux.len() as u32,
        };
        let total = encoded.main.len() + aux.len();
        // A frame that neither waited on the byte cap nor fills the pipe is app-limited: its
        // delivery rate reflects the content, not what the link could carry.
        let now = Instant::now();
        let cap = self.link.cap_bytes(now, self.ctl.is_slow_start());
        let app_limited = app_limited
            || (!self.blocked_noted
                && self.link.inflight_bytes() + total as u64 + self.link.est_frame_bytes() <= cap);
        self.outbox.send(Msg::Video(msg), vec![encoded.main, aux]);
        self.ctl.note_frame_sent(now);
        self.link.on_sent(t0, self.frame_id, total, app_limited);
        self.note_encode_time(enc_us as f64);
        // Pace from the encode start so a slow encode adds no extra wait.
        let interval = self.frame_interval();
        self.next_send_at = (self.next_send_at + interval).max(t0 + interval);
        self.blocked_noted = false;
        self.frame_id += 1;
        Ok(total)
    }
}

/// Ceiling on the GPU encoder's quantiser. Without one, a frame that changes the whole picture
/// (a page change, a scroll) comes out very coarse and the old page ghosts under the new one;
/// measured on a stand-in encoder, such a frame was 23 dB at 9 kB, and 37 dB at about 82 kB
/// with this ceiling. The CPU encoder has no such setting.
const GPU_MAX_QP: u8 = 36;

fn max_qp(gpu: bool) -> Option<u8> {
    gpu.then_some(GPU_MAX_QP)
}

/// The frame rate the encoder is told. A refinement pass comes every `REFINE_EVERY`, but told
/// the commanded rate each pass would get a 30th of the budget instead of a fifth; told its
/// real pace, still text sharpens within half a second. The CPU encoder is left alone: a frame
/// rate change rebuilds it with a keyframe.
fn encoder_framerate(gpu: bool, refining: bool, fps_cmd: u32) -> u32 {
    if gpu && refining {
        (1000 / REFINE_EVERY.as_millis() as u32).max(1)
    } else {
        fps_cmd
    }
}

fn encoder_settings(
    gpu: bool,
    size: (u32, u32),
    bitrate: u32,
    framerate: u32,
    vbv_ms: u32,
) -> EncoderSettings {
    EncoderSettings {
        width: size.0,
        height: size.1,
        bitrate,
        framerate,
        vbv_ms,
        max_qp: max_qp(gpu),
    }
}

/// Vulkan compute and VA-API on the GPU, or OpenH264 on the CPU.
enum VideoTier {
    Gpu(Arc<Gpu>),
    Cpu,
}

impl VideoTier {
    fn is_gpu(&self) -> bool {
        matches!(self, Self::Gpu(_))
    }

    fn open(mode: VideoMode, render_node: &std::path::Path) -> Self {
        if mode == VideoMode::Cpu {
            return Self::Cpu;
        }
        match Gpu::open(Some(render_node)) {
            Ok(gpu) if gpu.can_encode() => Self::Gpu(gpu),
            Ok(gpu) => {
                tracing::warn!(gpu = %gpu.name, "no VA-API H.264 encoder; falling back to the CPU pipeline");
                Self::Cpu
            }
            Err(e) => {
                tracing::warn!(error = %e, "no usable GPU; falling back to the CPU pipeline");
                Self::Cpu
            }
        }
    }

    fn encoder_max(&self) -> io::Result<(u32, u32)> {
        match self {
            Self::Gpu(gpu) => Encoder::max_size(gpu).map_err(|e| other("encoder limits", e)),
            Self::Cpu => Ok(gliff_sw::Encoder::MAX_SIZE),
        }
    }

    fn pipeline(&self) -> VideoPipeline {
        match self {
            Self::Gpu(_) => VideoPipeline::Gpu,
            Self::Cpu => VideoPipeline::Cpu,
        }
    }
}

/// The tier-dependent choices a stream starts with, settled by creating the encoder.
struct VideoStart {
    video: VideoTier,
    chroma: ChromaMode,
    stream: (u32, u32),
    encoder_max: (u32, u32),
    ctl: RateController,
    settings: EncoderSettings,
    encoder: VideoEncoder,
}

impl VideoStart {
    fn open(video: VideoTier, caps: &ClientCaps, output: (u32, u32), fps: u32) -> io::Result<Self> {
        // The CPU tier defaults to one 4:2:0 stream: dual-stream 4:4:4 doubles the encode work.
        let prefer_single = matches!(video, VideoTier::Cpu);
        let chroma = if !caps.chroma.contains(&ChromaMode::Dual420)
            || (prefer_single && caps.chroma.contains(&ChromaMode::Single420))
        {
            ChromaMode::Single420
        } else {
            ChromaMode::Dual420
        };
        let encoder_max = within_home(video.encoder_max()?, caps);
        let stream = EncoderSettings::fit_extent(output.0, output.1, encoder_max);
        let streams = if chroma == ChromaMode::Dual420 { 2 } else { 1 };
        let ctl = RateController::new(stream.0 as u64 * stream.1 as u64, streams, MAX_FPS, None);
        let settings = encoder_settings(
            video.is_gpu(),
            stream,
            ctl.stream_bitrate(),
            fps,
            ctl.vbv_ms(fps),
        );
        let encoder = VideoEncoder::new(&video, &settings, chroma == ChromaMode::Dual420)?;
        Ok(Self {
            video,
            chroma,
            stream,
            encoder_max,
            ctl,
            settings,
            encoder,
        })
    }
}

enum VideoEncoder {
    Gpu(Box<Encoder>),
    Cpu(Box<gliff_sw::Encoder>),
}

impl VideoEncoder {
    fn new(tier: &VideoTier, settings: &EncoderSettings, dual: bool) -> io::Result<Self> {
        match tier {
            VideoTier::Gpu(gpu) => Ok(Self::Gpu(Box::new(
                Encoder::new(gpu, settings.clone(), dual).map_err(|e| other("GPU encoder", e))?,
            ))),
            VideoTier::Cpu => Ok(Self::Cpu(Box::new(
                gliff_sw::Encoder::new(
                    gliff_sw::EncoderSettings {
                        width: settings.width,
                        height: settings.height,
                        bitrate: settings.bitrate,
                        framerate: settings.framerate,
                    },
                    dual,
                )
                .map_err(|e| other("CPU encoder", e))?,
            ))),
        }
    }

    /// The CPU encoder cannot change its rate live (each change costs a keyframe), so small
    /// moves are skipped until they add up to 10%.
    fn set_rate(&mut self, bitrate: u32, framerate: u32, vbv_ms: u32) {
        match self {
            Self::Gpu(enc) => enc.set_rate(bitrate, framerate, vbv_ms),
            Self::Cpu(enc) => {
                let s = enc.settings();
                let moved = (bitrate as f64 - s.bitrate as f64).abs() / s.bitrate.max(1) as f64;
                if moved >= 0.10 || framerate != s.framerate {
                    enc.set_rate(bitrate, framerate);
                }
            }
        }
    }

    /// The GPU imports the dmabuf and scales in its split shader; the CPU maps the buffer and
    /// scales the pixels before converting.
    fn encode(
        &mut self,
        frame: &Frame,
        stream: (u32, u32),
        force_keyframe: bool,
    ) -> io::Result<EncodedFrame> {
        match (self, frame) {
            (Self::Gpu(enc), Frame::Captured(frame)) => {
                let info = &frame.buffer.info;
                let fourcc = drm_fourcc::DrmFourcc::try_from(info.fourcc).map_err(|_| {
                    io::Error::other(format!(
                        "capture fourcc {:#x} is not a DRM format",
                        info.fourcc
                    ))
                })?;
                let plane = DmabufPlane {
                    fd: info.fd.as_fd(),
                    width: info.width,
                    height: info.height,
                    offset: info.planes[0].offset,
                    stride: info.planes[0].stride,
                    fourcc,
                    modifier: info.modifier,
                };
                // The import is cached per ring buffer; the generation changes when the ring
                // is reallocated, so old imports are never reused.
                let buffer_key = (frame.buffer.generation << 32) | frame.buffer.index as u64;
                enc.encode_dmabuf(buffer_key, &plane, force_keyframe)
                    .map_err(|e| other("GPU encode", e))
            }
            (Self::Cpu(enc), Frame::Captured(frame)) => {
                let image = frame
                    .buffer
                    .read_bgra()
                    .map_err(|e| other("read the captured frame", e))?;
                let (w, h) = (stream.0 as usize, stream.1 as usize);
                let pixels = if (image.width, image.height) == (w, h) {
                    image.pixels
                } else {
                    gliff_proto::color::downscale_bgra(
                        &image.pixels,
                        image.width,
                        image.height,
                        w,
                        h,
                    )
                };
                encode_on_cpu(enc, &pixels, force_keyframe)
            }
            #[cfg(test)]
            (Self::Cpu(enc), Frame::Pixels { bgra, .. }) => {
                encode_on_cpu(enc, bgra, force_keyframe)
            }
            #[cfg(test)]
            (Self::Gpu(_), Frame::Pixels { .. }) => {
                Err(io::Error::other("test pixels are encoded on the CPU"))
            }
        }
    }
}

fn encode_on_cpu(
    enc: &mut gliff_sw::Encoder,
    bgra: &[u8],
    force_keyframe: bool,
) -> io::Result<EncodedFrame> {
    let f = enc
        .encode_bgra(bgra, force_keyframe)
        .map_err(|e| other("CPU encode", e))?;
    Ok(EncodedFrame {
        main: f.main,
        aux: f.aux,
        keyframe: f.keyframe,
    })
}

/// The encoder's largest picture, no larger than home said it decodes.
fn within_home(max: (u32, u32), caps: &ClientCaps) -> (u32, u32) {
    (max.0.min(caps.max_width), max.1.min(caps.max_height))
}

fn start_capture(
    output: &str,
    render_node: &std::path::Path,
    tx: UnboundedSender<Incoming>,
) -> io::Result<Capturer> {
    let mut config = CaptureConfig::new(output.to_string());
    // The running Hyprland's display, not the one inherited from the environment.
    config.target.display = Some(
        crate::hypr::instance()?
            .wayland_display()
            .map_err(|e| other("capture", e))?,
    );
    config.render_node = render_node.to_path_buf();
    // The cursor's image goes home on its own, for home's compositor to draw at home's pointer,
    // as gliff does: with no delay, and never in the picture.
    config.cursor = true;
    // One extra ring slot: the session pins the last sent frame for refinement.
    config.buffers = 4;
    let mut visible = None;
    let sink = Box::new(move |ev: CaptureEvent| {
        if let Some(msg) = captured(ev, &mut visible) {
            let _ = tx.send(msg);
        }
    });
    Capturer::start(config, sink).map_err(|e| other("capture", e))
}

/// What the stream hears of a capture event. Of the cursor's moves, only its visibility goes
/// on, and only when it changes (`visible` keeps the last): home draws the cursor at home's
/// pointer.
fn captured(ev: CaptureEvent, visible: &mut Option<bool>) -> Option<Incoming> {
    Some(match ev {
        CaptureEvent::Frame(frame) => Incoming::Frame(Frame::Captured(frame)),
        CaptureEvent::CursorShape {
            width,
            height,
            hot_x,
            hot_y,
            argb,
        } => Incoming::Cursor {
            width,
            height,
            hot_x,
            hot_y,
            argb,
        },
        CaptureEvent::CursorPos { visible: shown, .. } => {
            if visible.replace(shown) == Some(shown) {
                return None;
            }
            Incoming::CursorVisible(shown)
        }
        CaptureEvent::Stopped => Incoming::Stopped,
        CaptureEvent::Error(e) => Incoming::Error(e),
        CaptureEvent::Ready { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(max_width: u32, max_height: u32) -> ClientCaps {
        ClientCaps {
            codecs: vec![Codec::H264],
            max_width,
            max_height,
            chroma: vec![ChromaMode::Single420],
            features: Vec::new(),
        }
    }

    #[test]
    fn the_cursor_s_visibility_goes_home_when_it_changes() {
        let mut visible = None;
        let mut seen = |shown| match captured(
            CaptureEvent::CursorPos {
                x: 1,
                y: 2,
                visible: shown,
            },
            &mut visible,
        ) {
            Some(Incoming::CursorVisible(shown)) => Some(shown),
            None => None,
            Some(_) => panic!("not the cursor's visibility"),
        };
        assert_eq!(seen(true), Some(true));
        // A move with the cursor still shown is not news: home draws it at its own pointer.
        assert_eq!(seen(true), None);
        assert_eq!(seen(false), Some(false));
        assert_eq!(seen(false), None);
        assert_eq!(seen(true), Some(true));
    }

    #[test]
    fn the_largest_picture_home_decodes_bounds_the_stream() {
        assert_eq!(within_home((4096, 4096), &caps(3840, 2160)), (3840, 2160));
        assert_eq!(within_home((1920, 1088), &caps(3840, 2160)), (1920, 1088));
    }

    /// The first re-encode of a still picture is small whatever the encoder: at the quantiser
    /// the picture was coded with, the residual quantises to nothing; only the next passes,
    /// once the rate control has seen a pass come in under budget, sharpen it. None of the
    /// passes may be skipped on that ground.
    #[test]
    fn still_text_gets_every_refinement_pass() {
        let mut desk = Desk::new(640, 360);
        desk.show(text_page(640, 360, 1));
        assert_eq!(desk.frames(12), 1 + MAX_REFINES as usize);
    }

    /// The controller's target only grows on what motion measured: on a page left still from
    /// the start it stays at its starting value, a fraction of what a LAN carries. The
    /// controller hears of no ack here, so the target cannot move.
    #[test]
    fn still_text_is_refined_at_the_ceiling_not_the_starting_target() {
        let mut desk = Desk::new(1280, 720);
        desk.acks = false;
        desk.show(text_page(1280, 720, 1));
        desk.frames(1);
        assert!(desk.session.ctl.ceiling_bitrate() > desk.session.ctl.stream_bitrate());
        assert_eq!(
            desk.session.settings.bitrate,
            desk.session.ctl.stream_bitrate()
        );
        desk.frames(1);
        assert_eq!(
            desk.session.settings.bitrate,
            desk.session.ctl.ceiling_bitrate()
        );
    }

    /// Refinement must not leak into motion: the next change is encoded at the target again.
    #[test]
    fn motion_after_still_text_is_encoded_at_the_target() {
        let mut desk = Desk::new(1280, 720);
        desk.acks = false;
        desk.show(text_page(1280, 720, 1));
        assert_eq!(desk.frames(2), 2);
        desk.show(text_page(1280, 720, 2));
        assert_eq!(desk.frames(1), 1);
        assert!(desk.session.ctl.ceiling_bitrate() > desk.session.ctl.stream_bitrate());
        assert_eq!(
            desk.session.settings.bitrate,
            desk.session.ctl.stream_bitrate()
        );
    }

    #[test]
    fn the_gpu_encoder_is_given_a_quantiser_ceiling_of_36() {
        let settings = encoder_settings(true, (1280, 720), 3_000_000, 30, 100);
        assert_eq!(settings.max_qp, Some(36));
    }

    #[test]
    fn the_cpu_encoder_is_given_no_quantiser_ceiling() {
        let settings = encoder_settings(false, (1280, 720), 3_000_000, 30, 100);
        assert_eq!(settings.max_qp, None);
    }

    #[test]
    fn during_refinement_the_gpu_encoder_is_told_the_passes_pace() {
        assert_eq!(encoder_framerate(true, true, 30), 5);
        assert_eq!(encoder_framerate(true, true, 60), 5);
    }

    #[test]
    fn when_the_picture_moves_the_gpu_encoder_is_told_the_command_rate_again() {
        assert_eq!(encoder_framerate(true, false, 30), 30);
    }

    #[test]
    fn the_cpu_encoder_keeps_the_command_rate_during_refinement() {
        assert_eq!(encoder_framerate(false, true, 30), 30);
        assert_eq!(encoder_framerate(false, false, 30), 30);
    }

    /// The CPU session reaches the same decision through its settings, whatever the pass.
    #[test]
    fn a_cpu_session_keeps_the_command_rate_while_it_refines() {
        let mut desk = Desk::new(1280, 720);
        desk.acks = false;
        desk.show(text_page(1280, 720, 1));
        desk.frames(2);
        assert!(desk.session.refining);
        assert_eq!(desk.session.settings.framerate, desk.session.fps_cmd);
    }

    /// A session on the CPU tier, stepped by hand: no clock to wait for, every frame acked at
    /// once.
    struct Desk {
        session: Session,
        scene: Scene,
        captures: UnboundedReceiver<Incoming>,
        outbox: Outbox,
        runtime: tokio::runtime::Runtime,
        /// The rate controller hears of home's acks; when not, its target stays where it
        /// started.
        acks: bool,
    }

    impl Desk {
        fn new(width: u32, height: u32) -> Self {
            let outbox = Outbox::default();
            let (tx, captures) = mpsc::unbounded_channel();
            let scene = Scene::new(tx, (width, height));
            let session = Session::new(
                Source {
                    output: "NAVETTE-1".into(),
                    scale: 1.0,
                },
                dual_caps(),
                outbox.clone(),
                (width, height),
                VideoTier::Cpu,
                Box::new(scene.clone()),
            )
            .expect("the CPU session starts");
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            Self {
                session,
                scene,
                captures,
                outbox,
                runtime,
                acks: true,
            }
        }

        fn show(&mut self, page: Vec<u8>) {
            self.scene.show(Arc::new(page));
            self.session.capturer.request_frame();
            self.session.capture_asked = true;
        }

        /// Lets the session send up to `max` frames, refinement passes included, as if each
        /// were due; returns how many it sent. Home takes each one off the link at once.
        fn frames(&mut self, max: usize) -> usize {
            let mut sent = 0;
            while sent < max {
                while let Ok(Incoming::Frame(frame)) = self.captures.try_recv() {
                    self.session.on_frame(frame).unwrap();
                }
                self.session.next_send_at = Instant::now();
                self.session.next_refine_at = Instant::now();
                let before = self.session.frame_id;
                self.session.pump_encoder().unwrap();
                if self.session.frame_id == before {
                    break;
                }
                sent += 1;
                while let Some(frame_id) = self.next_frame() {
                    if !self.acks {
                        self.session.link.on_ack(Instant::now(), frame_id);
                        continue;
                    }
                    self.session
                        .on_feedback(ClientMsg::FrameAck {
                            frame_id,
                            decoded_at_ms: now_us() / 1000,
                        })
                        .unwrap();
                }
            }
            sent
        }

        fn next_frame(&self) -> Option<u64> {
            while !self.outbox.queued().is_empty() {
                if let (_, Msg::Video(ServerMsg::VideoFrame { frame_id, .. }), _) =
                    self.runtime.block_on(self.outbox.next())
                {
                    return Some(frame_id);
                }
            }
            None
        }
    }

    fn dual_caps() -> ClientCaps {
        ClientCaps {
            chroma: vec![ChromaMode::Dual420],
            ..caps(4096, 4096)
        }
    }

    /// The headless measure behind the numbers in the pull request: a page left still, then
    /// scrolling followed by a still page. `NAVETTE_PAGE` names a binary PPM to use as the page
    /// (`tools/still-text-page.sh` renders one); without it a synthetic page is used.
    /// `cargo test --release --lib still_text_measure -- --ignored --nocapture`
    #[test]
    #[ignore = "a measure, not a check: minutes of CPU encoding"]
    fn still_text_measure() {
        let (width, height, page) = match std::env::var_os("NAVETTE_PAGE") {
            Some(path) => read_ppm(&std::fs::read(path).expect("read the page")),
            None => (2560, 1440, text_page(2560, 1440, 1)),
        };
        let page = Arc::new(page);
        let shown = page.clone();
        let still = measure(width, height, Lan::HOME, |scene| async move {
            scene.show(shown);
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        println!("still page from the start:\n{}", timeline(&still));

        let scroll = Duration::from_secs(4);
        let arrivals = measure(width, height, Lan::HOME, move |scene| async move {
            let start = Instant::now();
            let mut row = 0;
            while start.elapsed() < scroll {
                scene.show_moving(Arc::new(scrolled(&page, width, height, row)));
                row += 3;
                tokio::time::sleep(Duration::from_millis(16)).await;
            }
            scene.show(Arc::new(scrolled(&page, width, height, row)));
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        let moving: Vec<_> = arrivals.iter().filter(|a| a.moving).collect();
        let mut latency: Vec<_> = moving.iter().map(|a| a.latency).collect();
        latency.sort();
        let span = moving.last().unwrap().at - moving.first().unwrap().at;
        println!(
            "scrolling: {} frames, {:.1} fps, latency p50 {:?} p95 {:?}",
            moving.len(),
            moving.len() as f64 / span.as_secs_f64(),
            latency[latency.len() / 2],
            latency[latency.len() * 95 / 100],
        );
        let settled: Vec<_> = arrivals.into_iter().filter(|a| !a.moving).collect();
        println!("still page after scrolling:\n{}", timeline(&settled));
    }

    /// A LAN between the two machines, in one direction.
    #[derive(Clone, Copy)]
    struct Lan {
        bits_per_second: f64,
        one_way: Duration,
    }

    impl Lan {
        const HOME: Lan = Lan {
            bits_per_second: 300e6,
            one_way: Duration::from_millis(1),
        };

        fn transmit(&self, bytes: usize) -> Duration {
            Duration::from_secs_f64(bytes as f64 * 8.0 / self.bits_per_second)
        }
    }

    /// One frame decoded at home.
    struct Arrival {
        at: Instant,
        /// Since the picture last changed, when the frame arrived.
        since_change: Duration,
        /// The picture was still changing when the frame arrived.
        moving: bool,
        bytes: usize,
        latency: Duration,
        /// Against the picture shown last, RGB.
        psnr: f64,
    }

    fn timeline(arrivals: &[Arrival]) -> String {
        arrivals
            .iter()
            .map(|a| {
                format!(
                    "  +{:>4} ms  {:>6.1} dB  {:>7} B",
                    a.since_change.as_millis(),
                    a.psnr,
                    a.bytes
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The pictures a fake capture delivers, as `hypr-capture` does: one frame per request,
    /// once the picture has changed since the last one.
    #[derive(Clone)]
    struct Scene(Arc<std::sync::Mutex<SceneState>>);

    struct SceneState {
        tx: UnboundedSender<Incoming>,
        size: (u32, u32),
        picture: Arc<Vec<u8>>,
        changed_at: Instant,
        changing: bool,
        asked: bool,
        damaged: bool,
    }

    impl SceneState {
        fn deliver(&mut self) {
            self.asked = false;
            self.damaged = false;
            let _ = self.tx.send(Incoming::Frame(Frame::Pixels {
                width: self.size.0,
                height: self.size.1,
                bgra: self.picture.clone(),
            }));
        }
    }

    impl Scene {
        fn new(tx: UnboundedSender<Incoming>, size: (u32, u32)) -> Self {
            Self(Arc::new(std::sync::Mutex::new(SceneState {
                tx,
                size,
                picture: Arc::new(vec![0; size.0 as usize * size.1 as usize * 4]),
                changed_at: Instant::now(),
                changing: false,
                asked: false,
                damaged: false,
            })))
        }

        /// Shows `picture`, to stay.
        fn show(&self, picture: Arc<Vec<u8>>) {
            self.change(picture, false);
        }

        /// Shows `picture`, one of a motion.
        fn show_moving(&self, picture: Arc<Vec<u8>>) {
            self.change(picture, true);
        }

        fn change(&self, picture: Arc<Vec<u8>>, changing: bool) {
            let mut state = self.0.lock().unwrap();
            state.picture = picture;
            state.changing = changing;
            state.changed_at = Instant::now();
            state.damaged = true;
            if state.asked {
                state.deliver();
            }
        }

        fn last_change(&self) -> (Arc<Vec<u8>>, Instant, bool) {
            let state = self.0.lock().unwrap();
            (state.picture.clone(), state.changed_at, state.changing)
        }
    }

    impl Capture for Scene {
        fn request_frame(&mut self) {
            let mut state = self.0.lock().unwrap();
            state.asked = true;
            if state.damaged {
                state.deliver();
            }
        }

        fn recapture(&mut self) {
            self.0.lock().unwrap().deliver();
        }
    }

    /// Runs a real session on the CPU tier against a fake capture driven by `script`, over a
    /// simulated `lan` to a home that decodes on the CPU and acks every frame. Returns every
    /// frame decoded while the script ran. Dual420, as the remote's GPU tier encodes.
    fn measure<S, F>(width: u32, height: u32, lan: Lan, script: S) -> Vec<Arrival>
    where
        S: FnOnce(Scene) -> F,
        F: std::future::Future<Output = ()>,
    {
        let outbox = Outbox::default();
        let (cap_tx, captures) = mpsc::unbounded_channel();
        let scene = Scene::new(cap_tx, (width, height));
        let (feedback_tx, feedback) = mpsc::unbounded_channel();
        let (stop_tx, stop) = tokio::sync::oneshot::channel();
        let session_outbox = outbox.clone();
        let capture = scene.clone();
        let session = std::thread::spawn(move || {
            let source = Source {
                output: "NAVETTE-1".into(),
                scale: 1.0,
            };
            let session = Session::new(
                source,
                dual_caps(),
                session_outbox,
                (width, height),
                VideoTier::Cpu,
                Box::new(capture),
            )
            .expect("the CPU session starts");
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(session.run(captures, feedback, stop))
        });

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let arrivals = Arc::new(std::sync::Mutex::new(Vec::new()));
        runtime.block_on(async {
            let home = home(
                outbox.clone(),
                feedback_tx,
                scene.clone(),
                lan,
                arrivals.clone(),
            );
            let script = async {
                script(scene.clone()).await;
            };
            tokio::select! {
                _ = script => {}
                _ = home => unreachable!("home runs until the script ends"),
            }
        });
        let _ = stop_tx.send(());
        session.join().unwrap().expect("the session ran");
        std::mem::take(&mut *arrivals.lock().unwrap())
    }

    /// Home: writes each frame at the link's rate, decodes it and acks it.
    async fn home(
        outbox: Outbox,
        feedback: UnboundedSender<ClientMsg>,
        scene: Scene,
        lan: Lan,
        arrivals: Arc<std::sync::Mutex<Vec<Arrival>>>,
    ) {
        let mut decoder = None;
        let mut config = None;
        loop {
            let (_, msg, payloads) = outbox.next().await;
            match msg {
                Msg::Video(ServerMsg::Ping { t }) => {
                    tokio::time::sleep(2 * lan.one_way).await;
                    let _ = feedback.send(ClientMsg::Pong { t });
                }
                Msg::Video(ServerMsg::StreamConfig {
                    chroma,
                    width,
                    height,
                    ..
                }) => {
                    // As home does: a pace-only change keeps the decoder.
                    if config != Some((chroma, width, height)) {
                        config = Some((chroma, width, height));
                        decoder =
                            Some(gliff_sw::Decoder::new(chroma == ChromaMode::Dual420).unwrap());
                    }
                }
                Msg::Video(ServerMsg::VideoFrame {
                    frame_id, pts_us, ..
                }) => {
                    let bytes = payloads.iter().map(Vec::len).sum();
                    let took = lan.transmit(bytes);
                    tokio::time::sleep(took).await;
                    outbox.report_write(0, took);
                    tokio::time::sleep(lan.one_way).await;
                    let decoded = decoder
                        .as_mut()
                        .expect("a config before the frames")
                        .decode(&payloads[0], &payloads[1])
                        .expect("home decodes the frame")
                        .expect("no picture held back");
                    let at = Instant::now();
                    let (picture, changed_at, changing) = scene.last_change();
                    arrivals.lock().unwrap().push(Arrival {
                        at,
                        since_change: at - changed_at,
                        moving: changing,
                        bytes,
                        latency: Duration::from_micros(now_us().saturating_sub(pts_us)),
                        psnr: gliff_proto::color::psnr(&rgb(&picture), &rgb(&decoded.pixels)),
                    });
                    let feedback = feedback.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(lan.one_way).await;
                        let _ = feedback.send(ClientMsg::FrameAck {
                            frame_id,
                            decoded_at_ms: now_us() / 1000,
                        });
                    });
                }
                _ => {}
            }
        }
    }

    fn rgb(bgra: &[u8]) -> Vec<u8> {
        bgra.as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect()
    }

    /// A deterministic page of small text: glyph-like antialiased strokes, coloured code on a
    /// dark panel and dark prose on a white one, with a bar along the top.
    fn text_page(width: u32, height: u32, seed: u32) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let mut page = vec![0u8; w * h * 4];
        let mut fill = |x0: usize, y0: usize, x1: usize, y1: usize, rgb: [u8; 3]| {
            for y in y0..y1.min(h) {
                for x in x0..x1.min(w) {
                    page[(y * w + x) * 4..][..4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
                }
            }
        };
        fill(0, 0, w, h, [0x1e, 0x1e, 0x2e]);
        fill(0, 0, w, 18, [0x1a, 0x1b, 0x26]);
        fill(8, 26, w / 2 - 4, h - 8, [0x28, 0x2c, 0x34]);
        fill(w / 2 + 4, 26, w - 8, h - 8, [0xff, 0xff, 0xff]);
        let code = [
            [0xe0, 0x6c, 0x75],
            [0x98, 0xc3, 0x79],
            [0xe5, 0xc0, 0x7b],
            [0x61, 0xaf, 0xef],
            [0xc6, 0x78, 0xdd],
            [0xab, 0xb2, 0xbf],
        ];
        let mut rng = seed.wrapping_mul(2_654_435_761).max(1);
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng
        };
        let panels = [
            (16, 34, w / 2 - 12, h - 16, None, [0x28, 0x2c, 0x34]),
            (
                w / 2 + 16,
                36,
                w - 20,
                h - 16,
                Some([0x20, 0x20, 0x20]),
                [0xff; 3],
            ),
            (12, 4, w - 12, 16, None, [0x1a, 0x1b, 0x26]),
        ];
        for (x0, y0, x1, y1, ink, paper) in panels {
            let mut y = y0;
            while y + 10 <= y1 {
                let mut x = x0;
                while x + 6 <= x1 {
                    let letters = 2 + next() as usize % 7;
                    let color = ink.unwrap_or(code[next() as usize % code.len()]);
                    for _ in 0..letters {
                        if x + 6 > x1 {
                            break;
                        }
                        glyph(&mut page, w, x, y, next(), color, paper);
                        x += 7;
                    }
                    x += 7;
                }
                y += 14;
            }
        }
        page
    }

    /// One 6x9 glyph from `bits`: a few strokes, with grey edges as a rasteriser leaves them.
    fn glyph(
        page: &mut [u8],
        w: usize,
        x0: usize,
        y0: usize,
        bits: u32,
        ink: [u8; 3],
        paper: [u8; 3],
    ) {
        let mut cover = [[0f32; 6]; 9];
        let strokes: [&dyn Fn(usize, usize) -> bool; 6] = [
            &|x, _| x == 1,
            &|x, _| x == 4,
            &|_, y| y == 1,
            &|_, y| y == 4,
            &|_, y| y == 7,
            &|x, y| x + 1 == y * 2 / 3 + 1,
        ];
        for (i, stroke) in strokes.iter().enumerate() {
            if bits >> i & 1 == 1 {
                for (y, row) in cover.iter_mut().enumerate() {
                    for (x, c) in row.iter_mut().enumerate() {
                        if stroke(x, y) {
                            *c = 1.0;
                        }
                    }
                }
            }
        }
        for y in 0..9 {
            for x in 0..6 {
                let near = [(0, 1), (2, 1), (1, 0), (1, 2)]
                    .iter()
                    .filter_map(|(dx, dy)| {
                        let (nx, ny) = ((x + dx) as isize - 1, (y + dy) as isize - 1);
                        (nx >= 0 && ny >= 0 && nx < 6 && ny < 9)
                            .then(|| cover[ny as usize][nx as usize])
                    })
                    .fold(0f32, f32::max);
                let c = cover[y][x].max(0.35 * near);
                let px = &mut page[((y0 + y) * w + x0 + x) * 4..][..3];
                for k in 0..3 {
                    px[2 - k] =
                        (paper[k] as f32 + (ink[k] as f32 - paper[k] as f32) * c).round() as u8;
                }
            }
        }
    }

    /// The page scrolled up by `rows`, wrapping.
    fn scrolled(page: &[u8], width: u32, height: u32, rows: usize) -> Vec<u8> {
        let stride = width as usize * 4;
        let shift = (rows % height as usize) * stride;
        [&page[shift..], &page[..shift]].concat()
    }

    /// A binary PPM (P6, 8-bit), as BGRA.
    fn read_ppm(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut fields = bytes.splitn(4, |b| b.is_ascii_whitespace());
        assert_eq!(fields.next(), Some(&b"P6"[..]), "a binary PPM");
        let mut number = || -> usize {
            std::str::from_utf8(fields.next().unwrap())
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        let (width, height) = (number(), number());
        let rest = fields.next().unwrap();
        let mut rest = rest.splitn(2, |b| b.is_ascii_whitespace());
        assert_eq!(rest.next(), Some(&b"255"[..]), "8-bit");
        let rgb = rest.next().unwrap();
        let (w, h) = (width & !1, height & !1);
        let mut bgra = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for p in rgb[y * width * 3..][..w * 3].as_chunks::<3>().0 {
                bgra.extend_from_slice(&[p[2], p[1], p[0], 255]);
            }
        }
        (w as u32, h as u32, bgra)
    }
}
