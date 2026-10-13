//! The real PipeWire graph, on a thread of its own that runs PipeWire's loop: [`PipeWire`]
//! sends it commands and waits for the answers. It connects on first use, and again after
//! PipeWire restarts.
//!
//! The remote's sink is a stream with an `Audio/Sink` media class and no autoconnect, the way
//! PipeWire's own loopback module makes a virtual sink: the session manager lists it among the
//! outputs and links the apps to it. The default output is the `default.configured.audio.sink`
//! key of the `default` metadata, the one `wpctl set-default` writes.
//!
//! Like any sink, it applies its volume and mute to what it receives, which is the remote's
//! mixer acting on it. The sound that goes home is read from its monitor instead, which carries
//! it as the apps played it, and the volume and mute the sink holds are told to home by
//! [`Mixer`], to be applied to home's playback once.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::spa;
use pw::types::ObjectType;

use super::codec::{CHANNELS, RATE, SAMPLES_PER_CYCLE};
use super::jitter::Jitter;
use super::{DefaultNode, Device, Graph, Level, OnLevel, OnSound, lock};

const CONFIGURED_SINK: &str = "default.configured.audio.sink";
const CONFIGURED_SOURCE: &str = "default.configured.audio.source";
/// The source the session manager gives the apps: the configured one, or its own pick.
const SOURCE_IN_USE: &str = "default.audio.source";
/// Long enough for a busy PipeWire, short enough that a hung one does not hang the link.
const ANSWER: Duration = Duration::from_secs(5);
const BYTES_PER_FRAME: usize = CHANNELS * size_of::<f32>();

type Reply<T> = std_mpsc::Sender<io::Result<T>>;
/// The changes waiting for the server, by the sync that follows them.
type Pending = Rc<RefCell<Vec<(spa::utils::result::AsyncSeq, Reply<()>)>>>;

enum Command {
    DefaultNode(Device, Reply<Option<DefaultNode>>),
    SetDefaultNode(Device, Option<DefaultNode>, Reply<()>),
    MicInUse(Reply<Option<String>>),
    Open {
        name: String,
        description: String,
        what: Opening,
        reply: Reply<u64>,
    },
    Close(u64),
    /// The volume and mute of a playback stream.
    SetLevel(u64, Level),
    /// Ends a loop that stopped answering, and with it every stream it holds.
    Quit,
}

/// What a node of ours is.
enum Opening {
    Sink {
        on_sound: OnSound,
        on_level: OnLevel,
    },
    Playback(Arc<Mutex<Jitter>>),
    Source(Arc<Mutex<Jitter>>),
    Mic {
        mic: String,
        on_sound: OnSound,
    },
}

/// The way to one PipeWire loop: its commands, and whether it still runs.
#[derive(Clone)]
struct Loop {
    commands: Arc<dyn Fn(Command) -> Result<(), Command> + Send + Sync>,
    running: Arc<AtomicBool>,
}

impl Loop {
    /// Gives the command back when the loop has ended.
    fn send(&self, command: Command) -> Result<(), Command> {
        if !self.running.load(Ordering::Acquire) {
            return Err(command);
        }
        (self.commands)(command)
    }
}

type Connect = Arc<dyn Fn() -> io::Result<Loop> + Send + Sync>;

#[derive(Clone)]
pub struct PipeWire {
    current: Arc<Mutex<Option<Loop>>>,
    connect: Connect,
    answer: Duration,
}

impl Default for PipeWire {
    fn default() -> Self {
        Self::with(Arc::new(connect), ANSWER)
    }
}

/// A stream of ours in the graph; removed when dropped. It keeps the loop it was opened on:
/// its id means nothing to the loop of a later connection.
pub struct Node {
    on: Loop,
    id: u64,
}

impl Drop for Node {
    fn drop(&mut self) {
        // An ended loop took its streams with it.
        let _ = self.on.send(Command::Close(self.id));
    }
}

impl PipeWire {
    fn with(connect: Connect, answer: Duration) -> Self {
        Self {
            current: Arc::default(),
            connect,
            answer,
        }
    }

    /// Sends `command` to the current loop and waits for its answer. A loop that has ended
    /// (PipeWire restarted) gives the command back, which goes once more to a new connection.
    fn call<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> io::Result<(T, Loop)> {
        let mut current = lock(&self.current);
        let (reply, answer) = std_mpsc::channel();
        let mut command = command(reply);
        for _ in 0..2 {
            let pipewire = match current.as_ref() {
                Some(pipewire) => pipewire.clone(),
                None => current.insert((self.connect)()?).clone(),
            };
            match pipewire.send(command) {
                Ok(()) => {
                    return match answer.recv_timeout(self.answer) {
                        Ok(result) => result.map(|value| (value, pipewire)),
                        Err(_) => {
                            // Hung, or ended with the command: if it ever wakes up, it must not
                            // come back with a second sink.
                            let _ = pipewire.send(Command::Quit);
                            *current = None;
                            Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "PipeWire did not answer",
                            ))
                        }
                    };
                }
                Err(returned) => {
                    command = returned;
                    *current = None;
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "PipeWire ended twice in a row",
        ))
    }

    fn open(&self, name: &str, description: &str, what: Opening) -> io::Result<Node> {
        let (id, on) = self.call(|reply| Command::Open {
            name: name.to_owned(),
            description: description.to_owned(),
            what,
            reply,
        })?;
        Ok(Node { on, id })
    }
}

