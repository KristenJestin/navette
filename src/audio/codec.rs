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

/// Far above what Opus makes of 10 ms at the chosen bitrate.
const MAX_PACKET: usize = 4000;
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
            pending: Vec::with_capacity(FRAME_SAMPLES * 4),
        })
    }

    /// Takes interleaved stereo samples; returns a packet for each frame they complete.
    pub fn push(&mut self, pcm: &[f32]) -> io::Result<Vec<Vec<u8>>> {
        self.pending.extend_from_slice(pcm);
        let mut packets = Vec::new();
        let whole = self.pending.len() / FRAME_SAMPLES * FRAME_SAMPLES;
        for frame in self.pending[..whole].as_chunks::<FRAME_SAMPLES>().0 {
            packets.push(
                self.opus
                    .encode_vec_float(frame, MAX_PACKET)
                    .map_err(opus_error)?,
            );
        }
        self.pending.drain(..whole);
        Ok(packets)
    }
}

/// Decodes numbered packets back to PCM, filling the frames that never came.
pub struct Decoder {
    opus: opus::Decoder,
    next: Option<u32>,
}

impl Decoder {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            opus: opus::Decoder::new(RATE, opus::Channels::Stereo).map_err(opus_error)?,
            next: None,
        })
    }

    /// Decodes packet `seq` into interleaved stereo samples. A gap before it is filled first:
    /// the frame just before from this packet's FEC, older ones by concealment. A packet older
    /// than the last one decoded gives nothing.
    pub fn decode(&mut self, seq: u32, packet: &[u8]) -> io::Result<Vec<f32>> {
        let missing = match self.next {
            None => 0,
            Some(next) if seq.wrapping_sub(next) > u32::MAX / 2 => return Ok(Vec::new()),
            Some(next) => seq.wrapping_sub(next),
        };
        let mut pcm = Vec::new();
        if (1..=MAX_CONCEALED).contains(&missing) {
            for _ in 1..missing {
                self.decode_into(&[], false, &mut pcm)?;
            }
            self.decode_into(packet, true, &mut pcm)?;
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
mod tests {
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

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn blocks_of_any_size_come_out_as_10_ms_frames() {
        let mut encoder = Encoder::new().unwrap();
        let pcm = tone(5);
        // A graph quantum is rarely a whole number of frames.
        let first = encoder.push(&pcm[..FRAME_SAMPLES + 100]).unwrap();
        let rest = encoder.push(&pcm[FRAME_SAMPLES + 100..]).unwrap();
        assert_eq!((first.len(), rest.len()), (1, 4));
        assert!(first.iter().chain(&rest).all(|p| !p.is_empty()));
    }

    #[test]
    fn frames_go_through_the_codec_and_back() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let input = tone(50);
        let mut output = Vec::new();
        for (seq, packet) in encoder.push(&input).unwrap().iter().enumerate() {
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
        let packets = encoder.push(&tone(4)).unwrap();
        let mut output = Vec::new();
        for seq in [0u32, 1, 3] {
            output.extend(decoder.decode(seq, &packets[seq as usize]).unwrap());
        }
        assert_eq!(output.len(), 4 * FRAME_SAMPLES);
    }

    #[test]
    fn a_late_packet_is_dropped() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let packets = encoder.push(&tone(3)).unwrap();
        decoder.decode(0, &packets[0]).unwrap();
        decoder.decode(2, &packets[2]).unwrap();
        assert!(decoder.decode(1, &packets[1]).unwrap().is_empty());
    }
}
