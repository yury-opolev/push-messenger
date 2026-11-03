//! Opus at 48 kHz mono in 20 ms packets, with the helper's 16 kHz frames on the outside.

use super::resample::{Downsampler, Upsampler};

/// Samples of one 20 ms frame at 16 kHz.
pub(crate) const FRAME: usize = 320;
/// The Opus clock and frame: 48 kHz, 20 ms.
const OPUS_RATE: u32 = 48_000;
const OPUS_FRAME: usize = 960;
/// Opus decodes at most 120 ms per packet.
const MAX_DECODED: usize = 5_760;
/// Wideband voice at 32 kbit/s is plenty for speech recognition and synthesis.
const BITRATE: i32 = 32_000;

pub(crate) struct Encoder {
    opus: opus::Encoder,
    up: Upsampler,
    packet: Vec<u8>,
}

impl Encoder {
    pub(crate) fn new() -> Result<Self, String> {
        let mut opus = opus::Encoder::new(OPUS_RATE, opus::Channels::Mono, opus::Application::Voip)
            .map_err(|error| error.to_string())?;
        opus.set_bitrate(opus::Bitrate::Bits(BITRATE))
            .map_err(|error| error.to_string())?;
        opus.set_inband_fec(true)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            opus,
            up: Upsampler::new(),
            packet: vec![0; 1_500],
        })
    }

    /// One Opus packet of a 20 ms frame of 16 kHz samples.
    pub(crate) fn encode(&mut self, frame: &[i16]) -> Result<Vec<u8>, String> {
        if frame.len() != FRAME {
            return Err(format!("a frame is {FRAME} samples, not {}", frame.len()));
        }
        let samples = self.up.process(frame);
        debug_assert_eq!(samples.len(), OPUS_FRAME);
        let length = self
            .opus
            .encode(&samples, &mut self.packet)
            .map_err(|error| error.to_string())?;
        Ok(self.packet[..length].to_vec())
    }
}

pub(crate) struct Decoder {
    opus: opus::Decoder,
    down: Downsampler,
    samples: Vec<i16>,
}

impl Decoder {
    pub(crate) fn new() -> Result<Self, String> {
        Ok(Self {
            opus: opus::Decoder::new(OPUS_RATE, opus::Channels::Mono)
                .map_err(|error| error.to_string())?,
            down: Downsampler::new(),
            samples: vec![0; MAX_DECODED],
        })
    }

    /// The 16 kHz samples of one Opus packet (20 ms for packets from this helper; other
    /// peers may send 10 to 60 ms).
    pub(crate) fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, String> {
        let length = self
            .opus
            .decode(packet, &mut self.samples, false)
            .map_err(|error| error.to_string())?;
        // Opus frames are multiples of 2.5 ms: 120 samples, a multiple of 3.
        Ok(self.down.process(&self.samples[..length]))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::resample::tests::{amplitude, tone};

    #[test]
    fn a_tone_survives_opus() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let mut out = Vec::new();
        for index in 0..50 {
            let packet = encoder
                .encode(&tone(440.0, 16_000.0, FRAME, index * FRAME))
                .unwrap();
            assert!(!packet.is_empty() && packet.len() < 400, "{}", packet.len());
            let decoded = decoder.decode(&packet).unwrap();
            assert_eq!(decoded.len(), FRAME);
            if index >= 10 {
                out.extend(decoded);
            }
        }
        let level = amplitude(&out, 440.0, 16_000.0);
        assert!(level > 7_000.0 && level < 13_000.0, "{level}");
        let other = amplitude(&out, 1_000.0, 16_000.0);
        assert!(other < 500.0, "{other}");
    }

    #[test]
    fn a_frame_of_the_wrong_length_is_refused() {
        assert!(Encoder::new().unwrap().encode(&[0; 100]).is_err());
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let mut decoder = Decoder::new().unwrap();
        let _ = decoder.decode(&[0xff; 3]);
    }
}
