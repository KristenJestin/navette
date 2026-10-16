//! Short messages, behind a port: the desktop's notifications say when a mode starts and stops
//! (#69), and the corners say where the mic went at each switch ([`crate::corner::Board`]).
//!
//! The real port runs `notify-send` (libnotify), which talks to whatever notification service
//! the desktop runs. It runs on a thread of its own, one notification at a time and in order:
//! the service may be slow or absent, and the mic never waits for it.

use std::io;
use std::process::Stdio;
use std::sync::{Arc, mpsc};
use std::thread;

/// How long a notification stays up, in milliseconds.
const EXPIRE: &str = "3000";

/// How long an offer stays up, in milliseconds: long enough to reach for the mouse, which the
/// other messages, that only tell, are not.
const OFFER_EXPIRE: &str = "15000";

/// The name of an offer's button, which `notify-send` prints when it is pressed.
const ACTION: &str = "start";

/// The name libnotify gives to a click on the notification itself, which `notify-send` prints
/// like a button's: the click on the body does what the button does.
const BODY_CLICK: &str = "default";

/// Shows short messages to the user.
pub trait Notifier: Send + Sync {
    /// Shows `summary` (and `body` under it, unless empty), in place of the previous message
    /// of this notifier, so that they do not pile up. It never waits for the desktop.
    fn show(&self, summary: &str, body: &str);

    /// Shows `summary` with one button called `action`; `chosen` runs if the user presses it,
    /// and never if they dismiss the message or the desktop has no service. It never waits for
    /// the user, and does not replace or get replaced by the messages of [`Notifier::show`].
    /// A notifier with no button only says `summary`.
    fn offer(&self, summary: &str, action: &str, chosen: Box<dyn FnOnce() + Send>) {
        let _ = (action, chosen);
        self.show(summary, "");
    }
}

/// Runs `notify-send` (the port behind which tests put a fake). A clone is what an offer runs,
/// for it waits on the user: the one of the other messages must stay free.
pub trait Run: Clone + Send + Sync + 'static {
    /// Runs it with `args` and returns what it printed.
    fn run(&mut self, args: &[String]) -> io::Result<String>;
}

/// The `notify-send` of this machine.
#[derive(Clone)]
pub struct Command;

impl Run for Command {
    fn run(&mut self, args: &[String]) -> io::Result<String> {
        let output = std::process::Command::new("notify-send")
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            let why = String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::other(why.trim().to_string()));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// A `notify-send` run on its own thread, as an offer's.
type Spare = Arc<dyn Fn(&[String]) -> io::Result<String> + Send + Sync>;

/// Notifications shown with `notify-send`.
pub struct NotifySend {
    queue: mpsc::Sender<(String, String)>,
    /// What an offer runs `notify-send` with, on a thread of its own.
    spare: Spare,
}

impl NotifySend {
    /// The thread ends with the notifier.
    pub fn new(run: impl Run) -> Self {
        let (queue, messages) = mpsc::channel();
        let spare = run.clone();
        thread::Builder::new()
            .name("navette-notify".into())
            .spawn(move || work(run, &messages))
            .expect("a thread for the notifications");
        Self {
            queue,
            spare: Arc::new(move |args| spare.clone().run(args)),
        }
    }
}

impl Notifier for NotifySend {
    fn show(&self, summary: &str, body: &str) {
        let _ = self.queue.send((summary.to_owned(), body.to_owned()));
    }

