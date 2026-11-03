//! Voice activity detection over 20 ms frames, by energy.
//!
//! A frame's energy is its RMS in dBFS (`20·log10(rms / 32768)`, -100 for silence). The
//! noise floor is the lowest energy of the last 1.5 s of frames (75, the current one
//! included), speech or not, held between -70 dBFS (so digital silence does not pull it
//! down to -100) and -30 dBFS; before 1.5 s have been heard it is the lowest so far. A
//! frame is speech when its energy is above both `floor + 10 dB` and -55 dBFS. So a steady
//! sound stops being speech at most 1.5 s after it starts, and the utterance it opened
//! ends `silence_ms` later: noise that follows silence or steps up cannot hold the detector
//! in speech.
//!
//! Three speech frames in a row (60 ms) are an onset: [`VadEvent::SpeechStarted`], and the
//! utterance starts with the last 10 frames (200 ms, the onset frames included). It ends
//! after `silence_ms` of frames that are not speech: an [`VadEvent::Utterance`] (at most
//! 200 ms kept after its last speech frame) when it held at least `min_speech_ms` of speech
//! frames, else [`VadEvent::Discarded`]. An utterance reaching [`MAX_UTTERANCE`] samples is
//! cut there and is an [`VadEvent::Utterance`]; a new onset starts the next one.

use std::collections::VecDeque;

use super::{FRAME, Frame, SAMPLE_RATE};

/// Samples of the longest utterance (30 s).
pub const MAX_UTTERANCE: usize = 30 * SAMPLE_RATE as usize;

/// When speech ends and how short it may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VadConfig {
    /// Silence that ends an utterance.
    pub silence_ms: u32,
    /// Speech an utterance needs; shorter ones are discarded.
    pub min_speech_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            silence_ms: 700,
            min_speech_ms: 300,
        }
    }
}

/// What a frame did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VadEvent {
    /// Speech began (after the onset frames): barge-in point.
    SpeechStarted,
    /// An utterance ended: its samples, from up to 200 ms before the onset to the last
    /// speech frame plus up to 200 ms.
    Utterance(Vec<i16>),
    /// Speech ended but was too short to be an utterance (a cough, a click).
    Discarded,
}

/// Energy of digital silence, dBFS.
const SILENCE_DB: f64 = -100.0;
/// Highest noise floor, dBFS.
const MAX_FLOOR: f64 = -30.0;
/// How far above the floor speech is, dB.
const ABOVE_FLOOR: f64 = 10.0;
/// Quietest speech, dBFS.
const MIN_SPEECH_DB: f64 = -55.0;
/// Lowest noise floor, dBFS.
const MIN_FLOOR: f64 = -70.0;
/// Frames the noise floor is the quietest of (1.5 s).
const FLOOR_FRAMES: usize = 75;
/// Speech frames in a row that start an utterance.
const ONSET_FRAMES: usize = 3;
/// Frames kept before the onset and after the last speech frame (200 ms).
const PAD_FRAMES: usize = 10;
/// Milliseconds per frame.
const FRAME_MS: u32 = 20;

/// The detector for one call's incoming audio.
pub struct Vad {
    config: VadConfig,
    /// The energies of the latest frames (at most [`FLOOR_FRAMES`]), for the noise floor.
    energies: VecDeque<f64>,
    /// The latest frames while no utterance is in progress, for the pre-roll.
    recent: VecDeque<Frame>,
    /// Speech frames in a row while no utterance is in progress.
    onset: usize,
    /// The utterance in progress.
    utterance: Option<Utterance>,
}

struct Utterance {
    samples: Vec<i16>,
    speech_frames: usize,
    silent_frames: usize,
    /// Samples up to the end of the last speech frame.
    speech_end: usize,
}

impl Vad {
    /// A detector with nothing heard yet.
    pub fn new(config: VadConfig) -> Self {
        Self {
            config,
            energies: VecDeque::with_capacity(FLOOR_FRAMES),
            recent: VecDeque::with_capacity(PAD_FRAMES),
            onset: 0,
            utterance: None,
        }
    }

