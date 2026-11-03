//! WAV files (RIFF/WAVE, 16-bit PCM) for speech services, resampling to the call rate,
//! and call frames.

use super::{FRAME, Frame};

/// `WAVE_FORMAT_PCM`.
const PCM: u16 = 1;
/// `WAVE_FORMAT_EXTENSIBLE`: the format is the subformat GUID's first two bytes.
const EXTENSIBLE: u16 = 0xFFFE;
/// Taps of the low-pass filter before downsampling.
const TAPS: usize = 63;
/// The low-pass cutoff as a share of the new rate.
const CUTOFF: f64 = 0.45;

const NOT_WAV: &str = "not a WAV file";
const NOT_PCM16: &str = "not 16-bit PCM";
const NO_DATA: &str = "no data chunk";

/// A RIFF/WAVE file of `samples`: PCM, mono, 16-bit little-endian, a 44-byte header.
pub fn encode(samples: &[i16], rate: u32) -> Vec<u8> {
    let data = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&PCM.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// The sample rate and samples of a 16-bit PCM WAV file (mono, or stereo averaged to
/// mono; `WAVE_FORMAT_EXTENSIBLE` with a PCM subformat too). Unknown chunks are skipped; a
/// data chunk cut short (a streamed file) is read as far as it goes.
pub fn decode(bytes: &[u8]) -> Result<(u32, Vec<i16>), String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(NOT_WAV.to_owned());
    }
    let mut format = None;
    let mut data = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32_at(bytes, at + 4) as usize;
        let body = &bytes[at + 8..bytes.len().min((at + 8).saturating_add(size))];
        match id {
            b"fmt " => format = Some(body),
            b"data" => data = Some(body),
            _ => {}
        }
        at = (at + 8).saturating_add(size).saturating_add(size % 2);
    }
    let format = format.filter(|f| f.len() >= 16).ok_or(NOT_WAV)?;
    let mut tag = u16_at(format, 0);
    if tag == EXTENSIBLE && format.len() >= 26 {
        tag = u16_at(format, 24);
    }
    let channels = u16_at(format, 2);
    let rate = u32_at(format, 4);
    let bits = u16_at(format, 14);
    if tag != PCM || bits != 16 || !(1..=2).contains(&channels) {
        return Err(NOT_PCM16.to_owned());
    }
    if rate == 0 {
        return Err(NOT_WAV.to_owned());
    }
    let data = data.ok_or(NO_DATA)?;
    let values: Vec<i16> = data
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let samples = if channels == 2 {
        values
            .chunks_exact(2)
            .map(|pair| ((i32::from(pair[0]) + i32::from(pair[1])) / 2) as i16)
            .collect()
    } else {
        values
    };
    Ok((rate, samples))
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// `samples` at `from` Hz as `to` Hz: low-passed first when downsampling (a windowed-sinc
/// filter with its cutoff at 0.45 of the new rate), then linearly interpolated;
/// `round(len · to / from)` samples.
pub fn resample(samples: &[i16], from: u32, to: u32) -> Vec<i16> {
    if from == to || from == 0 || to == 0 || samples.is_empty() {
        return if from == to {
            samples.to_vec()
        } else {
            Vec::new()
        };
    }
    let len = samples.len() as u64;
    let out_len = ((len * u64::from(to) + u64::from(from) / 2) / u64::from(from)) as usize;
    let input: Vec<f64> = if to < from {
        low_pass(samples, CUTOFF * f64::from(to) / f64::from(from))
    } else {
        samples.iter().map(|&s| f64::from(s)).collect()
    };
    let step = f64::from(from) / f64::from(to);
    let last = input.len() - 1;
    (0..out_len)
        .map(|i| {
            let position = i as f64 * step;
            let index = (position.floor() as usize).min(last);
            let frac = position - index as f64;
            let next = input[(index + 1).min(last)];
            let value = input[index] + (next - input[index]) * frac;
            value
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
        })
        .collect()
}

