//! The mode, at home: the paired machine takes the home monitors named in the config, one or
//! more. `navette mode start` checks everything first (the config, the monitors, the link) and
//! changes nothing when a check fails; then home covers each monitor with its surface, asks the
//! remote for one output per monitor, in home's left-to-right order, and shows each stream on
//! its surface. `navette mode stop`, or the link dropping, gives the monitors back; so does any
//! one surface going away. Home monitors are never moved or reconfigured: the changes to the
//! home desk are a window rule per monitor that opens home windows on another home monitor
//! rather than hidden under a surface (#2) and, with a mic, the hotkey that switches it, made
//! through the desk recorder so they are undone on stop, on a dropped link and after a crash,
//! and made again after a config reload. A home window that lands on a given monitor all the
//! same (moved, dragged, opened on an explicit workspace) is sent back to a home monitor as
//! Hyprland reports it (#31). The home mic goes to the remote machine for as long as the mode runs.
//!
//! `navette serve` owns the link and the surface, so the two commands reach it over a Unix
//! socket in `$XDG_RUNTIME_DIR/navette`, private to the user.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gliff_vk::Gpu;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use crate::desk::{Change, Desk, DeskError};
use crate::display::decode::{self, Decoding, Picture, Video};
use crate::display::wire::{Display, Incoming, MAX_OUTPUTS, Msg, OutputCopy, Peer, Pointer};
use crate::display::{Ended, Released};
use crate::hypr::{self, Eval, Hypr, lua_string};
use crate::input::{self, Hotkey};

const CONFIG_FILE: &str = "mode.conf";
/// The keys that switch the mic, unless `mode.conf` names others (`mic_hotkey = ...`): free in
/// Omarchy's own bindings.
const MIC_HOTKEY: &str = "SUPER + CTRL + M";
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

/// The mode's config: which home monitors the remote machine takes, by output name, one or
/// more separated by commas, each once.
pub fn monitors(config_dir: &Path) -> io::Result<Vec<String>> {
    let path = config_dir.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no mode configured: write `monitor = <output name>` in {}",
                    path.display()
                ),
            ));
        }
        Err(e) => return Err(e),
    };
    let names: Vec<String> = setting(&text, "monitor")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} names no monitor: `monitor = <output name>`, or several: \
                 `monitor = DP-1, DP-2`",
                path.display()
            ),
        ));
    }
    if let Some(twice) = names
        .iter()
        .enumerate()
        .find_map(|(i, name)| names[..i].contains(name).then_some(name))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} names {twice} twice", path.display()),
        ));
    }
    if names.len() > usize::from(MAX_OUTPUTS) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} names {} monitors, {MAX_OUTPUTS} at most",
                path.display(),
                names.len()
            ),
        ));
    }
    Ok(names)
}

/// The value of `key = value` in the mode's config, where `#` starts a comment.
fn setting<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .map(|line| line.split_once('#').map_or(line, |(kept, _)| kept))
        .filter_map(|line| line.split_once('='))
        .find(|(name, _)| name.trim() == key)
        .map(|(_, value)| value.trim())
}

/// The release hotkey, from `release = <chord>` in the mode's config: `shift+escape` when it
/// says none, no hotkey for `release = none` or an empty `release =`.
pub fn release_hotkey(config_dir: &Path) -> io::Result<Option<Hotkey>> {
    let path = config_dir.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&path)?;
    let spec = setting(&text, "release").unwrap_or(input::DEFAULT_HOTKEY);
    Hotkey::parse(spec).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: `release = {spec}`: {e}", path.display()),
        )
    })
}

/// The keys that switch the mic: `mic_hotkey` in `mode.conf`, or [`MIC_HOTKEY`].
fn mic_hotkey(config_dir: &Path) -> String {
    let text = std::fs::read_to_string(config_dir.join(CONFIG_FILE)).unwrap_or_default();
    setting(&text, "mic_hotkey")
        .filter(|keys| !keys.is_empty())
        .unwrap_or(MIC_HOTKEY)
        .to_string()
}

