//! The corner (#69): while a mode runs, every home monitor shows, at its top left, what is
//! linked and where the mic is, with a button that switches the mic.
//!
//! - **On each reserved monitor**, over the remote's picture: a warm orange gradient, a
//!   `REMOTE` tag and the mic button. It is drawn at home and never sent: the remote never
//!   captures it, and a click on it stays home.
//! - **On each of home's own monitors**: the same corner, red with a `LOCAL` tag, wherever the
//!   mic is.
//! - **The tags and how far down the corners start** are the mode's to set (`remote_label`, `local_label`,
//!   `remote_bar`, `local_bar`): see [`Look`].
//! - **The mic button** is a mic and a round check: on the side that has the mic, the check draws
//!   in and the button turns green; on the other, it wipes out. A click switches the mic, as the
//!   hotkey and `navette mic toggle` do; hovering it shows the hotkey.
//! - **Messages** (`Mic → remote`, `Keyboard → home`, …) show under the button for two seconds,
//!   in place of desktop notifications.
//! - **Files crossing** the clipboard, either way, show as one bar per transfer under the tag,
//!   as wide as the tag row (more only when the text needs it, at most 20 characters) and
//!   stacked tightly, filled by the part done and saying the copy's name and what is left; a
//!   click on a bar stops its transfer, the others going on.
//!
//! [`Board`] carries the mic's place and the messages to every corner open; [`Corner`] is one
//! corner, drawn in software into ARGB pixels. Its text uses a 5×8 bitmap font of its own, so
//! that a few short labels need no font library.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::clipboard::files::human_bytes;
use crate::clipboard::progress::{Bar, Cancel, Headline};
use crate::mic::Status;
use crate::notify::Notifier;

/// How long a message stays under the button.
pub const MESSAGE: Duration = Duration::from_secs(2);
/// How long the check takes to draw in or wipe out.
pub const CHECK: Duration = Duration::from_millis(350);
/// The most characters a tag holds.
pub const LABEL_LONGEST: usize = 12;
/// The most bars of transfers a corner shows: the first ones started.
pub const BARS_SHOWN: usize = 3;

/// When a corner is drawn again with nothing new heard.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Next {
    /// At the compositor's next frame, while the check moves: one step per refresh of the
    /// monitor, never two in one.
    Frame,
    After(Duration),
}

/// What a corner hears.
#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    /// Where the mic goes.
    Mic(Status),
    /// A message to show under the button.
    Say(String),
    /// The transfers of files crossing the clipboard; none once they are done.
    Files(Vec<Bar>),
}

/// What a click on the corner does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// Switches the mic.
    Mic,
    /// Stops the transfer of this [`Bar::id`].
    Cancel(u64),
}

type Listener = Box<dyn FnMut(&Update) -> bool + Send>;

/// What every open corner shows: where the mic is, and the messages. It is also the mic's
/// [`Notifier`]: what the mic says goes to the corners, not to the desktop's notifications.
pub struct Board {
    inner: Mutex<Inner>,
    /// What a click on the mic button does.
    toggle: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// Changed by a click on a bar: the clipboard stops its transfer.
    cancel: watch::Sender<Cancel>,
}

struct Inner {
    mic: Status,
    files: Vec<Bar>,
    /// Each returns false once its corner is gone.
    listeners: Vec<Listener>,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                mic: Status::Off,
                files: Vec::new(),
                listeners: Vec::new(),
            }),
            toggle: OnceLock::new(),
            cancel: watch::channel(Cancel::default()).0,
        }
    }
}

impl Board {
    /// The mic goes `to`: every corner moves its check.
    pub fn mic(&self, to: Status) {
        let mut inner = self.inner.lock().unwrap();
        if inner.mic != to {
            inner.mic = to;
            inner.tell(&Update::Mic(to));
        }
    }

    /// Shows `text` in every corner open; with none, nobody sees it.
    pub fn say(&self, text: &str) {
        self.inner
            .lock()
            .unwrap()
            .tell(&Update::Say(text.to_owned()));
    }

    /// The transfers of files crossing the clipboard: every corner shows their bars, or no more.
    pub fn files(&self, bars: Vec<Bar>) {
        let mut inner = self.inner.lock().unwrap();
        if inner.files != bars {
            inner.files = bars.clone();
            inner.tell(&Update::Files(bars));
        }
    }

    /// Changes at each click on a bar, naming its transfer.
    pub fn cancels(&self) -> watch::Receiver<Cancel> {
        self.cancel.subscribe()
    }

    /// `listener` hears where the mic is now and the files crossing, then every change and
    /// message, until it returns false.
    pub fn listen(&self, mut listener: impl FnMut(&Update) -> bool + Send + 'static) {
        let mut inner = self.inner.lock().unwrap();
        let files = inner.files.clone();
        if listener(&Update::Mic(inner.mic))
            && (files.is_empty() || listener(&Update::Files(files)))
        {
            inner.listeners.push(Box::new(listener));
        }
    }

    /// What a click on the mic button does: set once, when the mic is there.
    pub fn on_click(&self, toggle: impl Fn() + Send + Sync + 'static) {
        let _ = self.toggle.set(Box::new(toggle));
    }

    /// The mic button was clicked. The switch may wait for PipeWire: it runs on a thread of its
    /// own, never on a corner's.
    pub fn click(self: &Arc<Self>) {
        let board = self.clone();
        let _ = std::thread::Builder::new()
            .name("navette-corner".into())
            .spawn(move || {
                if let Some(toggle) = board.toggle.get() {
                    toggle();
                }
            });
    }
}

impl Inner {
    fn tell(&mut self, update: &Update) {
        self.listeners.retain_mut(|listener| listener(update));
    }
}

impl Notifier for Board {
    fn show(&self, summary: &str, body: &str) {
        if body.is_empty() {
            self.say(summary);
        } else {
            self.say(&format!("{summary}: {body}"));
        }
    }
}

/// Keeps the board on where the mic goes, whoever switched it: the hotkey, `navette mic`, a
/// click, a mode starting or stopping.
pub async fn follow(board: Arc<Board>, mut status: watch::Receiver<Status>) {
    loop {
        board.mic(*status.borrow_and_update());
        if status.changed().await.is_err() {
            return;
        }
    }
}

/// What the mode sets of how its corners look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Look {
    /// The tag on the reserved monitors.
    pub remote_label: String,
    /// The tag on home's own monitors.
    pub home_label: String,
    /// How far below the top of a reserved monitor its corner starts, in logical pixels, over
    /// what the remote shows there.
    pub remote_bar: i32,
    /// How far below the top of each of home's own monitors its corner starts, in logical
    /// pixels, over home's bars; with none, the compositor places it below them.
    pub home_bar: Option<i32>,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            remote_label: "REMOTE".into(),
            home_label: "LOCAL".into(),
            remote_bar: 0,
            home_bar: Some(0),
        }
    }
}

/// Why `label` cannot be a tag, if it cannot: the font draws printable ASCII, and the corner
/// stays small.
pub fn check_label(label: &str) -> Result<(), String> {
    if label.chars().count() > LABEL_LONGEST {
        return Err(format!("{LABEL_LONGEST} characters at most"));
    }
    if !label.chars().all(|c| matches!(c, ' '..='~')) {
        return Err("printable ASCII only".into());
    }
    Ok(())
}

/// What a running mode hands each of its corners.
#[derive(Clone)]
pub struct Feed {
    board: Arc<Board>,
    /// The mic's hotkey, shown when the button is hovered.
    hint: Option<String>,
    look: Look,
}