/// `samples` through a Hamming-windowed sinc low-pass with `cutoff` in cycles per sample.
fn low_pass(samples: &[i16], cutoff: f64) -> Vec<f64> {
    use std::f64::consts::PI;
    let half = (TAPS / 2) as isize;
    let mut taps: Vec<f64> = (0..TAPS)
        .map(|k| {
            let x = (k as isize - half) as f64;
            let sinc = if x == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * PI * cutoff * x).sin() / (PI * x)
            };
            let window = 0.54 - 0.46 * (2.0 * PI * k as f64 / (TAPS - 1) as f64).cos();
            sinc * window
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    for tap in &mut taps {
        *tap /= sum;
    }
    let len = samples.len() as isize;
    (0..len)
        .map(|n| {
            taps.iter()
                .enumerate()
                .filter_map(|(k, tap)| {
                    let at = n + k as isize - half;
                    (0..len)
                        .contains(&at)
                        .then(|| tap * f64::from(samples[at as usize]))
                })
                .sum()
        })
        .collect()
}

/// `samples` as call frames, the last one padded with silence.
pub fn frames(samples: &[i16]) -> Vec<Frame> {
    samples
        .chunks(FRAME)
        .map(|chunk| {
            let mut frame = [0; FRAME];
            frame[..chunk.len()].copy_from_slice(chunk);
            frame
        })
        .collect()
}

