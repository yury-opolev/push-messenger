//! The audio format. Fixed in v1, both directions: PCM signed 16-bit little-endian,
//! 16 kHz, mono, 20 ms frames of 320 samples. Later versions negotiate other formats;
//! none is ever assumed.

use crate::frame::AudioFrame;
use crate::message::AudioSpec;

/// An audio format: what the hello's `audio` describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub rate: u32,
    pub channels: u32,
    pub frame_ms: u32,
}

/// A frame of the wrong shape for the negotiated format. Content-free.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("audio frame of {got} samples; the format needs {expected}")]
pub struct FormatError {
    pub got: usize,
    pub expected: usize,
}

/// The sample format's name in the hello.
pub const S16LE: &str = "s16le";

impl AudioFormat {
    /// The v1 format.
    pub const V1: AudioFormat = AudioFormat {
        rate: 16_000,
        channels: 1,
        frame_ms: 20,
    };

    /// Samples in one frame (all channels): 320 for v1.
    pub const fn samples_per_frame(&self) -> usize {
        (self.rate as usize * self.frame_ms as usize / 1000) * self.channels as usize
    }

    /// The hello's description of this format.
    pub fn spec(&self) -> AudioSpec {
        AudioSpec {
            rate: self.rate,
            channels: self.channels,
            format: S16LE.to_owned(),
            frame_ms: self.frame_ms,
        }
    }

    /// The format a hello describes, if this crate can carry it (s16le only).
    pub fn from_spec(spec: &AudioSpec) -> Option<Self> {
        if spec.format != S16LE || spec.rate == 0 || spec.channels == 0 || spec.frame_ms == 0 {
            return None;
        }
        Some(AudioFormat {
            rate: spec.rate,
            channels: spec.channels,
            frame_ms: spec.frame_ms,
        })
    }

    /// Whether a frame has this format's sample count.
    pub fn check(&self, frame: &AudioFrame) -> Result<(), FormatError> {
        let expected = self.samples_per_frame();
        if frame.samples.len() == expected {
            Ok(())
        } else {
            Err(FormatError {
                got: frame.samples.len(),
                expected,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Stream;

    #[test]
    fn v1_is_320_samples_of_16_khz_mono() {
        assert_eq!(AudioFormat::V1.samples_per_frame(), 320);
        let spec = AudioFormat::V1.spec();
        assert_eq!(
            (
                spec.rate,
                spec.channels,
                spec.format.as_str(),
                spec.frame_ms
            ),
            (16_000, 1, "s16le", 20)
        );
        assert_eq!(AudioFormat::from_spec(&spec), Some(AudioFormat::V1));
        let mut other = spec.clone();
        other.format = "f32le".into();
        assert_eq!(AudioFormat::from_spec(&other), None);
    }

    #[test]
    fn check_counts_samples() {
        let mut frame = AudioFrame {
            stream: Stream::FromCaller,
            seq: 0,
            timestamp_ms: 0,
            samples: vec![0; 320],
        };
        assert_eq!(AudioFormat::V1.check(&frame), Ok(()));
        frame.samples.pop();
        assert_eq!(
            AudioFormat::V1.check(&frame),
            Err(FormatError {
                got: 319,
                expected: 320
            })
        );
    }
}
