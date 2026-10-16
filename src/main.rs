//! `navette`: the work-from-home mode. The machine with the monitors ("home") pairs once with
//! the work laptop ("remote"), then `navette serve` at home and `navette agent` on the laptop
//! keep an authenticated link between them. `navette mode start [name]` gives the home monitors
//! of a mode to the remote machine, `navette mode stop` gives them back, `navette status` says
//! what runs; `navette serve --audio` brings the remote machine's sound home and sends the home
//! mic there while a mode runs; `navette mic` switches it.

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use gliff_transport::clipboard::Side;
use hypr_input::Target;
use navette::audio::{self, pipewire::PipeWire};
use navette::clipboard;
use navette::clipboard::progress::Shown;
use navette::corner::{self, Board};
use navette::desk::Desk;
use navette::display::{decode, remote};
use navette::hypr::{self, Eval};
use navette::input;
use navette::lid;
use navette::link::{self, Backoff, Event, Heartbeat, Wire};
use navette::mic::{self, Mic, NoSound, Routing};
use navette::mode::{self, Home, HomeGpu};
use navette::network::{self, ProcNet};
use navette::notify;
use navette::pair::{self, Pin, Timeouts};
use navette::single::{self, AgentLock};
use navette::store::Store;
use navette::transport::bind_lan;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    name = "navette",
    version = option_env!("NAVETTE_VERSION").unwrap_or("0.0.0-dev"),
    about = "The work-from-home mode for two Hyprland machines"
)]
struct Cli {
    /// Navette's config directory [default: $XDG_CONFIG_HOME/navette].
    #[arg(long, global = true, env = "NAVETTE_CONFIG_DIR", value_name = "DIR")]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pair this machine with the other one, once.
    ///
    /// At home, `navette pair --listen <LAN-address:port>` shows a PIN. On the remote machine,
    /// `navette pair <home-address:port>` asks for it.
    Pair {
        /// Home: the LAN address and port to listen on (never 0.0.0.0 or ::).
        #[arg(
            long,
            value_name = "ADDR:PORT",
            conflicts_with = "home",
            required_unless_present = "home"
        )]
        listen: Option<SocketAddr>,
        /// Remote: the home machine's address and port.
        #[arg(value_name = "HOME:PORT")]
        home: Option<SocketAddr>,
    },
    /// Home: wait for the paired remote machine and keep its link up.
    Serve {
        /// The LAN address and port to listen on [default: the one used to pair].
        #[arg(long, value_name = "ADDR:PORT")]
        listen: Option<SocketAddr>,
        /// While a mode runs, play the remote machine's sound here, mixed with home sound, and
        /// send it the home mic.
        #[arg(long)]
        audio: bool,
    },
    /// Remote: dial the paired home machine and keep the link up (a systemd user service).
    Agent,
    /// Home: give the configured monitors to the remote machine, or take them back.
    ///
    /// The monitors are named in `mode.conf` in the config directory: `monitor = <output name>`,
    /// or `monitor = desc:<start of the monitor's description>`. A `[name]` section defines a
    /// mode of its own, started by name. Over the monitors, the keyboard and every shortcut go
    /// to the remote machine; `release = <chord>` (default `shift+escape`, or `none`) gives the
    /// keyboard back. `navette serve` must be running, except for `list`.
    Mode {
        #[command(subcommand)]
        action: ModeAction,
    },
    /// Home: say which mode runs, and whether the remote machine is connected.
    ///
    /// `navette serve` must be running.
    Status,
    /// Home: send the home mic to the remote machine or keep it home, while a mode runs.
    ///
    /// A mode sends it to the remote machine when it starts, and brings it home when it stops.
    /// `navette serve --audio` must be running.
    Mic {
        #[command(subcommand)]
        action: MicAction,
    },
}

