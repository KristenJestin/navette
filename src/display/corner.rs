//! The corner on screen (see [`crate::corner`]): drawn in shared memory, again only when it
//! changes and at each of the compositor's frames while its check moves. Its input region is the
//! mic button alone, so the pointer anywhere else goes where it would without it.
//!
//! On a reserved monitor it is a subsurface of the remote's picture ([`super::surface`]); on each
//! of home's own monitors, a small layer surface of its own, [`HomeCorner`]: on the overlay
//! layer, anchored top left, with no keyboard, `local_bar` pixels from the top over the exclusive
//! zones there (home's bars), or placed by the compositor below them with `local_bar = auto`.

use std::io;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputInfo, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::{self, Event as ChannelEvent};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle, RegistrationToken};
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
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_callback, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

use crate::corner::{Corner, Feed, Next, Side, Update};

/// `BTN_LEFT`: the button that clicks the mic button, or the bar of the files crossing.
pub(super) const BTN_LEFT: u32 = 0x110;
/// The layer surface's namespace, for layer rules.
pub const NAMESPACE: &str = "navette-corner";

/// The corner drawn on its surface.
pub(super) struct Painter {
    pub corner: Corner,
    surface: wl_surface::WlSurface,
    viewport: wp_viewport::WpViewport,
    pool: Option<SlotPool>,
    shown: Option<Buffer>,
    /// The scale it was last drawn at.
    scale: Option<f64>,
    /// The size it is always drawn at, if it is: otherwise, the size of what it shows.
    room: Option<(u32, u32)>,
    /// A frame was asked of the compositor and has not come yet.
    framing: bool,
}

impl Painter {
    pub fn new(
        corner: Corner,
        surface: wl_surface::WlSurface,
        viewport: wp_viewport::WpViewport,
    ) -> Self {
        Self {
            corner,
            surface,
            viewport,
            pool: None,
            shown: None,
            scale: None,
            framing: false,
            room: None,
        }
    }

    /// Always drawn at `room` logical pixels, what it shows at the top left.
    pub fn in_room(mut self, room: (u32, u32)) -> Self {
        self.room = Some(room);
        self
    }

    pub fn surface(&self) -> &wl_surface::WlSurface {
        &self.surface
    }

    pub fn scale(&self) -> Option<f64> {
        self.scale
    }

    /// Asks the compositor for its next frame when `next` waits for it, unless asked already;
    /// before the commit.
    pub fn ask_frame<D>(&mut self, next: Option<Next>, qh: &QueueHandle<D>)
    where
        D: Dispatch<wl_callback::WlCallback, wl_surface::WlSurface> + 'static,
    {
        if next == Some(Next::Frame) && !self.framing {
            self.surface.frame(qh, self.surface.clone());
            self.framing = true;
        }
    }

    /// A frame came for `surface`: true when it is the corner's, to be drawn again.
    pub fn framed(&mut self, surface: &wl_surface::WlSurface) -> bool {
        let ours = *surface == self.surface;
        if ours {
            self.framing = false;
        }
        ours
    }

    /// Draws the corner at `now` and attaches it, with the button as its input region; the
    /// caller commits. Its size in logical pixels.
    pub fn paint(
        &mut self,
        now: Instant,
        scale: f64,
        shm: &Shm,
        compositor: &CompositorState,
    ) -> Option<(u32, u32)> {
        let size = self.room.unwrap_or_else(|| self.corner.size(now));
        let canvas = self.corner.draw_in(now, scale, size);
        let len = canvas.argb.len() * 4;
        if self.pool.is_none() {
            self.pool = SlotPool::new(len * 2, shm).ok();
        }
        let pool = self.pool.as_mut()?;
        let (width, height) = (canvas.width as i32, canvas.height as i32);
        let (buffer, bytes) = pool
            .create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
            .ok()?;
        canvas.write(&mut bytes[..len]);
        buffer.attach_to(&self.surface).ok()?;
        self.viewport.set_destination(size.0 as i32, size.1 as i32);
        let region = Region::new(compositor).ok()?;
        for area in [self.corner.button(), self.corner.bar()]
            .into_iter()
            .flatten()
        {
            region.add(
                area.x as i32,
                area.y as i32,
                area.width.ceil() as i32,
                area.height.ceil() as i32,
            );
        }
        self.surface.set_input_region(Some(region.wl_region()));
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.shown = Some(buffer);
        self.scale = Some(scale);
        Some(size)
    }
}

