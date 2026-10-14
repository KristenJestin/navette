//! Short messages, behind a port: the desktop's notifications say when a mode starts and stops
//! (#69), and the corners say where the mic went at each switch ([`crate::corner::Board`]).
//!
//! The real port runs `notify-send` (libnotify), which talks to whatever notification service
//! the desktop runs. It runs on a thread of its own, one notification at a time and in order:
//! the service may be slow or absent, and the mic never waits for it.

use std::io;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;

/// How long a notification stays up, in milliseconds.
const EXPIRE: &str = "3000";

/// Shows short messages to the user.
pub trait Notifier: Send + Sync {
    /// Shows `summary` (and `body` under it, unless empty), in place of the previous message
    /// of this notifier, so that they do not pile up. It never waits for the desktop.
    fn show(&self, summary: &str, body: &str);
}

/// Runs `notify-send` (the port behind which tests put a fake).
pub trait Run: Send + 'static {
    /// Runs it with `args` and returns what it printed.
    fn run(&mut self, args: &[String]) -> io::Result<String>;
}

/// The `notify-send` of this machine.
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

/// Notifications shown with `notify-send`.
pub struct NotifySend {
    queue: mpsc::Sender<(String, String)>,
}

impl NotifySend {
    /// The thread ends with the notifier.
    pub fn new(run: impl Run) -> Self {
        let (queue, messages) = mpsc::channel();
        thread::Builder::new()
            .name("navette-notify".into())
            .spawn(move || work(run, &messages))
            .expect("a thread for the notifications");
        Self { queue }
    }
}

impl Notifier for NotifySend {
    fn show(&self, summary: &str, body: &str) {
        let _ = self.queue.send((summary.to_owned(), body.to_owned()));
    }
}

fn work(mut run: impl Run, messages: &mpsc::Receiver<(String, String)>) {
    // The service's number for the message on screen, which the next one replaces.
    let mut shown: Option<u32> = None;
    let mut told = false;
    for (summary, body) in messages {
        let mut args: Vec<String> = ["--print-id", "--app-name", "Navette", "--expire-time"]
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

/// A notifier in memory, for the tests: it records what was shown.
#[cfg(test)]
#[derive(Default)]
pub struct Fake {
    shown: std::sync::Mutex<Vec<(String, String)>>,
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
}

#[cfg(test)]
impl Notifier for Fake {
    fn show(&self, summary: &str, body: &str) {
        self.shown
            .lock()
            .unwrap()
            .push((summary.to_owned(), body.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A `notify-send` that numbers its notifications from 7 and tells the test what it was
    /// run with. With `refuse`, it fails as one with no service does.
    struct FakeRun {
        calls: mpsc::Sender<Vec<String>>,
        next: u32,
        refuse: bool,
    }

    impl Run for FakeRun {
        fn run(&mut self, args: &[String]) -> io::Result<String> {
            self.calls.send(args.to_vec()).unwrap();
            if self.refuse {
                return Err(io::Error::other("no notification service"));
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
        notifier.show("Mic → home", "");
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
        notifier.show("Mic → home", "");
        for _ in 0..2 {
            let args = next(&seen);
            assert!(!args.contains(&"--replace-id".to_owned()), "{args:?}");
        }
    }
}
