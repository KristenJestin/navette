//! The remote machine's sound on the wire: Opus, 48 kHz stereo, 10 ms frames, with in-band
//! forward error correction so a dropped frame can be rebuilt from the next one.

use std::io;

/// The graph's sample rate on both machines; PipeWire converts whatever the apps play.
pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples per channel in one 10 ms frame.
pub const FRAME: usize = RATE as usize / 100;
/// Interleaved samples in one frame.
pub const FRAME_SAMPLES: usize = FRAME * CHANNELS;
/// Interleaved samples in the largest cycle a graph runs (8192 frames): the realtime buffers
/// are this big from the start, so a cycle never allocates.
pub const SAMPLES_PER_CYCLE: usize = 8192 * CHANNELS;

/// Far above what Opus makes of 10 ms at the chosen bitrate.
pub const MAX_PACKET: usize = 4000;
const BITRATE: i32 = 128_000;
/// What the encoder plans for: enough to make it spend bits on FEC.
const EXPECTED_LOSS_PERCENT: i32 = 10;
/// Beyond this many lost frames, concealing them would only add latency.
const MAX_CONCEALED: u32 = 5;

fn opus_error(e: opus::Error) -> io::Error {
    io::Error::other(format!("opus: {e}"))
}

/// Cuts the PCM the sink gives, in blocks of any size, into 10 ms frames and encodes them.
pub struct Encoder {
    opus: opus::Encoder,
    pending: Vec<f32>,
    packet: Box<[u8; MAX_PACKET]>,
}

impl Encoder {
    pub fn new() -> io::Result<Self> {
        let mut opus = opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Audio)
            .map_err(opus_error)?;
        opus.set_bitrate(opus::Bitrate::Bits(BITRATE))
            .map_err(opus_error)?;
        opus.set_inband_fec(true).map_err(opus_error)?;
        opus.set_packet_loss_perc(EXPECTED_LOSS_PERCENT)
            .map_err(opus_error)?;
        Ok(Self {
            opus,
            // A cycle, and what was left of a frame from the one before.
            pending: Vec::with_capacity(SAMPLES_PER_CYCLE + FRAME_SAMPLES),
            packet: Box::new([0; MAX_PACKET]),
        })
    }

    /// Takes interleaved stereo samples; hands `each` a packet for each frame they complete.
    pub fn push(&mut self, pcm: &[f32], mut each: impl FnMut(&[u8])) -> io::Result<()> {
        self.pending.extend_from_slice(pcm);
        let whole = self.pending.len() / FRAME_SAMPLES * FRAME_SAMPLES;
        for frame in self.pending[..whole].as_chunks::<FRAME_SAMPLES>().0 {
            let len = self
                .opus
                .encode_float(frame, &mut self.packet[..])
                .map_err(opus_error)?;
            each(&self.packet[..len]);
        }
        self.pending.drain(..whole);
        Ok(())
    }
}

/// Decodes numbered packets back to PCM, filling the frames that never came.
pub struct Decoder {
    opus: opus::Decoder,
    next: Option<u32>,
    gaps: Gaps,
}

/// The frames that did not come as they should, since the decoder was made, for the periodic
/// stats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gaps {
    /// Frames that never came, filled from the next packet's FEC or by concealment.
    pub concealed: u64,
    /// Frames that never came, in a gap too long to fill: the sound jumps over them.
    pub lost: u64,
    /// Packets that came after a later one: too late to play.
    pub late: u64,
}

