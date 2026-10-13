//! `navette agent` with a synthetic picture: the real link, the real headless output on the
//! remote's Hyprland, its real input and cursor, but a moving pattern encoded on the CPU instead
//! of the captured output.
//! For a test machine whose capture cannot be read (an NVIDIA GPU, whose headless Hyprland
//! offers only tiled buffers, and has no VA-API encoder), to check home's side end to end.
//!
//! ```text
//! cargo run --release --example synthetic_agent -- <remote config dir>
//! ```

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gliff_proto::{ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg, VideoPipeline};
use navette::display::remote::{self, Streams};
use navette::display::stream::Source;
use navette::display::wire::{Msg, Outbox};
use navette::hypr::{self, Hypr};
use navette::link::{self, Backoff, Heartbeat};
use navette::store::Store;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::oneshot;

const FPS: u64 = 60;

struct Synthetic;

/// Stops the pattern thread when dropped, and the cursor's capture.
struct Running {
    stop: Arc<std::sync::atomic::AtomicBool>,
    _cursor: std::sync::Mutex<hypr_capture::Capturer>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Streams for Synthetic {
    fn start(
        &self,
        source: Source,
        _caps: ClientCaps,
        outbox: Outbox,
        mut feedback: UnboundedReceiver<ClientMsg>,
        ended: oneshot::Sender<io::Error>,
    ) -> io::Result<Box<dyn Send + Sync>> {
        let monitor = hypr::monitors(&hypr::Live)?
            .into_iter()
            .find(|m| m.name == source.output)
            .ok_or_else(|| io::Error::other("no such output"))?;
        let (width, height) = (monitor.width & !1, monitor.height & !1);
        let mut encoder = gliff_sw::Encoder::new(
            gliff_sw::EncoderSettings {
                width,
                height,
                bitrate: 20_000_000,
                framerate: FPS as u32,
            },
            false,
        )
        .map_err(io::Error::other)?;
        let cursor = std::sync::Mutex::new(cursor(&source.output, outbox.clone())?);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let output = source.output.clone();
        std::thread::spawn(move || {
            let mut checked = Instant::now();
            outbox.send(
                Msg::Video(ServerMsg::StreamConfig {
                    codec: Codec::H264,
                    chroma: ChromaMode::Single420,
                    pipeline: Some(VideoPipeline::Cpu),
                    width,
                    height,
                    scale_milli: 1000,
                    extradata: Vec::new(),
                    aux_extradata: None,
                    view_width: width,
                    view_height: height,
                    fps_cap: FPS as u32,
                }),
                Vec::new(),
            );
            let mut frame_id = 0u64;
            let start = Instant::now();
            let mut pixels = vec![0u8; (width * height * 4) as usize];
            while !stopped.load(std::sync::atomic::Ordering::Relaxed) && !outbox.is_closed() {
                while let Ok(msg) = feedback.try_recv() {
                    if let ClientMsg::Ping { t } = msg {
                        outbox.send(
                            Msg::Video(ServerMsg::Pong {
                                t,
                                server_now_ms: 0,
                            }),
                            Vec::new(),
                        );
                    }
                }
                // A real capture stops with its output (a Hyprland restart): so does the pattern.
                if checked.elapsed() >= Duration::from_secs(1) {
                    checked = Instant::now();
                    let there = hypr::monitors(&hypr::Live)
                        .is_ok_and(|list| list.iter().any(|m| m.name == output));
                    if !there {
                        let _ = ended.send(io::Error::other("the output is gone"));
                        return;
                    }
                }
                let due = start + Duration::from_micros(frame_id * 1_000_000 / FPS);
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
                if !outbox.video_ready() {
                    // The previous frame is still being written: this one is skipped.
                    frame_id += 1;
                    continue;
                }
                paint(&mut pixels, width, height, frame_id);
                let pts_us = now_us();
                let encoded = match encoder.encode_bgra(&pixels, frame_id == 0) {
                    Ok(encoded) => encoded,
                    Err(e) => {
                        eprintln!("synthetic agent: encode failed: {e}");
                        break;
                    }
                };
                outbox.send(
                    Msg::Video(ServerMsg::VideoFrame {
                        frame_id,
                        pts_us,
                        keyframe: encoded.keyframe,
                        damage: Vec::new(),
                        data_len: encoded.main.len() as u32,
                        aux_len: 0,
                    }),
                    vec![encoded.main, Vec::new()],
                );
                frame_id += 1;
            }
            eprintln!("synthetic agent: the pattern stopped after {frame_id} frames");
        });
        Ok(Box::new(Running {
            stop,
            _cursor: cursor,
        }))
    }
}

/// The cursor's image, captured as `navette agent` does, with no frame ever asked for.
fn cursor(output: &str, outbox: Outbox) -> io::Result<hypr_capture::Capturer> {
    let mut config = hypr_capture::CaptureConfig::new(output.to_string());
    config.target.display = Some(
        hypr::instance()?
            .wayland_display()
            .map_err(io::Error::other)?,
    );
    config.cursor = true;
    let (mut id, mut shown) = (0, None);
    let sink = Box::new(move |event| match event {
        hypr_capture::CaptureEvent::CursorShape {
            width,
            height,
            hot_x,
            hot_y,
            argb,
        } => {
            id += 1;
            let shape = ServerMsg::CursorShape {
                id,
                width,
                height,
                hot_x,
                hot_y,
                argb_len: argb.len() as u32,
            };
            outbox.send(Msg::Video(shape), vec![argb]);
        }
        hypr_capture::CaptureEvent::CursorPos { visible, .. }
            if shown.replace(visible) != Some(visible) =>
        {
            let seen = ServerMsg::CursorPos {
                x: 0.0,
                y: 0.0,
                shape_id: id,
                visible,
            };
            outbox.send(Msg::Video(seen), Vec::new());
        }
        _ => {}
    });
    hypr_capture::Capturer::start(config, sink).map_err(io::Error::other)
}

/// Grey bars, and a bright square that crosses the screen in two seconds.
fn paint(pixels: &mut [u8], width: u32, height: u32, frame: u64) {
    let (w, h) = (width as usize, height as usize);
    let side = h / 4;
    let x0 = (frame as usize * w / (2 * FPS as usize)) % (w - side);
    let y0 = h / 2 - side / 2;
    for y in 0..h {
        for x in 0..w {
            let at = (y * w + x) * 4;
            let inside = (x0..x0 + side).contains(&x) && (y0..y0 + side).contains(&y);
            let px = if inside {
                [40, 200, 240, 255]
            } else {
                let g = (x * 8 / w * 32) as u8;
                [g, g, g, 255]
            };
            pixels[at..at + 4].copy_from_slice(&px);
        }
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .ok_or_else(|| io::Error::other("usage: synthetic_agent <remote config dir>"))?;
    let store = Store::new(dir);
    let dialer = link::remote_dialer(&store)?;
    let (events, mut log) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = log.recv().await {
            eprintln!("synthetic agent: {event:?}");
        }
    });
    let (peers, peers_rx) = mpsc::unbounded_channel();
    let (hypr_events, hypr_events_rx) = mpsc::unbounded_channel();
    hypr::watch_events(hypr_events);
    let hypr: Arc<dyn Hypr> = Arc::new(hypr::Live);
    tokio::spawn(remote::serve(
        peers_rx,
        hypr,
        Arc::new(Synthetic),
        Arc::new(navette::input::LiveInjectors),
        hypr_events_rx,
    ));
    // The display only: no clipboard bridge here.
    let attach = move |mut wire: link::Wire| {
        if let Some(display) = wire.display.take() {
            let _ = peers.send(display);
        }
        std::future::ready(())
    };
    link::agent(
        dialer,
        Heartbeat::default(),
        Backoff::default(),
        events,
        attach,
    )
    .await;
    Ok(())
}
