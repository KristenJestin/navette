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
        }
    }

    pub fn push(&mut self, pcm: &[f32]) {
        self.samples.extend(pcm);
        if self.samples.len() > self.max {
            let excess = self.samples.len() - self.target;
            self.samples.drain(..excess);
        }
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
        if filled < out.len() {
            // Fell behind: wait for a full buffer again rather than play in crumbs.
            self.playing = false;
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
    fn past_its_maximum_the_oldest_sound_is_dropped() {
        let mut jitter = Jitter::new(2, 4);
        jitter.push(&[1.0, 2.0, 3.0, 4.0]);
        jitter.push(&[5.0]);
        assert_eq!(pulled(&mut jitter, 2), [4.0, 5.0]);
    }
}
