//! The mode, at home: the paired machine takes the home monitors named in the config, one or
//! more. `navette mode start` checks everything first (the config, the monitors, the link) and
//! changes nothing when a check fails; then home covers each monitor with its surface, asks the
//! remote for one output per monitor, in home's left-to-right order, and shows each stream on
//! its surface. `navette mode stop`, or the link dropping, gives the monitors back; so does any
//! one surface going away. Home monitors are never moved or reconfigured: the changes to the
//! home desk are the monitors' workspaces, moved whole to another home monitor and back (#48,
//! with no slide at the first visit, #118),
//! a window rule per monitor that opens home windows on that home monitor rather than hidden
//! under a surface (#2), the layer rules that draw the remote's picture above home's overlays but
//! under the tools the user starts (#116) and, with a mic, the hotkey that switches it, made
//! through the desk recorder so they are undone on stop, on a dropped link and after a crash,
//! and made again after a config reload. A home window that lands on a given monitor all the
//! same (moved, dragged, opened on an explicit workspace) is sent back to a home monitor as
//! Hyprland reports it (#31). The remote's sound plays at home, and the home mic goes to the
//! remote machine, for as long as the mode runs. Every home monitor shows its corner meanwhile
//! (#69), and a notification says when the mode is up and when it stops.
//!
//! `navette serve` owns the link and the surface, so the two commands reach it over a Unix
//! socket in `$XDG_RUNTIME_DIR/navette`, private to the user.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gliff_vk::Gpu;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use crate::corner::{Board, Feed};
use crate::desk::{Change, Desk, DeskError};
use crate::display::decode::{self, Decoding, Picture, Video};
use crate::display::output;
use crate::display::surface::NAMESPACE as SURFACE_NAMESPACE;
use crate::display::wire::{Display, Incoming, MAX_OUTPUTS, Msg, OutputCopy, Peer, Pointer};
use crate::display::{Ended, Keyboard, Released};
use crate::hypr::{self, Eval, Hypr, lua_string};
use crate::input::{self, Hotkey};
use crate::notify::Notifier;

mod config;

pub use config::list;
use config::{Mode, resolve};

/// The `order` of the layer rule on Navette's picture: lower than any of home's overlays, which
/// have 0 unless the user's own rules say otherwise, so it is drawn after them (#116).
const PICTURE_ORDER: i32 = -1000;
/// The `order` of the `local_above` layers: drawn after the picture.
const LOCAL_ABOVE_ORDER: i32 = -2000;

/// How long the remote has to make its output.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// The remote makes its outputs one after the other.
fn start_timeout(outputs: usize) -> Duration {
    START_TIMEOUT * outputs as u32
}
/// How long `navette mode` waits for `navette serve`: its checks, the surfaces, the remote
/// making the most outputs a mode has.
const ORDER_TIMEOUT: Duration =
    Duration::from_secs(20).saturating_add(START_TIMEOUT.saturating_mul(MAX_OUTPUTS as u32));
/// How long, and how much, `navette serve` reads of an order.
const ORDER_READ: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(5)
};
const ORDER_LONGEST: u64 = 64;
/// With `mic = follow`, how long the keyboard stays on a side before the mic goes there: a
/// pointer only passing over a monitor moves nothing, as each switch shows in the corners and
/// makes a gap in a call.
const MIC_FOLLOWS: Duration = if cfg!(test) {
    Duration::from_millis(150)
} else {
    Duration::from_millis(500)
};

/// The home mic, which goes to the remote machine while a mode runs (the port behind which
/// tests put a fake). Its calls wait for PipeWire and the link: they are made off the executor.
pub trait Mic: Send + Sync {
    /// The mic goes to the remote machine, or stays home unless `remote`.
    fn start(&self, remote: bool) -> io::Result<()>;
    /// It goes to that side, as the hotkey sends it, unless it is there already.
    fn follow(&self, remote: bool) -> io::Result<()>;
    /// It comes back home.
    fn stop(&self);
    /// The shell command the hotkey runs to switch it.
    fn toggle_command(&self) -> String;
}

/// The mic sent to the remote machine, until dropped.
struct MicSent(Arc<dyn Mic>);

impl Drop for MicSent {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// The remote's sound, played at home while a mode runs (the port behind which tests put a
/// fake). Its calls wait for PipeWire and the link: they are made off the executor.
pub trait Sound: Send + Sync {
    /// The remote's sound comes home.
    fn start(&self) -> io::Result<()>;
    /// The remote plays it on its own output again.
    fn stop(&self);
}

/// The remote's sound played at home, until dropped.
struct SoundPlayed(Arc<dyn Sound>);

impl Drop for SoundPlayed {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// Covers a home monitor (the port behind which tests put a fake).
pub trait Screens: Send + Sync {
    /// Covers `monitor`; the guard takes the cover away when dropped. Home's input over it goes
    /// to `input`, until `hotkey`, which `released` then hears. `ended` hears it if the cover
    /// goes away on its own. The cover shows the corner `feed` tells.
    fn open(
        &self,
        monitor: &str,
        input: input::Queue,
        hotkey: (Option<Hotkey>, Released),
        ended: Ended,
        feed: Feed,
    ) -> io::Result<Box<dyn Screen>>;

    /// Shows the corner on `monitor`, one of home's own, until the guard is dropped.
    fn corner(&self, monitor: &str, feed: Feed) -> io::Result<Box<dyn Send>>;
}

pub trait Screen: Send {
    fn pictures(&self) -> Box<dyn FnMut(Picture) + Send>;
    /// The remote pointer: its image, shown at home's pointer over the cover.
    fn cursor(&self, pointer: Pointer);
    /// The layouts in which the GPU's pictures can be handed over as dmabufs, with no copy.
    fn dmabuf_modifiers(&self) -> Vec<u64>;
    /// Set once the compositor refused a dmabuf: the GPU's pictures are read back from then on.
    fn dmabuf_refused(&self) -> Arc<AtomicBool>;
    /// Has the compositor sort the monitor's layers again: Hyprland 0.56.2 applies a layer
    /// rule's `order` only when a layer maps, unmaps or commits layer-shell state.
    fn poke(&self);
}

/// The real cover: a layer surface on the monitor.
pub struct LiveScreens;

impl Screens for LiveScreens {
    fn open(
        &self,
        monitor: &str,
        input: input::Queue,
        hotkey: (Option<Hotkey>, Released),
        ended: Ended,
        feed: Feed,
    ) -> io::Result<Box<dyn Screen>> {
        Ok(Box::new(crate::display::surface::Surface::open(
            monitor, input, hotkey, ended, feed,
        )?))
    }

    fn corner(&self, monitor: &str, feed: Feed) -> io::Result<Box<dyn Send>> {
        Ok(Box::new(crate::display::corner::HomeCorner::open(
            monitor, feed,
        )?))
    }
}

impl Screen for crate::display::surface::Surface {
    fn pictures(&self) -> Box<dyn FnMut(Picture) + Send> {
        crate::display::surface::Surface::pictures(self)
    }

    fn cursor(&self, pointer: Pointer) {
        crate::display::surface::Surface::cursor(self, pointer)
    }

    fn dmabuf_modifiers(&self) -> Vec<u64> {
        crate::display::surface::Surface::dmabuf_modifiers(self)
    }

    fn dmabuf_refused(&self) -> Arc<AtomicBool> {
        crate::display::surface::Surface::dmabuf_refused(self)
    }

    fn poke(&self) {
        crate::display::surface::Surface::poke(self)
    }
}

/// An order from `navette mode start|stop` or `navette status`, with where to answer.
pub enum Order {
    /// The mode called so; with no name, the one the config has.
    Start(Option<String>, oneshot::Sender<io::Result<String>>),
    Stop(oneshot::Sender<io::Result<String>>),
    Status(oneshot::Sender<io::Result<String>>),
}

/// What home needs to run a mode.
pub struct Home {
    pub config_dir: PathBuf,
    pub hypr: Arc<dyn Hypr>,
    pub screens: Arc<dyn Screens>,
    /// The GPU that decodes, or none for the CPU.
    pub gpu: Arc<HomeGpu>,
    /// Where the desk recorder keeps what it must undo.
    pub desk_record: PathBuf,
    /// The mic, when home plays the remote's sound.
    pub mic: Option<Arc<dyn Mic>>,
    /// The remote's sound, with `navette serve --audio`.
    pub sound: Option<Arc<dyn Sound>>,
    /// What the corners show: where the mic is, and short messages.
    pub board: Arc<Board>,
    /// Says, once each, that a mode started and stopped.
    pub notifier: Arc<dyn Notifier>,
}

/// Home's GPU, kept from one mode to the next until a decoder lost it. A lost device fails
/// every decode, so the next mode must not build its first decoder on it.
pub struct HomeGpu<T = Arc<Gpu>> {
    gpu: Mutex<Option<T>>,
    /// Set by the decoders when one was lost.
    lost: Arc<AtomicBool>,
}

impl<T: Clone> HomeGpu<T> {
    pub fn new(gpu: Option<T>) -> Self {
        Self {
            gpu: Mutex::new(gpu),
            lost: Arc::default(),
        }
    }

    /// What the decoders set when a GPU decoder is lost.
    pub fn lost(&self) -> Arc<AtomicBool> {
        self.lost.clone()
    }

    /// The GPU for a mode, opened again first if a decoder lost it since. When it cannot be
    /// opened again, the mode decodes on the CPU and the next one tries again.
    pub fn current(&self, open: impl FnOnce() -> Option<T>) -> Option<T> {
        let mut gpu = self.gpu.lock().unwrap();
        if self.lost.load(Ordering::Relaxed) {
            *gpu = open();
            if gpu.is_some() {
                self.lost.store(false, Ordering::Relaxed);
            }
        }
        gpu.clone()
    }
}

/// The workspace each given monitor showed when home windows were last sent home.
type Seen = Vec<(String, i64)>;

struct Active {
    /// Home first, before the rest goes.
    _mic: Option<MicSent>,
    _sound: Option<SoundPlayed>,
    /// The mode's name, `None` for the unnamed one.
    name: Option<String>,
    /// What was stopped to start this one, said with the answer to `navette mode start`.
    switched: Option<String>,
    /// Said to the remote with the start, and back with its answers.
    generation: u32,
    /// The monitors given, left to right: monitor `i` shows output `i`.
    monitors: Vec<String>,
    /// The home monitor that takes the home windows the cover would hide; none when every
    /// monitor is given.
    home_monitor: Option<String>,
    seen: Arc<Mutex<Seen>>,
    shown: Vec<Shown>,
    /// The corners on home's own monitors.
    _corners: Vec<Box<dyn Send>>,
    desk: Arc<Mutex<Desk<Eval>>>,
    /// Why a surface or a decoder cannot go on.
    ended: UnboundedReceiver<String>,
    /// Each time the release hotkey gave the keyboard back, and a surface took it again; each
    /// time the pointer came over a reserved monitor or left it.
    released: UnboundedReceiver<Keyboard>,
    /// `mic = follow`, with a mic to switch.
    mic_follows: bool,
    /// With `mic = follow`, the side the keyboard went to last, whether the remote's, and when
    /// the mic follows it there.
    keyboard: Option<(bool, tokio::time::Instant)>,
    /// The `navette mode start` waiting for the remote's answer.
    starting: Option<(oneshot::Sender<io::Result<String>>, tokio::time::Instant)>,
    /// What the config says that is no longer read, said with `mode start`'s answer.
    notice: Option<String>,
    /// Says the mode stopped, once it said it started: last, once the rest is gone.
    told: Option<Told>,
}

/// The notification that a mode stopped, shown when the mode goes, however it ends.
struct Told(Arc<dyn Notifier>, String);

impl Drop for Told {
    fn drop(&mut self) {
        self.0.show(&self.1, "");
    }
}

/// One monitor's cover and the decoder of its output's stream.
struct Shown {
    // Field order is drop order: the decoder stops before the surface goes.
    decoding: Decoding,
    screen: Box<dyn Screen>,
}

impl Active {
    /// The monitors, as said to the user: `DP-2` or `DP-2, DP-3`.
    fn label(&self) -> String {
        self.monitors.join(", ")
    }

    /// The mode, as said to the user.
    fn title(&self) -> String {
        match &self.name {
            Some(name) => format!("mode `{name}`"),
            None => "the mode".to_string(),
        }
    }

    /// A start that failed, said with what the switch that led to it already did.
    fn start_error(&self, e: io::Error) -> io::Error {
        switch_error(self.switched.as_deref(), &self.title(), e)
    }

