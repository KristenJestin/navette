//! Home's surface on the reserved monitor (the kind chosen in #2): a layer-shell surface on the
//! overlay layer, bound to that output, anchored on every edge, above the bar. A GPU picture is
//! a dmabuf handed to the compositor as is (linux-dmabuf), with no copy: linear when the
//! compositor takes linear XRGB8888, else in one of the tiled layouts it lists (Hyprland on an
//! NVIDIA GPU lists only tiled ones). A CPU picture goes through shared memory, as does a GPU
//! picture when the decoder can write none of those layouts, or when the decoder's GPU is not
//! the compositor's. The newest picture wins: one is shown per frame callback, and an
//! older one still waiting is dropped, which gives its image back to the decoder.
//!
//! Focus follows the pointer (#7): the surface takes the keyboard (`on_demand`) and a shortcuts
//! inhibitor, so Super and every bind but the `dont_inhibit` ones reach the remote, when the
//! pointer enters it, and gives both back when it leaves or the release hotkey is typed.
//! - It is mapped with no keyboard, so its appearing does not take home's keys (#2: a layer
//!   surface that can take the keyboard takes it when it appears).
//! - Hyprland 0.56 hands the keyboard over at the next motion over the surface, which a real
//!   pointer crossing gives at once.
//! - It gives the keyboard back on leaving: Hyprland would leave it on the surface when the
//!   pointer moves onto a monitor with no window.
//! - Not `exclusive`: Hyprland then keeps the pointer on the surface wherever it goes.
//!
//! The pointer and keyboard events go to [`crate::input::Capture`]. The remote cursor's image is
//! the surface's pointer image, so home's compositor draws it at home's pointer with no delay,
//! as gliff's client does (gliff, MIT, see `LICENSES/gliff-MIT.txt`); before the first one, or
//! for one with nothing to show, the pointer is the default arrow. While
//! [`super::REMOTE_CURSOR`] is off, it is always the default arrow.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop::channel::{self, Channel, Event as ChannelEvent};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_dmabuf, delegate_layer, delegate_output, delegate_registry,
    delegate_shm, registry_handlers,
};
use wayland_client::backend::ObjectId;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

use super::REMOTE_CURSOR;
use super::decode::Picture;
use super::wire::{Cursor, MAX_CURSOR, Pointer};
use super::{Ended, Released};
use crate::input::{self, Capture, Grab, Hotkey, Seat};

/// How long the compositor has to place the surface.
const PLACE_TIMEOUT: Duration = Duration::from_secs(5);

const XRGB8888: u32 = drm_fourcc::DrmFourcc::Xrgb8888 as u32;
/// `DRM_FORMAT_MOD_INVALID`: a buffer whose layout the driver keeps to itself, which an
/// exported image never has.
const INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// The layouts in which the compositor's formats, as (fourcc, modifier), take the decoder's
/// XRGB8888, each once.
fn xrgb_modifiers(formats: impl IntoIterator<Item = (u32, u64)>) -> Vec<u64> {
    let mut modifiers = Vec::new();
    for (format, modifier) in formats {
        if format == XRGB8888 && modifier != INVALID && !modifiers.contains(&modifier) {
            modifiers.push(modifier);
        }
    }
    modifiers
}

/// How a dmabuf picture is handed to the compositor.
#[derive(Debug, PartialEq)]
enum Import {
    /// With `create_immed`: the compositor took this layout already.
    Immediate,
    /// With `create`, waiting for `created` or `failed`.
    Ask,
    /// Not at all: the compositor refused one, the pictures are copied.
    Copy,
}

/// What the compositor said of the decoder's dmabuf layouts. A buffer refused after
/// `create_immed` is a protocol error that ends the connection, and the cover with it: each
/// layout is asked first with `create`, whose refusal is only an event. One refusal and the
/// pictures are copied for the rest of the mode.
#[derive(Default)]
struct Imports {
    taken: Vec<u64>,
    /// Shared with the decoder, which reads the pictures back once it is set.
    refused: Arc<AtomicBool>,
}

impl Imports {
    fn import(&self, modifier: u64) -> Import {
        if self.refused.load(Ordering::Relaxed) {
            Import::Copy
        } else if self.taken.contains(&modifier) {
            Import::Immediate
        } else {
            Import::Ask
        }
    }

    fn created(&mut self, modifier: u64) {
        if !self.taken.contains(&modifier) {
            self.taken.push(modifier);
        }
    }

    /// Whether this is the first refusal.
    fn failed(&mut self) -> bool {
        !self.refused.swap(true, Ordering::Relaxed)
    }
}

/// The layer surface's namespace, for layer rules.
pub const NAMESPACE: &str = "navette";

/// The `major:minor` of a device number, as `/sys/dev/char` names it.
fn char_device(dev: u64) -> String {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    format!("{major}:{minor}")
}

