//! Input: home's keyboard and pointer, while the pointer is over the reserved monitor, sent to
//! the remote machine and injected there on its output. Focus follows the pointer: the surface
//! takes the keyboard (and the shortcuts inhibitor, so Super and every bind go too) when the
//! pointer enters it, and gives both back when it leaves, or when the release hotkey is typed.
//!
//! Positions are the surface's own (home's logical pixels) and the remote pointer's extent is
//! the surface's size: the virtual pointer's absolute motion is a fraction of its output, so
//! the position lands at the same place whatever scale Hyprland chose on either side.
//!
//! **The queue.** Each direction has one bounded queue, which never blocks its writer (the
//! surface's thread at home, the link's reader on the remote). Its policy keeps a held key from
//! ever staying down: a motion replaces a motion queued right before it; and when the queue is
//! full (the link stalled for [`QUEUE`] events), what waits is dropped and replaced by a
//! `ReleaseAll`, keeping only the latest keymap and area. A release is so never lost: it is
//! queued, or everything held is released. After such a cut, a key still held at home is up on
//! the remote until it is pressed again. A keymap replaces one still queued, so the queue holds
//! one at most, and its memory stays bounded.
//!
//! **Several outputs.** A mode of several monitors has one surface, and one remote output, per
//! monitor: each event names its output, and the remote injects it there. The queue's rules
//! hold per output: a motion replaces only its own output's, a cut keeps each output's area
//! and releases everything on each output it dropped events of.
//!
//! The release hotkey and the held-key bookkeeping are adapted from gliff's client
//! (`crates/gliff/src/main.rs`; gliff, MIT, see `LICENSES/gliff-MIT.txt`).

use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};

use gliff_proto::Axis;
use minicbor::{Decode, Encode};
use tokio::sync::Notify;
use xkbcommon::xkb;

/// Events waiting in one direction before the queue is cut to a `ReleaseAll`: several seconds
/// of typing and scrolling, far more than a link that keeps up ever holds.
pub const QUEUE: usize = 256;

/// The longest keymap taken from home: a full xkb keymap is about 100 KB.
pub const MAX_KEYMAP: usize = 512 * 1024;

/// One input event, home to remote.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Event {
    /// `code` is an evdev keycode.
    #[n(0)]
    Key {
        #[n(0)]
        code: u32,
        #[n(1)]
        pressed: bool,
    },
    /// In the surface's logical pixels.
    #[n(1)]
    Motion {
        #[n(0)]
        x: f64,
        #[n(1)]
        y: f64,
    },
    /// `button` is an evdev `BTN_*` code.
    #[n(2)]
    Button {
        #[n(0)]
        button: u32,
        #[n(1)]
        pressed: bool,
    },
    #[n(3)]
    Axis {
        #[n(0)]
        axis: Axis,
        #[n(1)]
        value: f64,
        #[n(2)]
        discrete: Option<i32>,
        #[n(3)]
        stop: bool,
    },
    /// Home's keymap (xkb text, format v1), so every key types the same on both machines.
    #[n(4)]
    Keymap(#[n(0)] String),
    /// The surface's size in logical pixels: the extent of the positions.
    #[n(5)]
    Area {
        #[n(0)]
        width: u32,
        #[n(1)]
        height: u32,
    },
    /// Every key and button held goes up; the locks stay.
    #[n(6)]
    ReleaseAll,
    /// The locks on at home, by xkb name (`Lock` for Caps Lock, `Mod2` for Num Lock).
    #[n(7)]
    Locked(#[n(0)] Vec<String>),
}

#[derive(Default)]
struct Waiting {
    /// With the output each is for.
    events: VecDeque<(u8, Event)>,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    waiting: Mutex<Waiting>,
    notify: Notify,
    /// The keys home's surfaces sent down, on any output, and home still holds as far as they
    /// know: the next surface to take the keyboard presses them again on its own output.
    carried: Mutex<BTreeSet<u32>>,
}

/// One direction's queue (see the module's doc for its policy). What it is given is for output
/// 0; [`Queue::on`] gives the same queue for another output.
#[derive(Clone, Default)]
pub struct Queue {
    shared: Arc<Shared>,
    output: u8,
}

impl Queue {
    /// The same queue, its events for `output`.
    pub fn on(&self, output: u8) -> Self {
        Self {
            shared: self.shared.clone(),
            output,
        }
    }

    /// Queues an event; never waits. Dropped once the queue is closed.
    pub fn push(&self, event: Event) {
        let output = self.output;
        let mut waiting = self.shared.waiting.lock().unwrap();
        if waiting.closed {
            return;
        }
        let events = &mut waiting.events;
        let follows_a_motion = matches!(event, Event::Motion { .. })
            && matches!(events.back(), Some((o, Event::Motion { .. })) if *o == output);
        let full = events.len() >= QUEUE;
        match events.back_mut() {
            Some((_, last)) if follows_a_motion => *last = event,
            _ if full => {
                // A keymap coming now replaces the one queued.
                let keymap = events
                    .iter()
                    .rfind(|(_, e)| matches!(e, Event::Keymap(_)))
                    .filter(|_| !matches!(event, Event::Keymap(_)))
                    .cloned();
                let dropped: BTreeSet<u8> = events.iter().map(|(o, _)| *o).collect();
                let areas: Vec<_> = dropped
                    .iter()
                    .filter_map(|&o| {
                        events
                            .iter()
                            .rfind(|(e_o, e)| *e_o == o && matches!(e, Event::Area { .. }))
                            .cloned()
                    })
                    .collect();
                events.clear();
                events.extend(keymap);
                events.extend(areas);
                events.extend(dropped.into_iter().map(|o| (o, Event::ReleaseAll)));
                events.push_back((output, event));
            }
            _ => {
                // Only the newest keymap matters: the queue holds one at most.
                if matches!(event, Event::Keymap(_)) {
                    events.retain(|(_, e)| !matches!(e, Event::Keymap(_)));
                }
                events.push_back((output, event));
            }
        }
        drop(waiting);
        self.shared.notify.notify_one();
    }

    /// The next event, oldest first, with its output; `None` once closed.
    pub async fn next(&self) -> Option<(u8, Event)> {
        loop {
            {
                let mut waiting = self.shared.waiting.lock().unwrap();
                if let Some(event) = waiting.events.pop_front() {
                    return Some(event);
                }
                if waiting.closed {
                    return None;
                }
            }
            self.shared.notify.notified().await;
        }
    }

    /// The link is down: nothing more is queued, and `next` ends.
    pub fn close(&self) {
        let mut waiting = self.shared.waiting.lock().unwrap();
        waiting.closed = true;
        waiting.events.clear();
        drop(waiting);
        self.shared.notify.notify_one();
    }

    /// What waits, whatever its output.
    #[cfg(test)]
    pub fn queued(&self) -> Vec<Event> {
        self.queued_on().into_iter().map(|(_, e)| e).collect()
    }

    #[cfg(test)]
    pub fn queued_on(&self) -> Vec<(u8, Event)> {
        let waiting = self.shared.waiting.lock().unwrap();
        waiting.events.iter().cloned().collect()
    }
}

/// A link's input queues: home fills `to_peer`, the remote reads `from_peer`.
#[derive(Clone, Default)]
pub struct Wire {
    pub to_peer: Queue,
    pub from_peer: Queue,
}

impl Wire {
    pub fn close(&self) {
        self.to_peer.close();
        self.from_peer.close();
    }
}

/// The modifiers a hotkey names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Mods {
    shift: bool,
    ctrl: bool,
    alt: bool,
    logo: bool,
}

