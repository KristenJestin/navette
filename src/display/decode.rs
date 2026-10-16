//! The desk's decode: on the GPU (VA-API and gliff-vk's recombine, the picture handed over as a
//! dmabuf with no copy, in a layout the compositor takes), or on the CPU (gliff-sw) when the GPU cannot. Runs on its own
//! thread, acks every frame so the remote keeps pacing, and measures what it does.
//!
//! Adapted from gliff's client decode path, `crates/gliff/src/net.rs` (gliff, MIT, see
//! `LICENSES/gliff-MIT.txt`): the tier choice, the fallback and the idle drain of the CPU
//! decoder are gliff's; the GTK hand-off becomes a plain callback.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use gliff_proto::{ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg, features};
use gliff_sw::VideoMode;
use gliff_vk::{Decoder, DisplayFrame, Gpu};

use super::Ended;
use super::wire::{Msg, Outbox};

/// `DRM_FORMAT_MOD_LINEAR`.
const LINEAR: u64 = 0;

/// The CPU decoder holds the newest picture of a High-profile stream until the next access
/// unit; after this long without one it is drained, so a still screen shows its last state.
const IDLE_DRAIN: Duration = Duration::from_millis(150);
/// How often the numbers are written to the log.
const REPORT_EVERY: Duration = Duration::from_secs(10);
/// Frames waiting for the decoder; past this, the desk has fallen behind and drops frames until a
/// keyframe rather than queue them without end.
const QUEUE: usize = 4;
/// Decodes failing in a row before a decoder is given up for a new one; a failed keyframe
/// gives it up at once, as nothing before it can explain the failure.
const BROKEN: u32 = 3;

/// A decoded picture, for the surface.
pub enum Picture {
    /// The GPU's output image; dropping it gives the image back to the decoder.
    Dmabuf(DisplayFrame),
    Bgra(gliff_sw::BgraFrame),
}

/// The GPU, when it can decode H.264; `GLIFF_VIDEO=cpu` forces the CPU.
pub fn open_gpu() -> Option<Arc<Gpu>> {
    if VideoMode::resolve(None) == VideoMode::Cpu {
        return None;
    }
    match Gpu::open(Some(&hypr_capture::render_node(None))) {
        Ok(gpu) if gpu.can_decode() => Some(gpu),
        Ok(gpu) => {
            eprintln!(
                "navette: {} has no VA-API H.264 decoder; decoding on the CPU",
                gpu.name
            );
            None
        }
        Err(e) => {
            eprintln!("navette: no usable GPU ({e}); decoding on the CPU");
            None
        }
    }
}

/// What the desk decodes. The CPU asks for one 4:2:0 stream, so it decodes one stream, not two.
pub fn caps(gpu: &Option<Arc<Gpu>>, max: (u32, u32)) -> ClientCaps {
    ClientCaps {
        codecs: vec![Codec::H264],
        max_width: max.0,
        max_height: max.1,
        chroma: if gpu.is_some() {
            vec![ChromaMode::Dual420, ChromaMode::Single420]
        } else {
            vec![ChromaMode::Single420]
        },
        features: features(),
    }
}

/// A gliff stream message with the bytes of a video frame.
pub struct Video {
    pub msg: ServerMsg,
    pub main: Bytes,
    pub aux: Bytes,
}

/// The decode thread; it ends when this is dropped.
pub struct Decoding {
    video: Option<Sender<Video>>,
    /// Frames sent to the thread and not taken yet.
    queued: Arc<AtomicUsize>,
    /// A frame was dropped: the next ones are too, until a keyframe.
    skipping: AtomicBool,
    outbox: Outbox,
    thread: Option<JoinHandle<()>>,
}

impl Decoding {
    /// `modifiers`: the layouts in which the surface takes the GPU's pictures as dmabufs; with
    /// none, they are read back, as they are once the surface sets `refused`. `ended` hears why
    /// decoding cannot go on; `lost` is set when a GPU decoder was lost, so that the GPU it
    /// came from is not used for the next mode.
    pub fn start(
        gpu: Option<Arc<Gpu>>,
        (modifiers, refused): (Vec<u64>, Arc<AtomicBool>),
        outbox: Outbox,
        show: Box<dyn FnMut(Picture) + Send>,
        (ended, lost): (Ended, Arc<AtomicBool>),
    ) -> std::io::Result<Self> {
        let handover = Handover::of(&modifiers);
        let real = Real {
            gpu,
            handover,
            refused,
        };
        Self::start_with(Box::new(real), outbox, show, (ended, lost))
    }

