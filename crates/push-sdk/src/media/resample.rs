//! Sample-rate conversion between Opus's 48 kHz and the helper's 16 kHz, by 3 in either
//! direction: one windowed-sinc low-pass FIR used as a polyphase decimator (48 k → 16 k)
//! and interpolator (16 k → 48 k). Both keep their history across frames, so a stream of
//! 20 ms frames converts without seams.

/// Taps of the low-pass filter: a multiple of the factor, so the interpolator's three
/// phases have the same length.
const TAPS: usize = 96;
const FACTOR: usize = 3;
/// The cut-off (Hz at 48 kHz): under the 8 kHz Nyquist frequency of 16 kHz, with room
/// for the transition band of a 96-tap Blackman window (about 2.8 kHz).
const CUTOFF_HZ: f64 = 7_200.0;
const HIGH_RATE: f64 = 48_000.0;

/// The low-pass filter, normalised to a gain of 1 at 0 Hz.
fn low_pass() -> [f32; TAPS] {
    let center = (TAPS - 1) as f64 / 2.0;
    let fc = CUTOFF_HZ / HIGH_RATE;
    let mut taps = [0.0f64; TAPS];
    for (index, tap) in taps.iter_mut().enumerate() {
        let x = index as f64 - center;
        let sinc = (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x);
        let phase = 2.0 * std::f64::consts::PI * index as f64 / (TAPS - 1) as f64;
        let blackman = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
        *tap = sinc * blackman;
    }
    let sum: f64 = taps.iter().sum();
    taps.map(|tap| (tap / sum) as f32)
}

fn to_i16(sample: f32) -> i16 {
    sample
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// The dot product of `taps` with `window` read newest first.
fn convolve(window: &[f32], taps: &[f32]) -> f32 {
    window
        .iter()
        .rev()
        .zip(taps)
        .map(|(sample, tap)| sample * tap)
        .sum()
}

/// 48 kHz → 16 kHz.
pub(crate) struct Downsampler {
    taps: [f32; TAPS],
    /// The last `TAPS - 1` input samples, oldest first.
    history: Vec<f32>,
}

impl Downsampler {
    pub(crate) fn new() -> Self {
        Self {
            taps: low_pass(),
            history: vec![0.0; TAPS - 1],
        }
    }

    /// Converts `input` (a multiple of 3 samples) to a third as many samples.
    pub(crate) fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let mut buffer = std::mem::take(&mut self.history);
        buffer.extend(input.iter().map(|&sample| f32::from(sample)));
        let output = (0..input.len() / FACTOR)
            .map(|index| {
                // The newest input sample of output `index` is input `3 index + 2`.
                let end = TAPS + FACTOR * index + FACTOR - 1;
                to_i16(convolve(&buffer[end - TAPS..end], &self.taps))
            })
            .collect();
        self.history = buffer[buffer.len() - (TAPS - 1)..].to_vec();
        output
    }
}

/// 16 kHz → 48 kHz.
pub(crate) struct Upsampler {
    /// `phases[p][j]` = `taps[3 j + p]` × 3: zero stuffing leaves a third of the energy.
    phases: [[f32; TAPS / FACTOR]; FACTOR],
    /// The last `TAPS / 3 - 1` input samples, oldest first.
    history: Vec<f32>,
}

impl Upsampler {
    pub(crate) fn new() -> Self {
        let mut phases = [[0.0f32; TAPS / FACTOR]; FACTOR];
        for (index, tap) in low_pass().iter().enumerate() {
            phases[index % FACTOR][index / FACTOR] = tap * FACTOR as f32;
        }
        Self {
            phases,
            history: vec![0.0; TAPS / FACTOR - 1],
        }
    }