/// The chord that gives the keyboard back to home without moving the pointer, as gliff's
/// `--release-hotkey`: `shift+escape` by default, `none` for no hotkey.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    mods: Mods,
    sym: xkb::Keysym,
}

pub const DEFAULT_HOTKEY: &str = "shift+escape";

impl Hotkey {
    /// `shift+escape`, `ctrl+alt+q`, `super+f12`…: modifiers and one key. `none`, or nothing
    /// (`release =` left empty), for no hotkey.
    pub fn parse(spec: &str) -> Result<Option<Self>, String> {
        let spec = spec.trim();
        if spec.is_empty() || spec.eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        let mut mods = Mods::default();
        let mut sym = None;
        for part in spec.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "" => {}
                "shift" => mods.shift = true,
                "ctrl" | "control" => mods.ctrl = true,
                "alt" => mods.alt = true,
                "super" | "logo" | "win" | "meta" => mods.logo = true,
                name => {
                    if sym.is_some() {
                        return Err(format!("two keys in the release hotkey '{spec}'"));
                    }
                    sym = Some(keysym(name).ok_or_else(|| format!("unknown key '{name}'"))?);
                }
            }
        }
        match sym {
            Some(sym) => Ok(Some(Self { mods, sym })),
            None => Err(format!("no key in the release hotkey '{spec}'")),
        }
    }
}

fn keysym(name: &str) -> Option<xkb::Keysym> {
    let name = match name {
        "esc" => "Escape",
        "enter" | "return" => "Return",
        other => other,
    };
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
    (sym.raw() != xkb::keysyms::KEY_NoSymbol).then_some(sym)
}

/// Home's keyboard state, from its keymap: what the hotkey is matched against.
struct Keys {
    keymap: xkb::Keymap,
    state: xkb::State,
    /// The keys held: a modifier key counts as typed even when xkb makes something else of it
    /// (with `shift:both_capslock_cancel`, Shift while Caps Lock is on is a Caps Lock key).
    held: BTreeSet<u32>,
}

impl Keys {
    fn new(text: &str) -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            text.to_owned(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;
        let state = xkb::State::new(&keymap);
        Some(Self {
            keymap,
            state,
            held: BTreeSet::new(),
        })
    }

    fn update(&mut self, code: u32, pressed: bool) {
        let direction = if pressed {
            xkb::KeyDirection::Down
        } else {
            xkb::KeyDirection::Up
        };
        self.state.update_key(keycode(code), direction);
        track(&mut self.held, code, pressed);
    }

    fn reset(&mut self) {
        self.state = xkb::State::new(&self.keymap);
        self.held.clear();
    }

    /// Home's modifiers as the compositor reports them; `group` is the layout in effect.
    fn set_mods(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.state
            .update_mask(depressed, latched, locked, 0, 0, group);
    }

    fn locks(&self) -> Vec<String> {
        [xkb::MOD_NAME_CAPS, xkb::MOD_NAME_NUM]
            .into_iter()
            .filter(|name| self.state.mod_name_is_active(name, xkb::STATE_MODS_LOCKED))
            .map(str::to_string)
            .collect()
    }

    /// What the press of `code` with the modifiers held now is to the hotkey. The key is read
    /// on its first level, so `shift+q` is the Q key with Shift, whatever Shift makes of it.
    fn chord(&self, hotkey: &Hotkey, code: u32) -> Chord {
        if self.first_sym(code) != Some(hotkey.sym) {
            return Chord::OtherKey;
        }
        let active = |name| {
            self.state
                .mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE)
        };
        let mut mods = Mods {
            shift: active(xkb::MOD_NAME_SHIFT),
            ctrl: active(xkb::MOD_NAME_CTRL),
            alt: active(xkb::MOD_NAME_ALT),
            logo: active(xkb::MOD_NAME_LOGO),
        };
        for sym in self.held.iter().filter_map(|&held| self.first_sym(held)) {
            use xkb::keysyms as k;
            match sym.raw() {
                k::KEY_Shift_L | k::KEY_Shift_R => mods.shift = true,
                k::KEY_Control_L | k::KEY_Control_R => mods.ctrl = true,
                k::KEY_Alt_L | k::KEY_Alt_R | k::KEY_Meta_L | k::KEY_Meta_R => mods.alt = true,
                k::KEY_Super_L | k::KEY_Super_R => mods.logo = true,
                _ => {}
            }
        }
        if mods == hotkey.mods {
            return Chord::Hotkey;
        }
        let held = (0..self.keymap.num_mods())
            .filter(|&index| {
                self.state
                    .mod_index_is_active(index, xkb::STATE_MODS_EFFECTIVE)
            })
            .map(|index| self.keymap.mod_get_name(index).to_string())
            .collect();
        Chord::OtherMods(held)
    }

    fn first_sym(&self, code: u32) -> Option<xkb::Keysym> {
        let layout = self.state.key_get_layout(keycode(code));
        let syms = self.keymap.key_get_syms_by_level(keycode(code), layout, 0);
        syms.first().copied()
    }
}

