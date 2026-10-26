//! Wording rules: what a definition's literal text, and a body's string values, may hold.

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

/// Whether `text` holds anything a client might turn into a link: `//`, `www.`, a known
/// scheme at the start of a word, or a dotted host ending in a common top-level domain.
/// Case-insensitive.
pub fn looks_like_url(text: &str) -> bool {
    let text = text.to_lowercase();
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
    scheme_at_word_start || has_dotted_host(&text)
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
/// characters, no markup (`<`, `>`, `` ` ``, `{`, `}`, `](`) and nothing URL-like.
pub fn check_literal(text: &str, max_chars: usize, allow_newlines: bool) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("is empty".to_owned());
    }
    check_value(text, max_chars, allow_newlines)?;
    if let Some(c) = text.chars().find(|c| MARKUP.contains(c)) {
        return Err(format!("contains markup ({c:?})"));
    }
    if text.contains("](") {
        return Err("contains markup (\"](\")".to_owned());
    }
    if looks_like_url(text) {
        return Err("looks like a URL".to_owned());
    }
    Ok(())
}

/// Checks a string value in a body: at most `max_chars` characters, no control characters
/// (but `\n` when `multiline`) and no invisible or formatting characters. Clients render
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
/// and bidi controls, variation selectors, the BOM and tags.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{AD}'
            | '\u{34F}'
            | '\u{61C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{E0000}'..='\u{E007F}'
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
}
