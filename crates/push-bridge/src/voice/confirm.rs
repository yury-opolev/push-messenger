//! Spoken answers to an open card: approve or reject, an option by number or label, or
//! free text.

use crate::backend::Answer;
use crate::cards::{CardKind, NUMBER_WORDS, answer_from_values, option_id};
use crate::mask::masked_line;

/// Said when an approval's answer is unclear.
pub const APPROVE_OR_REJECT: &str = "Sorry, I didn't catch that. Say approve or reject.";
/// Said when several options were heard for a single choice.
pub const WHICH_ONE: &str = "Sorry, which one? Say the option's number.";
/// Said when no option was heard.
pub const WHICH_OPTION: &str = "Sorry, which option? Say its number, like option one.";
/// Said when nothing was heard for a free-text question.
pub const SAY_AGAIN: &str = "Sorry, I didn't catch that. Say your answer again.";

/// Characters of an option label said back.
const LABEL: usize = 80;
/// Words that start a transcript and mean nothing.
const FILLERS: &[&str] = &["um", "uh", "er", "erm", "hmm", "hm"];
const APPROVE_WORDS: &[&str] = &[
    "approve",
    "approved",
    "yes",
    "yeah",
    "yep",
    "yup",
    "sure",
    "ok",
    "okay",
    "allow",
    "accept",
    "accepted",
    "confirm",
    "go ahead",
    "do it",
    "sounds good",
];
/// Normalised: "don't" is "don t".
const REJECT_WORDS: &[&str] = &[
    "reject", "rejected", "no", "nope", "deny", "denied", "decline", "declined", "cancel", "stop",
    "don t", "do not", "not now",
];
const ORDINALS: [&str; 10] = [
    "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth", "tenth",
];
/// Words before a number that make it an option's.
const POINTERS: &[&str] = &["option", "number", "choice"];
/// Words after an ordinal that make it an option's.
const ORDINAL_NOUNS: &[&str] = &["one", "option", "choice"];

/// What a transcript does to the open card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceAnswer {
    /// The transcript answers the card: the backend's answer, the card values it stands for
    /// (`approve`/`reject`, option ids such as `o2`; empty for free text), and the label to
    /// show and say ("Approve", "Reject", the option labels joined with ", ", or the free
    /// text).
    Answer {
        answer: Answer,
        values: Vec<String>,
        label: String,
    },
    /// Not clear: say this and keep the card open.
    Again(&'static str),
}

/// What `transcript` answers to a card of `kind`. Words are compared whole, lowercase and
/// without punctuation; leading fillers ("um", "uh") are ignored. An approval needs exactly
/// one of approve or reject; a question takes the options named by number ("option two",
/// "the second one", a bare "two") or by label, else the transcript as free text when the
/// card allows it. The answer is the one the card's buttons give for the same values.
pub fn match_answer(kind: &CardKind, transcript: &str) -> VoiceAnswer {
    let words = words(transcript);
    let CardKind::Question {
        options,
        multi,
        free_text,
    } = kind
    else {
        let approve = APPROVE_WORDS.iter().any(|p| has_phrase(&words, p));
        let reject = REJECT_WORDS.iter().any(|p| has_phrase(&words, p));
        return match (approve, reject) {
            (true, false) => answer(kind, vec![option_value(true)], "Approve".to_owned()),
            (false, true) => answer(kind, vec![option_value(false)], "Reject".to_owned()),
            _ => VoiceAnswer::Again(APPROVE_OR_REJECT),
        };
    };
    let open = *free_text || options.is_empty();
    if words.is_empty() {
        return VoiceAnswer::Again(if open { SAY_AGAIN } else { WHICH_OPTION });
    }
    let chosen = chosen(options, &words);
    if chosen.len() == 1 || (*multi && !chosen.is_empty()) {
        let values = chosen.iter().map(|&i| option_id(i)).collect();
        let label = chosen
            .iter()
            .map(|&i| masked_line(&options[i], LABEL))
            .collect::<Vec<_>>()
            .join(", ");
        return answer(kind, values, label);
    }
    if !chosen.is_empty() {
        return VoiceAnswer::Again(WHICH_ONE);
    }
    if open {
        let text = transcript.trim().to_owned();
        return VoiceAnswer::Answer {
            answer: Answer::Question(text.clone()),
            values: Vec::new(),
            label: text,
        };
    }
    VoiceAnswer::Again(WHICH_OPTION)
}