    /// `DP-2 is back home`, once the monitors are given back.
    fn given_back(&self) -> String {
        let verb = if self.monitors.len() > 1 { "are" } else { "is" };
        format!("{} {verb} back home", self.label())
    }
}

/// An answer of the remote for a mode other than the running one: `generation` is the one it
/// started. An agent that does not number its answers is taken at its word.
fn is_stale(active: Option<&Active>, generation: Option<u32>) -> bool {
    generation.is_some_and(|g| active.is_none_or(|a| a.generation != g))
}

/// A start that failed after a switch said "nothing was changed" of the new mode only: it also
/// says that the first one was stopped and given back.
fn switch_error(switched: Option<&str>, title: &str, e: io::Error) -> io::Error {
    let Some(switched) = switched else { return e };
    let text = e.to_string();
    let why = text.strip_suffix("; nothing was changed").unwrap_or(&text);
    io::Error::new(
        e.kind(),
        format!("{switched}; {title} could not start: {why}"),
    )
}

/// What `navette status` says: the mode, and whether the remote machine is there for the next.
fn status(active: Option<&Active>, peer: Option<&Peer>) -> String {
    let mode = match active {
        None => "no mode is running".to_string(),
        Some(a) if a.starting.is_some() => format!("{} is starting on {}", a.title(), a.label()),
        Some(a) => format!("{} runs on {}", a.title(), a.label()),
    };
    let linked = peer.is_some_and(|p| !p.outbox.is_closed());
    let link = if linked { "connected" } else { "not connected" };
    format!("{mode}; the remote machine is {link}")
}

/// How long the desk waits after a link comes up before it says the remote machine is here. A user
/// whose own setup starts the mode as the laptop links starts it within this moment, and would
/// otherwise see a notification offering what has just happened.
const ARRIVAL_DELAY: Duration = Duration::from_secs(3);

/// Says that the remote machine is here, with a button that starts the default mode. With no
/// mode to start (no `mode.conf`), there is nothing to offer.
fn announce(home: &Home, pressed: &UnboundedSender<()>) {
    let Ok(mode) = Mode::read(&home.config_dir, None) else {
        return;
    };
    let pressed = pressed.clone();
    home.notifier.offer(
        &format!("{} is here", mode.look.remote_label),
        "Start",
        Box::new(move || {
            let _ = pressed.send(());
        }),
    );
}

/// `navette mode start`, whoever asks: the answer goes to `reply` once the remote has answered.
async fn begin(
    home: &Home,
    peer: Option<&Peer>,
    name: Option<&str>,
    reply: oneshot::Sender<io::Result<String>>,
    generation: &mut u32,
    active: &mut Option<Active>,
) {
    *generation = generation.wrapping_add(1);
    match start(home, peer, name, *generation, active).await {
        Ok(()) => {
            if let Some(a) = active {
                let wait = start_timeout(a.monitors.len());
                let at = tokio::time::Instant::now() + wait;
                a.starting = Some((reply, at));
            }
        }
        Err(e) => {
            let _ = reply.send(Err(e));
        }
    }
}

/// Serves the orders and the links, until both channels close.
pub async fn serve(
    home: Home,
    mut peers: UnboundedReceiver<Peer>,
    mut orders: UnboundedReceiver<Order>,
    mut events: UnboundedReceiver<hypr::Event>,
) {
    let mut peer: Option<Peer> = None;
    let mut active: Option<Active> = None;
    // Each start has its own, said to the remote and back: see `Display::Start`.
    let mut generation: u32 = 0;
    // When the desk says the remote machine is here: set by a link coming up, and gone once said,
    // or once the link drops or a mode is asked for.
    let mut here_at: Option<tokio::time::Instant> = None;
    // The offer's button, pressed.
    let (pressed_tx, mut pressed) = tokio::sync::mpsc::unbounded_channel::<()>();
    loop {
        // The link carries the user's clipboard and sound once the remote started the mode,
        // and no longer once home stops it.
        if let Some(peer) = &peer {
            let running = active.as_ref().is_some_and(|a| a.starting.is_none());
            peer.mode
                .send_if_modified(|mode| std::mem::replace(mode, running) != running);
        }
        let deadline = active
            .as_ref()
            .and_then(|a| a.starting.as_ref().map(|(_, at)| *at));
        let mic_follows_at = active.as_ref().and_then(|a| a.keyboard.map(|(_, at)| at));
        tokio::select! {
            // A link that came up is known before the orders that follow it.
            biased;
            new = peers.recv() => match new {
                Some(new) => {
                    link_lost(&mut active, "the link was replaced").await;
                    peer = Some(new);
                    here_at = Some(tokio::time::Instant::now() + ARRIVAL_DELAY);
                }
                None => break,
            },
            heard = next(&mut peer) => match heard {
                Heard::Gone => {
                    peer = None;
                    here_at = None;
                    link_lost(&mut active, "the link to the remote machine dropped").await;
                }
                // The answer of a mode that is no longer the running one (a switch, a stop).
                Heard::Msg(Incoming::Display(
                    Display::Started { generation, .. } | Display::Failed { generation, .. },
                )) if is_stale(active.as_ref(), generation) => {}
                Heard::Msg(Incoming::Display(Display::Started { output, others, closed_lid, .. })) => {
                    let mut outputs = vec![output];
                    outputs.extend(others.unwrap_or_default());
                    let short = active
                        .as_ref()
                        .filter(|a| a.starting.is_some() && outputs.len() < a.monitors.len());
                    if let Some(a) = short {
                        // An agent that knows only one output made one.
                        let why = format!(
                            "the remote machine made {} output(s) for {} monitors: update \
                             navette there",
                            outputs.len(),
                            a.monitors.len()
                        );
                        if let Some(peer) = &peer {
                            peer.outbox.send(Msg::Display(Display::Stop), Vec::new());
                        }
                        stop(&mut active, &why).await;
                    } else if let Some(a) = &mut active
                        && let Some((reply, _)) = a.starting.take()
                    {
                        let verb = if a.monitors.len() > 1 { "show" } else { "shows" };
                        let shown = format!("{} {verb} the remote machine", a.label());
                        home.notifier.show(&shown, "");
                        a.told = Some(Told(home.notifier.clone(), a.given_back()));
                        let mut text = format!(
                            "{} {verb} the remote machine's {}",
                            a.label(),
                            outputs.join(", ")
                        );
                        if let Some(switched) = &a.switched {
                            text = format!("{switched}; {text}");
                        }
                        if closed_lid.is_none() {
                            text.push_str(
                                "; the remote machine's agent does not keep its lid during a \
                                 mode, closing it does what it always does: update navette there",
                            );
                        }
                        if let Some(notice) = &a.notice {
                            text = format!("{text}; {notice}");
                        }
                        let _ = reply.send(Ok(text));
                    }
                }
                Heard::Msg(Incoming::Display(Display::Failed { reason, .. })) => {
                    if let Some(mut a) = active.take() {
                        match a.starting.take() {
                            Some((reply, _)) => {
                                let e = io::Error::other(format!(
                                    "the remote machine could not make its output: {reason}"
                                ));
                                let _ = reply.send(Err(a.start_error(e)));
                            }
                            None => eprintln!(
                                "navette: {} given back, the remote machine ended the mode: \
                                 {reason}",
                                a.label()
                            ),
                        }
                        release(a).await;
                    }
                }
                Heard::Msg(Incoming::NoInput(reason)) => eprintln!(
                    "navette: the remote machine shows its picture but cannot take home's \
                     keyboard and pointer: {reason}"
                ),
                Heard::Msg(Incoming::NoLid(reason)) => eprintln!(
                    "navette: lid: the remote machine's lid does what it always does: \
                     {reason}"
                ),
                Heard::Pointer(pointers) => {
                    if let Some(a) = &active {
                        for (shown, pointer) in a.shown.iter().zip(
                            pointers.into_iter().chain(std::iter::repeat_with(Pointer::default)),
                        ) {
                            shown.screen.cursor(pointer);
                        }
                    }
                }
                Heard::Msg(Incoming::Video { output, msg, main, aux }) => {
                    let shown = active.as_ref().and_then(|a| a.shown.get(usize::from(output)));
                    if let Some(shown) = shown {
                        shown.decoding.video(Video { msg, main, aux });
                    }
                }
                Heard::Msg(_) => {}
            },
            order = orders.recv() => match order {
                Some(Order::Start(name, reply)) => {
                    here_at = None;
                    begin(&home, peer.as_ref(), name.as_deref(), reply, &mut generation, &mut active)
                        .await;
                }
                Some(Order::Stop(reply)) => {
                    let given = active.as_ref().map(Active::given_back);
                    let answer = match given {
                        Some(back) => {
                            if let Some(peer) = &peer {
                                peer.outbox.send(Msg::Display(Display::Stop), Vec::new());
                            }
                            stop(&mut active, "stopped").await;
                            Ok(back)
                        }
                        None => Ok("no mode was running".to_string()),
                    };
                    let _ = reply.send(answer);
                }
                Some(Order::Status(reply)) => {
                    let _ = reply.send(Ok(status(active.as_ref(), peer.as_ref())));
                }
                None => break,
            },
            Some(event) = events.recv() => {
                if let Some(a) = &active {
                    let desk = a.desk.clone();
                    // A reload may put a workspace back where the user's rules want it.
                    let sweep = matches!(event, hypr::Event::Placed | hypr::Event::ConfigReloaded);
                    let reloaded = event == hypr::Event::ConfigReloaded;
                    let sending = (a.monitors.clone(), a.home_monitor.clone(), a.seen.clone());
                    let handled =
                        blocking(move || desk.lock().unwrap().handle(&event).map_err(desk_error))
                            .await;
                    if let Err(e) = handled {
                        eprintln!("navette: the home desk could not be restored after a reload: {e}");
                    }
                    // The layer rules are back, but in effect only once each monitor is sorted
                    // again: until then, an overlay of home's that maps lands above the picture.
                    if reloaded && let Some(a) = &active {
                        for shown in &a.shown {
                            shown.screen.poke();
                        }
                    }
                    if sweep {
                        send_home(&home, sending).await;
                    }
                }
            }
            Some(said) = said(&mut active) => match said {
                Said::Keyboard(Keyboard::Remote) => home.board.say("Keyboard → remote"),
                Said::Keyboard(Keyboard::Home) => {
                    home.board.say("Keyboard → home");
                    if let Some(a) = &active {
                        let (hypr, given) = (home.hypr.clone(), a.monitors.clone());
                        let focus = move || hypr::focus_last_window(&*hypr, &given);
                        match blocking(focus).await {
                            Ok(Some(window)) => {
                                tracing::info!(window, "the keyboard went to a home window")
                            }
                            Ok(None) => tracing::info!("no home window shown to take the keyboard"),
                            Err(e) => eprintln!(
                                "navette: the keyboard came home but no window took it: {e}"
                            ),
                        }
                    }
                }
                // The pointer's own moves: the corner says nothing of them.
                Said::Keyboard(Keyboard::Came | Keyboard::Left) => {}
                Said::Ended(why) => {
                    if let Some(peer) = &peer {
                        peer.outbox.send(Msg::Display(Display::Stop), Vec::new());
                    }
                    stop(&mut active, &why).await;
                }
            },
            _ = sleep_until(mic_follows_at) => {
                let remote = active.as_mut().and_then(|a| a.keyboard.take()).map(|(r, _)| r);
                if let (Some(remote), Some(mic)) = (remote, home.mic.clone()) {
                    // Awaited here: a switch is never queued behind another.
                    if let Err(e) = blocking(move || mic.follow(remote)).await {
                        eprintln!("navette: mic: it did not follow the keyboard: {e}");
                    }
                }
            }
            _ = sleep_until(here_at) => {
                here_at = None;
                if active.is_none() {
                    announce(&home, &pressed_tx);
                }
            }
            Some(()) = pressed.recv() => {
                // The user started it by other means since: nothing to start.
                if active.is_none() {
                    let (reply, answer) = oneshot::channel();
                    begin(&home, peer.as_ref(), None, reply, &mut generation, &mut active).await;
                    let notifier = home.notifier.clone();
                    tokio::spawn(async move {
                        if let Ok(Err(e)) = answer.await {
                            notifier.show("The mode could not start", &e.to_string());
                        }
                    });
                }
            }
            _ = sleep_until(deadline) => {
                if let Some(peer) = &peer {
                    peer.outbox.send(Msg::Display(Display::Stop), Vec::new());
                }
                if let Some(mut a) = active.take() {
                    if let Some((reply, _)) = a.starting.take() {
                        let e = io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the remote machine did not answer in time",
                        );
                        let _ = reply.send(Err(a.start_error(e)));
                    }
                    release(a).await;
                }
            }
        }
    }
    stop(&mut active, "navette is stopping").await;
}

/// Whether the pointer, and the keyboard with it, is over one of the `given` monitors. When
/// Hyprland cannot say, the mic goes to the remote machine, as with `mic = fixed`.
fn pointer_over(hypr: &dyn Hypr, given: &[hypr::Monitor]) -> bool {
    match hypr::cursor(hypr) {
        Ok((x, y)) => given.iter().any(|m| output::covers(m, x, y)),
        Err(e) => {
            eprintln!("navette: mic: where the pointer is is unknown, the mic goes remote: {e}");
            true
        }
    }
}

/// What the running mode's surface and decoder say.
enum Said {
    /// The keyboard went home by the release hotkey, or came back to the remote, or went with
    /// the pointer.
    Keyboard(Keyboard),
    /// Why they gave up.
    Ended(String),
}

/// What the running mode's surface or decoder say next; pending forever with no mode.
async fn said(active: &mut Option<Active>) -> Option<Said> {
    let Some(a) = active else {
        return std::future::pending().await;
    };
    tokio::select! {
        Some(why) = a.ended.recv() => Some(Said::Ended(why)),
        Some(keyboard) = a.released.recv() => {
            if a.mic_follows {
                let at = tokio::time::Instant::now() + MIC_FOLLOWS;
                a.keyboard = Some((keyboard.remote(), at));
            }
            Some(Said::Keyboard(keyboard))
        }
        else => None,
    }
}

/// What the link brings: a message, the remote pointer as it changed, or its end.
enum Heard {
    Msg(Incoming),
    Pointer(Vec<Pointer>),
    Gone,
}

async fn next(peer: &mut Option<Peer>) -> Heard {
    let Some(peer) = peer else {
        return std::future::pending().await;
    };
    tokio::select! {
        msg = peer.inbox.recv() => msg.map_or(Heard::Gone, Heard::Msg),
        // Its sender goes with the inbox's.
        Ok(()) = peer.pointer.changed() => Heard::Pointer(peer.pointer.borrow_and_update().clone()),
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// A mode whose checks all passed.
struct Plan {
    mode: Mode,
    /// The monitors it gives, in home's left-to-right order.
    given: Vec<hypr::Monitor>,
    /// Every monitor at home.
    monitors: Vec<hypr::Monitor>,
}

/// Every check of a mode (the config, the monitors, the link), before anything changes.
async fn check<'p>(
    home: &Home,
    peer: Option<&'p Peer>,
    name: Option<&str>,
) -> io::Result<(Plan, &'p Peer)> {
    let mode = Mode::read(&home.config_dir, name)?;
    let hypr = home.hypr.clone();
    let monitors = blocking(move || hypr::monitors(&*hypr)).await?;
    let mut given = resolve(&mode.monitors, &monitors)?;
    // The remote's outputs keep home's physical order, whatever the config's.
    given.sort_by_key(|m| (m.x, m.y));
    let Some(peer) = peer.filter(|p| !p.outbox.is_closed()) else {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "the remote machine is not connected (is `navette agent` running there?); \
             nothing was changed",
        ));
    };
    Ok((
        Plan {
            mode,
            given,
            monitors,
        },
        peer,
    ))
}

/// Every check first, then the surfaces, then the order to the remote. A mode already running
/// is stopped, its monitors given back, only once the other has passed its checks.
async fn start(
    home: &Home,
    peer: Option<&Peer>,
    name: Option<&str>,
    generation: u32,
    active: &mut Option<Active>,
) -> io::Result<()> {
    let (plan, peer) = check(home, peer, name).await?;
    let mut switched = None;
    if let Some(a) = active.as_ref() {
        if a.name == plan.mode.name {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} is already running on {}", a.title(), a.label()),
            ));
        }
        // The remote's answers carry no more than a generation: while it still owes one for
        // this mode, a stop and a start would leave nothing to tell what it says next.
        if a.starting.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "{} is still starting on {}: wait for it, or `navette mode stop`; \
                     nothing was changed",
                    a.title(),
                    a.label()
                ),
            ));
        }
        switched = Some(format!("{} stopped, {}", a.title(), a.given_back()));
        peer.outbox.send(Msg::Display(Display::Stop), Vec::new());
        let to = plan.mode.name.as_deref().unwrap_or("the unnamed mode");
        stop(active, &format!("switching to {to}")).await;
    }
    let title = match &plan.mode.name {
        Some(name) => format!("mode `{name}`"),
        None => "the mode".to_string(),
    };
    launch(home, peer, plan, (switched.clone(), generation), active)
        .await
        .map_err(|e| switch_error(switched.as_deref(), &title, e))
}

async fn launch(
    home: &Home,
    peer: &Peer,
    Plan {
        mode,
        given,
        monitors,
    }: Plan,
    (switched, generation): (Option<String>, u32),
    active: &mut Option<Active>,
) -> io::Result<()> {
    let hotkey = mode.release;
    let mut copies = given.iter().map(|m| OutputCopy {
        width: m.width,
        height: m.height,
        refresh_mhz: (m.refresh_rate * 1000.0).round() as u32,
        scale_milli: (m.scale * 1000.0).round() as u32,
        transform: m.transform.clamp(0, 7) as u8,
    });
    let first = copies.next().expect("one monitor at least");
    let others: Vec<_> = copies.collect();
    let mic_follows = mode.mic_follows && home.mic.is_some();
    // With `mic = follow`, the mic starts where the keyboard is.
    let pointed = mic_follows.then(|| (home.hypr.clone(), given.clone()));
    let names: Vec<String> = given.into_iter().map(|m| m.name).collect();
    let home_monitor = monitors
        .iter()
        .filter(|m| !names.contains(&m.name) && !m.disabled)
        .min_by_key(|m| (m.x, m.y))
        .map(|m| m.name.clone());
    let desk = open_desk(
        home,
        &names,
        home_monitor.clone(),
        &mode.mic_hotkey,
        &mode.local_above,
    )
    .await?;
    // The hotkey is shown over the mic button; with no mic, there is no button.
    let hint = home.mic.as_ref().map(|_| mode.mic_hotkey.clone());
    let feed = Feed::new(home.board.clone(), hint, mode.look.clone());
    let home_monitors: Vec<String> = monitors
        .iter()
        .filter(|m| !names.contains(&m.name) && !m.disabled)
        .map(|m| m.name.clone())
        .collect();
    // Off the executor, and so is the undo when one fails: the desk drops with the closure.
    let (ended_tx, ended) = tokio::sync::mpsc::unbounded_channel();
    let (released_tx, released) = tokio::sync::mpsc::unbounded_channel();
    let (screens, gpu, outbox, input, mic, sound) = (
        home.screens.clone(),
        home.gpu.clone(),
        peer.outbox.clone(),
        peer.input.to_peer.clone(),
        home.mic.clone(),
        home.sound.clone(),
    );
    let covered = names.clone();
    let (shown, corners, desk, mic, sound, gpu) = blocking(move || {
        let lost = gpu.lost();
        let gpu = gpu.current(decode::open_gpu);
        let gpus = decoding_gpus(gpu.clone(), &covered, decode::open_gpu)?;
        let mut shown = Vec::new();
        for ((index, name), gpu) in covered.iter().enumerate().zip(gpus) {
            let output = index as u8;
            let screen = screens.open(
                name,
                input.on(output),
                (hotkey, released_tx.clone()),
                ended_tx.clone(),
                feed.clone(),
            )?;
            let decoding = Decoding::start(
                gpu,
                (screen.dmabuf_modifiers(), screen.dmabuf_refused()),
                outbox.on(output),
                screen.pictures(),
                (ended_tx.clone(), lost.clone()),
            )?;
            shown.push(Shown { decoding, screen });
        }
        // A corner that cannot go up leaves the mode as it is: it only shows what it is.
        let corners = home_monitors
            .iter()
            .filter_map(|name| match screens.corner(name, feed.clone()) {
                Ok(corner) => Some(corner),
                Err(e) => {
                    eprintln!("navette: no corner on {name}: {e}");
                    None
                }
            })
            .collect::<Vec<_>>();
        // Last, as nothing after them can fail: a mode without its sound or its mic is still
        // a mode. The mic is kept when it failed, so that the stop clears what it says.
        let sound = sound.map(|sound| {
            if let Err(e) = sound.start() {
                eprintln!("navette: audio: the remote's sound could not come home: {e}");
            }
            SoundPlayed(sound)
        });
        let mic = mic.map(|mic| {
            let remote = pointed.is_none_or(|(hypr, given)| pointer_over(&*hypr, &given));
            if let Err(e) = mic.start(remote) {
                eprintln!(
                    "navette: mic: it stays home, it could not go to the remote machine: {e}"
                );
            }
            MicSent(mic)
        });
        Ok((shown, corners, desk, mic, sound, gpu))
    })
    .await?;
    let caps = decode::caps(&gpu, (3840, 2160));
    peer.outbox.send(
        Msg::Display(Display::Start {
            output: first,
            caps,
            others: (!others.is_empty()).then_some(others),
            // An agent of the release before keeps its lid only when asked.
            closed_lid: Some(true),
            generation: Some(generation),
        }),
        Vec::new(),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sending = (names.clone(), home_monitor.clone(), seen.clone());
    *active = Some(Active {
        _mic: mic,
        _sound: sound,
        name: mode.name,
        switched,
        generation,
        monitors: names,
        home_monitor,
        seen,
        shown,
        _corners: corners,
        desk: Arc::new(Mutex::new(desk)),
        ended,
        released,
        mic_follows,
        keyboard: None,
        starting: None,
        notice: mode.notice,
        told: None,
    });
    // What was already under the monitors comes out from under the covers.
    send_home(home, sending).await;
    Ok(())
}

/// One decoding GPU per output, none shared: a `Gpu` is not thread-safe (one Vulkan queue,
/// one VA-API display), and two decoders submitting on one at once lost the device on an
/// NVIDIA GPU. The first output takes home's; each other one opens its own. Without a GPU at
/// home, every output decodes on the CPU. With one, an output that cannot have its own fails
/// the start: the remote is offered streams the CPU does not decode.
fn decoding_gpus<T>(
    first: Option<T>,
    given: &[String],
    mut open: impl FnMut() -> Option<T>,
) -> io::Result<Vec<Option<T>>> {
    let on_gpu = first.is_some();
    let mut gpus = vec![first];
    for name in given.iter().skip(1) {
        if !on_gpu {
            gpus.push(None);
            continue;
        }
        let gpu = open().ok_or_else(|| {
            io::Error::other(format!(
                "no GPU could be opened to decode {name} (home decodes on the GPU, each \
                 monitor on a device of its own); nothing was changed"
            ))
        })?;
        gpus.push(Some(gpu));
    }
    Ok(gpus)
}

/// Opens the desk (undoing what a previous run left) and keeps home windows off the `given`
/// monitors: their workspaces go to `other`, the leftmost home monitor kept home, and so do
/// windows that would open on any of their workspaces. Draws the remote's picture above home's
/// overlays, except the `local_above` ones (#116). With a mic, binds its hotkey.
async fn open_desk(
    home: &Home,
    given: &[String],
    other: Option<String>,
    mic_hotkey: &str,
    local_above: &[String],
) -> io::Result<Desk<Eval>> {
    let (hypr, record, given) = (home.hypr.clone(), home.desk_record.clone(), given.to_vec());
    let local_above = local_above.to_vec();
    let mic_bind = home
        .mic
        .as_ref()
        .map(|mic| (mic_hotkey.to_string(), mic.toggle_command()));
    blocking(move || {
        let mut desk = Desk::start(Eval(hypr.clone()), record).map_err(|e| match e {
            DeskError::Hypr(crate::hypr::HyprError::Io(e)) => io::Error::other(format!(
                "Hyprland could not be reached to undo what a previous run left ({e}); \
                 nothing was changed, try again"
            )),
            other => desk_error(other),
        })?;
        // Each workspace of a given monitor goes whole to `other`, so that the window rule
        // and the sweep (#31) find nothing to scatter, and comes back on stop (#48).
        if let Some(other) = &other {
            let held = hypr::held_workspaces(&*hypr, &given)?;
            let home_shows = hypr::shown_workspace(&*hypr, other)?;
            for workspace in &held {
                let (id, name, from) = (workspace.id, &workspace.name, &workspace.monitor);
                let change = Change::move_workspace(id, name, from, other, workspace.shown);
                desk.apply(change).map_err(desk_error)?;
            }
            // The workspace a monitor with the focus shows is shown where it goes (measured
            // on Hyprland 0.56.2): home shows again what it showed.
            if let Some(shows) = home_shows.filter(|_| held.iter().any(|w| w.shown)) {
                let lua = hypr::show_workspace(other, shows);
                if let Err(e) = hypr::dispatch(&*hypr, &format!("eval {lua}")) {
                    eprintln!("navette: {other} may show another workspace than it did: {e}");
                }
            }
        }
        for (index, monitor) in given.iter().enumerate() {
            let Some(other) = &other else { break };
            let fields = format!(
                "match = {{ workspace = {} }}, monitor = {}",
                lua_string(&format!("m[{monitor}]")),
                lua_string(other)
            );
            let name = desk.rule_name(&format!("home-windows-{}", index + 1));
            desk.apply(Change::window_rule(&name, &fields))
                .map_err(desk_error)?;
        }
        // Before the surfaces map: Hyprland 0.56.2 sorts a monitor's overlay layers by `order`,
        // highest first, only when one maps, unmaps or commits its layer-shell state, and draws
        // the last on top. Navette's picture then stays above an overlay of home's that maps
        // later (an OSD, a shell restarted), and the tools the user starts on purpose stay above
        // it. A mode without the rules still shows the picture.
        let above = [(
            SURFACE_NAMESPACE.to_string(),
            "above".to_string(),
            PICTURE_ORDER,
        )];
        let below = local_above.iter().enumerate().map(|(index, namespace)| {
            let base = format!("local-above-{}", index + 1);
            (namespace.clone(), base, LOCAL_ABOVE_ORDER)
        });
        for (namespace, base, order) in above.into_iter().chain(below) {
            let fields = format!(
                "match = {{ namespace = {} }}, order = {order}",
                lua_string(&format!("^{}$", regex_escaped(&namespace)))
            );
            let rule = Change::layer_rule(&desk.rule_name(&base), &fields);
            if let Err(e) = desk.apply(rule) {
                eprintln!("navette: the layer `{namespace}` keeps its place: {e}");
            }
        }
        // Live over the remote's monitor too, where every other shortcut goes to the remote.
        // A mode without its hotkey is still a mode: the bar and `navette mic` switch it.
        if let Some((keys, command)) = mic_bind {
            let name = desk.rule_name("mic");
            let bind = Change::bind(&name, &keys, &command, "Navette: switch the mic");
            if let Err(e) = desk.apply(bind) {
                eprintln!("navette: mic: its hotkey `{keys}` could not be bound: {e}");
            }
        }
        Ok(desk)
    })
    .await
}

/// `text` matched as is by Hyprland's regular expressions.
fn regex_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Moves to the home monitor the home windows that landed on the given ones, where a cover
/// hides them (#31). These moves are not desk changes: nothing puts a window back under a cover,
/// so a stop leaves them home. A failure is said, and the next event tries again.
/// `sending` is the mode's given monitors, home monitor and workspaces seen.
async fn send_home(home: &Home, sending: (Vec<String>, Option<String>, Arc<Mutex<Seen>>)) {
    let (given, Some(to), seen) = sending else {
        return;
    };
    let hypr = home.hypr.clone();
    let sent =
        blocking(move || hypr::send_home(&*hypr, &given, &to, &mut seen.lock().unwrap())).await;
    match sent {
        Ok(sent) if !sent.is_empty() => {
            tracing::info!(?sent, "home windows taken from under the cover")
        }
        Ok(_) => {}
        Err(e) => eprintln!("navette: a home window may be left under the cover: {e}"),
    }
}

fn desk_error(e: DeskError) -> io::Error {
    io::Error::other(e.to_string())
}

/// Takes the cover away, and returns once it is gone and the desk given back.
async fn stop(active: &mut Option<Active>, why: &str) {
    if let Some(mut a) = active.take() {
        if let Some((reply, _)) = a.starting.take() {
            let e = io::Error::other(format!("the mode ended: {why}"));
            let _ = reply.send(Err(a.start_error(e)));
        }
        eprintln!("navette: {} given back ({why})", a.label());
        release(a).await;
    }
}

/// The link went: a mode that is up is stopped as `stop` does. A start still waiting for the
/// remote is answered with the drop itself, and with "nothing was changed" only once the desk
/// is given back for real: the undo can fail with Hyprland unreachable.
async fn link_lost(active: &mut Option<Active>, why: &str) {
    let Some(a) = active else { return };
    let Some((reply, _)) = a.starting.take() else {
        return stop(active, why).await;
    };
    let a = active.take().expect("just seen");
    eprintln!("navette: {} given back ({why})", a.label());
    let (switched, title) = (a.switched.clone(), a.title());
    let desk = a.desk.clone();
    // The covers and the rest go first, as in `release`; the undo is then ours to read.
    release(a).await;
    let undone = blocking(move || desk.lock().unwrap().stop().map_err(desk_error)).await;
    let mut text = "the link to the remote machine dropped while the mode was starting".to_string();
    match undone {
        Ok(()) => text.push_str("; nothing was changed"),
        Err(e) => text.push_str(&format!(
            "; the home desk could not be given back ({e}): it is restored at the next start"
        )),
    }
    let e = io::Error::new(io::ErrorKind::ConnectionAborted, text);
    let _ = reply.send(Err(switch_error(switched.as_deref(), &title, e)));
}

/// Drops a mode off the executor (the threads are joined, the desk undone), and waits for it.
async fn release(active: Active) {
    let _ = tokio::task::spawn_blocking(move || drop(active)).await;
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
}

/// `$XDG_RUNTIME_DIR/navette/control.sock`.
pub fn control_path() -> io::Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime).join("navette").join("control.sock"))
}