    fn offer(&self, summary: &str, action: &str, chosen: Box<dyn FnOnce() + Send>) {
        let args: Vec<String> = [
            "--app-name",
            "Navette",
            "--icon",
            "navette",
            "--expire-time",
            OFFER_EXPIRE,
            "--wait",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain([
            format!("--action={ACTION}={action}"),
            format!("--action={BODY_CLICK}={action}"),
            "--".into(),
            summary.into(),
        ])
        .collect();
        let spare = self.spare.clone();
        // One thread per offer: it waits on the user, and the queue of the other messages must
        // not wait with it.
        let spawned =
            thread::Builder::new()
                .name("navette-offer".into())
                .spawn(move || match spare(&args) {
                    Ok(printed) if matches!(printed.trim(), ACTION | BODY_CLICK) => chosen(),
                    Ok(_) => {}
                    Err(e) => eprintln!("navette: notifications: `notify-send` failed: {e}"),
                });
        if let Err(e) = spawned {
            eprintln!("navette: notifications: no thread for an offer: {e}");
        }
    }
}

fn work(mut run: impl Run, messages: &mpsc::Receiver<(String, String)>) {
    // The service's number for the message on screen, which the next one replaces.
    let mut shown: Option<u32> = None;
    let mut told = false;
    for (summary, body) in messages {
        let mut args: Vec<String> = [
            "--print-id",
            "--app-name",
            "Navette",
            "--icon",
            "navette",
            "--expire-time",
        ]
        .into_iter()
        .chain([EXPIRE])
        .map(str::to_owned)
        .collect();
        if let Some(id) = shown {
            args.extend(["--replace-id".to_owned(), id.to_string()]);
        }
        args.push("--".into());
        args.push(summary);
        if !body.is_empty() {
            args.push(body);
        }
        shown = match run.run(&args) {
            Ok(printed) => printed.trim().parse().ok(),
            Err(e) => {
                // Once: with no notification service, every switch would say it again.
                if !std::mem::replace(&mut told, true) {
                    eprintln!(
                        "navette: notifications: `notify-send` failed, so none is shown: {e}"
                    );
                }
                None
            }
        };
    }
}

/// An offer the fake heard: its summary, its action, and what pressing the action runs.
#[cfg(test)]
type Offer = (String, String, Option<Box<dyn FnOnce() + Send>>);

/// A notifier in memory, for the tests: it records what was shown.
#[cfg(test)]
#[derive(Default)]
pub struct Fake {
    shown: std::sync::Mutex<Vec<(String, String)>>,
    offered: std::sync::Mutex<Vec<Offer>>,
}

#[cfg(test)]
impl Fake {
    /// The summaries shown so far, in order.
    pub fn summaries(&self) -> Vec<String> {
        let shown = self.shown.lock().unwrap();
        shown.iter().map(|(summary, _)| summary.clone()).collect()
    }

    /// Everything shown so far, in order, as `(summary, body)`.
    pub fn shown(&self) -> Vec<(String, String)> {
        self.shown.lock().unwrap().clone()
    }

    /// Every offer made so far, in order, as `(summary, action)`.
    pub fn offers(&self) -> Vec<(String, String)> {
        let offered = self.offered.lock().unwrap();
        offered
            .iter()
            .map(|(summary, action, _)| (summary.clone(), action.clone()))
            .collect()
    }

    /// The user presses the button of the `n`-th offer (counted from 0).
    pub fn press(&self, n: usize) {
        let chosen = self.offered.lock().unwrap()[n].2.take();
        chosen.expect("an offer not pressed yet")();
    }
}

#[cfg(test)]
impl Notifier for Fake {
    fn show(&self, summary: &str, body: &str) {
        self.shown
            .lock()
            .unwrap()
            .push((summary.to_owned(), body.to_owned()));
    }