/// A key press, as the release hotkey sees it.
#[derive(Debug, PartialEq, Eq)]
enum Chord {
    Hotkey,
    /// The hotkey's key, with these modifiers (xkb names) instead of the hotkey's.
    OtherMods(Vec<String>),
    /// Any other key, never named: what is typed over the monitor may be a password.
    OtherKey,
}

fn keycode(evdev: u32) -> xkb::Keycode {
    xkb::Keycode::new(evdev.saturating_add(8))
}

/// Whether the surface should hold the keyboard (and the shortcuts inhibitor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Take,
    Give,
}

/// Home's side over the reserved surface, apart from Wayland: what each event of the pointer
/// and the keyboard sends, and when the surface takes or gives the keyboard.
pub struct Capture {
    queue: Queue,
    hotkey: Option<Hotkey>,
    keys: Option<Keys>,
    /// The keymap last sent: the compositor may send the same one again on each focus.
    keymap: String,
    held_keys: BTreeSet<u32>,
    held_buttons: BTreeSet<u32>,
    /// The hotkey gave the keyboard back; until the pointer leaves, keys stay home.
    released: bool,
    /// The locks last sent since the keyboard came; `None` sends them at the next modifiers.
    locks_sent: Option<Vec<String>>,
    /// Hears each time the hotkey gives the keyboard back.
    on_release: Option<crate::display::Released>,
}

impl Capture {
    pub fn new(queue: Queue, hotkey: Option<Hotkey>) -> Self {
        Self {
            queue,
            hotkey,
            keys: None,
            keymap: String::new(),
            held_keys: BTreeSet::new(),
            held_buttons: BTreeSet::new(),
            released: false,
            locks_sent: None,
            on_release: None,
        }
    }

    /// `released` hears each time the hotkey gives the keyboard back.
    pub fn on_release(mut self, released: crate::display::Released) -> Self {
        self.on_release = Some(released);
        self
    }

    pub fn keymap(&mut self, text: String) {
        if text == self.keymap {
            return;
        }
        self.keys = Keys::new(&text);
        if self.keys.is_none() && self.hotkey.is_some() {
            eprintln!("navette: home's keymap does not compile here; the release hotkey is off");
        }
        self.keymap.clone_from(&text);
        if text.len() <= MAX_KEYMAP {
            self.queue.push(Event::Keymap(text));
        }
    }

    pub fn area(&mut self, width: u32, height: u32) {
        self.queue.push(Event::Area { width, height });
    }

    /// The pointer came over the surface: the keyboard follows it.
    pub fn enter(&mut self, x: f64, y: f64) -> Focus {
        self.released = false;
        self.queue.push(Event::Motion { x, y });
        Focus::Take
    }

    /// The pointer left: whatever is held goes up on the remote, the keyboard goes home.
    pub fn leave(&mut self) -> Focus {
        self.release();
        self.released = false;
        Focus::Give
    }

    /// The keyboard came, with the keys already held at home: they count for the hotkey. Those
    /// that went down on the remote over this surface or another of the link (the pointer
    /// crossing from one work monitor to the next with Shift held) go down again on this
    /// output, a virtual keyboard's modifiers being its own; the others' releases stay home,
    /// the remote never having had their presses.
    pub fn keyboard_entered(&mut self, keys: &[u32]) {
        if let Some(state) = &mut self.keys {
            for &code in keys {
                state.update(code, true);
            }
        }
        let mut carried = self.queue.shared.carried.lock().unwrap();
        carried.retain(|code| keys.contains(code));
        for &code in carried.iter() {
            if self.held_keys.insert(code) {
                self.queue.push(Event::Key {
                    code,
                    pressed: true,
                });
            }
        }
        drop(carried);
        // The remote may have lost them since (its injector started again): sent again.
        self.locks_sent = None;
    }

