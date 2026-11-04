//! What a [`crate::ScriptedAdapter`] does: its hello, the words it hears, how it speaks,
//! and the faults it plays. JSON, every field optional.

use std::path::{Path, PathBuf};

use push_voice_adapter_proto::{Capabilities, Voice};
use serde::{Deserialize, Serialize};

use crate::tones::{MAX_WORD_HZ, MIN_SPEAK_GAP_HZ, MIN_WORD_GAP_HZ, MIN_WORD_HZ, SPEAK_HZ};

/// The adapter's name in its hello by default.
pub const DEFAULT_NAME: &str = "push-voice-adapter-testkit";

/// A scripted adapter's behaviour. Unknown fields are refused (a typo in a test's script
/// fails loudly).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Script {
    /// The hello's `name`.
    pub name: String,
    /// The hello's `version`: this crate's version by default.
    pub version: String,
    /// The hello's capabilities: by default `stt`, `tts`, `vad` and `barge_in`, not
    /// `partials`. A capability left out of a script's object is `false`.
    pub capabilities: Capabilities,
    /// The hello's voices (none by default).
    pub voices: Vec<Voice>,
    /// The words it hears, a tone each.
    pub words: Vec<Word>,
    /// Its voice's frequency.
    pub speak_hz: f64,
    /// How long a character of a `speak` sounds: `max(1, chars × ms_per_char / 20)`
    /// frames.
    pub ms_per_char: u32,

    /// Fault: after this many stream-0 frames on a connection, crash: the library returns
    /// [`crate::AdapterError::Crashed`] without closing the stream properly, the binary
    /// exits with code 3.
    pub crash_after_frames: Option<u64>,
    /// With `crash_after_frames`: crash only when this file does not exist, and create
    /// it when crashing, so a restarted adapter works the second time.
    pub crash_once_marker: Option<PathBuf>,
    /// Fault: answer the hello with `error unsupported_format` and close.
    pub refuse_format: bool,
    /// Fault: answer the hello with this protocol, whatever the bridge offered.
    pub protocol: Option<String>,
    /// Fault: answer the hello, then never read or write again (the stream stays open).
    pub stall_after_hello: bool,
    /// Fault: right after the hello, send this many pings at once.
    pub flood_messages: u32,
    /// Fault: never answer the hello (keep reading until the bridge closes).
    pub no_hello: bool,
}

/// A word: heard when a tone at `hz` sounds. `text` is accepted for `word`, as in
/// push-bridge's voice fixtures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Word {
    pub hz: f64,
    #[serde(alias = "text")]
    pub word: String,
}

impl Word {
    pub fn new(hz: f64, word: impl Into<String>) -> Self {
        Self {
            hz,
            word: word.into(),
        }
    }
}

impl Default for Script {
    fn default() -> Self {
        Self {
            name: DEFAULT_NAME.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            capabilities: Capabilities {
                stt: true,
                tts: true,
                vad: true,
                barge_in: true,
                partials: false,
            },
            voices: Vec::new(),
            words: Vec::new(),
            speak_hz: SPEAK_HZ,
            ms_per_char: 40,
            crash_after_frames: None,
            crash_once_marker: None,
            refuse_format: false,
            protocol: None,
            stall_after_hello: false,
            flood_messages: 0,
            no_hello: false,
        }
    }
}

impl Script {
    /// The default script hearing `words`.
    pub fn with_words(words: Vec<Word>) -> Self {
        Self {
            words,
            ..Self::default()
        }
    }

    /// Parses and [checks](Self::check) a script.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let script: Script = serde_json::from_str(json).map_err(|error| error.to_string())?;
        script.check()?;
        Ok(script)
    }

    /// Reads a script file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::from_json(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    /// The tone rules: each word between 350 and 2400 Hz, 200 Hz from the others and
    /// 150 Hz from the voice; words not empty.
    pub fn check(&self) -> Result<(), String> {
        for (i, word) in self.words.iter().enumerate() {
            if word.word.trim().is_empty() {
                return Err(format!("word {i} is empty"));
            }
            if !(MIN_WORD_HZ..=MAX_WORD_HZ).contains(&word.hz) {
                return Err(format!(
                    "word {i} at {} Hz is outside {MIN_WORD_HZ}–{MAX_WORD_HZ} Hz",
                    word.hz
                ));
            }
            if (word.hz - self.speak_hz).abs() < MIN_SPEAK_GAP_HZ {
                return Err(format!(
                    "word {i} at {} Hz is within {MIN_SPEAK_GAP_HZ} Hz of the voice ({} Hz)",
                    word.hz, self.speak_hz
                ));
            }
            for (j, other) in self.words.iter().enumerate().skip(i + 1) {
                if (word.hz - other.hz).abs() < MIN_WORD_GAP_HZ {
                    return Err(format!(
                        "words {i} and {j} are less than {MIN_WORD_GAP_HZ} Hz apart"
                    ));
                }
            }
        }
        if self.ms_per_char == 0 {
            return Err("ms_per_char must be at least 1".to_owned());
        }
        Ok(())
    }

    /// Frames a speak of `chars` characters sounds for.
    pub fn speak_frames(&self, chars: usize) -> usize {
        let ms = chars.saturating_mul(self.ms_per_char as usize);
        (ms / crate::tones::FRAME_MS as usize).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_script_is_the_default() {
        let script = Script::from_json("{}").unwrap();
        assert_eq!(script, Script::default());
        assert_eq!(script.name, DEFAULT_NAME);
        assert!(script.capabilities.stt && script.capabilities.tts);
        assert!(script.capabilities.vad && script.capabilities.barge_in);
        assert!(!script.capabilities.partials);
        assert_eq!((script.speak_hz, script.ms_per_char), (600.0, 40));
    }

    #[test]
    fn fixture_words_and_faults_parse() {
        let script = Script::from_json(
            r#"{"words": [{"hz": 800, "text": "run the tests"}, {"hz": 1000, "word": "yes"}],
                "crash_after_frames": 5, "no_hello": true}"#,
        )
        .unwrap();
        assert_eq!(
            script.words,
            vec![Word::new(800.0, "run the tests"), Word::new(1000.0, "yes")]
        );
        assert_eq!(script.crash_after_frames, Some(5));
        assert!(script.no_hello);
    }

    #[test]
    fn typos_and_bad_tones_are_refused() {
        assert!(Script::from_json(r#"{"crash_after_frame": 3}"#).is_err());
        for words in [
            r#"[{"hz": 300, "word": "low"}]"#,
            r#"[{"hz": 2500, "word": "high"}]"#,
            r#"[{"hz": 700, "word": "near the voice"}]"#,
            r#"[{"hz": 1000, "word": "a"}, {"hz": 1100, "word": "b"}]"#,
            r#"[{"hz": 1000, "word": " "}]"#,
        ] {
            let json = format!(r#"{{"words": {words}}}"#);
            assert!(Script::from_json(&json).is_err(), "{json}");
        }
        assert!(Script::from_json(r#"{"words": [{"hz": 450, "word": "edge"}]}"#).is_ok());
    }

    #[test]
    fn speaks_last_ms_per_char() {
        let script = Script::default();
        assert_eq!(script.speak_frames(10), 20);
        assert_eq!(script.speak_frames(0), 1);
    }
}