    fn start_with(
        decoders: Box<dyn Decoders>,
        outbox: Outbox,
        show: Box<dyn FnMut(Picture) + Send>,
        (ended, lost): (Ended, Arc<AtomicBool>),
    ) -> std::io::Result<Self> {
        let (video, rx) = std::sync::mpsc::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let thread = {
            let (outbox, queued) = (outbox.clone(), queued.clone());
            std::thread::Builder::new()
                .name("navette-decode".into())
                .spawn(move || {
                    let thread = Thread {
                        decoders,
                        on_gpu: true,
                        outbox,
                        queued,
                        ended,
                        lost,
                    };
                    thread.run(show, rx)
                })?
        };
        Ok(Self {
            video: Some(video),
            queued,
            skipping: AtomicBool::new(false),
            outbox,
            thread: Some(thread),
        })
    }

    /// Hands a message to the thread; a frame that would wait behind [`QUEUE`] others is
    /// dropped, and so are the next ones until a keyframe, which is asked for. A dropped frame is
    /// acked all the same: the remote paces on acks.
    pub fn video(&self, video: Video) {
        if let ServerMsg::VideoFrame {
            frame_id, keyframe, ..
        } = video.msg
        {
            let full = self.queued.load(Ordering::Relaxed) >= QUEUE;
            let skipping = self.skipping.load(Ordering::Relaxed);
            if full || (skipping && !keyframe) {
                if !skipping || keyframe {
                    self.outbox
                        .send(Msg::Feedback(ClientMsg::RequestKeyframe), Vec::new());
                }
                self.skipping.store(true, Ordering::Relaxed);
                self.outbox.send(
                    Msg::Feedback(ClientMsg::FrameAck {
                        frame_id,
                        decoded_at_ms: now_us() / 1000,
                    }),
                    Vec::new(),
                );
                return;
            }
            self.skipping.store(false, Ordering::Relaxed);
            self.queued.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(sender) = &self.video {
            let _ = sender.send(video);
        }
    }
}

impl Drop for Decoding {
    /// The thread is joined: once this returns, no picture is on its way to the surface.
    fn drop(&mut self) {
        self.video = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One stream's decoder (a port, so a test can make one that fails).
trait Decode {
    /// `GPU` or `CPU`, for the logs.
    fn tier(&self) -> &'static str;
    /// How its pictures reach the surface, for the logs.
    fn handover(&self) -> &'static str {
        "copied to the surface"
    }
    fn decode(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<Picture>, String>;
    /// The picture it still holds after an idle time, if any.
    fn drain(&mut self) -> Option<Picture> {
        None
    }
}

/// Makes the decoders (a port, so a test can make fakes).
trait Decoders: Send {
    /// A decoder on the GPU, none without a GPU. `fresh`: on the device opened again, the one
    /// before being lost.
    fn gpu(
        &mut self,
        fresh: bool,
        dual: bool,
        size: (u32, u32),
    ) -> Option<Result<Box<dyn Decode>, String>>;
    fn cpu(&mut self, dual: bool) -> Result<Box<dyn Decode>, String>;
}

/// How the GPU's pictures reach the surface.
#[derive(Debug, PartialEq)]
enum Handover {
    /// As linear dmabufs, when the compositor takes them: the layout every GPU writes.
    Linear,
    /// As dmabufs in one of the compositor's tiled layouts (Hyprland on NVIDIA lists only those).
    Tiled(Vec<u64>),
    /// Read back and copied to the surface.
    Copy,
}

impl Handover {
    /// From the layouts in which the compositor takes the decoder's dmabufs.
    fn of(modifiers: &[u64]) -> Self {
        if modifiers.contains(&LINEAR) {
            Self::Linear
        } else if modifiers.is_empty() {
            Self::Copy
        } else {
            Self::Tiled(modifiers.to_vec())
        }
    }

    /// The decoder for this handover, and whether its pictures are dmabufs: `tiled` makes one
    /// for the tiled layouts, `plain` one with linear pictures. When the GPU can write none of
    /// the tiled layouts, the pictures are copied.
    fn decoder<D>(
        &self,
        tiled: impl FnOnce(&[u64]) -> Result<D, String>,
        plain: impl FnOnce() -> Result<D, String>,
    ) -> Result<(D, bool), String> {
        match self {
            Self::Linear => plain().map(|d| (d, true)),
            Self::Copy => plain().map(|d| (d, false)),
            Self::Tiled(modifiers) => match tiled(modifiers) {
                Ok(d) => Ok((d, true)),
                Err(e) => {
                    eprintln!(
                        "navette: the GPU cannot write the compositor's tiled layouts ({e}); \
                         the pictures are copied"
                    );
                    plain().map(|d| (d, false))
                }
            },
        }
    }
}

/// The decoders of gliff-vk and gliff-sw.
struct Real {
    gpu: Option<Arc<Gpu>>,
    handover: Handover,
    refused: Arc<AtomicBool>,
}

impl Decoders for Real {
    fn gpu(
        &mut self,
        fresh: bool,
        dual: bool,
        size: (u32, u32),
    ) -> Option<Result<Box<dyn Decode>, String>> {
        if fresh {
            self.gpu = open_gpu();
        }
        let gpu = self.gpu.as_ref()?;
        let (width, height) = size;
        let made = self.handover.decoder(
            |modifiers| {
                Decoder::with_modifiers(gpu, dual, width, height, modifiers)
                    .map_err(|e| e.to_string())
            },
            || Decoder::new(gpu, dual, width, height).map_err(|e| e.to_string()),
        );
        Some(made.map(|(decoder, dmabuf)| {
            let tiled = dmabuf && matches!(self.handover, Handover::Tiled(_));
            Box::new(GpuDecoder {
                decoder,
                dmabuf,
                tiled,
                refused: self.refused.clone(),
                size,
            }) as Box<dyn Decode>
        }))
    }

    fn cpu(&mut self, dual: bool) -> Result<Box<dyn Decode>, String> {
        gliff_sw::Decoder::new(dual)
            .map(|d| Box::new(d) as Box<dyn Decode>)
            .map_err(|e| e.to_string())
    }
}

struct GpuDecoder {
    decoder: Decoder,
    /// The surface takes the pictures as dmabufs; otherwise they are read back.
    dmabuf: bool,
    /// The dmabufs are in one of the compositor's tiled layouts.
    tiled: bool,
    /// Set by the surface when the compositor refused a dmabuf.
    refused: Arc<AtomicBool>,
    size: (u32, u32),
}

impl GpuDecoder {
    fn dmabuf(&self) -> bool {
        self.dmabuf && !self.refused.load(Ordering::Relaxed)
    }
}

impl Decode for GpuDecoder {
    fn tier(&self) -> &'static str {
        "GPU"
    }

    fn handover(&self) -> &'static str {
        match (self.dmabuf(), self.tiled) {
            (true, true) => "shown as a tiled dmabuf",
            (true, false) => "shown as a linear dmabuf",
            (false, _) => "copied to the surface",
        }
    }