/// The hardware behind a DRM node: a card and its render node lead to the same one.
fn hardware(dev: u64) -> Option<PathBuf> {
    std::fs::canonicalize(format!("/sys/dev/char/{}/device", char_device(dev))).ok()
}

/// Whether the decoder's render node is on the compositor's main device, so its buffers import.
fn same_gpu(main_device: u64, render_node: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(node) = std::fs::metadata(render_node) else {
        return false;
    };
    hardware(main_device).is_some_and(|main| hardware(node.rdev()) == Some(main))
}

impl Cursor {
    /// Whether it can be shown: a sane size, its pixels all there, and a visible shape (not
    /// fully transparent, nor one flat colour as Hyprland sends when it has no image to share).
    fn shows(&self) -> bool {
        let needed = self.width as u64 * self.height as u64 * 4;
        if self.width == 0
            || self.height == 0
            || self.width > MAX_CURSOR
            || self.height > MAX_CURSOR
            || (self.argb.len() as u64) < needed
        {
            return false;
        }
        let mut pixels = self.argb[..needed as usize].as_chunks::<4>().0.iter();
        let Some(first) = pixels.next() else {
            return false;
        };
        let opaque = first[3] != 0 || pixels.clone().any(|p| p[3] != 0);
        let uniform = pixels.all(|p| p == first);
        opaque && !uniform
    }
}

/// What the surface is given to show.
enum Show {
    Picture(Picture),
    /// The remote pointer changed: the newest waits in [`Newest`].
    Pointer,
}

/// The newest value only: a writer replaces what the reader has not taken yet, so remote
/// cursors never pile up in the channel.
struct Newest<T>(Arc<Mutex<Option<T>>>);

impl<T> Clone for Newest<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Default for Newest<T> {
    fn default() -> Self {
        Self(Arc::default())
    }
}

impl<T> Newest<T> {
    /// True when nothing was waiting: the reader has to be woken.
    fn put(&self, value: T) -> bool {
        self.0.lock().unwrap().replace(value).is_none()
    }

    fn take(&self) -> Option<T> {
        self.0.lock().unwrap().take()
    }
}

/// What the pointer over the surface shows of the remote's: all of it, or, with
/// [`REMOTE_CURSOR`] off, nothing (home's default arrow, never hidden).
fn shown(pointer: Pointer) -> Pointer {
    if REMOTE_CURSOR {
        pointer
    } else {
        Pointer::default()
    }
}

/// The surface, on its own thread. Dropping it removes the surface.
pub struct Surface {
    /// The layouts in which the compositor takes the decoder's XRGB8888 dmabufs.
    modifiers: Vec<u64>,
    /// Set once the compositor refused a dmabuf.
    refused: Arc<AtomicBool>,
    pictures: Option<channel::Sender<Show>>,
    pointer: Newest<Pointer>,
    thread: Option<JoinHandle<()>>,
}

impl Surface {
    /// Puts the surface on `output` and waits until the compositor has placed it. Home's input
    /// over it goes to `input`; `hotkey` gives the keyboard back. `ended` hears it if the
    /// surface goes away on its own: the monitor unplugged, the compositor gone.
    pub fn open(
        output: &str,
        input: input::Queue,
        (hotkey, released): (Option<Hotkey>, Released),
        ended: Ended,
    ) -> io::Result<Self> {
        let capture = (input, hotkey, Some(released));
        Self::open_on(compositor()?, output, capture, PLACE_TIMEOUT, ended)
    }