/// A state that shows a corner, drawn again at the compositor's frames while it moves.
pub(super) trait Shows: Sized + 'static {
    fn timer(&mut self) -> &mut Option<RegistrationToken>;
    fn handle(&self) -> LoopHandle<'static, Self>;
    /// Draws the corner again and commits it.
    fn show_corner(&mut self);
}

/// Draws the corner again after the time `next` says; at a frame, the frame asked does it; with
/// none, only when something changes.
pub(super) fn draw_again<D: Shows>(state: &mut D, next: Option<Next>) {
    let handle = state.handle();
    if let Some(token) = state.timer().take() {
        handle.remove(token);
    }
    let Some(Next::After(after)) = next else {
        return;
    };
    let token = handle.insert_source(Timer::from_duration(after), |_, _, state: &mut D| {
        // Fired: nothing to remove when it is drawn again.
        *state.timer() = None;
        state.show_corner();
        TimeoutAction::Drop
    });
    *state.timer() = token.ok();
}

/// What the board says, heard on the corner's thread.
pub(super) fn listen<D: Shows>(
    feed: &Feed,
    handle: &LoopHandle<'static, D>,
    mut heard: impl FnMut(&mut D, Update) + 'static,
) -> io::Result<()> {
    let (updates, channel) = channel::channel();
    handle
        .insert_source(channel, move |event, _, state| {
            if let ChannelEvent::Msg(update) = event {
                heard(state, update);
            }
        })
        .map_err(|e| io::Error::other(e.error.to_string()))?;
    feed.listen(move |update| updates.send(update.clone()).is_ok());
    Ok(())
}

/// A monitor's pixels per logical pixel.
pub(super) fn scale_of(info: &OutputInfo) -> f64 {
    let pixels = info.modes.iter().find(|m| m.current).map(|m| {
        let (width, height) = m.dimensions;
        let turned = matches!(
            info.transform,
            wl_output::Transform::_90
                | wl_output::Transform::_270
                | wl_output::Transform::Flipped90
                | wl_output::Transform::Flipped270
        );
        if turned { height } else { width }
    });
    match (pixels, info.logical_size) {
        (Some(pixels), Some((width, _))) if width > 0 => pixels as f64 / width as f64,
        _ => info.scale_factor.max(1) as f64,
    }
}

/// Where home's corner sits on its monitor: its layer surface's anchor, exclusive zone and top
/// margin.
#[derive(Debug, PartialEq)]
struct Place {
    anchor: Anchor,
    exclusive: i32,
    top: i32,
}

/// Where home's corner sits with `local_bar` (see [`crate::corner::Look`]).
fn place(home_bar: Option<i32>) -> Place {
    let anchor = Anchor::TOP | Anchor::LEFT;
    match home_bar {
        // An exclusive zone of 0: the compositor places it after the space the bars reserve.
        None => Place {
            anchor,
            exclusive: 0,
            top: 0,
        },
        // -1: the space the bars reserve is ignored, the margin counts from the monitor's top.
        Some(bar) => Place {
            anchor,
            exclusive: -1,
            top: bar,
        },
    }
}

/// The size home's layer surface is given, in logical pixels, whatever the corner shows:
/// Hyprland animates a layer surface's new size, and stretches the buffer already drawn at that
/// size to the animated one, so a corner that grew with it looked squashed.
fn layer_size(corner: &Corner) -> (u32, u32) {
    corner.room()
}

/// The corner on one of home's own monitors, on its own thread. Dropping it removes it.
pub struct HomeCorner {
    stop: Option<channel::Sender<()>>,
    /// The compositor's socket: shut down at the drop, a thread still waiting on it ends.
    socket: UnixStream,
    /// Set at the drop: the thread's end is no failure then.
    dropped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HomeCorner {
    /// Puts the corner on `output`. It goes up on its own thread, never holding the mode's start:
    /// a corner that cannot go up says why and leaves the mode as it is.
    pub fn open(output: &str, feed: Feed) -> io::Result<Self> {
        Self::open_on(super::surface::compositor()?, output, feed)
    }

    fn open_on(socket: UnixStream, output: &str, feed: Feed) -> io::Result<Self> {
        let shutdown = socket.try_clone()?;
        let conn = Connection::from_socket(socket).map_err(wayland)?;
        let (stop, stopped) = channel::channel();
        let output = output.to_string();
        let dropped = Arc::new(AtomicBool::new(false));
        let gone = dropped.clone();
        let thread = std::thread::Builder::new()
            .name("navette-corner".into())
            .spawn(move || {
                if let Err(e) = run(conn, &output, feed, stopped)
                    && !gone.load(Ordering::Relaxed)
                {
                    eprintln!("navette: no corner on {output}: {e}");
                }
            })?;
        Ok(Self {
            stop: Some(stop),
            socket: shutdown,
            dropped,
            thread: Some(thread),
        })
    }
}

impl Drop for HomeCorner {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Relaxed);
        self.stop = None;
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn wayland(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("wayland: {e}"))
}

