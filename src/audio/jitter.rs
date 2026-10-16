//! The desk's small buffer between the frames arriving from the link and the playback stream that
//! PipeWire pulls at its own pace: it starts playing once a few frames are in, and plays
//! silence when the link falls behind. It keeps as little as the link lets it: each time it
//! runs dry it keeps more, and two minutes with room to spare let it keep less. The two machines'
//! clocks drift apart; rather than jump over sound or run dry every few minutes, it asks the
//! playback to take the sound a little faster or slower ([`Jitter::rate`]), which PipeWire's
//! resampler does, so that what it holds stays on its target.

use std::collections::VecDeque;

use super::codec::{CHANNELS, RATE, SAMPLES_PER_CYCLE};

/// Interleaved samples in a second.
const SECOND: usize = RATE as usize * CHANNELS;
/// What the rate steers is what was left after the pulls, smoothed over this many samples: a
/// cycle's ups and downs are no drift.
const SMOOTHING: usize = 2 * SECOND;
/// The rate moves by 0.1 % for each 10 ms off the target...
const GAIN: f64 = 1e-3 / (SECOND / 100) as f64;
/// ...and never by more than 0.5 %: 9 cents, the edge of what an ear tells on a held note.
const MAX_CORRECTION: f64 = 5e-3;
/// A silence longer than this is the remote's sound pausing, not the link falling behind.
const PAUSE: usize = SECOND / 2;
/// How long the buffer must have had room to spare before it keeps less.
const WINDOW: usize = 120 * SECOND;

pub struct Jitter {
    samples: VecDeque<f32>,
    /// What is to be left after each pull, on average.
    target: usize,
    floor: usize,
    ceiling: usize,
    /// The size of the playback's last pull: playing starts once a pull and the target are in.
    pull: usize,
    playing: bool,
    /// The silence played since the buffer ran dry, until it is told an underrun or a pause.
    dry: Option<usize>,
    /// What was left after the pulls, smoothed.
    level: f64,
    /// Samples pulled since the window started, and the least left after a pull in it.
    window: usize,
    low: usize,
    counts: Counts,
}

/// What went wrong since the buffer was made, for the periodic stats: each is a crackle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Gaps of silence, the buffer having run dry while playing; told once the sound is back,
    /// and only when it was back within half a second (longer, the remote's sound paused).
    pub underruns: u64,
    /// Pushes that dropped the oldest sound, the buffer being past its maximum: a jump.
    pub trims: u64,
}

