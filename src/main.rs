//! `navette`: the work-from-home mode. The machine with the monitors ("home") pairs once with
//! the work laptop ("remote"), then `navette serve` at home and `navette agent` on the laptop
//! keep an authenticated link between them. The modes come with the next issues.

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use navette::link::{self, Backoff, Event, Heartbeat};
use navette::pair::{self, Pin, Timeouts};
use navette::store::Store;
use navette::transport::bind_lan;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    name = "navette",
    version,
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
    },
    /// Remote: dial the paired home machine and keep the link up (a systemd user service).
    Agent,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
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
        Command::Serve { listen } => {
            let acceptor = link::home_acceptor(&store, listen).await?;
            eprintln!(
                "navette: waiting for the paired remote machine on {}",
                acceptor.local_addr()?
            );
            let (events, log) = mpsc::unbounded_channel();
            tokio::spawn(print_events(log));
            link::serve(acceptor, Heartbeat::default(), events).await;
            Ok(())
        }
        Command::Agent => {
            let dialer = link::remote_dialer(&store)?;
            let (events, log) = mpsc::unbounded_channel();
            tokio::spawn(print_events(log));
            link::agent(dialer, Heartbeat::default(), Backoff::default(), events).await;
            Ok(())
        }
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
