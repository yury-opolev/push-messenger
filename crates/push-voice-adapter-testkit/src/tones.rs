//! Speech is tones: a word is a sine at its frequency, the adapter's voice a sine at
//! [`crate::Script::speak_hz`]. The same rules as push-bridge's PR 7 fakes
//! (`push_bridge::voice::fake`), so its recorded fixtures work here too: word frequencies
//! lie between [`MIN_WORD_HZ`] and [`MAX_WORD_HZ`], at least [`MIN_WORD_GAP_HZ`] apart and
//! at least [`MIN_SPEAK_GAP_HZ`] from the voice's frequency.

use std::f64::consts::PI;

use push_voice_adapter_proto::AudioFormat;

/// Samples per second of v1 audio.
pub const SAMPLE_RATE: u32 = AudioFormat::V1.rate;
/// Samples in one 20 ms frame of v1 audio.
pub const FRAME_SAMPLES: usize = AudioFormat::V1.samples_per_frame();
/// Milliseconds per frame.
pub const FRAME_MS: u32 = AudioFormat::V1.frame_ms;
/// The amplitude of [`tone`]'s words (a fixture's tones).
pub const WORD_AMPLITUDE: f64 = 8000.0;
/// The amplitude of the adapter's voice.
pub const SPEAK_AMPLITUDE: f64 = 6000.0;
/// The weakest tone heard as a word.
pub const HEARING: f64 = 1000.0;
/// The voice's default frequency.
pub const SPEAK_HZ: f64 = 600.0;
/// The lowest word frequency.
pub const MIN_WORD_HZ: f64 = 350.0;
/// The highest word frequency.
pub const MAX_WORD_HZ: f64 = 2400.0;
/// How far apart two words' frequencies are at least.
pub const MIN_WORD_GAP_HZ: f64 = 200.0;
/// How far a word's frequency is from the voice's at least.
pub const MIN_SPEAK_GAP_HZ: f64 = 150.0;

/// One 20 ms frame of samples.
pub type FrameSamples = [i16; FRAME_SAMPLES];

/// Frame `index` of a continuous sine at `hz` with `amplitude`: consecutive indexes join
/// without a phase jump.
pub fn tone_frame(hz: f64, amplitude: f64, index: usize) -> FrameSamples {
    let rate = f64::from(SAMPLE_RATE);
    let first = index * FRAME_SAMPLES;
    let mut frame = [0i16; FRAME_SAMPLES];
    for (i, sample) in frame.iter_mut().enumerate() {
        let value = amplitude * (2.0 * PI * hz * (first + i) as f64 / rate).sin();
        *sample = value
            .round()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
    }
    frame
}

/// `frames` frames of a word's tone at `hz` (amplitude [`WORD_AMPLITUDE`]), as one
/// sample buffer.
pub fn tone(hz: f64, frames: usize) -> Vec<i16> {
    (0..frames)
        .flat_map(|index| tone_frame(hz, WORD_AMPLITUDE, index))
        .collect()
}

/// `frames` frames of digital silence.
pub fn silence(frames: usize) -> Vec<i16> {
    vec![0; frames * FRAME_SAMPLES]
}

/// `samples` cut into frames, the last one padded with silence.
pub fn frames(samples: &[i16]) -> Vec<FrameSamples> {
    samples
        .chunks(FRAME_SAMPLES)
        .map(|chunk| {
            let mut frame = [0i16; FRAME_SAMPLES];
            frame[..chunk.len()].copy_from_slice(chunk);
            frame
        })
        .collect()
}

/// The amplitude of the `hz` component of `samples` (Goertzel): a sine of amplitude `a`
/// at `hz` gives about `a`. A copy of `push_bridge::voice::fake::amplitude`.
pub fn amplitude(samples: &[i16], hz: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let omega = 2.0 * PI * hz / f64::from(SAMPLE_RATE);
    let coefficient = 2.0 * omega.cos();
    let (mut previous, mut before) = (0.0f64, 0.0f64);
    for &sample in samples {
        let current = f64::from(sample) + coefficient * previous - before;
        before = previous;
        previous = current;
    }
    let power = previous * previous + before * before - coefficient * previous * before;
    2.0 * power.max(0.0).sqrt() / samples.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tones_have_their_length_and_amplitude() {
        let samples = tone(440.0, 10);
        assert_eq!(samples.len(), 10 * FRAME_SAMPLES);
        assert!((amplitude(&samples, 440.0) - WORD_AMPLITUDE).abs() < 150.0);
        assert!(amplitude(&samples, 1040.0) < 300.0);
        assert_eq!(amplitude(&silence(3), 440.0), 0.0);
        assert_eq!(amplitude(&[], 440.0), 0.0);
    }

    #[test]
    fn frames_join_without_a_phase_jump() {
        let joined: Vec<i16> = (0..4)
            .flat_map(|index| tone_frame(SPEAK_HZ, SPEAK_AMPLITUDE, index))
            .collect();
        let whole: Vec<i16> = (0..4 * FRAME_SAMPLES)
            .map(|i| {
                (SPEAK_AMPLITUDE * (2.0 * PI * SPEAK_HZ * i as f64 / 16_000.0).sin()).round() as i16
            })
            .collect();
        assert_eq!(joined, whole);
    }

    #[test]
    fn samples_are_cut_into_padded_frames() {
        let cut = frames(&[1; FRAME_SAMPLES + 2]);
        assert_eq!(cut.len(), 2);
        assert_eq!(cut[1][..2], [1, 1]);
        assert!(cut[1][2..].iter().all(|s| *s == 0));
    }
}