    /// Home's modifiers, which the compositor sends when the keyboard comes and when they
    /// change: the hotkey reads them, and the remote takes home's locks.
    pub fn modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        let Some(keys) = &mut self.keys else { return };
        keys.set_mods(depressed, latched, locked, group);
        let locks = keys.locks();
        if self.locks_sent.as_ref() != Some(&locks) {
            self.locks_sent = Some(locks.clone());
            self.queue.push(Event::Locked(locks));
        }
    }

    /// The keyboard went elsewhere while the pointer stayed: the keys held go up there. When
    /// the pointer left first, it released them already.
    pub fn keyboard_left(&mut self) {
        if !self.held_keys.is_empty() || !self.held_buttons.is_empty() {
            self.release();
        }
    }

    pub fn motion(&mut self, x: f64, y: f64) {
        self.queue.push(Event::Motion { x, y });
    }

    pub fn button(&mut self, button: u32, pressed: bool) {
        if track(&mut self.held_buttons, button, pressed) {
            self.queue.push(Event::Button { button, pressed });
        }
    }

    pub fn axis(&mut self, axis: Axis, value: f64, discrete: Option<i32>, stop: bool) {
        self.queue.push(Event::Axis {
            axis,
            value,
            discrete,
            stop,
        });
    }

    /// A key; `Some(Focus::Give)` when it was the release hotkey, which the remote never sees.
    pub fn key(&mut self, code: u32, pressed: bool) -> Option<Focus> {
        if self.released {
            return None;
        }
        if pressed && let Some(hotkey) = &self.hotkey {
            match self.keys.as_ref().map(|keys| keys.chord(hotkey, code)) {
                Some(Chord::Hotkey) => {
                    tracing::info!("the release hotkey: the keyboard goes home");
                    self.release();
                    self.released = true;
                    if let Some(on_release) = &self.on_release {
                        let _ = on_release.send(());
                    }
                    return Some(Focus::Give);
                }
                Some(Chord::OtherMods(held)) => tracing::debug!(
                    ?held,
                    "the release hotkey's key, not with the hotkey's modifiers"
                ),
                Some(Chord::OtherKey) => {}
                None => tracing::debug!("no keymap from home: the release hotkey cannot be read"),
            }
        }
        if let Some(keys) = &mut self.keys {
            keys.update(code, pressed);
        }
        // A release whose press the remote never got (held before the pointer came) stays home.
        if track(&mut self.held_keys, code, pressed) {
            self.queue.push(Event::Key { code, pressed });
        }
        let mut carried = self.queue.shared.carried.lock().unwrap();
        if !pressed {
            carried.remove(&code);
        } else if self.held_keys.contains(&code) {
            carried.insert(code);
        }
        None
    }

    fn release(&mut self) {
        self.held_keys.clear();
        self.held_buttons.clear();
        if let Some(keys) = &mut self.keys {
            keys.reset();
        }
        self.queue.push(Event::ReleaseAll);
    }
}

impl Drop for Capture {
    /// The surface goes (the mode stops): nothing stays held on the remote.
    fn drop(&mut self) {
        self.release();
    }
}

/// What the seat tells the surface, as far as input goes.
pub enum Seat<'a> {
    Keymap(String),
    PointerEnter {
        x: f64,
        y: f64,
    },
    PointerLeave,
    /// With the keys already held at home.
    KeyboardEnter {
        keys: &'a [u32],
    },
    /// `ours` is false when the keyboard left another of this client's surfaces.
    KeyboardLeave {
        ours: bool,
    },
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
    Key {
        code: u32,
        pressed: bool,
    },
}

/// The surface's hold on home's keyboard: its keyboard interactivity (`on_demand` or `none`)
/// and the shortcuts inhibitor.
pub trait Grab {
    fn interactive(&mut self, on: bool);
    /// Creates the inhibitor, unless one lives.
    fn inhibit(&mut self);
    /// Destroys the inhibitor, if one lives.
    fn uninhibit(&mut self);
    fn commit(&mut self);
}

/// The focus lifecycle: what the seat says goes to `capture`, and the keyboard and the
/// shortcuts are taken or given back as it decides.
pub fn handle(capture: &mut Capture, grab: &mut dyn Grab, event: Seat) {
    let focus = match event {
        Seat::Keymap(text) => {
            capture.keymap(text);
            None
        }
        Seat::PointerEnter { x, y } => Some(capture.enter(x, y)),
        Seat::PointerLeave => Some(capture.leave()),
        Seat::KeyboardEnter { keys } => {
            capture.keyboard_entered(keys);
            None
        }
        Seat::KeyboardLeave { ours } => {
            if ours {
                capture.keyboard_left();
            }
            None
        }
        Seat::Modifiers {
            depressed,
            latched,
            locked,
            group,
        } => {
            capture.modifiers(depressed, latched, locked, group);
            None
        }
        Seat::Key { code, pressed } => capture.key(code, pressed),
    };
    match focus {
        Some(Focus::Take) => {
            grab.interactive(true);
            grab.inhibit();
        }
        Some(Focus::Give) => {
            grab.interactive(false);
            grab.uninhibit();
        }
        None => return,
    }
    grab.commit();
}

/// Records a press or a release; false for a repeat or a release of nothing held.
fn track(held: &mut BTreeSet<u32>, code: u32, pressed: bool) -> bool {
    if pressed {
        held.insert(code)
    } else {
        held.remove(&code)
    }
}

/// Injects input on the remote's output (the port behind which tests put a fake).
pub trait Injectors: Send + Sync {
    /// Starts injecting on `output`, with home's keymap when it is known.
    fn start(&self, output: &str, keymap: Option<&str>) -> io::Result<Box<dyn Inject>>;
}

/// A running injector; dropping it releases everything it holds.
pub trait Inject: Send + Sync {
    fn send(&self, event: &Event);
}

/// The real injector: gliff's virtual keyboard and pointer, on the running Hyprland.
pub struct LiveInjectors;

impl Injectors for LiveInjectors {
    fn start(&self, output: &str, keymap: Option<&str>) -> io::Result<Box<dyn Inject>> {
        let display = crate::hypr::instance()?
            .wayland_display()
            .map_err(|e| io::Error::other(format!("input: {e}")))?;
        let start = |keymap: Option<&str>| {
            let mut config = hypr_input::InputConfig::new(output);
            config.target.display = Some(display.clone());
            config.keymap = keymap.map(str::to_string);
            hypr_input::Input::start(
                config,
                Box::new(|event| {
                    if let hypr_input::InputEvent::Error(e) = event {
                        tracing::warn!("input: {e}");
                    }
                }),
            )
        };
        // A keymap this machine cannot compile still leaves the default one.
        let input = match start(keymap) {
            Err(hypr_input::Error::Xkb(e)) if keymap.is_some() => {
                eprintln!("navette: home's keymap was refused ({e}); typing with the default one");
                start(None)
            }
            other => other,
        }
        .map_err(|e| io::Error::other(format!("input: {e}")))?;
        Ok(Box::new(input))
    }
}

