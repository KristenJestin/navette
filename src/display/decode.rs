//! Home's decode: on the GPU (VA-API and gliff-vk's recombine, the picture handed over as a
//! dmabuf with no copy), or on the CPU (gliff-sw) when the GPU cannot. Runs on its own
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

/// The CPU decoder holds the newest picture of a High-profile stream until the next access
/// unit; after this long without one it is drained, so a still screen shows its last state.
const IDLE_DRAIN: Duration = Duration::from_millis(150);
/// How often the numbers are written to the log.
const REPORT_EVERY: Duration = Duration::from_secs(10);
/// Frames waiting for the decoder; past this, home has fallen behind and drops frames until a
/// keyframe rather than queue them without end.
const QUEUE: usize = 4;

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

/// What home decodes. The CPU asks for one 4:2:0 stream, so it decodes one stream, not two.
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

/// Makes the CPU decoder (a port, so a test can make it fail).
type CpuDecoder = fn(bool) -> Result<gliff_sw::Decoder, String>;

impl Decoding {
    /// `dmabuf`: the surface takes the GPU's pictures as dmabufs; otherwise they are read back.
    /// `ended` hears why decoding cannot go on.
    pub fn start(
        gpu: Option<Arc<Gpu>>,
        dmabuf: bool,
        outbox: Outbox,
        show: Box<dyn FnMut(Picture) + Send>,
        ended: Ended,
    ) -> std::io::Result<Self> {
        Self::start_with(gpu, dmabuf, outbox, show, ended, |dual| {
            gliff_sw::Decoder::new(dual).map_err(|e| e.to_string())
        })
    }

