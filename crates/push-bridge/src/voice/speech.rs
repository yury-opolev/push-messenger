//! The agent's Markdown reply as short plain sentences to speak, built while the reply
//! streams in.
//!
//! Complete lines are read as Markdown: a fenced code block is said as [`CODE_NOTE`] and
//! a table as [`TABLE_NOTE`]; headings and list items are sentences of their own; quote
//! markers, rules, links, URLs, code spans, emphasis and HTML tags go. A paragraph's text is
//! cut into sentences after `.`, `!`, `?` or `…` and whitespace (not after abbreviations
//! such as "e.g."), and sentences over 300 characters at a comma or space.

use std::mem;
use std::sync::LazyLock;

use regex::Regex;

/// Said instead of a code block.
pub const CODE_NOTE: &str = "I've put the code in the chat.";
/// Said instead of a table.
pub const TABLE_NOTE: &str = "I've put a table in the chat.";

/// Characters of the longest sentence.
const MAX_SENTENCE: usize = 300;
/// Words ending in a period that do not end a sentence (lowercase).
const ABBREVIATIONS: &[&str] = &[
    "e.g.", "i.e.", "etc.", "mr.", "mrs.", "ms.", "dr.", "vs.", "prof.", "st.",
];
/// The first of the private-use characters that stand for protected ASCII punctuation.
const PROTECTED: u32 = 0xF_0000;

static TABLE_DELIMITER: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^\|?\s*:?-+:?\s*(?:\|\s*:?-+:?\s*)+\|?$"));
static RULE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^(?:(?:\*\s*){3,}|(?:-\s*){3,}|(?:_\s*){3,}|=+)$"));
static LIST_ITEM: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^(?:[-*+]|\d{1,9}[.)])\s+(?:\[[ xX]\]\s+)?(.*)$"));
static ESCAPE: LazyLock<Regex> = LazyLock::new(|| regex(r"\\([!-/:-@\[-`{-~])"));
static IMAGE: LazyLock<Regex> = LazyLock::new(|| regex(r"!\[([^\]]*)\]\([^)]*\)"));
static LINK: LazyLock<Regex> = LazyLock::new(|| regex(r"\[([^\]]+)\](?:\([^)]*\)|\[[^\]]*\])"));
static AUTOLINK: LazyLock<Regex> = LazyLock::new(|| regex(r"<(?:https?://|www\.|mailto:)[^>\s]*>"));
static URL: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"\b(?:https?://|www\.)[^\s<>]*[^\s<>.,;:!?'")\]]"#));
static HTML_TAG: LazyLock<Regex> =
    LazyLock::new(|| regex(r"<!--.*?-->|</?[A-Za-z][A-Za-z0-9-]*(?:\s[^<>]*)?/?>"));

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid pattern")
}

/// Streamed Markdown in, spoken sentences out.
#[derive(Default)]
pub struct SpeechText {
    /// The line still arriving.
    line: String,
    /// Plain text of the paragraph so far, each line followed by a space.
    paragraph: String,
    /// The open code fence: its character and length.
    fence: Option<(char, usize)>,
    /// Whether the last line was a table row.
    table: bool,
    /// The note said last, with nothing said since.
    last_note: Option<&'static str>,
    out: Vec<String>,
}