impl Graph for PipeWire {
    type Node = Node;

    fn default_node(&self, device: Device) -> io::Result<Option<DefaultNode>> {
        Ok(self.call(|reply| Command::DefaultNode(device, reply))?.0)
    }

    fn set_default_node(&self, device: Device, node: Option<&DefaultNode>) -> io::Result<()> {
        self.call(|reply| Command::SetDefaultNode(device, node.cloned(), reply))
            .map(drop)
    }

    fn open_sink(
        &self,
        name: &str,
        description: &str,
        on_sound: OnSound,
        on_level: OnLevel,
    ) -> io::Result<Node> {
        self.open(name, description, Opening::Sink { on_sound, on_level })
    }

    fn set_level(&self, node: &Node, level: Level) -> io::Result<()> {
        node.on
            .send(Command::SetLevel(node.id, level))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "PipeWire has ended"))
    }

    fn open_playback(
        &self,
        name: &str,
        description: &str,
        jitter: Arc<Mutex<Jitter>>,
    ) -> io::Result<Node> {
        self.open(name, description, Opening::Playback(jitter))
    }

    fn open_source(
        &self,
        name: &str,
        description: &str,
        jitter: Arc<Mutex<Jitter>>,
    ) -> io::Result<Node> {
        self.open(name, description, Opening::Source(jitter))
    }

    fn mic_in_use(&self) -> io::Result<Option<String>> {
        Ok(self.call(Command::MicInUse)?.0)
    }

    fn open_mic(
        &self,
        name: &str,
        description: &str,
        mic: &str,
        on_sound: OnSound,
    ) -> io::Result<Node> {
        let mic = mic.to_owned();
        self.open(name, description, Opening::Mic { mic, on_sound })
    }

    fn is_gone(&self, node: &Node) -> bool {
        !node.on.running.load(Ordering::Acquire)
    }
}

fn pw_error(e: pw::Error) -> io::Error {
    io::Error::other(format!("PipeWire: {e}"))
}

/// Starts PipeWire's thread, once it has connected and read the default metadata.
fn connect() -> io::Result<Loop> {
    let (sender, receiver) = pw::channel::channel();
    let (ready, started) = std_mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    std::thread::Builder::new()
        .name("navette-pipewire".into())
        .spawn({
            let running = running.clone();
            move || {
                if let Err(e) = run(receiver, &ready) {
                    let _ = ready.send(Err(e));
                }
                running.store(false, Ordering::Release);
            }
        })?;
    started
        .recv_timeout(ANSWER)
        .unwrap_or_else(|_| Err(io::Error::other("PipeWire did not answer")))?;
    Ok(Loop {
        commands: Arc::new(move |command| sender.send(command)),
        running,
    })
}

/// A stream of ours, its listener dropped first so it unhooks from a live stream, and the
/// module it records from, if any, last.
struct Open {
    _listener: Box<dyn std::any::Any>,
    stream: pw::stream::StreamRc,
    _module: Option<Module>,
    /// The sink's monitor, that its sound is read from.
    _monitor: Option<Box<Open>>,
}

/// A module loaded into this process's own context: its nodes live here, and go when it is
/// destroyed, or when the process ends, crash included. It holds the context, which must
/// outlive it.
struct Module {
    module: std::ptr::NonNull<pw::sys::pw_impl_module>,
    _context: pw::context::ContextRc,
}

impl Drop for Module {
    fn drop(&mut self) {
        // SAFETY: `module` came from `pw_context_load_module` on `_context`, which this value
        // keeps alive until after this call; it is destroyed only here, once, on the loop's
        // thread (`Module` is not `Send`).
        unsafe { pw::sys::pw_impl_module_destroy(self.module.as_ptr()) }
    }
}

fn load_module(context: &pw::context::ContextRc, name: &str, args: &str) -> io::Result<Module> {
    let (name, args) = (
        std::ffi::CString::new(name).map_err(io::Error::other)?,
        std::ffi::CString::new(args).map_err(io::Error::other)?,
    );
    // SAFETY: the context is alive and used from its loop's thread; the strings are copied by
    // the call; no properties are passed.
    let module = unsafe {
        pw::sys::pw_context_load_module(
            context.as_raw_ptr(),
            name.as_ptr(),
            args.as_ptr(),
            std::ptr::null_mut(),
        )
    };
    std::ptr::NonNull::new(module)
        .map(|module| Module {
            module,
            _context: context.clone(),
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "PipeWire could not load {}: {}",
                    name.to_string_lossy(),
                    io::Error::last_os_error()
                ),
            )
        })
}

