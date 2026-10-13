//! Home's small buffer between the frames arriving from the link and the playback stream that
//! PipeWire pulls at its own pace: it starts playing once a few frames are in, plays silence
//! when the link falls behind, and drops the oldest sound when the two clocks have drifted
//! too far apart.

use std::collections::VecDeque;

pub struct Jitter {
    samples: VecDeque<f32>,
    target: usize,
    max: usize,
    playing: bool,
    counts: Counts,
}

/// What went wrong since the buffer was made, for the periodic stats: each is a crackle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Pulls that found less than they asked for while playing: a gap of silence.
    pub underruns: u64,
    /// Pushes that dropped the oldest sound, the buffer being past its maximum: a jump.
    pub trims: u64,
}

impl Jitter {
    /// `target` interleaved samples are buffered before playing starts; beyond `max`, the
    /// oldest are dropped back down to `target`.
    pub fn new(target: usize, max: usize) -> Self {
        assert!(target <= max);
        Self {
            samples: VecDeque::with_capacity(max),
            target,
            max,
            playing: false,
            counts: Counts::default(),
        }
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// Interleaved samples buffered now.
    pub fn buffered(&self) -> usize {
        self.samples.len()
    }

    pub fn push(&mut self, pcm: &[f32]) {
        // Trimmed before it grows, so the buffer never outgrows what it was given at first.
        let mut pcm = pcm;
        let total = self.samples.len() + pcm.len();
        if total > self.max {
            let excess = total - self.target;
            let buffered = excess.min(self.samples.len());
            self.samples.drain(..buffered);
            pcm = &pcm[excess - buffered..];
            self.counts.trims += 1;
        }
        self.samples.extend(pcm);
        if self.samples.len() >= self.target {
            self.playing = true;
        }
    }

    /// Fills `out` with what is buffered, silence where nothing is.
    pub fn pull(&mut self, out: &mut [f32]) {
        let mut filled = 0;
        if self.playing {
            filled = out.len().min(self.samples.len());
            for (slot, sample) in out.iter_mut().zip(self.samples.drain(..filled)) {
                *slot = sample;
            }
        }
        out[filled..].fill(0.0);
        if filled < out.len() && self.playing {
            // Fell behind: wait for a full buffer again rather than play in crumbs.
            self.playing = false;
            self.counts.underruns += 1;
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

    #[test]
    fn silence_until_the_buffer_holds_its_target() {
        let mut jitter = Jitter::new(4, 8);
        jitter.push(&[1.0, 2.0]);
        assert_eq!(pulled(&mut jitter, 2), [0.0, 0.0]);
        jitter.push(&[3.0, 4.0]);
        assert_eq!(pulled(&mut jitter, 3), [1.0, 2.0, 3.0]);
        assert_eq!(pulled(&mut jitter, 1), [4.0]);
    }

    #[test]
    fn falling_behind_plays_silence_then_waits_for_the_target_again() {
        let mut jitter = Jitter::new(2, 8);
        jitter.push(&[1.0, 2.0, 3.0]);
        assert_eq!(pulled(&mut jitter, 4), [1.0, 2.0, 3.0, 0.0]);
        jitter.push(&[4.0]);
        assert_eq!(pulled(&mut jitter, 1), [0.0]);
        jitter.push(&[5.0]);
        assert_eq!(pulled(&mut jitter, 2), [4.0, 5.0]);
    }

    #[test]
    fn a_push_past_the_maximum_does_not_allocate() {
        let mut jitter = Jitter::new(2, 4);
        jitter.push(&[1.0, 2.0, 3.0]);
        let allocated = crate::audio::allocations::during(|| jitter.push(&[4.0, 5.0, 6.0]));
        assert_eq!(allocated, 0);
        assert_eq!(pulled(&mut jitter, 2), [5.0, 6.0]);
    }

    #[test]
    fn a_block_longer_than_the_maximum_keeps_its_newest_sound() {
        let mut jitter = Jitter::new(2, 4);
        jitter.push(&[1.0]);
        jitter.push(&[2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        assert_eq!(pulled(&mut jitter, 3), [6.0, 7.0, 0.0]);
    }

    #[test]
    fn an_underrun_is_counted() {
        let mut jitter = Jitter::new(2, 8);
        // Silence before the first sound is no underrun.
        pulled(&mut jitter, 2);
        jitter.push(&[1.0, 2.0, 3.0]);
        pulled(&mut jitter, 2);
        assert_eq!(jitter.counts().underruns, 0);
        pulled(&mut jitter, 2);
        assert_eq!(jitter.counts().underruns, 1);
        // Waiting for the target again is the same gap.
        pulled(&mut jitter, 2);
        assert_eq!(jitter.counts().underruns, 1);
    }

    #[test]
    fn a_trim_is_counted() {
        let mut jitter = Jitter::new(2, 4);
        jitter.push(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(jitter.counts().trims, 0);
        jitter.push(&[5.0]);
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
        jitter.push(&[1.0, 2.0, 3.0, 4.0]);
        jitter.push(&[5.0]);
        assert_eq!(pulled(&mut jitter, 2), [4.0, 5.0]);
    }
}