impl Feed {
    pub fn new(board: Arc<Board>, hint: Option<String>, look: Look) -> Self {
        Self { board, hint, look }
    }

    pub fn look(&self) -> &Look {
        &self.look
    }

    /// A corner on monitor of the given `side`.
    pub fn corner(&self, side: Side) -> Corner {
        let label = match side {
            Side::Remote => &self.look.remote_label,
            Side::Home => &self.look.home_label,
        };
        Corner::new(side, self.hint.clone(), label.clone())
    }

    pub fn listen(&self, listener: impl FnMut(&Update) -> bool + Send + 'static) {
        self.board.listen(listener);
    }

    /// A click on the corner, by where it fell.
    pub fn press(&self, press: Press) {
        match press {
            Press::Mic => self.board.click(),
            Press::Cancel(transfer) => self.board.cancel.send_modify(|cancel| {
                cancel.clicks = cancel.clicks.wrapping_add(1);
                cancel.transfer = transfer;
            }),
        }
    }
}

/// Which monitor a corner is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// A reserved monitor, showing the remote machine.
    Remote,
    /// One of home's own.
    Home,
}

/// A rectangle in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn holds(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

/// Pixels in ARGB, premultiplied, as `wl_shm`'s ARGB8888 takes them.
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub argb: Vec<u32>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            argb: vec![0; (width * height) as usize],
        }
    }

    /// The pixel at (`x`, `y`) as (alpha, red, green, blue), premultiplied.
    pub fn at(&self, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let p = self.argb[(y * self.width + x) as usize];
        ((p >> 24) as u8, (p >> 16) as u8, (p >> 8) as u8, p as u8)
    }

    /// The bytes in memory order (little endian), for a `wl_shm` buffer.
    pub fn write(&self, bytes: &mut [u8]) {
        for (out, p) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(&self.argb) {
            *out = p.to_le_bytes();
        }
    }

    /// Puts `color` (straight, alpha in 0..1) over the pixel with `coverage`.
    fn blend(&mut self, x: i64, y: i64, color: Color, coverage: f64) {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            return;
        }
        let a = (color.a * coverage).clamp(0.0, 1.0);
        if a <= 0.0 {
            return;
        }
        let index = (y as u32 * self.width + x as u32) as usize;
        let under = self.argb[index];
        let channel = |shift: u32, value: f64| {
            let below = ((under >> shift) & 0xff) as f64;
            (value * 255.0 * a + below * (1.0 - a)).round() as u32
        };
        self.argb[index] = (channel(24, 1.0) << 24)
            | (channel(16, color.r) << 16)
            | (channel(8, color.g) << 8)
            | channel(0, color.b);
    }
}

/// A straight colour, each part in 0..1.
#[derive(Clone, Copy, Debug)]
struct Color {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

const fn rgb(hex: u32, a: f64) -> Color {
    Color {
        r: ((hex >> 16) & 0xff) as f64 / 255.0,
        g: ((hex >> 8) & 0xff) as f64 / 255.0,
        b: (hex & 0xff) as f64 / 255.0,
        a,
    }
}

impl Color {
    fn faded(self, by: f64) -> Self {
        Self {
            a: self.a * by,
            ..self
        }
    }

    fn towards(self, other: Self, t: f64) -> Self {
        let mix = |a: f64, b: f64| a + (b - a) * t;
        Self {
            r: mix(self.r, other.r),
            g: mix(self.g, other.g),
            b: mix(self.b, other.b),
            a: mix(self.a, other.a),
        }
    }
}

const PRO: Color = rgb(0xe0a24a, 1.0);
const HOME: Color = rgb(0xe0524a, 1.0);
const ON: Color = rgb(0x5fd38d, 1.0);
const OFF: Color = rgb(0x8a919c, 1.0);
const TAG_TEXT: Color = rgb(0x1b1206, 1.0);
const PILL: Color = rgb(0x0e1013, 0.85);
const BORDER: Color = rgb(0xffffff, 0.15);
const LABEL: Color = rgb(0xe7e2d8, 1.0);
const FILL: Color = rgb(0x3d7bd9, 0.9);

/// The corner's layout, in logical pixels.
const PAD: f64 = 10.0;
const ROW: f64 = 22.0;
const GAP: f64 = 8.0;
/// Where the gradient fades out, at 1080p.
const GLOW: (f64, f64) = (150.0, 80.0);
/// One dot of the font, in logical pixels.
const DOT: f64 = 2.0;
/// The longest message shown, in characters; a longer one ends with `…`.
const LONGEST: usize = 64;
/// A bar of files crossing is as tall as the font's eight rows, the descenders of `g`, `p` and
/// `y` included, with a pixel of air: several of them stack under the tag without going far
/// below the room the tag row and a message already take.
const BAR_ROW: f64 = 17.0;
/// The air between two bars, and between the tag row and the first bar.
const BAR_GAP: f64 = 2.0;
/// The longest text of a bar of files crossing, in characters: its name is cut to fit.
const BAR_CHARS: usize = 20;

/// One corner: what it shows, and how it is drawn.
pub struct Corner {
    side: Side,
    /// The text of the tag.
    label: String,
    mic: Status,
    hint: Option<String>,
    /// The check's movement: from, to, since when.
    check: (f64, f64, Option<Instant>),
    message: Option<(String, Instant)>,
    hover: bool,
    files: Vec<Bar>,
    /// Where the pointer is over the corner, in its logical pixels.
    pointer: Option<(f64, f64)>,
}

impl Corner {
    pub fn new(side: Side, hint: Option<String>, label: String) -> Self {
        Self {
            side,
            label,
            mic: Status::Off,
            hint,
            check: (0.0, 0.0, None),
            message: None,
            hover: false,
            files: Vec::new(),
            pointer: None,
        }
    }

    /// Takes what the board says, at `now`.
    pub fn update(&mut self, update: Update, now: Instant) {
        match update {
            Update::Mic(to) => {
                let first = self.mic == Status::Off && self.check.2.is_none();
                self.mic = to;
                let target = if self.has_mic() { 1.0 } else { 0.0 };
                if first {
                    // A corner that opens with the mic already there shows it checked.
                    self.check = (target, target, Some(now));
                } else {
                    self.check = (self.check_at(now), target, Some(now));
                }
            }
            Update::Say(text) => self.message = Some((text, now)),
            Update::Files(bar) => self.files = bar,
        }
    }

    /// The pointer moved to (`x`, `y`) over the corner.
    pub fn point_at(&mut self, x: f64, y: f64) {
        self.pointer = Some((x, y));
    }

    /// What a click where the pointer is does: on a bar, it stops its transfer; anywhere
    /// else, it switches the mic.
    pub fn press(&self) -> Press {
        let Some((x, y)) = self.pointer else {
            return Press::Mic;
        };
        self.bars()
            .into_iter()
            .find(|(rect, _)| rect.holds(x, y))
            .map_or(Press::Mic, |(_, bar)| Press::Cancel(bar.id))
    }

