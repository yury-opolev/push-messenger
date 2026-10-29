//! Secret masking and character caps for everything that leaves the machine: previews,
//! plans, step labels, thinking, text and command results.
//!
//! Masking errs on the side of hiding: a false positive costs a word of output, a false
//! negative leaks a secret into a message history the server keeps unencrypted.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

/// What a secret becomes.
pub const MASK: &str = "«masked»";

/// The words that make a name a secret's name.
macro_rules! secret_word {
    () => {
        "(?:key|secret|token|password|passwd|pwd)"
    };
}

/// A value after a name: a quoted string up to its closing quote (or the line's end), or
/// an unquoted run up to whitespace, `;`, `,`, `&`, `}` or `)`. An unquoted value never
/// starts with `=`, `:` or `>`, so `a == b`, `Type::path` and `key => value` are no pairs.
macro_rules! value {
    () => {
        r#"(?P<value>"[^"\r\n]*"?|'[^'\r\n]*'?|[^\s;,&})"'=:>][^\s;,&})]*)"#
    };
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid pattern")
}

/// `-----BEGIN … KEY-----` to its `-----END … KEY-----`, or to the end of the text.
static PEM: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"(?s)-----BEGIN [A-Z0-9 ]*KEY-----.*?(?:-----END [A-Z0-9 ]*KEY-----|\z)")
});

static BEARER: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)\b(?P<scheme>bearer)\s+[A-Za-z0-9\-._~+/]+=*"));

/// Also a JWS with an empty signature (`alg: none`).
static JWT: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*"));

static CONNECTION_STRING: LazyLock<Regex> = LazyLock::new(|| {
    regex(r#"(?i)\b(?P<name>AccountKey|SharedAccessKey|SharedAccessSignature)=[^;\s"']+"#)
});

/// The signature of a SAS URL (`…?sv=…&sig=…`): the same secret as a
/// `SharedAccessSignature=` value, in a URL.
static SAS_SIGNATURE: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)(?P<name>[?&]sig=)[^&\s"'#]+"#));

/// `name=value`, `name: value`, `"name": "value"`, `'name'='value'` whose name is an
/// identifier (no spaces: `token count: 12` is no pair) containing a secret word.
static PAIR: LazyLock<Regex> = LazyLock::new(|| {
    regex(concat!(
        r#"(?i)(?P<name>["']?[A-Za-z0-9_.-]*"#,
        secret_word!(),
        r#"[A-Za-z0-9_.-]*["']?)(?P<delimiter>[ \t]*[=:][ \t]*)"#,
        value!(),
    ))
});

/// A long command-line flag naming a secret, then its value: `--password X`. The value
/// does not start with `-` (that is the next flag); `--name=value` is a [`PAIR`].
static FLAG: LazyLock<Regex> = LazyLock::new(|| {
    regex(concat!(
        r"(?i)(?P<name>--[A-Za-z0-9_-]*",
        secret_word!(),
        r#"[A-Za-z0-9_-]*)(?P<delimiter>[ \t]+)(?P<value>"[^"\r\n]*"?|'[^'\r\n]*'?|[^\s"'-]\S*)"#,
    ))
});

static HEX_RUN: LazyLock<Regex> = LazyLock::new(|| regex(r"\b[0-9a-fA-F]{40,}\b"));

/// Base64 and base64url together, so a run mixing both alphabets is one run.
static BASE64_RUN: LazyLock<Regex> = LazyLock::new(|| regex(r"[A-Za-z0-9+/_-]{40,}={0,2}"));

/// A path segment made of words: lower-case letters and digits, each word optionally
/// capitalised (camelCase, PascalCase), joined by `-` or `_`.
static PATH_SEGMENT: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^(?:[A-Z]?[a-z0-9]+)+(?:[-_](?:[A-Z]?[a-z0-9]+)+)*$"));

/// Masks every secret pattern in `text`.
pub fn mask(text: &str) -> String {
    let text = PEM.replace_all(text, MASK);
    let text = replace(text, &BEARER, |caps| format!("{} {MASK}", &caps["scheme"]));
    let text = replace(text, &JWT, |_| MASK.to_owned());
    let text = replace(text, &CONNECTION_STRING, |caps| {
        format!("{}={MASK}", &caps["name"])
    });
    let text = replace(text, &SAS_SIGNATURE, |caps| {
        format!("{}{MASK}", &caps["name"])
    });
    let text = replace(text, &PAIR, mask_value);
    let text = replace(text, &FLAG, mask_value);
    let text = replace(text, &HEX_RUN, |_| MASK.to_owned());
    let text = replace(text, &BASE64_RUN, |caps| mask_run(&caps[0]));
    text.into_owned()
}

fn replace<'t>(
    text: Cow<'t, str>,
    pattern: &Regex,
    with: impl FnMut(&Captures<'_>) -> String,
) -> Cow<'t, str> {
    let replaced = match pattern.replace_all(&text, with) {
        Cow::Borrowed(_) => None,
        Cow::Owned(replaced) => Some(replaced),
    };
    match replaced {
        Some(replaced) => Cow::Owned(replaced),
        None => text,
    }
}

