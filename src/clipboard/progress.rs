//! What the user sees of the files crossing, both ways: one bar for all of them, at home in
//! the corner, and a notification when a paste ends; and how they stop them all, by a click on
//! the bar. The pasting side counts the bytes it writes, the copying side the bytes it serves:
//! home shows both, whichever machine pasted.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gliff_transport::clipboard::ChunkSource;
use gliff_transport::clipboard::progress::Meter;
use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::notify::Notifier;

/// How often the bar moves while files cross.
pub const CADENCE: Duration = Duration::from_millis(250);

/// The files crossing now: bytes done of the bytes they hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub done: u64,
    pub total: u64,
}

impl Bar {
    pub fn left(&self) -> u64 {
        self.total.saturating_sub(self.done)
    }
}

/// Where the bridge shows the files crossing, and hears the user stop them.
#[derive(Clone)]
pub struct Shown {
    /// The bar of every file crossing, both ways; none once they are all done.
    pub bar: Arc<dyn Fn(Option<Bar>) + Send + Sync>,
    /// A paste's end, or its failure, on the machine that pasted; what a copy left out, on the
    /// machine that copied.
    pub notifier: Arc<dyn Notifier>,
    /// Each change stops every file crossing.
    pub cancel: watch::Receiver<u64>,
}

impl Shown {
    /// No bar and nobody to cancel: notifications only. The remote machine's, whose transfers
    /// home shows and stops.
    pub fn notifications(notifier: Arc<dyn Notifier>) -> Self {
        Self {
            bar: Arc::new(|_| {}),
            notifier,
            cancel: watch::channel(0).1,
        }
    }
}

/// One set of files crossing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Crossing {
    /// A paste on this machine, by its own number.
    Paste(u32),
    /// The peer pasting the copy of this serial, served from here.
    Serve(u32),
}

/// How a crossing is stopped.
pub enum Stop {
    /// The paste's task is dropped: its fetch tells the peer, its staging folder goes.
    Task(AbortHandle),
    /// The served file's next read fails: gliff's engine tells the peer.
    Source(Rc<Cell<bool>>),
}

struct Running {
    /// Tells this run from a later one of the same crossing.
    run: u64,
    done: Meter,
    total: u64,
    stop: Stop,
}

/// The crossings of one bridge.
#[derive(Clone)]
pub struct Progress {
    running: Rc<RefCell<HashMap<Crossing, Running>>>,
    bar: Arc<dyn Fn(Option<Bar>) + Send + Sync>,
    ticking: Rc<Cell<bool>>,
    runs: Rc<Cell<u64>>,
}

impl Progress {
    pub fn new(bar: Arc<dyn Fn(Option<Bar>) + Send + Sync>) -> Self {
        Self {
            running: Rc::default(),
            bar,
            ticking: Rc::default(),
            runs: Rc::default(),
        }
    }

    /// `crossing` starts, `total` bytes to go through `done`; the bar shows it from now, at
    /// [`CADENCE`]. Its run, to end it with. Needs a `LocalSet`.
    pub fn start(&self, crossing: Crossing, total: u64, done: Meter, stop: Stop) -> u64 {
        let run = self.runs.get() + 1;
        self.runs.set(run);
        self.running.borrow_mut().insert(
            crossing,
            Running {
                run,
                done,
                total,
                stop,
            },
        );
        if !self.ticking.replace(true) {
            let progress = self.clone();
            tokio::task::spawn_local(async move {
                loop {
                    let bar = progress.now();
                    (progress.bar)(bar);
                    if bar.is_none() {
                        progress.ticking.set(false);
                        return;
                    }
                    tokio::time::sleep(CADENCE).await;
                }
            });
        }
        run
    }

    /// The meter, stop flag and run of the peer's paste of copy `serial`, started now with
    /// `total` bytes to serve unless it runs already.
    pub fn serving(&self, serial: u32, total: u64) -> (Meter, Rc<Cell<bool>>, u64) {
        let crossing = Crossing::Serve(serial);
        if let Some(running) = self.running.borrow().get(&crossing)
            && let Stop::Source(stopped) = &running.stop
        {
            return (running.done.clone(), stopped.clone(), running.run);
        }
        let (done, stopped) = (Meter::default(), Rc::new(Cell::new(false)));
        let run = self.start(crossing, total, done.clone(), Stop::Source(stopped.clone()));
        (done, stopped, run)
    }

    /// `crossing`'s `run` is over; the bar loses it at its next move.
    pub fn end(&self, crossing: Crossing, run: u64) {
        let mut running = self.running.borrow_mut();
        if running.get(&crossing).is_some_and(|r| r.run == run) {
            running.remove(&crossing);
        }
    }

    /// Stops every crossing; the pastes stopped.
    pub fn cancel(&self) -> Vec<Crossing> {
        let stopped: Vec<_> = self.running.borrow_mut().drain().collect();
        if !stopped.is_empty() {
            (self.bar)(None);
        }
        stopped
            .into_iter()
            .filter_map(|(crossing, running)| {
                match running.stop {
                    Stop::Task(task) => task.abort(),
                    Stop::Source(stopped) => stopped.set(true),
                }
                matches!(crossing, Crossing::Paste(_)).then_some(crossing)
            })
            .collect()
    }

    fn now(&self) -> Option<Bar> {
        let running = self.running.borrow();
        (!running.is_empty()).then(|| Bar {
            // Each total is counted, not their sum: several near the limit saturate it.
            done: running
                .values()
                .fold(0, |sum, r| sum.saturating_add(r.done.get())),
            total: running
                .values()
                .fold(0, |sum, r| sum.saturating_add(r.total)),
        })
    }
}

/// A copied file served to the peer: its bytes counted in its crossing's meter, its reads
/// failing once the crossing is stopped. The crossing ends with the last of its bytes, or with
/// a file that did not reach its end.
pub struct Served<S> {
    pub source: S,
    pub done: Meter,
    pub stopped: Rc<Cell<bool>>,
    pub progress: Progress,
    pub crossing: Crossing,
    pub run: u64,
    pub total: u64,
    pub finished: bool,
}

impl<S: ChunkSource> ChunkSource for Served<S> {
    async fn next(&mut self) -> io::Result<Option<Bytes>> {
        if self.stopped.get() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let chunk = self.source.next().await?;
        match &chunk {
            Some(bytes) => self.done.add(bytes.len() as u64),
            None => self.finished = true,
        }
        Ok(chunk)
    }
}

impl<S> Drop for Served<S> {
    fn drop(&mut self) {
        if !self.finished || self.done.get() >= self.total {
            self.progress.end(self.crossing, self.run);
        }
    }
}
