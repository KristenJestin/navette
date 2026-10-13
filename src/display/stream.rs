//! The remote's picture path for one output: capture it (`hypr-capture`), encode it (gliff-vk
//! and VA-API on the GPU, Dual420 as gliff does; gliff-sw on the CPU when the GPU cannot),
//! pace it and adapt the rate to the link, and send it home.
//!
//! Adapted from gliff-server's `session.rs` (gliff, MIT, see `LICENSES/gliff-MIT.txt`): the
//! capture, pacing, refinement, ladder and fallback logic is gliff's. What Navette drops: the
//! handshake (the display's `Start` order replaces it), input and clipboard (their own issues),
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
use crate::wire::{Msg, Outbox};

/// The session-wide fps ceiling; ladder levels cap below it.
const MAX_FPS: u32 = 60;
/// Still-picture refinement: at most this many re-encodes of an unchanged frame, at least
/// this far apart.
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
    Frame(CapturedFrame),
    Stopped,
    Error(String),
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
    capturer: Capturer,
    /// The newest captured frame not yet encoded.
    pending: Option<CapturedFrame>,
    capture_asked: bool,
    /// The pending frame has already counted as blocked for the rate controller.
    blocked_noted: bool,
    frame_id: u64,
    want_keyframe: bool,
    /// The one commanded cadence: the pace timer and the encoder's frame rate both use it.
    fps_cmd: u32,
    /// Smoothed encode time, for the sustainable-fps clamp.
    encode_us: f64,
    next_send_at: Instant,
    /// The last frame that went out, kept for still-picture refinement.
    last_frame: Option<CapturedFrame>,
    refines: u8,
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
                "home decodes no codec this machine encodes",
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
                "home decodes no chroma mode this machine encodes",
            ));
        }
        let video = VideoTier::open(VideoMode::resolve(None), &render_node);
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
        let (cap_tx, captures) = mpsc::unbounded_channel();
        let capturer = start_capture(&source.output, &render_node, cap_tx)?;
        let session = Self {
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
            want_keyframe: true,
            fps_cmd,
            encode_us: 0.0,
            next_send_at: Instant::now(),
            last_frame: None,
            refines: MAX_REFINES,
            next_refine_at: Instant::now(),
            recaptured: false,
            ladder,
            base_stream: start.stream,
            base_chroma: start.chroma,
        };
        Ok((session, captures))
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
        self.capturer.request_frame().ok();
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
                    self.capturer.recapture().ok();
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

    fn on_frame(&mut self, frame: CapturedFrame) -> io::Result<()> {
        let size = (frame.buffer.info.width & !1, frame.buffer.info.height & !1);
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
                let bytes = self.encode_and_send(&frame, true)?;
                self.refines += 1;
                self.next_refine_at = now + REFINE_EVERY;
                if (bytes as f64) < self.refined_done_bytes() {
                    self.refines = MAX_REFINES;
                }
                self.last_frame = Some(frame);
            }
        }
        // Keep one capture request outstanding even while blocked: a newer frame replaces
        // `pending`, so the frame that finally goes out on a slow link is current.
        if !self.capture_asked {
            self.capturer.request_frame().ok();
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

    fn matches_output(&self, frame: &CapturedFrame) -> bool {
        let info = &frame.buffer.info;
        (info.width & !1, info.height & !1) == self.output
    }

    fn refined_done_bytes(&self) -> f64 {
        let streams = if self.chroma == ChromaMode::Dual420 {
            2.0
        } else {
            1.0
        };
        0.2 * self.ctl.budget_per_frame(self.fps_cmd) as f64 * streams / 8.0
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
        let settings = EncoderSettings {
            width: want.0,
            height: want.1,
            bitrate: self.ctl.stream_bitrate(),
            framerate: self.fps_cmd,
            vbv_ms: self.ctl.vbv_ms(self.fps_cmd),
        };
        match VideoEncoder::new(&self.video, &settings, chroma == ChromaMode::Dual420) {
            Ok(encoder) => {
                self.encoder = encoder;
                self.settings = settings;
                self.stream = want;
                self.chroma = chroma;
                self.want_keyframe = true;
                self.encode_us = 0.0;
                self.refines = 0;
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

    fn apply_rate(&mut self) {
        let bitrate = self.ctl.stream_bitrate();
        let vbv = self.ctl.vbv_ms(self.fps_cmd);
        self.settings.bitrate = bitrate;
        self.settings.framerate = self.fps_cmd;
        self.settings.vbv_ms = vbv;
        self.encoder.set_rate(bitrate, self.fps_cmd, vbv);
    }

    /// Encodes `frame` and queues it; returns the coded byte count. `app_limited` marks a send
    /// that does not use the capacity (a refinement pass), so it cannot lower the estimate.
    fn encode_and_send(&mut self, frame: &CapturedFrame, app_limited: bool) -> io::Result<usize> {
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
                self.encoder_max = gliff_sw::Encoder::MAX_SIZE;
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

/// Vulkan compute and VA-API on the GPU, or OpenH264 on the CPU.
enum VideoTier {
    Gpu(Arc<Gpu>),
    Cpu,
}

impl VideoTier {
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
        let encoder_max = video.encoder_max()?;
        let stream = EncoderSettings::fit_extent(output.0, output.1, encoder_max);
        let streams = if chroma == ChromaMode::Dual420 { 2 } else { 1 };
        let ctl = RateController::new(stream.0 as u64 * stream.1 as u64, streams, MAX_FPS, None);
        let settings = EncoderSettings {
            width: stream.0,
            height: stream.1,
            bitrate: ctl.stream_bitrate(),
            framerate: fps,
            vbv_ms: ctl.vbv_ms(fps),
        };
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
        frame: &CapturedFrame,
        stream: (u32, u32),
        force_keyframe: bool,
    ) -> io::Result<EncodedFrame> {
        match self {
            Self::Gpu(enc) => {
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
            Self::Cpu(enc) => {
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
                let f = enc
                    .encode_bgra(&pixels, force_keyframe)
                    .map_err(|e| other("CPU encode", e))?;
                Ok(EncodedFrame {
                    main: f.main,
                    aux: f.aux,
                    keyframe: f.keyframe,
                })
            }
        }
    }
}

fn start_capture(
    output: &str,
    render_node: &std::path::Path,
    tx: UnboundedSender<Incoming>,
) -> io::Result<Capturer> {
    let mut config = CaptureConfig::new(output.to_string());
    config.render_node = render_node.to_path_buf();
    // The pointer is home's own, drawn by home's compositor.
    config.cursor = false;
    // One extra ring slot: the session pins the last sent frame for refinement.
    config.buffers = 4;
    let sink = Box::new(move |ev: CaptureEvent| {
        let msg = match ev {
            CaptureEvent::Frame(frame) => Incoming::Frame(frame),
            CaptureEvent::Stopped => Incoming::Stopped,
            CaptureEvent::Error(e) => Incoming::Error(e),
            _ => return,
        };
        let _ = tx.send(msg);
    });
    Capturer::start(config, sink).map_err(|e| other("capture", e))
}