fn answer(kind: &CardKind, values: Vec<String>, label: String) -> VoiceAnswer {
    VoiceAnswer::Answer {
        answer: answer_from_values(kind, &values),
        values,
        label,
    }
}

/// The approval card's value for approve or reject.
fn option_value(approve: bool) -> String {
    if approve { "approve" } else { "reject" }.to_owned()
}

/// Lowercase words of `text` without punctuation, leading fillers dropped.
fn words(text: &str) -> Vec<String> {
    let mut words = plain_words(text);
    let fillers = words
        .iter()
        .take_while(|word| FILLERS.contains(&word.as_str()))
        .count();
    words.drain(..fillers);
    words
}

fn plain_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Whether `words` hold `phrase` (space-separated words) as whole words.
fn has_phrase(words: &[String], phrase: &str) -> bool {
    let phrase: Vec<&str> = phrase.split(' ').collect();
    words
        .windows(phrase.len())
        .any(|window| window.iter().zip(&phrase).all(|(a, b)| a == b))
}

/// The options `words` name (0-based, in option order, each once).
fn chosen(options: &[String], words: &[String]) -> Vec<usize> {
    let mut found = Vec::new();
    for (i, word) in words.iter().enumerate() {
        let next = words.get(i + 1).map(String::as_str);
        if POINTERS.contains(&word.as_str()) {
            found.extend(next.and_then(|n| cardinal(n).or_else(|| ordinal(n))));
        }
        if let Some(n) = ordinal(word) {
            let the = i > 0 && words[i - 1] == "the";
            if words.len() == 1 || the || next.is_some_and(|n| ORDINAL_NOUNS.contains(&n)) {
                found.push(n);
            }
        }
    }
    if let [word] = words {
        found.extend(cardinal(word));
    }
    let mut chosen: Vec<usize> = found
        .into_iter()
        .filter(|n| (1..=options.len()).contains(n))
        .map(|n| n - 1)
        .collect();
    for (i, label) in options.iter().enumerate() {
        let label = plain_words(label).join(" ");
        if !label.is_empty() && has_phrase(words, &label) {
            chosen.push(i);
        }
    }
    chosen.sort_unstable();
    chosen.dedup();
    chosen
}

/// "2" or "two" as 2.
fn cardinal(word: &str) -> Option<usize> {
    if word.chars().all(|c| c.is_ascii_digit()) {
        return word.parse().ok();
    }
    NUMBER_WORDS.iter().position(|w| *w == word).map(|i| i + 1)
}