impl Inject for hypr_input::Input {
    fn send(&self, event: &Event) {
        use hypr_input::InputCmd;
        let cmd = match event.clone() {
            Event::Key { code, pressed } => InputCmd::Key { code, pressed },
            Event::Motion { x, y } => InputCmd::Motion { x, y },
            Event::Button { button, pressed } => InputCmd::Button { button, pressed },
            Event::Axis {
                axis,
                value,
                discrete,
                stop,
            } => InputCmd::Axis {
                axis: match axis {
                    Axis::Vertical => hypr_input::Axis::Vertical,
                    Axis::Horizontal => hypr_input::Axis::Horizontal,
                },
                value,
                discrete,
                stop,
            },
            Event::Keymap(text) => InputCmd::SetKeymap(text),
            Event::Area { width, height } => InputCmd::SetExtent { width, height },
            Event::ReleaseAll => InputCmd::ReleaseAll,
            Event::Locked(names) => InputCmd::SetLocked(names),
        };
        let _ = hypr_input::Input::send(self, cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_LEFTSHIFT: u32 = 42;
    const KEY_LEFTMETA: u32 = 125;
    const KEY_ESC: u32 = 1;
    const KEY_T: u32 = 20;
    const BTN_LEFT: u32 = 0x110;

    fn us() -> String {
        hypr_input::keymap_from_names(&hypr_input::KeymapNames {
            layout: "us".into(),
            ..Default::default()
        })
        .unwrap()
    }

    fn armed(hotkey: &str) -> (Capture, Queue) {
        let queue = Queue::default();
        let mut capture = Capture::new(queue.clone(), Hotkey::parse(hotkey).unwrap());
        capture.keymap(us());
        capture.enter(10.0, 10.0);
        let _ = queue.queued();
        (capture, queue)
    }

    fn sent(queue: &Queue) -> Vec<Event> {
        let events = queue.queued();
        while queue
            .shared
            .waiting
            .lock()
            .unwrap()
            .events
            .pop_front()
            .is_some()
        {}
        events
            .into_iter()
            .filter(|e| !matches!(e, Event::Keymap(_) | Event::Motion { .. }))
            .collect()
    }

    fn key(code: u32, pressed: bool) -> Event {
        Event::Key { code, pressed }
    }

    #[test]
    fn super_and_a_key_over_the_reserved_surface_go_to_the_remote() {
        let (mut capture, queue) = armed(DEFAULT_HOTKEY);
        for (code, pressed) in [(KEY_LEFTMETA, true), (KEY_T, true), (KEY_T, false)] {
            assert_eq!(capture.key(code, pressed), None);
        }
        capture.key(KEY_LEFTMETA, false);
        assert_eq!(
            sent(&queue),
            [
                key(KEY_LEFTMETA, true),
                key(KEY_T, true),
                key(KEY_T, false),
                key(KEY_LEFTMETA, false)
            ]
        );
    }

    #[test]
    fn leaving_the_surface_with_a_key_held_never_leaves_it_stuck_down() {
        let (mut capture, queue) = armed(DEFAULT_HOTKEY);
        capture.key(KEY_LEFTSHIFT, true);
        capture.button(BTN_LEFT, true);
        assert_eq!(capture.leave(), Focus::Give);
        assert_eq!(
            sent(&queue),
            [
                key(KEY_LEFTSHIFT, true),
                Event::Button {
                    button: BTN_LEFT,
                    pressed: true
                },
                Event::ReleaseAll
            ]
        );
        // Its release, typed at home, is not sent again.
        capture.key(KEY_LEFTSHIFT, false);
        assert_eq!(sent(&queue), []);
    }

    #[test]
    fn the_keyboard_leaving_releases_the_keys_held() {
        let (mut capture, queue) = armed(DEFAULT_HOTKEY);
        capture.key(KEY_T, true);
        capture.keyboard_left();
        assert_eq!(sent(&queue), [key(KEY_T, true), Event::ReleaseAll]);
    }

    #[test]
    fn the_release_hotkey_gives_the_keyboard_back_until_the_pointer_leaves() {
        let (mut capture, queue) = armed(DEFAULT_HOTKEY);
        capture.key(KEY_LEFTSHIFT, true);
        assert_eq!(capture.key(KEY_ESC, true), Some(Focus::Give));
        // Escape never reaches the remote; Shift, sent before, is released there.
        assert_eq!(sent(&queue), [key(KEY_LEFTSHIFT, true), Event::ReleaseAll]);
        assert_eq!(capture.key(KEY_T, true), None);
        assert_eq!(sent(&queue), []);
        capture.leave();
        assert_eq!(capture.enter(1.0, 1.0), Focus::Take);
        capture.key(KEY_T, true);
        assert_eq!(sent(&queue), [Event::ReleaseAll, key(KEY_T, true)]);
    }

    // The us keymap's modifier masks.
    const SHIFT: u32 = 1 << 0;
    const CAPS: u32 = 1 << 1;
    const NUM: u32 = 1 << 4;

    fn locked(names: &[&str]) -> Event {
        Event::Locked(names.iter().map(|name| name.to_string()).collect())
    }

    #[test]
    fn caps_lock_on_at_home_is_on_on_the_remote_when_the_keyboard_comes() {
        let (mut capture, queue) = armed(DEFAULT_HOTKEY);
        capture.keyboard_entered(&[]);
        capture.modifiers(0, 0, CAPS | NUM, 0);
        assert_eq!(sent(&queue), [locked(&["Lock", "Mod2"])]);
        // The same locks again are not news; Caps Lock off is.
        capture.modifiers(0, 0, CAPS | NUM, 0);
        assert_eq!(sent(&queue), []);
        capture.modifiers(0, 0, NUM, 0);
        assert_eq!(sent(&queue), [locked(&["Mod2"])]);
        // The keyboard comes again: the remote may have lost them, they are sent again.
        capture.keyboard_left();
        capture.keyboard_entered(&[]);
        capture.modifiers(0, 0, NUM, 0);
        assert_eq!(sent(&queue), [locked(&["Mod2"])]);
    }

    #[test]
    fn shift_held_before_the_pointer_came_then_escape_gives_the_keyboard_back() {
        let (mut capture, queue) = armed(DEFAULT_HOTKEY);
        capture.keyboard_entered(&[KEY_LEFTSHIFT]);
        capture.modifiers(SHIFT, 0, 0, 0);
        assert_eq!(capture.key(KEY_ESC, true), Some(Focus::Give));
        assert_eq!(sent(&queue), [locked(&[]), Event::ReleaseAll]);
    }

    const KEY_RIGHTSHIFT: u32 = 54;
    const KEY_LEFTCTRL: u32 = 29;

    /// Home's keymap on the maintainer's desk: French, with Omarchy's keyboard options.
    fn omarchy_fr() -> String {
        hypr_input::keymap_from_names(&hypr_input::KeymapNames {
            layout: "fr".into(),
            options: Some("compose:caps,shift:both_capslock_cancel".into()),
            ..Default::default()
        })
        .unwrap()
    }

    /// Over the surface with home's keymap on the maintainer's desk, and `locked` at home.
    fn armed_on_the_desk(locked: u32) -> (Capture, Queue) {
        let queue = Queue::default();
        let mut capture = Capture::new(queue.clone(), Hotkey::parse(DEFAULT_HOTKEY).unwrap());
        capture.keymap(omarchy_fr());
        capture.enter(10.0, 10.0);
        capture.keyboard_entered(&[]);
        capture.modifiers(0, 0, locked, 0);
        let _ = sent(&queue);
        (capture, queue)
    }

    #[test]
    fn shift_escape_releases_with_omarchy_s_keyboard_options() {
        for shift in [KEY_LEFTSHIFT, KEY_RIGHTSHIFT] {
            let (mut capture, queue) = armed_on_the_desk(0);
            // As Hyprland sends them: the key, then the modifiers it changed.
            assert_eq!(capture.key(shift, true), None);
            capture.modifiers(SHIFT, 0, 0, 0);
            assert_eq!(capture.key(KEY_ESC, true), Some(Focus::Give));
            assert_eq!(sent(&queue), [key(shift, true), Event::ReleaseAll]);
        }
    }

    #[test]
    fn shift_escape_releases_with_omarchy_s_keyboard_options_and_caps_lock_on() {
        let (mut capture, queue) = armed_on_the_desk(CAPS);
        // With `shift:both_capslock_cancel`, Shift alone while Caps Lock is on is a Caps Lock
        // key: Hyprland reports Lock held, not Shift.
        assert_eq!(capture.key(KEY_LEFTSHIFT, true), None);
        capture.modifiers(CAPS, 0, CAPS, 0);
        assert_eq!(capture.key(KEY_ESC, true), Some(Focus::Give));
        assert_eq!(sent(&queue), [key(KEY_LEFTSHIFT, true), Event::ReleaseAll]);
    }

    #[test]
    fn a_chord_that_is_not_the_hotkey_names_the_modifiers_seen_and_never_another_key() {
        let mut keys = Keys::new(&omarchy_fr()).unwrap();
        let hotkey = Hotkey::parse(DEFAULT_HOTKEY).unwrap().unwrap();
        keys.update(KEY_LEFTCTRL, true);
        assert_eq!(
            keys.chord(&hotkey, KEY_ESC),
            Chord::OtherMods(vec!["Control".into()])
        );
        assert_eq!(keys.chord(&hotkey, KEY_T), Chord::OtherKey);
    }

    #[test]
    fn the_pointer_leaving_releases_everything_once() {
        let (mut capture, queue) = armed(DEFAULT_HOTKEY);
        capture.key(KEY_T, true);
        capture.leave();
        // Giving the keyboard back makes it leave too.
        capture.keyboard_left();
        assert_eq!(sent(&queue), [key(KEY_T, true), Event::ReleaseAll]);
    }

    /// The surface's hold on the keyboard, as calls.
    #[derive(Default)]
    struct FakeGrab(Vec<&'static str>);

    impl Grab for FakeGrab {
        fn interactive(&mut self, on: bool) {
            self.0.push(if on { "on_demand" } else { "none" });
        }
        fn inhibit(&mut self) {
            self.0.push("inhibit");
        }
        fn uninhibit(&mut self) {
            self.0.push("uninhibit");
        }
        fn commit(&mut self) {
            self.0.push("commit");
        }
    }

    fn calls(grab: &mut FakeGrab) -> Vec<&'static str> {
        std::mem::take(&mut grab.0)
    }

    #[test]
    fn the_pointer_entering_takes_the_keyboard_and_the_shortcuts_and_leaving_gives_them_back() {
        let queue = Queue::default();
        let mut capture = Capture::new(queue.clone(), Hotkey::parse(DEFAULT_HOTKEY).unwrap());
        let mut grab = FakeGrab::default();
        handle(&mut capture, &mut grab, Seat::Keymap(us()));
        assert_eq!(calls(&mut grab), [] as [&str; 0]);
        handle(
            &mut capture,
            &mut grab,
            Seat::PointerEnter { x: 1.0, y: 2.0 },
        );
        assert_eq!(calls(&mut grab), ["on_demand", "inhibit", "commit"]);
        handle(&mut capture, &mut grab, Seat::KeyboardEnter { keys: &[] });
        handle(
            &mut capture,
            &mut grab,
            Seat::Key {
                code: KEY_T,
                pressed: true,
            },
        );
        assert_eq!(calls(&mut grab), [] as [&str; 0]);
        handle(&mut capture, &mut grab, Seat::PointerLeave);
        assert_eq!(calls(&mut grab), ["none", "uninhibit", "commit"]);
        handle(&mut capture, &mut grab, Seat::KeyboardLeave { ours: true });
        assert_eq!(calls(&mut grab), [] as [&str; 0]);
        assert_eq!(sent(&queue), [key(KEY_T, true), Event::ReleaseAll]);
    }

    #[test]
    fn the_release_hotkey_gives_the_keyboard_and_the_shortcuts_back() {
        let queue = Queue::default();
        let mut capture = Capture::new(queue.clone(), Hotkey::parse(DEFAULT_HOTKEY).unwrap());
        let mut grab = FakeGrab::default();
        handle(&mut capture, &mut grab, Seat::Keymap(us()));
        handle(
            &mut capture,
            &mut grab,
            Seat::PointerEnter { x: 1.0, y: 2.0 },
        );
        handle(
            &mut capture,
            &mut grab,
            Seat::Key {
                code: KEY_LEFTSHIFT,
                pressed: true,
            },
        );
        let _ = calls(&mut grab);
        handle(
            &mut capture,
            &mut grab,
            Seat::Key {
                code: KEY_ESC,
                pressed: true,
            },
        );
        assert_eq!(calls(&mut grab), ["none", "uninhibit", "commit"]);
    }

    #[test]
    fn the_keyboard_leaving_another_surface_releases_nothing() {
        let (mut capture, queue) = armed(DEFAULT_HOTKEY);
        let mut grab = FakeGrab::default();
        handle(
            &mut capture,
            &mut grab,
            Seat::Key {
                code: KEY_T,
                pressed: true,
            },
        );
        handle(&mut capture, &mut grab, Seat::KeyboardLeave { ours: false });
        assert_eq!(sent(&queue), [key(KEY_T, true)]);
        handle(&mut capture, &mut grab, Seat::KeyboardLeave { ours: true });
        assert_eq!(sent(&queue), [Event::ReleaseAll]);
        assert_eq!(calls(&mut grab), [] as [&str; 0]);
    }

    #[test]
    fn escape_alone_reaches_the_remote() {
        let (mut capture, queue) = armed(DEFAULT_HOTKEY);
        assert_eq!(capture.key(KEY_ESC, true), None);
        assert_eq!(sent(&queue), [key(KEY_ESC, true)]);
    }

    #[test]
    fn a_configured_hotkey_replaces_the_default() {
        let (mut capture, queue) = armed("super+t");
        capture.key(KEY_LEFTSHIFT, true);
        assert_eq!(capture.key(KEY_ESC, true), None);
        capture.key(KEY_LEFTSHIFT, false);
        capture.key(KEY_ESC, false);
        capture.key(KEY_LEFTMETA, true);
        assert_eq!(capture.key(KEY_T, true), Some(Focus::Give));
        assert_eq!(sent(&queue).last(), Some(&Event::ReleaseAll));
        let (mut capture, queue) = armed("none");
        capture.key(KEY_LEFTSHIFT, true);
        assert_eq!(capture.key(KEY_ESC, true), None);
        assert_eq!(sent(&queue).len(), 2);
    }

    #[test]
    fn a_hotkey_is_parsed_or_refused_with_the_reason() {
        assert_eq!(Hotkey::parse("none"), Ok(None));
        assert!(Hotkey::parse("ctrl+alt+q").unwrap().is_some());
        assert!(Hotkey::parse("Shift+Esc").unwrap().is_some());
        assert!(Hotkey::parse("shift+nokey").unwrap_err().contains("nokey"));
        assert!(Hotkey::parse("shift").unwrap_err().contains("no key"));
        assert!(Hotkey::parse("shift+a+b").unwrap_err().contains("two keys"));
        assert_eq!(Hotkey::parse(""), Ok(None));
    }

    #[test]
    fn the_same_keymap_is_sent_once() {
        let queue = Queue::default();
        let mut capture = Capture::new(queue.clone(), None);
        capture.keymap(us());
        let _ = sent(&queue);
        // Sent again by the compositor once the first went over the link: nothing to send.
        capture.keymap(us());
        assert!(queue.queued().is_empty());
    }

    #[test]
    fn a_key_repeat_is_left_to_the_remote() {
        let (mut capture, queue) = armed(DEFAULT_HOTKEY);
        capture.key(KEY_T, true);
        capture.key(KEY_T, true);
        assert_eq!(sent(&queue), [key(KEY_T, true)]);
    }

    #[test]
    fn the_surface_going_away_releases_everything() {
        let (mut capture, queue) = armed(DEFAULT_HOTKEY);
        capture.key(KEY_T, true);
        drop(capture);
        assert_eq!(sent(&queue), [key(KEY_T, true), Event::ReleaseAll]);
    }

    #[test]
    fn motions_in_a_row_keep_only_the_newest() {
        let queue = Queue::default();
        for x in 0..100 {
            queue.push(Event::Motion {
                x: x as f64,
                y: 0.0,
            });
        }
        queue.push(key(KEY_T, true));
        queue.push(Event::Motion { x: 1.0, y: 1.0 });
        assert_eq!(
            queue.queued(),
            [
                Event::Motion { x: 99.0, y: 0.0 },
                key(KEY_T, true),
                Event::Motion { x: 1.0, y: 1.0 }
            ]
        );
    }

    #[test]
    fn a_full_queue_releases_everything_and_never_loses_a_release() {
        let queue = Queue::default();
        queue.push(Event::Keymap("km".into()));
        queue.push(Event::Area {
            width: 1920,
            height: 1080,
        });
        queue.push(key(KEY_LEFTSHIFT, true));
        for n in 0..QUEUE {
            queue.push(key(KEY_T, n % 2 == 0));
        }
        queue.push(key(KEY_LEFTSHIFT, false));
        let queued = queue.queued();
        assert!(queued.len() <= QUEUE + 1, "{}", queued.len());
        // The release pushed after the cut is there, after the ReleaseAll.
        assert_eq!(queued.last(), Some(&key(KEY_LEFTSHIFT, false)));
        assert!(queued.contains(&Event::Keymap("km".into())));
        let release_all = queued.iter().position(|e| *e == Event::ReleaseAll).unwrap();
        // Everything after the cut is pressed again from nothing held: Shift is not down.
        assert!(
            !queued[release_all..].contains(&key(KEY_LEFTSHIFT, true)),
            "{queued:?}"
        );
    }

    #[test]
    fn a_keymap_that_comes_to_a_full_queue_is_the_only_one_kept() {
        let queue = Queue::default();
        queue.push(Event::Keymap("old".into()));
        // Full to the last place: the new keymap is what cuts it.
        for n in 0..QUEUE - 1 {
            queue.push(key(KEY_T, n % 2 == 0));
        }
        queue.push(Event::Keymap("new".into()));
        let keymaps: Vec<Event> = queue
            .queued()
            .into_iter()
            .filter(|e| matches!(e, Event::Keymap(_)))
            .collect();
        assert_eq!(keymaps, [Event::Keymap("new".into())]);
    }

    #[tokio::test]
    async fn a_closed_queue_ends_its_reader_and_takes_nothing() {
        let queue = Queue::default();
        queue.push(key(KEY_T, true));
        queue.close();
        queue.push(key(KEY_T, false));
        assert_eq!(queue.next().await, None);
    }

    #[test]
    fn every_input_event_survives_the_trip_through_the_link() {
        let events = [
            key(KEY_T, true),
            Event::Motion { x: 1.5, y: 2.25 },
            Event::Button {
                button: BTN_LEFT,
                pressed: false,
            },
            Event::Axis {
                axis: Axis::Vertical,
                value: 15.0,
                discrete: Some(1),
                stop: false,
            },
            Event::Keymap("xkb_keymap {};".into()),
            Event::Area {
                width: 2560,
                height: 1440,
            },
            Event::ReleaseAll,
            locked(&["Lock"]),
        ];
        for event in events {
            let msg = crate::wire::LinkMsg::Input(event.clone(), None);
            let bytes = minicbor::to_vec(&msg).unwrap();
            let back: crate::wire::LinkMsg = minicbor::decode(&bytes).unwrap();
            assert_eq!(back, msg);
        }
    }

    #[test]
    fn a_motion_on_the_next_output_does_not_replace_the_last_one_on_the_first() {
        let queue = Queue::default();
        let (left, right) = (queue.on(0), queue.on(1));
        left.push(Event::Motion { x: 1.0, y: 1.0 });
        left.push(Event::ReleaseAll);
        left.push(Event::Motion { x: 1919.0, y: 5.0 });
        right.push(Event::Motion { x: 0.0, y: 5.0 });
        right.push(Event::Motion { x: 3.0, y: 5.0 });
        assert_eq!(
            queue.queued_on(),
            [
                (0, Event::Motion { x: 1.0, y: 1.0 }),
                (0, Event::ReleaseAll),
                (0, Event::Motion { x: 1919.0, y: 5.0 }),
                (1, Event::Motion { x: 3.0, y: 5.0 }),
            ]
        );
    }

    #[test]
    fn a_cut_keeps_each_output_s_area_and_releases_each_output_it_dropped() {
        let queue = Queue::default();
        let (left, right) = (queue.on(0), queue.on(1));
        let area = |width| Event::Area {
            width,
            height: 1080,
        };
        left.push(area(1920));
        right.push(area(2560));
        right.push(key(KEY_T, true));
        // Full to the last place: the last key is what cuts it.
        for n in 0..QUEUE - 2 {
            left.push(key(KEY_LEFTSHIFT, n % 2 == 0));
        }
        let queued = queue.queued_on();
        assert_eq!(
            queued,
            [
                (0, area(1920)),
                (1, area(2560)),
                (0, Event::ReleaseAll),
                (1, Event::ReleaseAll),
                (0, key(KEY_LEFTSHIFT, false)),
            ]
        );
    }

    /// Every output's events, the pointer's left out, and the queue emptied.
    fn sent_on(queue: &Queue) -> Vec<(u8, Event)> {
        let events = queue.queued_on();
        queue.shared.waiting.lock().unwrap().events.clear();
        events
            .into_iter()
            .filter(|(_, e)| !matches!(e, Event::Keymap(_) | Event::Motion { .. }))
            .collect()
    }

    #[test]
    fn shift_held_from_one_work_monitor_to_the_next_stays_down_and_goes_up_at_home() {
        let queue = Queue::default();
        let mut first = Capture::new(queue.clone(), None);
        let mut second = Capture::new(queue.on(1), None);
        first.enter(1900.0, 500.0);
        first.key(KEY_LEFTSHIFT, true);
        first.leave();
        second.enter(10.0, 500.0);
        second.keyboard_entered(&[KEY_LEFTSHIFT]);
        second.key(KEY_T, true);
        second.key(KEY_T, false);
        assert_eq!(
            sent_on(&queue),
            [
                (0, key(KEY_LEFTSHIFT, true)),
                (0, Event::ReleaseAll),
                (1, key(KEY_LEFTSHIFT, true)),
                (1, key(KEY_T, true)),
                (1, key(KEY_T, false)),
            ]
        );
        // To a home monitor: everything goes up, the second output's Shift with it.
        second.leave();
        assert_eq!(sent_on(&queue), [(1, Event::ReleaseAll)]);
        second.key(KEY_LEFTSHIFT, false);
        assert_eq!(sent_on(&queue), []);
    }

    #[test]
    fn a_key_let_go_at_home_is_not_pressed_on_the_next_work_monitor() {
        let queue = Queue::default();
        let mut first = Capture::new(queue.clone(), None);
        let mut second = Capture::new(queue.on(1), None);
        first.enter(1900.0, 500.0);
        first.key(KEY_LEFTSHIFT, true);
        first.leave();
        // Let go over a home monitor, then the pointer comes to the second.
        second.enter(10.0, 500.0);
        second.keyboard_entered(&[]);
        assert_eq!(
            sent_on(&queue),
            [(0, key(KEY_LEFTSHIFT, true)), (0, Event::ReleaseAll)]
        );
        second.keyboard_entered(&[KEY_LEFTSHIFT]);
        assert_eq!(sent_on(&queue), []);
    }
}
