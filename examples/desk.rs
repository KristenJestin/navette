//! Holds one desk change on the running Hyprland until stdin closes or reads a line: a rule that
//! sends windows opening on `<workspace>` to `<monitor>`. Config reloads re-apply it; a new run
//! cleans what a killed one left. For checking the rollback by hand, in a headless Hyprland.
//!
//!   cargo run --example desk -- <monitor> <workspace>

use std::io::BufRead;
use std::sync::mpsc;
use std::thread;

use navette::desk::{Change, Desk};
use navette::hypr::{Event, HyprIpc, lua_string};

enum Message {
    Hypr(Event),
    Stop,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [monitor, workspace] = args.as_slice() else {
        return Err("usage: desk <monitor> <workspace>".into());
    };
    let ipc = HyprIpc::from_env()?;
    let mut desk = Desk::start(ipc.clone(), Desk::<HyprIpc>::default_record()?)?;
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

    let (tx, rx) = mpsc::channel();
    let events = ipc.events()?;
    let hypr = tx.clone();
    thread::spawn(move || {
        for event in events.flatten() {
            let _ = hypr.send(Message::Hypr(event));
        }
    });
    thread::spawn(move || {
        let _ = std::io::stdin().lock().lines().next();
        let _ = tx.send(Message::Stop);
    });

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
