//! Speech fakes for tests and CI: speech is tones. [`FakeStt`] hears a word per frequency,
//! [`FakeTts`] speaks a tone, so both survive real Opus media in end-to-end tests.
//! Recorded [`Fixture`]s (`tests/fixtures/voice/<name>.json`) script the owner's side.
//!
//! Word frequencies should lie between 350 and 2400 Hz, at least 200 Hz apart, and away
//! from [`FakeTts`]'s default 600 Hz (± 150 Hz): the agent's voice must not be heard as
//! the owner's words.

use std::f64::consts::PI;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;

use super::SAMPLE_RATE;
use super::provider::{SpeechError, Stt, Tts};

/// The weakest tone [`FakeStt`] hears.
const HEARING: f64 = 1000.0;
/// [`FakeTts`]'s default frequency.
pub const TTS_HZ: f64 = 600.0;
const TTS_AMPLITUDE: f64 = 6000.0;
/// A [`Fixture`]'s tones.
const FIXTURE_AMPLITUDE: f64 = 8000.0;

fn samples_in(ms: u32) -> usize {
    (u64::from(ms) * u64::from(SAMPLE_RATE) / 1000) as usize
}

/// `ms` of a sine at `hz`, amplitude `amplitude`, 16 kHz.
pub fn tone(hz: f64, ms: u32, amplitude: f64) -> Vec<i16> {
    let rate = f64::from(SAMPLE_RATE);
    (0..samples_in(ms))
        .map(|i| {
            let value = amplitude * (2.0 * PI * hz * i as f64 / rate).sin();
            value
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
        })
        .collect()
}

/// Silence of `ms`.
pub fn silence(ms: u32) -> Vec<i16> {
    vec![0; samples_in(ms)]
}

/// The amplitude of the `hz` component of `samples` (Goertzel): a sine of amplitude `a`
/// at `hz` gives about `a`.
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

/// Recognises tones: each word is a frequency; the strongest one above 1000 (amplitude)
/// is the transcript, else "".
pub struct FakeStt {
    words: Vec<(f64, String)>,
    heard: Arc<Mutex<Vec<String>>>,
    delay: Duration,
}

impl FakeStt {
    pub fn new(words: Vec<(f64, String)>) -> Self {
        Self {
            words,
            heard: Arc::new(Mutex::new(Vec::new())),
            delay: Duration::ZERO,
        }
    }

    /// Takes `delay` to transcribe, like a real service.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// The transcripts so far (empty ones included).
    pub fn heard(&self) -> Vec<String> {
        self.heard.lock().expect("the log is not poisoned").clone()
    }

    fn recognise(&self, samples: &[i16]) -> String {
        self.words
            .iter()
            .map(|(hz, text)| (amplitude(samples, *hz), text))
            .filter(|(strength, _)| *strength > HEARING)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, text)| text.clone())
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Stt for FakeStt {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn transcribe(&self, samples: &[i16]) -> Result<String, SpeechError> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let text = self.recognise(samples);
        self.heard
            .lock()
            .expect("the log is not poisoned")
            .push(text.clone());
        Ok(text)
    }
}

/// Speaks every text as a tone at `hz` (default 600 Hz, amplitude 6000) lasting 60 ms per
/// word (at least 200 ms, at most 3 s), and logs the texts.
pub struct FakeTts {
    hz: f64,
    spoken: Arc<Mutex<Vec<String>>>,
}

impl Default for FakeTts {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeTts {
    pub fn new() -> Self {
        Self::with_hz(TTS_HZ)
    }

    pub fn with_hz(hz: f64) -> Self {
        Self {
            hz,
            spoken: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The texts spoken so far.
    pub fn spoken(&self) -> Vec<String> {
        self.spoken.lock().expect("the log is not poisoned").clone()
    }

    /// The log of spoken texts, shared.
    pub fn log(&self) -> Arc<Mutex<Vec<String>>> {
        self.spoken.clone()
    }
}

#[async_trait::async_trait]
impl Tts for FakeTts {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn synthesize(&self, text: &str) -> Result<Vec<i16>, SpeechError> {
        self.spoken
            .lock()
            .expect("the log is not poisoned")
            .push(text.to_owned());
        let words = u32::try_from(text.split_whitespace().count()).unwrap_or(u32::MAX);
        let ms = words.saturating_mul(60).clamp(200, 3000);
        Ok(tone(self.hz, ms, TTS_AMPLITUDE))
    }
}

/// A recorded fixture: `crates/push-bridge/tests/fixtures/voice/<name>.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub description: String,
    pub words: Vec<FixtureWord>,
    pub script: Vec<Step>,
}

/// A word [`FakeStt`] hears at `hz`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureWord {
    pub hz: f64,
    pub text: String,
}

/// One step of the owner's audio: `{"say": {"hz": 500, "ms": 900}}` or
/// `{"silence": {"ms": 800}}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Step {
    Say { hz: f64, ms: u32 },
    Silence { ms: u32 },
}