/// The home mic, which goes to the remote machine while a mode runs (the port behind which
/// tests put a fake). Its calls wait for PipeWire and the link: they are made off the executor.
pub trait Mic: Send + Sync {
    /// The mic goes to the remote machine.
    fn start(&self) -> io::Result<()>;
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

/// Covers a home monitor (the port behind which tests put a fake).
pub trait Screens: Send + Sync {
    /// Covers `monitor`; the guard takes the cover away when dropped. Home's input over it goes
    /// to `input`, until `hotkey`, which `released` then hears. `ended` hears it if the cover
    /// goes away on its own.
    fn open(
        &self,
        monitor: &str,
        input: input::Queue,
        hotkey: (Option<Hotkey>, Released),
        ended: Ended,
    ) -> io::Result<Box<dyn Screen>>;
}

pub trait Screen: Send {
    fn pictures(&self) -> Box<dyn FnMut(Picture) + Send>;
    /// The remote pointer: its image, shown at home's pointer over the cover.
    fn cursor(&self, pointer: Pointer);
    /// The GPU's pictures can be handed over as dmabufs, with no copy.
    fn takes_dmabuf(&self) -> bool;
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
    ) -> io::Result<Box<dyn Screen>> {
        Ok(Box::new(crate::display::surface::Surface::open(
            monitor, input, hotkey, ended,
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

    fn takes_dmabuf(&self) -> bool {
        self.takes_linear_dmabuf()
    }
}

/// An order from `navette mode start|stop`, with where to answer.
pub enum Order {
    Start(oneshot::Sender<io::Result<String>>),
    Stop(oneshot::Sender<io::Result<String>>),
}

/// What home needs to run a mode.
pub struct Home {
    pub config_dir: PathBuf,
    pub hypr: Arc<dyn Hypr>,
    pub screens: Arc<dyn Screens>,
    /// The GPU that decodes, or none for the CPU.
    pub gpu: Option<Arc<Gpu>>,
    /// Where the desk recorder keeps what it must undo.
    pub desk_record: PathBuf,
    /// The mic, when home plays the remote's sound.
    pub mic: Option<Arc<dyn Mic>>,
}

/// The workspace each given monitor showed when home windows were last sent home.
type Seen = Vec<(String, i64)>;

struct Active {
    /// Home first, before the rest goes.
    _mic: Option<MicSent>,
    /// The monitors given, left to right: monitor `i` shows output `i`.
    monitors: Vec<String>,
    /// The home monitor that takes the home windows the cover would hide; none when every
    /// monitor is given.
    home_monitor: Option<String>,
    seen: Arc<Mutex<Seen>>,
    shown: Vec<Shown>,
    desk: Arc<Mutex<Desk<Eval>>>,
    /// Why a surface or a decoder cannot go on.
    ended: UnboundedReceiver<String>,
    /// Each time the release hotkey gave the keyboard back.
    released: UnboundedReceiver<()>,
    /// The `navette mode start` waiting for the remote's answer.
    starting: Option<(oneshot::Sender<io::Result<String>>, tokio::time::Instant)>,
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
    loop {
        let deadline = active
            .as_ref()
            .and_then(|a| a.starting.as_ref().map(|(_, at)| *at));
        tokio::select! {
            // A link that came up is known before the orders that follow it.
            biased;
            new = peers.recv() => match new {
                Some(new) => {
                    if active.is_some() {
                        stop(&mut active, "the link was replaced").await;
                    }
                    peer = Some(new);
                }
                None => break,
            },
            heard = next(&mut peer) => match heard {
                Heard::Gone => {
                    peer = None;
                    if active.is_some() {
                        stop(&mut active, "the link to the remote machine dropped").await;
                    }
                }
                Heard::Msg(Incoming::Display(Display::Started { output, others })) => {
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
                        let _ = reply.send(Ok(format!(
                            "{} {verb} the remote machine's {}",
                            a.label(),
                            outputs.join(", ")
                        )));
                    }
                }
                Heard::Msg(Incoming::Display(Display::Failed { reason })) => {
                    if let Some(mut a) = active.take() {
                        match a.starting.take() {
                            Some((reply, _)) => {
                                let _ = reply.send(Err(io::Error::other(format!(
                                    "the remote machine could not make its output: {reason}"
                                ))));
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
                Some(Order::Start(reply)) => {
                    match start(&home, peer.as_ref(), &mut active).await {
                        Ok(()) => {
                            if let Some(a) = &mut active {
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
                Some(Order::Stop(reply)) => {
                    let given = active.as_ref().map(|a| (a.label(), a.monitors.len()));
                    let answer = match given {
                        Some((monitors, count)) => {
                            if let Some(peer) = &peer {
                                peer.outbox.send(Msg::Display(Display::Stop), Vec::new());
                            }
                            stop(&mut active, "stopped").await;
                            let verb = if count > 1 { "are" } else { "is" };
                            Ok(format!("{monitors} {verb} back home"))
                        }
                        None => Ok("no mode was running".to_string()),
                    };
                    let _ = reply.send(answer);
                }
                None => break,
            },
            Some(event) = events.recv() => {
                if let Some(a) = &active {
                    let desk = a.desk.clone();
                    // A reload may put a workspace back where the user's rules want it.
                    let sweep = matches!(event, hypr::Event::Placed | hypr::Event::ConfigReloaded);
                    let sending = (a.monitors.clone(), a.home_monitor.clone(), a.seen.clone());
                    let handled =
                        blocking(move || desk.lock().unwrap().handle(&event).map_err(desk_error))
                            .await;
                    if let Err(e) = handled {
                        eprintln!("navette: the home desk could not be restored after a reload: {e}");
                    }
                    if sweep {
                        send_home(&home, sending).await;
                    }
                }
            }
            Some(said) = said(&mut active) => match said {
                Said::Released => {
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
                Said::Ended(why) => {
                    if let Some(peer) = &peer {
                        peer.outbox.send(Msg::Display(Display::Stop), Vec::new());
                    }
                    stop(&mut active, &why).await;
                }
            },
            _ = sleep_until(deadline) => {
                if let Some(peer) = &peer {
                    peer.outbox.send(Msg::Display(Display::Stop), Vec::new());
                }
                if let Some(mut a) = active.take() {
                    if let Some((reply, _)) = a.starting.take() {
                        let _ = reply.send(Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the remote machine did not answer in time",
                        )));
                    }
                    release(a).await;
                }
            }
        }
    }
    stop(&mut active, "navette is stopping").await;
}

/// What the running mode's surface and decoder say.
enum Said {
    /// The release hotkey gave the keyboard back.
    Released,
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
        Some(()) = a.released.recv() => Some(Said::Released),
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

/// Every check first, then the surfaces, then the order to the remote.
async fn start(home: &Home, peer: Option<&Peer>, active: &mut Option<Active>) -> io::Result<()> {
    if let Some(a) = active {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("a mode is already running on {}", a.label()),
        ));
    }
    let names = self::monitors(&home.config_dir)?;
    let hotkey = release_hotkey(&home.config_dir)?;
    let hypr = home.hypr.clone();
    let monitors = blocking(move || hypr::monitors(&*hypr)).await?;
    let mut given = Vec::new();
    for name in &names {
        let Some(found) = monitors.iter().find(|m| m.name == *name && !m.disabled) else {
            let there: Vec<_> = monitors.iter().map(|m| m.name.as_str()).collect();
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no monitor {name} at home (there are: {}); nothing was changed",
                    there.join(", ")
                ),
            ));
        };
        given.push(found.clone());
    }
    // The remote's outputs keep home's physical order, whatever the config's.
    given.sort_by_key(|m| (m.x, m.y));
    let Some(peer) = peer.filter(|p| !p.outbox.is_closed()) else {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "the remote machine is not connected (is `navette agent` running there?); \
             nothing was changed",
        ));
    };
    let mut copies = given.iter().map(|m| OutputCopy {
        width: m.width,
        height: m.height,
        refresh_mhz: (m.refresh_rate * 1000.0).round() as u32,
        scale_milli: (m.scale * 1000.0).round() as u32,
        transform: m.transform.clamp(0, 7) as u8,
    });
    let first = copies.next().expect("one monitor at least");
    let others: Vec<_> = copies.collect();
    let names: Vec<String> = given.into_iter().map(|m| m.name).collect();
    let home_monitor = monitors
        .iter()
        .filter(|m| !names.contains(&m.name) && !m.disabled)
        .min_by_key(|m| (m.x, m.y))
        .map(|m| m.name.clone());
    let desk = open_desk(home, &names, home_monitor.clone()).await?;
    // Off the executor, and so is the undo when one fails: the desk drops with the closure.
    let (ended_tx, ended) = tokio::sync::mpsc::unbounded_channel();
    let (released_tx, released) = tokio::sync::mpsc::unbounded_channel();
    let (screens, gpu, outbox, input, mic) = (
        home.screens.clone(),
        home.gpu.clone(),
        peer.outbox.clone(),
        peer.input.to_peer.clone(),
        home.mic.clone(),
    );
    let covered = names.clone();
    let (shown, desk, mic) = blocking(move || {
        let gpus = decoding_gpus(gpu, &covered, decode::open_gpu)?;
        let mut shown = Vec::new();
        for ((index, name), gpu) in covered.iter().enumerate().zip(gpus) {
            let output = index as u8;
            let screen = screens.open(
                name,
                input.on(output),
                (hotkey, released_tx.clone()),
                ended_tx.clone(),
            )?;
            let decoding = Decoding::start(
                gpu,
                screen.takes_dmabuf(),
                outbox.on(output),
                screen.pictures(),
                ended_tx.clone(),
            )?;
            shown.push(Shown { decoding, screen });
        }
        // Last, as nothing after it can fail: a mode without its mic is still a mode. Kept
        // when it failed, so that the stop clears what it says.
        let mic = mic.map(|mic| {
            if let Err(e) = mic.start() {
                eprintln!(
                    "navette: mic: it stays home, it could not go to the remote machine: {e}"
                );
            }
            MicSent(mic)
        });
        Ok((shown, desk, mic))
    })
    .await?;
    let caps = decode::caps(&home.gpu, (3840, 2160));
    peer.outbox.send(
        Msg::Display(Display::Start {
            output: first,
            caps,
            others: (!others.is_empty()).then_some(others),
        }),
        Vec::new(),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sending = (names.clone(), home_monitor.clone(), seen.clone());
    *active = Some(Active {
        _mic: mic,
        monitors: names,
        home_monitor,
        seen,
        shown,
        desk: Arc::new(Mutex::new(desk)),
        ended,
        released,
        starting: None,
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
/// monitors: those that would open on any of their workspaces go to `other`, the leftmost
/// home monitor kept home. With a mic, binds its hotkey.
async fn open_desk(home: &Home, given: &[String], other: Option<String>) -> io::Result<Desk<Eval>> {
    let (hypr, record, given) = (home.hypr.clone(), home.desk_record.clone(), given.to_vec());
    let mic_bind = home
        .mic
        .as_ref()
        .map(|mic| (mic_hotkey(&home.config_dir), mic.toggle_command()));
    blocking(move || {
        let mut desk = Desk::start(Eval(hypr.clone()), record).map_err(|e| match e {
            DeskError::Hypr(crate::hypr::HyprError::Io(e)) => io::Error::other(format!(
                "Hyprland could not be reached to undo what a previous run left ({e}); \
                 nothing was changed, try again"
            )),
            other => desk_error(other),
        })?;
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
            let _ = reply.send(Err(io::Error::other(format!("the mode ended: {why}"))));
        }
        eprintln!("navette: {} given back ({why})", a.label());
        release(a).await;
    }
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

/// Listens for `navette mode start|stop`, in a directory only this user can enter.
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
                "start" => Order::Start(reply),
                "stop" => Order::Stop(reply),
                _ => return,
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

/// `navette mode start|stop`: sends the order to `navette serve` and returns its answer.
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

    use super::*;
    use crate::display::wire::{Cursor, Outbox};
    use crate::hypr::fake::{FakeHypr, FakeMonitor, FakeWindow};

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
    }

    struct FakeScreen {
        monitor: String,
        covered: Arc<Mutex<Vec<String>>>,
        cursors: Arc<Mutex<Vec<(String, Pointer)>>>,
    }

    impl Screens for FakeScreens {
        fn open(
            &self,
            monitor: &str,
            input: input::Queue,
            (hotkey, released): (Option<Hotkey>, Released),
            ended: Ended,
        ) -> io::Result<Box<dyn Screen>> {
            self.inputs
                .lock()
                .unwrap()
                .push((monitor.to_string(), input));
            self.hotkeys.lock().unwrap().push(hotkey);
            self.releases.lock().unwrap().push(released);
            self.covered.lock().unwrap().push(monitor.to_string());
            self.ends.lock().unwrap().push(ended);
            Ok(Box::new(FakeScreen {
                monitor: monitor.to_string(),
                covered: self.covered.clone(),
                cursors: self.cursors.clone(),
            }))
        }
    }

    impl Screen for FakeScreen {
        fn pictures(&self) -> Box<dyn FnMut(Picture) + Send> {
            Box::new(|_| {})
        }

        fn takes_dmabuf(&self) -> bool {
            false
        }

        fn cursor(&self, pointer: Pointer) {
            self.cursors
                .lock()
                .unwrap()
                .push((self.monitor.clone(), pointer));
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
    }

    impl Mic for FakeMic {
        fn start(&self) -> io::Result<()> {
            self.log.lock().unwrap().push("start");
            if self.refuse {
                return Err(io::Error::other("no echo canceller"));
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

    struct Desk {
        _config: tempfile::TempDir,
        hypr: Arc<FakeHypr>,
        mic: Arc<FakeMic>,
        covered: Arc<Mutex<Vec<String>>>,
        ends: Arc<Mutex<Vec<Ended>>>,
        hotkeys: Arc<Mutex<Vec<Option<Hotkey>>>>,
        cursors: Arc<Mutex<Vec<(String, Pointer)>>>,
        inputs: Arc<Mutex<Vec<(String, input::Queue)>>>,
        releases: Arc<Mutex<Vec<Released>>>,
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
        let hypr = Arc::new(FakeHypr::with(monitors));
        let screens = FakeScreens::default();
        let covered = screens.covered.clone();
        let ends = screens.ends.clone();
        let hotkeys = screens.hotkeys.clone();
        let releases = screens.releases.clone();
        let cursors = screens.cursors.clone();
        let inputs = screens.inputs.clone();
        let (peers, peers_rx) = mpsc::unbounded_channel();
        let (orders, orders_rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        let home = Home {
            config_dir: config.path().to_path_buf(),
            hypr: hypr.clone(),
            screens: Arc::new(screens),
            gpu: None,
            desk_record: config.path().join("desk"),
            mic: Some(mic.clone()),
        };
        tokio::spawn(serve(home, peers_rx, orders_rx, events_rx));
        Desk {
            _config: config,
            hypr,
            mic,
            covered,
            ends,
            hotkeys,
            releases,
            cursors,
            inputs,
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
    }

    fn connect(desk: &Desk) -> Remote {
        let outbox = Outbox::default();
        let (to_home, inbox) = mpsc::channel(64);
        let (pointer, pointer_rx) = watch::channel(Vec::new());
        let input = input::Wire::default();
        desk.peers
            .send(Peer {
                outbox: outbox.clone(),
                inbox,
                pointer: pointer_rx,
                input: input.clone(),
            })
            .unwrap();
        Remote {
            outbox,
            to_home,
            pointer,
            input,
        }
    }

    async fn ask(desk: &Desk, start: bool) -> oneshot::Receiver<io::Result<String>> {
        let (reply, answer) = oneshot::channel();
        let order = if start {
            Order::Start(reply)
        } else {
            Order::Stop(reply)
        };
        desk.orders.send(order).unwrap();
        answer
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
                output: "NAVETTE-1".into(),
                others: None,
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
        released.send(()).unwrap();
        // Focused with the pointer left where it is, over the covered monitor.
        until(|| desk.hypr.focused() == [("0xb2".to_string(), false)]).await;
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
        released.send(()).unwrap();
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
                output: "NAVETTE-1".into(),
                others: None,
            }))
            .unwrap();
        answer(pending).await.unwrap();
        remote
    }

    fn mic_log(desk: &Desk) -> Vec<&'static str> {
        desk.mic.log.lock().unwrap().clone()
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
                output: "NAVETTE-1".into(),
                others: None,
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
                output: "NAVETTE-1".into(),
                others: Some(vec!["NAVETTE-2".into()]),
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
        assert_eq!(monitors(config.path()).unwrap(), ["DP-2"]);
        assert_eq!(
            release_hotkey(config.path()).unwrap(),
            Hotkey::parse("ctrl+alt+q").unwrap()
        );
    }

    #[test]
    fn an_empty_release_hotkey_means_none() {
        let config = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join(CONFIG_FILE),
            "monitor = DP-2\nrelease =\n",
        )
        .unwrap();
        assert_eq!(release_hotkey(config.path()).unwrap(), None);
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
                output: "NAVETTE-1".into(),
                others: None,
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
                output: "NAVETTE-1".into(),
                others: None,
            }))
            .unwrap();
        answer(pending).await.unwrap();
        drop(remote);
        until(|| desk.covered.lock().unwrap().is_empty()).await;
        until(|| desk.hypr.window_rules().len() == 2).await;
        assert!(desk.hypr.window_rules()[1].contains("enabled = false"));
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
                    Order::Start(reply) => {
                        let _ = reply.send(Err(io::Error::other("no monitor X")));
                    }
                    Order::Stop(reply) => {
                        let _ = reply.send(Ok("DP-1 is back home".into()));
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
        assert_eq!(error.to_string(), "no monitor X");
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
                output: outputs[0].into(),
                others: (!others.is_empty()).then_some(others),
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
            });
        }
        assert_eq!(
            remote.input.to_peer.queued_on(),
            [
                (0, input::Event::Motion { x: 1919.0, y: 5.0 }),
                (1, input::Event::Motion { x: 0.0, y: 5.0 }),
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

    #[test]
    fn several_monitors_are_read_from_the_config_each_once() {
        let config = tempfile::tempdir().unwrap();
        let path = config.path().join(CONFIG_FILE);
        std::fs::write(&path, "monitor = DP-3 , DP-2,   # the two right ones\n").unwrap();
        assert_eq!(monitors(config.path()).unwrap(), ["DP-3", "DP-2"]);
        std::fs::write(&path, "monitor = DP-3, DP-2, DP-3\n").unwrap();
        let error = monitors(config.path()).unwrap_err().to_string();
        assert!(error.contains("DP-3 twice"), "{error}");
        std::fs::write(&path, "monitor = ,\n").unwrap();
        assert!(monitors(config.path()).is_err());
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