impl SpeechText {
    /// Nothing heard yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds streamed text; returns the sentences it completed, in order.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.line.push_str(delta);
        while let Some(at) = self.line.find('\n') {
            let line: String = self.line.drain(..=at).collect();
            self.read_line(line.trim_end_matches(['\n', '\r']));
        }
        self.complete();
        mem::take(&mut self.out)
    }

    /// The rest, at the end of the reply.
    pub fn finish(&mut self) -> Vec<String> {
        let line = mem::take(&mut self.line);
        if !line.is_empty() {
            self.read_line(line.trim_end_matches('\r'));
        }
        self.flush(false);
        let out = mem::take(&mut self.out);
        *self = Self::default();
        out
    }

    fn read_line(&mut self, raw: &str) {
        let line = raw.trim();
        if let Some((fence, len)) = self.fence {
            if fence_of(line).is_some_and(|(c, n)| c == fence && n >= len && closing(line, n)) {
                self.fence = None;
            }
            return;
        }
        if let Some(fence) = fence_of(line) {
            self.flush(false);
            self.note(CODE_NOTE);
            self.fence = Some(fence);
            self.table = false;
            return;
        }
        if line.starts_with('|') || TABLE_DELIMITER.is_match(line) {
            if !self.table {
                self.flush(false);
                self.note(TABLE_NOTE);
                self.table = true;
            }
            return;
        }
        self.table = false;
        let mut line = line;
        while let Some(rest) = line.strip_prefix('>') {
            line = rest.trim_start();
        }
        if line.is_empty() || RULE.is_match(line) {
            self.flush(false);
        } else if let Some(heading) = heading(line) {
            self.alone(heading);
        } else if let Some(item) = LIST_ITEM.captures(line) {
            let item = item.get(1).map_or("", |m| m.as_str());
            self.alone(item);
        } else {
            let text = inline(line);
            if !text.is_empty() {
                self.paragraph.push_str(&text);
                self.paragraph.push(' ');
            }
        }
    }

    /// `text` as sentences of its own, the last one ending in punctuation.
    fn alone(&mut self, text: &str) {
        self.flush(false);
        self.paragraph = inline(text);
        self.flush(true);
    }

    /// Says the paragraph's complete sentences.
    fn complete(&mut self) {
        let (sentences, rest) = split(&self.paragraph);
        if !sentences.is_empty() {
            self.paragraph = rest;
            for sentence in sentences {
                self.say(&sentence);
            }
        }
    }

    /// Says the whole paragraph; with `period`, its last sentence ends in punctuation.
    fn flush(&mut self, period: bool) {
        let (mut sentences, rest) = split(&mem::take(&mut self.paragraph));
        let rest = rest.trim();
        if !rest.is_empty() {
            sentences.push(rest.to_owned());
        }
        if period {
            if let Some(last) = sentences.last_mut() {
                if !last.ends_with(['.', '!', '?', '…', ':', ';']) {
                    last.push('.');
                }
            }
        }
        for sentence in sentences {
            self.say(&sentence);
        }
    }

    fn say(&mut self, sentence: &str) {
        for piece in split_long(sentence) {
            if piece.chars().any(char::is_alphanumeric) {
                self.out.push(piece);
                self.last_note = None;
            }
        }
    }

    /// Says `note` unless it was the last thing said.
    fn note(&mut self, note: &'static str) {
        if self.last_note != Some(note) {
            self.out.push(note.to_owned());
            self.last_note = Some(note);
        }
    }
}

/// The whole of `markdown` as sentences.
pub fn sentences(markdown: &str) -> Vec<String> {
    let mut text = SpeechText::new();
    let mut out = text.push(markdown);
    out.extend(text.finish());
    out
}

/// The fence character and length a (trimmed) line starts with: three or more backticks
/// (with no backtick after them) or tildes.
fn fence_of(line: &str) -> Option<(char, usize)> {
    let fence = line.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = line.chars().take_while(|c| *c == fence).count();
    let info = &line[len..];
    (len >= 3 && !(fence == '`' && info.contains('`'))).then_some((fence, len))
}

/// Whether a line opening with a fence of `len` characters is only the fence.
fn closing(line: &str, len: usize) -> bool {
    line[len..].trim().is_empty()
}

/// The text of an ATX heading (`#`..`######`, a space, the text, optional closing `#`s).
fn heading(line: &str) -> Option<&str> {
    let level = line.chars().take_while(|c| *c == '#').count();
    let rest = &line[level..];
    if !(1..=6).contains(&level) || !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    let text = rest.trim();
    let open = text.trim_end_matches('#');
    if open.len() != text.len() && (open.is_empty() || open.ends_with(char::is_whitespace)) {
        return Some(open.trim_end());
    }
    Some(text)
}