impl Jitter {
    /// At first, a pull's worth and three `floor`s of interleaved samples are to be left after
    /// each pull, never less than a pull and one `floor`; each underrun raises that by `floor`,
    /// up to `ceiling`, and two minutes that always had three `floor`s to spare lower it by
    /// one. A silence of more than half a second is a pause, and no underrun. Beyond `ceiling` more than it starts with, the
    /// oldest sound is dropped.
    pub fn new(floor: usize, ceiling: usize) -> Self {
        assert!(0 < floor && floor <= ceiling);
        Self {
            // Never more than this: it does not grow on PipeWire's thread.
            samples: VecDeque::with_capacity(2 * ceiling + SAMPLES_PER_CYCLE),
            target: floor,
            floor,
            ceiling,
            pull: 0,
            playing: false,
            dry: None,
            level: 0.0,
            window: 0,
            low: usize::MAX,
            counts: Counts::default(),
        }
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// How fast the playback should take the sound, 1 being the graph's rate: faster when the
    /// buffer holds more than its target, slower when less.
    pub fn rate(&self) -> f64 {
        if !self.playing {
            return 1.0;
        }
        let correction = (self.level - self.target as f64) * GAIN;
        1.0 + correction.clamp(-MAX_CORRECTION, MAX_CORRECTION)
    }

    /// Interleaved samples buffered now.
    pub fn buffered(&self) -> usize {
        self.samples.len()
    }

    /// What is to be left after each pull now.
    pub fn target(&self) -> usize {
        self.target
    }

    /// What playing starts with: the next pull, and the target left after it.
    fn start(&self) -> usize {
        self.target + self.pull.min(SAMPLES_PER_CYCLE)
    }

    /// The least the target may be: a pull's worth beyond `floor`. The remote sends a cycle
    /// of its own at a time, a pull's worth when both graphs run the same quantum; as the
    /// clocks drift, one of them now and then reaches the desk just after the pull it used to be
    /// in time for, and that pull finds a cycle less. Only a drift of the phase shows it, every
    /// few minutes: no window would see it coming.
    fn least(&self) -> usize {
        (self.pull.min(SAMPLES_PER_CYCLE) + self.floor).min(self.ceiling)
    }

    pub fn push(&mut self, pcm: &[f32]) {
        // Trimmed before it grows, so the buffer never outgrows what it was given at first.
        let start = self.start();
        let mut pcm = pcm;
        let total = self.samples.len() + pcm.len();
        if total > start + self.ceiling {
            let excess = total - start;
            let buffered = excess.min(self.samples.len());
            self.samples.drain(..buffered);
            pcm = &pcm[excess - buffered..];
            self.counts.trims += 1;
        }
        self.samples.extend(pcm);
        if !self.playing && self.samples.len() >= start {
            // The sound is back after a gap: short, the link fell behind, and from now on the
            // buffer waits for more.
            if self.dry.take().is_some_and(|silence| silence < PAUSE) {
                self.counts.underruns += 1;
                self.target = (self.target + self.floor).min(self.ceiling);
                if self.samples.len() < self.start() {
                    return;
                }
            }
            self.playing = true;
            self.level = self.target as f64;
        }
    }

    /// Fills `out` with what is buffered, silence where nothing is.
    pub fn pull(&mut self, out: &mut [f32]) {
        let first = self.pull == 0;
        self.pull = out.len();
        self.target = if first {
            // Room to spare at first, which two calm minutes take back: the link has not shown
            // how it behaves yet.
            (self.least() + 2 * self.floor).min(self.ceiling)
        } else {
            self.target.max(self.least())
        };
        let mut filled = 0;
        if self.playing {
            filled = out.len().min(self.samples.len());
            for (slot, sample) in out.iter_mut().zip(self.samples.drain(..filled)) {
                *slot = sample;
            }
        }
        out[filled..].fill(0.0);
        if !self.playing {
            if let Some(silence) = &mut self.dry {
                *silence += out.len();
            }
            return;
        }
        if filled < out.len() {
            // Ran dry: wait for a full buffer again rather than play in crumbs.
            self.playing = false;
            self.dry = Some(out.len() - filled);
            self.window = 0;
            self.low = usize::MAX;
            return;
        }
        let left = self.samples.len();
        let weight = (out.len() as f64 / SMOOTHING as f64).min(1.0);
        self.level += (left as f64 - self.level) * weight;
        self.low = self.low.min(left);
        self.window += out.len();
        if self.window >= WINDOW {
            if self.low >= 3 * self.floor {
                self.target = (self.target - self.floor).max(self.least());
            }
            self.window = 0;
            self.low = usize::MAX;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pulled(jitter: &mut Jitter, n: usize) -> Vec<f32> {
        let mut out = vec![9.0; n];
        jitter.pull(&mut out);
        out
    }

    /// What the tests push: `from..=to`.
    fn sound(from: u8, to: u8) -> Vec<f32> {
        (from..=to).map(f32::from).collect()
    }

    #[test]
    fn silence_until_the_buffer_holds_a_pull_and_its_target() {
        let mut jitter = Jitter::new(2, 20);
        // The playback asks before any sound comes: 2 now, and 2 + 3 × 2 to keep after a pull.
        assert_eq!(pulled(&mut jitter, 2), [0.0, 0.0]);
        jitter.push(&sound(1, 9));
        assert_eq!(pulled(&mut jitter, 2), [0.0, 0.0]);
        jitter.push(&[10.0]);
        assert_eq!(pulled(&mut jitter, 2), [1.0, 2.0]);
    }

    /// A buffer that keeps 8 after a pull of 2, and has just run dry.
    fn run_dry() -> Jitter {
        let mut jitter = Jitter::new(2, 20);
        pulled(&mut jitter, 2);
        jitter.push(&sound(1, 10));
        for _ in 0..5 {
            pulled(&mut jitter, 2);
        }
        assert_eq!(pulled(&mut jitter, 2), [0.0, 0.0]);
        jitter
    }

    #[test]
    fn falling_behind_plays_silence_then_waits_for_more_than_before() {
        let mut jitter = run_dry();
        // It now keeps 10 after a pull, not 8.
        jitter.push(&sound(11, 20));
        assert_eq!(pulled(&mut jitter, 2), [0.0, 0.0]);
        jitter.push(&sound(21, 22));
        assert_eq!(pulled(&mut jitter, 2), [11.0, 12.0]);
    }

    #[test]
    fn an_underrun_is_counted() {
        let mut jitter = Jitter::new(2, 20);
        // Silence before the first sound is no underrun.
        pulled(&mut jitter, 2);
        assert_eq!(jitter.counts().underruns, 0);
        let mut jitter = run_dry();
        // Waiting for the target again is the same gap.
        pulled(&mut jitter, 2);
        jitter.push(&sound(11, 22));
        assert_eq!(jitter.counts().underruns, 1);
    }

    #[test]
    fn a_pause_in_the_sound_is_no_underrun() {
        let mut jitter = run_dry();
        for _ in 0..PAUSE / 2 {
            pulled(&mut jitter, 2);
        }
        jitter.push(&sound(11, 20));
        assert_eq!(jitter.counts().underruns, 0);
        // Nor does it keep more: it plays as soon as it holds what it did before.
        assert_eq!(pulled(&mut jitter, 2), [11.0, 12.0]);
    }

    #[test]
    fn a_push_past_the_maximum_does_not_allocate() {
        let mut jitter = Jitter::new(2, 4);
        jitter.push(&sound(1, 3));
        let more = sound(4, 7);
        let allocated = crate::audio::allocations::during(|| jitter.push(&more));
        assert_eq!(allocated, 0);
        assert_eq!(pulled(&mut jitter, 2), [6.0, 7.0]);
    }

    #[test]
    fn a_block_longer_than_the_maximum_keeps_its_newest_sound() {
        let mut jitter = Jitter::new(2, 4);
        jitter.push(&[1.0]);
        jitter.push(&sound(2, 7));
        assert_eq!(pulled(&mut jitter, 3), [6.0, 7.0, 0.0]);
    }

    #[test]
    fn a_trim_is_counted() {
        let mut jitter = Jitter::new(2, 4);
        jitter.push(&sound(1, 6));
        assert_eq!(jitter.counts().trims, 0);
        jitter.push(&[7.0]);
        assert_eq!(
            jitter.counts(),
            Counts {
                underruns: 0,
                trims: 1
            }
        );
    }

    #[test]
    fn past_its_maximum_the_oldest_sound_is_dropped() {
        let mut jitter = Jitter::new(2, 4);
        jitter.push(&sound(1, 6));
        jitter.push(&[7.0]);
        assert_eq!(pulled(&mut jitter, 2), [6.0, 7.0]);
    }

    /// A deterministic stand-in for chance (xorshift), in [0, 1).
    struct Chance(u64);

    impl Chance {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    use crate::audio::codec::{CHANNELS, FRAME, FRAME_SAMPLES, RATE};

    /// Both graphs run 1024-frame cycles, PipeWire's default quantum.
    const QUANTUM: usize = 1024;

    /// The desk's Wi-Fi as the laptop sees it: a few ms most of the time, and every few seconds a
    /// stall of 20 to 50 ms (a retransmission, the radio waking up), in seconds.
    fn wifi(chance: &mut Chance) -> f64 {
        let mut ms = 1.0 - 2.0 * (1.0 - chance.next()).ln();
        if chance.next() < 0.002 {
            ms += 20.0 + 30.0 * chance.next();
        }
        ms / 1000.0
    }

    fn calm(_: &mut Chance) -> f64 {
        0.003
    }

    /// How long each frame takes to reach the desk, in seconds.
    type Delay<'a> = &'a mut dyn FnMut(&mut Chance) -> f64;

    /// What a stretch of a simulated session gave.
    #[derive(Debug)]
    struct Stretch {
        counts: Counts,
        /// What the buffer held, on average, each time the playback came for sound: the
        /// latency it adds.
        mean_ms: f64,
    }

    /// A session, as `jitter` would play it at the desk: the remote's sink encodes its cycles into
    /// 10 ms frames, on a clock `drift_ppm` faster than the desk's; each frame reaches the desk after
    /// `delay`, in order, as on TCP; the desk's playback takes a cycle's worth at the rate the
    /// buffer asks for. One stretch per pair of `(seconds, delay)`, one after the other.
    fn session(
        jitter: &mut Jitter,
        drift_ppm: f64,
        stretches: &mut [(f64, Delay)],
    ) -> Vec<Stretch> {
        let mut chance = Chance(0x9e37_79b9_7f4a_7c15);
        let cycle = QUANTUM as f64 / RATE as f64;
        let remote_cycle = cycle / (1.0 + drift_ppm * 1e-6);
        let frame = [0.25f32; FRAME_SAMPLES];
        let mut out = vec![0.0f32; 2 * QUANTUM * CHANNELS];
        let (mut sent, mut pending, mut last_arrival) = (0u64, 0usize, 0.0f64);
        let mut arrivals = std::collections::VecDeque::new();
        let mut owed = 0.0;
        let mut now = 0.0;
        let mut home_cycles = 0u64;
        let mut heard = Vec::new();
        for (seconds, delay) in stretches.iter_mut() {
            let before = jitter.counts();
            let (mut level, mut pulls) = (0.0, 0u64);
            let end = now + *seconds;
            while now < end {
                // The desk's cycles start a third of a cycle after the remote's.
                now = (home_cycles as f64 + 0.33) * cycle;
                home_cycles += 1;
                // The remote starts a cycle after the desk's playback, which asks for sound before
                // any comes.
                while (sent + 1) as f64 * remote_cycle <= now {
                    let at = (sent + 1) as f64 * remote_cycle;
                    sent += 1;
                    pending += QUANTUM;
                    while pending >= FRAME {
                        pending -= FRAME;
                        last_arrival = f64::max(last_arrival, at + delay(&mut chance));
                        arrivals.push_back(last_arrival);
                    }
                }
                while arrivals.front().is_some_and(|&at| at <= now) {
                    arrivals.pop_front();
                    jitter.push(&frame);
                }
                level += jitter.buffered() as f64;
                pulls += 1;
                owed += QUANTUM as f64 * jitter.rate();
                let frames = owed as usize;
                owed -= frames as f64;
                jitter.pull(&mut out[..frames * CHANNELS]);
            }
            let after = jitter.counts();
            heard.push(Stretch {
                counts: Counts {
                    underruns: after.underruns - before.underruns,
                    trims: after.trims - before.trims,
                },
                mean_ms: level / pulls as f64 / (FRAME_SAMPLES / 10) as f64,
            });
        }
        heard
    }

    /// The desk's buffer, as the playback has it.
    fn home() -> Jitter {
        Jitter::new(crate::audio::JITTER_FLOOR, crate::audio::JITTER_CEILING)
    }

    const NONE: Counts = Counts {
        underruns: 0,
        trims: 0,
    };

    #[test]
    fn wifi_jitter_plays_without_an_underrun_once_the_buffer_has_adapted() {
        let heard = session(
            &mut home(),
            0.0,
            &mut [(180.0, &mut wifi), (600.0, &mut wifi)],
        );
        assert_eq!(heard[1].counts, NONE, "{heard:?}");
        assert!(heard[1].mean_ms < 100.0, "{heard:?}");
    }

    #[test]
    fn a_clock_drift_plays_without_a_trim_or_an_underrun() {
        for drift_ppm in [-200.0, 200.0] {
            let heard = session(
                &mut home(),
                drift_ppm,
                &mut [(1.0, &mut calm), (1800.0, &mut calm)],
            );
            assert_eq!(heard[1].counts, NONE, "{drift_ppm} ppm: {heard:?}");
        }
    }

    #[test]
    fn the_room_kept_at_first_is_given_back_on_a_calm_link() {
        let heard = session(
            &mut home(),
            50.0,
            &mut [(1.0, &mut calm), (900.0, &mut calm), (300.0, &mut calm)],
        );
        assert_eq!(heard[1].counts, NONE, "{heard:?}");
        // A cycle's worth and 10 ms after each cycle: 52 ms before it.
        assert!(heard[2].mean_ms < 55.0, "{heard:?}");
    }

    #[test]
    fn the_buffer_comes_back_down_once_the_link_is_calm() {
        let heard = session(
            &mut home(),
            50.0,
            &mut [(120.0, &mut wifi), (600.0, &mut calm), (60.0, &mut calm)],
        );
        assert_eq!(heard[2].counts, NONE, "{heard:?}");
        assert!(heard[2].mean_ms < 60.0, "{heard:?}");
    }
}