    /// The pointer is over the button, or not; true when that changes what is shown.
    pub fn hover(&mut self, over: bool) -> bool {
        std::mem::replace(&mut self.hover, over) != over
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether this corner's side has the mic.
    fn has_mic(&self) -> bool {
        matches!(
            (self.side, self.mic),
            (Side::Remote, Status::Remote) | (Side::Home, Status::Home)
        )
    }

    /// How far the check is drawn, 0 to 1.
    fn check_at(&self, now: Instant) -> f64 {
        let (from, to, since) = self.check;
        let Some(since) = since else { return to };
        let t = (now.saturating_duration_since(since).as_secs_f64() / CHECK.as_secs_f64()).min(1.0);
        from + (to - from) * t
    }

    fn message_at(&self, now: Instant) -> Option<&str> {
        let (text, since) = self.message.as_ref()?;
        (now.saturating_duration_since(*since) < MESSAGE).then_some(text.as_str())
    }

    /// When the corner must be drawn again with nothing new heard: at the next frame while the
    /// check moves, when the message goes, or never.
    pub fn next_draw(&self, now: Instant) -> Option<Next> {
        let (_, to, since) = self.check;
        if since.is_some() && self.check_at(now) != to {
            return Some(Next::Frame);
        }
        let (_, said) = self.message.as_ref()?;
        let left = MESSAGE.checked_sub(now.saturating_duration_since(*said))?;
        (!left.is_zero()).then_some(Next::After(left))
    }

    fn button_shown(&self) -> bool {
        self.mic != Status::Off
    }

    /// The tag's rectangle, when there is one.
    fn tag(&self) -> Option<(Rect, &str)> {
        let text = self.label.as_str();
        let width = text_width(text) + 2.0 * 6.0;
        Some((
            Rect {
                x: PAD,
                y: PAD,
                width,
                height: ROW,
            },
            text,
        ))
    }

    /// The mic button, in the corner's logical pixels: where a click switches the mic.
    pub fn button(&self) -> Option<Rect> {
        if !self.button_shown() {
            return None;
        }
        let x = self.tag().map_or(PAD, |(tag, _)| tag.x + tag.width + GAP);
        Some(Rect {
            x,
            y: PAD,
            width: 48.0,
            height: ROW,
        })
    }

    /// The bars of the transfers shown, one under the other below the tag, all as wide as the
    /// tag row or as their longest text: where a click stops one.
    pub fn bars(&self) -> Vec<(Rect, Bar)> {
        let shown = self.files.iter().take(BARS_SHOWN);
        let widest = shown
            .clone()
            .map(|bar| text_width(&bar_text(bar)) + 2.0 * 6.0)
            .fold(self.row_width(), f64::max);
        let mut y = PAD + ROW;
        shown
            .map(|bar| {
                y += BAR_GAP;
                let rect = Rect {
                    x: PAD,
                    y,
                    width: widest,
                    height: BAR_ROW,
                };
                y += BAR_ROW;
                (rect, bar.clone())
            })
            .collect()
    }

    /// How wide the tag and the button make the first row.
    fn row_width(&self) -> f64 {
        let right = self.button().map_or_else(
            || self.tag().map_or(PAD, |(tag, _)| tag.x + tag.width),
            |button| button.x + button.width,
        );
        right - PAD
    }

    /// The top of the line under the button: under the bars when there are some.
    fn line_y(&self) -> f64 {
        let bottom = self
            .bars()
            .last()
            .map_or(PAD + ROW, |(bar, _)| bar.y + bar.height);
        bottom + 6.0
    }

    /// The line under the button: the message, else the hint while hovered.
    fn line(&self, now: Instant) -> Option<String> {
        let text = match self.message_at(now) {
            Some(text) => text.to_owned(),
            None if self.hover && self.button_shown() => self.hint.clone()?,
            None => return None,
        };
        if text.chars().count() > LONGEST {
            let cut: String = text.chars().take(LONGEST - 1).collect();
            return Some(format!("{cut}…"));
        }
        Some(text)
    }

    /// The corner's size in logical pixels at `now`.
    pub fn size(&self, now: Instant) -> (u32, u32) {
        let mut right = PAD;
        if let Some((tag, _)) = self.tag() {
            right = tag.x + tag.width;
        }
        if let Some(button) = self.button() {
            right = button.x + button.width;
        }
        let mut bottom = PAD + ROW;
        for (bar, _) in self.bars() {
            right = right.max(bar.x + bar.width);
            bottom = bar.y + bar.height;
        }
        if let Some(line) = self.line(now) {
            right = right.max(PAD + text_width(&line) + 2.0 * 6.0);
            bottom = self.line_y() + ROW;
        }
        let (width, height) = ((right + PAD).max(GLOW.0), (bottom + PAD).max(GLOW.1));
        (width.ceil() as u32, height.ceil() as u32)
    }

    /// The most room the corner takes whatever it shows, in logical pixels: its longest line,
    /// under the most bars of files crossing it shows.
    pub fn room(&self) -> (u32, u32) {
        let now = Instant::now();
        let mut widest = Corner::new(self.side, None, self.label.clone());
        widest.update(Update::Mic(Status::Home), now);
        let bar = Bar {
            id: 0,
            done: 0,
            total: u64::MAX,
            headline: Some(Headline {
                name: "M".repeat(BAR_CHARS),
                more: 0,
            }),
        };
        widest.update(Update::Files(vec![bar; BARS_SHOWN]), now);
        widest.update(Update::Say("M".repeat(LONGEST)), now);
        widest.size(now)
    }

    /// The corner at `now`, at `scale` pixels per logical pixel.
    pub fn draw(&self, now: Instant, scale: f64) -> Canvas {
        self.draw_in(now, scale, self.size(now))
    }

    /// The corner at `now` on a canvas of `size` logical pixels, at `scale`.
    pub fn draw_in(&self, now: Instant, scale: f64, size: (u32, u32)) -> Canvas {
        let (width, height) = size;
        let px = |v: u32| ((v as f64 * scale).ceil() as u32).max(1);
        let mut canvas = Canvas::new(px(width), px(height));
        let mut pen = Pen {
            canvas: &mut canvas,
            scale,
        };
        let glow = match self.side {
            Side::Remote => PRO,
            Side::Home => HOME,
        };
        pen.glow(glow);
        if let Some((tag, text)) = self.tag() {
            let fill = match self.side {
                Side::Remote => PRO,
                Side::Home => HOME,
            };
            pen.rounded(tag, 5.0, fill);
            pen.text(tag.x + 6.0, tag.y + (ROW - 7.0 * DOT) / 2.0, text, TAG_TEXT);
        }
        if let Some(button) = self.button() {
            let check = self.check_at(now);
            let accent = OFF.towards(ON, check);
            pen.rounded(button, ROW / 2.0, PILL);
            pen.rounded_outline(button, ROW / 2.0, BORDER.towards(accent.faded(0.6), check));
            pen.mic(button.x + 15.0, button.y + ROW / 2.0, accent);
            let (cx, cy) = (button.x + button.width - 15.0, button.y + ROW / 2.0);
            pen.ring(cx, cy, 7.0, accent);
            pen.check(cx, cy, check, accent);
        }
        for (bar, files) in self.bars() {
            pen.rounded(bar, 4.0, PILL);
            let part = (files.done as f64 / files.total.max(1) as f64).min(1.0);
            if part > 0.0 {
                let done = Rect {
                    width: (bar.width * part).max(10.0),
                    ..bar
                };
                pen.rounded(done, 4.0, FILL);
            }
            pen.rounded_outline(bar, 4.0, BORDER);
            let text = bar_text(&files);
            pen.text(
                bar.x + 6.0,
                bar.y + (BAR_ROW - 8.0 * DOT) / 2.0,
                &text,
                LABEL,
            );
        }
        if let Some(line) = self.line(now) {
            let row = Rect {
                x: PAD,
                y: self.line_y(),
                width: text_width(&line) + 2.0 * 6.0,
                height: ROW,
            };
            pen.rounded(row, 5.0, PILL);
            pen.text(row.x + 6.0, row.y + (ROW - 7.0 * DOT) / 2.0, &line, LABEL);
        }
        canvas
    }
}

/// What a bar of files crossing says.
///
/// The copy's name (and how many more it holds), what is left and the cancel mark: the fill says
/// how far it is. The name is cut with `…` so that the whole stays within [`BAR_CHARS`]; what is
/// left and the mark never are.
fn bar_text(bar: &Bar) -> String {
    let tail = format!("{} x", human_bytes(bar.left()));
    let Some(Headline { name, more }) = &bar.headline else {
        return tail;
    };
    let more = if *more > 0 {
        format!(" +{more}")
    } else {
        String::new()
    };
    let room = BAR_CHARS.saturating_sub(tail.chars().count() + 1 + more.chars().count());
    if room == 0 {
        return tail;
    }
    let name = if name.chars().count() > room {
        let cut: String = name.chars().take(room - 1).collect();
        format!("{cut}…")
    } else {
        name.clone()
    };
    format!("{name}{more} {tail}")
}

/// Draws in logical pixels on a canvas of physical ones.
struct Pen<'a> {
    canvas: &'a mut Canvas,
    scale: f64,
}

impl Pen<'_> {
    /// Each physical pixel's centre, in logical pixels, with its coordinates.
    fn pixels(&self, area: Rect) -> impl Iterator<Item = (i64, i64, f64, f64)> + use<> {
        let s = self.scale;
        let x0 = (area.x * s).floor().max(0.0) as i64;
        let y0 = (area.y * s).floor().max(0.0) as i64;
        let x1 = (((area.x + area.width) * s).ceil() as i64).min(self.canvas.width as i64);
        let y1 = (((area.y + area.height) * s).ceil() as i64).min(self.canvas.height as i64);
        (y0..y1).flat_map(move |y| {
            (x0..x1).map(move |x| (x, y, (x as f64 + 0.5) / s, (y as f64 + 0.5) / s))
        })
    }