fn run(commands: pw::channel::Receiver<Command>, ready: &Reply<()>) -> io::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_error)?;
    let core = context.connect_rc(None).map_err(pw_error)?;
    let registry = core.get_registry_rc().map_err(pw_error)?;

    // PipeWire gone (restarted, say): the loop ends, and the next call connects again.
    let failed = Rc::new(Cell::new(false));
    let _core_listener = core
        .add_listener_local()
        .error({
            let (failed, mainloop) = (failed.clone(), mainloop.clone());
            move |id, _seq, _res, message| {
                if id == pw::core::PW_ID_CORE {
                    eprintln!("navette: audio: PipeWire: {message}");
                    failed.set(true);
                    mainloop.quit();
                }
            }
        })
        .register();

    let metadata: Rc<RefCell<Option<(Metadata, MetadataListener)>>> = Rc::default();
    let configured: Rc<RefCell<Configured>> = Rc::default();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let (registry, metadata, configured) =
                (registry.downgrade(), metadata.clone(), configured.clone());
            move |global| {
                let is_default = global.type_ == ObjectType::Metadata
                    && global.props.and_then(|p| p.get("metadata.name")) == Some("default");
                let Some(registry) = registry.upgrade().filter(|_| is_default) else {
                    return;
                };
                let Ok(bound) = registry.bind::<Metadata, _>(global) else {
                    return;
                };
                let configured = configured.clone();
                let listener = bound
                    .add_listener_local()
                    .property(move |subject, key, _type, value| {
                        follow(&mut configured.borrow_mut(), subject, key, value);
                        0
                    })
                    .register();
                *metadata.borrow_mut() = Some((bound, listener));
            }
        })
        .register();
    // The globals, then the properties of the metadata bound on the way.
    roundtrip(&core, &mainloop, &failed)?;
    roundtrip(&core, &mainloop, &failed)?;
    let _ = ready.send(Ok(()));

    // Answered once the server has taken the change, so that an agent ending right after
    // does not leave it unsent.
    let pending: Pending = Rc::default();
    let _done_listener = core
        .add_listener_local()
        .done({
            let pending = pending.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE {
                    pending.borrow_mut().retain(|(waiting, reply)| {
                        let answered = *waiting == seq;
                        if answered {
                            let _ = reply.send(Ok(()));
                        }
                        !answered
                    });
                }
            }
        })
        .register();
    let streams: Rc<RefCell<HashMap<u64, Open>>> = Rc::default();
    let quit = mainloop.clone();
    let next_id = Cell::new(0u64);
    let _commands = commands.attach(mainloop.loop_(), move |command| match command {
        Command::DefaultNode(device, reply) => {
            let value = configured.borrow_mut().of(device).clone();
            let _ = reply.send(Ok(value.map(DefaultNode)));
        }
        Command::MicInUse(reply) => {
            let value = configured.borrow().source_in_use.clone();
            let _ = reply.send(Ok(
                value.and_then(|v| DefaultNode(v).name().map(str::to_owned))
            ));
        }
        Command::SetDefaultNode(device, node, reply) => {
            let result = match metadata.borrow().as_ref() {
                Some((metadata, _)) => {
                    let value = node.map(|node| node.0);
                    metadata.set_property(
                        pw::core::PW_ID_CORE,
                        key(device),
                        value.as_ref().map(|_| "Spa:String:JSON"),
                        value.as_deref(),
                    );
                    // The server echoes it back; until then, this is what it holds.
                    *configured.borrow_mut().of(device) = value;
                    core.sync(0).map_err(pw_error)
                }
                None => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "PipeWire has no default metadata: is WirePlumber running?",
                )),
            };
            match result {
                Ok(seq) => pending.borrow_mut().push((seq, reply)),
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            }
        }
        Command::Open {
            name,
            description,
            what,
            reply,
        } => {
            let id = next_id.replace(next_id.get() + 1);
            let opened = match what {
                Opening::Sink { on_sound, on_level } => {
                    open_sink(&core, &name, &description, on_sound, on_level)
                }
                Opening::Playback(jitter) => open_playback(&core, &name, &description, jitter),
                Opening::Source(jitter) => open_source(&core, &name, &description, jitter),
                Opening::Mic { mic, on_sound } => {
                    open_mic(&context, &core, &name, &description, &mic, on_sound)
                }
            };
            let result = opened.map(|open| {
                streams.borrow_mut().insert(id, open);
                id
            });
            let _ = reply.send(result);
        }
        Command::Close(id) => {
            streams.borrow_mut().remove(&id);
        }
        Command::SetLevel(id, level) => {
            if let Some(open) = streams.borrow().get(&id) {
                set_level(&open.stream, level);
            }
        }
        Command::Quit => quit.quit(),
    });
    if !failed.get() {
        mainloop.run();
    }
    Ok(())
}

/// The configured defaults, and the source in use, as the `default` metadata last said them.
#[derive(Debug, Default, PartialEq)]
struct Configured {
    sink: Option<String>,
    source: Option<String>,
    source_in_use: Option<String>,
}

impl Configured {
    fn of(&mut self, device: Device) -> &mut Option<String> {
        match device {
            Device::Sink => &mut self.sink,
            Device::Source => &mut self.source,
        }
    }
}

fn key(device: Device) -> &'static str {
    match device {
        Device::Sink => CONFIGURED_SINK,
        Device::Source => CONFIGURED_SOURCE,
    }
}