fn run(
    conn: Connection,
    output_name: &str,
    feed: Feed,
    stopped: channel::Channel<()>,
) -> io::Result<()> {
    let (globals, queue) = registry_queue_init::<State>(&conn).map_err(wayland)?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).map_err(wayland)?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|_| io::Error::other("the compositor has no layer shell"))?;
    let viewporter: wp_viewporter::WpViewporter = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| io::Error::other("the compositor has no viewporter"))?;
    let _seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=7, ())
        .map_err(|_| io::Error::other("the compositor has no seat"))?;
    let shapes: Option<WpCursorShapeManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), queue)
        .insert(event_loop.handle())
        .map_err(|e| wayland(e.error))?;
    let surface = compositor.create_surface(&qh);
    let viewport = viewporter.get_viewport(&surface, &qh, ());
    let mut state = State {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh).map_err(wayland)?,
        compositor,
        painter: {
            let corner = feed.corner(Side::Home);
            let room = layer_size(&corner);
            Painter::new(corner, surface.clone(), viewport).in_room(room)
        },
        feed: feed.clone(),
        layer: None,
        configured: false,
        scale: 1.0,
        handle: event_loop.handle(),
        qh: qh.clone(),
        timer: None,
        pointer: None,
        shapes,
        shape: None,
        exit: false,
    };
    // The outputs and their names arrive over the first round trips.
    let mut found = None;
    for _ in 0..20 {
        event_loop.dispatch(Some(Duration::from_millis(25)), &mut state)?;
        found = state
            .outputs
            .outputs()
            .find(|o| state.outputs.info(o).and_then(|i| i.name).as_deref() == Some(output_name));
        if found.is_some() {
            break;
        }
    }
    let output =
        found.ok_or_else(|| io::Error::other(format!("no monitor {output_name} at home")))?;
    state.scale = state
        .outputs
        .info(&output)
        .map_or(1.0, |info| scale_of(&info));
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some(NAMESPACE),
        Some(&output),
    );
    let place = place(feed.look().home_bar);
    layer.set_anchor(place.anchor);
    layer.set_exclusive_zone(place.exclusive);
    layer.set_margin(place.top, 0, 0, 0);
    let (width, height) = layer_size(&state.painter.corner);
    layer.set_size(width, height);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.commit();
    state.layer = Some(layer);
    listen(&feed, &event_loop.handle(), |state: &mut State, update| {
        state.painter.corner.update(update, Instant::now());
        state.show_corner();
    })?;
    event_loop
        .handle()
        .insert_source(stopped, |event, _, state| {
            if let ChannelEvent::Closed = event {
                state.exit = true;
            }
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
    compositor: CompositorState,
    painter: Painter,
    feed: Feed,
    layer: Option<LayerSurface>,
    configured: bool,
    scale: f64,
    handle: LoopHandle<'static, State>,
    qh: QueueHandle<State>,
    timer: Option<RegistrationToken>,
    pointer: Option<wl_pointer::WlPointer>,
    shapes: Option<WpCursorShapeManagerV1>,
    shape: Option<WpCursorShapeDeviceV1>,
    exit: bool,
}

impl Shows for State {
    fn timer(&mut self) -> &mut Option<RegistrationToken> {
        &mut self.timer
    }

    fn handle(&self) -> LoopHandle<'static, Self> {
        self.handle.clone()
    }

    fn show_corner(&mut self) {
        if !self.configured {
            return;
        }
        let now = Instant::now();
        if self
            .painter
            .paint(now, self.scale, &self.shm, &self.compositor)
            .is_none()
        {
            return;
        }
        let next = self.painter.corner.next_draw(now);
        self.painter.ask_frame(next, &self.qh);
        if let Some(layer) = &self.layer {
            layer.commit();
        }
        draw_again(self, next);
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

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.painter.framed(surface) {
            self.show_corner();
        }
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
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        if !self.configured {
            self.configured = true;
            self.show_corner();
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
delegate_layer!(State);
delegate_registry!(State);
delegate_noop!(State: wp_viewporter::WpViewporter);
delegate_noop!(State: wp_viewport::WpViewport);
delegate_noop!(State: WpCursorShapeManagerV1);
delegate_noop!(State: WpCursorShapeDeviceV1);

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
            && caps.contains(wl_seat::Capability::Pointer)
            && state.pointer.is_none()
        {
            let pointer = seat.get_pointer(qh, ());
            state.shape = state
                .shapes
                .as_ref()
                .map(|shapes| shapes.get_pointer(&pointer, qh, ()));
            state.pointer = Some(pointer);
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
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(shape) = &state.shape {
                    shape.set_shape(serial, wp_cursor_shape_device_v1::Shape::Pointer);
                }
                state.painter.corner.point_at(surface_x, surface_y);
                if state.painter.corner.hover(true) {
                    state.show_corner();
                }
            }
            Event::Leave { .. } => {
                if state.painter.corner.hover(false) {
                    state.show_corner();
                }
            }
            Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.painter.corner.point_at(surface_x, surface_y),
            Event::Button {
                button: BTN_LEFT,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => state.feed.press(state.painter.corner.press()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_home_corner_the_compositor_never_places_does_not_hold_the_start() {
        let (ours, _silent) = UnixStream::pair().unwrap();
        let begin = Instant::now();
        let corner = HomeCorner::open_on(
            ours,
            "DP-2",
            Feed::new(Arc::default(), None, Default::default()),
        );
        assert!(corner.is_ok(), "it goes up on its own time");
        assert!(begin.elapsed() < Duration::from_millis(500));
        drop(corner);
        assert!(begin.elapsed() < Duration::from_secs(1), "nor its stop");
    }

    #[test]
    fn home_s_layer_surface_keeps_one_size_while_the_corner_grows() {
        use crate::clipboard::progress::Bar;
        use crate::corner::Update;
        use crate::mic::Status;

        let now = Instant::now();
        let feed = Feed::new(
            Arc::default(),
            Some("SUPER + CTRL + M".into()),
            Default::default(),
        );
        let mut corner = feed.corner(Side::Home);
        corner.update(Update::Mic(Status::Home), now);
        let first = layer_size(&corner);
        let mut sizes = vec![corner.size(now)];
        // Hovered: the hint shows under the button.
        corner.hover(true);
        sizes.push(corner.size(now));
        corner.update(Update::Mic(Status::Remote), now);
        sizes.push(corner.size(now));
        corner.update(Update::Say("x".repeat(200)), now);
        sizes.push(corner.size(now));
        corner.update(
            Update::Files(Some(Bar {
                done: 1,
                total: u64::MAX,
            })),
            now,
        );
        sizes.push(corner.size(now));
        assert!(sizes.windows(2).any(|w| w[0] != w[1]), "the corner grows");
        assert_eq!(layer_size(&corner), first);
        for (width, height) in sizes {
            assert!(
                width <= first.0 && height <= first.1,
                "{width}x{height} in {first:?}"
            );
        }
    }

    #[test]
    fn home_s_corner_sits_below_home_s_bars_with_auto_and_over_them_with_a_height() {
        let top_left = Anchor::TOP | Anchor::LEFT;
        assert_eq!(
            place(None),
            Place {
                anchor: top_left,
                exclusive: 0,
                top: 0
            }
        );
        for bar in [0, 38] {
            assert_eq!(
                place(Some(bar)),
                Place {
                    anchor: top_left,
                    exclusive: -1,
                    top: bar
                }
            );
        }
    }

    #[test]
    fn with_no_home_bar_home_s_corner_sits_at_the_top_over_home_s_bars() {
        assert_eq!(
            place(crate::corner::Look::default().home_bar),
            Place {
                anchor: Anchor::TOP | Anchor::LEFT,
                exclusive: -1,
                top: 0
            }
        );
    }
}
