//! What the user sees of the files crossing, both ways: one bar per transfer, at home in the
//! corner, and a notification when a paste fails or is stopped; and how they stop one, by a
//! click on its bar. The pasting side counts the bytes it writes, the copying side the bytes it
//! serves: home shows both, whichever machine pasted.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gliff_transport::clipboard::progress::Meter;
use gliff_transport::clipboard::{ChunkSource, IDLE_TIMEOUT};
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::notify::Notifier;

/// How often the bar moves while files cross.
pub const CADENCE: Duration = Duration::from_millis(250);

/// What tells a copy of files from another: the name of the first file or folder it holds, and
/// how many more there are at its top.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Headline {
    pub name: String,
    pub more: usize,
}

/// One transfer of files crossing now: bytes done of the bytes it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bar {
    /// Which transfer: what a click on its bar names.
    pub id: u64,
    pub done: u64,
    pub total: u64,
    /// The copy's headline, from whichever machine knows its list of files; none when the
    /// list is not known.
    pub headline: Option<Headline>,
}

impl Bar {
    pub fn left(&self) -> u64 {
        self.total.saturating_sub(self.done)
    }
}

/// A click on the bar of a transfer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cancel {
    /// Counts the clicks, so that two on the same bar are two changes.
    pub clicks: u64,
    /// The [`Bar::id`] clicked.
    pub transfer: u64,
}

/// Where the bridge shows the files crossing, and hears the user stop them.
#[derive(Clone)]
pub struct Shown {
    /// The bars of the transfers crossing, both ways, in the order they started; none once
    /// they are all done.
    pub bar: Arc<dyn Fn(Vec<Bar>) + Send + Sync>,
    /// A paste's failure or cancel, on the machine that pasted; what a copy left out, on the
    /// machine that copied.
    pub notifier: Arc<dyn Notifier>,
    /// Each change stops the transfer it names.
    pub cancel: watch::Receiver<Cancel>,
}

impl Shown {
    /// No bar and nobody to cancel: notifications only. The remote machine's, whose transfers
    /// home shows and stops.
    pub fn notifications(notifier: Arc<dyn Notifier>) -> Self {
        Self {
            bar: Arc::new(|_| {}),
            notifier,
            cancel: watch::channel(Cancel::default()).1,
        }
    }
}

/// One set of files crossing: a transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Crossing {
    /// A paste on this machine, by its own number.
    Paste(u32),
    /// The peer pasting the copy of this serial, served from here.
    Serve(u32),
    /// Applications here reading the files of the peer's copy of this serial.
    Read(u32),
}

impl Crossing {
    /// Its bar's [`Bar::id`].
    pub fn id(self) -> u64 {
        let (serial, kind) = match self {
            Crossing::Paste(n) => (n, 0),
            Crossing::Serve(serial) => (serial, 1),
            Crossing::Read(serial) => (serial, 2),
        };
        u64::from(serial) << 2 | kind
    }
}

/// How a crossing is stopped.
pub enum Stop {
    /// The paste's task is dropped: its fetch tells the peer, its staging folder goes.
    Task(AbortHandle),
    /// Each fetch of the reads is dropped, those to come with them.
    Tasks(Rc<RefCell<Vec<AbortHandle>>>),
    /// The served file's next read fails: gliff's engine tells the peer.
    Source(Rc<Cell<bool>>),
}

struct Running {
    /// Tells this run from a later one of the same crossing.
    run: u64,
    headline: Option<Headline>,
    done: Meter,
    total: u64,
    stop: Stop,
    /// Since when a served crossing waits for the peer to ask for its next file.
    idle: Option<Instant>,
}

/// The crossings of one bridge.
#[derive(Clone)]
pub struct Progress {
    running: Rc<RefCell<HashMap<Crossing, Running>>>,
    bar: Arc<dyn Fn(Vec<Bar>) + Send + Sync>,
    ticking: Rc<Cell<bool>>,
    runs: Rc<Cell<u64>>,
    /// The copies whose serving the user stopped: the peer's next requests for them are
    /// refused, rather than start them again. Whether the refusal was said already.
    stopped: Rc<RefCell<HashMap<u32, bool>>>,
}