    /// Paints `area` with the coverage `shape` gives each point, from its signed distance to
    /// the shape's edge in logical pixels (negative inside).
    fn fill(&mut self, area: Rect, color: Color, distance: impl Fn(f64, f64) -> f64) {
        let s = self.scale;
        for (x, y, lx, ly) in self.pixels(area).collect::<Vec<_>>() {
            let coverage = (0.5 - distance(lx, ly) * s).clamp(0.0, 1.0);
            self.canvas.blend(x, y, color, coverage);
        }
    }

    /// The gradient from the corner, fading out within [`GLOW`].
    fn glow(&mut self, color: Color) {
        let all = Rect {
            x: 0.0,
            y: 0.0,
            width: GLOW.0,
            height: GLOW.1,
        };
        for (x, y, lx, ly) in self.pixels(all).collect::<Vec<_>>() {
            let d = ((lx / GLOW.0).powi(2) + (ly / GLOW.1).powi(2)).sqrt();
            let a = if d < 0.5 {
                0.55 - (0.55 - 0.2) * d / 0.5
            } else {
                (0.2 * (1.0 - d) / 0.5).max(0.0)
            };
            self.canvas.blend(x, y, color.faded(a), 1.0);
        }
    }

    fn rounded(&mut self, r: Rect, radius: f64, color: Color) {
        self.fill(r, color, |x, y| rounded_distance(r, radius, x, y));
    }

    fn rounded_outline(&mut self, r: Rect, radius: f64, color: Color) {
        self.fill(r, color, |x, y| {
            let d = rounded_distance(r, radius, x, y);
            d.max(-d - 1.0)
        });
    }