/// Follows the changes of the `default` metadata to the configured defaults.
/// Only the core's keys hold the defaults; other subjects are nodes with metadata of their own.
fn follow(configured: &mut Configured, subject: u32, key: Option<&str>, value: Option<&str>) {
    if subject != pw::core::PW_ID_CORE {
        return;
    }
    match key {
        Some(CONFIGURED_SINK) => configured.sink = value.map(str::to_owned),
        Some(CONFIGURED_SOURCE) => configured.source = value.map(str::to_owned),
        Some(SOURCE_IN_USE) => configured.source_in_use = value.map(str::to_owned),
        // Every key of the core cleared.
        None => *configured = Configured::default(),
        _ => {}
    }
}

/// Runs the loop until the server has answered everything asked so far.
fn roundtrip(
    core: &pw::core::CoreRc,
    mainloop: &pw::main_loop::MainLoopRc,
    failed: &Rc<Cell<bool>>,
) -> io::Result<()> {
    let done = Rc::new(Cell::new(false));
    let pending = core.sync(0).map_err(pw_error)?;
    let _listener = core
        .add_listener_local()
        .done({
            let (done, mainloop) = (done.clone(), mainloop.clone());
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done.set(true);
                    mainloop.quit();
                }
            }
        })
        .register();
    while !done.get() && !failed.get() {
        mainloop.run();
    }
    if failed.get() {
        return Err(io::Error::other("PipeWire closed the connection"));
    }
    Ok(())
}

/// Interleaved F32 stereo at 48 kHz: the format of both streams, converted by PipeWire.
fn format() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .expect("a fixed audio format serializes")
    .0
    .into_inner()
}

/// The graph cycle Navette's nodes ask for, in samples.
const CYCLE: u32 = 512;

fn properties(name: &str, description: &str) -> pw::properties::PropertiesBox {
    pw::properties::properties! {
        *pw::keys::NODE_NAME => name,
        *pw::keys::NODE_DESCRIPTION => description,
        *pw::keys::MEDIA_NAME => description,
        *pw::keys::APP_NAME => "Navette",
        // Cycles of 512 samples (11 ms) at most. PipeWire rounds a latency down to a power of
        // two: asking for a 10 ms frame (480) ran the whole graph, the user's own apps
        // included, in cycles of 256 (5 ms), short enough for a busy desk to miss some.
        *pw::keys::NODE_LATENCY => format!("{CYCLE}/{RATE}"),
    }
}

fn connect_stream(
    stream: &pw::stream::StreamRc,
    direction: spa::utils::Direction,
    flags: pw::stream::StreamFlags,
) -> io::Result<()> {
    let format = format();
    let mut params = [spa::pod::Pod::from_bytes(&format).expect("a serialized pod")];
    // The callbacks run on PipeWire's realtime thread, not this loop's: they neither block nor
    // allocate, and a loop that is late by a cycle, the desk busy with a build, would cost the
    // graph that cycle, heard as a crackle that no buffer can absorb.
    stream
        .connect(
            direction,
            None,
            flags | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(pw_error)
}

/// The sink's bytes as samples, into `samples`.
fn read_samples(bytes: &[u8], samples: &mut Vec<f32>) {
    samples.clear();
    samples.extend(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b)),
    );
}

/// What the playback plays next, and how fast it should take the sound from now on. A panic
/// on the other side of the lock leaves the buffer usable, and a panic here would cross
/// PipeWire's C code.
fn fill(jitter: &Mutex<Jitter>, out: &mut [f32]) -> f64 {
    let mut jitter = lock(jitter);
    jitter.pull(out);
    jitter.rate()
}

/// The sink's volume and mute as its controls say them, and the level they make. The sink
/// multiplies the sound by the volume and by the gain of each channel, as the remote's mixer
/// (`wpctl`, a volume key, a slider) sets them.
struct Mixer {
    volume: f32,
    channels: [f32; 2],
    muted: bool,
}

impl Mixer {
    fn new() -> Self {
        Self {
            volume: 1.0,
            channels: [1.0; 2],
            muted: false,
        }
    }

    /// The level once the control `id` of the sink took `values`, or `None` for a control that
    /// is not part of it.
    fn follow(&mut self, id: u32, values: &[f32]) -> Option<Level> {
        match id {
            spa::sys::SPA_PROP_volume => self.volume = values.first().copied().unwrap_or(1.0),
            spa::sys::SPA_PROP_mute => self.muted = values.first().is_some_and(|mute| *mute > 0.5),
            // Empty until the sink has its format: no gain is no change of the sound.
            spa::sys::SPA_PROP_channelVolumes => {
                let left = values.first().copied().unwrap_or(1.0);
                self.channels = [left, values.get(1).copied().unwrap_or(left)];
            }
            _ => return None,
        }
        Some(Level {
            left: self.volume * self.channels[0],
            right: self.volume * self.channels[1],
            muted: self.muted,
        })
    }
}