#[derive(Subcommand)]
enum MicAction {
    /// Switch it to the other machine.
    Toggle,
    /// Send it to the remote machine.
    Remote,
    /// Keep it home.
    Home,
    /// Say where it goes: `remote`, `home`, or `off` with no mode running.
    Status {
        /// One line of JSON (`text`, `alt`, `class`, `tooltip`), for a bar.
        #[arg(long)]
        json: bool,
    },
    /// Say where it goes, then again at every change, until stopped: for a bar.
    Watch {
        /// Lines of JSON (`text`, `alt`, `class`, `tooltip`).
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ModeAction {
    /// The remote machine takes the monitors of the mode called NAME (the only one, or the
    /// unnamed one, with no name). A mode already running is stopped first, and its monitors
    /// given back.
    Start {
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    /// The monitors come back home.
    Stop,
    /// Say the modes `mode.conf` defines and their monitors.
    List,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // gliff's crates log through `tracing`: their warnings (a GPU fallback) reach the journal.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("navette: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> std::io::Result<()> {
    let store = Store::new(match cli.config_dir {
        Some(dir) => dir,
        None => Store::default_dir()?,
    });
    match cli.command {
        Command::Pair {
            listen: Some(listen),
            ..
        } => {
            let listener = bind_lan(listen).await?;
            let address = listener.local_addr()?;
            let pin = Pin::random();
            let timeouts = Timeouts::default();
            println!("Pairing PIN: {}", pin.as_str());
            println!(
                "On the remote machine, run `navette pair {address}` and type this PIN \
                 (within {} s).",
                timeouts.wait.as_secs()
            );
            pair::home(&store, listener, &pin, timeouts).await?;
            println!("Paired. Run `navette serve` here, and the agent on the remote machine.");
            Ok(())
        }
        Command::Pair {
            home: Some(home), ..
        } => {
            eprint!("PIN shown on the home machine: ");
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            let pin = Pin::parse(&line).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "a PIN is six digits")
            })?;
            // Home's router, as this machine sees it now, on home's network: the agent dials
            // home only behind it.
            let gateway = network::gateways(&ProcNet).first().copied();
            pair::remote(&store, home, gateway, &pin, Timeouts::default()).await?;
            // A running agent read the previous pairing when it started: restart it.
            println!(
                "Paired. Start the agent, or restart it to use this pairing: \
                 systemctl --user enable navette-agent && systemctl --user restart navette-agent"
            );
            Ok(())
        }
        Command::Pair { .. } => unreachable!("clap requires --listen or the home address"),
        Command::Serve { listen, audio } => {
            let acceptor = link::home_acceptor(&store, listen).await?;
            eprintln!(
                "navette: waiting for the paired remote machine on {}",
                acceptor.local_addr()?
            );
            let (events, log) = mpsc::unbounded_channel();
            tokio::spawn(print_events(log));
            let control = mode::listen(&mode::control_path()?)?;
            let mic_control = mic::control_path()?;
            let mic_control = mode::listen(&mic_control)?;
            let gpu = tokio::task::spawn_blocking(decode::open_gpu)
                .await
                .map_err(std::io::Error::other)?;
            // Without `--audio`, home never reaches PipeWire.
            let graph = audio.then(PipeWire::default);
            let toggle = mic::toggle_command(&std::env::current_exe()?);
            let saved = audio::Saved::in_dir(audio::Saved::default_dir()?);
            if let Some(graph) = graph.clone() {
                // Home's mic back first, if a previous run died with it away.
                let saved = saved.clone();
                tokio::task::spawn_blocking(move || audio::mic::repair_home(&graph, &saved))
                    .await
                    .map_err(std::io::Error::other)?;
            }
            // What the mic says at each switch shows in the corners, not as notifications.
            let board = Arc::new(Board::default());
            let mic = graph
                .clone()
                .map(|graph| Arc::new(Mic::new(graph, saved, toggle, board.clone())));
            let sound = graph
                .is_some()
                .then(|| Arc::new(audio::HomeSound::default()));
            let routing: Arc<dyn Routing> = match &mic {
                Some(mic) => mic.clone(),
                None => Arc::new(NoSound::default()),
            };
            if mic.is_some() {
                let routing = routing.clone();
                board.on_click(move || {
                    // Refused, the mic says why in the corners.
                    let _ = routing.order(mic::Order::Toggle);
                });
            }
            tokio::spawn(corner::follow(board.clone(), routing.watch()));
            let mic_control = tokio::spawn(mic::serve(mic_control, routing));
            let home = Home {
                config_dir: store.dir().to_path_buf(),
                hypr: Arc::new(hypr::Live),
                screens: Arc::new(mode::LiveScreens),
                gpu: Arc::new(HomeGpu::new(gpu)),
                desk_record: Desk::<Eval>::default_record()?,
                mic: mic.clone().map(|mic| mic as Arc<dyn mode::Mic>),
                sound: sound.clone().map(|sound| sound as Arc<dyn mode::Sound>),
                board: board.clone(),
                notifier: Arc::new(notify::NotifySend::new(notify::Command)),
            };
            let (peers, peers_rx) = mpsc::unbounded_channel();
            let (orders, orders_rx) = mpsc::unbounded_channel();
            let (hypr_events, hypr_events_rx) = mpsc::unbounded_channel();
            hypr::watch_events(hypr_events);
            let modes = tokio::spawn(mode::serve(home, peers_rx, orders_rx, hypr_events_rx));
            let control = tokio::spawn(mode::accept_orders(control, orders));
            let kept = clipboard::Kept::default();
            // The files crossing show in the corners, which stop them.
            let shown = Shown {
                bar: {
                    let board = board.clone();
                    Arc::new(move |bar| board.files(bar))
                },
                notifier: Arc::new(notify::NotifySend::new(notify::Command)),
                cancel: board.cancels(),
            };
            // Each service takes its part of the link; the clipboard bridge, not `Send`, runs on
            // this thread, in a local set.
            let attach = move |mut wire: Wire| {
                if let Some(display) = wire.display.take() {
                    let _ = peers.send(display);
                }
                if let Some(graph) = &graph
                    && let Some(played) = &sound
                    && let Some(sound) = wire.audio.take()
                {
                    if let Some(mic) = &mic {
                        mic.attach(sound.to_peer.clone());
                    }
                    if let Err(e) = audio::home(graph.clone(), sound, played) {
                        eprintln!("navette: audio: {e}");
                    }
                }
                let hypr = clipboard::hypr(Target::default());
                clipboard::attach(Side::Client, hypr, wire, kept.clone(), shown.clone())
            };
            let local = tokio::task::LocalSet::new();
            tokio::select! {
                () = local.run_until(link::serve(acceptor, Heartbeat::default(), events, attach)) => {}
                result = stopped() => result?,
            }
            // Closing every way in ends the mode, which gives the monitor back.
            drop(local);
            tokio::task::spawn_blocking(clipboard::vfs::unmounted).await?;
            control.abort();
            let _ = control.await;
            let _ = modes.await;
            mic_control.abort();
            let _ = mic_control.await;
            Ok(())
        }
        Command::Agent => {
            // Before anything is dialed: a second agent would drop the first one's link.
            let _one_agent = AgentLock::take(&single::agent_lock_path()?)?;
            let dialer = link::remote_dialer(&store)?;
            let home_network = network::HomeNetwork::new(ProcNet, store.clone())?;
            let (events, log) = mpsc::unbounded_channel();
            tokio::spawn(print_events(log));
            let (peers, peers_rx) = mpsc::unbounded_channel();
            let (hypr_events, hypr_events_rx) = mpsc::unbounded_channel();
            hypr::watch_events(hypr_events);
            let ports = remote::Ports {
                hypr: Arc::new(hypr::Live),
                streams: Arc::new(remote::LiveStreams),
                injectors: Arc::new(input::LiveInjectors),
                logind: Arc::new(lid::LiveLogind),
                lid_record: lid::record_path(),
                lid_poll: lid::POLL,
                notifier: Arc::new(notify::NotifySend::new(notify::Command)),
            };
            let mut display = tokio::spawn(remote::serve(peers_rx, ports, hypr_events_rx));
            let graph = PipeWire::default();
            let saved = audio::Saved::in_dir(audio::Saved::default_dir()?);
            // The latest link's audio thread; each one joins the one before it.
            let audio_thread = Arc::new(Mutex::new(None));
            // Home shows the files crossing, and stops them: this machine only says how a paste
            // ended.
            let shown = Shown::notifications(Arc::new(notify::NotifySend::new(notify::Command)));
            let kept = clipboard::Kept::default();
            let attach = {
                let audio_thread = audio_thread.clone();
                move |mut wire: Wire| {
                    if let Some(display) = wire.display.take() {
                        let _ = peers.send(display);
                    }
                    if let Some(sound) = wire.audio.take() {
                        let mut latest = audio_thread.lock().unwrap();
                        match audio::remote(graph.clone(), saved.clone(), sound, latest.take()) {
                            Ok(thread) => *latest = Some(thread),
                            Err(e) => eprintln!("navette: audio: {e}"),
                        }
                    }
                    let hypr = clipboard::hypr(Target::default());
                    clipboard::attach(Side::Server, hypr, wire, kept.clone(), shown.clone())
                }
            };
            let agent = link::agent(
                dialer,
                home_network,
                Heartbeat::default(),
                Backoff::default(),
                events,
                attach,
            );
            let local = tokio::task::LocalSet::new();
            tokio::select! {
                () = local.run_until(agent) => {}
                result = stopped() => result?,
                // It serves until the link's end; ended before, it panicked. Its watch put the
                // laptop's screen back; the agent ends, and systemd starts it again, which
                // removes what is left.
                ended = &mut display => {
                    return Err(std::io::Error::other(format!(
                        "the display stopped: {}",
                        ended.err().map_or_else(|| "no reason".into(), |e| e.to_string())
                    )));
                }
            }
            // The link is dropped: the display removes its output, and the audio thread removes
            // its sink and puts the previous default output back; the process waits for both,
            // and for the clipboard's virtual folder to be unmounted.
            drop(local);
            tokio::task::spawn_blocking(clipboard::vfs::unmounted).await?;
            let _ = display.await;
            let latest = audio_thread.lock().unwrap().take();
            tokio::task::spawn_blocking(move || {
                if let Some(thread) = latest {
                    let _ = thread.join();
                }
            })
            .await?;
            Ok(())
        }
        Command::Mode { action } => {
            let order = match action {
                ModeAction::Start { name } => mode::start_order(name.as_deref())?,
                ModeAction::Stop => "stop".to_string(),
                ModeAction::List => {
                    println!("{}", mode::list(store.dir())?);
                    return Ok(());
                }
            };
            println!("{}", mode::order(&mode::control_path()?, &order).await?);
            Ok(())
        }
        Command::Status => {
            println!("{}", mode::order(&mode::control_path()?, "status").await?);
            Ok(())
        }
        Command::Mic { action } => {
            let path = mic::control_path()?;
            let show = |status: mic::Status, json: bool| match json {
                true => status.json(),
                false => status.word().to_string(),
            };
            let order = match action {
                MicAction::Toggle => mic::Order::Toggle,
                MicAction::Remote => mic::Order::Remote,
                MicAction::Home => mic::Order::Home,
                MicAction::Status { json } => {
                    // No `navette serve`: the mic is home.
                    let status = mic::ask(&path, mic::Order::Status)
                        .await
                        .unwrap_or(mic::Status::Off);
                    println!("{}", show(status, json));
                    return Ok(());
                }
                MicAction::Watch { json } => {
                    return mic::watch(&path, |status| {
                        use std::io::Write;
                        let mut out = std::io::stdout().lock();
                        writeln!(out, "{}", show(status, json))?;
                        // A bar reads it through a pipe: each line at once.
                        out.flush()
                    })
                    .await;
                }
            };
            println!("{}", mic::ask(&path, order).await?.word());
            Ok(())
        }
    }
}

/// SIGTERM (`systemctl --user stop`) or Ctrl-C.
async fn stopped() -> std::io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = terminate.recv() => Ok(()),
        result = tokio::signal::ctrl_c() => result,
    }
}

/// The link's events, on stderr (the journal, under systemd). They name no key or certificate.
async fn print_events(mut events: mpsc::UnboundedReceiver<Event>) {
    while let Some(event) = events.recv().await {
        match event {
            Event::Connected => eprintln!("navette: link up"),
            Event::Disconnected(e) => eprintln!("navette: link down: {e}"),
            Event::Failed(e) => eprintln!("navette: {e}"),
            Event::Retrying(delay) => {
                eprintln!("navette: dialing again in {} s", delay.as_secs_f32())
            }
            Event::Away => eprintln!(
                "navette: not on the desk's network: not dialing it, checking again every \
                 {} s (after a new router there, pair again)",
                network::RECHECK.as_secs()
            ),
            Event::Back => eprintln!("navette: on the desk's network: dialing it"),
        }
    }
}
