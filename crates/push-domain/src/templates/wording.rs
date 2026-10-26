//! Wording rules: what a definition's literal text, and a body's string values, may hold.

use unicode_normalization::UnicodeNormalization;

/// Schemes refused at the start of a word in literal text.
const SCHEMES: &[&str] = &[
    "javascript:",
    "data:",
    "vbscript:",
    "file:",
    "mailto:",
    "tel:",
    "sms:",
    "blob:",
    "about:",
    "intent:",
    "ftp:",
    "http:",
    "https:",
];

/// Top-level domains that make a dotted token look like a host.
const TLDS: &[&str] = &[
    "com", "net", "org", "io", "dev", "app", "ai", "co", "info", "biz", "me", "xyz", "ly", "gl",
    "gg", "to", "sh", "ru", "cn", "uk", "de", "fr", "eu", "us", "ca", "au", "in", "jp", "tv", "cc",
    "link", "site", "online", "top", "club", "page",
];

const MARKUP: &[char] = &['<', '>', '`', '{', '}'];

/// Whether `text` holds anything a client might turn into a link: `//`, `www.`, a scheme at
/// the start of a word (a known one, or two or more letters, `:` and then neither a space nor
/// a digit), a dotted-quad IPv4 address, or a dotted host ending in a common top-level
/// domain. Case-insensitive, and compatibility forms (fullwidth letters, ideographic full
/// stops, ...) and Cyrillic or Greek Latin lookalikes count as the characters they stand
/// for.
pub fn looks_like_url(text: &str) -> bool {
    url_like(&normalise(text))
}

/// NFKC, then the dot lookalikes NFKC leaves alone mapped to `.` and Cyrillic and Greek
/// letters that look like Latin ones mapped to those, then lowercase: `example.соm`
/// (Cyrillic `с` and `о`) is `example.com`.
fn normalise(text: &str) -> String {
    text.nfkc()
        .map(|c| match c {
            '\u{3002}' | '\u{FF61}' | '\u{FE52}' | '\u{2024}' => '.',
            c => latin_lookalike(c).unwrap_or(c),
        })
        .collect::<String>()
        .to_lowercase()
}

/// The ASCII letter a common Cyrillic or Greek homoglyph stands for.
fn latin_lookalike(c: char) -> Option<char> {
    Some(match c {
        // Cyrillic
        'а' | 'А' => 'a',
        'е' | 'Е' => 'e',
        'о' | 'О' => 'o',
        'р' | 'Р' => 'p',
        'с' | 'С' => 'c',
        'у' | 'У' => 'y',
        'х' | 'Х' => 'x',
        'і' | 'І' | 'Ӏ' => 'i',
        'ј' | 'Ј' => 'j',
        'ѕ' | 'Ѕ' => 's',
        'ԁ' | 'Ԁ' => 'd',
        'һ' | 'Һ' => 'h',
        'ӏ' => 'l',
        'ԛ' | 'Ԛ' => 'q',
        'ԝ' | 'Ԝ' => 'w',
        // Greek
        'ο' | 'Ο' => 'o',
        'α' | 'Α' => 'a',
        'ε' | 'Ε' => 'e',
        'ι' | 'Ι' => 'i',
        'κ' | 'Κ' => 'k',
        'ν' => 'v',
        'Ν' => 'n',
        'ρ' | 'Ρ' => 'p',
        'τ' | 'Τ' => 't',
        'υ' => 'u',
        'Υ' => 'y',
        'χ' | 'Χ' => 'x',
        _ => return None,
    })
}

/// `looks_like_url` on text already normalised.
fn url_like(text: &str) -> bool {
    if text.contains("//") || text.contains("www.") {
        return true;
    }
    let scheme_at_word_start = SCHEMES.iter().any(|scheme| {
        text.match_indices(scheme).any(|(i, _)| {
            text[..i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric())
        })
    });
    let chars: Vec<char> = text.chars().collect();
    scheme_at_word_start || has_generic_scheme(&chars) || has_ipv4(&chars) || has_dotted_host(text)
}