/// Keeps the name, the delimiter and a quoted value's quotes; masks the value.
fn mask_value(caps: &Captures<'_>) -> String {
    let value = &caps["value"];
    let (open, close) = match value.chars().next() {
        Some(quote @ ('"' | '\'')) => {
            let closed = value.len() > 1 && value.ends_with(quote);
            (Some(quote), closed.then_some(quote))
        }
        _ => (None, None),
    };
    let mut masked = format!("{}{}", &caps["name"], &caps["delimiter"]);
    masked.extend(open);
    masked.push_str(MASK);
    masked.extend(close);
    masked
}

/// A base64 run is masked, unless it is a path: long Unix and relative paths
/// (`/home/me/projects/push-messenger/crates/…`) are runs of the base64 alphabet too.
/// A run counts as a path only when it has at least two `/`, no `+`, no `=` padding, and
/// every segment is shorter than 40 characters and made of words ([`PATH_SEGMENT`]). A
/// random base64 string almost never passes the word test: it has upper-case letters in
/// a row or digits followed by capitals. (Windows paths need no exception: `\` and `:`
/// end a run.)
fn mask_run(run: &str) -> String {
    let path_like = !run.contains('+')
        && !run.ends_with('=')
        && run.matches('/').count() >= 2
        && run.split('/').all(|segment| {
            segment.is_empty() || (segment.len() < 40 && PATH_SEGMENT.is_match(segment))
        });
    if path_like {
        run.to_owned()
    } else {
        MASK.to_owned()
    }
}

/// At most `max` characters; when cut, ends with "…" (the result then has `max` chars).
pub fn cap(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut capped: String = text.chars().take(max - 1).collect();
    capped.push('…');
    capped
}

/// `cap(mask(text), max)`: masked before it is cut, so a cut never shortens a secret
/// below the length that marks it.
pub fn masked(text: &str, max: usize) -> String {
    cap(&mask(text), max)
}