    /// Feeds one frame; at most one event per frame.
    pub fn push(&mut self, frame: &Frame) -> Option<VadEvent> {
        let energy = energy(frame);
        if self.energies.len() == FLOOR_FRAMES {
            self.energies.pop_front();
        }
        self.energies.push_back(energy);
        let floor = self
            .energies
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
            .clamp(MIN_FLOOR, MAX_FLOOR);
        let speech = energy > (floor + ABOVE_FLOOR).max(MIN_SPEECH_DB);
        match self.utterance.as_mut() {
            Some(utterance) => {
                utterance.samples.extend_from_slice(frame);
                if speech {
                    utterance.speech_frames += 1;
                    utterance.silent_frames = 0;
                    utterance.speech_end = utterance.samples.len();
                } else {
                    utterance.silent_frames += 1;
                }
                let silence = (self.config.silence_ms / FRAME_MS).max(1) as usize;
                if utterance.samples.len() >= MAX_UTTERANCE {
                    self.end(true)
                } else if utterance.silent_frames >= silence {
                    let spoken = utterance.speech_frames as u64 * u64::from(FRAME_MS);
                    self.end(spoken >= u64::from(self.config.min_speech_ms))
                } else {
                    None
                }
            }
            None => {
                if self.recent.len() == PAD_FRAMES {
                    self.recent.pop_front();
                }
                self.recent.push_back(*frame);
                if !speech {
                    self.onset = 0;
                    return None;
                }
                self.onset += 1;
                if self.onset < ONSET_FRAMES {
                    return None;
                }
                let samples: Vec<i16> = self.recent.drain(..).flatten().collect();
                self.onset = 0;
                self.utterance = Some(Utterance {
                    speech_end: samples.len(),
                    samples,
                    speech_frames: ONSET_FRAMES,
                    silent_frames: 0,
                });
                Some(VadEvent::SpeechStarted)
            }
        }
    }

    /// Whether an utterance is in progress.
    pub fn in_speech(&self) -> bool {
        self.utterance.is_some()
    }

    /// Ends the utterance in progress: an [`VadEvent::Utterance`] when `keep`.
    fn end(&mut self, keep: bool) -> Option<VadEvent> {
        let mut utterance = self.utterance.take()?;
        if !keep {
            return Some(VadEvent::Discarded);
        }
        let end = utterance
            .samples
            .len()
            .min(utterance.speech_end + PAD_FRAMES * FRAME);
        utterance.samples.truncate(end);
        Some(VadEvent::Utterance(utterance.samples))
    }
}