impl Decoder {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            opus: opus::Decoder::new(RATE, opus::Channels::Stereo).map_err(opus_error)?,
            next: None,
            gaps: Gaps::default(),
        })
    }

    pub fn gaps(&self) -> Gaps {
        self.gaps
    }

    /// Decodes packet `seq` into interleaved stereo samples. A gap before it is filled first:
    /// the frame just before from this packet's FEC, older ones by concealment. A packet older
    /// than the last one decoded gives nothing.
    pub fn decode(&mut self, seq: u32, packet: &[u8]) -> io::Result<Vec<f32>> {
        let missing = match self.next {
            None => 0,
            Some(next) if seq.wrapping_sub(next) > u32::MAX / 2 => {
                self.gaps.late += 1;
                return Ok(Vec::new());
            }
            Some(next) => seq.wrapping_sub(next),
        };
        let mut pcm = Vec::new();
        if (1..=MAX_CONCEALED).contains(&missing) {
            for _ in 1..missing {
                self.decode_into(&[], false, &mut pcm)?;
            }
            self.decode_into(packet, true, &mut pcm)?;
            self.gaps.concealed += u64::from(missing);
        } else if missing > MAX_CONCEALED {
            self.gaps.lost += u64::from(missing);
        }
        self.decode_into(packet, false, &mut pcm)?;
        self.next = Some(seq.wrapping_add(1));
        Ok(pcm)
    }

    fn decode_into(&mut self, packet: &[u8], fec: bool, pcm: &mut Vec<f32>) -> io::Result<()> {
        let start = pcm.len();
        pcm.resize(start + FRAME_SAMPLES, 0.0);
        let per_channel = self
            .opus
            .decode_float(packet, &mut pcm[start..], fec)
            .map_err(opus_error)?;
        pcm.truncate(start + per_channel * CHANNELS);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `frames` frames of a 440 Hz tone on both channels, at half scale.
    fn tone(frames: usize) -> Vec<f32> {
        (0..frames * FRAME)
            .flat_map(|i| {
                let v = 0.5 * (i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin();
                [v, v]
            })
            .collect()
    }

    pub(crate) fn packets(encoder: &mut Encoder, pcm: &[f32]) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        encoder.push(pcm, |p| packets.push(p.to_vec())).unwrap();
        packets
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn blocks_of_any_size_come_out_as_10_ms_frames() {
        let mut encoder = Encoder::new().unwrap();
        let pcm = tone(5);
        // A graph quantum is rarely a whole number of frames.
        let first = packets(&mut encoder, &pcm[..FRAME_SAMPLES + 100]);
        let rest = packets(&mut encoder, &pcm[FRAME_SAMPLES + 100..]);
        assert_eq!((first.len(), rest.len()), (1, 4));
        assert!(first.iter().chain(&rest).all(|p| !p.is_empty()));
    }

    #[test]
    fn encoding_a_cycle_does_not_allocate() {
        let mut encoder = Encoder::new().unwrap();
        let pcm = tone(20);
        let mut packets = 0;
        // A graph quantum's worth at a time, the way the sink hands it over.
        for block in pcm.chunks(1024 * CHANNELS) {
            let allocated = crate::audio::allocations::during(|| {
                encoder.push(block, |_| packets += 1).unwrap();
            });
            assert_eq!(allocated, 0);
        }
        assert_eq!(packets, 20);
    }

    #[test]
    fn frames_go_through_the_codec_and_back() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let input = tone(50);
        let mut output = Vec::new();
        for (seq, packet) in packets(&mut encoder, &input).iter().enumerate() {
            output.extend(decoder.decode(seq as u32, packet).unwrap());
        }
        assert_eq!(output.len(), input.len());
        // Past the codec's start-up, the tone comes back at its level.
        let settled = &output[10 * FRAME_SAMPLES..];
        let ratio = rms(settled) / rms(&input);
        assert!((0.8..1.2).contains(&ratio), "level ratio {ratio}");
    }

    #[test]
    fn a_lost_frame_is_filled_in() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let packets = packets(&mut encoder, &tone(4));
        let mut output = Vec::new();
        for seq in [0u32, 1, 3] {
            output.extend(decoder.decode(seq, &packets[seq as usize]).unwrap());
        }
        assert_eq!(output.len(), 4 * FRAME_SAMPLES);
    }

    #[test]
    fn a_concealed_frame_is_counted() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let packets = packets(&mut encoder, &tone(5));
        for seq in [0u32, 1, 4] {
            decoder.decode(seq, &packets[seq as usize]).unwrap();
        }
        assert_eq!(
            decoder.gaps(),
            Gaps {
                concealed: 2,
                ..Gaps::default()
            }
        );
    }

    #[test]
    fn a_gap_too_long_to_fill_is_counted_as_lost() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let packets = packets(&mut encoder, &tone(10));
        decoder.decode(0, &packets[0]).unwrap();
        decoder.decode(9, &packets[9]).unwrap();
        assert_eq!(
            decoder.gaps(),
            Gaps {
                lost: 8,
                ..Gaps::default()
            }
        );
    }

    #[test]
    fn a_late_frame_is_counted() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let packets = packets(&mut encoder, &tone(3));
        decoder.decode(0, &packets[0]).unwrap();
        decoder.decode(2, &packets[2]).unwrap();
        decoder.decode(1, &packets[1]).unwrap();
        assert_eq!(
            decoder.gaps(),
            Gaps {
                concealed: 1,
                late: 1,
                ..Gaps::default()
            }
        );
    }

    #[test]
    fn a_late_packet_is_dropped() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let packets = packets(&mut encoder, &tone(3));
        decoder.decode(0, &packets[0]).unwrap();
        decoder.decode(2, &packets[2]).unwrap();
        assert!(decoder.decode(1, &packets[1]).unwrap().is_empty());
    }
}