/// One line's inline Markdown as plain text.
fn inline(line: &str) -> String {
    let text = code_spans(line);
    let text = ESCAPE.replace_all(&text, |caps: &regex::Captures<'_>| protect(&caps[1]));
    let text = IMAGE.replace_all(&text, "$1");
    let text = LINK.replace_all(&text, "$1");
    let text = AUTOLINK.replace_all(&text, "a link");
    let text = URL.replace_all(&text, "a link");
    let text = HTML_TAG.replace_all(&text, "");
    let text = text.replace("~~", "");
    let text = emphasis(&text);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    text.chars().map(unprotect).collect()
}

/// Code spans as their (protected) content.
fn code_spans(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        let ticks = after.chars().take_while(|c| *c == '`').count();
        let body = &after[ticks..];
        match closing_ticks(body, ticks) {
            Some(close) => {
                out.push_str(&protect(body[..close].trim()));
                rest = &body[close + ticks..];
            }
            None => {
                out.push_str(&after[..ticks]);
                rest = body;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Where a run of exactly `ticks` backticks starts in `body`.
fn closing_ticks(body: &str, ticks: usize) -> Option<usize> {
    let mut at = 0;
    while let Some(found) = body[at..].find('`') {
        let start = at + found;
        let run = body[start..].chars().take_while(|c| *c == '`').count();
        if run == ticks {
            return Some(start);
        }
        at = start + run;
    }
    None
}

/// Emphasis markers (`*`, `_` runs at a word's edge) removed; `snake_case`, `2*3` and a
/// lone ` * ` stay.
fn emphasis(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '*' && chars[i] != '_' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let end = i + chars[i..]
            .iter()
            .take_while(|c| **c == '*' || **c == '_')
            .count();
        let before = i.checked_sub(1).map(|j| chars[j]);
        let after = chars.get(end).copied();
        let inside =
            before.is_some_and(char::is_alphanumeric) && after.is_some_and(char::is_alphanumeric);
        let alone = before.is_none_or(char::is_whitespace) && after.is_none_or(char::is_whitespace);
        if inside || alone {
            out.extend(&chars[i..end]);
        }
        i = end;
    }
    out
}

/// ASCII punctuation in `text` as private-use characters, safe from the inline rules.
fn protect(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_punctuation() {
                char::from_u32(PROTECTED + c as u32).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

fn unprotect(c: char) -> char {
    let code = c as u32;
    if (PROTECTED..PROTECTED + 0x80).contains(&code) {
        char::from_u32(code - PROTECTED).unwrap_or(c)
    } else {
        c
    }
}

/// The complete sentences of `text` (each ends in `.`, `!`, `?` or `…`, maybe closing
/// quotes, then whitespace) and what follows the last.
fn split(text: &str) -> (Vec<String>, String) {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut sentences = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        if !matches!(c, '.' | '!' | '?' | '…') {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < chars.len()
            && matches!(
                chars[j].1,
                '.' | '!' | '?' | '…' | '"' | '\'' | '”' | '’' | ')' | ']'
            )
        {
            j += 1;
        }
        let ends = j < chars.len() && chars[j].1.is_whitespace();
        if ends && !(c == '.' && j == i + 1 && abbreviation(text, &chars, i)) {
            let end = chars[j].0;
            sentences.push(text[start..end].trim().to_owned());
            start = end;
        }
        i = j;
    }
    (sentences, text[start..].to_owned())
}

/// Whether the word ending with the period at `chars[at]` is an abbreviation.
fn abbreviation(text: &str, chars: &[(usize, char)], at: usize) -> bool {
    let mut first = at;
    while first > 0 && !chars[first - 1].1.is_whitespace() {
        first -= 1;
    }
    let word = text[chars[first].0..chars[at].0 + 1]
        .trim_start_matches(['(', '"', '\''])
        .to_lowercase();
    ABBREVIATIONS.contains(&word.as_str())
}

/// `sentence` in pieces of at most [`MAX_SENTENCE`] characters, cut after the last comma or
/// semicolon, else at the last space, before the limit.
fn split_long(sentence: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = sentence.trim();
    while let Some((limit, _)) = rest.char_indices().nth(MAX_SENTENCE) {
        let head = &rest[..limit];
        let cut = head
            .rfind(", ")
            .max(head.rfind("; "))
            .map(|at| at + 1)
            .or_else(|| head.rfind(' '))
            .filter(|&at| at > 0)
            .unwrap_or(limit);
        pieces.push(rest[..cut].trim().to_owned());
        rest = rest[cut..].trim();
    }
    if !rest.is_empty() {
        pieces.push(rest.to_owned());
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Plan\n\nI looked at the **build** script. It fails on Windows, e.g. when \
paths have spaces! The fix costs 3.5 hours.\nSee [the docs](https://example.com/a) or \
https://example.com/b for more.\n\n```rust\nfn main() {}\n```\n\n- Run `cargo test`.\n\
2. Check the snake_case_name\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nDone… Shall I go on?";

    fn all(markdown: &str) -> Vec<String> {
        sentences(markdown)
    }

    #[test]
    fn markdown_becomes_plain_sentences() {
        assert_eq!(
            all(SAMPLE),
            [
                "Plan.",
                "I looked at the build script.",
                "It fails on Windows, e.g. when paths have spaces!",
                "The fix costs 3.5 hours.",
                "See the docs or a link for more.",
                CODE_NOTE,
                "Run cargo test.",
                "Check the snake_case_name.",
                TABLE_NOTE,
                "Done…",
                "Shall I go on?",
            ]
        );
    }

    #[test]
    fn deltas_split_anywhere_give_the_same_sentences() {
        let whole = all(SAMPLE);
        let bounds: Vec<usize> = (0..=SAMPLE.len())
            .filter(|&i| SAMPLE.is_char_boundary(i))
            .collect();
        for &i in &bounds {
            let mut text = SpeechText::new();
            let mut out = text.push(&SAMPLE[..i]);
            out.extend(text.push(&SAMPLE[i..]));
            out.extend(text.finish());
            assert_eq!(out, whole, "split at {i}");
        }
        let mut text = SpeechText::new();
        let mut out = Vec::new();
        for c in SAMPLE.chars() {
            out.extend(text.push(&c.to_string()));
        }
        out.extend(text.finish());
        assert_eq!(out, whole);
    }

    #[test]
    fn complete_sentences_come_out_while_streaming() {
        let mut text = SpeechText::new();
        assert!(text.push("Hello there. How").is_empty());
        assert_eq!(
            text.push(" are you?\nI am"),
            ["Hello there.", "How are you?"]
        );
        assert!(text.push(" fine").is_empty());
        assert_eq!(text.finish(), ["I am fine"]);
        assert!(text.finish().is_empty());
    }

    #[test]
    fn a_code_block_is_a_note_and_never_spoken() {
        let mut text = SpeechText::new();
        assert!(text.push("Here it is:\n``").is_empty());
        assert_eq!(
            text.push("`sh\nrm -rf / # secret\n"),
            ["Here it is:", CODE_NOTE]
        );
        assert!(text.push("``").is_empty());
        assert!(text.push("`\nAll set.").is_empty());
        assert_eq!(text.finish(), ["All set."]);

        let fenced = all("A.\n~~~~\n```\nstill code\n~~~\nmore code\n~~~~~\nB.");
        assert_eq!(fenced, ["A.", CODE_NOTE, "B."]);
        let two = all("```\na\n```\n```\nb\n```\nAfter.");
        assert_eq!(two, [CODE_NOTE, "After."]);
    }

    #[test]
    fn an_unclosed_fence_at_the_end_speaks_only_the_note() {
        assert_eq!(all("Look:\n```\nlet x = 1;\nlet y"), ["Look:", CODE_NOTE]);
    }

    #[test]
    fn a_table_is_one_note() {
        assert_eq!(
            all("Results:\n|a|b|\n|:-|-:|\n|1|2|\n|3|4|\nThat is all."),
            ["Results:", TABLE_NOTE, "That is all."]
        );
    }

    #[test]
    fn headings_lists_and_quotes_lose_their_markers() {
        assert_eq!(
            all(
                "## Next steps ##\n> > Quoted text\n* [x] done item\n+ [ ] open item?\n1) first\n---\n<div>\nEnd"
            ),
            [
                "Next steps.",
                "Quoted text",
                "done item.",
                "open item?",
                "first.",
                "End",
            ]
        );
    }

    #[test]
    fn inline_markup_is_dropped() {
        assert_eq!(
            all(
                "*Really* __bold__ ~~gone~~ _it_ ![a chart](c.png) <https://x.y> www.example.com, \
`a*b` <b>tag</b> 2*3 a * b \\*literal\\* snake_case."
            ),
            ["Really bold gone it a chart a link a link, a*b tag 2*3 a * b *literal* snake_case."]
        );
    }

    #[test]
    fn numbers_and_abbreviations_do_not_end_sentences() {
        assert_eq!(
            all("Pi is 3.14 roughly. Use tools, i.e. cargo, etc. and Dr. Who vs. Mr. Smith. Ok."),
            [
                "Pi is 3.14 roughly.",
                "Use tools, i.e. cargo, etc. and Dr. Who vs. Mr. Smith.",
                "Ok.",
            ]
        );
    }

    #[test]
    fn a_line_break_joins_and_a_blank_line_ends_the_paragraph() {
        assert_eq!(
            all("One line\nand the next\n\nNew paragraph"),
            ["One line and the next", "New paragraph"]
        );
    }

    #[test]
    fn a_run_on_sentence_is_split_under_three_hundred_characters() {
        let long = format!("{}end.", "word, and more words ".repeat(33));
        assert!(long.chars().count() > 690);
        let out = all(&long);
        assert!(out.len() >= 3);
        assert!(out.iter().all(|s| s.chars().count() <= 300), "{out:?}");
        assert_eq!(
            out.join(" ").split_whitespace().count(),
            long.split_whitespace().count()
        );
        assert!(out[0].ends_with(','));
        let unbroken = "x".repeat(700);
        assert!(all(&unbroken).iter().all(|s| s.chars().count() <= 300));
    }

    #[test]
    fn sentences_without_letters_or_digits_are_dropped() {
        assert!(all("...\n\n- \n\n***\n\n!!").is_empty());
    }
}