/// "2nd" or "second" as 2.
fn ordinal(word: &str) -> Option<usize> {
    if let Some(i) = ORDINALS.iter().position(|w| *w == word) {
        return Some(i + 1);
    }
    let digits = word
        .strip_suffix("st")
        .or_else(|| word.strip_suffix("nd"))
        .or_else(|| word.strip_suffix("rd"))
        .or_else(|| word.strip_suffix("th"))?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn question(options: &[&str], multi: bool, free_text: bool) -> CardKind {
        CardKind::Question {
            options: strings(options),
            multi,
            free_text,
        }
    }

    /// The values and label, after checking the answer is what the card's buttons give.
    fn heard(kind: &CardKind, transcript: &str) -> Result<(Vec<String>, String), &'static str> {
        match match_answer(kind, transcript) {
            VoiceAnswer::Answer {
                answer,
                values,
                label,
            } => {
                if !values.is_empty() {
                    assert_eq!(answer, answer_from_values(kind, &values), "{transcript}");
                }
                Ok((values, label))
            }
            VoiceAnswer::Again(said) => Err(said),
        }
    }

    fn chose(values: &[&str], label: &str) -> Result<(Vec<String>, String), &'static str> {
        Ok((strings(values), label.to_owned()))
    }

    #[test]
    fn approvals_hear_approve_or_reject() {
        let approve = chose(&["approve"], "Approve");
        let reject = chose(&["reject"], "Reject");
        let cases = [
            ("Approve.", &approve),
            ("yes please", &approve),
            ("Um, go ahead!", &approve),
            ("OK, sounds good", &approve),
            ("Reject", &reject),
            ("no", &reject),
            ("Nope, not now.", &reject),
            ("do not", &reject),
        ];
        for kind in [CardKind::Permission, CardKind::Plan, CardKind::Bypass] {
            for (transcript, expected) in &cases {
                assert_eq!(
                    &heard(&kind, transcript),
                    *expected,
                    "{kind:?} {transcript}"
                );
            }
            for transcript in [
                "don't approve",
                "yes no",
                "maybe",
                "",
                "  uh  ",
                "yesterday",
            ] {
                assert_eq!(
                    heard(&kind, transcript),
                    Err(APPROVE_OR_REJECT),
                    "{kind:?} {transcript}"
                );
            }
        }
        assert_eq!(
            match_answer(&CardKind::Plan, "approved"),
            VoiceAnswer::Answer {
                answer: Answer::Plan(true),
                values: strings(&["approve"]),
                label: "Approve".to_owned(),
            }
        );
    }

    #[test]
    fn options_are_heard_by_number_ordinal_or_label() {
        let kind = question(&["EU", "US", "Asia Pacific"], false, false);
        let us = chose(&["o2"], "US");
        for transcript in [
            "option two",
            "Option 2.",
            "the second one",
            "Second.",
            "2nd option",
            "two",
            "2",
            "number two please",
            "choice 2",
            "US",
            "I'd say us, please",
        ] {
            assert_eq!(heard(&kind, transcript), us, "{transcript}");
        }
        assert_eq!(heard(&kind, "asia pacific"), chose(&["o3"], "Asia Pacific"));
        assert_eq!(heard(&kind, "the third"), chose(&["o3"], "Asia Pacific"));
        assert_eq!(heard(&kind, "option eleven"), Err(WHICH_OPTION));
        assert_eq!(heard(&kind, "option four"), Err(WHICH_OPTION));
        assert_eq!(heard(&kind, "wait a second"), Err(WHICH_OPTION));
        assert_eq!(heard(&kind, "Mars"), Err(WHICH_OPTION));
        assert_eq!(heard(&kind, ""), Err(WHICH_OPTION));
        assert_eq!(heard(&kind, "EU and US"), Err(WHICH_ONE));
        assert_eq!(heard(&kind, "EU, option one"), chose(&["o1"], "EU"));
    }

    #[test]
    fn a_multi_select_hears_every_option_in_option_order() {
        let kind = question(&["EU", "US", "Asia Pacific"], true, false);
        assert_eq!(heard(&kind, "US and EU"), chose(&["o1", "o2"], "EU, US"));
        assert_eq!(
            heard(&kind, "option three and the first one"),
            chose(&["o1", "o3"], "EU, Asia Pacific")
        );
        assert_eq!(heard(&kind, "just us"), chose(&["o2"], "US"));
        assert_eq!(heard(&kind, "none of them"), Err(WHICH_OPTION));
    }

    #[test]
    fn free_text_is_the_answer_when_no_option_is_heard() {
        let kind = question(&["EU", "US"], false, true);
        assert_eq!(heard(&kind, "option one"), chose(&["o1"], "EU"));
        assert_eq!(
            match_answer(&kind, "  Somewhere in Mars orbit. "),
            VoiceAnswer::Answer {
                answer: Answer::Question("Somewhere in Mars orbit.".to_owned()),
                values: Vec::new(),
                label: "Somewhere in Mars orbit.".to_owned(),
            }
        );
        assert_eq!(heard(&kind, " um "), Err(SAY_AGAIN));
        let open = question(&[], false, false);
        assert_eq!(heard(&open, "Call it Nova"), chose(&[], "Call it Nova"));
        assert_eq!(heard(&open, "two"), chose(&[], "two"));
        assert_eq!(heard(&open, ""), Err(SAY_AGAIN));
    }

    #[test]
    fn labels_are_masked_for_showing() {
        let kind = question(&["token=s3cr3t", "plain"], false, false);
        let Ok((values, label)) = heard(&kind, "option one") else {
            panic!("an answer");
        };
        assert_eq!(values, ["o1"]);
        assert!(!label.contains("s3cr3t"));
    }
}
