//! Home's surface on the reserved monitor (the kind chosen in #2): a layer-shell surface on the
//! overlay layer, bound to that output, anchored on every edge, above the bar. A GPU picture is
//! a dmabuf handed to the compositor as is (linux-dmabuf), with no copy; a CPU picture goes
//! through shared memory, as does a GPU picture when the compositor does not take the decoder's
//! linear dmabufs (Hyprland on an NVIDIA GPU lists only tiled ones), or when the decoder's GPU
//! is not the compositor's. The newest picture wins: one is shown per frame callback, and an
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
//! The pointer and keyboard events go to [`crate::input::Capture`].

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
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
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

use super::Ended;
use super::decode::Picture;
use crate::input::{self, Capture, Focus, Hotkey};

/// How long the compositor has to place the surface.
const PLACE_TIMEOUT: Duration = Duration::from_secs(5);

const XRGB8888: u32 = drm_fourcc::DrmFourcc::Xrgb8888 as u32;
const LINEAR: u64 = 0;

/// Whether the compositor's formats, as (fourcc, modifier), hold the decoder's linear XRGB8888.
fn takes_linear_xrgb(formats: impl IntoIterator<Item = (u32, u64)>) -> bool {
    formats.into_iter().any(|f| f == (XRGB8888, LINEAR))
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

/// The surface, on its own thread. Dropping it removes the surface.
pub struct Surface {
    /// The compositor takes the decoder's linear XRGB8888 dmabuf.
    linear_dmabuf: bool,
    pictures: Option<channel::Sender<Picture>>,
    thread: Option<JoinHandle<()>>,
}

impl Surface {
    /// Puts the surface on `output` and waits until the compositor has placed it. Home's input
    /// over it goes to `input`; `hotkey` gives the keyboard back. `ended` hears it if the
    /// surface goes away on its own: the monitor unplugged, the compositor gone.
    pub fn open(
        output: &str,
        input: input::Queue,
        hotkey: Option<Hotkey>,
        ended: Ended,
    ) -> io::Result<Self> {
        let capture = (input, hotkey);
        Self::open_on(compositor()?, output, capture, PLACE_TIMEOUT, ended)
    }

    fn open_on(
        socket: UnixStream,
        output: &str,
        (input, hotkey): (input::Queue, Option<Hotkey>),
        wait: Duration,
        ended: Ended,
    ) -> io::Result<Self> {
        let shutdown = socket.try_clone()?;
        let conn = Connection::from_socket(socket).map_err(wayland)?;
        let (pictures, channel) = channel::channel();
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let output = output.to_string();
        let thread = std::thread::Builder::new()
            .name("navette-surface".into())
            .spawn(move || {
                let capture = Capture::new(input, hotkey);
                let why = match run(conn, &output, channel, capture, &ready_tx) {
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
            linear_dmabuf: false,
            pictures: Some(pictures),
            thread: Some(thread),
        };
        match ready.recv_timeout(wait) {
            Ok(Ok((size, linear_dmabuf))) => {
                tracing::debug!(?size, linear_dmabuf, "surface placed");
                surface.linear_dmabuf = linear_dmabuf;
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

    /// Whether a GPU picture can be handed over as is; otherwise it is read back first.
    pub fn takes_linear_dmabuf(&self) -> bool {
        self.linear_dmabuf
    }

    /// Where the decoder sends its pictures.
    pub fn pictures(&self) -> Box<dyn FnMut(Picture) + Send> {
        let pictures = self.pictures.clone().expect("open until dropped");
        Box::new(move |picture| {
            let _ = pictures.send(picture);
        })
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

/// The surface's size once placed, and whether the compositor takes linear dmabufs.
type Ready = io::Result<((u32, u32), bool)>;

fn wayland(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("wayland: {e}"))
}

/// Runs the surface until it is dropped (`None`) or goes away on its own (why).
fn run(
    conn: Connection,
    output_name: &str,
    pictures: Channel<Picture>,
    capture: Capture,
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
        linear_dmabuf: None,
        render_node: hypr_capture::render_node(None),
        pending: None,
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
    };

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
        if state.linear_dmabuf.is_some() || state.dmabuf.version().is_none_or(|v| v < 4) {
            break;
        }
        conn.roundtrip().map_err(wayland)?;
        event_loop.dispatch(Some(Duration::from_millis(50)), &mut state)?;
    }

    // Before version 4, the formats came as a list, with no main device.
    if state.linear_dmabuf.is_none() {
        let linear = takes_linear_xrgb(
            state
                .dmabuf
                .modifiers()
                .iter()
                .map(|f| (f.format, f.modifier)),
        );
        state.linear_dmabuf = Some(linear);
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
            ChannelEvent::Msg(picture) => {
                // An older picture still waiting is dropped here.
                state.pending = Some(picture);
                state.present();
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
    /// From the compositor's dmabuf feedback (version 4 and up).
    linear_dmabuf: Option<bool>,
    /// The decoder's GPU, compared with the compositor's main device.
    render_node: PathBuf,
    pending: Option<Picture>,
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
}

impl State {
    fn surface(&self) -> Option<&wl_surface::WlSurface> {
        self.layer.as_ref().map(|l| l.wl_surface())
    }

    /// Takes the keyboard and the shortcuts, or gives them back.
    fn focus(&mut self, focus: Focus) {
        let Some(layer) = &self.layer else {
            return;
        };
        match focus {
            Focus::Take => {
                layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
                if self.inhibitor.is_none()
                    && let Some(inhibit) = &self.inhibit
                {
                    let surface = layer.wl_surface();
                    self.inhibitor =
                        Some(inhibit.inhibit_shortcuts(surface, &self.seat, &self.qh, ()));
                }
            }
            Focus::Give => {
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                if let Some(inhibitor) = self.inhibitor.take() {
                    inhibitor.destroy();
                }
            }
        }
        layer.commit();
    }

    fn ours(&self, surface: &wl_surface::WlSurface) -> bool {
        self.surface() == Some(surface)
    }

    /// Shows the waiting picture if the compositor is ready for one.
    fn present(&mut self) {
        let Some((width, height)) = self.size else {
            return;
        };
        if self.waiting_frame {
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
                let (buffer, _) = params.create_immed(
                    frame.width as i32,
                    frame.height as i32,
                    frame.fourcc as u32,
                    zwp_linux_buffer_params_v1::Flags::empty(),
                    &qh,
                );
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
        // The picture is in the monitor's mode, unturned, like the screen it was captured from:
        // the compositor applies the monitor's transform, flips and half turns included.
        surface.set_buffer_transform(self.transform);
        if let Some(viewport) = &self.viewport {
            viewport.set_destination(width as i32, height as i32);
        }
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        surface.frame(&qh, surface.clone());
        surface.commit();
        self.waiting_frame = true;
    }

    fn shm_buffer(
        &mut self,
        width: u32,
        height: u32,
        draw: impl FnOnce(&mut [u8]),
    ) -> Option<Buffer> {
        let len = (width * height * 4) as usize;
        if self.pool.is_none() {
            self.pool = SlotPool::new(len * 2, &self.shm).ok();
        }
        let pool = self.pool.as_mut()?;
        let (buffer, canvas) = pool
            .create_buffer(
                width as i32,
                height as i32,
                width as i32 * 4,
                wl_shm::Format::Xrgb8888,
            )
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
                let _ = ready.send(Ok((size, self.linear_dmabuf.unwrap_or(false))));
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
        let linear = takes_linear_xrgb(feedback.tranches().iter().flat_map(|tranche| {
            tranche
                .formats
                .iter()
                .filter_map(|&i| table.get(i as usize))
                .map(|f| (f.format, f.modifier))
        }));
        self.linear_dmabuf = Some(linear && same_gpu(feedback.main_device(), &self.render_node));
    }

    fn created(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        _: wl_buffer::WlBuffer,
    ) {
    }

    fn failed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    ) {
        tracing::warn!("the compositor refused a decoded picture");
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
            state.pointer = Some(seat.get_pointer(qh, ()));
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
                surface,
                surface_x,
                surface_y,
                ..
            } if state.ours(&surface) => {
                let focus = state.capture.enter(surface_x, surface_y);
                state.focus(focus);
            }
            Event::Leave { surface, .. } if state.ours(&surface) => {
                let focus = state.capture.leave();
                state.focus(focus);
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
                Ok(text) => state.capture.keymap(text),
                Err(e) => tracing::warn!("cannot read home's keymap: {e}"),
            },
            Event::Leave { surface, .. } if state.ours(&surface) => state.capture.keyboard_left(),
            Event::Key {
                key,
                state: WEnum::Value(pressed),
                ..
            } => {
                let pressed = pressed == wl_keyboard::KeyState::Pressed;
                if let Some(focus) = state.capture.key(key, pressed) {
                    state.focus(focus);
                }
            }
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

    #[test]
    fn a_device_number_is_read_as_sysfs_names_it() {
        // makedev(226, 128): the first render node.
        assert_eq!(char_device((226 << 8) | 128), "226:128");
        // makedev(226, 300): the minor's high bits sit above the major.
        assert_eq!(char_device((256 << 12) | (226 << 8) | 44), "226:300");
    }

    #[test]
    fn a_compositor_with_only_tiled_dmabufs_gets_a_copy() {
        assert!(!takes_linear_xrgb([
            (XRGB8888, BLOCK_LINEAR),
            (XRGB8888, 0x00ff_ffff_ffff_ffff)
        ]));
    }

    #[test]
    fn a_compositor_that_takes_linear_xrgb_gets_the_dmabuf() {
        assert!(takes_linear_xrgb([
            (XRGB8888, BLOCK_LINEAR),
            (XRGB8888, LINEAR)
        ]));
    }
}