/// The remote's sink: a virtual output whose sound is read from its monitor, with the volume
/// and mute that the sink itself applies told to `on_level`.
fn open_sink(
    core: &pw::core::CoreRc,
    name: &str,
    description: &str,
    on_sound: OnSound,
    on_level: OnLevel,
) -> io::Result<Open> {
    let mut props = properties(name, description);
    props.insert(*pw::keys::MEDIA_CLASS, "Audio/Sink");
    // Not autoconnected: the session manager links the apps to it, as to any sink.
    let stream = pw::stream::StreamRc::new(core.clone(), name, props).map_err(pw_error)?;
    let listener = stream
        .add_local_listener_with_user_data((Mixer::new(), on_level))
        .control_info(|_stream, (mixer, on_level), id, control| {
            // SAFETY: PipeWire hands over a control that lives for the call; a null one is
            // skipped, and `values` holds `n_values` floats.
            let values = unsafe {
                control.as_ref().map(|control| {
                    std::slice::from_raw_parts(control.values, control.n_values as usize)
                })
            };
            if let Some(level) = values.and_then(|values| mixer.follow(id, values)) {
                on_level(level);
            }
        })
        // What it receives is scaled by the mixer: left unread, for the monitor has it whole.
        .process(|stream, _| drop(stream.dequeue_buffer()))
        .register()
        .map_err(pw_error)?;
    connect_stream(
        &stream,
        spa::utils::Direction::Input,
        pw::stream::StreamFlags::empty(),
    )?;

    let mut capture = properties(&format!("{name}-monitor"), description);
    capture.insert(*pw::keys::MEDIA_TYPE, "Audio");
    capture.insert(*pw::keys::MEDIA_CATEGORY, "Capture");
    // The monitor of the sink, not a mic, and never anything else if the sink is not there yet.
    capture.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    capture.insert(*pw::keys::TARGET_OBJECT, name);
    capture.insert(*pw::keys::NODE_DONT_RECONNECT, "true");
    capture.insert("node.dont-fallback", "true");
    // The sink idles while nothing plays into it, as without the monitor.
    capture.insert(*pw::keys::NODE_PASSIVE, "true");
    let monitor = recording(
        core,
        &format!("{name}-monitor"),
        capture,
        pw::stream::StreamFlags::AUTOCONNECT,
        on_sound,
    )?;
    Ok(Open {
        _listener: Box::new(listener),
        stream,
        _module: None,
        _monitor: Some(Box::new(monitor)),
    })
}

/// Sets the volume and mute of a stream of ours, as a mixer would.
fn set_level(stream: &pw::stream::StreamRc, level: Level) {
    let muted = if level.muted { 1.0 } else { 0.0 };
    if stream
        .set_control(
            spa::sys::SPA_PROP_channelVolumes,
            &[level.left, level.right],
        )
        .and_then(|()| stream.set_control(spa::sys::SPA_PROP_mute, &[muted]))
        .is_err()
    {
        eprintln!("navette: audio: could not set the volume of the remote's sound");
    }
}

fn open_playback(
    core: &pw::core::CoreRc,
    name: &str,
    description: &str,
    jitter: Arc<Mutex<Jitter>>,
) -> io::Result<Open> {
    let mut props = properties(name, description);
    props.insert(*pw::keys::MEDIA_TYPE, "Audio");
    props.insert(*pw::keys::MEDIA_CATEGORY, "Playback");
    // The volume is the remote's to say: WirePlumber would put back one it kept from an
    // earlier run.
    props.insert("state.restore-props", "false");
    playing(
        core,
        name,
        props,
        pw::stream::StreamFlags::AUTOCONNECT,
        jitter,
    )
}

/// A virtual mic the way PipeWire's loopback module makes one: a stream with the `Audio/Source`
/// media class, not autoconnected, that the session manager lists among the sources.
fn open_source(
    core: &pw::core::CoreRc,
    name: &str,
    description: &str,
    jitter: Arc<Mutex<Jitter>>,
) -> io::Result<Open> {
    let mut props = properties(name, description);
    props.insert(*pw::keys::MEDIA_CLASS, "Audio/Source");
    playing(core, name, props, pw::stream::StreamFlags::empty(), jitter)
}

/// PipeWire's echo canceller (WebRTC), loaded here: it captures `mic`, named rather than the
/// default, which goes silent while the mic is away, and the monitor of the default output as
/// what to cancel, so that home's own default output is left as it is. Its source is no
/// default: only this stream records from it.
fn echo_cancel_args(name: &str, description: &str, mic: &str) -> String {
    let quoted = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    format!(
        "{{ library.name = aec/libspa-aec-webrtc monitor.mode = true \
         node.description = {description} \
         capture.props = {{ node.name = {capture} node.description = {description} \
         target.object = {mic} }} \
         sink.props = {{ node.name = {reference} node.description = {description} }} \
         source.props = {{ node.name = {source} node.description = {description} \
         priority.session = 0 priority.driver = 0 }} }}",
        description = quoted(description),
        capture = quoted(&format!("{name}-capture")),
        reference = quoted(&format!("{name}-reference")),
        source = quoted(&echo_cancelled(name)),
        mic = quoted(mic),
    )
}

/// The echo canceller's source, which the mic records from.
fn echo_cancelled(name: &str) -> String {
    format!("{name}-echo-cancelled")
}

