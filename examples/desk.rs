//! Holds one desk change on the running Hyprland until stdin closes or reads a line, or until
//! SIGINT or SIGTERM: a rule that sends windows opening on `<workspace>` to `<monitor>`. Config
//! reloads re-apply it; a new run cleans what a killed one left. For checking the rollback by
//! hand, in a headless Hyprland.
//!
//!   cargo run --example desk -- <monitor> <workspace>

use std::io::BufRead;
use std::sync::mpsc;
use std::thread;

use navette::desk::{Change, Desk};
use navette::hypr::{Event, HyprIpc, lua_string};
use tokio::signal::unix::{SignalKind, signal};

enum Message {
    Hypr(Event),
    Stop,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [monitor, workspace] = args.as_slice() else {
        return Err("usage: desk <monitor> <workspace>".into());
    };
    let (tx, rx) = mpsc::channel();

    // Signals are caught before anything changes, so that no exit skips the stop.
    let signals = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (mut interrupt, mut terminate) = signals.block_on(async {
        Ok::<_, std::io::Error>((
            signal(SignalKind::interrupt())?,
            signal(SignalKind::terminate())?,
        ))
    })?;
    let stop = tx.clone();
    thread::spawn(move || {
        signals.block_on(async {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
        });
        let _ = stop.send(Message::Stop);
    });
    let stop = tx.clone();
    thread::spawn(move || {
        let _ = std::io::stdin().lock().lines().next();
        let _ = stop.send(Message::Stop);
    });

    let ipc = HyprIpc::from_env()?;
    // Subscribed before applying, so a reload in between is not missed.
    let events = ipc.events()?;
    thread::spawn(move || {
        for event in events.flatten() {
            let _ = tx.send(Message::Hypr(event));
        }
    });

    // From here on, an early return drops the desk, which undoes what it applied.
    let mut desk = Desk::start(ipc, Desk::<HyprIpc>::default_record()?)?;
    println!("started, leftovers cleaned");
    let fields = format!(
        "match = {{ workspace = {} }}, monitor = {}",
        lua_string(workspace),
        lua_string(monitor)
    );
    desk.apply(Change::window_rule(
        &desk.rule_name("keep-home-off"),
        &fields,
    ))?;
    println!("applied");

    for message in rx {
        match message {
            Message::Hypr(event @ Event::ConfigReloaded) => {
                desk.handle(&event)?;
                println!("reapplied after a reload");
            }
            Message::Hypr(_) => {}
            Message::Stop => break,
        }
    }
    desk.stop()?;
    println!("stopped");
    Ok(())
}
