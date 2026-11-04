//! Hearing words in the caller's audio, frame by frame: the strongest scripted tone above
//! [`HEARING`](crate::tones::HEARING) in a frame (Goertzel) is that frame's word. A word
//! heard in [`WORD_FRAMES`] frames in a row counts; the first one opens an utterance
//! (`speech_started`). Words join the utterance in the order they are heard (a word
//! repeating the previous one is not added again); [`END_SILENCE_FRAMES`] frames without
//! any word end it: the final transcript is its words joined by spaces.
//!
//! The recorded [`crate::fixture`]s say one word per utterance, each followed by a second
//! of silence: each utterance's transcript is its word.

use crate::script::Word;
use crate::tones::{HEARING, amplitude};

/// Frames in a row a word must be heard to count (60 ms).
pub const WORD_FRAMES: usize = 3;
/// Frames without a word that end an utterance (300 ms).
pub const END_SILENCE_FRAMES: usize = 15;

/// What a frame did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    /// The first word of an utterance counted.
    SpeechStarted,
    /// A word joined the utterance: the words so far.
    Partial(String),
    /// The utterance ended: its words.
    Final(String),
}

/// One call's word detector.
#[derive(Debug, Clone)]
pub struct Detector {
    words: Vec<Word>,
    /// The word heard in the latest frames and in how many in a row.
    candidate: Option<(usize, usize)>,
    /// The utterance in progress: its words (indexes) and the wordless frames since.
    utterance: Option<(Vec<usize>, usize)>,
}

impl Detector {
    pub fn new(words: Vec<Word>) -> Self {
        Self {
            words,
            candidate: None,
            utterance: None,
        }
    }

    /// The word a frame sounds, if any.
    pub fn word_in(&self, samples: &[i16]) -> Option<usize> {
        self.words
            .iter()
            .enumerate()
            .map(|(i, word)| (amplitude(samples, word.hz), i))
            .filter(|(strength, _)| *strength > HEARING)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, i)| i)
    }

    /// Feeds one frame; what it did, in order.
    pub fn push(&mut self, samples: &[i16]) -> Vec<Heard> {
        let mut heard = Vec::new();
        let word = self.word_in(samples);
        self.candidate = match (word, self.candidate) {
            (Some(word), Some((previous, run))) if word == previous => Some((word, run + 1)),
            (Some(word), _) => Some((word, 1)),
            (None, _) => None,
        };
        let counted = match self.candidate {
            Some((word, run)) if run >= WORD_FRAMES => Some(word),
            _ => None,
        };
        let (mut started, mut grew, mut ended) = (false, false, false);
        match (&mut self.utterance, counted) {
            (None, Some(word)) => {
                self.utterance = Some((vec![word], 0));
                started = true;
            }
            (Some((words, silent)), Some(word)) => {
                *silent = 0;
                if words.last() != Some(&word) {
                    words.push(word);
                    grew = true;
                }
            }
            (Some((_, silent)), None) => {
                if word.is_none() {
                    *silent += 1;
                    ended = *silent >= END_SILENCE_FRAMES;
                }
            }
            (None, None) => {}
        }
        if started {
            heard.push(Heard::SpeechStarted);
        }
        if started || grew {
            heard.push(Heard::Partial(self.text()));
        }
        if ended {
            heard.push(Heard::Final(self.text()));
            self.utterance = None;
        }
        heard
    }

    /// Forgets the utterance in progress (a call ended).
    pub fn reset(&mut self) {
        self.candidate = None;
        self.utterance = None;
    }

    fn text(&self) -> String {
        let Some((words, _)) = &self.utterance else {
            return String::new();
        };
        words
            .iter()
            .map(|i| self.words[*i].word.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tones::{FRAME_SAMPLES, frames, silence, tone};

    fn hear(detector: &mut Detector, samples: &[i16]) -> Vec<Heard> {
        frames(samples)
            .iter()
            .flat_map(|frame| detector.push(frame))
            .collect()
    }

    fn words() -> Vec<Word> {
        vec![Word::new(800.0, "run the tests"), Word::new(1000.0, "yes")]
    }

    #[test]
    fn a_tone_is_an_utterance_of_its_word() {
        let mut detector = Detector::new(words());
        let mut audio = silence(10);
        audio.extend(tone(800.0, 45));
        audio.extend(silence(50));
        assert_eq!(
            hear(&mut detector, &audio),
            vec![
                Heard::SpeechStarted,
                Heard::Partial("run the tests".into()),
                Heard::Final("run the tests".into()),
            ]
        );
    }

    #[test]
    fn words_join_in_order_and_short_blips_do_not_count() {
        let mut detector = Detector::new(words());
        let mut audio = tone(1000.0, 2);
        audio.extend(silence(20));
        assert!(
            hear(&mut detector, &audio).is_empty(),
            "two frames are a blip"
        );
        let mut audio = tone(1000.0, 10);
        audio.extend(silence(5));
        audio.extend(tone(800.0, 10));
        audio.extend(tone(1000.0, 10));
        audio.extend(silence(15));
        assert_eq!(
            hear(&mut detector, &audio).last(),
            Some(&Heard::Final("yes run the tests yes".into()))
        );
    }

    #[test]
    fn the_voice_and_unknown_tones_are_not_words() {
        let mut detector = Detector::new(words());
        let voice: Vec<i16> = (0..50)
            .flat_map(|i| crate::tones::tone_frame(600.0, 6000.0, i))
            .collect();
        assert!(hear(&mut detector, &voice).is_empty());
        assert!(hear(&mut detector, &tone(1800.0, 50)).is_empty());
        assert_eq!(detector.word_in(&[0; FRAME_SAMPLES]), None);
    }
}