/// One line: line breaks and tabs become spaces and runs of spaces collapse; then masked
/// and capped.
pub fn masked_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    masked(&line, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    const B64_40: &str = "QWxhZGRpbjpvcGVuIHNlc2FtZQ0K4fG7hJk2LmNo";

    fn masks(input: &str, expected: &str) {
        assert_eq!(mask(input), expected, "input: {input}");
    }

    fn keeps(input: &str) {
        assert_eq!(mask(input), input);
    }

    #[test]
    fn a_private_key_block_is_masked() {
        masks(
            "key:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\nabc\n-----END RSA PRIVATE KEY-----\ndone",
            "key:\n«masked»\ndone",
        );
        masks(
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY----- and -----BEGIN EC PRIVATE KEY-----x-----END EC PRIVATE KEY-----",
            "«masked» and «masked»",
        );
    }

    #[test]
    fn an_unterminated_private_key_block_is_masked_to_the_end() {
        masks(
            "before -----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjE\nmore",
            "before «masked»",
        );
    }

    #[test]
    fn a_bearer_token_is_masked() {
        masks(
            "Authorization: Bearer abc.DEF-123_~+/xyz== next",
            "Authorization: Bearer «masked» next",
        );
        masks(
            "curl -H 'authorization: bearer t0k3n'",
            "curl -H 'authorization: bearer «masked»'",
        );
    }

    #[test]
    fn a_jwt_is_masked() {
        masks(
            "token eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl here",
            "token «masked» here",
        );
    }

    #[test]
    fn a_jws_with_an_empty_signature_is_masked() {
        masks("x eyJhbGciOiJub25lIn0.eyJzdWIiOiIxIn0. y", "x «masked» y");
    }

    #[test]
    fn connection_string_secrets_are_masked() {
        masks(
            "DefaultEndpointsProtocol=https;AccountName=demo;AccountKey=abc/def+ghi==;EndpointSuffix=core.windows.net",
            "DefaultEndpointsProtocol=https;AccountName=demo;AccountKey=«masked»;EndpointSuffix=core.windows.net",
        );
        masks(
            "Endpoint=sb://x.servicebus.windows.net/;SharedAccessKeyName=root;SharedAccessKey=k3y",
            "Endpoint=sb://x.servicebus.windows.net/;SharedAccessKeyName=«masked»;SharedAccessKey=«masked»",
        );
        masks(
            "SharedAccessSignature=sv=2020&sig=abc;BlobEndpoint=https://x",
            "SharedAccessSignature=«masked»;BlobEndpoint=https://x",
        );
    }

    #[test]
    fn a_sas_signature_in_a_url_is_masked() {
        masks(
            "https://acct.blob.core.windows.net/c/b?sv=2022-11-02&sig=a%2Bb%3D&se=2026",
            "https://acct.blob.core.windows.net/c/b?sv=2022-11-02&sig=«masked»&se=2026",
        );
    }

    #[test]
    fn a_name_value_pair_with_a_secret_name_is_masked() {
        masks("api_key=abc123 next", "api_key=«masked» next");
        masks("PGPASSWORD=hunter2 psql", "PGPASSWORD=«masked» psql");
        masks("db_pwd=x;user=y", "db_pwd=«masked»;user=y");
        masks(
            "export GITHUB_TOKEN = ghp_abc",
            "export GITHUB_TOKEN = «masked»",
        );
        masks(
            "https://x/cb?access_token=abc&state=1",
            "https://x/cb?access_token=«masked»&state=1",
        );
    }

    #[test]
    fn a_name_colon_value_pair_is_masked() {
        masks(
            "clientSecret: s3cr3t, other: 1",
            "clientSecret: «masked», other: 1",
        );
        masks("{password:abc}", "{password:«masked»}");
    }

    #[test]
    fn a_quoted_pair_is_masked_up_to_its_closing_quote() {
        masks(
            r#"{"client_secret": "two words", "id": "x"}"#,
            r#"{"client_secret": "«masked»", "id": "x"}"#,
        );
        masks("'apiKey'='a b c' rest", "'apiKey'='«masked»' rest");
        masks(r#"$env:API_KEY = "abc""#, r#"$env:API_KEY = "«masked»""#);
    }

    #[test]
    fn a_long_flag_naming_a_secret_masks_its_value() {
        masks(
            "tool --password hunter2 --verbose",
            "tool --password «masked» --verbose",
        );
        masks(
            "tool --api-token \"a b\" x",
            "tool --api-token \"«masked»\" x",
        );
        masks(
            "tool --client-secret=abc x",
            "tool --client-secret=«masked» x",
        );
        keeps("tool --token-file --verbose");
    }

    #[test]
    fn a_hex_run_of_40_is_masked_and_39_is_not() {
        let hex40 = "0123456789abcdef0123456789ABCDEF01234567";
        masks(&format!("sha {hex40} end"), "sha «masked» end");
        keeps(&format!("sha {} end", &hex40[..39]));
    }

    #[test]
    fn a_base64_run_of_40_is_masked() {
        masks(&format!("k {B64_40} e"), "k «masked» e");
        masks(
            "k QWxhZGRpbjpvcGVuIHNlc2FtZQ0K4fG7hJk2LmNoZQ== e",
            "k «masked» e",
        );
        masks(
            "k QWxh_ZGRpbjpvcGVu-IHNlc2FtZQ0K4fG7hJk2LmNo e",
            "k «masked» e",
        );
    }

    #[test]
    fn a_base64_run_with_slashes_is_masked() {
        masks(
            "k QWxhZGRp/bjpvcGVuIHNlc2/FtZQ0K4fG7hJk2LmNo e",
            "k «masked» e",
        );
        masks(
            "k a/b/QWxhZGRpbjpvcGVuIHNlc2FtZQ0K4fG7hJk2LmNo e",
            "k «masked» e",
        );
        masks(
            "k some/word/like/segments+with/plus/abcdefgh e",
            "k «masked» e",
        );
    }

    #[test]
    fn a_long_path_is_not_masked() {
        keeps("/home/someone/projects/push-messenger/crates/push-bridge/src");
        keeps("docs/superpowers/plans/2026-10-29-agent-bridge-implementation");
        keeps("/Users/Someone/Projects/PushMessenger/crates/push_bridge/src");
    }

    #[test]
    fn ordinary_text_is_not_masked() {
        for input in [
            "src/main.rs",
            r"C:\Users\me\project\file.txt",
            "/usr/local/share/doc/package/README.md",
            "cargo test --workspace",
            "the key is in the drawer",
            "token count: 12",
            "123e4567-e89b-12d3-a456-426614174000",
            "commit 56abe0c fixed it",
            "https://github.com/yury-opolev/push-messenger/pull/123",
            "SecretStore::load(&self, key)",
            "match key => value",
            "if key == other {",
        ] {
            keeps(input);
        }
    }

    /// Accepted false positives: any identifier containing a secret word counts as a
    /// secret name.
    #[test]
    fn a_name_merely_containing_a_secret_word_is_masked() {
        masks("monkey=banana", "monkey=«masked»");
        masks("keyboard: logitech", "keyboard: «masked»");
    }

    #[test]
    fn masking_twice_changes_nothing_more() {
        let once = mask("api_key=abc Bearer xyz AccountKey=q");
        assert_eq!(mask(&once), once);
    }

    #[test]
    fn cap_counts_characters() {
        assert_eq!(cap("short", 10), "short");
        assert_eq!(cap("exactly", 7), "exactly");
        assert_eq!(cap("abcdefgh", 5), "abcd…");
        assert_eq!(cap("ééééé", 3), "éé…");
        assert_eq!(cap("ééééé", 3).chars().count(), 3);
        assert_eq!(cap("abc", 0), "");
    }

    #[test]
    fn masked_masks_before_it_cuts() {
        let secret = "zq9".repeat(15);
        let shown = masked(&format!("a long line then the value {secret}"), 30);
        assert!(!shown.contains("zq9"), "{shown}");
        assert_eq!(shown.chars().count(), 30);
    }

    #[test]
    fn masked_line_flattens_then_masks() {
        assert_eq!(
            masked_line("  first\r\nsecond\t\tthird  \n password:\n hunter2 ", 100),
            "first second third password: «masked»"
        );
        assert_eq!(masked_line("a\nb c d e", 5), "a b …");
    }
}