    fn start_with(
        gpu: Option<Arc<Gpu>>,
        dmabuf: bool,
        outbox: Outbox,
        show: Box<dyn FnMut(Picture) + Send>,
        ended: Ended,
        cpu: CpuDecoder,
    ) -> std::io::Result<Self> {
        let (video, rx) = std::sync::mpsc::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let thread = {
            let (outbox, queued) = (outbox.clone(), queued.clone());
            std::thread::Builder::new()
                .name("navette-decode".into())
                .spawn(move || {
                    let thread = Thread {
                        gpu,
                        dmabuf,
                        cpu,
                        outbox,
                        queued,
                        ended,
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

enum VideoDecoder {
    Gpu(Box<Decoder>),
    Cpu(Box<gliff_sw::Decoder>),
}

impl VideoDecoder {
    fn new(
        gpu: &Option<Arc<Gpu>>,
        cpu: CpuDecoder,
        chroma: ChromaMode,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let dual = chroma != ChromaMode::Single420;
        if let Some(gpu) = gpu {
            match Decoder::new(gpu, dual, width, height) {
                Ok(d) => return Ok(Self::Gpu(Box::new(d))),
                Err(e) => eprintln!("navette: cannot create the GPU decoder ({e}); using the CPU"),
            }
        }
        cpu(dual)
            .map(|d| Self::Cpu(Box::new(d)))
            .map_err(|e| format!("home cannot decode the picture: {e}"))
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Gpu(_) => "GPU",
            Self::Cpu(_) => "CPU",
        }
    }

    fn decode(
        &mut self,
        main: &[u8],
        aux: &[u8],
        dmabuf: bool,
        (width, height): (u32, u32),
    ) -> Result<Option<Picture>, String> {
        match self {
            Self::Gpu(d) if dmabuf => d
                .decode(main, aux)
                .map(|f| f.map(Picture::Dmabuf))
                .map_err(|e| e.to_string()),
            Self::Gpu(d) => d
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
                .map_err(|e| e.to_string()),
            Self::Cpu(d) => d
                .decode(main, aux)
                .map(|f| f.map(Picture::Bgra))
                .map_err(|e| e.to_string()),
        }
    }
}

/// What the decode thread owns.
struct Thread {
    gpu: Option<Arc<Gpu>>,
    dmabuf: bool,
    cpu: CpuDecoder,
    outbox: Outbox,
    queued: Arc<AtomicUsize>,
    ended: Ended,
}

impl Thread {
    fn feedback(&self, msg: ClientMsg) {
        self.outbox.send(Msg::Feedback(msg), Vec::new());
    }

    /// A decoder, or the mode is told it cannot go on.
    fn decoder(&self, chroma: ChromaMode, width: u32, height: u32) -> Option<VideoDecoder> {
        match VideoDecoder::new(&self.gpu, self.cpu, chroma, width, height) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("navette: {e}");
                let _ = self.ended.send(e);
                None
            }
        }
    }

    fn run(mut self, mut show: Box<dyn FnMut(Picture) + Send>, rx: Receiver<Video>) {
        let mut decoder: Option<VideoDecoder> = None;
        let mut config = (0, 0, ChromaMode::Single420);
        let mut gpu_decoded = false;
        let mut stats = Stats::new(self.outbox.output());
        loop {
            let video = match rx.recv_timeout(IDLE_DRAIN) {
                Ok(video) => video,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(VideoDecoder::Cpu(d)) = &mut decoder
                        && d.has_pending()
                        && let Ok(Some(frame)) = d.flush()
                    {
                        show(Picture::Bgra(frame));
                    }
                    stats.report_if_due(decoder.as_ref().map(VideoDecoder::label));
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
                        decoder = self.decoder(chroma, width, height);
                        if let Some(d) = &decoder {
                            let handover = match d {
                                VideoDecoder::Gpu(_) if self.dmabuf => "shown as a dmabuf",
                                _ => "copied to the surface",
                            };
                            eprintln!(
                                "navette: decoding {width}x{height} {chroma:?} on the {}, {handover}",
                                d.label()
                            );
                        }
                    }
                }
                ServerMsg::VideoFrame {
                    frame_id, pts_us, ..
                } => {
                    self.queued.fetch_sub(1, Ordering::Relaxed);
                    let Some(d) = &mut decoder else {
                        // Acked all the same, so the remote keeps pacing until the mode ends.
                        self.feedback(ClientMsg::FrameAck {
                            frame_id,
                            decoded_at_ms: now_us() / 1000,
                        });
                        continue;
                    };
                    let t0 = Instant::now();
                    let decoded =
                        d.decode(&video.main, &video.aux, self.dmabuf, (config.0, config.1));
                    let decode_ms = t0.elapsed().as_secs_f32() * 1000.0;
                    // Ack at once so the remote keeps pacing.
                    self.feedback(ClientMsg::FrameAck {
                        frame_id,
                        decoded_at_ms: now_us() / 1000,
                    });
                    match decoded {
                        Ok(Some(picture)) => {
                            gpu_decoded |= matches!(d, VideoDecoder::Gpu(_));
                            stats.note(decode_ms, now_us().saturating_sub(pts_us));
                            show(picture);
                        }
                        Ok(None) => {}
                        Err(e) if matches!(d, VideoDecoder::Gpu(_)) && !gpu_decoded => {
                            // A driver path that cannot work here: the CPU takes over for good.
                            eprintln!("navette: the GPU decode failed ({e}); decoding on the CPU");
                            self.gpu = None;
                            decoder = self.decoder(config.2, config.0, config.1);
                            self.feedback(ClientMsg::RequestKeyframe);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "decode error; requesting a keyframe");
                            self.feedback(ClientMsg::RequestKeyframe);
                        }
                    }
                }
                ServerMsg::Ping { t } => self.feedback(ClientMsg::Pong { t }),
                _ => {}
            }
            stats.report_if_due(decoder.as_ref().map(VideoDecoder::label));
        }
        stats.report(decoder.as_ref().map(VideoDecoder::label));
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
}

impl Stats {
    fn new(output: u8) -> Self {
        Self {
            output,
            since: Instant::now(),
            total: 0,
            decode_ms: Vec::new(),
            latency_ms: Vec::new(),
        }
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
        let secs = self.since.elapsed().as_secs_f32();
        if !self.decode_ms.is_empty() {
            eprintln!(
                "navette: {}: {} frames in {secs:.1} s ({:.1} fps) on the {}, {} total; decode \
                 p50 {:.2} ms p95 {:.2} ms; encode-to-decoded p50 {:.1} ms p95 {:.1} ms",
                super::output::name(usize::from(self.output)),
                self.decode_ms.len(),
                self.decode_ms.len() as f32 / secs,
                tier.unwrap_or("?"),
                self.total,
                percentile(&mut self.decode_ms, 0.5),
                percentile(&mut self.decode_ms, 0.95),
                percentile(&mut self.latency_ms, 0.5),
                percentile(&mut self.latency_ms, 0.95),
            );
        }
        self.since = Instant::now();
        self.decode_ms.clear();
        self.latency_ms.clear();
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
        let (width, height) = (320u32, 240u32);
        let shown = Arc::new(Mutex::new(Vec::new()));
        let outbox = Outbox::default();
        let decoding = {
            let shown = shown.clone();
            Decoding::start(
                None,
                false,
                outbox.clone(),
                Box::new(move |p| {
                    if let Picture::Bgra(f) = p {
                        shown.lock().unwrap().push((f.width, f.height, f.pixels));
                    }
                }),
                tokio::sync::mpsc::unbounded_channel().0,
            )
            .unwrap()
        };
        let mut encoder = gliff_sw::Encoder::new(
            gliff_sw::EncoderSettings {
                width,
                height,
                bitrate: 4_000_000,
                framerate: 30,
            },
            false,
        )
        .unwrap();
        decoding.video(config(width, height));
        let frames = 10;
        for id in 0..frames {
            let encoded = encoder
                .encode_bgra(&picture(width as usize, height as usize, id), id == 0)
                .unwrap();
            decoding.video(Video {
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
            });
        }
        // The last picture comes out of the idle drain.
        for _ in 0..200 {
            if shown.lock().unwrap().len() == frames {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
        let acks = outbox
            .queued()
            .into_iter()
            .filter(|m| matches!(m, Msg::Feedback(ClientMsg::FrameAck { .. })))
            .count();
        assert_eq!(acks, frames);
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
        let decoding =
            Decoding::start_with(None, false, outbox.clone(), Box::new(|_| {}), ended, |_| {
                Err("no OpenH264".into())
            })
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
                false,
                outbox.clone(),
                // The surface is stuck until the test lets it go.
                Box::new(move |_| {
                    let _ = held.recv();
                    *shown.lock().unwrap() += 1;
                }),
                tokio::sync::mpsc::unbounded_channel().0,
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

    #[test]
    fn a_cpu_only_home_asks_for_one_420_stream() {
        assert_eq!(caps(&None, (1920, 1080)).chroma, [ChromaMode::Single420]);
    }
}