    fn ring(&mut self, cx: f64, cy: f64, radius: f64, color: Color) {
        let area = Rect {
            x: cx - radius - 2.0,
            y: cy - radius - 2.0,
            width: 2.0 * radius + 4.0,
            height: 2.0 * radius + 4.0,
        };
        self.fill(area, color, |x, y| {
            (((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - radius).abs() - 0.75
        });
    }

    /// A stroke along `points`, `width` wide.
    fn stroke(&mut self, points: &[(f64, f64)], width: f64, color: Color) {
        if points.len() < 2 {
            return;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for &(x, y) in points {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
        let area = Rect {
            x: x0 - width,
            y: y0 - width,
            width: x1 - x0 + 2.0 * width,
            height: y1 - y0 + 2.0 * width,
        };
        self.fill(area, color, |x, y| {
            points
                .windows(2)
                .map(|w| segment_distance(w[0], w[1], x, y))
                .fold(f64::MAX, f64::min)
                - width / 2.0
        });
    }

    /// A mic: a capsule on a stand.
    fn mic(&mut self, cx: f64, cy: f64, color: Color) {
        let capsule = Rect {
            x: cx - 2.5,
            y: cy - 7.0,
            width: 5.0,
            height: 9.0,
        };
        self.rounded(capsule, 2.5, color);
        let cup: Vec<(f64, f64)> = (0..=8)
            .map(|i| {
                let a = std::f64::consts::PI * i as f64 / 8.0;
                (cx + 4.5 * a.cos(), cy - 1.0 + 4.5 * a.sin())
            })
            .collect();
        self.stroke(&cup, 1.4, color);
        self.stroke(&[(cx, cy + 3.5), (cx, cy + 6.5)], 1.4, color);
    }

    /// The check inside the ring, drawn `part` of the way along.
    fn check(&mut self, cx: f64, cy: f64, part: f64, color: Color) {
        if part <= 0.0 {
            return;
        }
        let path = [(cx - 3.5, cy), (cx - 1.0, cy + 2.5), (cx + 3.5, cy - 2.5)];
        let lengths: Vec<f64> = path.windows(2).map(|w| distance(w[0], w[1])).collect();
        let mut left = part.min(1.0) * lengths.iter().sum::<f64>();
        let mut drawn = vec![path[0]];
        for (w, length) in path.windows(2).zip(lengths) {
            if left <= 0.0 {
                break;
            }
            let t = (left / length).min(1.0);
            drawn.push((
                w[0].0 + (w[1].0 - w[0].0) * t,
                w[0].1 + (w[1].1 - w[0].1) * t,
            ));
            left -= length;
        }
        self.stroke(&drawn, 1.6, color);
    }

    fn text(&mut self, x: f64, y: f64, text: &str, color: Color) {
        let s = self.scale;
        let dot = (DOT * s).round().max(1.0) as i64;
        let (mut px, py) = ((x * s).round() as i64, (y * s).round() as i64);
        for c in text.chars() {
            for (row, bits) in glyph(c).split(' ').enumerate() {
                for (column, bit) in bits.chars().enumerate() {
                    if bit != '#' {
                        continue;
                    }
                    let (gx, gy) = (px + column as i64 * dot, py + row as i64 * dot);
                    for dy in 0..dot {
                        for dx in 0..dot {
                            self.canvas.blend(gx + dx, gy + dy, color, 1.0);
                        }
                    }
                }
            }
            px += 6 * dot;
        }
    }
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn segment_distance(a: (f64, f64), b: (f64, f64), x: f64, y: f64) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx * dx + dy * dy;
    let t = if length == 0.0 {
        0.0
    } else {
        (((x - a.0) * dx + (y - a.1) * dy) / length).clamp(0.0, 1.0)
    };
    distance((a.0 + dx * t, a.1 + dy * t), (x, y))
}

fn rounded_distance(r: Rect, radius: f64, x: f64, y: f64) -> f64 {
    let (cx, cy) = (r.x + r.width / 2.0, r.y + r.height / 2.0);
    let qx = (x - cx).abs() - (r.width / 2.0 - radius);
    let qy = (y - cy).abs() - (r.height / 2.0 - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

/// A line of text's width in logical pixels: 6 dots a character, the last one's gap left out.
fn text_width(text: &str) -> f64 {
    let n = text.chars().count() as f64;
    (6.0 * n - 1.0).max(0.0) * DOT
}

/// A character's eight rows of five dots; `?` for one the font does not have.
fn glyph(c: char) -> &'static str {
    FONT.iter()
        .find(|(k, _)| *k == c)
        .or_else(|| FONT.iter().find(|(k, _)| *k == '?'))
        .map(|(_, rows)| *rows)
        .expect("the font has `?`")
}

/// Printable ASCII, the arrow and the ellipsis: drawn for Navette, row by row.
const FONT: &[(char, &str)] = &[
    (' ', "..... ..... ..... ..... ..... ..... ..... ....."),
    ('!', "..#.. ..#.. ..#.. ..#.. ..#.. ..... ..#.. ....."),
    ('"', ".#.#. .#.#. ..... ..... ..... ..... ..... ....."),
    ('#', ".#.#. .#.#. ##### .#.#. ##### .#.#. .#.#. ....."),
    ('$', "..#.. .#### #.#.. .###. ..#.# ####. ..#.. ....."),
    ('%', "##... ##..# ...#. ..#.. .#... #..## ...## ....."),
    ('&', ".##.. #..#. #.#.. .#... #.#.# #..#. .##.# ....."),
    ('\'', "..#.. ..#.. ..... ..... ..... ..... ..... ....."),
    ('(', "...#. ..#.. .#... .#... .#... ..#.. ...#. ....."),
    (')', ".#... ..#.. ...#. ...#. ...#. ..#.. .#... ....."),
    ('*', "..... ..#.. #.#.# .###. #.#.# ..#.. ..... ....."),
    ('+', "..... ..#.. ..#.. ##### ..#.. ..#.. ..... ....."),
    (',', "..... ..... ..... ..... ..... ..#.. ..#.. .#..."),
    ('-', "..... ..... ..... ##### ..... ..... ..... ....."),
    ('.', "..... ..... ..... ..... ..... ..... ..#.. ....."),
    ('/', "....# ...#. ...#. ..#.. .#... .#... #.... ....."),
    ('0', ".###. #...# #..## #.#.# ##..# #...# .###. ....."),
    ('1', "..#.. .##.. ..#.. ..#.. ..#.. ..#.. .###. ....."),
    ('2', ".###. #...# ....# ...#. ..#.. .#... ##### ....."),
    ('3', "####. ....# ....# .###. ....# ....# ####. ....."),
    ('4', "...#. ..##. .#.#. #..#. ##### ...#. ...#. ....."),
    ('5', "##### #.... ####. ....# ....# #...# .###. ....."),
    ('6', ".###. #.... #.... ####. #...# #...# .###. ....."),
    ('7', "##### ....# ...#. ..#.. .#... .#... .#... ....."),
    ('8', ".###. #...# #...# .###. #...# #...# .###. ....."),
    ('9', ".###. #...# #...# .#### ....# ....# .###. ....."),
    (':', "..... ..... ..#.. ..... ..... ..#.. ..... ....."),
    (';', "..... ..... ..#.. ..... ..... ..#.. ..#.. .#..."),
    ('<', "...#. ..#.. .#... #.... .#... ..#.. ...#. ....."),
    ('=', "..... ..... ##### ..... ##### ..... ..... ....."),
    ('>', ".#... ..#.. ...#. ....# ...#. ..#.. .#... ....."),
    ('?', ".###. #...# ....# ...#. ..#.. ..... ..#.. ....."),
    ('@', ".###. #...# #.### #.#.# #.### #.... .###. ....."),
    ('A', ".###. #...# #...# ##### #...# #...# #...# ....."),
    ('B', "####. #...# #...# ####. #...# #...# ####. ....."),
    ('C', ".###. #...# #.... #.... #.... #...# .###. ....."),
    ('D', "####. #...# #...# #...# #...# #...# ####. ....."),
    ('E', "##### #.... #.... ####. #.... #.... ##### ....."),
    ('F', "##### #.... #.... ####. #.... #.... #.... ....."),
    ('G', ".###. #...# #.... #.### #...# #...# .#### ....."),
    ('H', "#...# #...# #...# ##### #...# #...# #...# ....."),
    ('I', ".###. ..#.. ..#.. ..#.. ..#.. ..#.. .###. ....."),
    ('J', "..### ...#. ...#. ...#. ...#. #..#. .##.. ....."),
    ('K', "#...# #..#. #.#.. ##... #.#.. #..#. #...# ....."),
    ('L', "#.... #.... #.... #.... #.... #.... ##### ....."),
    ('M', "#...# ##.## #.#.# #.#.# #...# #...# #...# ....."),
    ('N', "#...# #...# ##..# #.#.# #..## #...# #...# ....."),
    ('O', ".###. #...# #...# #...# #...# #...# .###. ....."),
    ('P', "####. #...# #...# ####. #.... #.... #.... ....."),
    ('Q', ".###. #...# #...# #...# #.#.# #..#. .##.# ....."),
    ('R', "####. #...# #...# ####. #.#.. #..#. #...# ....."),
    ('S', ".#### #.... #.... .###. ....# ....# ####. ....."),
    ('T', "##### ..#.. ..#.. ..#.. ..#.. ..#.. ..#.. ....."),
    ('U', "#...# #...# #...# #...# #...# #...# .###. ....."),
    ('V', "#...# #...# #...# #...# #...# .#.#. ..#.. ....."),
    ('W', "#...# #...# #...# #.#.# #.#.# #.#.# .#.#. ....."),
    ('X', "#...# #...# .#.#. ..#.. .#.#. #...# #...# ....."),
    ('Y', "#...# #...# .#.#. ..#.. ..#.. ..#.. ..#.. ....."),
    ('Z', "##### ....# ...#. ..#.. .#... #.... ##### ....."),
    ('[', ".###. .#... .#... .#... .#... .#... .###. ....."),
    ('\\', "#.... .#... .#... ..#.. ...#. ...#. ....# ....."),
    (']', ".###. ...#. ...#. ...#. ...#. ...#. .###. ....."),
    ('^', "..#.. .#.#. #...# ..... ..... ..... ..... ....."),
    ('_', "..... ..... ..... ..... ..... ..... ..... #####"),
    ('`', ".#... ..#.. ..... ..... ..... ..... ..... ....."),
    ('a', "..... ..... .###. ....# .#### #...# .#### ....."),
    ('b', "#.... #.... ####. #...# #...# #...# ####. ....."),
    ('c', "..... ..... .###. #.... #.... #...# .###. ....."),
    ('d', "....# ....# .#### #...# #...# #...# .#### ....."),
    ('e', "..... ..... .###. #...# ##### #.... .###. ....."),
    ('f', "..##. .#..# .#... ###.. .#... .#... .#... ....."),
    ('g', "..... ..... .#### #...# #...# .#### ....# .###."),
    ('h', "#.... #.... #.##. ##..# #...# #...# #...# ....."),
    ('i', "..#.. ..... .##.. ..#.. ..#.. ..#.. .###. ....."),
    ('j', "...#. ..... ..##. ...#. ...#. ...#. #..#. .##.."),
    ('k', "#.... #.... #..#. #.#.. ##... #.#.. #..#. ....."),
    ('l', ".##.. ..#.. ..#.. ..#.. ..#.. ..#.. .###. ....."),
    ('m', "..... ..... ##.#. #.#.# #.#.# #.#.# #.#.# ....."),
    ('n', "..... ..... #.##. ##..# #...# #...# #...# ....."),
    ('o', "..... ..... .###. #...# #...# #...# .###. ....."),
    ('p', "..... ..... ####. #...# #...# ####. #.... #...."),
    ('q', "..... ..... .#### #...# #...# .#### ....# ....#"),
    ('r', "..... ..... #.##. ##..# #.... #.... #.... ....."),
    ('s', "..... ..... .#### #.... .###. ....# ####. ....."),
    ('t', ".#... .#... ###.. .#... .#... .#..# ..##. ....."),
    ('u', "..... ..... #...# #...# #...# #..## .##.# ....."),
    ('v', "..... ..... #...# #...# #...# .#.#. ..#.. ....."),
    ('w', "..... ..... #...# #...# #.#.# #.#.# .#.#. ....."),
    ('x', "..... ..... #...# .#.#. ..#.. .#.#. #...# ....."),
    ('y', "..... ..... #...# #...# #...# .#### ....# .###."),
    ('z', "..... ..... ##### ...#. ..#.. .#... ##### ....."),
    ('{', "...#. ..#.. ..#.. .#... ..#.. ..#.. ...#. ....."),
    ('|', "..#.. ..#.. ..#.. ..#.. ..#.. ..#.. ..#.. ....."),
    ('}', ".#... ..#.. ..#.. ...#. ..#.. ..#.. .#... ....."),
    ('~', "..... ..... .#... #.#.# ...#. ..... ..... ....."),
    ('→', "..... ..#.. ...#. ##### ...#. ..#.. ..... ....."),
    ('…', "..... ..... ..... ..... ..... ..... #.#.# ....."),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::progress::Headline;

    fn corner(side: Side, mic: Status, now: Instant) -> Corner {
        let look = Look::default();
        let label = match side {
            Side::Remote => look.remote_label,
            Side::Home => look.home_label,
        };
        let mut corner = Corner::new(side, Some("SUPER + CTRL + M".into()), label);
        corner.update(Update::Mic(mic), now);
        corner
    }

    /// The pixel's straight colour at (`x`, `y`) logical, and its alpha.
    fn color(canvas: &Canvas, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let (a, r, g, b) = canvas.at(x, y);
        let straight = |v: u8| (v as u32 * 255 / (a as u32).max(1)) as u8;
        (straight(r), straight(g), straight(b), a)
    }

    /// The button's ring, on its right: where the check is.
    fn ring(corner: &Corner) -> (u32, u32) {
        let button = corner.button().expect("a button");
        let (x, y) = (button.x + button.width - 15.0 + 7.0, button.y + ROW / 2.0);
        (x as u32, y as u32)
    }

    fn is_green((r, g, b, a): (u8, u8, u8, u8)) -> bool {
        a > 0 && g > r + 40 && g > b + 20
    }

    #[test]
    fn home_s_corner_keeps_its_glow_and_tag_whichever_side_has_the_mic() {
        let now = Instant::now();
        for mic in [Status::Home, Status::Remote] {
            let home = corner(Side::Home, mic, now);
            let drawn = home.draw(now, 1.0);
            // The gradient, red, at the very corner.
            let (r, g, b, a) = color(&drawn, 1, 1);
            assert!(
                a > 100 && r > 200 && g < 120 && b < 120,
                "{mic:?}: {:?}",
                (r, g, b, a)
            );
            assert_eq!(home.tag().unwrap().1, "LOCAL", "{mic:?}");
            assert!(
                drawn.width >= 150 && drawn.height >= 80,
                "{mic:?}: the whole gradient"
            );
        }
        // Only the button tells where the mic is.
        let here = corner(Side::Home, Status::Home, now);
        assert!(is_green(color(
            &here.draw(now, 1.0),
            ring(&here).0,
            ring(&here).1
        )));
        let away = corner(Side::Home, Status::Remote, now);
        assert!(!is_green(color(
            &away.draw(now, 1.0),
            ring(&away).0,
            ring(&away).1
        )));
        assert_eq!(here.button(), away.button());
    }

    #[test]
    fn a_corner_drawn_in_its_room_keeps_its_shapes_where_they_are() {
        let now = Instant::now();
        let home = corner(Side::Home, Status::Home, now);
        let room = home.room();
        let tight = home.draw(now, 2.0);
        let roomy = home.draw_in(now, 2.0, room);
        assert_eq!((roomy.width, roomy.height), (2 * room.0, 2 * room.1));
        for y in 0..tight.height {
            for x in 0..tight.width {
                assert_eq!(tight.at(x, y), roomy.at(x, y), "({x}, {y})");
            }
        }
        // Around it, nothing.
        assert_eq!(roomy.at(roomy.width - 1, roomy.height - 1).0, 0);
    }

    #[test]
    fn the_reserved_corner_is_orange_with_its_tag_and_checked_with_the_mic() {
        let now = Instant::now();
        let pro = corner(Side::Remote, Status::Remote, now);
        let drawn = pro.draw(now, 1.0);
        let (r, g, b, a) = color(&drawn, 1, 1);
        assert!(
            a > 100 && r > 200 && (130..190).contains(&g) && b < 110,
            "{:?}",
            (r, g, b)
        );
        assert_eq!(pro.tag().unwrap().1, "REMOTE");
        assert!(is_green(color(&drawn, ring(&pro).0, ring(&pro).1)));
        // The mic home: the remote's corner stays whole, its check out.
        let home = corner(Side::Remote, Status::Home, now);
        assert!(home.tag().is_some());
        assert!(!is_green(color(
            &home.draw(now, 1.0),
            ring(&home).0,
            ring(&home).1
        )));
    }

    #[test]
    fn the_check_draws_in_over_350_ms_and_then_stops_drawing() {
        let now = Instant::now();
        let mut pro = corner(Side::Remote, Status::Home, now);
        assert_eq!(
            pro.next_draw(now),
            None,
            "a corner at rest is not drawn again"
        );
        pro.update(Update::Mic(Status::Remote), now);
        assert_eq!(pro.check_at(now), 0.0);
        assert_eq!(pro.next_draw(now), Some(Next::Frame));
        let half = now + CHECK / 2;
        assert!((pro.check_at(half) - 0.5).abs() < 0.01);
        assert_eq!(pro.check_at(now + CHECK), 1.0);
        assert_eq!(pro.next_draw(now + CHECK), None);
        // And wipes out the same way.
        let later = now + CHECK * 2;
        pro.update(Update::Mic(Status::Home), later);
        assert!((pro.check_at(later + CHECK / 2) - 0.5).abs() < 0.01);
        assert_eq!(pro.check_at(later + CHECK), 0.0);
    }

    #[test]
    fn the_moving_check_is_drawn_again_at_the_compositor_s_next_frame_not_on_a_timer() {
        let now = Instant::now();
        let mut pro = corner(Side::Remote, Status::Home, now);
        pro.update(Update::Mic(Status::Remote), now);
        pro.update(Update::Say("Mic → remote".into()), now);
        // A timer of its own drifts against the monitor's refresh: now and then two steps land
        // in one refresh and one is never shown.
        for step in 0..21 {
            let at = now + CHECK * step / 21;
            assert_eq!(pro.next_draw(at), Some(Next::Frame), "step {step}");
        }
        // The message alone still waits its time.
        assert_eq!(
            pro.next_draw(now + CHECK),
            Some(Next::After(MESSAGE - CHECK))
        );
    }

    #[test]
    fn a_message_shows_for_two_seconds() {
        let now = Instant::now();
        let mut pro = corner(Side::Remote, Status::Remote, now);
        pro.update(Update::Say("Keyboard → home".into()), now);
        assert_eq!(pro.line(now).as_deref(), Some("Keyboard → home"));
        assert_eq!(pro.next_draw(now), Some(Next::After(MESSAGE)));
        assert_eq!(pro.line(now + MESSAGE), None);
        assert_eq!(pro.next_draw(now + MESSAGE), None);
    }

    #[test]
    fn a_long_message_widens_the_corner_and_is_cut() {
        let now = Instant::now();
        let mut pro = corner(Side::Remote, Status::Remote, now);
        let why = format!("Mic not switched: {}", "x".repeat(200));
        pro.update(Update::Say(why), now);
        let line = pro.line(now).unwrap();
        assert_eq!(line.chars().count(), LONGEST);
        assert!(line.ends_with('…'));
        assert!(pro.size(now).0 as f64 >= text_width(&line));
    }

    #[test]
    fn hovering_the_button_shows_the_hotkey() {
        let now = Instant::now();
        let mut pro = corner(Side::Remote, Status::Remote, now);
        assert_eq!(pro.line(now), None);
        assert!(pro.hover(true));
        assert!(!pro.hover(true), "nothing new to draw");
        assert_eq!(pro.line(now).as_deref(), Some("SUPER + CTRL + M"));
        assert!(pro.hover(false));
        assert_eq!(pro.line(now), None);
    }

    #[test]
    fn with_no_mic_there_is_no_button() {
        let now = Instant::now();
        let pro = corner(Side::Remote, Status::Off, now);
        assert_eq!(pro.button(), None);
        assert!(pro.tag().is_some());
    }

    #[test]
    fn a_monitor_at_twice_the_scale_has_twice_the_pixels() {
        let now = Instant::now();
        let pro = corner(Side::Remote, Status::Remote, now);
        let (one, two) = (pro.draw(now, 1.0), pro.draw(now, 2.0));
        // The corner grows with its tag: its size is compared with itself, not pinned.
        assert_eq!((two.width, two.height), (one.width * 2, one.height * 2));
    }

    #[test]
    fn the_bar_shows_the_files_crossing_with_what_is_left_and_goes_after() {
        let now = Instant::now();
        let mut pro = corner(Side::Remote, Status::Remote, now);
        let files = Bar {
            id: 1,
            done: 1 << 30,
            total: 3 << 30,
            headline: None,
        };
        pro.update(Update::Files(vec![files.clone()]), now);
        assert_eq!(bar_text(&files), "2.0 GiB x");
        let (bar, _) = pro.bars()[0];
        let (width, height) = pro.size(now);
        assert!(f64::from(width) >= bar.x + bar.width && f64::from(height) >= bar.y + bar.height);
        let drawn = pro.draw(now, 1.0);
        let blue = |(r, g, b, a): (u8, u8, u8, u8)| a > 0 && b > r + 60 && b > g + 30;
        let (y, left, right) = (bar.y + 2.0, bar.x + 4.0, bar.x + bar.width - 4.0);
        assert!(blue(color(&drawn, left as u32, y as u32)), "a third done");
        assert!(
            !blue(color(&drawn, right as u32, y as u32)),
            "the rest to go"
        );
        // A message goes under the bar, not over it.
        pro.update(Update::Say("Mic → remote".into()), now);
        assert!(pro.line_y() >= bar.y + bar.height);

        pro.update(Update::Files(Vec::new()), now);
        assert_eq!(pro.bars(), []);
    }

    #[test]
    fn several_transfers_do_not_grow_the_corner_past_the_bound() {
        let now = Instant::now();
        let bare = corner(Side::Remote, Status::Remote, now);
        let (bare_width, bare_height) = bare.size(now);
        // Two pastes of a 2 GiB file, then a third: the bound is the corner without bars for
        // the width, and for the height too up to two transfers, one more row after.
        let transfer = |id, done_gib: u64, name: &str, more| Bar {
            id,
            done: done_gib << 30,
            total: 2 << 30,
            headline: Some(Headline {
                name: name.into(),
                more,
            }),
        };
        let all = [
            transfer(1, 1, "big-backup-of-the-year.tar", 0),
            transfer(2, 0, "photos", 3),
            transfer(3, 2, "a.txt", 0),
        ];
        // No wider than the longest bar there is, plus the corner's padding on both sides.
        let longest = 2.0 * PAD + text_width(&"M".repeat(BAR_CHARS)) + 2.0 * 6.0;
        for count in 1..=BARS_SHOWN {
            let mut pro = bare_clone(now);
            pro.update(Update::Files(all[..count].to_vec()), now);
            let (width, height) = pro.size(now);
            assert!(
                width <= bare_width.max(longest.ceil() as u32),
                "{count} transfers: {width} wider than the {BAR_CHARS}-character bar"
            );
            let bound = if count <= 2 {
                bare_height
            } else {
                bare_height + (BAR_ROW + BAR_GAP).ceil() as u32
            };
            assert!(height <= bound, "{count} transfers: {height} > {bound}");

            // Each one is still there to click: inside the corner, clear of the others and of
            // the tag row, and named by its own click.
            let bars = pro.bars();
            assert_eq!(bars.len(), count);
            let row_bottom = PAD + ROW;
            let mut above = row_bottom;
            for (rect, bar) in &bars {
                assert!(rect.y >= above, "{count} transfers: bars overlap");
                above = rect.y + rect.height;
                assert!(rect.x + rect.width <= f64::from(width), "inside the corner");
                assert!(above <= f64::from(height), "inside the corner");
                pro.point_at(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
                assert_eq!(pro.press(), Press::Cancel(bar.id));
                pro.point_at(rect.x + 1.0, rect.y + rect.height - 1.0);
                assert_eq!(pro.press(), Press::Cancel(bar.id), "its corners too");
            }
            // The room the surface is given holds them.
            let (room_width, room_height) = pro.room();
            assert!(room_width >= width && room_height >= height);
        }
    }

    #[test]
    fn the_bars_never_cover_the_mic_button() {
        let now = Instant::now();
        let mut pro = bare_clone(now);
        let bar = |id| Bar {
            id,
            done: 1,
            total: 3,
            headline: None,
        };
        pro.update(Update::Files((1..=3).map(bar).collect()), now);
        let button = pro.button().unwrap();
        for (rect, _) in pro.bars() {
            assert!(rect.y >= button.y + button.height, "under the button");
        }
    }

    fn named(name: &str, more: usize, left_gib: u64) -> Bar {
        Bar {
            id: 1,
            done: 0,
            total: left_gib << 30,
            headline: Some(Headline {
                name: name.into(),
                more,
            }),
        }
    }

    #[test]
    fn a_bar_names_its_copy_and_keeps_the_size_and_the_cross_whole() {
        // Fits: the name, then what is left, then the cross.
        assert_eq!(bar_text(&named("notes.txt", 0, 2)), "notes.txt 2.0 GiB x");
        assert_eq!(bar_text(&named("photos", 3, 2)), "photos +3 2.0 GiB x");
        // Cut with `…`, within twenty characters; the size and the cross stay.
        let long = bar_text(&named("holiday-videos-2026.mkv", 0, 2));
        assert_eq!(long, "holiday-v… 2.0 GiB x");
        let with_more = bar_text(&named("holiday-videos-2026", 12, 2));
        assert_eq!(with_more, "holid… +12 2.0 GiB x");
        for text in [&long, &with_more] {
            assert!(text.chars().count() <= BAR_CHARS, "{text}");
            assert!(text.ends_with("2.0 GiB x"));
        }
        // A copy whose list is not known names nothing: the bar says the size and the cross only.
        let mut unnamed = named("x", 0, 2);
        unnamed.headline = None;
        assert_eq!(bar_text(&unnamed), "2.0 GiB x");
    }

    #[test]
    fn the_text_of_a_bar_stays_inside_the_bar() {
        // The same bar named with letters that hang below the line, and with letters that do
        // not: whatever differs between the two is the descenders, and none may fall outside.
        let now = Instant::now();
        for scale in [1.0, 2.0, 3.0] {
            let drawn = |name: &str| {
                let mut pro = bare_clone(now);
                pro.update(Update::Files(vec![named(name, 0, 1)]), now);
                (pro.bars()[0].0, pro.draw(now, scale))
            };
            let (bar, hanging) = drawn("gypqgypq");
            let (_, level) = drawn("AAAAAAAA");
            assert_eq!((hanging.width, hanging.height), (level.width, level.height));
            let inside = |x: u32, y: u32| {
                let (x, y) = (x as f64 / scale, y as f64 / scale);
                x >= bar.x && x < bar.x + bar.width && y >= bar.y && y < bar.y + bar.height
            };
            for y in 0..hanging.height {
                for x in 0..hanging.width {
                    assert!(
                        inside(x, y) || hanging.at(x, y) == level.at(x, y),
                        "scale {scale}: the text drew at ({x}, {y}), outside the bar {bar:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn all_bars_share_the_width_of_the_longest_text() {
        let now = Instant::now();
        let mut pro = bare_clone(now);
        let short = named("a", 0, 1);
        let long = Bar {
            id: 2,
            ..named("holiday-videos-2026.mkv", 0, 1)
        };
        pro.update(Update::Files(vec![short, long.clone()]), now);
        let rects = pro.bars();
        assert_eq!(rects[0].0.width, rects[1].0.width);
        let wanted = text_width(&bar_text(&long)) + 2.0 * 6.0;
        assert_eq!(rects[0].0.width, wanted.max(pro.row_width()));
        assert!(rects[0].0.width > pro.row_width(), "wider than the tag row");
    }

    fn bare_clone(now: Instant) -> Corner {
        corner(Side::Remote, Status::Remote, now)
    }

    #[test]
    fn a_click_on_a_bar_stops_its_transfer_and_anywhere_else_switches_the_mic() {
        let now = Instant::now();
        let board = Arc::new(Board::default());
        let (clicked, clicks) = std::sync::mpsc::channel();
        let clicked = Mutex::new(clicked);
        board.on_click(move || clicked.lock().unwrap().send(()).unwrap());
        let mut cancels = board.cancels();
        let feed = Feed::new(board, None, Look::default());
        let mut pro = feed.corner(Side::Remote);
        pro.update(Update::Mic(Status::Remote), now);
        let bar = |id| Bar {
            id,
            done: 0,
            total: 10,
            headline: None,
        };
        pro.update(Update::Files(vec![bar(5), bar(9)]), now);
        let (second, button) = (pro.bars()[1].0, pro.button().unwrap());
        assert!(
            second.y >= pro.bars()[0].0.y + BAR_ROW,
            "one under the other"
        );

        pro.point_at(second.x + second.width / 2.0, second.y + 4.0);
        assert_eq!(pro.press(), Press::Cancel(9));
        feed.press(pro.press());
        assert!(cancels.has_changed().unwrap(), "a transfer is stopped");
        assert_eq!(cancels.borrow_and_update().transfer, 9, "the one clicked");

        pro.point_at(button.x + 4.0, button.y + 4.0);
        assert_eq!(pro.press(), Press::Mic);
        feed.press(pro.press());
        clicks
            .recv_timeout(Duration::from_secs(5))
            .expect("the mic switched");
        assert!(!cancels.has_changed().unwrap(), "the files go on");
    }

    #[test]
    fn a_corner_opened_while_files_cross_shows_their_bar() {
        let board = Board::default();
        let files = vec![Bar {
            id: 1,
            done: 1,
            total: 2,
            headline: None,
        }];
        board.files(files.clone());
        let heard = listening(&board);
        assert_eq!(
            *heard[0].lock().unwrap(),
            [Update::Mic(Status::Off), Update::Files(files)]
        );
        board.files(Vec::new());
        assert_eq!(
            heard[1].lock().unwrap().last(),
            Some(&Update::Files(Vec::new()))
        );
    }

    /// Two corners listening to `board`, the updates each heard, as a corner would take them.
    fn listening(board: &Board) -> Vec<Arc<Mutex<Vec<Update>>>> {
        (0..2)
            .map(|_| {
                let heard = Arc::new(Mutex::new(Vec::new()));
                let into = heard.clone();
                board.listen(move |update| {
                    into.lock().unwrap().push(update.clone());
                    true
                });
                heard
            })
            .collect()
    }

    #[tokio::test]
    async fn a_switch_from_the_hotkey_or_the_cli_moves_the_check_in_every_corner() {
        let board = Arc::new(Board::default());
        let (status, watched) = watch::channel(Status::Remote);
        let following = tokio::spawn(follow(board.clone(), watched));
        tokio::task::yield_now().await;
        let heard = listening(&board);
        let now = Instant::now();
        let mut corners = [
            corner(Side::Remote, Status::Off, now),
            corner(Side::Home, Status::Off, now),
        ];
        // The hotkey and `navette mic toggle` both end in the mic's status.
        status.send(Status::Home).unwrap();
        for _ in 0..50 {
            if heard.iter().all(|h| h.lock().unwrap().len() == 2) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for (corner, heard) in corners.iter_mut().zip(&heard) {
            assert_eq!(
                *heard.lock().unwrap(),
                [Update::Mic(Status::Remote), Update::Mic(Status::Home)]
            );
            for update in heard.lock().unwrap().drain(..) {
                corner.update(update, now);
            }
        }
        let done = now + CHECK;
        assert_eq!(corners[0].check_at(done), 0.0, "out on the remote's corner");
        assert_eq!(corners[1].check_at(done), 1.0, "in on home's");
        following.abort();
    }

    #[test]
    fn what_the_mic_says_is_said_in_every_corner_and_a_closed_corner_hears_no_more() {
        let board = Board::default();
        let heard = listening(&board);
        board.show("Mic not switched", "no mode is running");
        board.show("Mic → home", "");
        let said = [
            Update::Mic(Status::Off),
            Update::Say("Mic not switched: no mode is running".into()),
            Update::Say("Mic → home".into()),
        ];
        for heard in &heard {
            assert_eq!(*heard.lock().unwrap(), said);
        }
        board.listen(|_| false);
        assert_eq!(board.inner.lock().unwrap().listeners.len(), 2);
    }

    #[test]
    fn a_click_runs_the_toggle_off_the_corner_s_thread() {
        let board = Arc::new(Board::default());
        let (clicked, heard) = std::sync::mpsc::channel();
        let clicked = Mutex::new(clicked);
        board.on_click(move || {
            let _ = clicked.lock().unwrap().send(std::thread::current().id());
        });
        board.click();
        let on = heard.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_ne!(on, std::thread::current().id());
    }

    #[test]
    fn every_glyph_is_eight_rows_of_five_dots() {
        for (c, rows) in FONT {
            let rows: Vec<&str> = rows.split(' ').collect();
            assert_eq!(rows.len(), 8, "{c:?}");
            assert!(rows.iter().all(|r| r.len() == 5), "{c:?}");
        }
        for c in ' '..='~' {
            assert!(FONT.iter().any(|(k, _)| *k == c), "{c:?} is missing");
        }
    }
}