/// Listens for `navette mode start|stop` and `navette status`, in a directory only this user can enter.
pub fn listen(path: &Path) -> io::Result<UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = path.parent().expect("the socket sits in a directory");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another `navette serve` is running",
        ));
    }
    // A socket left by a `navette serve` that did not end cleanly.
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path)
}

/// Turns each control connection into an order.
pub async fn accept_orders(listener: UnixListener, orders: UnboundedSender<Order>) {
    while let Ok((stream, _)) = listener.accept().await {
        let orders = orders.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            // One short line, soon: a client that says nothing, or too much, is let go.
            let mut line = String::new();
            let mut read = BufReader::new(tokio::io::AsyncReadExt::take(read, ORDER_LONGEST));
            match tokio::time::timeout(ORDER_READ, read.read_line(&mut line)).await {
                Ok(Ok(_)) if line.ends_with('\n') => {}
                _ => return,
            }
            let (reply, answer) = oneshot::channel();
            let order = match line.trim() {
                "start" => Order::Start(None, reply),
                "stop" => Order::Stop(reply),
                "status" => Order::Status(reply),
                other => match other.strip_prefix("start ").map(str::trim) {
                    Some(name) if config::is_name(name) => {
                        Order::Start(Some(name.to_string()), reply)
                    }
                    _ => return,
                },
            };
            if orders.send(order).is_err() {
                return;
            }
            let text = match answer.await {
                Ok(Ok(text)) => format!("ok {text}\n"),
                Ok(Err(e)) => format!("error {e}\n"),
                Err(_) => "error navette is stopping\n".to_string(),
            };
            let _ = write.write_all(text.as_bytes()).await;
        });
    }
}

/// The order that starts the mode called `name`, or the config's own with no name.
pub fn start_order(name: Option<&str>) -> io::Result<String> {
    match name {
        None => Ok("start".to_string()),
        Some(name) if config::is_name(name) => Ok(format!("start {name}")),
        Some(name) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "`{name}` is not a mode name: letters, digits, `-`, `_` and `.`, {} at most",
                config::NAME_LONGEST
            ),
        )),
    }
}

/// `navette mode start|stop`, `navette status`: sends the order to `navette serve` and returns
/// its answer.
pub async fn order(path: &Path, order: &str) -> io::Result<String> {
    order_within(path, order, ORDER_TIMEOUT).await
}

async fn order_within(path: &Path, order: &str, timeout: Duration) -> io::Result<String> {
    tokio::time::timeout(timeout, send_order(path, order))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "`navette serve` did not answer in time",
            )
        })?
}