/// The RMS of `frame` in dBFS.
fn energy(frame: &Frame) -> f64 {
    let sum: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    let rms = (sum / FRAME as f64).sqrt();
    if rms <= 0.0 {
        return SILENCE_DB;
    }
    (20.0 * (rms / 32_768.0).log10()).max(SILENCE_DB)
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;

    /// Builds a signal frame by frame; the tone's phase runs on across frames.
    struct Signal {
        frames: Vec<Frame>,
        n: usize,
        seed: u32,
    }

    impl Signal {
        fn new() -> Self {
            Self {
                frames: Vec::new(),
                n: 0,
                seed: 1,
            }
        }

        fn noise_sample(&mut self, amplitude: f64) -> f64 {
            self.seed = self
                .seed
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223);
            (f64::from(self.seed >> 8) / f64::from(1u32 << 24) * 2.0 - 1.0) * amplitude
        }

        /// `ms` of a 300 Hz tone at `tone` over uniform noise at `noise` (amplitudes).
        fn add(mut self, ms: usize, tone: f64, noise: f64) -> Self {
            for _ in 0..ms / 20 {
                let mut frame = [0i16; FRAME];
                for sample in frame.iter_mut() {
                    let t = self.n as f64 / f64::from(SAMPLE_RATE);
                    let value = tone * (2.0 * PI * 300.0 * t).sin() + self.noise_sample(noise);
                    *sample = value as i16;
                    self.n += 1;
                }
                self.frames.push(frame);
            }
            self
        }

        fn silence(self, ms: usize) -> Self {
            self.add(ms, 0.0, 0.0)
        }

        fn speech(self, ms: usize) -> Self {
            self.add(ms, 8_000.0, 0.0)
        }

        /// The events and the frame indexes they came at.
        fn run(&self, config: VadConfig) -> Vec<(usize, VadEvent)> {
            let mut vad = Vad::new(config);
            self.frames
                .iter()
                .enumerate()
                .filter_map(|(i, frame)| vad.push(frame).map(|event| (i, event)))
                .collect()
        }
    }

    /// The events as (frame index, name, utterance frames).
    fn summary(events: &[(usize, VadEvent)]) -> Vec<(usize, &'static str, usize)> {
        events
            .iter()
            .map(|(i, event)| match event {
                VadEvent::SpeechStarted => (*i, "started", 0),
                VadEvent::Utterance(samples) => {
                    assert_eq!(samples.len() % FRAME, 0);
                    (*i, "utterance", samples.len() / FRAME)
                }
                VadEvent::Discarded => (*i, "discarded", 0),
            })
            .collect()
    }

    #[test]
    fn silence_alone_is_no_speech() {
        let signal = Signal::new().silence(5_000);
        assert!(signal.run(VadConfig::default()).is_empty());
    }

    #[test]
    fn a_second_of_speech_is_one_utterance_with_pre_roll_and_a_short_tail() {
        let signal = Signal::new().silence(500).speech(1_000).silence(1_000);
        let events = signal.run(VadConfig::default());
        // Onset on the third tone frame (25 + 2); the 35th silent frame (75 + 34) ends it:
        // 7 silent + 50 tone + 10 silent frames.
        assert_eq!(
            summary(&events),
            [(27, "started", 0), (109, "utterance", 67)]
        );
        let VadEvent::Utterance(samples) = &events[1].1 else {
            panic!("an utterance");
        };
        let seconds = samples.len() as f64 / f64::from(SAMPLE_RATE);
        assert!((1.2..=1.45).contains(&seconds), "{seconds}");
        assert!(samples[..7 * FRAME].iter().all(|&s| s == 0));
        assert!(
            samples[samples.len() - 10 * FRAME..]
                .iter()
                .all(|&s| s == 0)
        );
    }

    #[test]
    fn in_speech_follows_the_utterance() {
        let mut vad = Vad::new(VadConfig::default());
        let signal = Signal::new().speech(100).silence(800);
        let states: Vec<bool> = signal
            .frames
            .iter()
            .map(|frame| {
                vad.push(frame);
                vad.in_speech()
            })
            .collect();
        assert_eq!(&states[..4], [false, false, true, true]);
        assert!(!states[states.len() - 1]);
    }

    #[test]
    fn a_short_burst_is_discarded() {
        let signal = Signal::new().silence(200).speech(100).silence(1_000);
        assert_eq!(
            summary(&signal.run(VadConfig::default())),
            [(12, "started", 0), (49, "discarded", 0)]
        );
    }

    #[test]
    fn speech_over_steady_noise_is_found_and_the_noise_alone_is_not() {
        // Uniform noise at amplitude 318 is about -45 dBFS.
        let noise = Signal::new().add(3_000, 0.0, 318.0);
        assert!(noise.run(VadConfig::default()).is_empty());
        let signal = Signal::new()
            .add(1_000, 0.0, 318.0)
            .add(1_000, 8_000.0, 318.0)
            .add(1_000, 0.0, 318.0);
        assert_eq!(
            summary(&signal.run(VadConfig::default())),
            [(52, "started", 0), (134, "utterance", 67)]
        );
    }

    #[test]
    fn a_second_of_silence_separates_two_utterances() {
        let signal = Signal::new()
            .silence(500)
            .speech(600)
            .silence(1_000)
            .speech(600)
            .silence(1_000);
        let names: Vec<_> = summary(&signal.run(VadConfig::default()))
            .into_iter()
            .map(|(_, name, _)| name)
            .collect();
        assert_eq!(names, ["started", "utterance", "started", "utterance"]);
    }

    #[test]
    fn a_short_pause_stays_inside_the_utterance() {
        let signal = Signal::new()
            .silence(500)
            .speech(600)
            .silence(600)
            .speech(600)
            .silence(1_000);
        assert_eq!(
            summary(&signal.run(VadConfig::default())),
            // 7 silent + 30 tone + 30 silent + 30 tone + 10 silent frames.
            [(27, "started", 0), (149, "utterance", 107)]
        );
    }

    #[test]
    fn the_silence_and_minimum_speech_are_configurable() {
        let config = VadConfig {
            silence_ms: 300,
            min_speech_ms: 60,
        };
        let signal = Signal::new().silence(200).speech(100).silence(400);
        assert_eq!(
            summary(&signal.run(config)),
            [(12, "started", 0), (29, "utterance", 22)]
        );
    }

    /// Feeds `signal` and checks that nothing happens after the first `settle_ms` from
    /// frame `step` on and that no utterance is in progress at the end.
    fn settles(signal: &Signal, step: usize, settle_ms: usize) {
        let mut vad = Vad::new(VadConfig::default());
        let mut events = Vec::new();
        for (i, frame) in signal.frames.iter().enumerate() {
            if let Some(event) = vad.push(frame) {
                events.push((i, event));
            }
        }
        let summary = summary(&events);
        assert!(summary.len() <= 2, "{summary:?}");
        let last = step + settle_ms / FRAME_MS as usize;
        assert!(summary.iter().all(|(i, _, _)| *i < last), "{summary:?}");
        assert!(!vad.in_speech());
    }

    #[test]
    fn steady_noise_after_digital_silence_is_not_speech_for_long() {
        let signal = Signal::new().silence(500).add(60_000, 0.0, 318.0);
        // The 1.5 s window forgets the silence; 700 ms more of non-speech end it.
        settles(&signal, 25, 2_500);
    }

    #[test]
    fn a_noise_step_up_is_not_speech_for_long() {
        // Uniform noise at amplitude 57 is about -60 dBFS, at 568 about -40 dBFS.
        let signal = Signal::new().add(3_000, 0.0, 57.0).add(30_000, 0.0, 568.0);
        settles(&signal, 150, 2_500);
    }

    #[test]
    fn a_long_utterance_is_cut_at_thirty_seconds() {
        let signal = Signal::new().silence(200).speech(31_000).silence(1_000);
        let events = summary(&signal.run(VadConfig::default()));
        // Onset at frame 12 with 10 frames of pre-roll; 1,490 more frames make 30 s.
        assert_eq!(events[0], (12, "started", 0));
        assert_eq!(events[1], (1_502, "utterance", MAX_UTTERANCE / FRAME));
        assert_eq!(events[2], (1_505, "started", 0));
        assert_eq!(events[3].1, "utterance");
        assert_eq!(events.len(), 4);
    }
}
