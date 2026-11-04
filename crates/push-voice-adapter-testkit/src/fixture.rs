//! Recorded owner speech for voice tests: `fixtures/<name>.json` in this crate (see its
//! README). A fixture names its words (a [`Word`] each, the tones a [`Script`] hears) and
//! scripts the owner's audio as tones and silences.
//!
//! [`Script`]: crate::Script

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::script::Word;
use crate::tones::{FRAME_MS, WORD_AMPLITUDE, silence, tone_frame};

/// The names of the fixtures in [`dir`].
pub const NAMES: [&str; 5] = ["prompt", "barge_in", "approve", "option_two", "ambiguous"];

/// This crate's `fixtures` folder.
pub fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// A recorded fixture.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub description: String,
    /// The words it says, a tone each (`text` is accepted for `word`).
    pub words: Vec<Word>,
    pub script: Vec<Step>,
}

/// One step of the owner's audio: `{"say": {"hz": 500, "ms": 900}}` or
/// `{"silence": {"ms": 800}}`; `ms` is rounded down to whole 20 ms frames.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Step {
    Say { hz: f64, ms: u32 },
    Silence { ms: u32 },
}

impl Fixture {
    /// The fixture `fixtures/<name>.json`; panics when it cannot be loaded.
    pub fn named(name: &str) -> Self {
        Self::load(&dir().join(format!("{name}.json"))).unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| error.to_string())
    }

    /// The owner's audio the script describes (16 kHz mono; tones at
    /// [`WORD_AMPLITUDE`]).
    pub fn audio(&self) -> Vec<i16> {
        self.script
            .iter()
            .flat_map(|step| match step {
                Step::Say { hz, ms } => (0..frames(*ms))
                    .flat_map(|index| tone_frame(*hz, WORD_AMPLITUDE, index))
                    .collect(),
                Step::Silence { ms } => silence(frames(*ms)),
            })
            .collect()
    }
}

fn frames(ms: u32) -> usize {
    (ms / FRAME_MS) as usize
}

/// Every fixture's words, in [`NAMES`] order.
pub fn all_words() -> Vec<Word> {
    NAMES
        .iter()
        .flat_map(|name| Fixture::named(name).words)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Script;
    use crate::detector::{Detector, Heard};
    use crate::tones::{FRAME_SAMPLES, amplitude, frames as cut};

    const FIXTURE: &str = r#"{
        "description": "the owner asks for the tests",
        "words": [{ "hz": 400, "text": "run the tests" }, { "hz": 1000, "text": "yes" }],
        "script": [
            { "silence": { "ms": 500 } },
            { "say": { "hz": 400, "ms": 900 } },
            { "silence": { "ms": 800 } }
        ]
    }"#;

    fn finals(words: Vec<Word>, audio: &[i16]) -> Vec<String> {
        let mut detector = Detector::new(words);
        cut(audio)
            .iter()
            .flat_map(|frame| detector.push(frame))
            .filter_map(|heard| match heard {
                Heard::Final(text) => Some(text),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_fixture_parses_into_audio_the_detector_hears() {
        let fixture = Fixture::parse(FIXTURE).unwrap();
        assert_eq!(fixture.description, "the owner asks for the tests");
        assert_eq!(fixture.words[0], Word::new(400.0, "run the tests"));
        let audio = fixture.audio();
        assert_eq!(audio.len(), 110 * FRAME_SAMPLES);
        let said = &audio[25 * FRAME_SAMPLES..70 * FRAME_SAMPLES];
        assert!((amplitude(said, 400.0) - WORD_AMPLITUDE).abs() < 200.0);
        assert_eq!(finals(fixture.words, &audio), ["run the tests"]);
    }

    #[test]
    fn a_fixture_refuses_unknown_fields() {
        for (from, to, field) in [
            ("\"description\"", "\"descripton\"", "descripton"),
            (
                "\"text\": \"yes\"",
                "\"text\": \"yes\", \"loud\": true",
                "loud",
            ),
            (
                "\"hz\": 400, \"ms\"",
                "\"hz\": 400, \"db\": 3, \"ms\"",
                "db",
            ),
            ("\"say\"", "\"speak\"", "speak"),
        ] {
            let error = Fixture::parse(&FIXTURE.replace(from, to)).unwrap_err();
            assert!(error.contains(field), "{error}");
        }
        let error = Fixture::load(Path::new("no-such-fixture.json")).unwrap_err();
        assert!(error.contains("no-such-fixture.json"), "{error}");
    }

    #[test]
    fn the_recorded_fixtures_follow_the_tone_rules_and_say_their_words() {
        Script::with_words(all_words()).check().unwrap();
        for name in NAMES {
            let fixture = Fixture::named(name);
            let said: Vec<String> = fixture.words.iter().map(|word| word.word.clone()).collect();
            assert_eq!(finals(all_words(), &fixture.audio()), said, "{name}");
        }
    }
}