async fn send_order(path: &Path, order: &str) -> io::Result<String> {
    let stream = UnixStream::connect(path).await.map_err(|e| {
        io::Error::new(
            e.kind(),
            "`navette serve` is not running here: start it first",
        )
    })?;
    let (read, mut write) = stream.into_split();
    write.write_all(format!("{order}\n").as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    match line.trim_end().split_once(' ') {
        Some(("ok", text)) => Ok(text.to_string()),
        Some(("error", text)) => Err(io::Error::other(text.to_string())),
        _ => Err(io::Error::other("`navette serve` gave no answer")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::sync::{mpsc, watch};

    use super::config::CONFIG_FILE;
    use super::*;
    use crate::corner::{Look, Press, Side, Update};
    use crate::display::wire::{Cursor, Outbox};
    use crate::hypr::fake::{FakeHypr, FakeMonitor, FakeWindow};
    use crate::mic::Status;

    /// What a test makes happen as a screen opens.
    type OnOpen = Option<Box<dyn Fn() + Send>>;

    #[derive(Default)]
    struct FakeScreens {
        /// Monitors covered right now.
        covered: Arc<Mutex<Vec<String>>>,
        /// How each screen says it went away.
        ends: Arc<Mutex<Vec<Ended>>>,
        /// The release hotkey each screen was given.
        hotkeys: Arc<Mutex<Vec<Option<Hotkey>>>>,
        /// The cursors the screens were given, with their monitor.
        cursors: Arc<Mutex<Vec<(String, Pointer)>>>,
        /// Where each screen sends home's input, with its monitor.
        inputs: Arc<Mutex<Vec<(String, input::Queue)>>>,
        /// Where each screen says its hotkey gave the keyboard back.
        releases: Arc<Mutex<Vec<Released>>>,
        /// Runs each time a screen opens: the link can drop while the covers go up.
        on_open: Arc<Mutex<OnOpen>>,
        /// The monitors whose cover cannot go up.
        refuse: Arc<Mutex<Vec<String>>>,
        /// The corners showing right now, by monitor and side, and what each heard.
        corners: Corners,
        /// What each corner was handed, by monitor.
        feeds: Arc<Mutex<Vec<(String, Feed)>>>,
        /// Each poke of a screen, by monitor, with how many layer rules Hyprland had then.
        pokes: Arc<Mutex<Vec<(String, usize)>>>,
        hypr: Option<Arc<FakeHypr>>,
    }

    /// The corners showing, by monitor and side, and what each heard.
    #[derive(Clone, Default)]
    struct Corners {
        shown: Arc<Mutex<Vec<(String, Side)>>>,
        heard: Arc<Mutex<Vec<(String, Update)>>>,
    }

    /// A corner, drawn nowhere: it records what it hears until dropped.
    struct FakeCorner {
        monitor: String,
        corners: Corners,
        open: Arc<AtomicBool>,
    }

    impl FakeCorner {
        fn open(monitor: &str, side: Side, feed: &Feed, corners: &Corners) -> Self {
            corners
                .shown
                .lock()
                .unwrap()
                .push((monitor.to_string(), side));
            let open = Arc::new(AtomicBool::new(true));
            let (still, heard, name) = (open.clone(), corners.heard.clone(), monitor.to_string());
            feed.listen(move |update| {
                let open = still.load(Ordering::Relaxed);
                if open {
                    heard.lock().unwrap().push((name.clone(), update.clone()));
                }
                open
            });
            Self {
                monitor: monitor.to_string(),
                corners: corners.clone(),
                open,
            }
        }
    }

    impl Drop for FakeCorner {
        fn drop(&mut self) {
            self.open.store(false, Ordering::Relaxed);
            let mut shown = self.corners.shown.lock().unwrap();
            shown.retain(|(m, _)| *m != self.monitor);
        }
    }

    struct FakeScreen {
        monitor: String,
        covered: Arc<Mutex<Vec<String>>>,
        cursors: Arc<Mutex<Vec<(String, Pointer)>>>,
        pokes: Arc<Mutex<Vec<(String, usize)>>>,
        hypr: Option<Arc<FakeHypr>>,
        _corner: FakeCorner,
    }

    impl Screens for FakeScreens {
        fn open(
            &self,
            monitor: &str,
            input: input::Queue,
            (hotkey, released): (Option<Hotkey>, Released),
            ended: Ended,
            feed: Feed,
        ) -> io::Result<Box<dyn Screen>> {
            if self.refuse.lock().unwrap().iter().any(|m| m == monitor) {
                return Err(io::Error::other("no layer shell"));
            }
            self.inputs
                .lock()
                .unwrap()
                .push((monitor.to_string(), input));
            self.hotkeys.lock().unwrap().push(hotkey);
            self.releases.lock().unwrap().push(released);
            self.covered.lock().unwrap().push(monitor.to_string());
            self.ends.lock().unwrap().push(ended);
            if let Some(hook) = &*self.on_open.lock().unwrap() {
                hook();
            }
            let corner = FakeCorner::open(monitor, Side::Remote, &feed, &self.corners);
            self.feeds.lock().unwrap().push((monitor.to_string(), feed));
            Ok(Box::new(FakeScreen {
                monitor: monitor.to_string(),
                covered: self.covered.clone(),
                cursors: self.cursors.clone(),
                pokes: self.pokes.clone(),
                hypr: self.hypr.clone(),
                _corner: corner,
            }))
        }

        fn corner(&self, monitor: &str, feed: Feed) -> io::Result<Box<dyn Send>> {
            let corner = FakeCorner::open(monitor, Side::Home, &feed, &self.corners);
            self.feeds.lock().unwrap().push((monitor.to_string(), feed));
            Ok(Box::new(corner))
        }
    }

    impl Screen for FakeScreen {
        fn pictures(&self) -> Box<dyn FnMut(Picture) + Send> {
            Box::new(|_| {})
        }

        fn dmabuf_modifiers(&self) -> Vec<u64> {
            Vec::new()
        }

        fn dmabuf_refused(&self) -> Arc<AtomicBool> {
            Arc::default()
        }

        fn cursor(&self, pointer: Pointer) {
            self.cursors
                .lock()
                .unwrap()
                .push((self.monitor.clone(), pointer));
        }

        fn poke(&self) {
            let rules = self.hypr.as_ref().map_or(0, |h| h.layer_rules().len());
            self.pokes
                .lock()
                .unwrap()
                .push((self.monitor.clone(), rules));
        }
    }

    impl Drop for FakeScreen {
        fn drop(&mut self) {
            self.covered.lock().unwrap().retain(|m| *m != self.monitor);
        }
    }

    /// A mic that logs what the mode asks of it.
    #[derive(Default)]
    struct FakeMic {
        log: Mutex<Vec<&'static str>>,
        refuse: bool,
        /// Whether it goes to the remote machine.
        remote: Mutex<bool>,
    }

    impl FakeMic {
        /// The hotkey, the corner's button or `navette mic`, which the mode does not see.
        fn by_hand(&self, remote: bool) {
            *self.remote.lock().unwrap() = remote;
            self.log.lock().unwrap().push("by hand");
        }
    }

    impl Mic for FakeMic {
        fn start(&self, remote: bool) -> io::Result<()> {
            let said = if remote { "start" } else { "start home" };
            self.log.lock().unwrap().push(said);
            if self.refuse {
                return Err(io::Error::other("no echo canceller"));
            }
            *self.remote.lock().unwrap() = remote;
            Ok(())
        }

        fn follow(&self, remote: bool) -> io::Result<()> {
            let mut now = self.remote.lock().unwrap();
            if *now != remote {
                *now = remote;
                let said = if remote { "to remote" } else { "to home" };
                self.log.lock().unwrap().push(said);
            }
            Ok(())
        }

        fn stop(&self) {
            self.log.lock().unwrap().push("stop");
        }

        fn toggle_command(&self) -> String {
            "'/opt/invented/navette' mic toggle".into()
        }
    }

    /// The remote's sound, logging what the mode asks of it.
    #[derive(Default)]
    struct FakeSound {
        log: Mutex<Vec<&'static str>>,
    }

    impl Sound for FakeSound {
        fn start(&self) -> io::Result<()> {
            self.log.lock().unwrap().push("start");
            Ok(())
        }

        fn stop(&self) {
            self.log.lock().unwrap().push("stop");
        }
    }

    struct Desk {
        _config: tempfile::TempDir,
        hypr: Arc<FakeHypr>,
        mic: Arc<FakeMic>,
        sound: Arc<FakeSound>,
        covered: Arc<Mutex<Vec<String>>>,
        ends: Arc<Mutex<Vec<Ended>>>,
        hotkeys: Arc<Mutex<Vec<Option<Hotkey>>>>,
        cursors: Arc<Mutex<Vec<(String, Pointer)>>>,
        inputs: Arc<Mutex<Vec<(String, input::Queue)>>>,
        releases: Arc<Mutex<Vec<Released>>>,
        on_open: Arc<Mutex<OnOpen>>,
        refuse: Arc<Mutex<Vec<String>>>,
        corners: Corners,
        feeds: Arc<Mutex<Vec<(String, Feed)>>>,
        pokes: Arc<Mutex<Vec<(String, usize)>>>,
        board: Arc<Board>,
        notices: Arc<crate::notify::Fake>,
        peers: UnboundedSender<Peer>,
        orders: UnboundedSender<Order>,
        events: UnboundedSender<hypr::Event>,
    }

    fn desk(monitor: &str) -> Desk {
        desk_with(&format!("monitor = {monitor}\n"))
    }

    fn desk_with(config_text: &str) -> Desk {
        desk_with_mic(config_text, FakeMic::default())
    }

    fn desk_with_mic(config_text: &str, mic: FakeMic) -> Desk {
        let monitors = vec![
            FakeMonitor::new("DP-1", 1920, 1080, 0),
            FakeMonitor::new("DP-2", 2560, 1440, 1920),
        ];
        desk_on(config_text, monitors, mic)
    }

    fn desk_on(config_text: &str, monitors: Vec<FakeMonitor>, mic: FakeMic) -> Desk {
        let config = tempfile::tempdir().unwrap();
        std::fs::write(config.path().join(CONFIG_FILE), config_text).unwrap();
        let mic = Arc::new(mic);
        let sound = Arc::new(FakeSound::default());
        let hypr = Arc::new(FakeHypr::with(monitors));
        let screens = FakeScreens {
            hypr: Some(hypr.clone()),
            ..FakeScreens::default()
        };
        let covered = screens.covered.clone();
        let ends = screens.ends.clone();
        let hotkeys = screens.hotkeys.clone();
        let releases = screens.releases.clone();
        let on_open = screens.on_open.clone();
        let refuse = screens.refuse.clone();
        let cursors = screens.cursors.clone();
        let inputs = screens.inputs.clone();
        let corners = screens.corners.clone();
        let feeds = screens.feeds.clone();
        let pokes = screens.pokes.clone();
        let board = Arc::new(Board::default());
        let notices = Arc::new(crate::notify::Fake::default());
        let (peers, peers_rx) = mpsc::unbounded_channel();
        let (orders, orders_rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        let home = Home {
            config_dir: config.path().to_path_buf(),
            hypr: hypr.clone(),
            screens: Arc::new(screens),
            gpu: Arc::new(HomeGpu::new(None)),
            desk_record: config.path().join("desk"),
            mic: Some(mic.clone()),
            sound: Some(sound.clone()),
            board: board.clone(),
            notifier: notices.clone(),
        };
        tokio::spawn(serve(home, peers_rx, orders_rx, events_rx));
        Desk {
            _config: config,
            hypr,
            mic,
            sound,
            covered,
            ends,
            hotkeys,
            releases,
            on_open,
            refuse,
            cursors,
            inputs,
            corners,
            feeds,
            pokes,
            board,
            notices,
            peers,
            orders,
            events,
        }
    }

    /// A remote machine on the other end of an in-memory peer.
    struct Remote {
        outbox: Outbox,
        to_home: mpsc::Sender<Incoming>,
        pointer: watch::Sender<Vec<Pointer>>,
        input: input::Wire,
        /// What home's clipboard and sound read: whether a mode runs.
        mode: watch::Receiver<bool>,
    }

    fn connect(desk: &Desk) -> Remote {
        let outbox = Outbox::default();
        let (to_home, inbox) = mpsc::channel(64);
        let (pointer, pointer_rx) = watch::channel(Vec::new());
        let input = input::Wire::default();
        let (mode, in_mode) = watch::channel(false);
        desk.peers
            .send(Peer {
                outbox: outbox.clone(),
                inbox,
                pointer: pointer_rx,
                input: input.clone(),
                mode,
            })
            .unwrap();
        Remote {
            outbox,
            to_home,
            pointer,
            input,
            mode: in_mode,
        }
    }

    async fn ask(desk: &Desk, start: bool) -> oneshot::Receiver<io::Result<String>> {
        let (reply, answer) = oneshot::channel();
        let order = if start {
            Order::Start(None, reply)
        } else {
            Order::Stop(reply)
        };
        desk.orders.send(order).unwrap();
        answer
    }

    /// `navette mode start <name>`.
    async fn ask_mode(desk: &Desk, name: &str) -> oneshot::Receiver<io::Result<String>> {
        let (reply, answer) = oneshot::channel();
        desk.orders
            .send(Order::Start(Some(name.to_string()), reply))
            .unwrap();
        answer
    }

    /// `navette status`.
    async fn status_of(desk: &Desk) -> String {
        let (reply, answer) = oneshot::channel();
        desk.orders.send(Order::Status(reply)).unwrap();
        answer_text(answer).await
    }

    async fn answer_text(answer: oneshot::Receiver<io::Result<String>>) -> String {
        self::answer(answer).await.unwrap()
    }

    async fn answer(answer: oneshot::Receiver<io::Result<String>>) -> io::Result<String> {
        tokio::time::timeout(Duration::from_secs(5), answer)
            .await
            .expect("an answer in time")
            .unwrap()
    }

    async fn until(done: impl Fn() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("home never got there");
    }

    fn starts(remote: &Remote) -> Vec<OutputCopy> {
        remote
            .outbox
            .queued()
            .into_iter()
            .filter_map(|m| match m {
                Msg::Display(Display::Start { output, .. }) => Some(output),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn starting_covers_the_monitor_and_asks_the_remote_for_a_copy() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-2"]);
        assert_eq!(
            starts(&remote),
            [OutputCopy {
                width: 2560,
                height: 1440,
                refresh_mhz: 60_000,
                scale_milli: 1000,
                transform: 0,
            }]
        );
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: Some(true),
            }))
            .unwrap();
        let text = answer(pending).await.unwrap();
        assert!(text.contains("DP-2"), "{text}");
        // Home windows that would open on DP-2's workspace go to DP-1 instead.
        let rules = desk.hypr.window_rules();
        assert_eq!(rules.len(), 1);
        assert!(
            rules[0].contains(r#"match = { workspace = "m[DP-2]" }, monitor = "DP-1""#),
            "{rules:?}"
        );
    }

    #[tokio::test]
    async fn the_release_hotkey_gives_typing_to_the_home_window_that_had_it_last() {
        let desk = desk("DP-2");
        desk.hypr.add_window(FakeWindow::new("0xa1", "DP-1", 1));
        desk.hypr.add_window(FakeWindow::new("0xb2", "DP-1", 0));
        let _remote = started(&desk).await;
        assert_eq!(desk.hypr.focused(), []);
        let released = desk.releases.lock().unwrap()[0].clone();
        released.send(Keyboard::Home).unwrap();
        // Focused with the pointer left where it is, over the covered monitor.
        until(|| desk.hypr.focused() == [("0xb2".to_string(), false)]).await;
    }

    /// The corners showing, by monitor and side, in home's order.
    fn corners(desk: &Desk) -> Vec<(String, Side)> {
        let mut shown = desk.corners.shown.lock().unwrap().clone();
        shown.sort_by(|a, b| a.0.cmp(&b.0));
        shown
    }

    fn corner(monitor: &str, side: Side) -> (String, Side) {
        (monitor.to_string(), side)
    }

    /// What the corners heard, by monitor.
    fn heard(desk: &Desk) -> Vec<(String, Update)> {
        desk.corners.heard.lock().unwrap().clone()
    }

    fn said(monitor: &str, text: &str) -> (String, Update) {
        (monitor.to_string(), Update::Say(text.to_string()))
    }

    #[tokio::test]
    async fn each_reserved_monitor_shows_its_corner_while_the_mode_runs() {
        let monitors = vec![
            FakeMonitor::new("DP-1", 1920, 1080, 0),
            FakeMonitor::new("DP-2", 2560, 1440, 1920),
            FakeMonitor::new("DP-3", 1920, 1080, 4480),
        ];
        let desk = desk_on("monitor = DP-2, DP-3\n", monitors, FakeMic::default());
        desk.board.mic(Status::Remote);
        let (_remote, text) = started_with(&desk, &["NAVETTE-1", "NAVETTE-2"]).await;
        text.unwrap();
        assert_eq!(
            corners(&desk),
            [
                corner("DP-1", Side::Home),
                corner("DP-2", Side::Remote),
                corner("DP-3", Side::Remote)
            ]
        );
        // Each one shows where the mic is from the start.
        for monitor in ["DP-1", "DP-2", "DP-3"] {
            let mic = (monitor.to_string(), Update::Mic(Status::Remote));
            assert!(heard(&desk).contains(&mic), "{monitor}");
        }
    }

    #[tokio::test]
    async fn no_mode_shows_no_corner() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        assert_eq!(corners(&desk), []);
        drop(remote);
        let _remote = started(&desk).await;
        assert_eq!(
            corners(&desk),
            [corner("DP-1", Side::Home), corner("DP-2", Side::Remote)]
        );
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(corners(&desk), []);
        let before = heard(&desk).len();
        desk.board.say("Mic → home");
        assert_eq!(heard(&desk).len(), before, "nobody hears the board");
    }

    #[tokio::test]
    async fn the_release_hotkey_says_keyboard_home_in_the_corner() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        let released = desk.releases.lock().unwrap()[0].clone();
        released.send(Keyboard::Home).unwrap();
        until(|| {
            let heard = heard(&desk);
            heard.contains(&said("DP-1", "Keyboard → home"))
                && heard.contains(&said("DP-2", "Keyboard → home"))
        })
        .await;
        released.send(Keyboard::Remote).unwrap();
        until(|| heard(&desk).contains(&said("DP-2", "Keyboard → remote"))).await;
        // In the corners only: the desktop's notifications said the start alone.
        assert_eq!(desk.notices.summaries(), ["DP-2 shows the remote machine"]);
    }

    #[tokio::test]
    async fn starting_and_stopping_a_mode_notifies_each_machine_once() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        assert_eq!(desk.notices.summaries(), ["DP-2 shows the remote machine"]);
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(
            desk.notices.summaries(),
            ["DP-2 shows the remote machine", "DP-2 is back home"]
        );
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(desk.notices.summaries().len(), 2, "said once");
    }

    #[tokio::test]
    async fn a_mode_the_remote_never_started_notifies_nothing() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Failed {
                reason: "no headless backend".into(),
                generation: None,
            }))
            .unwrap();
        answer(pending).await.unwrap_err();
        assert_eq!(desk.notices.shown(), []);
    }

    #[tokio::test]
    async fn the_corner_never_reaches_the_frames_sent_or_the_input_forwarded() {
        let desk = desk("DP-2");
        let clicks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = clicks.clone();
        desk.board.on_click(move || {
            counted.fetch_add(1, Ordering::Relaxed);
        });
        let remote = started(&desk).await;
        let queued = remote.outbox.queued().len();
        // Everything a corner shows and does.
        desk.board.mic(Status::Remote);
        desk.board.say("Mic → remote");
        let feeds = desk.feeds.lock().unwrap().clone();
        for (_, feed) in &feeds {
            feed.press(Press::Mic);
        }
        until(|| clicks.load(Ordering::Relaxed) == feeds.len()).await;
        assert_eq!(
            remote.outbox.queued().len(),
            queued,
            "nothing for the remote"
        );
        let inputs = desk.inputs.lock().unwrap();
        assert!(inputs.iter().all(|(_, queue)| queue.queued().is_empty()));
    }

    #[tokio::test]
    async fn with_no_home_window_the_release_hotkey_changes_no_focus() {
        let desk = desk("DP-2");
        desk.hypr.add_window(FakeWindow {
            hidden_workspace: true,
            ..FakeWindow::new("0xc3", "DP-1", 0)
        });
        let _remote = started(&desk).await;
        let released = desk.releases.lock().unwrap()[0].clone();
        released.send(Keyboard::Home).unwrap();
        // The mode goes on, and a stop still answers once the release was handled.
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(desk.hypr.focused(), []);
    }

    async fn started(desk: &Desk) -> Remote {
        let remote = connect(desk);
        let pending = ask(desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: Some(true),
            }))
            .unwrap();
        answer(pending).await.unwrap();
        remote
    }

    fn mic_log(desk: &Desk) -> Vec<&'static str> {
        desk.mic.log.lock().unwrap().clone()
    }

    fn sound_log(desk: &Desk) -> Vec<&'static str> {
        desk.sound.log.lock().unwrap().clone()
    }

    fn offers(desk: &Desk) -> Vec<(String, String)> {
        desk.notices.offers()
    }

    /// Waits for the desk to have said what it says once a link has settled, and a little more.
    async fn settle() {
        tokio::time::sleep(ARRIVAL_DELAY * 2).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_link_with_no_mode_offers_to_start_it() {
        let desk = desk("DP-2");
        let _remote = connect(&desk);
        settle().await;
        assert_eq!(offers(&desk), [("REMOTE is here".into(), "Start".into())]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_offer_names_the_remote_as_the_config_does() {
        let desk = desk_with("remote_label = FAR\n[office]\nmonitor = DP-2\n");
        let _remote = connect(&desk);
        settle().await;
        assert_eq!(offers(&desk), [("FAR is here".into(), "Start".into())]);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_offered_before_the_moment_is_over() {
        let desk = desk("DP-2");
        let _remote = connect(&desk);
        tokio::time::sleep(ARRIVAL_DELAY / 2).await;
        assert!(offers(&desk).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_offered_with_no_mode_to_start() {
        let desk = desk_with("");
        let _remote = connect(&desk);
        settle().await;
        assert!(offers(&desk).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_offer_is_not_made_again_while_the_link_stays_up() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        settle().await;
        desk.notices.press(0);
        until(|| !starts(&remote).is_empty()).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: Some(true),
            }))
            .unwrap();
        // The mode runs, then stops, and the link is still the same one.
        let stopping = ask(&desk, false).await;
        answer_text(stopping).await;
        settle().await;
        assert_eq!(offers(&desk).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_link_after_a_drop_offers_again() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        settle().await;
        assert_eq!(offers(&desk).len(), 1);
        drop(remote);
        settle().await;
        let _again = connect(&desk);
        settle().await;
        assert_eq!(offers(&desk).len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_link_that_drops_within_the_moment_offers_nothing() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        tokio::time::sleep(ARRIVAL_DELAY / 2).await;
        drop(remote);
        settle().await;
        assert!(offers(&desk).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_action_starts_the_default_mode() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        settle().await;
        desk.notices.press(0);
        until(|| !starts(&remote).is_empty()).await;
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_action_pressed_with_a_mode_running_starts_nothing_more() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        settle().await;
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: Some(true),
            }))
            .unwrap();
        answer(pending).await.unwrap();
        desk.notices.press(0);
        settle().await;
        assert_eq!(starts(&remote).len(), 1);
        assert!(
            desk.notices
                .summaries()
                .iter()
                .all(|s| !s.contains("could not")),
            "{:?}",
            desk.notices.summaries()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_action_that_cannot_start_the_mode_says_why() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        settle().await;
        drop(remote);
        settle().await;
        desk.notices.press(0);
        until(|| !desk.notices.shown().is_empty()).await;
        let (summary, body) = desk.notices.shown().remove(0);
        assert_eq!(summary, "The mode could not start");
        assert!(body.contains("not connected"), "{body}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_mode_started_within_the_moment_of_the_link_is_not_offered() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: Some(true),
            }))
            .unwrap();
        answer(pending).await.unwrap();
        settle().await;
        assert!(offers(&desk).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_mode_started_and_stopped_within_the_moment_is_not_offered_either() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: Some(true),
            }))
            .unwrap();
        answer(pending).await.unwrap();
        let stopping = ask(&desk, false).await;
        answer_text(stopping).await;
        settle().await;
        assert!(offers(&desk).is_empty());
    }

    #[tokio::test]
    async fn with_the_link_up_and_no_mode_home_lets_nothing_cross() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!*remote.mode.borrow());
    }

    #[tokio::test]
    async fn after_mode_start_home_lets_the_link_carry_and_after_mode_stop_no_longer() {
        let desk = desk("DP-2");
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        // Asked, not started: the remote may not carry yet, and home waits for it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!*remote.mode.borrow());
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: Some(true),
            }))
            .unwrap();
        answer(pending).await.unwrap();
        until(|| *remote.mode.borrow()).await;
        answer(ask(&desk, false).await).await.unwrap();
        until(|| !*remote.mode.borrow()).await;
    }

    #[tokio::test]
    async fn no_sound_comes_home_without_a_mode() {
        let desk = desk("DP-2");
        let _remote = connect(&desk);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(sound_log(&desk).is_empty());
    }

    #[tokio::test]
    async fn starting_a_mode_brings_the_remote_sound_home() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        assert_eq!(sound_log(&desk), ["start"]);
    }

    #[tokio::test]
    async fn stopping_the_mode_stops_the_remote_sound() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(sound_log(&desk), ["start", "stop"]);
    }

    #[tokio::test]
    async fn a_dropped_link_stops_the_remote_sound() {
        let desk = desk("DP-2");
        let remote = started(&desk).await;
        drop(remote);
        until(|| sound_log(&desk) == ["start", "stop"]).await;
    }

    #[tokio::test]
    async fn starting_a_mode_sends_the_mic_to_the_remote() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        assert_eq!(mic_log(&desk), ["start"]);
    }

    #[tokio::test]
    async fn stopping_the_mode_brings_the_mic_home() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(mic_log(&desk), ["start", "stop"]);
    }

    #[tokio::test]
    async fn a_dropped_link_brings_the_mic_home() {
        let desk = desk("DP-2");
        let remote = started(&desk).await;
        drop(remote);
        until(|| mic_log(&desk) == ["start", "stop"]).await;
    }

    #[tokio::test]
    async fn a_mic_that_cannot_start_leaves_the_mode_running() {
        let mic = FakeMic {
            refuse: true,
            ..FakeMic::default()
        };
        let desk = desk_with_mic("monitor = DP-2\n", mic);
        let _remote = started(&desk).await;
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-2"]);
        answer(ask(&desk, false).await).await.unwrap();
        // Stopped all the same, so that it no longer says it failed.
        assert_eq!(mic_log(&desk), ["start", "stop"]);
    }

    /// The keyboard going with the pointer, or by the release hotkey, as DP-2's surface says.
    fn keyboard(desk: &Desk, keyboard: Keyboard) {
        desk.releases.lock().unwrap()[0].send(keyboard).unwrap();
    }

    fn follows() -> Desk {
        desk_with("monitor = DP-2\nmic = follow\n")
    }

    #[tokio::test]
    async fn with_mic_fixed_a_crossing_never_switches_the_mic() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        keyboard(&desk, Keyboard::Left);
        tokio::time::sleep(MIC_FOLLOWS * 3).await;
        keyboard(&desk, Keyboard::Came);
        tokio::time::sleep(MIC_FOLLOWS * 3).await;
        assert_eq!(mic_log(&desk), ["start"]);
    }

    #[tokio::test]
    async fn with_mic_follow_the_mode_starts_with_the_mic_where_the_keyboard_is() {
        // The pointer over DP-1, at home.
        let desk = follows();
        let _remote = started(&desk).await;
        assert_eq!(mic_log(&desk), ["start home"]);
        // Over DP-2, which the mode gives.
        let desk = follows();
        desk.hypr.move_cursor(2000, 10);
        let _remote = started(&desk).await;
        assert_eq!(mic_log(&desk), ["start"]);
    }

    #[tokio::test]
    async fn with_mic_follow_the_keyboard_crossing_moves_the_mic_after_the_delay() {
        let desk = follows();
        let _remote = started(&desk).await;
        let crossed = std::time::Instant::now();
        keyboard(&desk, Keyboard::Came);
        until(|| mic_log(&desk) == ["start home", "to remote"]).await;
        assert!(crossed.elapsed() >= MIC_FOLLOWS, "{:?}", crossed.elapsed());
        let crossed = std::time::Instant::now();
        keyboard(&desk, Keyboard::Left);
        until(|| mic_log(&desk) == ["start home", "to remote", "to home"]).await;
        assert!(crossed.elapsed() >= MIC_FOLLOWS, "{:?}", crossed.elapsed());
    }

    #[tokio::test]
    async fn with_mic_follow_the_release_hotkey_brings_the_mic_home() {
        let desk = follows();
        desk.hypr.move_cursor(2000, 10);
        let _remote = started(&desk).await;
        keyboard(&desk, Keyboard::Home);
        until(|| mic_log(&desk) == ["start", "to home"]).await;
        // The pointer back over the picture takes the keyboard again.
        keyboard(&desk, Keyboard::Left);
        keyboard(&desk, Keyboard::Remote);
        until(|| mic_log(&desk) == ["start", "to home", "to remote"]).await;
    }

    #[tokio::test]
    async fn with_mic_follow_a_pass_over_a_monitor_shorter_than_the_delay_does_not_switch() {
        let desk = follows();
        let _remote = started(&desk).await;
        keyboard(&desk, Keyboard::Came);
        keyboard(&desk, Keyboard::Left);
        tokio::time::sleep(MIC_FOLLOWS * 3).await;
        assert_eq!(mic_log(&desk), ["start home"]);
    }

    #[tokio::test]
    async fn with_mic_follow_a_switch_by_hand_holds_until_the_next_crossing() {
        let desk = follows();
        let _remote = started(&desk).await;
        keyboard(&desk, Keyboard::Came);
        until(|| mic_log(&desk) == ["start home", "to remote"]).await;
        // The hotkey brings the mic home, the keyboard staying with the remote.
        desk.mic.by_hand(false);
        tokio::time::sleep(MIC_FOLLOWS * 3).await;
        assert_eq!(mic_log(&desk), ["start home", "to remote", "by hand"]);
        // The keyboard comes home, where the mic already is, then goes back: the mic follows.
        keyboard(&desk, Keyboard::Left);
        tokio::time::sleep(MIC_FOLLOWS * 3).await;
        assert_eq!(mic_log(&desk), ["start home", "to remote", "by hand"]);
        keyboard(&desk, Keyboard::Came);
        until(|| mic_log(&desk) == ["start home", "to remote", "by hand", "to remote"]).await;
    }

    #[tokio::test]
    async fn a_hotkey_hyprland_refuses_leaves_the_mode_running() {
        let desk = desk_with("monitor = DP-2\nmic_hotkey = NOT A KEY\n");
        let _remote = started(&desk).await;
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-2"]);
        let binds = desk.hypr.binds();
        assert!(binds.iter().all(|b| !b.contains("hl.bind(")), "{binds:?}");
        assert_eq!(mic_log(&desk), ["start"]);
    }

    #[tokio::test]
    async fn the_mic_hotkey_is_bound_through_the_inhibitor_and_undone_on_stop() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        let binds = desk.hypr.binds();
        assert_eq!(binds.len(), 1, "{binds:?}");
        assert!(
            binds[0].contains(r#"hl.bind("SUPER + CTRL + M""#),
            "{binds:?}"
        );
        assert!(binds[0].contains("dont_inhibit = true"), "{binds:?}");
        assert!(
            binds[0].contains(r#"exec_cmd("'/opt/invented/navette' mic toggle")"#),
            "{binds:?}"
        );
        answer(ask(&desk, false).await).await.unwrap();
        let binds = desk.hypr.binds();
        assert_eq!(binds.len(), 2, "{binds:?}");
        assert!(binds[1].contains(":set_enabled(false)"), "{binds:?}");
    }

    #[tokio::test]
    async fn the_mic_hotkey_comes_from_mode_conf() {
        let desk = desk_with("monitor = DP-2\nmic_hotkey = SUPER + ALT + F9\n");
        let _remote = started(&desk).await;
        let binds = desk.hypr.binds();
        assert!(
            binds[0].contains(r#"hl.bind("SUPER + ALT + F9""#),
            "{binds:?}"
        );
    }

    #[tokio::test]
    async fn a_reload_binds_the_mic_hotkey_again() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        desk.events.send(hypr::Event::ConfigReloaded).unwrap();
        until(|| desk.hypr.binds().len() == 2).await;
        let binds = desk.hypr.binds();
        assert_eq!(binds[0], binds[1]);
    }

    #[tokio::test]
    async fn a_surface_that_goes_away_ends_the_mode() {
        let desk = desk("DP-2");
        let remote = started(&desk).await;
        let end = desk.ends.lock().unwrap().pop().unwrap();
        end.send("the monitor was unplugged".into()).unwrap();
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        until(|| desk.hypr.window_rules().len() == 2).await;
        assert!(desk.hypr.window_rules()[1].contains("enabled = false"));
        assert_eq!(
            remote.outbox.queued().last(),
            Some(&Msg::Display(Display::Stop))
        );
        // Nothing runs any more: a new start is not refused.
        let pending = ask(&desk, true).await;
        until(|| starts(&remote).len() == 2).await;
        drop(pending);
    }

    #[tokio::test]
    async fn a_picture_home_cannot_decode_any_more_gives_the_monitor_back() {
        use gliff_proto::{ChromaMode, Codec, ServerMsg};
        let desk = desk("DP-2");
        let remote = started(&desk).await;
        let video = |msg| Incoming::Video {
            output: 0,
            msg,
            main: bytes::Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xff, 0xff, 0xff, 0xff]),
            aux: bytes::Bytes::new(),
        };
        remote
            .to_home
            .try_send(video(ServerMsg::StreamConfig {
                codec: Codec::H264,
                chroma: ChromaMode::Single420,
                pipeline: None,
                width: 320,
                height: 240,
                scale_milli: 1000,
                extradata: Vec::new(),
                aux_extradata: None,
                view_width: 320,
                view_height: 240,
                fps_cap: 60,
            }))
            .unwrap();
        // Home has no GPU here: the CPU decoder is the last, and a keyframe it fails ends it.
        remote
            .to_home
            .try_send(video(ServerMsg::VideoFrame {
                frame_id: 0,
                pts_us: 0,
                keyframe: true,
                damage: Vec::new(),
                data_len: 9,
                aux_len: 0,
            }))
            .unwrap();
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        assert_eq!(
            remote.outbox.queued().last(),
            Some(&Msg::Display(Display::Stop))
        );
    }

    #[tokio::test]
    async fn a_start_right_after_a_stop_finds_home_given_back() {
        let desk = desk("DP-2");
        let remote = started(&desk).await;
        answer(ask(&desk, false).await).await.unwrap();
        // The answer comes once the surface is gone and the rule undone.
        assert!(desk.covered.lock().unwrap().is_empty());
        assert_eq!(desk.hypr.window_rules().len(), 2);
        let pending = ask(&desk, true).await;
        until(|| starts(&remote).len() == 2).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: Some(true),
            }))
            .unwrap();
        answer(pending).await.unwrap();
    }

    #[tokio::test]
    async fn a_home_window_under_the_monitor_when_the_mode_starts_goes_home() {
        let desk = desk("DP-2");
        desk.hypr.add_window(FakeWindow::new("0xe5", "DP-2", 0));
        let _remote = started(&desk).await;
        until(|| desk.hypr.place("0xe5").0 == "DP-1").await;
    }

    #[tokio::test]
    async fn a_home_window_moved_onto_the_monitor_goes_home() {
        let desk = desk("DP-2");
        desk.hypr.add_window(FakeWindow::new("0xa1", "DP-1", 0));
        let _remote = started(&desk).await;
        // `movewindowv2`, after a move to a workspace of DP-2 or a drag onto it.
        desk.hypr.add_window(FakeWindow::new("0xe5", "DP-2", 1));
        desk.events.send(hypr::Event::Placed).unwrap();
        until(|| desk.hypr.place("0xe5") == ("DP-1".to_string(), 1)).await;
    }

    #[tokio::test]
    async fn a_workspace_moved_onto_the_monitor_goes_back_home_whole() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        desk.hypr.add_workspace(5, "5", "DP-2");
        desk.hypr.show("DP-2", 5);
        desk.hypr
            .add_window(FakeWindow::new("0xa1", "DP-2", 0).on_workspace(5));
        desk.events.send(hypr::Event::Placed).unwrap();
        until(|| desk.hypr.place("0xa1") == ("DP-1".to_string(), 5)).await;
        assert_eq!(desk.hypr.shown("DP-1"), 5);
    }

    #[tokio::test]
    async fn a_window_opened_silently_on_a_workspace_of_the_monitor_goes_home() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        // `[workspace 7 silent]`: the window rule does not catch it.
        desk.hypr.add_workspace(7, "7", "DP-2");
        desk.hypr
            .add_window(FakeWindow::new("0xc3", "DP-2", 0).on_workspace(7));
        desk.events.send(hypr::Event::Placed).unwrap();
        until(|| desk.hypr.place("0xc3") == ("DP-1".to_string(), 7)).await;
        assert_eq!(desk.hypr.shown("DP-1"), 1);
    }

    #[tokio::test]
    async fn windows_landing_on_any_of_the_monitors_go_home() {
        let monitors = vec![
            FakeMonitor::new("DP-1", 1920, 1080, 0),
            FakeMonitor::new("DP-2", 2560, 1440, 1920),
            FakeMonitor::new("DP-3", 1920, 1080, 4480),
        ];
        let desk = desk_on("monitor = DP-2, DP-3\n", monitors, FakeMic::default());
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: Some(vec!["NAVETTE-2".into()]),
                closed_lid: Some(true),
            }))
            .unwrap();
        answer(pending).await.unwrap();
        desk.hypr.add_window(FakeWindow::new("0xe5", "DP-2", 0));
        desk.hypr.add_window(FakeWindow::new("0xf6", "DP-3", 1));
        desk.events.send(hypr::Event::Placed).unwrap();
        until(|| desk.hypr.place("0xe5").0 == "DP-1" && desk.hypr.place("0xf6").0 == "DP-1").await;
    }

    #[tokio::test]
    async fn a_reload_sends_home_what_it_put_under_the_monitor() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        // The user's workspace rules may put a workspace back on DP-2.
        desk.hypr.add_workspace(4, "4", "DP-2");
        desk.hypr
            .add_window(FakeWindow::new("0xa1", "DP-2", 0).on_workspace(4));
        desk.events.send(hypr::Event::ConfigReloaded).unwrap();
        until(|| desk.hypr.place("0xa1").0 == "DP-1").await;
    }

    #[tokio::test]
    async fn once_stopped_the_monitor_takes_home_windows_again() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        answer(ask(&desk, false).await).await.unwrap();
        desk.hypr.add_window(FakeWindow::new("0xe5", "DP-2", 0));
        desk.events.send(hypr::Event::Placed).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(desk.hypr.place("0xe5").0, "DP-2");
    }

    /// The issue's desk (#48): left to right DP-3, DP-1, DP-2, each showing the first of its
    /// workspaces (1, 11, 21), with windows on several workspaces of each, and a workspace that
    /// the user's config binds to its monitor (1-10, 11-20, 21-30).
    fn per_monitor_desk(config_text: &str) -> Desk {
        per_monitor_desk_named(config_text, |id| id.to_string())
    }

    fn per_monitor_desk_named(config_text: &str, name: fn(i64) -> String) -> Desk {
        let monitors = vec![
            FakeMonitor::new("DP-3", 1920, 1080, 0),
            FakeMonitor::new("DP-1", 2560, 1440, 1920),
            FakeMonitor::new("DP-2", 2560, 1440, 4480),
        ];
        let desk = desk_on(config_text, monitors, FakeMic::default());
        let hypr = &desk.hypr;
        for (monitor, shown, ids) in [
            ("DP-3", 1, [1, 2, 3]),
            ("DP-1", 11, [11, 12, 13]),
            ("DP-2", 21, [21, 22, 23]),
        ] {
            for id in ids {
                hypr.add_workspace(id, &name(id), monitor);
                hypr.bind_workspace(id, monitor);
            }
            hypr.show(monitor, shown);
        }
        for (address, monitor, workspace, focus) in [
            ("0x1a", "DP-3", 1, 0),
            ("0x2a", "DP-3", 2, 9),
            ("0x11a", "DP-1", 11, 1),
            ("0x11b", "DP-1", 11, 2),
            ("0x12a", "DP-1", 12, 3),
            ("0x13a", "DP-1", 13, 4),
            ("0x21a", "DP-2", 21, 5),
            ("0x22a", "DP-2", 22, 6),
        ] {
            hypr.add_window(FakeWindow::new(address, monitor, focus).on_workspace(workspace));
        }
        desk
    }

    /// Where each workspace with windows is, and what each monitor shows.
    fn placement(desk: &Desk) -> (Vec<(String, String)>, [i64; 3]) {
        let hypr = &desk.hypr;
        (
            hypr.workspaces(),
            [hypr.shown("DP-3"), hypr.shown("DP-1"), hypr.shown("DP-2")],
        )
    }

    fn on(monitor: &str, names: &[&str]) -> Vec<(String, String)> {
        names
            .iter()
            .map(|n| (n.to_string(), monitor.to_string()))
            .collect()
    }

    /// The issue's desk (#93): the same, but each monitor names its workspaces 1, 2, 3, and
    /// the third of each has a window too.
    fn repeated_names_desk(config_text: &str) -> Desk {
        let desk = per_monitor_desk_named(config_text, |id| (id % 10).to_string());
        desk.hypr
            .add_window(FakeWindow::new("0x3a", "DP-3", 10).on_workspace(3));
        desk.hypr
            .add_window(FakeWindow::new("0x23a", "DP-2", 11).on_workspace(23));
        desk
    }

    /// Where each workspace with windows is, by id, and what each monitor shows.
    fn placement_by_id(desk: &Desk) -> (Vec<(i64, String)>, [i64; 3]) {
        let hypr = &desk.hypr;
        (
            hypr.workspace_ids(),
            [hypr.shown("DP-3"), hypr.shown("DP-1"), hypr.shown("DP-2")],
        )
    }

    fn ids_on(monitor: &str, ids: &[i64]) -> Vec<(i64, String)> {
        ids.iter().map(|id| (*id, monitor.to_string())).collect()
    }

    #[tokio::test]
    async fn workspaces_named_alike_on_two_monitors_each_move_and_come_back_by_id() {
        let desk = repeated_names_desk("monitor = DP-1\n");
        let before = placement_by_id(&desk);
        let _remote = started(&desk).await;
        let mut running = ids_on("DP-3", &[1, 2, 3, 11, 12, 13]);
        running.extend(ids_on("DP-2", &[21, 22, 23]));
        assert_eq!(desk.hypr.workspace_ids(), running);
        assert_eq!(desk.hypr.shown("DP-3"), 1);
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(placement_by_id(&desk), before);
    }

    #[tokio::test]
    async fn workspaces_named_alike_on_three_monitors_each_move_and_come_back_by_id() {
        let desk = repeated_names_desk("monitor = DP-1, DP-2\n");
        let before = placement_by_id(&desk);
        let (_remote, text) = started_with(&desk, &["NAVETTE-1", "NAVETTE-2"]).await;
        text.unwrap();
        let running = ids_on("DP-3", &[1, 2, 3, 11, 12, 13, 21, 22, 23]);
        assert_eq!(desk.hypr.workspace_ids(), running);
        assert_eq!(desk.hypr.shown("DP-3"), 1);
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(placement_by_id(&desk), before);
    }

    #[tokio::test]
    async fn a_named_workspace_a_reserved_monitor_shows_moves_and_comes_back() {
        let desk = repeated_names_desk("monitor = DP-1\n");
        desk.hypr.add_workspace(-1337, "mail", "DP-1");
        desk.hypr
            .add_window(FakeWindow::new("0xa1", "DP-1", 12).on_workspace(-1337));
        desk.hypr.show("DP-1", -1337);
        let before = placement_by_id(&desk);
        let _remote = started(&desk).await;
        assert_eq!(desk.hypr.place("0xa1"), ("DP-3".to_string(), -1337));
        assert_eq!(desk.hypr.shown("DP-3"), 1);
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(placement_by_id(&desk), before);
    }

    #[tokio::test]
    async fn the_workspaces_of_a_reserved_monitor_move_whole_and_come_back_on_stop() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let before = placement(&desk);
        let _remote = started(&desk).await;
        let mut running = on("DP-3", &["1", "11", "12", "13", "2"]);
        running.extend(on("DP-2", &["21", "22"]));
        running.sort();
        assert_eq!(desk.hypr.workspaces(), running);
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(placement(&desk), before);
    }

    #[tokio::test]
    async fn a_reserved_monitor_shows_the_workspace_it_showed_before_once_the_mode_stops() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let _remote = started(&desk).await;
        // Home still shows what it showed; the covered monitor something else.
        assert_eq!(desk.hypr.shown("DP-3"), 1);
        assert_ne!(desk.hypr.shown("DP-1"), 11);
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(placement(&desk).1, [1, 11, 21]);
    }

    #[tokio::test]
    async fn windows_of_a_reserved_monitor_stay_on_their_own_workspace_while_the_mode_runs() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let _remote = started(&desk).await;
        for (window, workspace) in [("0x11a", 11), ("0x11b", 11), ("0x12a", 12), ("0x13a", 13)] {
            assert_eq!(desk.hypr.place(window), ("DP-3".to_string(), workspace));
        }
        assert_eq!(desk.hypr.place("0x1a"), ("DP-3".to_string(), 1));
        // Nothing is merged into what home shows, and nothing is left under the cover.
        let shown = desk.hypr.shown("DP-1");
        assert!(desk.hypr.workspaces().iter().all(|(_, m)| m != "DP-1"));
        assert!(shown != 11 && shown != 12 && shown != 13, "{shown}");
    }

    #[tokio::test]
    async fn workspaces_bound_to_a_reserved_monitor_by_the_user_s_config_come_back_on_stop() {
        // Every workspace of the fixture is bound by a rule of the user's.
        let desk = per_monitor_desk("monitor = DP-2\n");
        let before = placement(&desk);
        let _remote = started(&desk).await;
        assert_eq!(desk.hypr.place("0x22a"), ("DP-3".to_string(), 22));
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(placement(&desk), before);
    }

    #[tokio::test]
    async fn a_crash_during_a_mode_gives_the_workspaces_back_at_the_next_start() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let before = placement(&desk);
        // The first run moved the workspaces, then died without a word.
        let crashed = Home {
            config_dir: desk._config.path().to_path_buf(),
            hypr: desk.hypr.clone(),
            screens: Arc::new(FakeScreens::default()),
            gpu: Arc::new(HomeGpu::new(None)),
            desk_record: desk._config.path().join("desk"),
            mic: None,
            sound: None,
            board: Arc::default(),
            notifier: Arc::new(crate::notify::Fake::default()),
        };
        open_desk(
            &crashed,
            &["DP-1".to_string()],
            Some("DP-3".to_string()),
            "SUPER + CTRL + M",
            &[],
        )
        .await
        .unwrap()
        .crash();
        assert_eq!(desk.hypr.place("0x12a").0, "DP-3");
        let _remote = started(&desk).await;
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(placement(&desk), before);
    }

    #[tokio::test]
    async fn a_config_reload_during_a_mode_leaves_the_workspaces_where_the_mode_put_them_and_stop_still_restores_them()
     {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let before = placement(&desk);
        let _remote = started(&desk).await;
        let running = placement(&desk);
        // The user's rules take 11, 12, 13 back under the cover.
        desk.hypr.reload();
        assert_eq!(desk.hypr.place("0x12a").0, "DP-1");
        desk.events.send(hypr::Event::ConfigReloaded).unwrap();
        until(|| placement(&desk).0 == running.0).await;
        assert_eq!(desk.hypr.shown("DP-3"), 1);
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(placement(&desk), before);
    }

    #[tokio::test]
    async fn two_reserved_monitors_each_get_their_own_workspaces_back() {
        let desk = per_monitor_desk("monitor = DP-1, DP-2\n");
        let before = placement(&desk);
        let (_remote, text) = started_with(&desk, &["NAVETTE-1", "NAVETTE-2"]).await;
        text.unwrap();
        let mut running = on("DP-3", &["1", "11", "12", "13", "2", "21", "22"]);
        running.sort();
        assert_eq!(desk.hypr.workspaces(), running);
        assert_eq!(desk.hypr.shown("DP-3"), 1);
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(placement(&desk), before);
    }

    #[tokio::test]
    async fn a_dropped_link_gives_the_workspaces_back() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let before = placement(&desk);
        let remote = started(&desk).await;
        drop(remote);
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        until(|| placement(&desk) == before).await;
    }

    #[tokio::test]
    async fn the_workspace_a_reserved_monitor_showed_comes_back_without_a_slide_from_home() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let _remote = started(&desk).await;
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(desk.hypr.shown("DP-1"), 11);
        assert_eq!(desk.hypr.stray_animations(), []);
        desk.hypr.switch_to("DP-1", 12);
        desk.hypr.switch_to("DP-1", 11);
        assert_eq!(desk.hypr.stray_animations(), []);
    }

    #[tokio::test]
    async fn a_workspace_visited_at_home_during_the_mode_comes_back_without_a_slide_from_home() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let _remote = started(&desk).await;
        // Home goes to two of the moved workspaces, which lays their windows out on DP-3.
        desk.hypr.switch_to("DP-3", 12);
        desk.hypr.switch_to("DP-3", 13);
        desk.hypr.switch_to("DP-3", 1);
        answer(ask(&desk, false).await).await.unwrap();
        for id in [12, 13, 11] {
            desk.hypr.switch_to("DP-1", id);
        }
        assert_eq!(desk.hypr.stray_animations(), []);
    }

    #[tokio::test]
    async fn a_stop_gives_the_animations_back_as_the_user_had_them() {
        for on in [true, false] {
            let desk = per_monitor_desk("monitor = DP-1\n");
            desk.hypr.set_animations(on);
            let _remote = started(&desk).await;
            desk.hypr.switch_to("DP-3", 12);
            answer(ask(&desk, false).await).await.unwrap();
            assert_eq!(desk.hypr.animations_on(), on);
        }
    }

    #[tokio::test]
    async fn the_workspaces_a_crashed_mode_left_come_back_without_a_slide_at_the_next_start() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let crashed = Home {
            config_dir: desk._config.path().to_path_buf(),
            hypr: desk.hypr.clone(),
            screens: Arc::new(FakeScreens::default()),
            gpu: Arc::new(HomeGpu::new(None)),
            desk_record: desk._config.path().join("desk"),
            mic: None,
            sound: None,
            board: Arc::default(),
            notifier: Arc::new(crate::notify::Fake::default()),
        };
        open_desk(
            &crashed,
            &["DP-1".to_string()],
            Some("DP-3".to_string()),
            "SUPER + CTRL + M",
            &[],
        )
        .await
        .unwrap()
        .crash();
        desk.hypr.switch_to("DP-3", 12);
        let _remote = started(&desk).await;
        answer(ask(&desk, false).await).await.unwrap();
        for id in [12, 13, 11] {
            desk.hypr.switch_to("DP-1", id);
        }
        assert_eq!(desk.hypr.stray_animations(), []);
    }

    #[tokio::test]
    async fn stopping_undoes_the_home_window_rule() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        answer(ask(&desk, false).await).await.unwrap();
        until(|| desk.hypr.window_rules().len() == 2).await;
        assert!(desk.hypr.window_rules()[1].contains("enabled = false"));
    }

    #[tokio::test]
    async fn a_reload_brings_the_home_window_rule_back() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        desk.events.send(hypr::Event::ConfigReloaded).unwrap();
        until(|| desk.hypr.window_rules().len() == 2).await;
        let rules = desk.hypr.window_rules();
        assert_eq!(rules[0], rules[1]);
    }

    /// The rule that draws Navette's picture last among the overlay layers.
    const ABOVE: &str = r#"match = { namespace = "^navette$" }, order = -1000"#;

    fn below(namespace: &str) -> String {
        format!(r#"match = {{ namespace = "^{namespace}$" }}, order = -2000"#)
    }

    #[tokio::test]
    async fn mode_start_sends_the_layer_rules_for_navette_s_namespace_and_the_listed_ones_only() {
        let desk = desk_with("monitor = DP-2\nlocal_above = selection, invented.picker\n");
        let _remote = started(&desk).await;
        let rules = desk.hypr.layer_rules();
        assert_eq!(rules.len(), 3, "{rules:?}");
        assert!(rules[0].contains(ABOVE), "{rules:?}");
        assert!(rules[1].contains(&below("selection")), "{rules:?}");
        // A namespace is matched whole, its dots as dots.
        assert!(
            rules[2].contains(&below("invented\\\\.picker")),
            "{rules:?}"
        );
        assert!(rules.iter().all(|r| r.contains("name = \"navette-")));
    }

    #[tokio::test]
    async fn by_default_slurp_s_and_hyprpicker_s_overlays_stay_above() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        let rules = desk.hypr.layer_rules();
        assert_eq!(rules.len(), 3, "{rules:?}");
        assert!(rules[1].contains(&below("selection")), "{rules:?}");
        assert!(rules[2].contains(&below("hyprpicker")), "{rules:?}");
    }

    #[tokio::test]
    async fn with_local_above_none_only_navette_s_rule_is_sent() {
        let desk = desk_with("monitor = DP-2\nlocal_above = none\n");
        let _remote = started(&desk).await;
        let rules = desk.hypr.layer_rules();
        assert_eq!(rules.len(), 1, "{rules:?}");
        assert!(rules[0].contains(ABOVE), "{rules:?}");
    }

    #[tokio::test]
    async fn the_layer_rules_are_set_before_the_surfaces_map() {
        let desk = desk("DP-2");
        let (hypr, seen) = (desk.hypr.clone(), Arc::new(Mutex::new(Vec::new())));
        let at_open = seen.clone();
        *desk.on_open.lock().unwrap() = Some(Box::new(move || {
            at_open.lock().unwrap().push(hypr.layer_rules().len());
        }));
        let _remote = started(&desk).await;
        assert_eq!(*seen.lock().unwrap(), [3]);
    }

    #[tokio::test]
    async fn configreloaded_sends_the_layer_rules_again_then_pokes_each_surface() {
        let desk = desk_on(
            "monitor = DP-2, DP-3\n",
            vec![
                FakeMonitor::new("DP-1", 1920, 1080, 0),
                FakeMonitor::new("DP-2", 1920, 1080, 1920),
                FakeMonitor::new("DP-3", 1920, 1080, 3840),
            ],
            FakeMic::default(),
        );
        let (_remote, text) = started_with(&desk, &["NAVETTE-1", "NAVETTE-2"]).await;
        text.unwrap();
        assert!(desk.pokes.lock().unwrap().is_empty());
        desk.events.send(hypr::Event::ConfigReloaded).unwrap();
        until(|| desk.pokes.lock().unwrap().len() == 2).await;
        let rules = desk.hypr.layer_rules();
        assert_eq!(rules.len(), 6, "{rules:?}");
        assert_eq!(rules[..3], rules[3..]);
        assert_eq!(
            *desk.pokes.lock().unwrap(),
            [("DP-2".to_string(), 6), ("DP-3".to_string(), 6)]
        );
    }

    #[tokio::test]
    async fn another_event_pokes_no_surface() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        desk.events.send(hypr::Event::Placed).unwrap();
        desk.events.send(hypr::Event::ConfigReloaded).unwrap();
        // Events are handled in order, so Placed is over before the reload's poke: waiting for
        // that poke (taken after the 3 rules are sent again) also waits out a wrong poke on Placed.
        until(|| {
            desk.pokes
                .lock()
                .unwrap()
                .iter()
                .any(|(_, rules)| *rules == 6)
        })
        .await;
        assert_eq!(desk.pokes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn mode_stop_disables_the_layer_rules() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        answer(ask(&desk, false).await).await.unwrap();
        let rules = desk.hypr.layer_rules();
        assert_eq!(rules.len(), 6, "{rules:?}");
        for (set, undone) in rules[..3].iter().zip(rules[3..].iter().rev()) {
            let name = &set[..set.find(',').unwrap()];
            assert_eq!(*undone, format!("{{ {}, enabled = false }}", &name[2..]));
        }
    }

    #[tokio::test]
    async fn the_layer_rules_a_crashed_mode_left_are_disabled_at_the_next_start() {
        let desk = desk("DP-2");
        let crashed = Home {
            config_dir: desk._config.path().to_path_buf(),
            hypr: desk.hypr.clone(),
            screens: Arc::new(FakeScreens::default()),
            gpu: Arc::new(HomeGpu::new(None)),
            desk_record: desk._config.path().join("desk"),
            mic: None,
            sound: None,
            board: Arc::default(),
            notifier: Arc::new(crate::notify::Fake::default()),
        };
        open_desk(
            &crashed,
            &["DP-2".to_string()],
            Some("DP-1".to_string()),
            "SUPER + CTRL + M",
            &["selection".to_string()],
        )
        .await
        .unwrap()
        .crash();
        let left = desk.hypr.layer_rules();
        assert_eq!(left.len(), 2, "{left:?}");
        let _remote = started(&desk).await;
        let rules = desk.hypr.layer_rules();
        // Each leftover is undone by its full name, the last one set first.
        let name = |rule: &str| rule[2..rule.find(',').unwrap()].to_string();
        assert_eq!(
            rules[2],
            format!("{{ {}, enabled = false }}", name(&left[1]))
        );
        assert_eq!(
            rules[3],
            format!("{{ {}, enabled = false }}", name(&left[0]))
        );
    }

    #[tokio::test]
    async fn the_remote_cursor_is_drawn_on_the_cover() {
        let desk = desk("DP-2");
        let remote = started(&desk).await;
        let cursor = Cursor {
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            argb: bytes::Bytes::from_static(&[1, 2, 3, 255]),
        };
        remote.pointer.send_replace(vec![Pointer {
            shape: Some(cursor.clone()),
            visible: true,
        }]);
        until(|| !desk.cursors.lock().unwrap().is_empty()).await;
        assert_eq!(desk.cursors.lock().unwrap()[0].1.shape, Some(cursor));
    }

    #[test]
    fn the_mode_config_of_the_docs_is_read_comments_and_all() {
        let docs = include_str!("../docs/mode.md");
        let example = docs
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("release =") && line.contains('#'))
            .expect("the docs show a release hotkey with a comment");
        let config = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join(CONFIG_FILE),
            format!("# the work monitor\nmonitor = DP-2   # the middle one\n{example}\n"),
        )
        .unwrap();
        let mode = Mode::read(config.path(), None).unwrap();
        assert_eq!(mode.monitors, ["DP-2"]);
        assert_eq!(mode.release, Hotkey::parse("ctrl+alt+q").unwrap());
    }

    #[tokio::test]
    async fn the_release_hotkey_comes_from_the_mode_config() {
        let desk = desk("DP-2");
        started(&desk).await;
        let default = Hotkey::parse(input::DEFAULT_HOTKEY).unwrap();
        assert_eq!(*desk.hotkeys.lock().unwrap(), [default]);

        let desk = desk_with("monitor = DP-2\nrelease = ctrl+alt+q\n");
        started(&desk).await;
        let chosen = Hotkey::parse("ctrl+alt+q").unwrap();
        assert_eq!(*desk.hotkeys.lock().unwrap(), [chosen]);
    }

    fn closed_lid_sent(remote: &Remote) -> Vec<Option<bool>> {
        remote
            .outbox
            .queued()
            .into_iter()
            .filter_map(|m| match m {
                Msg::Display(Display::Start { closed_lid, .. }) => Some(closed_lid),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn the_lid_is_asked_with_every_mode_with_no_key_in_the_config() {
        // An agent of the release before keeps its lid only when asked: it is always asked.
        for text in [
            "monitor = DP-2\n",
            "monitor = DP-2\nclosed_lid = off\n",
            "monitor = DP-2\nclosed_lid = on\n",
        ] {
            let desk = desk_with(text);
            let remote = connect(&desk);
            let _pending = ask(&desk, true).await;
            until(|| !starts(&remote).is_empty()).await;
            assert_eq!(closed_lid_sent(&remote), [Some(true)], "{text}");
        }
    }

    /// A mode of `text` the remote says started, `said` back: `mode start`'s answer.
    async fn started_saying(text: &str, said: Option<bool>) -> String {
        let desk = desk_with(text);
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: said,
            }))
            .unwrap();
        answer(pending).await.unwrap()
    }

    #[tokio::test]
    async fn an_agent_that_does_not_keep_its_lid_is_said_at_start() {
        let text = started_saying("monitor = DP-2\n", None).await;
        assert!(text.contains("lid") && text.contains("update"), "{text}");
        let text = started_saying("monitor = DP-2\n", Some(true)).await;
        assert!(!text.contains("lid"), "{text}");
    }

    #[tokio::test]
    async fn a_config_that_still_names_closed_lid_starts_with_a_notice() {
        for value in ["on", "off", "yes please"] {
            let config = format!("monitor = DP-2\nclosed_lid = {value}\n");
            let text = started_saying(&config, Some(true)).await;
            assert!(text.contains("closed_lid"), "{text}");
            assert!(!text.contains('\n'), "{text}");
        }
    }

    #[tokio::test]
    async fn a_config_that_still_names_home_label_or_home_bar_starts_with_a_notice() {
        for (config, new) in [
            ("monitor = DP-2\nhome_label = NEAR\n", "local_label"),
            ("monitor = DP-2\nhome_bar = auto\n", "local_bar"),
            (
                "monitor = DP-2\nhome_bar = 5\nlocal_bar = auto\n",
                "local_bar",
            ),
        ] {
            let text = started_saying(config, Some(true)).await;
            assert!(text.contains(new), "{config}: {text}");
            assert!(!text.contains('\n'), "{config}: {text}");
        }
        let text = started_saying("monitor = DP-2\nlocal_label = NEAR\n", Some(true)).await;
        assert!(!text.contains("local_label"), "{text}");
    }

    #[tokio::test]
    async fn the_corner_tags_are_neutral_by_default() {
        let desk = desk("DP-2");
        let _remote = started(&desk).await;
        let feeds = desk.feeds.lock().unwrap().clone();
        assert_eq!(feeds.len(), 2);
        for (monitor, feed) in feeds {
            assert_eq!(*feed.look(), Look::default(), "{monitor}");
        }
        let look = Look::default();
        assert_eq!(
            (look.remote_label.as_str(), look.home_label.as_str()),
            ("REMOTE", "LOCAL")
        );
    }

    #[tokio::test]
    async fn a_mode_names_its_own_corner_labels() {
        let desk = desk_with("remote_label = FAR\n[office]\nmonitor = DP-2\nlocal_label = NEAR\n");
        let _remote = started(&desk).await;
        // The home corner and the reserved monitor's corner each carry their own tag.
        let feeds = desk.feeds.lock().unwrap().clone();
        let tags = |side| {
            feeds
                .iter()
                .map(|(_, feed)| feed.corner(side).label().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(tags(Side::Remote), ["FAR", "FAR"]);
        assert_eq!(tags(Side::Home), ["NEAR", "NEAR"]);
    }

    #[tokio::test]
    async fn a_label_the_corner_cannot_draw_fails_clearly_and_changes_nothing() {
        for (label, why) in [
            ("remote_label = Café", "remote_label"),
            ("local_label = 1234567890123", "12 characters"),
        ] {
            let desk = desk_with(&format!("monitor = DP-2\n{label}\n"));
            let remote = connect(&desk);
            let error = answer(ask(&desk, true).await).await.unwrap_err();
            assert!(error.to_string().contains(why), "{label}: {error}");
            assert!(desk.covered.lock().unwrap().is_empty());
            assert!(remote.outbox.queued().is_empty());
            assert!(desk.hypr.changes().is_empty());
            assert_eq!(corners(&desk), []);
        }
    }

    #[tokio::test]
    async fn the_remote_corner_starts_at_the_top_unless_the_mode_sets_a_height() {
        let tall = desk_with("monitor = DP-2\nremote_bar = 40\n");
        let _remote = started(&tall).await;
        let feeds = tall.feeds.lock().unwrap().clone();
        assert!(!feeds.is_empty());
        for (_, feed) in feeds {
            assert_eq!(feed.look().remote_bar, 40);
        }
        let default = desk("DP-2");
        let _remote = started(&default).await;
        for (_, feed) in default.feeds.lock().unwrap().iter() {
            assert_eq!(feed.look().remote_bar, 0);
        }
    }

    #[tokio::test]
    async fn home_s_corner_starts_at_the_top_unless_the_mode_sets_it() {
        let top = desk_with("monitor = DP-2\nlocal_bar = 0\n");
        let _remote = started(&top).await;
        let feeds = top.feeds.lock().unwrap().clone();
        assert!(!feeds.is_empty());
        for (_, feed) in feeds {
            assert_eq!(feed.look().home_bar, Some(0));
        }
        let below = desk_with("monitor = DP-2\nlocal_bar = auto\n");
        let _remote = started(&below).await;
        for (_, feed) in below.feeds.lock().unwrap().iter() {
            assert_eq!(feed.look().home_bar, None);
        }
        let default = desk("DP-2");
        let _remote = started(&default).await;
        for (_, feed) in default.feeds.lock().unwrap().iter() {
            assert_eq!(feed.look().home_bar, Some(0));
        }
    }

    #[tokio::test]
    async fn a_local_bar_that_is_neither_auto_nor_pixels_fails_clearly_and_changes_nothing() {
        let desk = desk_with("monitor = DP-2\nlocal_bar = high\n");
        let remote = connect(&desk);
        let error = answer(ask(&desk, true).await).await.unwrap_err();
        assert!(error.to_string().contains("local_bar"), "{error}");
        assert!(desk.covered.lock().unwrap().is_empty());
        assert!(remote.outbox.queued().is_empty());
        assert!(desk.hypr.changes().is_empty());
        assert_eq!(corners(&desk), []);
    }

    #[tokio::test]
    async fn a_release_hotkey_with_no_known_key_fails_clearly_and_changes_nothing() {
        let desk = desk_with("monitor = DP-2\nrelease = shift+nokey\n");
        let remote = connect(&desk);
        let error = answer(ask(&desk, true).await).await.unwrap_err();
        assert!(error.to_string().contains("nokey"), "{error}");
        assert!(desk.covered.lock().unwrap().is_empty());
        assert!(remote.outbox.queued().is_empty());
        assert!(desk.hypr.changes().is_empty());
    }

    #[tokio::test]
    async fn a_monitor_that_does_not_exist_fails_clearly_and_changes_nothing() {
        let desk = desk("HDMI-A-9");
        let remote = connect(&desk);
        let error = answer(ask(&desk, true).await).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let text = error.to_string();
        assert!(
            text.contains("HDMI-A-9") && text.contains("DP-1, DP-2"),
            "{text}"
        );
        assert!(desk.covered.lock().unwrap().is_empty());
        assert!(remote.outbox.queued().is_empty());
        assert!(desk.hypr.changes().is_empty());
    }

    #[tokio::test]
    async fn no_connected_remote_fails_clearly_and_changes_nothing() {
        let desk = desk("DP-1");
        let error = answer(ask(&desk, true).await).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
        assert!(error.to_string().contains("not connected"));
        assert!(desk.covered.lock().unwrap().is_empty());
        assert!(desk.hypr.changes().is_empty());
    }

    #[tokio::test]
    async fn no_mode_configured_says_where_to_write_it() {
        let desk = desk("DP-1");
        std::fs::remove_file(desk._config.path().join(CONFIG_FILE)).unwrap();
        let _remote = connect(&desk);
        let error = answer(ask(&desk, true).await).await.unwrap_err();
        assert!(error.to_string().contains("monitor = <output name>"));
        assert!(desk.covered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stopping_removes_the_surface_and_tells_the_remote() {
        let desk = desk("DP-1");
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: Some(true),
            }))
            .unwrap();
        answer(pending).await.unwrap();

        answer(ask(&desk, false).await).await.unwrap();
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        assert_eq!(
            remote.outbox.queued().last(),
            Some(&Msg::Display(Display::Stop))
        );
    }

    #[tokio::test]
    async fn a_remote_that_cannot_make_its_output_leaves_home_unchanged() {
        let desk = desk("DP-1");
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Failed {
                generation: None,
                reason: "no encoder".into(),
            }))
            .unwrap();
        let error = answer(pending).await.unwrap_err();
        assert!(error.to_string().contains("no encoder"));
        until(|| desk.covered.lock().unwrap().is_empty()).await;
    }

    #[tokio::test]
    async fn a_dropped_link_gives_the_monitor_back() {
        let desk = desk("DP-1");
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: Some(true),
            }))
            .unwrap();
        answer(pending).await.unwrap();
        drop(remote);
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        until(|| desk.hypr.window_rules().len() == 2).await;
        assert!(desk.hypr.window_rules()[1].contains("enabled = false"));
    }

    const DROPPED_WHILE_STARTING: &str = "the link to the remote machine dropped while the mode \
                                          was starting; nothing was changed";

    #[tokio::test]
    async fn a_link_drop_during_start_rolls_back_and_says_so() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let before = placement(&desk);
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        drop(remote);
        let error = answer(pending).await.unwrap_err();
        assert_eq!(error.to_string(), DROPPED_WHILE_STARTING);
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        assert_eq!(placement(&desk), before);
        assert!(desk.hypr.window_rules()[1].contains("enabled = false"));
        assert_eq!(desk.mic.log.lock().unwrap().last(), Some(&"stop"));
        // Nothing runs any more: the next start is not refused.
        let again = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&again).is_empty()).await;
        drop(pending);
    }

    #[tokio::test]
    async fn a_link_drop_while_the_covers_go_up_says_so_too() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let before = placement(&desk);
        let Remote {
            outbox,
            to_home,
            pointer,
            input,
            ..
        } = connect(&desk);
        let _kept = (pointer, input);
        // The link goes: its queue closes, and so does its inbox with the last sender.
        let to_home = Mutex::new(Some(to_home));
        *desk.on_open.lock().unwrap() = Some(Box::new(move || {
            outbox.close();
            to_home.lock().unwrap().take();
        }));
        let pending = ask(&desk, true).await;
        let error = answer(pending).await.unwrap_err();
        assert_eq!(error.to_string(), DROPPED_WHILE_STARTING);
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        assert_eq!(placement(&desk), before);
    }

    #[tokio::test]
    async fn a_link_replaced_during_start_says_the_same() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let before = placement(&desk);
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        let _new = connect(&desk);
        let error = answer(pending).await.unwrap_err();
        assert_eq!(error.to_string(), DROPPED_WHILE_STARTING);
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        assert_eq!(placement(&desk), before);
    }

    #[tokio::test]
    async fn a_desk_that_cannot_be_given_back_is_not_said_unchanged() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let remote = connect(&desk);
        let pending = ask(&desk, true).await;
        until(|| !starts(&remote).is_empty()).await;
        desk.hypr.set_down(true, Vec::new());
        drop(remote);
        let error = answer(pending).await.unwrap_err().to_string();
        assert!(
            error.starts_with(
                "the link to the remote machine dropped while the mode was starting; "
            ),
            "{error}"
        );
        assert!(!error.contains("nothing was changed"), "{error}");
        assert!(error.contains("next start"), "{error}");
    }

    #[tokio::test]
    async fn a_link_drop_after_the_mode_is_up_keeps_today_s_behaviour() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let before = placement(&desk);
        let remote = started(&desk).await;
        drop(remote);
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        until(|| placement(&desk) == before).await;
        // The start was answered long ago: the drop is only said by serve.
        let answer = answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(answer, "no mode was running");
    }

    #[tokio::test]
    async fn a_link_that_closed_before_the_start_keeps_the_not_connected_message() {
        let desk = per_monitor_desk("monitor = DP-1\n");
        let before = placement(&desk);
        let remote = connect(&desk);
        remote.outbox.close();
        let error = answer(ask(&desk, true).await).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
        assert!(error.to_string().contains("nothing was changed"));
        assert_eq!(placement(&desk), before);
    }

    #[tokio::test]
    async fn the_cli_reaches_serve_over_the_control_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navette").join("control.sock");
        let listener = listen(&path).unwrap();
        let (orders, mut orders_rx) = mpsc::unbounded_channel();
        tokio::spawn(accept_orders(listener, orders));
        tokio::spawn(async move {
            while let Some(order) = orders_rx.recv().await {
                match order {
                    Order::Start(name, reply) => {
                        let why = format!("no monitor X for {name:?}");
                        let _ = reply.send(Err(io::Error::other(why)));
                    }
                    Order::Stop(reply) => {
                        let _ = reply.send(Ok("DP-1 is back home".into()));
                    }
                    Order::Status(reply) => {
                        let _ = reply.send(Ok("no mode is running".into()));
                    }
                }
            }
        });
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(order(&path, "stop").await.unwrap(), "DP-1 is back home");
        let error = order(&path, "start").await.unwrap_err();
        assert_eq!(error.to_string(), "no monitor X for None");
        let error = order(&path, &start_order(Some("office")).unwrap())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "no monitor X for Some(\"office\")");
        assert_eq!(order(&path, "status").await.unwrap(), "no mode is running");
        assert!(listen(&path).is_err(), "a second serve is refused");
    }

    async fn control(dir: &Path) -> (PathBuf, UnboundedReceiver<Order>) {
        let path = dir.join("navette").join("control.sock");
        let listener = listen(&path).unwrap();
        let (orders, orders_rx) = mpsc::unbounded_channel();
        tokio::spawn(accept_orders(listener, orders));
        (path, orders_rx)
    }

    /// Reads until the other side closes, in time.
    async fn closed(stream: &mut UnixStream) {
        use tokio::io::AsyncReadExt;
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut rest))
            .await
            .expect("serve lets the connection go")
            .unwrap();
    }

    #[tokio::test]
    async fn a_control_client_that_says_nothing_is_let_go() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut orders) = control(dir.path()).await;
        let mut silent = UnixStream::connect(&path).await.unwrap();
        closed(&mut silent).await;
        assert!(orders.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_endless_control_line_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut orders) = control(dir.path()).await;
        let stream = UnixStream::connect(&path).await.unwrap();
        let (mut read, mut write) = stream.into_split();
        tokio::spawn(async move {
            let _ = write.write_all(&vec![b's'; 1 << 20]).await;
        });
        use tokio::io::AsyncReadExt;
        let mut rest = Vec::new();
        // Closed with bytes unread: the end may come as a reset.
        let _ = tokio::time::timeout(Duration::from_secs(2), read.read_to_end(&mut rest))
            .await
            .expect("serve lets the connection go");
        assert!(orders.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_cli_gives_up_on_a_serve_that_never_answers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let _deaf = UnixListener::bind(&path).unwrap();
        let error = order_within(&path, "start", Duration::from_millis(200))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn a_socket_left_by_a_serve_that_crashed_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navette").join("control.sock");
        drop(listen(&path).unwrap());
        assert!(path.exists(), "the crashed serve left its socket");
        let listener = listen(&path).unwrap();
        let (orders, mut orders_rx) = mpsc::unbounded_channel();
        tokio::spawn(accept_orders(listener, orders));
        tokio::spawn(async move {
            if let Some(Order::Stop(reply)) = orders_rx.recv().await {
                let _ = reply.send(Ok("no mode was running".into()));
            }
        });
        assert_eq!(order(&path, "stop").await.unwrap(), "no mode was running");
    }

    #[tokio::test]
    async fn the_cli_says_when_serve_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let error = order(&dir.path().join("control.sock"), "start")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("navette serve"));
    }

    /// Every start's copies, output 0 first.
    fn all_starts(remote: &Remote) -> Vec<Vec<OutputCopy>> {
        remote
            .outbox
            .queued()
            .into_iter()
            .filter_map(|m| match m {
                Msg::Display(Display::Start { output, others, .. }) => {
                    let mut copies = vec![output];
                    copies.extend(others.unwrap_or_default());
                    Some(copies)
                }
                _ => None,
            })
            .collect()
    }

    /// The maintainer's desk, with one more on the right: a portrait monitor kept home, then
    /// three landscape ones.
    fn four_monitors(config_text: &str) -> Desk {
        let mut portrait = FakeMonitor::new("HDMI-A-1", 2560, 1440, 0);
        portrait.transform = 1;
        let monitors = vec![
            FakeMonitor::new("DP-3", 1920, 1080, 4000),
            portrait,
            FakeMonitor::new("DP-1", 2560, 1440, 1440),
            FakeMonitor::new("DP-2", 1920, 1080, 4000 + 1920),
        ];
        desk_on(config_text, monitors, FakeMic::default())
    }

    fn copy_of(width: u32, height: u32, transform: u8) -> OutputCopy {
        OutputCopy {
            width,
            height,
            refresh_mhz: 60_000,
            scale_milli: 1000,
            transform,
        }
    }

    async fn started_with(desk: &Desk, outputs: &[&str]) -> (Remote, io::Result<String>) {
        let remote = connect(desk);
        let pending = ask(desk, true).await;
        until(|| !all_starts(&remote).is_empty()).await;
        let others: Vec<String> = outputs[1..].iter().map(|o| o.to_string()).collect();
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation: None,
                output: outputs[0].into(),
                others: (!others.is_empty()).then_some(others),
                closed_lid: Some(true),
            }))
            .unwrap();
        let text = answer(pending).await;
        (remote, text)
    }

    #[tokio::test]
    async fn the_monitors_given_are_copied_in_home_s_left_to_right_order_portrait_included() {
        // Not side by side, and named in another order than they stand.
        let desk = four_monitors("monitor = DP-2, HDMI-A-1\n");
        let (remote, text) = started_with(&desk, &["NAVETTE-1", "NAVETTE-2"]).await;
        assert_eq!(
            all_starts(&remote),
            [vec![copy_of(2560, 1440, 1), copy_of(1920, 1080, 0)]]
        );
        assert_eq!(*desk.covered.lock().unwrap(), ["HDMI-A-1", "DP-2"]);
        let text = text.unwrap();
        assert_eq!(
            text,
            "HDMI-A-1, DP-2 show the remote machine's NAVETTE-1, NAVETTE-2"
        );
    }

    #[tokio::test]
    async fn home_windows_are_kept_off_every_monitor_given() {
        let desk = four_monitors("monitor = DP-3, DP-2\n");
        let (_remote, _) = started_with(&desk, &["NAVETTE-1", "NAVETTE-2"]).await;
        let rules = desk.hypr.window_rules();
        assert_eq!(rules.len(), 2, "{rules:?}");
        // To the leftmost monitor kept home.
        assert!(
            rules[0].contains(r#"match = { workspace = "m[DP-3]" }, monitor = "HDMI-A-1""#),
            "{rules:?}"
        );
        assert!(
            rules[1].contains(r#"match = { workspace = "m[DP-2]" }, monitor = "HDMI-A-1""#),
            "{rules:?}"
        );
        let text = answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(text, "DP-3, DP-2 are back home");
        let rules = desk.hypr.window_rules();
        assert_eq!(rules.len(), 4, "{rules:?}");
        assert!(rules[2..].iter().all(|r| r.contains("enabled = false")));
    }

    #[tokio::test]
    async fn each_surface_s_input_and_cursor_are_its_own_output_s() {
        let desk = four_monitors("monitor = DP-3, DP-2\n");
        let (remote, _) = started_with(&desk, &["NAVETTE-1", "NAVETTE-2"]).await;
        let inputs = desk.inputs.lock().unwrap().clone();
        for (monitor, queue) in &inputs {
            queue.push(input::Event::Motion {
                x: if monitor == "DP-3" { 1919.0 } else { 0.0 },
                y: 5.0,
                delta: None,
            });
        }
        assert_eq!(
            remote.input.to_peer.queued_on(),
            [
                (
                    0,
                    input::Event::Motion {
                        x: 1919.0,
                        y: 5.0,
                        delta: None
                    }
                ),
                (
                    1,
                    input::Event::Motion {
                        x: 0.0,
                        y: 5.0,
                        delta: None
                    }
                ),
            ]
        );
        let hidden = Pointer {
            shape: None,
            visible: false,
        };
        remote
            .pointer
            .send_replace(vec![hidden.clone(), Pointer::default()]);
        until(|| desk.cursors.lock().unwrap().len() == 2).await;
        let cursors = desk.cursors.lock().unwrap().clone();
        assert_eq!(
            cursors,
            [
                ("DP-3".to_string(), hidden),
                ("DP-2".to_string(), Pointer::default())
            ]
        );
    }

    #[tokio::test]
    async fn any_surface_that_goes_away_gives_every_monitor_back() {
        let desk = four_monitors("monitor = DP-3, DP-2\n");
        let (remote, _) = started_with(&desk, &["NAVETTE-1", "NAVETTE-2"]).await;
        let end = desk.ends.lock().unwrap()[1].clone();
        end.send("the monitor was unplugged".into()).unwrap();
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        assert_eq!(
            remote.outbox.queued().last(),
            Some(&Msg::Display(Display::Stop))
        );
    }

    #[tokio::test]
    async fn a_remote_that_makes_one_output_for_two_monitors_is_told_to_update() {
        let desk = four_monitors("monitor = DP-3, DP-2\n");
        let (remote, text) = started_with(&desk, &["NAVETTE-1"]).await;
        let error = text.unwrap_err().to_string();
        assert_eq!(
            error,
            "the mode ended: the remote machine made 1 output(s) for 2 monitors: update \
             navette there"
        );
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        assert_eq!(
            remote.outbox.queued().last(),
            Some(&Msg::Display(Display::Stop))
        );
    }

    #[tokio::test]
    async fn a_missing_monitor_among_several_fails_clearly_and_changes_nothing() {
        let desk = four_monitors("monitor = DP-3, DP-9\n");
        let remote = connect(&desk);
        let error = answer(ask(&desk, true).await).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("DP-9"), "{error}");
        assert!(desk.covered.lock().unwrap().is_empty());
        assert!(remote.outbox.queued().is_empty());
        assert!(desk.hypr.changes().is_empty());
    }

    fn two_modes() -> &'static str {
        "[office]\nmonitor = DP-1\n[travel]\nmonitor = DP-2\n"
    }

    /// Answers the remote's side of a start: the output it made, once home asked for it.
    async fn made(remote: &Remote, asked: usize) {
        made_for(remote, asked, None).await;
    }

    /// The generation home gave its `nth` start (from 0).
    fn generation_of(remote: &Remote, nth: usize) -> u32 {
        remote
            .outbox
            .queued()
            .into_iter()
            .filter_map(|m| match m {
                Msg::Display(Display::Start { generation, .. }) => generation,
                _ => None,
            })
            .nth(nth)
            .expect("home numbers its starts")
    }

    /// The remote made its output for the start of this generation (none: an agent that does
    /// not number its answers).
    async fn made_for(remote: &Remote, asked: usize, generation: Option<u32>) {
        until(|| starts(remote).len() == asked).await;
        remote
            .to_home
            .try_send(Incoming::Display(Display::Started {
                generation,
                output: "NAVETTE-1".into(),
                others: None,
                closed_lid: Some(true),
            }))
            .unwrap();
    }

    #[test]
    fn a_mode_name_on_the_control_line_is_checked() {
        assert_eq!(start_order(None).unwrap(), "start");
        assert_eq!(start_order(Some("office-2")).unwrap(), "start office-2");
        for bad in ["", "two words", "a\nstop", &"n".repeat(33)] {
            let error = start_order(Some(bad)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_named_mode_gives_its_own_monitors() {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let pending = ask_mode(&desk, "travel").await;
        made(&remote, 1).await;
        assert_eq!(
            answer(pending).await.unwrap(),
            "DP-2 shows the remote machine's NAVETTE-1"
        );
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-2"]);
        assert_eq!(starts(&remote)[0].width, 2560);
    }

    #[tokio::test]
    async fn with_no_name_and_several_modes_nothing_starts_and_the_names_are_said() {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let error = answer(ask(&desk, true).await).await.unwrap_err();
        assert!(error.to_string().contains("office, travel"), "{error}");
        assert!(desk.covered.lock().unwrap().is_empty());
        assert!(remote.outbox.queued().is_empty());
        assert!(desk.hypr.changes().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_mode_name_changes_nothing() {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let error = answer(ask_mode(&desk, "holiday").await).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("office, travel"), "{error}");
        assert!(desk.covered.lock().unwrap().is_empty());
        assert!(remote.outbox.queued().is_empty());
        assert!(desk.hypr.changes().is_empty());
    }

    #[tokio::test]
    async fn a_mode_naming_a_monitor_that_is_not_connected_changes_nothing() {
        let desk = desk_with("[office]\nmonitor = DP-1\n[away]\nmonitor = DP-1, HDMI-A-9\n");
        let remote = connect(&desk);
        let error = answer(ask_mode(&desk, "away").await).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("HDMI-A-9"), "{error}");
        assert!(desk.covered.lock().unwrap().is_empty());
        assert!(remote.outbox.queued().is_empty());
        assert!(desk.hypr.changes().is_empty());
    }

    #[tokio::test]
    async fn a_monitor_named_by_its_description_survives_a_change_of_connectors() {
        let config = "[desk]\nmonitor = desc:Dell Inc. DELL U2723QE\n";
        let dell = "Dell Inc. DELL U2723QE 7XYZ123";
        let plugged = |first: &str, second: &str| {
            vec![
                FakeMonitor::new("DP-1", 1920, 1080, 0).described(first),
                FakeMonitor::new("DP-2", 2560, 1440, 1920).described(second),
            ]
        };
        for (monitors, covered) in [
            (plugged(dell, "LG Electronics LG ULTRAWIDE 0123"), "DP-1"),
            (plugged("LG Electronics LG ULTRAWIDE 0123", dell), "DP-2"),
        ] {
            let desk = desk_on(config, monitors, FakeMic::default());
            let remote = connect(&desk);
            let pending = ask_mode(&desk, "desk").await;
            made(&remote, 1).await;
            answer(pending).await.unwrap();
            assert_eq!(*desk.covered.lock().unwrap(), [covered]);
        }
    }

    #[tokio::test]
    async fn starting_another_mode_gives_the_first_back_then_starts_the_other_and_says_so() {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let first = ask_mode(&desk, "office").await;
        made(&remote, 1).await;
        answer(first).await.unwrap();
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-1"]);

        let second = ask_mode(&desk, "travel").await;
        made(&remote, 2).await;
        let text = answer(second).await.unwrap();
        assert_eq!(
            text,
            "mode `office` stopped, DP-1 is back home; \
             DP-2 shows the remote machine's NAVETTE-1"
        );
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-2"]);
        // The remote is told to give its output back before it is asked for the next one.
        let sequence: Vec<_> = remote
            .outbox
            .queued()
            .into_iter()
            .filter_map(|m| match m {
                Msg::Display(Display::Start { .. }) => Some("start"),
                Msg::Display(Display::Stop) => Some("stop"),
                _ => None,
            })
            .collect();
        assert_eq!(sequence, ["start", "stop", "start"]);
        // The mic and the sound went back home with the first mode, and came again.
        assert_eq!(mic_log(&desk), ["start", "stop", "start"]);
        assert_eq!(sound_log(&desk), ["start", "stop", "start"]);
        assert!(
            status_of(&desk)
                .await
                .starts_with("mode `travel` runs on DP-2")
        );
    }

    #[tokio::test]
    async fn a_switch_to_a_mode_that_fails_its_checks_leaves_the_running_one_alone() {
        let desk = desk_with("[office]\nmonitor = DP-1\n[away]\nmonitor = HDMI-A-9\n");
        let remote = connect(&desk);
        let first = ask_mode(&desk, "office").await;
        made(&remote, 1).await;
        answer(first).await.unwrap();
        let queued = remote.outbox.queued().len();

        for name in ["holiday", "away"] {
            let error = answer(ask_mode(&desk, name).await).await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound, "{name}");
        }
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-1"]);
        assert_eq!(
            remote.outbox.queued().len(),
            queued,
            "the remote is told nothing"
        );
        assert_eq!(mic_log(&desk), ["start"]);
        assert!(
            status_of(&desk)
                .await
                .starts_with("mode `office` runs on DP-1")
        );
    }

    #[tokio::test]
    async fn starting_the_running_mode_again_is_refused() {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let first = ask_mode(&desk, "office").await;
        made(&remote, 1).await;
        answer(first).await.unwrap();

        let error = answer(ask_mode(&desk, "office").await).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("mode `office`"), "{error}");
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-1"]);
        assert_eq!(starts(&remote).len(), 1);
    }

    #[tokio::test]
    async fn stopping_a_named_mode_gives_its_monitors_back() {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let first = ask_mode(&desk, "travel").await;
        made(&remote, 1).await;
        answer(first).await.unwrap();
        assert_eq!(
            answer(ask(&desk, false).await).await.unwrap(),
            "DP-2 is back home"
        );
        assert!(desk.covered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_status_says_what_runs_and_whether_the_remote_machine_is_there() {
        let desk = desk_with(two_modes());
        assert_eq!(
            status_of(&desk).await,
            "no mode is running; the remote machine is not connected"
        );
        let remote = connect(&desk);
        assert_eq!(
            status_of(&desk).await,
            "no mode is running; the remote machine is connected"
        );
        let pending = ask_mode(&desk, "office").await;
        until(|| !starts(&remote).is_empty()).await;
        assert_eq!(
            status_of(&desk).await,
            "mode `office` is starting on DP-1; the remote machine is connected"
        );
        made(&remote, 1).await;
        answer(pending).await.unwrap();
        assert_eq!(
            status_of(&desk).await,
            "mode `office` runs on DP-1; the remote machine is connected"
        );
        answer(ask(&desk, false).await).await.unwrap();
        assert_eq!(
            status_of(&desk).await,
            "no mode is running; the remote machine is connected"
        );
    }

    #[tokio::test]
    async fn the_status_of_the_unnamed_mode_has_no_name() {
        let desk = desk("DP-1");
        let _remote = started(&desk).await;
        assert_eq!(
            status_of(&desk).await,
            "the mode runs on DP-1; the remote machine is connected"
        );
    }

    fn failure_of(generation: Option<u32>, reason: &str) -> Incoming {
        Incoming::Display(Display::Failed {
            reason: reason.into(),
            generation,
        })
    }

    #[tokio::test]
    async fn a_switch_is_refused_while_the_running_mode_is_still_starting() {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let first = ask_mode(&desk, "office").await;
        until(|| starts(&remote).len() == 1).await;
        let queued = remote.outbox.queued().len();

        let error = answer(ask_mode(&desk, "travel").await).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ResourceBusy);
        let text = error.to_string();
        assert!(text.contains("mode `office` is still starting"), "{text}");
        assert!(text.contains("navette mode stop"), "{text}");
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-1"]);
        assert_eq!(
            remote.outbox.queued().len(),
            queued,
            "the remote is told nothing"
        );

        // The first start goes on, and the switch is possible once it is up.
        made_for(&remote, 1, Some(generation_of(&remote, 0))).await;
        answer(first).await.unwrap();
        let second = ask_mode(&desk, "travel").await;
        until(|| starts(&remote).len() == 2).await;
        made_for(&remote, 2, Some(generation_of(&remote, 1))).await;
        answer(second).await.unwrap();
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-2"]);
    }

    #[tokio::test]
    async fn the_failure_of_the_stopped_mode_does_not_end_the_one_that_replaced_it() {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let first = ask_mode(&desk, "office").await;
        made_for(&remote, 1, None).await;
        answer(first).await.unwrap();
        let before = generation_of(&remote, 0);

        let second = ask_mode(&desk, "travel").await;
        until(|| starts(&remote).len() == 2).await;
        let after = generation_of(&remote, 1);
        assert_ne!(before, after);
        // The old mode ended on the remote just before it heard the stop.
        remote
            .to_home
            .try_send(failure_of(Some(before), "the picture of NAVETTE-1 failed"))
            .unwrap();
        made_for(&remote, 2, Some(after)).await;
        answer(second).await.unwrap();
        assert_eq!(*desk.covered.lock().unwrap(), ["DP-2"]);
        assert!(
            status_of(&desk)
                .await
                .starts_with("mode `travel` runs on DP-2")
        );
    }

    #[tokio::test]
    async fn the_failure_of_the_running_mode_still_ends_it() {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let first = ask_mode(&desk, "office").await;
        made_for(&remote, 1, None).await;
        answer(first).await.unwrap();
        let running = generation_of(&remote, 0);

        remote
            .to_home
            .try_send(failure_of(Some(running), "the picture of NAVETTE-1 failed"))
            .unwrap();
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        assert!(status_of(&desk).await.starts_with("no mode is running"));
    }

    #[tokio::test]
    async fn the_answers_of_a_mode_stopped_while_it_started_do_not_confirm_the_next_one() {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let first = ask_mode(&desk, "office").await;
        until(|| starts(&remote).len() == 1).await;
        let before = generation_of(&remote, 0);
        answer(ask(&desk, false).await).await.unwrap();
        answer(first).await.unwrap_err();

        let mut second = ask_mode(&desk, "travel").await;
        until(|| starts(&remote).len() == 2).await;
        // What the remote made for the first one, late: neither a confirmation nor an error.
        made_for(&remote, 2, Some(before)).await;
        remote
            .to_home
            .try_send(failure_of(Some(before), "NAVETTE-1: no encoder"))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            second.try_recv().is_err(),
            "the second start is still waiting"
        );
        assert!(
            status_of(&desk)
                .await
                .starts_with("mode `travel` is starting")
        );

        let current = generation_of(&remote, 1);
        made_for(&remote, 2, Some(current)).await;
        assert_eq!(
            answer(second).await.unwrap(),
            "DP-2 shows the remote machine's NAVETTE-1"
        );
    }

    #[tokio::test]
    async fn a_switch_whose_start_fails_says_the_first_mode_was_stopped_and_why_the_second_did_not_start()
     {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let first = ask_mode(&desk, "office").await;
        made(&remote, 1).await;
        answer(first).await.unwrap();
        desk.refuse.lock().unwrap().push("DP-2".into());

        let error = answer(ask_mode(&desk, "travel").await).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "mode `office` stopped, DP-1 is back home; \
             mode `travel` could not start: no layer shell"
        );
        // The first mode really is given back, and the second left nothing.
        assert!(desk.covered.lock().unwrap().is_empty());
        assert_eq!(mic_log(&desk), ["start", "stop"]);
        assert!(status_of(&desk).await.starts_with("no mode is running"));
        let stops = remote
            .outbox
            .queued()
            .into_iter()
            .filter(|m| matches!(m, Msg::Display(Display::Stop)))
            .count();
        assert_eq!(stops, 1);
        assert_eq!(
            starts(&remote).len(),
            1,
            "the remote was never asked for the second"
        );
    }

    #[tokio::test]
    async fn a_switch_whose_remote_cannot_make_its_output_says_so_too() {
        let desk = desk_with(two_modes());
        let remote = connect(&desk);
        let first = ask_mode(&desk, "office").await;
        made(&remote, 1).await;
        answer(first).await.unwrap();

        let second = ask_mode(&desk, "travel").await;
        until(|| starts(&remote).len() == 2).await;
        let generation = Some(generation_of(&remote, 1));
        remote
            .to_home
            .try_send(failure_of(generation, "NAVETTE-1: no encoder"))
            .unwrap();
        let error = answer(second).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "mode `office` stopped, DP-1 is back home; mode `travel` could not start: \
             the remote machine could not make its output: NAVETTE-1: no encoder"
        );
    }

    #[test]
    fn an_error_of_a_switch_drops_the_nothing_was_changed_of_the_new_mode_only() {
        let switched = Some("mode `a` stopped, DP-1 is back home");
        let error = io::Error::new(io::ErrorKind::NotFound, "no monitor X; nothing was changed");
        let said = switch_error(switched, "mode `b`", error);
        assert_eq!(said.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            said.to_string(),
            "mode `a` stopped, DP-1 is back home; mode `b` could not start: no monitor X"
        );
        // No switch: the error is the one it was.
        let error = io::Error::other("no monitor X; nothing was changed");
        assert_eq!(
            switch_error(None, "mode `b`", error).to_string(),
            "no monitor X; nothing was changed"
        );
    }

    #[test]
    fn no_two_outputs_decode_on_the_same_gpu() {
        let opened = std::cell::Cell::new(0);
        let open = || {
            opened.set(opened.get() + 1);
            Some(Arc::new(opened.get()))
        };
        let home = Arc::new(0);
        let gpus = decoding_gpus(Some(home.clone()), &names(3), open).unwrap();
        assert!(Arc::ptr_eq(gpus[0].as_ref().unwrap(), &home));
        let distinct: std::collections::HashSet<_> = gpus
            .iter()
            .map(|g| Arc::as_ptr(g.as_ref().unwrap()))
            .collect();
        assert_eq!(distinct.len(), 3);
        // On the CPU at home, no GPU is opened for the others.
        let gpus = decoding_gpus(None, &names(2), open).unwrap();
        assert_eq!(gpus, [None, None]);
        assert_eq!(opened.get(), 2);
    }

    #[test]
    fn a_mode_started_with_no_loss_keeps_the_gpu_it_has() {
        let gpu = HomeGpu::new(Some(Arc::new(0)));
        let first = gpu.current(|| panic!("no GPU is opened again")).unwrap();
        let again = gpu.current(|| panic!("no GPU is opened again")).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
    }

    #[test]
    fn a_mode_started_after_a_lost_gpu_decodes_on_a_fresh_one() {
        let gpu = HomeGpu::new(Some(Arc::new(0)));
        let lost_one = gpu.current(|| None).unwrap();
        gpu.lost().store(true, Ordering::Relaxed);
        let fresh = gpu.current(|| Some(Arc::new(1))).unwrap();
        assert!(!Arc::ptr_eq(&lost_one, &fresh));
        // It is kept, not opened again, for the mode after.
        let kept = gpu.current(|| panic!("no GPU is opened again")).unwrap();
        assert!(Arc::ptr_eq(&fresh, &kept));
    }

    #[test]
    fn a_lost_gpu_that_cannot_be_opened_again_is_tried_again_at_the_next_mode() {
        let gpu = HomeGpu::new(Some(Arc::new(0)));
        gpu.lost().store(true, Ordering::Relaxed);
        assert!(
            gpu.current(|| None).is_none(),
            "the mode decodes on the CPU"
        );
        let fresh = gpu.current(|| Some(Arc::new(1))).unwrap();
        assert_eq!(*fresh, 1);
    }

    fn names(count: usize) -> Vec<String> {
        (1..=count).map(|i| format!("DP-{i}")).collect()
    }

    #[test]
    fn a_gpu_home_that_cannot_open_a_device_for_an_output_says_which() {
        // Home's GPU offered the remote a stream the CPU does not decode.
        let opened = std::cell::Cell::new(0);
        let open = || {
            opened.set(opened.get() + 1);
            (opened.get() == 1).then_some(1)
        };
        let e = decoding_gpus(Some(0), &names(3), open).unwrap_err();
        assert_eq!(
            e.to_string(),
            "no GPU could be opened to decode DP-3 (home decodes on the GPU, each monitor on \
             a device of its own); nothing was changed"
        );
    }

    #[test]
    fn the_remote_has_as_long_again_for_each_more_output() {
        assert_eq!(start_timeout(1), START_TIMEOUT);
        assert_eq!(start_timeout(3), START_TIMEOUT * 3);
        // `navette mode` waits longer than serve does for the remote.
        assert!(ORDER_TIMEOUT > start_timeout(MAX_OUTPUTS.into()));
    }
}