    fn decode(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<Picture>, String> {
        let (width, height) = self.size;
        if self.dmabuf() {
            return self
                .decoder
                .decode(main, aux)
                .map(|f| f.map(Picture::Dmabuf))
                .map_err(|e| e.to_string());
        }
        self.decoder
            .decode_to_bgra(main, aux)
            .map(|f| {
                f.map(|pixels| {
                    Picture::Bgra(gliff_sw::BgraFrame {
                        width,
                        height,
                        pixels,
                    })
                })
            })
            .map_err(|e| e.to_string())
    }
}

impl Decode for gliff_sw::Decoder {
    fn tier(&self) -> &'static str {
        "CPU"
    }

    fn decode(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<Picture>, String> {
        gliff_sw::Decoder::decode(self, main, aux)
            .map(|f| f.map(Picture::Bgra))
            .map_err(|e| e.to_string())
    }

    fn drain(&mut self) -> Option<Picture> {
        if !self.has_pending() {
            return None;
        }
        self.flush().ok().flatten().map(Picture::Bgra)
    }
}

/// What the decode thread owns.
struct Thread {
    decoders: Box<dyn Decoders>,
    /// The GPU is tried; false once it showed it cannot decode here.
    on_gpu: bool,
    outbox: Outbox,
    queued: Arc<AtomicUsize>,
    ended: Ended,
    /// Set when a GPU decoder was lost.
    lost: Arc<AtomicBool>,
}

impl Thread {
    fn feedback(&self, msg: ClientMsg) {
        self.outbox.send(Msg::Feedback(msg), Vec::new());
    }

    /// A decoder, or the mode is told it cannot go on.
    fn decoder(
        &mut self,
        (width, height, chroma): (u32, u32, ChromaMode),
        fresh: bool,
    ) -> Option<Box<dyn Decode>> {
        let dual = chroma != ChromaMode::Single420;
        if self.on_gpu {
            match self.decoders.gpu(fresh, dual, (width, height)) {
                Some(Ok(d)) => return Some(d),
                Some(Err(e)) => {
                    eprintln!("navette: cannot create the GPU decoder ({e}); using the CPU")
                }
                None => {}
            }
        }
        match self.decoders.cpu(dual) {
            Ok(d) => Some(d),
            Err(e) => {
                let why = format!("the desk cannot decode the picture: {e}");
                eprintln!("navette: {why}");
                let _ = self.ended.send(why);
                None
            }
        }
    }