/// Two or more ASCII letters at the start of a word, `:`, then neither whitespace nor a
/// digit: `magnet:?`, `tg:x`, but not `ready: yes` or `10:30`.
fn has_generic_scheme(chars: &[char]) -> bool {
    chars.iter().enumerate().any(|(i, &c)| {
        if c != ':' {
            return false;
        }
        let letters = chars[..i]
            .iter()
            .rev()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
        let at_word_start = i
            .checked_sub(letters + 1)
            .is_none_or(|before| !chars[before].is_alphanumeric());
        let next_is_target = chars
            .get(i + 1)
            .is_some_and(|c| !c.is_whitespace() && !c.is_ascii_digit());
        letters >= 2 && at_word_start && next_is_target
    })
}

/// `\b\d{1,3}(\.\d{1,3}){3}\b`.
fn has_ipv4(chars: &[char]) -> bool {
    let is_word = |c: &char| c.is_alphanumeric() || *c == '_';
    (0..chars.len()).any(|start| {
        if start > 0 && is_word(&chars[start - 1]) {
            return false;
        }
        let mut at = start;
        for group in 0..4 {
            if group > 0 {
                if chars.get(at) != Some(&'.') {
                    return false;
                }
                at += 1;
            }
            let digits = chars[at..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count();
            if !(1..=3).contains(&digits) {
                return false;
            }
            at += digits;
        }
        chars.get(at).is_none_or(|c| !is_word(c))
    })
}

/// A `label.tld` pair inside a run of `[a-z0-9.-]`, the labels compared whole.
fn has_dotted_host(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
        .any(|run| {
            let labels: Vec<&str> = run.split('.').collect();
            labels
                .windows(2)
                .any(|pair| !pair[0].is_empty() && TLDS.contains(&pair[1]))
        })
}

/// Checks literal text in a definition: 1 to `max_chars` characters, not only whitespace;
/// no control characters (but `\n` when `allow_newlines`), no invisible or formatting
/// characters, no markup (`<`, `>`, `` ` ``, `{`, `}`, `](`) and nothing URL-like, judged
/// after the same normalisation as `looks_like_url`.
pub fn check_literal(text: &str, max_chars: usize, allow_newlines: bool) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("is empty".to_owned());
    }
    check_value(text, max_chars, allow_newlines)?;
    let text = normalise(text);
    if let Some(c) = text.chars().find(|c| MARKUP.contains(c)) {
        return Err(format!("contains markup ({c:?})"));
    }
    if text.contains("](") {
        return Err("contains markup (\"](\")".to_owned());
    }
    if url_like(&text) {
        return Err("looks like a URL".to_owned());
    }
    Ok(())
}

/// Checks a string value in a body: at most `max_chars` characters, no control characters
/// (but `\n` when `multiline`), no line or paragraph separators and no invisible or formatting characters. Clients render
/// values inert, so URLs and markup are allowed.
pub fn check_value(text: &str, max_chars: usize, multiline: bool) -> Result<(), String> {
    let chars = text.chars().count();
    if chars > max_chars {
        return Err(format!("is {chars} characters; the limit is {max_chars}"));
    }
    for c in text.chars() {
        if c.is_control() && !(multiline && c == '\n') {
            return Err(format!("contains the control character U+{:04X}", c as u32));
        }
        if matches!(c, '\u{2028}' | '\u{2029}') {
            return Err(format!(
                "contains the line or paragraph separator U+{:04X}",
                c as u32
            ));
        }
        if is_invisible(c) {
            return Err(format!(
                "contains the invisible or formatting character U+{:04X}",
                c as u32
            ));
        }
    }
    Ok(())
}