fn open_mic(
    context: &pw::context::ContextRc,
    core: &pw::core::CoreRc,
    name: &str,
    description: &str,
    mic: &str,
    on_sound: OnSound,
) -> io::Result<Open> {
    let module = load_module(
        context,
        "libpipewire-module-echo-cancel",
        &echo_cancel_args(name, description, mic),
    )?;
    let mut props = properties(name, description);
    props.insert(*pw::keys::MEDIA_TYPE, "Audio");
    props.insert(*pw::keys::MEDIA_CATEGORY, "Capture");
    props.insert(*pw::keys::MEDIA_ROLE, "Communication");
    props.insert(*pw::keys::TARGET_OBJECT, echo_cancelled(name));
    // Never the raw mic in its place: what the remote hears would echo.
    props.insert(*pw::keys::NODE_DONT_RECONNECT, "true");
    let mut open = recording(
        core,
        name,
        props,
        pw::stream::StreamFlags::AUTOCONNECT,
        on_sound,
    )?;
    open._module = Some(module);
    Ok(open)
}

/// A stream that takes sound in and hands it to `on_sound`.
fn recording(
    core: &pw::core::CoreRc,
    name: &str,
    props: pw::properties::PropertiesBox,
    flags: pw::stream::StreamFlags,
    on_sound: OnSound,
) -> io::Result<Open> {
    let stream = pw::stream::StreamRc::new(core.clone(), name, props).map_err(pw_error)?;
    let samples = Vec::with_capacity(SAMPLES_PER_CYCLE);
    let listener = stream
        .add_local_listener_with_user_data((on_sound, samples))
        .process(|stream, (on_sound, samples)| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            if let Some(bytes) = data.data() {
                let end = (offset + size).min(bytes.len());
                read_samples(&bytes[offset.min(end)..end], samples);
                on_sound(samples);
            }
        })
        .register()
        .map_err(pw_error)?;
    connect_stream(&stream, spa::utils::Direction::Input, flags)?;
    Ok(Open {
        _listener: Box::new(listener),
        stream,
        _module: None,
        _monitor: None,
    })
}