    fn open_on(
        socket: UnixStream,
        output: &str,
        (input, hotkey, released): (input::Queue, Option<Hotkey>, Option<Released>),
        wait: Duration,
        ended: Ended,
    ) -> io::Result<Self> {
        let shutdown = socket.try_clone()?;
        let conn = Connection::from_socket(socket).map_err(wayland)?;
        let (pictures, channel) = channel::channel();
        let pointer = Newest::default();
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let output = output.to_string();
        let newest = pointer.clone();
        let refused = Arc::new(AtomicBool::new(false));
        let imports = Imports {
            taken: Vec::new(),
            refused: refused.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("navette-surface".into())
            .spawn(move || {
                let capture = Capture::new(input, hotkey);
                let capture = match released {
                    Some(released) => capture.on_release(released),
                    None => capture,
                };
                let why = match run(
                    conn,
                    &output,
                    (channel, newest),
                    (capture, imports),
                    &ready_tx,
                ) {
                    Ok(None) => return,
                    Ok(Some(why)) => why,
                    Err(e) => {
                        let why = format!("the surface on {output} ended: {e}");
                        let _ = ready_tx.send(Err(e));
                        why
                    }
                };
                let _ = ended.send(why);
            })?;
        let mut surface = Self {
            modifiers: Vec::new(),
            refused,
            pictures: Some(pictures),
            pointer,
            thread: Some(thread),
        };
        match ready.recv_timeout(wait) {
            Ok(Ok((size, modifiers))) => {
                tracing::debug!(?size, ?modifiers, "surface placed");
                surface.modifiers = modifiers;
                Ok(surface)
            }
            Ok(Err(e)) => Err(e),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // The thread may wait on the compositor for good: cut it off, so it ends.
                let _ = shutdown.shutdown(std::net::Shutdown::Both);
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "the compositor did not place the surface within {} s",
                        wait.as_secs_f32()
                    ),
                ))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("the surface thread ended"))
            }
        }
    }

    /// The layouts in which a GPU picture can be handed over as is; with none, it is read back
    /// first.
    pub fn dmabuf_modifiers(&self) -> Vec<u64> {
        self.modifiers.clone()
    }

    /// Set once the compositor refused a dmabuf: the GPU's pictures are read back from then on.
    pub fn dmabuf_refused(&self) -> Arc<AtomicBool> {
        self.refused.clone()
    }

    /// Where the decoder sends its pictures.
    pub fn pictures(&self) -> Box<dyn FnMut(Picture) + Send> {
        let pictures = self.pictures.clone().expect("open until dropped");
        Box::new(move |picture| {
            let _ = pictures.send(Show::Picture(picture));
        })
    }

    /// The remote pointer as it changed.
    pub fn cursor(&self, pointer: Pointer) {
        if self.pointer.put(shown(pointer))
            && let Some(pictures) = &self.pictures
        {
            let _ = pictures.send(Show::Pointer);
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.pictures = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The compositor's socket: `WAYLAND_DISPLAY`, else the running Hyprland's, as after a
/// restart of Hyprland the inherited name may be gone.
fn compositor() -> io::Result<UnixStream> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    let inherited = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|d| !d.is_empty())
        .and_then(|d| UnixStream::connect(runtime.join(d)).ok());
    match inherited {
        Some(socket) => Ok(socket),
        None => {
            let display = crate::hypr::instance()?
                .wayland_display()
                .map_err(wayland)?;
            UnixStream::connect(runtime.join(display))
        }
    }
}

/// The surface's size once placed, and the layouts in which the compositor takes the decoder's
/// dmabufs.
type Ready = io::Result<((u32, u32), Vec<u64>)>;

fn wayland(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("wayland: {e}"))
}