impl Progress {
    pub fn new(bar: Arc<dyn Fn(Vec<Bar>) + Send + Sync>) -> Self {
        Self {
            running: Rc::default(),
            bar,
            ticking: Rc::default(),
            runs: Rc::default(),
            stopped: Rc::default(),
        }
    }

    /// `crossing` starts, `total` bytes to go through `done`; the bar shows it from now, at
    /// [`CADENCE`]. Its run, to end it with. Needs a `LocalSet`.
    pub fn start(
        &self,
        crossing: Crossing,
        headline: Option<Headline>,
        total: u64,
        done: Meter,
        stop: Stop,
    ) -> u64 {
        let run = self.runs.get() + 1;
        self.runs.set(run);
        self.running.borrow_mut().insert(
            crossing,
            Running {
                run,
                headline,
                done,
                total,
                stop,
                idle: None,
            },
        );
        if !self.ticking.replace(true) {
            let progress = self.clone();
            tokio::task::spawn_local(async move {
                loop {
                    let bars = progress.now();
                    let done = bars.is_empty();
                    (progress.bar)(bars);
                    if done {
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
    /// `total` bytes to serve unless it runs already; none once the user stopped it.
    pub fn serving(
        &self,
        serial: u32,
        headline: Option<Headline>,
        total: u64,
    ) -> Option<(Meter, Rc<Cell<bool>>, u64)> {
        if self.stopped.borrow().contains_key(&serial) {
            return None;
        }
        let crossing = Crossing::Serve(serial);
        if let Some(running) = self.running.borrow_mut().get_mut(&crossing)
            && let Stop::Source(stopped) = &running.stop
        {
            running.idle = None;
            return Some((running.done.clone(), stopped.clone(), running.run));
        }
        let (done, stopped) = (Meter::default(), Rc::new(Cell::new(false)));
        let run = self.start(
            crossing,
            headline,
            total,
            done.clone(),
            Stop::Source(stopped.clone()),
        );
        Some((done, stopped, run))
    }

    /// Whether this is the first refusal of the stopped copy `serial` to say: one cancel is said
    /// once, however many times the peer's application asks again.
    pub fn first_refusal(&self, serial: u32) -> bool {
        self.stopped
            .borrow_mut()
            .get_mut(&serial)
            .is_some_and(|said| !std::mem::replace(said, true))
    }

    /// `crossing`'s `run` is over; the bar loses it at its next move.
    pub fn end(&self, crossing: Crossing, run: u64) {
        let mut running = self.running.borrow_mut();
        if running.get(&crossing).is_some_and(|r| r.run == run) {
            running.remove(&crossing);
        }
    }

    /// The copy `serial` was made again: the peer may paste it again.
    pub fn resume(&self, serial: u32) {
        self.stopped.borrow_mut().remove(&serial);
    }

    /// Stops the paste or the reads `crossing` if they still run, as a click on the bar would
    /// but without a word; whether they ran.
    pub fn stop(&self, crossing: Crossing) -> bool {
        let removed = self.running.borrow_mut().remove(&crossing);
        match removed.map(|running| running.stop) {
            Some(Stop::Task(task)) => task.abort(),
            Some(Stop::Tasks(tasks)) => tasks.borrow().iter().for_each(AbortHandle::abort),
            _ => return false,
        }
        true
    }

    /// The meter, the fetches and the run of the reads of the peer's copy `serial`, started
    /// now with `total` bytes to read, `done` of them already, unless they run already.
    pub fn reading(
        &self,
        serial: u32,
        headline: Option<Headline>,
        total: u64,
        done: Meter,
    ) -> (Meter, Rc<RefCell<Vec<AbortHandle>>>, u64) {
        let crossing = Crossing::Read(serial);
        if let Some(running) = self.running.borrow().get(&crossing)
            && let Stop::Tasks(tasks) = &running.stop
        {
            return (running.done.clone(), tasks.clone(), running.run);
        }
        let tasks = Rc::new(RefCell::new(Vec::new()));
        let run = self.start(
            crossing,
            headline,
            total,
            done.clone(),
            Stop::Tasks(tasks.clone()),
        );
        (done, tasks, run)
    }

    /// `crossing`'s `run` served a file whole and waits for the peer to ask for the next one:
    /// it ends unless one comes within gliff's idle timeout, the peer's paste gone.
    fn idle(&self, crossing: Crossing, run: u64) {
        if let Some(running) = self.running.borrow_mut().get_mut(&crossing)
            && running.run == run
        {
            running.idle = Some(Instant::now());
        }
    }

    /// Stops the crossing whose bar is `id`, the others going on; the crossing stopped.
    pub fn cancel(&self, id: u64) -> Option<Crossing> {
        let crossing = *self.running.borrow().keys().find(|c| c.id() == id)?;
        let running = self.running.borrow_mut().remove(&crossing)?;
        (self.bar)(self.now());
        match running.stop {
            Stop::Task(task) => task.abort(),
            Stop::Tasks(tasks) => tasks.borrow().iter().for_each(AbortHandle::abort),
            Stop::Source(stopped) => stopped.set(true),
        }
        if let Crossing::Serve(serial) = crossing {
            self.stopped.borrow_mut().insert(serial, false);
        }
        Some(crossing)
    }

    fn now(&self) -> Vec<Bar> {
        let mut running = self.running.borrow_mut();
        running.retain(|_, r| r.idle.is_none_or(|since| since.elapsed() < IDLE_TIMEOUT));
        let mut bars: Vec<_> = running
            .iter()
            .map(|(crossing, r)| {
                let bar = Bar {
                    id: crossing.id(),
                    done: r.done.get(),
                    total: r.total,
                    headline: r.headline.clone(),
                };
                (r.run, bar)
            })
            .collect();
        bars.sort_unstable_by_key(|(run, _)| *run);
        bars.into_iter().map(|(_, bar)| bar).collect()
    }
}

/// A copied file served to the peer: its bytes counted in its crossing's meter, its reads
/// failing once the crossing is stopped. The crossing ends with the last of its bytes, with a
/// file that did not reach its end or not at its size in the offer (the peer's paste fails
/// there), or when the peer asks for no next file.
pub struct Served<S> {
    pub source: S,
    /// The file's size in the offer.
    pub size: u64,
    /// The bytes read of it so far.
    pub read: u64,
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
            Some(bytes) => {
                self.read += bytes.len() as u64;
                self.done.add(bytes.len() as u64);
            }
            None => self.finished = true,
        }
        Ok(chunk)
    }
}

impl<S> Drop for Served<S> {
    fn drop(&mut self) {
        if !self.finished || self.read != self.size || self.done.get() >= self.total {
            if let Crossing::Serve(serial) = self.crossing {
                let how = if !self.finished {
                    "stopped in the middle of a file"
                } else if self.read != self.size {
                    "ended, a file was not the size the copy said"
                } else {
                    "served whole"
                };
                eprintln!("navette: clipboard: offer {serial}: {how}");
            }
            self.progress.end(self.crossing, self.run);
        } else {
            self.progress.idle(self.crossing, self.run);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use gliff_transport::clipboard::IDLE_TIMEOUT;
    use tokio::task::LocalSet;

    use super::*;

    /// A copied file whose bytes are already read: `chunks`, then its end.
    struct Chunks(VecDeque<Bytes>);

    impl ChunkSource for Chunks {
        async fn next(&mut self) -> io::Result<Option<Bytes>> {
            Ok(self.0.pop_front())
        }
    }

    /// The bars shown, and the bridge's tracker that shows them.
    fn tracker() -> (Progress, Arc<Mutex<Vec<Vec<Bar>>>>) {
        let bars = Arc::new(Mutex::new(Vec::new()));
        let shown = bars.clone();
        let progress = Progress::new(Arc::new(move |bar| shown.lock().unwrap().push(bar)));
        (progress, bars)
    }

    /// Serves one file of copy 1, offered at `size` bytes, the copy's files holding `total`,
    /// whose source holds `held` bytes: to its end, then dropped as gliff drops it.
    async fn serve(progress: &Progress, size: u64, total: u64, held: usize) {
        let (done, stopped, run) = progress.serving(1, None, total).unwrap();
        let mut served = Served {
            source: Chunks(VecDeque::from([Bytes::from(vec![0; held])])),
            size,
            read: 0,
            done,
            stopped,
            progress: progress.clone(),
            crossing: Crossing::Serve(1),
            run,
            total,
            finished: false,
        };
        while served.next().await.unwrap().is_some() {}
    }

    fn last(bars: &Mutex<Vec<Vec<Bar>>>) -> Vec<Bar> {
        bars.lock().unwrap().last().unwrap().clone()
    }

    /// The bar of copy 1 served, at `done` bytes of 10.
    fn served(done: u64) -> Vec<Bar> {
        let id = Crossing::Serve(1).id();
        vec![Bar {
            id,
            done,
            total: 10,
            headline: None,
        }]
    }

    #[tokio::test(start_paused = true)]
    async fn a_served_file_that_shrank_since_the_copy_ends_its_crossing() {
        LocalSet::new()
            .run_until(async {
                let (progress, bars) = tracker();
                // Offered at 10 bytes, 4 left when the peer asks: its paste fails there.
                serve(&progress, 10, 10, 4).await;
                tokio::time::sleep(CADENCE * 2).await;
                assert_eq!(last(&bars), []);
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_paste_that_stops_between_two_files_ends_its_crossing() {
        LocalSet::new()
            .run_until(async {
                let (progress, bars) = tracker();
                // The first of two files is served whole, then the peer asks for nothing more.
                serve(&progress, 4, 10, 4).await;
                tokio::time::sleep(CADENCE * 2).await;
                assert_eq!(last(&bars), served(4));
                tokio::time::sleep(IDLE_TIMEOUT).await;
                assert_eq!(last(&bars), []);
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_next_file_asked_in_time_keeps_the_crossing() {
        LocalSet::new()
            .run_until(async {
                let (progress, bars) = tracker();
                serve(&progress, 4, 10, 4).await;
                tokio::time::sleep(IDLE_TIMEOUT / 2).await;
                let (_done, _stopped, _run) = progress.serving(1, None, 10).unwrap();
                tokio::time::sleep(IDLE_TIMEOUT).await;
                assert_eq!(last(&bars), served(4));
            })
            .await;
    }
    #[tokio::test(start_paused = true)]
    async fn two_transfers_have_a_bar_each_and_a_cancel_stops_only_its_own() {
        LocalSet::new()
            .run_until(async {
                let (progress, bars) = tracker();
                let photos = Headline {
                    name: "photos".into(),
                    more: 3,
                };
                // One copy knows its name, the other does not.
                let (first, first_stopped, _) =
                    progress.serving(1, Some(photos.clone()), 10).unwrap();
                let (second, _, _) = progress.serving(2, None, 20).unwrap();
                first.add(3);
                second.add(5);
                tokio::time::sleep(CADENCE * 2).await;
                let (one, two) = (Crossing::Serve(1).id(), Crossing::Serve(2).id());
                assert_eq!(
                    last(&bars),
                    [
                        Bar {
                            id: one,
                            done: 3,
                            total: 10,
                            headline: Some(photos),
                        },
                        Bar {
                            id: two,
                            done: 5,
                            total: 20,
                            headline: None,
                        }
                    ]
                );
                assert_eq!(progress.cancel(one), Some(Crossing::Serve(1)));
                assert!(first_stopped.get());
                tokio::time::sleep(CADENCE * 2).await;
                assert_eq!(
                    last(&bars),
                    [Bar {
                        id: two,
                        done: 5,
                        total: 20,
                        headline: None,
                    }]
                );
                assert!(
                    progress.serving(1, None, 10).is_none(),
                    "its next file is refused"
                );
            })
            .await;
    }
}
