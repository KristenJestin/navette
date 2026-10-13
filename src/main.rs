//! `navette`: the work-from-home mode. The machine with the monitors ("home") pairs once with
//! the work laptop ("remote"), then `navette serve` at home and `navette agent` on the laptop
//! keep an authenticated link between them. `navette mode start` gives one home monitor to the
//! remote machine, `navette mode stop` gives it back; `navette serve --audio` brings the remote
//! machine's sound home while the link is up.

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
use navette::desk::Desk;
use navette::display::{decode, remote};
use navette::hypr::{self, Eval};
use navette::input;
use navette::link::{self, Backoff, Event, Heartbeat, Wire};
use navette::mode::{self, Home};
use navette::pair::{self, Pin, Timeouts};
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
        /// Play the remote machine's sound here, mixed with home sound, while the link is up
        /// (until the modes take this over).
        #[arg(long)]
        audio: bool,
    },
    /// Remote: dial the paired home machine and keep the link up (a systemd user service).
    Agent,
    /// Home: give the configured monitor to the remote machine, or take it back.
    ///
    /// The monitor is named in `mode.conf` in the config directory: `monitor = <output name>`.
    /// Over it, the keyboard and every shortcut go to the remote machine; `release = <chord>`
    /// (default `shift+escape`, or `none`) gives the keyboard back. `navette serve` must be
    /// running.
    Mode {
        #[command(subcommand)]
        action: ModeAction,
    },
}

#[derive(Subcommand)]
enum ModeAction {
    /// The remote machine takes the monitor.
    Start,
    /// The monitor comes back home.
    Stop,
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
            pair::remote(&store, home, &pin, Timeouts::default()).await?;
            println!("Paired. Start the agent: systemctl --user enable --now navette-agent");
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
            let gpu = tokio::task::spawn_blocking(decode::open_gpu)
                .await
                .map_err(std::io::Error::other)?;
            let home = Home {
                config_dir: store.dir().to_path_buf(),
                hypr: Arc::new(hypr::Live),
                screens: Arc::new(mode::LiveScreens),
                gpu,
                desk_record: Desk::<Eval>::default_record()?,
            };
            let (peers, peers_rx) = mpsc::unbounded_channel();
            let (orders, orders_rx) = mpsc::unbounded_channel();
            let (hypr_events, hypr_events_rx) = mpsc::unbounded_channel();
            hypr::watch_events(hypr_events);
            let modes = tokio::spawn(mode::serve(home, peers_rx, orders_rx, hypr_events_rx));
            let control = tokio::spawn(mode::accept_orders(control, orders));
            // Without `--audio`, home never reaches PipeWire.
            let graph = audio.then(PipeWire::default);
            // Each service takes its part of the link; the clipboard bridge, not `Send`, runs on
            // this thread, in a local set.
            let attach = move |mut wire: Wire| {
                if let Some(display) = wire.display.take() {
                    let _ = peers.send(display);
                }
                if let Some(graph) = &graph
                    && let Some(sound) = wire.audio.take()
                    && let Err(e) = audio::home(graph.clone(), sound)
                {
                    eprintln!("navette: audio: {e}");
                }
                clipboard::attach(Side::Client, clipboard::hypr(Target::default()), wire)
            };
            let local = tokio::task::LocalSet::new();
            tokio::select! {
                () = local.run_until(link::serve(acceptor, Heartbeat::default(), events, attach)) => {}
                result = stopped() => result?,
            }
            // Closing every way in ends the mode, which gives the monitor back.
            drop(local);
            control.abort();
            let _ = control.await;
            let _ = modes.await;
            Ok(())
        }
        Command::Agent => {
            let dialer = link::remote_dialer(&store)?;
            let (events, log) = mpsc::unbounded_channel();
            tokio::spawn(print_events(log));
            let (peers, peers_rx) = mpsc::unbounded_channel();
            let (hypr_events, hypr_events_rx) = mpsc::unbounded_channel();
            hypr::watch_events(hypr_events);
            let display = tokio::spawn(remote::serve(
                peers_rx,
                Arc::new(hypr::Live),
                Arc::new(remote::LiveStreams),
                Arc::new(input::LiveInjectors),
                hypr_events_rx,
            ));
            let graph = PipeWire::default();
            let saved = audio::Saved::in_dir(audio::Saved::default_dir()?);
            // The latest link's audio thread; each one joins the one before it.
            let audio_thread = Arc::new(Mutex::new(None));
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
                    clipboard::attach(Side::Server, clipboard::hypr(Target::default()), wire)
                }
            };
            let agent = link::agent(
                dialer,
                Heartbeat::default(),
                Backoff::default(),
                events,
                attach,
            );
            let local = tokio::task::LocalSet::new();
            tokio::select! {
                () = local.run_until(agent) => {}
                result = stopped() => result?,
            }
            // The link is dropped: the display removes its output, and the audio thread removes
            // its sink and puts the previous default output back; the process waits for both.
            drop(local);
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
                ModeAction::Start => "start",
                ModeAction::Stop => "stop",
            };
            println!("{}", mode::order(&mode::control_path()?, order).await?);
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
        }
    }
}