/// Runs the surface until it is dropped (`None`) or goes away on its own (why).
fn run(
    conn: Connection,
    output_name: &str,
    (pictures, pointer): (Channel<Show>, Newest<Pointer>),
    (capture, imports): (Capture, Imports),
    ready: &std::sync::mpsc::Sender<Ready>,
) -> io::Result<Option<String>> {
    let (globals, queue) = registry_queue_init::<State>(&conn).map_err(wayland)?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).map_err(wayland)?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|_| io::Error::other("the compositor has no layer shell"))?;
    let viewporter: wp_viewporter::WpViewporter = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| io::Error::other("the compositor has no viewporter"))?;
    // Version 7 at most: version 8 replaces the wheel's discrete steps by value120.
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=7, ())
        .map_err(|_| io::Error::other("the compositor has no seat"))?;
    let inhibit: Option<ZwpKeyboardShortcutsInhibitManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let shapes: Option<WpCursorShapeManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    if inhibit.is_none() {
        eprintln!(
            "navette: the compositor cannot inhibit its shortcuts: they stay home over the \
             remote monitor"
        );
    }
    let mut state = State {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh).map_err(wayland)?,
        dmabuf: DmabufState::new(&globals, &qh),
        pool: None,
        layer: None,
        viewport: None,
        transform: wl_output::Transform::Normal,
        size: None,
        ready: Some(ready.clone()),
        modifiers: None,
        render_node: hypr_capture::render_node(None),
        pending: None,
        imports,
        asking: None,
        waiting_frame: false,
        shown_shm: None,
        shown_dmabuf: HashMap::new(),
        exit: None,
        qh: qh.clone(),
        capture,
        seat,
        inhibit,
        inhibitor: None,
        pointer: None,
        keyboard: None,
        discrete: None,
        shapes,
        shape: None,
        cursor_surface: compositor.create_surface(&qh),
        cursor_viewport: None,
        cursor_buffer: None,
        hotspot: None,
        cursor_hidden: false,
        entered: None,
        physical_width: None,
    };
    state.cursor_viewport = Some(viewporter.get_viewport(&state.cursor_surface, &qh, ()));

    if state.dmabuf.version().is_some_and(|v| v >= 4) {
        let _ = state.dmabuf.get_default_feedback(&qh);
    }
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), queue)
        .insert(event_loop.handle())
        .map_err(|e| wayland(e.error))?;
    // The output list arrives with the first round trips.
    event_loop.dispatch(Some(Duration::from_millis(100)), &mut state)?;
    conn.roundtrip().map_err(wayland)?;
    event_loop.dispatch(Some(Duration::from_millis(100)), &mut state)?;
    // Version 4's feedback comes once complete: no guess before its `done`.
    for _ in 0..10 {
        if state.modifiers.is_some() || state.dmabuf.version().is_none_or(|v| v < 4) {
            break;
        }
        conn.roundtrip().map_err(wayland)?;
        event_loop.dispatch(Some(Duration::from_millis(50)), &mut state)?;
    }

    // Before version 4, the formats came as a list, with no main device.
    if state.modifiers.is_none() {
        let modifiers = xrgb_modifiers(
            state
                .dmabuf
                .modifiers()
                .iter()
                .map(|f| (f.format, f.modifier)),
        );
        state.modifiers = Some(modifiers);
    }

    let output = state
        .outputs
        .outputs()
        .find(|o| state.outputs.info(o).and_then(|i| i.name).as_deref() == Some(output_name))
        .ok_or_else(|| io::Error::other(format!("no monitor {output_name} at home")))?;
    state.transform = state
        .outputs
        .info(&output)
        .map(|i| i.transform)
        .unwrap_or(wl_output::Transform::Normal);
    // The monitor's width in pixels as laid out: with the surface's logical width, its scale.
    state.physical_width = state.outputs.info(&output).and_then(|info| {
        let (width, height) = info.modes.iter().find(|m| m.current)?.dimensions;
        let turned = matches!(
            info.transform,
            wl_output::Transform::_90
                | wl_output::Transform::_270
                | wl_output::Transform::Flipped90
                | wl_output::Transform::Flipped270
        );
        Some(if turned { height } else { width } as f64)
    });
    // Where the monitor stands in home's layout: a drag carried past the surface's edge goes on
    // to the work monitor beside it.
    if let Some(info) = state.outputs.info(&output) {
        let (x, y) = info.logical_position.unwrap_or(info.location);
        state.capture.at(x, y);
    }

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface.clone(),
        Layer::Overlay,
        Some(NAMESPACE),
        Some(&output),
    );
    layer.set_anchor(Anchor::all());
    layer.set_size(0, 0);
    layer.set_exclusive_zone(-1);
    // No keyboard until the pointer comes over it.
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.commit();
    state.viewport = Some(viewporter.get_viewport(&surface, &qh, ()));
    state.layer = Some(layer);

    event_loop
        .handle()
        .insert_source(pictures, |event, _, state| match event {
            ChannelEvent::Msg(Show::Picture(picture)) => {
                // An older picture still waiting is dropped here.
                state.pending = Some(picture);
                state.present();
            }
            ChannelEvent::Msg(Show::Pointer) => {
                if let Some(pointer) = pointer.take() {
                    state.remote_cursor(pointer);
                }
            }
            ChannelEvent::Closed => state.exit = Some(None),
        })
        .map_err(|e| wayland(e.error))?;

    loop {
        if let Some(why) = state.exit.take() {
            return Ok(why);
        }
        event_loop.dispatch(None, &mut state)?;
    }
}

struct State {
    registry: RegistryState,
    outputs: OutputState,
    shm: Shm,
    dmabuf: DmabufState,
    pool: Option<SlotPool>,
    layer: Option<LayerSurface>,
    viewport: Option<wp_viewport::WpViewport>,
    /// The monitor's transform, applied to a picture captured unrotated.
    transform: wl_output::Transform,
    /// The surface's size in logical pixels, once configured.
    size: Option<(u32, u32)>,
    ready: Option<std::sync::mpsc::Sender<Ready>>,
    /// The XRGB8888 layouts the compositor takes from the decoder's GPU, once known.
    modifiers: Option<Vec<u64>>,
    /// The decoder's GPU, compared with the compositor's main device.
    render_node: PathBuf,
    pending: Option<Picture>,
    imports: Imports,
    /// A dmabuf picture whose buffer the compositor is asked for, with its params.
    asking: Option<(zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, Picture)>,
    /// A picture was committed and its frame callback has not come yet.
    waiting_frame: bool,
    shown_shm: Option<Buffer>,
    /// Dmabuf pictures the compositor still holds, by buffer.
    shown_dmabuf: HashMap<ObjectId, (wl_buffer::WlBuffer, Picture)>,
    /// Set to stop: `None` when dropped, else why the surface went away.
    exit: Option<Option<String>>,
    qh: QueueHandle<State>,
    capture: Capture,
    seat: wl_seat::WlSeat,
    inhibit: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    /// Present while the surface holds the keyboard.
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// The wheel's step, sent with the axis event that follows it.
    discrete: Option<i32>,
    /// For the default arrow, when the compositor has cursor shapes.
    shapes: Option<WpCursorShapeManagerV1>,
    shape: Option<WpCursorShapeDeviceV1>,
    /// The remote cursor's image, and its hotspot in logical pixels once there is one to show.
    cursor_surface: wl_surface::WlSurface,
    cursor_viewport: Option<wp_viewport::WpViewport>,
    cursor_buffer: Option<Buffer>,
    hotspot: Option<(i32, i32)>,
    cursor_hidden: bool,
    /// The serial of the pointer's entry, while it is over the surface.
    entered: Option<u32>,
    /// The monitor's width in pixels, turned as laid out.
    physical_width: Option<f64>,
}

