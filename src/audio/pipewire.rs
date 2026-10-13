//! The real PipeWire graph, on a thread of its own that runs PipeWire's loop: [`PipeWire`]
//! sends it commands and waits for the answers. It connects on first use, and again after
//! PipeWire restarts.
//!
//! The remote's sink is a stream with an `Audio/Sink` media class and no autoconnect, the way
//! PipeWire's own loopback module makes a virtual sink: the session manager lists it among the
//! outputs and links the apps to it. The default output is the `default.configured.audio.sink`
//! key of the `default` metadata, the one `wpctl set-default` writes.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::spa;
use pw::types::ObjectType;

use super::codec::{CHANNELS, FRAME, RATE};
use super::jitter::Jitter;
use super::{DefaultSink, Graph, OnSound};

const CONFIGURED_SINK: &str = "default.configured.audio.sink";
/// Long enough for a busy PipeWire, short enough that a hung one does not hang the link.
const ANSWER: Duration = Duration::from_secs(5);
const BYTES_PER_FRAME: usize = CHANNELS * size_of::<f32>();

type Reply<T> = std_mpsc::Sender<io::Result<T>>;
/// The changes waiting for the server, by the sync that follows them.
type Pending = Rc<RefCell<Vec<(spa::utils::result::AsyncSeq, Reply<()>)>>>;

enum Command {
    DefaultSink(Reply<Option<DefaultSink>>),
    SetDefaultSink(Option<DefaultSink>, Reply<()>),
    OpenSink {
        name: String,
        description: String,
        on_sound: OnSound,
        reply: Reply<u64>,
    },
    OpenPlayback {
        name: String,
        description: String,
        jitter: Arc<Mutex<Jitter>>,
        reply: Reply<u64>,
    },
    Close(u64),
}

#[derive(Clone, Default)]
pub struct PipeWire {
    connection: Arc<Mutex<Option<pw::channel::Sender<Command>>>>,
}

/// A stream of ours in the graph; removed when dropped.
pub struct Node {
    graph: PipeWire,
    id: u64,
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(sender) = self.graph.connection.lock().unwrap().as_ref() {
            let _ = sender.send(Command::Close(self.id));
        }
    }
}

impl PipeWire {
    fn call<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> io::Result<T> {
        let mut connection = self.connection.lock().unwrap();
        let sender = match connection.as_ref() {
            Some(sender) => sender.clone(),
            None => connection.insert(connect()?).clone(),
        };
        let (reply, answer) = std_mpsc::channel();
        let answer = match sender.send(command(reply)) {
            Ok(()) => answer.recv_timeout(ANSWER).ok(),
            Err(_) => None,
        };
        answer.unwrap_or_else(|| {
            // The loop is gone (PipeWire restarted) or hung: the next call connects again.
            *connection = None;
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PipeWire did not answer",
            ))
        })
    }

    fn node(&self, id: u64) -> Node {
        Node {
            graph: self.clone(),
            id,
        }
    }
}

impl Graph for PipeWire {
    type Node = Node;

    fn default_sink(&self) -> io::Result<Option<DefaultSink>> {
        self.call(Command::DefaultSink)
    }

    fn set_default_sink(&self, sink: Option<&DefaultSink>) -> io::Result<()> {
        self.call(|reply| Command::SetDefaultSink(sink.cloned(), reply))
    }

    fn open_sink(&self, name: &str, description: &str, on_sound: OnSound) -> io::Result<Node> {
        let id = self.call(|reply| Command::OpenSink {
            name: name.to_owned(),
            description: description.to_owned(),
            on_sound,
            reply,
        })?;
        Ok(self.node(id))
    }

    fn open_playback(
        &self,
        name: &str,
        description: &str,
        jitter: Arc<Mutex<Jitter>>,
    ) -> io::Result<Node> {
        let id = self.call(|reply| Command::OpenPlayback {
            name: name.to_owned(),
            description: description.to_owned(),
            jitter,
            reply,
        })?;
        Ok(self.node(id))
    }
}

fn pw_error(e: pw::Error) -> io::Error {
    io::Error::other(format!("PipeWire: {e}"))
}

/// Starts PipeWire's thread, once it has connected and read the default metadata.
fn connect() -> io::Result<pw::channel::Sender<Command>> {
    let (sender, receiver) = pw::channel::channel();
    let (ready, started) = std_mpsc::channel();
    std::thread::Builder::new()
        .name("navette-pipewire".into())
        .spawn(move || {
            if let Err(e) = run(receiver, &ready) {
                let _ = ready.send(Err(e));
            }
        })?;
    started
        .recv_timeout(ANSWER)
        .unwrap_or_else(|_| Err(io::Error::other("PipeWire did not answer")))?;
    Ok(sender)
}