/// A stream that plays what `jitter` holds.
fn playing(
    core: &pw::core::CoreRc,
    name: &str,
    props: pw::properties::PropertiesBox,
    flags: pw::stream::StreamFlags,
    jitter: Arc<Mutex<Jitter>>,
) -> io::Result<Open> {
    let stream = pw::stream::StreamRc::new(core.clone(), name, props).map_err(pw_error)?;
    let samples = vec![0.0f32; SAMPLES_PER_CYCLE];
    let listener = stream
        .add_local_listener_with_user_data((jitter, samples, 1.0f32))
        .process(|stream, (jitter, samples, applied)| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let frames = match data.data() {
                Some(bytes) => {
                    let mut frames =
                        (bytes.len() / BYTES_PER_FRAME).min(SAMPLES_PER_CYCLE / CHANNELS);
                    // Only what the graph asks for: a whole buffer would be latency.
                    if requested > 0 {
                        frames = frames.min(requested);
                    }
                    let samples = &mut samples[..frames * CHANNELS];
                    let rate = fill(jitter, samples) as f32;
                    // The stream's resampler takes the sound at this rate, which keeps the
                    // buffer on its target while the two machines' clocks drift.
                    // Within 10 ppm, nothing is told: a fraction of a ms over a minute.
                    if (rate - *applied).abs() >= 1e-5
                        && stream.set_control(spa::sys::SPA_PROP_rate, &[rate]).is_ok()
                    {
                        *applied = rate;
                    }
                    for (out, sample) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(samples.iter())
                    {
                        out.copy_from_slice(&sample.to_le_bytes());
                    }
                    frames
                }
                None => 0,
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = BYTES_PER_FRAME as i32;
            *chunk.size_mut() = (frames * BYTES_PER_FRAME) as u32;
        })
        .register()
        .map_err(pw_error)?;
    connect_stream(&stream, spa::utils::Direction::Output, flags)?;
    Ok(Open {
        _listener: Box::new(listener),
        stream,
        _module: None,
        _monitor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loops that answer every command and log what they got, as `<loop>: <command>`.
    #[derive(Clone, Default)]
    struct FakeLoops {
        log: Arc<Mutex<Vec<String>>>,
        running: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
        /// The loops from this one on never answer a change of the default.
        hung_from: Option<usize>,
    }

    impl FakeLoops {
        fn pipewire(&self) -> PipeWire {
            let loops = self.clone();
            PipeWire::with(
                Arc::new(move || Ok(loops.start())),
                Duration::from_millis(200),
            )
        }

        fn start(&self) -> Loop {
            let n = {
                let mut running = lock(&self.running);
                running.push(Arc::new(AtomicBool::new(true)));
                running.len() - 1
            };
            let running = lock(&self.running)[n].clone();
            let hung = self.hung_from.is_some_and(|from| n >= from);
            let (sender, commands) = std_mpsc::channel::<Command>();
            let log = self.log.clone();
            std::thread::spawn(move || {
                let mut next_id = 0;
                for command in commands {
                    let entry = match command {
                        Command::DefaultNode(_, reply) => {
                            let _ = reply.send(Ok(None));
                            "default?".to_owned()
                        }
                        Command::MicInUse(reply) => {
                            let _ = reply.send(Ok(None));
                            "mic?".to_owned()
                        }
                        Command::SetDefaultNode(_, sink, reply) => {
                            if !hung {
                                let _ = reply.send(Ok(()));
                            }
                            format!("default {}", sink.map_or("cleared".into(), |s| s.0))
                        }
                        Command::Open { reply, .. } => {
                            let _ = reply.send(Ok(next_id));
                            next_id += 1;
                            format!("open {}", next_id - 1)
                        }
                        Command::Close(id) => format!("close {id}"),
                        Command::SetLevel(id, level) => {
                            format!("level {id} {} {} {}", level.left, level.right, level.muted)
                        }
                        Command::Quit => "quit".to_owned(),
                    };
                    lock(&log).push(format!("{n}: {entry}"));
                }
            });
            Loop {
                commands: Arc::new(move |command| sender.send(command).map_err(|e| e.0)),
                running,
            }
        }

        /// PipeWire restarted: loop `n` ended.
        fn end(&self, n: usize) {
            lock(&self.running)[n].store(false, Ordering::Release);
        }

        fn log(&self) -> Vec<String> {
            // The loops log on their own threads, after answering.
            std::thread::sleep(Duration::from_millis(50));
            lock(&self.log).clone()
        }
    }

    #[test]
    fn a_cycle_does_not_allocate_on_either_side() {
        use crate::audio::allocations::during;

        // What the callbacks hold from the start.
        let mut samples = Vec::with_capacity(SAMPLES_PER_CYCLE);
        let mut out = vec![0.0f32; SAMPLES_PER_CYCLE];
        let jitter = Mutex::new(Jitter::new(4, SAMPLES_PER_CYCLE));
        jitter.lock().unwrap().push(&[0.5; 4096]);

        let bytes: Vec<u8> = [0.5f32; 2048]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        for _ in 0..3 {
            assert_eq!(during(|| read_samples(&bytes, &mut samples)), 0);
            assert_eq!(samples.len(), 2048);
            assert_eq!(
                during(|| {
                    fill(&jitter, &mut out[..2048]);
                }),
                0
            );
        }
    }

    #[test]
    fn the_playback_still_plays_after_a_panic_poisoned_its_buffer() {
        let jitter = Arc::new(Mutex::new(Jitter::new(1, 4)));
        jitter.lock().unwrap().push(&[0.5]);
        let poisoner = jitter.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.lock().unwrap();
            panic!("poisons the buffer");
        })
        .join();
        assert!(jitter.is_poisoned());
        let mut out = [0.0];
        fill(&jitter, &mut out);
        assert_eq!(out, [0.5]);
    }

    fn sink(pipewire: &PipeWire) -> io::Result<Node> {
        pipewire.open_sink("navette-test", "Test", Box::new(|_| {}), Box::new(|_| {}))
    }

    #[test]
    fn an_old_node_is_not_closed_on_a_new_loop() {
        let loops = FakeLoops::default();
        let pipewire = loops.pipewire();
        let old = sink(&pipewire).unwrap();
        loops.end(0);
        // The first call after the restart may fail; the one after it reaches a new loop.
        let _ = pipewire.default_node(Device::Sink);
        let new = sink(&pipewire).unwrap();
        drop(old);
        let log = loops.log();
        assert!(log.contains(&"1: open 0".to_owned()), "{log:?}");
        assert!(!log.contains(&"1: close 0".to_owned()), "{log:?}");
        drop(new);
        assert!(loops.log().contains(&"1: close 0".to_owned()));
    }

    #[test]
    fn a_node_on_an_ended_loop_is_gone() {
        let loops = FakeLoops::default();
        let pipewire = loops.pipewire();
        let node = sink(&pipewire).unwrap();
        assert!(!pipewire.is_gone(&node));
        loops.end(0);
        assert!(pipewire.is_gone(&node));
    }

    #[test]
    fn the_default_is_put_back_after_pipewire_restarts() {
        let loops = FakeLoops::default();
        let pipewire = loops.pipewire();
        pipewire.default_node(Device::Sink).unwrap();
        loops.end(0);
        let previous = DefaultNode::named("invented-speakers");
        pipewire
            .set_default_node(Device::Sink, Some(&previous))
            .unwrap();
        assert!(loops.log().contains(&format!("1: default {}", previous.0)));
    }

    #[test]
    fn a_loop_that_does_not_answer_is_told_to_quit() {
        let loops = FakeLoops {
            hung_from: Some(0),
            ..FakeLoops::default()
        };
        let pipewire = loops.pipewire();
        assert!(pipewire.set_default_node(Device::Sink, None).is_err());
        assert!(loops.log().contains(&"0: quit".to_owned()));
    }

    #[test]
    fn the_level_is_the_volume_times_the_gain_of_each_channel() {
        use spa::sys::{SPA_PROP_channelVolumes, SPA_PROP_mute, SPA_PROP_volume};

        let mut mixer = Mixer::new();
        // `wpctl set-volume 0.5` writes the gains, cubed: 0.125.
        assert_eq!(
            mixer.follow(SPA_PROP_channelVolumes, &[0.125, 0.5]),
            Some(Level {
                left: 0.125,
                right: 0.5,
                muted: false
            })
        );
        assert_eq!(
            mixer.follow(SPA_PROP_volume, &[0.5]),
            Some(Level {
                left: 0.0625,
                right: 0.25,
                muted: false
            })
        );
        assert_eq!(
            mixer.follow(SPA_PROP_mute, &[1.0]),
            Some(Level {
                left: 0.0625,
                right: 0.25,
                muted: true
            })
        );
        assert_eq!(
            mixer.follow(SPA_PROP_mute, &[0.0]).map(|l| l.muted),
            Some(false)
        );
    }

    #[test]
    fn a_sink_that_has_not_its_format_yet_is_at_unity() {
        use spa::sys::SPA_PROP_channelVolumes;

        let mut mixer = Mixer::new();
        // PipeWire reports the gains with no value before the sink has channels.
        assert_eq!(
            mixer.follow(SPA_PROP_channelVolumes, &[]),
            Some(Level::UNITY)
        );
        // One channel's gain is both channels'.
        assert_eq!(
            mixer
                .follow(SPA_PROP_channelVolumes, &[0.25])
                .map(|l| (l.left, l.right)),
            Some((0.25, 0.25))
        );
    }

    #[test]
    fn a_control_that_is_not_the_volume_says_nothing() {
        let mut mixer = Mixer::new();
        // The sink's soft volumes, monitor volumes and the like: not what it does to the sound.
        for id in [
            spa::sys::SPA_PROP_softMute,
            spa::sys::SPA_PROP_softVolumes,
            spa::sys::SPA_PROP_monitorMute,
            spa::sys::SPA_PROP_monitorVolumes,
        ] {
            assert_eq!(mixer.follow(id, &[0.3, 0.3]), None);
        }
    }

    #[test]
    fn the_level_of_a_playback_reaches_its_stream_on_the_loop_that_holds_it() {
        let loops = FakeLoops::default();
        let pipewire = loops.pipewire();
        let node = pipewire
            .open_playback(
                "navette-test",
                "Test",
                Arc::new(Mutex::new(Jitter::new(1, 4))),
            )
            .unwrap();
        let level = Level {
            left: 0.125,
            right: 0.125,
            muted: true,
        };
        pipewire.set_level(&node, level).unwrap();
        assert!(
            loops
                .log()
                .contains(&"0: level 0 0.125 0.125 true".to_owned()),
            "{:?}",
            loops.log()
        );

        // The loop is gone with PipeWire: nothing to set it on.
        loops.end(0);
        assert!(pipewire.set_level(&node, level).is_err());
    }

    const SPEAKERS: &str = "{ \"name\": \"invented-speakers\" }";

    #[test]
    fn the_configured_sink_follows_its_key_on_the_core() {
        let mut configured = Configured::default();
        follow(&mut configured, 0, Some(CONFIGURED_SINK), Some(SPEAKERS));
        assert_eq!(configured.sink.as_deref(), Some(SPEAKERS));
        follow(&mut configured, 0, Some("default.audio.sink"), None);
        assert_eq!(configured.sink.as_deref(), Some(SPEAKERS));
        follow(&mut configured, 0, None, None);
        assert_eq!(configured, Configured::default());
    }

    const MIC: &str = "{ \"name\": \"invented-mic\" }";

    #[test]
    fn the_configured_source_follows_its_own_key() {
        let mut configured = Configured::default();
        follow(&mut configured, 0, Some(CONFIGURED_SINK), Some(SPEAKERS));
        follow(&mut configured, 0, Some(CONFIGURED_SOURCE), Some(MIC));
        assert_eq!(configured.sink.as_deref(), Some(SPEAKERS));
        assert_eq!(configured.source.as_deref(), Some(MIC));
        follow(&mut configured, 0, Some(CONFIGURED_SOURCE), None);
        assert_eq!(configured.sink.as_deref(), Some(SPEAKERS));
        assert_eq!(configured.source, None);
    }

    #[test]
    fn the_mic_in_use_follows_the_session_managers_own_key() {
        let mut configured = Configured::default();
        follow(&mut configured, 0, Some(SOURCE_IN_USE), Some(MIC));
        assert_eq!(configured.source_in_use.as_deref(), Some(MIC));
        assert_eq!(configured.source, None);
        follow(&mut configured, 0, None, None);
        assert_eq!(configured, Configured::default());
    }

    #[test]
    fn the_echo_canceller_captures_the_mic_it_is_given_not_the_default() {
        let args = echo_cancel_args("navette-mic", "Mic", "invented-mic");
        assert!(
            args.contains(
                "capture.props = { node.name = \"navette-mic-capture\" \
                 node.description = \"Mic\" target.object = \"invented-mic\" }"
            ),
            "{args}"
        );
    }

    #[test]
    fn a_clear_on_another_subject_keeps_the_default() {
        let mut configured = Configured {
            sink: Some(SPEAKERS.to_owned()),
            source: Some(MIC.to_owned()),
            ..Configured::default()
        };
        // A node's own metadata cleared (its id as the subject), not the core's.
        follow(&mut configured, 42, None, None);
        follow(&mut configured, 42, Some(CONFIGURED_SINK), None);
        assert_eq!(configured.sink.as_deref(), Some(SPEAKERS));
        assert_eq!(configured.source.as_deref(), Some(MIC));
    }
}