/// Characters that render as nothing or reorder text: soft hyphen, fillers, zero-width
/// and bidi controls, Mongolian and other variation selectors, interlinear annotation
/// controls, musical and shorthand format controls, the braille blank, the BOM and tags.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{AD}'
            | '\u{34F}'
            | '\u{61C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{2800}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E007F}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_like_text_is_recognised() {
        for text in [
            "https://x",
            "HTTP://X",
            "www.x",
            "javascript:alert(1)",
            "data:text/html",
            "see example.com",
            "a.b.io",
            "//host",
            "Mail (mailto:a)",
            "EXAMPLE.CO.UK.",
        ] {
            assert!(looks_like_url(text), "{text}");
        }
    }

    #[test]
    fn ordinary_text_is_not_url_like() {
        for text in [
            "e.g. this",
            "version 1.2",
            "Approve",
            "Ready: yes",
            "Deploy to prod?",
            "metadata: none",
            "example.community",
        ] {
            assert!(!looks_like_url(text), "{text}");
        }
    }

    #[test]
    fn literals_refuse_markup_urls_and_hidden_characters() {
        assert_eq!(check_literal("Approve", 80, false), Ok(()));
        assert_eq!(check_literal("two\nlines", 500, true), Ok(()));
        for (text, newlines) in [
            ("", false),
            ("  \n ", true),
            ("two\nlines", false),
            ("tab\there", false),
            ("<b>bold</b>", true),
            ("a `code`", false),
            ("{x}", false),
            ("[a](b)", false),
            ("go to example.com", false),
            ("right\u{202E}left", false),
            ("zero\u{200B}width", false),
        ] {
            assert!(check_literal(text, 80, newlines).is_err(), "{text:?}");
        }
        assert!(check_literal(&"é".repeat(80), 80, false).is_ok());
        assert!(check_literal(&"é".repeat(81), 80, false).is_err());
    }

    #[test]
    fn values_allow_urls_and_markup_but_not_hidden_characters() {
        assert_eq!(check_value("", 10, false), Ok(()));
        assert_eq!(check_value("<b> https://x.com", 100, false), Ok(()));
        assert_eq!(check_value("a\nb", 10, true), Ok(()));
        assert!(check_value("a\nb", 10, false).is_err());
        assert!(check_value("a\u{202E}b", 10, true).is_err());
        assert!(check_value("a\u{0}b", 10, true).is_err());
        assert!(check_value(&"ü".repeat(11), 10, false).is_err());
    }

    #[test]
    fn lookalike_characters_do_not_hide_urls() {
        for text in [
            "ｗｗｗ．example．com",
            "example。com",
            "example．com",
            "example｡com",
            "example․com",
            "ｅｘａｍｐｌｅ.ｃｏｍ",
            "example.ｃｏｍ",
            "javascript：x",
        ] {
            assert!(looks_like_url(text), "{text}");
            assert!(check_literal(text, 80, false).is_err(), "{text}");
        }
    }

    #[test]
    fn cyrillic_and_greek_lookalikes_do_not_hide_urls() {
        for text in [
            "example.\u{0441}\u{043E}m",
            "EXAMPLE.\u{0421}\u{041E}M",
            "\u{0430}pp.\u{0456}\u{043E}",
            "example.c\u{03BF}m",
            "example.\u{039F}RG",
            "\u{0445}\u{0443}z.n\u{03B5}t",
        ] {
            assert!(looks_like_url(text), "{text}");
            assert!(check_literal(text, 80, false).is_err(), "{text}");
        }
        for text in ["Привет, мир", "Καλημέρα", "Одобрить развёртывание?"]
        {
            assert!(!looks_like_url(text), "{text}");
            assert_eq!(check_literal(text, 80, false), Ok(()), "{text}");
        }
    }

    #[test]
    fn lookalike_characters_do_not_hide_markup() {
        for text in ["＜b＞", "a ｛x｝", "[a]（b）"] {
            assert!(check_literal(text, 80, false).is_err(), "{text}");
        }
    }

    #[test]
    fn ip_addresses_and_unknown_schemes_are_url_like() {
        for text in [
            "10.0.0.1",
            "go to 192.168.1.20 now",
            "magnet:?x",
            "tg:foo",
            "skype:bob",
            "(Skype:Bob)",
        ] {
            assert!(looks_like_url(text), "{text}");
        }
        for text in [
            "Ready: yes",
            "Step 1:2",
            "at 10:30",
            "version 1.2.3",
            "a:b",
            "x1:y",
        ] {
            assert!(!looks_like_url(text), "{text}");
        }
    }

    #[test]
    fn line_and_paragraph_separators_are_refused() {
        for c in ['\u{2028}', '\u{2029}'] {
            let text = format!("a{c}b");
            assert!(check_value(&text, 10, true).is_err(), "{c:?}");
            assert!(check_value(&text, 10, false).is_err(), "{c:?}");
            assert!(check_literal(&text, 10, true).is_err(), "{c:?}");
        }
    }

    #[test]
    fn more_invisible_characters_are_refused() {
        for c in [
            '\u{180B}',
            '\u{180F}',
            '\u{E0100}',
            '\u{E01EF}',
            '\u{FFF9}',
            '\u{FFFB}',
            '\u{1D173}',
            '\u{1D17A}',
            '\u{1BCA0}',
            '\u{1BCA3}',
            '\u{2800}',
        ] {
            assert!(check_value(&format!("a{c}b"), 10, true).is_err(), "{c:?}");
        }
    }
}
