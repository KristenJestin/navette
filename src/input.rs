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
//! queued, or everything held is released. A keymap replaces one still queued, so the queue
//! holds one at most, and its memory stays bounded.
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
    /// Every key and button held goes up.
    #[n(6)]
    ReleaseAll,
}

#[derive(Default)]
struct Waiting {
    events: VecDeque<Event>,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    waiting: Mutex<Waiting>,
    notify: Notify,
}

/// One direction's queue (see the module's doc for its policy).
#[derive(Clone, Default)]
pub struct Queue(Arc<Shared>);

impl Queue {
    /// Queues an event; never waits. Dropped once the queue is closed.
    pub fn push(&self, event: Event) {
        let mut waiting = self.0.waiting.lock().unwrap();
        if waiting.closed {
            return;
        }
        let events = &mut waiting.events;
        let follows_a_motion = matches!(event, Event::Motion { .. })
            && matches!(events.back(), Some(Event::Motion { .. }));
        let full = events.len() >= QUEUE;
        match events.back_mut() {
            Some(last) if follows_a_motion => *last = event,
            _ if full => {
                let keymap = events
                    .iter()
                    .rfind(|e| matches!(e, Event::Keymap(_)))
                    .cloned();
                let area = events
                    .iter()
                    .rfind(|e| matches!(e, Event::Area { .. }))
                    .cloned();
                events.clear();
                events.extend(keymap);
                events.extend(area);
                events.push_back(Event::ReleaseAll);
                events.push_back(event);
            }
            _ => {
                // Only the newest keymap matters: the queue holds one at most.
                if matches!(event, Event::Keymap(_)) {
                    events.retain(|e| !matches!(e, Event::Keymap(_)));
                }
                events.push_back(event);
            }
        }
        drop(waiting);
        self.0.notify.notify_one();
    }

    /// The next event, oldest first; `None` once closed.
    pub async fn next(&self) -> Option<Event> {
        loop {
            {
                let mut waiting = self.0.waiting.lock().unwrap();
                if let Some(event) = waiting.events.pop_front() {
                    return Some(event);
                }
                if waiting.closed {
                    return None;
                }
            }
            self.0.notify.notified().await;
        }
    }

    /// The link is down: nothing more is queued, and `next` ends.
    pub fn close(&self) {
        let mut waiting = self.0.waiting.lock().unwrap();
        waiting.closed = true;
        waiting.events.clear();
        drop(waiting);
        self.0.notify.notify_one();
    }

    #[cfg(test)]
    pub fn queued(&self) -> Vec<Event> {
        self.0
            .waiting
            .lock()
            .unwrap()
            .events
            .iter()
            .cloned()
            .collect()
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
    /// `shift+escape`, `ctrl+alt+q`, `super+f12`…; `none` (or nothing) for no hotkey.
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
                name => sym = Some(keysym(name).ok_or_else(|| format!("unknown key '{name}'"))?),
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
        Some(Self { keymap, state })
    }

    fn update(&mut self, code: u32, pressed: bool) {
        let direction = if pressed {
            xkb::KeyDirection::Down
        } else {
            xkb::KeyDirection::Up
        };
        self.state.update_key(keycode(code), direction);
    }

    fn reset(&mut self) {
        self.state = xkb::State::new(&self.keymap);
    }

    /// The press of `code` with the modifiers held now is the hotkey. The key is read on its
    /// first level, so `shift+q` is the Q key with Shift, whatever Shift makes of it.
    fn is(&self, hotkey: &Hotkey, code: u32) -> bool {
        let active = |name| {
            self.state
                .mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE)
        };
        let mods = Mods {
            shift: active(xkb::MOD_NAME_SHIFT),
            ctrl: active(xkb::MOD_NAME_CTRL),
            alt: active(xkb::MOD_NAME_ALT),
            logo: active(xkb::MOD_NAME_LOGO),
        };
        let layout = self.state.key_get_layout(keycode(code));
        let syms = self.keymap.key_get_syms_by_level(keycode(code), layout, 0);
        mods == hotkey.mods && syms.first() == Some(&hotkey.sym)
    }
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
        }
    }

    pub fn keymap(&mut self, text: String) {
        if text == self.keymap {
            return;
        }
        self.keys = Keys::new(&text);
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

    /// The keyboard went elsewhere while the pointer stayed: the keys held go up there.
    pub fn keyboard_left(&mut self) {
        self.release();
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
        if pressed
            && let (Some(hotkey), Some(keys)) = (&self.hotkey, &self.keys)
            && keys.is(hotkey, code)
        {
            self.release();
            self.released = true;
            return Some(Focus::Give);
        }
        if let Some(keys) = &mut self.keys {
            keys.update(code, pressed);
        }
        // A release whose press the remote never got (held before the pointer came) stays home.
        if track(&mut self.held_keys, code, pressed) {
            self.queue.push(Event::Key { code, pressed });
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
        while queue.0.waiting.lock().unwrap().events.pop_front().is_some() {}
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
        assert!(queued.contains(&Event::Keymap("km".into())));
        let release_all = queued.iter().position(|e| *e == Event::ReleaseAll).unwrap();
        // Everything after the cut is pressed again from nothing held: Shift is not down.
        assert!(
            !queued[release_all..].contains(&key(KEY_LEFTSHIFT, true)),
            "{queued:?}"
        );
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
        ];
        for event in events {
            let msg = crate::wire::LinkMsg::Input(event.clone());
            let bytes = minicbor::to_vec(&msg).unwrap();
            let back: crate::wire::LinkMsg = minicbor::decode(&bytes).unwrap();
            assert_eq!(back, msg);
        }
    }
}