    /// Converts `input` to three times as many samples.
    pub(crate) fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let length = TAPS / FACTOR;
        let mut buffer = std::mem::take(&mut self.history);
        buffer.extend(input.iter().map(|&sample| f32::from(sample)));
        let mut output = Vec::with_capacity(input.len() * FACTOR);
        for index in 0..input.len() {
            let window = &buffer[index..index + length];
            for phase in &self.phases {
                output.push(to_i16(convolve(window, phase)));
            }
        }
        self.history = buffer[buffer.len() - (length - 1)..].to_vec();
        output
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The amplitude of the `frequency` component of `signal` (Goertzel).
    pub(crate) fn amplitude(signal: &[i16], frequency: f64, rate: f64) -> f64 {
        let omega = 2.0 * std::f64::consts::PI * frequency / rate;
        let coefficient = 2.0 * omega.cos();
        let (mut previous, mut before) = (0.0f64, 0.0f64);
        for &sample in signal {
            let current = f64::from(sample) + coefficient * previous - before;
            before = previous;
            previous = current;
        }
        let power = previous * previous + before * before - coefficient * previous * before;
        2.0 * power.max(0.0).sqrt() / signal.len().max(1) as f64
    }

    pub(crate) fn tone(frequency: f64, rate: f64, samples: usize, start: usize) -> Vec<i16> {
        (start..start + samples)
            .map(|index| {
                (10_000.0 * (2.0 * std::f64::consts::PI * frequency * index as f64 / rate).sin())
                    as i16
            })
            .collect()
    }

    /// 50 frames of 20 ms through `convert`, the first 5 dropped (the filter's start).
    fn stream(
        frequency: f64,
        rate: f64,
        frame: usize,
        mut convert: impl FnMut(&[i16]) -> Vec<i16>,
    ) -> Vec<i16> {
        let mut out = Vec::new();
        for index in 0..50 {
            let converted = convert(&tone(frequency, rate, frame, index * frame));
            if index >= 5 {
                out.extend(converted);
            }
        }
        out
    }

    #[test]
    fn a_20_ms_frame_converts_to_a_20_ms_frame() {
        assert_eq!(Downsampler::new().process(&[0; 960]).len(), 320);
        assert_eq!(Upsampler::new().process(&[0; 320]).len(), 960);
    }

    #[test]
    fn downsampling_keeps_a_voice_tone() {
        let mut down = Downsampler::new();
        let out = stream(1_000.0, 48_000.0, 960, |frame| down.process(frame));
        let level = amplitude(&out, 1_000.0, 16_000.0);
        assert!((level - 10_000.0).abs() < 300.0, "{level}");
    }

    #[test]
    fn downsampling_removes_what_would_alias() {
        // 12 kHz would fold onto 4 kHz at 16 kHz.
        let mut down = Downsampler::new();
        let out = stream(12_000.0, 48_000.0, 960, |frame| down.process(frame));
        assert_eq!(out.len(), 45 * 320);
        let level = amplitude(&out, 4_000.0, 16_000.0);
        assert!(level < 30.0, "{level}");
    }

    #[test]
    fn upsampling_keeps_a_voice_tone_and_removes_its_images() {
        let mut up = Upsampler::new();
        let out = stream(1_000.0, 16_000.0, 320, |frame| up.process(frame));
        let level = amplitude(&out, 1_000.0, 48_000.0);
        assert!((level - 10_000.0).abs() < 300.0, "{level}");
        let image = amplitude(&out, 15_000.0, 48_000.0);
        assert!(image < 30.0, "{image}");
    }

    #[test]
    fn frames_join_without_seams() {
        // One long conversion equals the same samples converted 20 ms at a time.
        let input = tone(700.0, 16_000.0, 3_200, 0);
        let whole = Upsampler::new().process(&input);
        let mut up = Upsampler::new();
        let pieces: Vec<i16> = input.chunks(320).flat_map(|frame| up.process(frame)).collect();
        assert_eq!(whole.len(), 9_600);
        assert_eq!(whole, pieces);
        let input = tone(700.0, 48_000.0, 9_600, 0);
        let whole = Downsampler::new().process(&input);
        let mut down = Downsampler::new();
        let pieces: Vec<i16> = input.chunks(960).flat_map(|frame| down.process(frame)).collect();
        assert_eq!(whole.len(), 3_200);
        assert_eq!(whole, pieces);
    }
}