/// The layer surface's hold on the keyboard; nothing before the surface is mapped.
struct LayerGrab<'a> {
    layer: Option<&'a LayerSurface>,
    inhibit: Option<&'a ZwpKeyboardShortcutsInhibitManagerV1>,
    inhibitor: &'a mut Option<ZwpKeyboardShortcutsInhibitorV1>,
    seat: &'a wl_seat::WlSeat,
    qh: &'a QueueHandle<State>,
}

impl Grab for LayerGrab<'_> {
    fn interactive(&mut self, on: bool) {
        if let Some(layer) = self.layer {
            layer.set_keyboard_interactivity(if on {
                KeyboardInteractivity::OnDemand
            } else {
                KeyboardInteractivity::None
            });
        }
    }

    fn inhibit(&mut self) {
        if self.inhibitor.is_none()
            && let (Some(layer), Some(inhibit)) = (self.layer, self.inhibit)
        {
            *self.inhibitor =
                Some(inhibit.inhibit_shortcuts(layer.wl_surface(), self.seat, self.qh, ()));
        }
    }

    fn uninhibit(&mut self) {
        if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.destroy();
        }
    }

    fn commit(&mut self) {
        if let Some(layer) = self.layer {
            layer.commit();
        }
    }
}

impl State {
    fn surface(&self) -> Option<&wl_surface::WlSurface> {
        self.layer.as_ref().map(|l| l.wl_surface())
    }

    /// What the seat says goes to the capture, which takes the keyboard and the shortcuts or
    /// gives them back.
    fn seat(&mut self, event: Seat) {
        let mut grab = LayerGrab {
            layer: self.layer.as_ref(),
            inhibit: self.inhibit.as_ref(),
            inhibitor: &mut self.inhibitor,
            seat: &self.seat,
            qh: &self.qh,
        };
        input::handle(&mut self.capture, &mut grab, event);
    }

    fn ours(&self, surface: &wl_surface::WlSurface) -> bool {
        self.surface() == Some(surface)
    }

    /// The monitor's scale: its pixels per logical pixel.
    fn scale(&self) -> f64 {
        match (self.physical_width, self.size) {
            (Some(pixels), Some((width, _))) if width > 0 => pixels / width as f64,
            _ => 1.0,
        }
    }

    /// Draws the remote cursor's new image, and shows it if the pointer is over the surface.
    fn remote_cursor(&mut self, pointer: Pointer) {
        tracing::debug!(
            visible = pointer.visible,
            size = ?pointer.shape.as_ref().map(|c| (c.width, c.height)),
            shows = pointer.shape.as_ref().is_some_and(Cursor::shows),
            "remote cursor"
        );
        self.cursor_hidden = !pointer.visible;
        let Some(cursor) = pointer.shape else {
            self.hotspot = None;
            self.set_cursor();
            return;
        };
        if !cursor.shows() {
            self.hotspot = None;
            self.set_cursor();
            return;
        }
        let len = (cursor.width * cursor.height * 4) as usize;
        let Some(buffer) = self.shm_buffer_argb(cursor.width, cursor.height, |canvas| {
            canvas.copy_from_slice(&cursor.argb[..len])
        }) else {
            return;
        };
        // The image is in the remote output's pixels, which are this monitor's: shown at its
        // scale, it is the size the remote shows.
        let scale = self.scale();
        let logical = |v: f64| ((v / scale).round() as i32).max(1);
        let _ = buffer.attach_to(&self.cursor_surface);
        if let Some(viewport) = &self.cursor_viewport {
            viewport.set_destination(logical(cursor.width as f64), logical(cursor.height as f64));
        }
        self.cursor_surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.cursor_surface.commit();
        self.cursor_buffer = Some(buffer);
        self.hotspot = Some((
            (cursor.hot_x as f64 / scale).round() as i32,
            (cursor.hot_y as f64 / scale).round() as i32,
        ));
        self.set_cursor();
    }

    /// The pointer's image over the surface: the remote cursor's, else the default arrow; none
    /// while the remote hides its own.
    fn set_cursor(&self) {
        let (Some(serial), Some(pointer)) = (self.entered, &self.pointer) else {
            return;
        };
        if self.cursor_hidden {
            pointer.set_cursor(serial, None, 0, 0);
            return;
        }
        match (self.hotspot, &self.shape) {
            (Some((x, y)), _) => pointer.set_cursor(serial, Some(&self.cursor_surface), x, y),
            (None, Some(shape)) => {
                shape.set_shape(serial, wp_cursor_shape_device_v1::Shape::Default)
            }
            (None, None) => {}
        }
    }