/// The samples of `frames`, in order.
pub fn samples_of(frames: &[Frame]) -> Vec<i16> {
    frames.concat()
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;

    fn tone(hz: f64, rate: u32, amplitude: f64, count: usize) -> Vec<i16> {
        (0..count)
            .map(|n| (amplitude * (2.0 * PI * hz * n as f64 / f64::from(rate)).sin()) as i16)
            .collect()
    }

    /// The amplitude of `hz` in `samples` (Goertzel).
    fn amplitude(samples: &[i16], hz: f64, rate: u32) -> f64 {
        let w = 2.0 * PI * hz / f64::from(rate);
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0, 0.0);
        for &x in samples {
            let s0 = f64::from(x) + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        2.0 * power.sqrt() / samples.len() as f64
    }

    fn chunk(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn fmt_body(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let align = channels * bits / 8;
        let mut body = Vec::new();
        body.extend_from_slice(&tag.to_le_bytes());
        body.extend_from_slice(&channels.to_le_bytes());
        body.extend_from_slice(&rate.to_le_bytes());
        body.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
        body.extend_from_slice(&align.to_le_bytes());
        body.extend_from_slice(&bits.to_le_bytes());
        body
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(&body);
        out
    }

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn encoded_audio_decodes_back() {
        let samples = vec![0, 1, -1, i16::MAX, i16::MIN, 1234];
        let bytes = encode(&samples, 16_000);
        assert_eq!(bytes.len(), 44 + 12);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..16], b"WAVEfmt ");
        assert_eq!(decode(&bytes), Ok((16_000, samples)));
        assert_eq!(decode(&encode(&[], 8_000)), Ok((8_000, Vec::new())));
    }

    #[test]
    fn unknown_chunks_are_skipped_with_their_padding() {
        let bytes = riff(&[
            chunk(b"fmt ", &fmt_body(1, 1, 22_050, 16)),
            chunk(b"LIST", b"INFOISFT\x03\x00\x00\x00ab\x00"),
            chunk(b"data", &pcm(&[5, -5, 7])),
        ]);
        assert_eq!(decode(&bytes), Ok((22_050, vec![5, -5, 7])));
    }

    #[test]
    fn stereo_is_averaged_to_mono() {
        let bytes = riff(&[
            chunk(b"fmt ", &fmt_body(1, 2, 24_000, 16)),
            chunk(b"data", &pcm(&[100, 200, -100, -300, i16::MAX, i16::MAX])),
        ]);
        assert_eq!(decode(&bytes), Ok((24_000, vec![150, -200, i16::MAX])));
    }

    #[test]
    fn extensible_pcm_is_accepted() {
        let mut body = fmt_body(0xFFFE, 1, 48_000, 16);
        body.extend_from_slice(&22u16.to_le_bytes());
        body.extend_from_slice(&16u16.to_le_bytes());
        body.extend_from_slice(&4u32.to_le_bytes());
        body.extend_from_slice(&[
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71,
        ]);
        let bytes = riff(&[chunk(b"fmt ", &body), chunk(b"data", &pcm(&[9]))]);
        assert_eq!(decode(&bytes), Ok((48_000, vec![9])));
        body[24] = 3;
        let float = riff(&[chunk(b"fmt ", &body), chunk(b"data", &pcm(&[9]))]);
        assert_eq!(decode(&float), Err("not 16-bit PCM".to_owned()));
    }

    #[test]
    fn other_files_are_refused() {
        let not_wav = Err("not a WAV file".to_owned());
        assert_eq!(decode(b""), not_wav);
        assert_eq!(decode(b"RIFF\x04\x00\x00\x00AVI "), not_wav);
        let float = riff(&[
            chunk(b"fmt ", &fmt_body(3, 1, 16_000, 32)),
            chunk(b"data", &[0; 4]),
        ]);
        assert_eq!(decode(&float), Err("not 16-bit PCM".to_owned()));
        let eight_bit = riff(&[
            chunk(b"fmt ", &fmt_body(1, 1, 16_000, 8)),
            chunk(b"data", &[0; 4]),
        ]);
        assert_eq!(decode(&eight_bit), Err("not 16-bit PCM".to_owned()));
        let no_data = riff(&[chunk(b"fmt ", &fmt_body(1, 1, 16_000, 16))]);
        assert_eq!(decode(&no_data), Err("no data chunk".to_owned()));
        let no_format = riff(&[chunk(b"data", &pcm(&[1]))]);
        assert_eq!(decode(&no_format), not_wav);
    }

    #[test]
    fn resampled_lengths_follow_the_rates() {
        let second = |rate: u32| vec![0i16; rate as usize];
        for rate in [8_000, 22_050, 24_000, 44_100, 48_000] {
            assert_eq!(
                resample(&second(rate), rate, 16_000).len(),
                16_000,
                "{rate}"
            );
        }
        assert_eq!(resample(&[0; 441], 22_050, 16_000).len(), 320);
        assert_eq!(resample(&[0; 3], 48_000, 16_000).len(), 1);
        assert_eq!(resample(&[1, 2, 3], 16_000, 16_000), vec![1, 2, 3]);
        assert!(resample(&[], 48_000, 16_000).is_empty());
    }

    #[test]
    fn a_tone_survives_resampling() {
        for rate in [8_000, 22_050, 24_000, 44_100, 48_000] {
            let input = tone(1_000.0, rate, 10_000.0, rate as usize);
            let output = resample(&input, rate, 16_000);
            let found = amplitude(&output[1_000..15_000], 1_000.0, 16_000);
            assert!((9_000.0..11_000.0).contains(&found), "{rate}: {found}");
        }
    }

    #[test]
    fn tones_above_the_new_nyquist_are_filtered_out() {
        let input = tone(13_000.0, 48_000, 10_000.0, 48_000);
        let output = resample(&input, 48_000, 16_000);
        // 13 kHz would alias to 3 kHz at 16 kHz.
        let aliased = amplitude(&output[1_000..15_000], 3_000.0, 16_000);
        assert!(aliased < 500.0, "{aliased}");
    }

    #[test]
    fn frames_are_padded_with_silence() {
        assert!(frames(&[]).is_empty());
        let samples: Vec<i16> = (1..=700).map(|n| n as i16).collect();
        let cut = frames(&samples);
        assert_eq!(cut.len(), 3);
        assert_eq!(cut[1][0], 321);
        assert_eq!(cut[2][59], 700);
        assert!(cut[2][60..].iter().all(|&s| s == 0));
        assert_eq!(&samples_of(&cut)[..700], &samples[..]);
        assert_eq!(samples_of(&cut).len(), 960);
    }
}