    fn run(mut self, mut show: Box<dyn FnMut(Picture) + Send>, rx: Receiver<Video>) {
        let mut decoder: Option<Box<dyn Decode>> = None;
        let mut config = (0, 0, ChromaMode::Single420);
        let mut gpu_decoded = false;
        // The GPU decoder was made again once already.
        let mut renewed = false;
        let mut failures = 0;
        // A new decoder has nothing to refer to before a keyframe.
        let mut wait_key = false;
        let mut stats = Stats::new(self.outbox.output());
        loop {
            let video = match rx.recv_timeout(IDLE_DRAIN) {
                Ok(video) => video,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(picture) = decoder.as_mut().and_then(|d| d.drain()) {
                        show(picture);
                    }
                    stats.report_if_due(decoder.as_ref().map(|d| d.tier()));
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };
            match video.msg {
                ServerMsg::StreamConfig {
                    width,
                    height,
                    chroma,
                    ..
                } => {
                    // A pace-only change must not reset the decoder.
                    if (width, height, chroma) != config || decoder.is_none() {
                        config = (width, height, chroma);
                        failures = 0;
                        decoder = self.decoder(config, false);
                        if let Some(d) = &decoder {
                            eprintln!(
                                "navette: decoding {width}x{height} {chroma:?} on the {}, {}",
                                d.tier(),
                                d.handover()
                            );
                        }
                    }
                }
                ServerMsg::VideoFrame {
                    frame_id,
                    pts_us,
                    keyframe,
                    ..
                } => {
                    self.queued.fetch_sub(1, Ordering::Relaxed);
                    let Some(d) = decoder.as_mut().filter(|_| keyframe || !wait_key) else {
                        // Acked all the same, so the remote keeps pacing until the mode ends.
                        self.feedback(ClientMsg::FrameAck {
                            frame_id,
                            decoded_at_ms: now_us() / 1000,
                        });
                        continue;
                    };
                    wait_key = false;
                    let t0 = Instant::now();
                    let decoded = d.decode(&video.main, &video.aux);
                    let decode_ms = t0.elapsed().as_secs_f32() * 1000.0;
                    // Ack at once so the remote keeps pacing.
                    self.feedback(ClientMsg::FrameAck {
                        frame_id,
                        decoded_at_ms: now_us() / 1000,
                    });
                    match decoded {
                        Ok(Some(picture)) => {
                            failures = 0;
                            gpu_decoded |= d.tier() == "GPU";
                            stats.note(decode_ms, now_us().saturating_sub(pts_us));
                            show(picture);
                        }
                        Ok(None) => failures = 0,
                        Err(e) if d.tier() == "GPU" && !gpu_decoded => {
                            // A driver path that cannot work here: the CPU takes over for good.
                            eprintln!("navette: the GPU decode failed ({e}); decoding on the CPU");
                            self.on_gpu = false;
                            decoder = self.decoder(config, false);
                            self.feedback(ClientMsg::RequestKeyframe);
                        }
                        Err(e) if !keyframe && failures + 1 < BROKEN => {
                            failures += 1;
                            tracing::warn!(error = %e, "decode error; requesting a keyframe");
                            self.feedback(ClientMsg::RequestKeyframe);
                        }
                        Err(e) => {
                            // A lost device fails every decode: the decoder is given up, for
                            // a new one on the GPU once, then the CPU, then the mode ends.
                            let tier = d.tier();
                            let output = super::output::name(usize::from(self.outbox.output()));
                            failures = 0;
                            stats.lost();
                            decoder = None;
                            if tier == "GPU" {
                                self.lost.store(true, Ordering::Relaxed);
                            }
                            if tier == "GPU" && !renewed {
                                eprintln!(
                                    "navette: {output}: the GPU decoder failed ({e}); a new one \
                                     on the GPU opened again"
                                );
                                renewed = true;
                                gpu_decoded = false;
                                decoder = self.decoder(config, true);
                            } else if tier == "GPU" {
                                eprintln!(
                                    "navette: {output}: the GPU decoder failed again ({e}); \
                                     decoding on the CPU"
                                );
                                self.on_gpu = false;
                                decoder = self.decoder(config, false);
                            } else {
                                let why = format!(
                                    "the desk cannot decode {output} any more, {} decoder(s) lost, \
                                     the CPU's last: {e}",
                                    stats.lost
                                );
                                eprintln!("navette: {why}");
                                let _ = self.ended.send(why);
                            }
                            if decoder.is_some() {
                                wait_key = true;
                                self.feedback(ClientMsg::RequestKeyframe);
                            }
                        }
                    }
                }
                ServerMsg::Ping { t } => self.feedback(ClientMsg::Pong { t }),
                _ => {}
            }
            stats.report_if_due(decoder.as_ref().map(|d| d.tier()));
        }
        stats.report(decoder.as_ref().map(|d| d.tier()));
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

/// Frames decoded, decode time, and the time from the remote's encode start to the decoded
/// picture (meaningful when both clocks agree: on one machine, or synchronised by NTP).
struct Stats {
    /// The remote output whose stream this is, named in the report.
    output: u8,
    since: Instant,
    total: u64,
    decode_ms: Vec<f32>,
    latency_ms: Vec<f32>,
    /// Decoders given up since the stream began.
    lost: u32,
}

impl Stats {
    fn new(output: u8) -> Self {
        Self {
            output,
            since: Instant::now(),
            total: 0,
            decode_ms: Vec::new(),
            latency_ms: Vec::new(),
            lost: 0,
        }
    }

    fn lost(&mut self) {
        self.lost += 1;
    }

    fn note(&mut self, decode_ms: f32, latency_us: u64) {
        self.total += 1;
        self.decode_ms.push(decode_ms);
        self.latency_ms.push(latency_us as f32 / 1000.0);
    }

    fn report_if_due(&mut self, tier: Option<&str>) {
        if self.since.elapsed() >= REPORT_EVERY {
            self.report(tier);
        }
    }

    fn report(&mut self, tier: Option<&str>) {
        if let Some(line) = self.line(tier) {
            eprintln!("navette: {line}");
        }
    }

    /// What the report says, none without a decoded frame; the period starts again.
    fn line(&mut self, tier: Option<&str>) -> Option<String> {
        let secs = self.since.elapsed().as_secs_f32();
        let mut line = None;
        if !self.decode_ms.is_empty() {
            let lost = match self.lost {
                0 => String::new(),
                1 => "; 1 decoder lost".into(),
                n => format!("; {n} decoders lost"),
            };
            line = Some(format!(
                "{}: {} frames in {secs:.1} s ({:.1} fps) on the {}, {} total; decode \
                 p50 {:.2} ms p95 {:.2} ms; encode-to-decoded p50 {:.1} ms p95 {:.1} ms{lost}",
                super::output::name(usize::from(self.output)),
                self.decode_ms.len(),
                self.decode_ms.len() as f32 / secs,
                tier.unwrap_or("?"),
                self.total,
                percentile(&mut self.decode_ms, 0.5),
                percentile(&mut self.decode_ms, 0.95),
                percentile(&mut self.latency_ms, 0.5),
                percentile(&mut self.latency_ms, 0.95),
            ));
        }
        self.since = Instant::now();
        self.decode_ms.clear();
        self.latency_ms.clear();
        line
    }
}

fn percentile(values: &mut [f32], p: f32) -> f32 {
    values.sort_by(f32::total_cmp);
    let index = ((values.len() - 1) as f32 * p).round() as usize;
    values[index]
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A moving pattern: a bright square that walks across a dark screen.
    fn picture(width: usize, height: usize, step: usize) -> Vec<u8> {
        let mut bgra = vec![32u8; width * height * 4];
        for y in 0..height / 4 {
            for x in 0..width / 4 {
                let at = ((y + step * 4) % height * width + (x + step * 8) % width) * 4;
                bgra[at..at + 4].copy_from_slice(&[40, 200, 240, 255]);
            }
        }
        bgra
    }

    fn config(width: u32, height: u32) -> Video {
        Video {
            msg: ServerMsg::StreamConfig {
                codec: Codec::H264,
                chroma: ChromaMode::Single420,
                pipeline: None,
                width,
                height,
                scale_milli: 1000,
                extradata: Vec::new(),
                aux_extradata: None,
                view_width: width,
                view_height: height,
                fps_cap: 60,
            },
            main: Bytes::new(),
            aux: Bytes::new(),
        }
    }

    #[test]
    fn frames_encoded_on_the_cpu_are_decoded_shown_and_acked() {
        let real = Real {
            gpu: None,
            handover: Handover::Copy,
            refused: Arc::default(),
        };
        cpu_round_trip(Box::new(real));
    }

    #[test]
    fn a_slow_cpu_decoder_loses_no_picture_when_fed_at_its_pace() {
        cpu_round_trip(Box::new(SlowCpu(Duration::from_millis(60))));
    }

    /// The real CPU decoder, slowed down as a busy runner slows it.
    struct SlowCpu(Duration);

    struct SlowDecoder(gliff_sw::Decoder, Duration);

    impl Decode for SlowDecoder {
        fn tier(&self) -> &'static str {
            "CPU"
        }

        fn decode(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<Picture>, String> {
            std::thread::sleep(self.1);
            Decode::decode(&mut self.0, main, aux)
        }

        fn drain(&mut self) -> Option<Picture> {
            Decode::drain(&mut self.0)
        }
    }

    impl Decoders for SlowCpu {
        fn gpu(
            &mut self,
            _: bool,
            _: bool,
            _: (u32, u32),
        ) -> Option<Result<Box<dyn Decode>, String>> {
            None
        }

        fn cpu(&mut self, dual: bool) -> Result<Box<dyn Decode>, String> {
            let inner = gliff_sw::Decoder::new(dual).map_err(|e| e.to_string())?;
            Ok(Box::new(SlowDecoder(inner, self.0)))
        }
    }

    /// Waits for `done` up to a deadline no healthy run comes near, so a slow runner is waited
    /// for and a real loss still fails the test.
    fn wait_until(done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Encodes ten pictures, hands them to `decoders` and checks that all ten are decoded, shown
    /// and acked. Each frame is handed over once the one before was taken: the desk drops frames it
    /// cannot keep up with, so a feed faster than the decoder would lose some on a slow runner.
    fn cpu_round_trip(decoders: Box<dyn Decoders>) {
        let (width, height) = (320u32, 240u32);
        let shown = Arc::new(Mutex::new(Vec::new()));
        let outbox = Outbox::default();
        let decoding = {
            let shown = shown.clone();
            Decoding::start_with(
                decoders,
                outbox.clone(),
                Box::new(move |p| {
                    if let Picture::Bgra(f) = p {
                        shown.lock().unwrap().push((f.width, f.height, f.pixels));
                    }
                }),
                (tokio::sync::mpsc::unbounded_channel().0, Default::default()),
            )
            .unwrap()
        };
        let mut encoder = encoder((width, height));
        decoding.video(config(width, height));
        let frames = 10;
        for id in 0..frames {
            decoding.video(encoded(&mut encoder, id, (width, height)));
            wait_until(|| acks(&outbox) > id);
        }
        // The last picture may come out of the idle drain.
        wait_until(|| shown.lock().unwrap().len() == frames);
        let shown = shown.lock().unwrap();
        assert_eq!(shown.len(), frames);
        let (w, h, pixels) = &shown[frames - 1];
        assert_eq!((*w, *h), (width, height));
        // The square is bright where it should be, the rest dark.
        let source = picture(width as usize, height as usize, frames - 1);
        let close = pixels
            .chunks(4)
            .zip(source.chunks(4))
            .filter(|(a, b)| (a[1] as i32 - b[1] as i32).abs() < 24)
            .count();
        assert!(close * 100 / (width * height) as usize >= 95, "{close}");
        assert_eq!(acks(&outbox), frames);
    }

    /// A decoder that shows a picture for each frame until its `fails_after`th, then fails.
    struct Fake {
        tier: &'static str,
        decoded: usize,
        fails_after: Option<usize>,
    }

    impl Decode for Fake {
        fn tier(&self) -> &'static str {
            self.tier
        }

        fn decode(&mut self, _: &[u8], _: &[u8]) -> Result<Option<Picture>, String> {
            if self.fails_after.is_some_and(|n| self.decoded >= n) {
                return Err("logical device has been lost".into());
            }
            self.decoded += 1;
            Ok(Some(Picture::Bgra(gliff_sw::BgraFrame {
                width: 1,
                height: 1,
                pixels: vec![0; 4],
            })))
        }
    }

    /// The decoders the desk can make: one GPU decoder per entry of `gpus`, failing after so many
    /// frames, and a CPU one when `cpu` says so. Each one made is written down.
    #[derive(Default)]
    struct Fakes {
        gpus: std::collections::VecDeque<Option<usize>>,
        cpu: Option<Option<usize>>,
        made: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Decoders for Fakes {
        fn gpu(
            &mut self,
            _: bool,
            _: bool,
            _: (u32, u32),
        ) -> Option<Result<Box<dyn Decode>, String>> {
            let fails_after = self.gpus.pop_front()?;
            self.made.lock().unwrap().push("GPU");
            Some(Ok(Box::new(Fake {
                tier: "GPU",
                decoded: 0,
                fails_after,
            })))
        }

        fn cpu(&mut self, _: bool) -> Result<Box<dyn Decode>, String> {
            let fails_after = self.cpu.ok_or("no OpenH264")?;
            self.made.lock().unwrap().push("CPU");
            Ok(Box::new(Fake {
                tier: "CPU",
                decoded: 0,
                fails_after,
            }))
        }
    }

    fn acks(outbox: &Outbox) -> usize {
        outbox
            .queued()
            .into_iter()
            .filter(|m| matches!(m, Msg::Feedback(ClientMsg::FrameAck { .. })))
            .count()
    }

    fn encoded(encoder: &mut gliff_sw::Encoder, id: usize, (w, h): (u32, u32)) -> Video {
        let encoded = encoder
            .encode_bgra(&picture(w as usize, h as usize, id), id == 0)
            .unwrap();
        Video {
            msg: ServerMsg::VideoFrame {
                frame_id: id as u64,
                pts_us: now_us(),
                keyframe: encoded.keyframe,
                damage: Vec::new(),
                data_len: encoded.main.len() as u32,
                aux_len: 0,
            },
            main: Bytes::from(encoded.main),
            aux: Bytes::new(),
        }
    }

    fn encoder((width, height): (u32, u32)) -> gliff_sw::Encoder {
        gliff_sw::Encoder::new(
            gliff_sw::EncoderSettings {
                width,
                height,
                bitrate: 4_000_000,
                framerate: 30,
            },
            false,
        )
        .unwrap()
    }

    #[test]
    fn a_home_that_cannot_decode_ends_the_mode_and_still_acks() {
        let size = (320, 240);
        let outbox = Outbox::default();
        let (ended, mut why) = tokio::sync::mpsc::unbounded_channel();
        let decoding = Decoding::start_with(
            Box::new(Fakes::default()),
            outbox.clone(),
            Box::new(|_| {}),
            (ended, Default::default()),
        )
        .unwrap();
        decoding.video(config(size.0, size.1));
        let mut encoder = encoder(size);
        for id in 0..3 {
            decoding.video(encoded(&mut encoder, id, size));
        }
        drop(decoding);
        let why = why.try_recv().expect("the mode is told");
        assert!(why.contains("no OpenH264"), "{why}");
        assert_eq!(acks(&outbox), 3);
    }

    #[test]
    fn a_decoder_falling_behind_drops_frames_until_a_keyframe() {
        let size = (320, 240);
        let outbox = Outbox::default();
        let (hold, held) = std::sync::mpsc::channel::<()>();
        let shown = Arc::new(Mutex::new(0));
        let decoding = {
            let shown = shown.clone();
            Decoding::start(
                None,
                Default::default(),
                outbox.clone(),
                // The surface is stuck until the test lets it go.
                Box::new(move |_| {
                    let _ = held.recv();
                    *shown.lock().unwrap() += 1;
                }),
                (tokio::sync::mpsc::unbounded_channel().0, Default::default()),
            )
            .unwrap()
        };
        decoding.video(config(size.0, size.1));
        let mut encoder = encoder(size);
        let frames = 30;
        for id in 0..frames {
            decoding.video(encoded(&mut encoder, id, size));
        }
        drop(hold);
        drop(decoding);
        let queued = outbox.queued();
        assert!(
            queued
                .iter()
                .any(|m| matches!(m, Msg::Feedback(ClientMsg::RequestKeyframe))),
            "a keyframe is asked for"
        );
        // Every frame is acked, decoded or dropped, so the remote keeps its pace.
        assert_eq!(acks(&outbox), frames);
        assert!(*shown.lock().unwrap() < frames);
    }

    /// A frame for a fake decoder, which does not look at its bytes.
    fn frame(id: u64, keyframe: bool) -> Video {
        Video {
            msg: ServerMsg::VideoFrame {
                frame_id: id,
                pts_us: now_us(),
                keyframe,
                damage: Vec::new(),
                data_len: 0,
                aux_len: 0,
            },
            main: Bytes::new(),
            aux: Bytes::new(),
        }
    }

    /// Plays `frames` (`k` a keyframe, `p` not) to decoders made by `fakes`, one at a time.
    /// Returns the pictures shown, the decoders made, the keyframes asked for and why the mode
    /// was told to end, if it was.
    fn play(fakes: Fakes, frames: &str) -> (usize, Vec<&'static str>, usize, Option<String>) {
        play_marking(fakes, frames, Default::default())
    }

    /// As [`play`], with the flag the decoders set when a GPU decoder is lost.
    fn play_marking(
        fakes: Fakes,
        frames: &str,
        lost: Arc<AtomicBool>,
    ) -> (usize, Vec<&'static str>, usize, Option<String>) {
        let made = fakes.made.clone();
        let outbox = Outbox::default();
        let shown = Arc::new(AtomicUsize::new(0));
        let (ended, mut why) = tokio::sync::mpsc::unbounded_channel();
        let decoding = {
            let shown = shown.clone();
            Decoding::start_with(
                Box::new(fakes),
                outbox.clone(),
                Box::new(move |_| {
                    shown.fetch_add(1, Ordering::Relaxed);
                }),
                (ended, lost),
            )
            .unwrap()
        };
        decoding.video(config(320, 240));
        for (id, kind) in frames.chars().enumerate() {
            decoding.video(frame(id as u64, kind == 'k'));
            // One at a time, so none is dropped for a full queue.
            wait_until(|| acks(&outbox) > id);
        }
        drop(decoding);
        assert_eq!(acks(&outbox), frames.len(), "every frame is acked");
        let keyframes = outbox
            .queued()
            .into_iter()
            .filter(|m| matches!(m, Msg::Feedback(ClientMsg::RequestKeyframe)))
            .count();
        let made = made.lock().unwrap().clone();
        (
            shown.load(Ordering::Relaxed),
            made,
            keyframes,
            why.try_recv().ok(),
        )
    }

    #[test]
    fn a_lost_gpu_decoder_is_replaced_by_a_new_one_and_a_keyframe_asked_for() {
        let fakes = Fakes {
            gpus: [Some(5), None].into(),
            ..Fakes::default()
        };
        // Five pictures, three failures in a row, then a frame before the keyframe.
        let (shown, made, keyframes, ended) = play(fakes, "kppppppppkp");
        assert_eq!(made, ["GPU", "GPU"]);
        assert!(keyframes >= 1);
        // The frame between the new decoder and the keyframe is not decoded.
        assert_eq!(shown, 5 + 2);
        assert_eq!(ended, None);
    }

    #[test]
    fn a_keyframe_the_gpu_decoder_cannot_decode_gives_it_up_at_once() {
        let fakes = Fakes {
            gpus: [Some(2), None].into(),
            ..Fakes::default()
        };
        let (shown, made, _, ended) = play(fakes, "kpkk");
        assert_eq!(made, ["GPU", "GPU"]);
        assert_eq!(shown, 3);
        assert_eq!(ended, None);
    }

    #[test]
    fn a_lost_gpu_decoder_marks_home_s_gpu_as_lost() {
        let lost = Arc::new(AtomicBool::new(false));
        let fakes = Fakes {
            gpus: [Some(2), None].into(),
            ..Default::default()
        };
        play_marking(fakes, "kppppp", lost.clone());
        assert!(lost.load(Ordering::Relaxed));
    }

    #[test]
    fn a_gpu_decoder_never_lost_leaves_home_s_gpu_as_it_is() {
        let lost = Arc::new(AtomicBool::new(false));
        let fakes = Fakes {
            gpus: [None].into(),
            ..Default::default()
        };
        play_marking(fakes, "kppp", lost.clone());
        assert!(!lost.load(Ordering::Relaxed));
        // A GPU that never showed a picture cannot decode here: it was not lost.
        let fakes = Fakes {
            gpus: [Some(0)].into(),
            cpu: Some(None),
            ..Default::default()
        };
        play_marking(fakes, "kppp", lost.clone());
        assert!(!lost.load(Ordering::Relaxed));
    }

    #[test]
    fn a_gpu_decoder_lost_twice_hands_over_to_the_cpu() {
        let fakes = Fakes {
            gpus: [Some(1), Some(1), None].into(),
            cpu: Some(None),
            ..Fakes::default()
        };
        let (shown, made, keyframes, ended) = play(fakes, "kkkkkpp");
        assert_eq!(made, ["GPU", "GPU", "CPU"]);
        assert!(keyframes >= 2);
        assert_eq!(shown, 1 + 1 + 3);
        assert_eq!(ended, None);
    }

    #[test]
    fn decoders_lost_on_the_gpu_and_the_cpu_end_the_mode_with_the_reason() {
        let fakes = Fakes {
            gpus: [Some(1), Some(1)].into(),
            cpu: Some(Some(1)),
            ..Fakes::default()
        };
        let (shown, made, _, ended) = play(fakes, "kkkkkkpp");
        assert_eq!(made, ["GPU", "GPU", "CPU"]);
        assert_eq!(shown, 3);
        let why = ended.expect("the mode is told");
        assert!(why.contains("logical device has been lost"), "{why}");
    }

    #[test]
    fn the_decode_report_says_how_many_decoders_were_lost() {
        let mut stats = Stats::new(0);
        stats.note(2.0, 10_000);
        assert!(!stats.line(Some("GPU")).unwrap().contains("lost"));
        stats.lost();
        stats.lost();
        stats.note(2.0, 10_000);
        let line = stats.line(Some("CPU")).unwrap();
        assert!(line.contains("on the CPU"), "{line}");
        assert!(line.contains("2 decoders lost"), "{line}");
    }

    const BLOCK_LINEAR: u64 = 0x0300_0000_0060_6015;

    #[test]
    fn a_compositor_that_takes_linear_dmabufs_keeps_the_linear_pictures() {
        assert_eq!(Handover::of(&[BLOCK_LINEAR, LINEAR]), Handover::Linear);
        let made = Handover::Linear.decoder(|_| Ok("tiled"), || Ok("linear"));
        assert_eq!(made, Ok(("linear", true)));
    }

    #[test]
    fn a_compositor_with_only_tiled_dmabufs_gets_tiled_pictures_with_no_copy() {
        let handover = Handover::of(&[BLOCK_LINEAR]);
        assert_eq!(handover, Handover::Tiled(vec![BLOCK_LINEAR]));
        let asked = Mutex::new(Vec::new());
        let made = handover.decoder(
            |modifiers| {
                asked.lock().unwrap().extend_from_slice(modifiers);
                Ok("tiled")
            },
            || Ok("linear"),
        );
        assert_eq!(made, Ok(("tiled", true)));
        assert_eq!(*asked.lock().unwrap(), [BLOCK_LINEAR]);
    }

    #[test]
    fn a_gpu_that_cannot_write_the_compositor_s_tiled_layouts_falls_back_to_the_copy() {
        let made = Handover::of(&[BLOCK_LINEAR]).decoder(
            |_| Err::<&str, _>("no usable modifier".into()),
            || Ok("linear"),
        );
        assert_eq!(made, Ok(("linear", false)));
    }

    #[test]
    fn a_compositor_that_takes_no_dmabuf_from_the_decoder_s_gpu_gets_a_copy() {
        assert_eq!(Handover::of(&[]), Handover::Copy);
        let made = Handover::Copy.decoder(|_| Ok("tiled"), || Ok("linear"));
        assert_eq!(made, Ok(("linear", false)));
    }

    #[test]
    fn a_cpu_only_home_asks_for_one_420_stream() {
        assert_eq!(caps(&None, (1920, 1080)).chroma, [ChromaMode::Single420]);
    }
}