    /// Shows the waiting picture if the compositor is ready for one.
    fn present(&mut self) {
        if self.size.is_none() || self.waiting_frame || self.asking.is_some() {
            return;
        }
        let Some(picture) = self.pending.take() else {
            return;
        };
        let qh = self.qh.clone();
        let Some(surface) = self.surface().cloned() else {
            return;
        };
        match picture {
            Picture::Dmabuf(frame) => {
                let import = self.imports.import(frame.modifier);
                if import == Import::Copy {
                    // Dropped: the decoder reads the next ones back.
                    return;
                }
                let Ok(params) = self.dmabuf.create_params(&qh) else {
                    return;
                };
                params.add(
                    frame.fd.as_fd(),
                    0,
                    frame.offset,
                    frame.stride,
                    frame.modifier,
                );
                let (width, height) = (frame.width as i32, frame.height as i32);
                let (format, flags) = (
                    frame.fourcc as u32,
                    zwp_linux_buffer_params_v1::Flags::empty(),
                );
                if import == Import::Ask {
                    let params = params.create(width, height, format, flags);
                    self.asking = Some((params, Picture::Dmabuf(frame)));
                    return;
                }
                let (buffer, params) = params.create_immed(width, height, format, flags, &qh);
                params.destroy();
                surface.attach(Some(&buffer), 0, 0);
                self.shown_dmabuf
                    .insert(buffer.id(), (buffer, Picture::Dmabuf(frame)));
            }
            Picture::Bgra(frame) => {
                let Some(buffer) = self.shm_buffer(frame.width, frame.height, |canvas| {
                    canvas.copy_from_slice(&frame.pixels)
                }) else {
                    return;
                };
                let _ = buffer.attach_to(&surface);
                self.shown_shm = Some(buffer);
            }
        }
        self.commit_picture(&surface);
    }

    /// Commits the attached picture, and asks to hear when the compositor wants the next one.
    fn commit_picture(&mut self, surface: &wl_surface::WlSurface) {
        let Some((width, height)) = self.size else {
            return;
        };
        // The picture is in the monitor's mode, unturned, like the screen it was captured from:
        // the compositor applies the monitor's transform, flips and half turns included.
        surface.set_buffer_transform(self.transform);
        if let Some(viewport) = &self.viewport {
            viewport.set_destination(width as i32, height as i32);
        }
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        surface.frame(&self.qh, surface.clone());
        surface.commit();
        self.waiting_frame = true;
    }

    fn shm_buffer(
        &mut self,
        width: u32,
        height: u32,
        draw: impl FnOnce(&mut [u8]),
    ) -> Option<Buffer> {
        self.shm_buffer_in(width, height, wl_shm::Format::Xrgb8888, draw)
    }

    fn shm_buffer_argb(
        &mut self,
        width: u32,
        height: u32,
        draw: impl FnOnce(&mut [u8]),
    ) -> Option<Buffer> {
        self.shm_buffer_in(width, height, wl_shm::Format::Argb8888, draw)
    }

    fn shm_buffer_in(
        &mut self,
        width: u32,
        height: u32,
        format: wl_shm::Format,
        draw: impl FnOnce(&mut [u8]),
    ) -> Option<Buffer> {
        let len = (width * height * 4) as usize;
        if self.pool.is_none() {
            self.pool = SlotPool::new(len * 2, &self.shm).ok();
        }
        let pool = self.pool.as_mut()?;
        let (buffer, canvas) = pool
            .create_buffer(width as i32, height as i32, width as i32 * 4, format)
            .ok()?;
        draw(&mut canvas[..len]);
        Some(buffer)
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.waiting_frame = false;
        self.present();
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = Some(Some("the monitor went away".into()));
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let size = configure.new_size;
        let first = self.size.is_none();
        if self.size != Some(size) {
            self.capture.area(size.0, size.1);
        }
        self.size = Some(size);
        if first {
            // Black until the first picture, so the monitor shows at once that it is taken.
            if let Some(buffer) = self.shm_buffer(1, 1, |canvas| canvas.fill(0))
                && let Some(surface) = self.surface().cloned()
            {
                let _ = buffer.attach_to(&surface);
                if let Some(viewport) = &self.viewport {
                    viewport.set_destination(size.0 as i32, size.1 as i32);
                }
                surface.damage_buffer(0, 0, 1, 1);
                surface.frame(qh, surface.clone());
                surface.commit();
                self.waiting_frame = true;
                self.shown_shm = Some(buffer);
            }
            if let Some(ready) = self.ready.take() {
                let modifiers = self.modifiers.clone().unwrap_or_default();
                let _ = ready.send(Ok((size, modifiers)));
            }
        }
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }

    fn dmabuf_feedback(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        feedback: DmabufFeedback,
    ) {
        let table = feedback.format_table();
        let modifiers = xrgb_modifiers(feedback.tranches().iter().flat_map(|tranche| {
            tranche
                .formats
                .iter()
                .filter_map(|&i| table.get(i as usize))
                .map(|f| (f.format, f.modifier))
        }));
        self.modifiers = Some(if same_gpu(feedback.main_device(), &self.render_node) {
            modifiers
        } else {
            Vec::new()
        });
    }

    fn created(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        buffer: wl_buffer::WlBuffer,
    ) {
        params.destroy();
        let Some((_, picture)) = self.asking.take() else {
            buffer.destroy();
            return;
        };
        if let Picture::Dmabuf(frame) = &picture {
            self.imports.created(frame.modifier);
        }
        let Some(surface) = self.surface().cloned() else {
            buffer.destroy();
            return;
        };
        surface.attach(Some(&buffer), 0, 0);
        self.shown_dmabuf.insert(buffer.id(), (buffer, picture));
        self.commit_picture(&surface);
    }

    fn failed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    ) {
        params.destroy();
        // The picture goes back to the decoder.
        self.asking = None;
        if self.imports.failed() {
            eprintln!(
                "navette: the compositor refused the decoded picture; the pictures are copied \
                 for the rest of the mode"
            );
        }
        self.present();
    }

    fn released(&mut self, _: &Connection, _: &QueueHandle<Self>, buffer: &wl_buffer::WlBuffer) {
        if let Some((buffer, _picture)) = self.shown_dmabuf.remove(&buffer.id()) {
            buffer.destroy();
        }
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_dmabuf!(State);
delegate_layer!(State);
delegate_registry!(State);
delegate_noop!(State: wp_viewporter::WpViewporter);
delegate_noop!(State: ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(State: WpCursorShapeManagerV1);
delegate_noop!(State: WpCursorShapeDeviceV1);
delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitorV1);

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        else {
            return;
        };
        if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
            let pointer = seat.get_pointer(qh, ());
            state.shape = state
                .shapes
                .as_ref()
                .map(|shapes| shapes.get_pointer(&pointer, qh, ()));
            state.pointer = Some(pointer);
        }
        if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_pointer::Event;
        match event {
            Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } if state.ours(&surface) => {
                state.entered = Some(serial);
                state.set_cursor();
                state.seat(Seat::PointerEnter {
                    x: surface_x,
                    y: surface_y,
                });
            }
            Event::Leave { surface, .. } if state.ours(&surface) => {
                state.entered = None;
                state.seat(Seat::PointerLeave);
            }
            Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.capture.motion(surface_x, surface_y),
            Event::Button {
                button,
                state: WEnum::Value(pressed),
                ..
            } => state
                .capture
                .button(button, pressed == wl_pointer::ButtonState::Pressed),
            Event::AxisDiscrete { discrete, .. } => state.discrete = Some(discrete),
            Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } => {
                let discrete = state.discrete.take();
                state.capture.axis(axis_of(axis), value, discrete, false);
            }
            Event::AxisStop {
                axis: WEnum::Value(axis),
                ..
            } => state.capture.axis(axis_of(axis), 0.0, None, true),
            _ => {}
        }
    }
}

fn axis_of(axis: wl_pointer::Axis) -> gliff_proto::Axis {
    match axis {
        wl_pointer::Axis::HorizontalScroll => gliff_proto::Axis::Horizontal,
        _ => gliff_proto::Axis::Vertical,
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_keyboard::Event;
        match event {
            Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => match read_keymap(fd, size) {
                Ok(text) => state.seat(Seat::Keymap(text)),
                Err(e) => tracing::warn!("cannot read home's keymap: {e}"),
            },
            Event::Enter { surface, keys, .. } if state.ours(&surface) => {
                let (codes, _) = keys.as_chunks::<4>();
                let codes: Vec<u32> = codes.iter().map(|code| u32::from_ne_bytes(*code)).collect();
                state.seat(Seat::KeyboardEnter { keys: &codes });
            }
            Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => state.seat(Seat::Modifiers {
                depressed: mods_depressed,
                latched: mods_latched,
                locked: mods_locked,
                group,
            }),
            Event::Leave { surface, .. } => state.seat(Seat::KeyboardLeave {
                ours: state.ours(&surface),
            }),
            Event::Key {
                key,
                state: WEnum::Value(pressed),
                ..
            } => state.seat(Seat::Key {
                code: key,
                pressed: pressed == wl_keyboard::KeyState::Pressed,
            }),
            _ => {}
        }
    }
}

