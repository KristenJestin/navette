//! Home's surface on the reserved monitor (the kind chosen in #2): a layer-shell surface on the
//! overlay layer, bound to that output, anchored on every edge, above the bar. A GPU picture is
//! a dmabuf handed to the compositor as is (linux-dmabuf), with no copy; a CPU picture goes
//! through shared memory, as does a GPU picture when the compositor does not take the decoder's
//! linear dmabufs (Hyprland on an NVIDIA GPU lists only tiled ones). The newest picture wins: one is shown per frame callback, and an
//! older one still waiting is dropped, which gives its image back to the decoder.
//!
//! The keyboard (focus, the shortcuts inhibitor) comes with the input issue (#7); the surface
//! already takes it on demand, as #2 measured.

use std::collections::HashMap;
use std::io;
use std::os::fd::AsFd;
use std::thread::JoinHandle;

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
use wayland_client::protocol::{wl_buffer, wl_output, wl_shm, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

use super::decode::Picture;

const XRGB8888: u32 = drm_fourcc::DrmFourcc::Xrgb8888 as u32;
const LINEAR: u64 = 0;

/// Whether the compositor's formats, as (fourcc, modifier), hold the decoder's linear XRGB8888.
fn takes_linear_xrgb(formats: impl IntoIterator<Item = (u32, u64)>) -> bool {
    formats.into_iter().any(|f| f == (XRGB8888, LINEAR))
}

/// The layer surface's namespace, for layer rules.
pub const NAMESPACE: &str = "navette";

/// The surface, on its own thread. Dropping it removes the surface.
pub struct Surface {
    /// The compositor takes a linear XRGB8888 dmabuf, the decoder's output.
    linear_dmabuf: bool,
    pictures: Option<channel::Sender<Picture>>,
    thread: Option<JoinHandle<()>>,
}

impl Surface {
    /// Puts the surface on `output` and waits until the compositor has placed it.
    pub fn open(output: &str) -> io::Result<Self> {
        let (pictures, channel) = channel::channel();
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let output = output.to_string();
        let thread = std::thread::Builder::new()
            .name("navette-surface".into())
            .spawn(move || {
                if let Err(e) = run(&output, channel, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })?;
        let mut surface = Self {
            linear_dmabuf: false,
            pictures: Some(pictures),
            thread: Some(thread),
        };
        match ready.recv() {
            Ok(Ok((size, linear_dmabuf))) => {
                tracing::debug!(?size, linear_dmabuf, "surface placed");
                surface.linear_dmabuf = linear_dmabuf;
                Ok(surface)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(io::Error::other("the surface thread ended")),
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

/// The surface's size once placed, and whether the compositor takes linear dmabufs.
type Ready = io::Result<((u32, u32), bool)>;

fn wayland(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("wayland: {e}"))
}

fn run(
    output_name: &str,
    pictures: Channel<Picture>,
    ready: &std::sync::mpsc::Sender<Ready>,
) -> io::Result<()> {
    let conn = Connection::connect_to_env().map_err(wayland)?;
    let (globals, queue) = registry_queue_init::<State>(&conn).map_err(wayland)?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).map_err(wayland)?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|_| io::Error::other("the compositor has no layer shell"))?;
    let viewporter: wp_viewporter::WpViewporter = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| io::Error::other("the compositor has no viewporter"))?;
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
        pending: None,
        waiting_frame: false,
        shown_shm: None,
        shown_dmabuf: HashMap::new(),
        exit: false,
        qh: qh.clone(),
    };

    if state.dmabuf.version().is_some_and(|v| v >= 4) {
        let _ = state.dmabuf.get_default_feedback(&qh);
    }
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), queue)
        .insert(event_loop.handle())
        .map_err(|e| wayland(e.error))?;
    // The output list arrives with the first round trips.
    event_loop.dispatch(Some(std::time::Duration::from_millis(100)), &mut state)?;
    conn.roundtrip().map_err(wayland)?;
    event_loop.dispatch(Some(std::time::Duration::from_millis(100)), &mut state)?;

    // Before version 4, the formats came as a list.
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
    layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
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
            ChannelEvent::Closed => state.exit = true,
        })
        .map_err(|e| wayland(e.error))?;

    while !state.exit {
        event_loop.dispatch(None, &mut state)?;
    }
    Ok(())
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
    pending: Option<Picture>,
    /// A picture was committed and its frame callback has not come yet.
    waiting_frame: bool,
    shown_shm: Option<Buffer>,
    /// Dmabuf pictures the compositor still holds, by buffer.
    shown_dmabuf: HashMap<ObjectId, (wl_buffer::WlBuffer, Picture)>,
    exit: bool,
    qh: QueueHandle<State>,
}

impl State {
    fn surface(&self) -> Option<&wl_surface::WlSurface> {
        self.layer.as_ref().map(|l| l.wl_surface())
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
        let (buffer_w, buffer_h) = match &picture {
            Picture::Dmabuf(f) => (f.width, f.height),
            Picture::Bgra(f) => (f.width, f.height),
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
        // A picture of the unrotated mode on a turned monitor is turned by the compositor.
        let turned = (buffer_w > buffer_h) != (width > height);
        surface.set_buffer_transform(if turned {
            self.transform
        } else {
            wl_output::Transform::Normal
        });
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
        // The monitor went away.
        self.exit = true;
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
        self.linear_dmabuf = Some(linear);
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
delegate_noop!(State: wp_viewport::WpViewport);

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK_LINEAR: u64 = 0x0300_0000_0060_6015;

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