impl Fixture {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| error.to_string())
    }

    /// The owner's audio the script describes (tones at amplitude 8000).
    pub fn audio(&self) -> Vec<i16> {
        self.script
            .iter()
            .flat_map(|step| match step {
                Step::Say { hz, ms } => tone(*hz, *ms, FIXTURE_AMPLITUDE),
                Step::Silence { ms } => silence(*ms),
            })
            .collect()
    }

    /// A [`FakeStt`] hearing the fixture's words.
    pub fn stt(&self) -> FakeStt {
        FakeStt::new(
            self.words
                .iter()
                .map(|word| (word.hz, word.text.clone()))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words() -> Vec<(f64, String)> {
        vec![
            (400.0, "run the tests".into()),
            (1000.0, "yes".into()),
            (1800.0, "stop".into()),
        ]
    }

    #[test]
    fn tones_have_their_length_and_amplitude() {
        let samples = tone(440.0, 250, 5000.0);
        assert_eq!(samples.len(), 4000);
        assert!(samples.iter().map(|s| s.unsigned_abs()).max().unwrap() <= 5000);
        assert!((amplitude(&samples, 440.0) - 5000.0).abs() < 100.0);
        assert!(amplitude(&samples, 1040.0) < 200.0);
        assert_eq!(silence(20), vec![0; 320]);
        assert_eq!(amplitude(&silence(100), 440.0), 0.0);
        assert_eq!(amplitude(&[], 440.0), 0.0);
    }

    #[tokio::test]
    async fn fake_stt_hears_the_strongest_word() {
        let stt = FakeStt::new(words());
        let mut audio = silence(300);
        audio.extend(tone(1000.0, 800, 8000.0));
        audio.extend(silence(700));
        assert_eq!(stt.transcribe(&audio).await.unwrap(), "yes");

        let strong = tone(1800.0, 500, 8000.0);
        let weak = tone(400.0, 500, 3000.0);
        let mixed: Vec<i16> = strong.iter().zip(&weak).map(|(a, b)| a + b).collect();
        assert_eq!(stt.transcribe(&mixed).await.unwrap(), "stop");

        assert_eq!(stt.transcribe(&silence(500)).await.unwrap(), "");
        let mut seed = 12345u32;
        let noise: Vec<i16> = (0..8000)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                ((seed >> 16) % 4000) as i16 - 2000
            })
            .collect();
        assert_eq!(stt.transcribe(&noise).await.unwrap(), "");
        assert_eq!(stt.transcribe(&tone(600.0, 500, 6000.0)).await.unwrap(), "");
        assert_eq!(stt.heard(), vec!["yes", "stop", "", "", ""]);
    }

    #[tokio::test(start_paused = true)]
    async fn fake_stt_can_take_its_time() {
        let stt = FakeStt::new(words()).with_delay(Duration::from_secs(2));
        let started = tokio::time::Instant::now();
        stt.transcribe(&tone(400.0, 300, 8000.0)).await.unwrap();
        assert!(started.elapsed() >= Duration::from_secs(2));
    }

    #[tokio::test]
    async fn fake_tts_speaks_60_ms_a_word_within_bounds_and_logs() {
        let tts = FakeTts::new();
        let log = tts.log();
        assert_eq!(
            tts.synthesize("one two").await.unwrap().len(),
            samples_in(200)
        );
        let ten = "word ".repeat(10);
        let spoken = tts.synthesize(&ten).await.unwrap();
        assert_eq!(spoken.len(), samples_in(600));
        assert!((amplitude(&spoken, TTS_HZ) - TTS_AMPLITUDE).abs() < 150.0);
        let long = "word ".repeat(100);
        assert_eq!(tts.synthesize(&long).await.unwrap().len(), samples_in(3000));
        assert_eq!(tts.spoken(), vec!["one two".to_owned(), ten, long]);
        assert_eq!(log.lock().unwrap().len(), 3);
        let high = FakeTts::with_hz(2000.0).synthesize("hi").await.unwrap();
        assert!(amplitude(&high, 2000.0) > 5000.0);
    }

    const FIXTURE: &str = r#"{
        "description": "the owner asks for the tests",
        "words": [{ "hz": 400, "text": "run the tests" }, { "hz": 1000, "text": "yes" }],
        "script": [
            { "silence": { "ms": 500 } },
            { "say": { "hz": 400, "ms": 900 } },
            { "silence": { "ms": 800 } }
        ]
    }"#;

    #[tokio::test]
    async fn a_fixture_parses_and_makes_audio_and_a_listener() {
        let fixture = Fixture::parse(FIXTURE).unwrap();
        assert_eq!(fixture.description, "the owner asks for the tests");
        assert_eq!(fixture.script.len(), 3);
        let audio = fixture.audio();
        assert_eq!(audio.len(), samples_in(2200));
        assert!((amplitude(&audio[8000..22_400], 400.0) - 8000.0).abs() < 200.0);
        assert_eq!(
            fixture.stt().transcribe(&audio).await.unwrap(),
            "run the tests"
        );
    }

    #[test]
    fn a_fixture_refuses_unknown_fields() {
        let typo = FIXTURE.replace("\"description\"", "\"descripton\"");
        assert!(Fixture::parse(&typo).unwrap_err().contains("descripton"));
        let typo = FIXTURE.replace("\"text\": \"yes\"", "\"text\": \"yes\", \"loud\": true");
        assert!(Fixture::parse(&typo).unwrap_err().contains("loud"));
        let typo = FIXTURE.replace("\"hz\": 400, \"ms\"", "\"hz\": 400, \"db\": 3, \"ms\"");
        assert!(Fixture::parse(&typo).unwrap_err().contains("db"));
        let typo = FIXTURE.replace("\"say\"", "\"speak\"");
        assert!(Fixture::parse(&typo).unwrap_err().contains("speak"));
    }

    #[test]
    fn a_missing_fixture_names_its_path() {
        let error = Fixture::load(Path::new("no-such-fixture.json")).unwrap_err();
        assert!(error.contains("no-such-fixture.json"), "{error}");
    }
}