    fn offer(&self, summary: &str, action: &str, chosen: Box<dyn FnOnce() + Send>) {
        self.offered
            .lock()
            .unwrap()
            .push((summary.to_owned(), action.to_owned(), Some(chosen)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A `notify-send` that numbers its notifications from 7 and tells the test what it was
    /// run with. With `refuse`, it fails as one with no service does.
    #[derive(Clone)]
    struct FakeRun {
        calls: mpsc::Sender<Vec<String>>,
        next: u32,
        refuse: bool,
        /// What a `notify-send --wait` prints once the user is done with the message.
        printed: String,
    }

    impl Run for FakeRun {
        fn run(&mut self, args: &[String]) -> io::Result<String> {
            self.calls.send(args.to_vec()).unwrap();
            if self.refuse {
                return Err(io::Error::other("no notification service"));
            }
            if args.iter().any(|a| a == "--wait") {
                return Ok(self.printed.clone());
            }
            self.next += 1;
            Ok(format!("{}\n", self.next + 6))
        }
    }

    fn notifier(refuse: bool) -> (NotifySend, mpsc::Receiver<Vec<String>>) {
        let (calls, seen) = mpsc::channel();
        let run = FakeRun {
            calls,
            next: 0,
            refuse,
            printed: String::new(),
        };
        (NotifySend::new(run), seen)
    }

    fn next(seen: &mpsc::Receiver<Vec<String>>) -> Vec<String> {
        seen.recv_timeout(Duration::from_secs(5))
            .expect("a notify-send in time")
    }

    #[test]
    fn a_notification_is_a_notify_send_that_prints_its_id() {
        let (notifier, seen) = notifier(false);
        notifier.show("Mic → remote", "");
        let args = next(&seen);
        assert!(args.contains(&"--print-id".to_owned()), "{args:?}");
        assert!(!args.contains(&"--replace-id".to_owned()), "{args:?}");
        // The icon by its name in the icon theme, where the install puts `navette.svg`.
        assert!(
            args.windows(2).any(|pair| pair == ["--icon", "navette"]),
            "{args:?}"
        );
        // The text comes last, after `--`: a summary never reads as an option.
        assert_eq!(args[args.len() - 2..], ["--", "Mic → remote"]);
    }

    #[test]
    fn a_body_comes_under_the_summary() {
        let (notifier, seen) = notifier(false);
        notifier.show("Mic failed", "no echo canceller");
        let args = next(&seen);
        assert_eq!(
            args[args.len() - 3..],
            ["--", "Mic failed", "no echo canceller"]
        );
    }

    #[test]
    fn a_second_notification_replaces_the_first_one() {
        let (notifier, seen) = notifier(false);
        notifier.show("Mic → remote", "");
        let first = next(&seen);
        assert!(!first.contains(&"--replace-id".to_owned()), "{first:?}");
        notifier.show("Mic → desk", "");
        notifier.show("Mic → remote", "");
        // The service numbered the first 7, the second 8.
        let second = next(&seen);
        assert!(
            second.windows(2).any(|w| w == ["--replace-id", "7"]),
            "{second:?}"
        );
        let third = next(&seen);
        assert!(
            third.windows(2).any(|w| w == ["--replace-id", "8"]),
            "{third:?}"
        );
    }

    #[test]
    fn with_no_notification_service_each_message_is_tried_and_none_replaces() {
        let (notifier, seen) = notifier(true);
        notifier.show("Mic → remote", "");
        notifier.show("Mic → desk", "");
        for _ in 0..2 {
            let args = next(&seen);
            assert!(!args.contains(&"--replace-id".to_owned()), "{args:?}");
        }
    }

    /// A notifier whose `notify-send --wait` prints `printed` (or fails, with `refuse`).
    fn offering(printed: &str, refuse: bool) -> (NotifySend, mpsc::Receiver<Vec<String>>) {
        let (calls, seen) = mpsc::channel();
        let run = FakeRun {
            calls,
            next: 0,
            refuse,
            printed: printed.to_owned(),
        };
        (NotifySend::new(run), seen)
    }

    /// An offer, and whether its button was heard of within a moment.
    fn pressed_after_offer(notifier: &NotifySend) -> bool {
        let (pressed, heard) = mpsc::channel();
        notifier.offer(
            "REMOTE is here",
            "Start",
            Box::new(move || pressed.send(()).unwrap()),
        );
        heard.recv_timeout(Duration::from_millis(500)).is_ok()
    }

    #[test]
    fn an_offer_is_a_notify_send_that_waits_for_its_action() {
        let (notifier, seen) = offering("start\n", false);
        assert!(pressed_after_offer(&notifier));
        let args = next(&seen);
        assert!(
            args.contains(&"--action=start=Start".to_owned()),
            "{args:?}"
        );
        assert!(args.contains(&"--wait".to_owned()), "{args:?}");
        assert_eq!(args[args.len() - 2..], ["--", "REMOTE is here"]);
    }

    #[test]
    fn a_dismissed_offer_does_nothing() {
        let (notifier, seen) = offering("", false);
        assert!(!pressed_after_offer(&notifier));
        next(&seen);
    }

    #[test]
    fn an_offer_with_no_notification_service_does_nothing() {
        let (notifier, seen) = offering("start\n", true);
        assert!(!pressed_after_offer(&notifier));
        next(&seen);
    }

    #[test]
    fn a_click_on_the_notification_starts_the_mode_like_its_button() {
        let (notifier, seen) = offering("default\n", false);
        assert!(pressed_after_offer(&notifier));
        next(&seen);
    }

    #[test]
    fn an_offer_carries_the_button_and_the_click_on_the_body() {
        let (notifier, seen) = offering("", false);
        pressed_after_offer(&notifier);
        let args = next(&seen);
        assert!(
            args.contains(&"--action=start=Start".to_owned()),
            "{args:?}"
        );
        assert!(
            args.contains(&"--action=default=Start".to_owned()),
            "{args:?}"
        );
    }

    #[test]
    fn an_action_other_than_the_offered_ones_does_nothing() {
        let (notifier, seen) = offering("other\n", false);
        assert!(!pressed_after_offer(&notifier));
        next(&seen);
    }
}