/// The keymap the compositor shares, read without moving the file's offset (other clients
/// read the same file).
fn read_keymap(fd: OwnedFd, size: u32) -> io::Result<String> {
    use std::os::unix::fs::FileExt;
    if size as usize > input::MAX_KEYMAP {
        return Err(io::Error::other("a keymap larger than any keyboard's"));
    }
    let mut bytes = vec![0; size as usize];
    std::fs::File::from(fd).read_exact_at(&mut bytes, 0)?;
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}
delegate_noop!(State: wp_viewport::WpViewport);

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK_LINEAR: u64 = 0x0300_0000_0060_6015;

    #[test]
    fn a_compositor_that_never_places_the_surface_does_not_hold_the_start() {
        let (ours, _silent) = UnixStream::pair().unwrap();
        let begin = std::time::Instant::now();
        let error = Surface::open_on(
            ours,
            "DP-1",
            Default::default(),
            std::time::Duration::from_millis(300),
            tokio::sync::mpsc::unbounded_channel().0,
        )
        .err()
        .expect("no surface");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(begin.elapsed() < std::time::Duration::from_secs(2));
    }

    fn cursor(width: u32, height: u32, argb: Vec<u8>) -> Cursor {
        Cursor {
            width,
            height,
            hot_x: 0,
            hot_y: 0,
            argb: argb.into(),
        }
    }

    #[test]
    fn only_the_newest_remote_pointer_waits() {
        let newest = Newest::default();
        assert!(newest.put(1));
        // The reader, already woken, takes the newer one.
        assert!(!newest.put(2));
        assert_eq!(newest.take(), Some(2));
        assert_eq!(newest.take(), None);
        assert!(newest.put(3));
    }

    #[test]
    fn home_shows_its_own_arrow_whatever_the_remote_says() {
        let mut argb = vec![0; 16];
        argb[3] = 255;
        let hidden_text_cursor = Pointer {
            shape: Some(cursor(2, 2, argb)),
            visible: false,
        };
        assert_eq!(shown(hidden_text_cursor), Pointer::default());
    }

    #[test]
    fn a_remote_cursor_with_a_shape_is_shown() {
        let mut argb = vec![0; 16];
        argb[3] = 255;
        assert!(cursor(2, 2, argb).shows());
    }

    #[test]
    fn a_remote_cursor_with_nothing_to_show_leaves_the_default_arrow() {
        // Fully transparent; one flat colour (Hyprland's "no image"); pixels missing; too large.
        assert!(!cursor(2, 2, vec![0; 16]).shows());
        assert!(!cursor(2, 2, [9, 9, 9, 255].repeat(4)).shows());
        assert!(!cursor(2, 2, vec![255; 8]).shows());
        assert!(!cursor(MAX_CURSOR + 1, 1, vec![255; 4 * 1025]).shows());
    }

    #[test]
    fn a_device_number_is_read_as_sysfs_names_it() {
        // makedev(226, 128): the first render node.
        assert_eq!(char_device((226 << 8) | 128), "226:128");
        // makedev(226, 300): the minor's high bits sit above the major.
        assert_eq!(char_device((256 << 12) | (226 << 8) | 44), "226:300");
    }

    const ARGB8888: u32 = drm_fourcc::DrmFourcc::Argb8888 as u32;

    #[test]
    fn a_compositor_with_only_tiled_dmabufs_is_offered_its_tiled_layouts() {
        let modifiers = xrgb_modifiers([
            (XRGB8888, BLOCK_LINEAR),
            (ARGB8888, 0x0300_0000_0060_6014),
            (XRGB8888, INVALID),
            (XRGB8888, BLOCK_LINEAR),
        ]);
        assert_eq!(modifiers, [BLOCK_LINEAR]);
    }

    #[test]
    fn a_compositor_that_takes_linear_xrgb_gets_the_dmabuf() {
        let modifiers = xrgb_modifiers([(XRGB8888, BLOCK_LINEAR), (XRGB8888, 0)]);
        assert_eq!(modifiers, [BLOCK_LINEAR, 0]);
    }

    #[test]
    fn a_compositor_that_refuses_the_tiled_picture_gets_copies_for_the_rest_of_the_mode() {
        let mut imports = Imports::default();
        let refused = imports.refused.clone();
        assert_eq!(imports.import(BLOCK_LINEAR), Import::Ask);
        assert!(imports.failed(), "the first refusal is told");
        assert!(refused.load(Ordering::Relaxed), "the decoder hears it");
        assert_eq!(imports.import(BLOCK_LINEAR), Import::Copy);
        assert!(!imports.failed(), "told once");
    }

    #[test]
    fn a_layout_the_compositor_took_once_is_handed_over_at_once() {
        let mut imports = Imports::default();
        assert_eq!(imports.import(BLOCK_LINEAR), Import::Ask);
        imports.created(BLOCK_LINEAR);
        assert_eq!(imports.import(BLOCK_LINEAR), Import::Immediate);
        assert_eq!(
            imports.import(0),
            Import::Ask,
            "a new layout is asked again"
        );
    }

    #[test]
    fn a_compositor_without_xrgb_dmabufs_gets_a_copy() {
        assert!(xrgb_modifiers([(ARGB8888, 0), (XRGB8888, INVALID)]).is_empty());
    }
}