/// A stream of ours, its listener dropped first so it unhooks from a live stream.
struct Open {
    _listener: Box<dyn std::any::Any>,
    _stream: pw::stream::StreamRc,
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
    let configured: Rc<RefCell<Option<String>>> = Rc::default();
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
                        match key {
                            Some(CONFIGURED_SINK) if subject == pw::core::PW_ID_CORE => {
                                *configured.borrow_mut() = value.map(str::to_owned);
                            }
                            // Every key cleared.
                            None => *configured.borrow_mut() = None,
                            _ => {}
                        }
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
    let next_id = Cell::new(0u64);
    let _commands = commands.attach(mainloop.loop_(), move |command| match command {
        Command::DefaultSink(reply) => {
            let _ = reply.send(Ok(configured.borrow().clone().map(DefaultSink)));
        }
        Command::SetDefaultSink(sink, reply) => {
            let result = match metadata.borrow().as_ref() {
                Some((metadata, _)) => {
                    let value = sink.map(|sink| sink.0);
                    metadata.set_property(
                        pw::core::PW_ID_CORE,
                        CONFIGURED_SINK,
                        value.as_ref().map(|_| "Spa:String:JSON"),
                        value.as_deref(),
                    );
                    // The server echoes it back; until then, this is what it holds.
                    *configured.borrow_mut() = value;
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
        Command::OpenSink {
            name,
            description,
            on_sound,
            reply,
        } => {
            let id = next_id.replace(next_id.get() + 1);
            let result = open_sink(&core, &name, &description, on_sound).map(|open| {
                streams.borrow_mut().insert(id, open);
                id
            });
            let _ = reply.send(result);
        }
        Command::OpenPlayback {
            name,
            description,
            jitter,
            reply,
        } => {
            let id = next_id.replace(next_id.get() + 1);
            let result = open_playback(&core, &name, &description, jitter).map(|open| {
                streams.borrow_mut().insert(id, open);
                id
            });
            let _ = reply.send(result);
        }
        Command::Close(id) => {
            streams.borrow_mut().remove(&id);
        }
    });
    if !failed.get() {
        mainloop.run();
    }
    Ok(())
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

fn properties(name: &str, description: &str) -> pw::properties::PropertiesBox {
    pw::properties::properties! {
        *pw::keys::NODE_NAME => name,
        *pw::keys::NODE_DESCRIPTION => description,
        *pw::keys::MEDIA_NAME => description,
        *pw::keys::APP_NAME => "Navette",
        // One 10 ms frame per cycle when the graph allows it.
        *pw::keys::NODE_LATENCY => format!("{FRAME}/{RATE}"),
    }
}

fn connect_stream(
    stream: &pw::stream::StreamRc,
    direction: spa::utils::Direction,
    flags: pw::stream::StreamFlags,
) -> io::Result<()> {
    let format = format();
    let mut params = [spa::pod::Pod::from_bytes(&format).expect("a serialized pod")];
    stream
        .connect(
            direction,
            None,
            flags | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(pw_error)
}

fn open_sink(
    core: &pw::core::CoreRc,
    name: &str,
    description: &str,
    on_sound: OnSound,
) -> io::Result<Open> {
    let mut props = properties(name, description);
    props.insert(*pw::keys::MEDIA_CLASS, "Audio/Sink");
    let stream = pw::stream::StreamRc::new(core.clone(), name, props).map_err(pw_error)?;
    let listener = stream
        .add_local_listener_with_user_data(on_sound)
        .process(|stream, on_sound| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            if let Some(bytes) = data.data() {
                let end = (offset + size).min(bytes.len());
                let samples: Vec<f32> = bytes[offset.min(end)..end]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b))
                    .collect();
                on_sound(&samples);
            }
        })
        .register()
        .map_err(pw_error)?;
    // Not autoconnected: the session manager links the apps to it, as to any sink.
    connect_stream(
        &stream,
        spa::utils::Direction::Input,
        pw::stream::StreamFlags::empty(),
    )?;
    Ok(Open {
        _listener: Box::new(listener),
        _stream: stream,
    })
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
    let stream = pw::stream::StreamRc::new(core.clone(), name, props).map_err(pw_error)?;
    let listener = stream
        .add_local_listener_with_user_data(jitter)
        .process(|stream, jitter| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let frames = match data.data() {
                Some(bytes) => {
                    let mut frames = bytes.len() / BYTES_PER_FRAME;
                    // Only what the graph asks for: a whole buffer would be latency.
                    if requested > 0 {
                        frames = frames.min(requested);
                    }
                    let mut samples = vec![0.0f32; frames * CHANNELS];
                    jitter.lock().unwrap().pull(&mut samples);
                    for (out, sample) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(&samples) {
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
    connect_stream(
        &stream,
        spa::utils::Direction::Output,
        pw::stream::StreamFlags::AUTOCONNECT,
    )?;
    Ok(Open {
        _listener: Box::new(listener),
        _stream: stream,
    })
}
