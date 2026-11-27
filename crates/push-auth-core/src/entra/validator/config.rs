//! The settings of an external token validator: `PUSH_ENTRA_VALIDATOR_*`.

use std::time::Duration;

use reqwest::Url;
use reqwest::header::{HeaderName, HeaderValue};
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::Value;

use crate::config::ConfigError;

const URL_VARIABLE: &str = "PUSH_ENTRA_VALIDATOR_URL";
const TIMEOUT_VARIABLE: &str = "PUSH_ENTRA_VALIDATOR_TIMEOUT_MS";
const SECRET_VARIABLE: &str = "PUSH_ENTRA_VALIDATOR_SECRET_FILE";
const PROTOCOL_VARIABLE: &str = "PUSH_ENTRA_VALIDATOR_PROTOCOL";
const HEADERS_VARIABLE: &str = "PUSH_ENTRA_VALIDATOR_REQUEST_HEADERS";
const CORRELATION_VARIABLE: &str = "PUSH_ENTRA_VALIDATOR_CORRELATION_HEADER";
const PREFIX_VARIABLE: &str = "PUSH_ENTRA_VALIDATOR_CLAIM_HEADER_PREFIX";
const ENCODED_VARIABLE: &str = "PUSH_ENTRA_VALIDATOR_ENCODED_CLAIM_HEADER_PREFIX";
const DEFAULT_TIMEOUT_MS: u64 = 2000;
const MAX_SECRET_BYTES: usize = 1024;
const DEFAULT_CORRELATION_HEADER: &str = "x-request-id";
const DEFAULT_CLAIM_PREFIX: &str = "subject-token-claim-";
const DEFAULT_ENCODED_CLAIM_PREFIX: &str = "subject-token-encoded-claim-";
/// The most static request headers.
const MAX_REQUEST_HEADERS: usize = 32;
/// The longest static request header value, in characters.
const MAX_HEADER_VALUE_CHARS: usize = 1024;
/// The headers push-auth sets itself (lower case); the correlation header is one too.
const RESERVED_HEADERS: [&str; 18] = [
    "authorization",
    "original-uri",
    "original-method",
    "x-forwarded-for",
    "host",
    "content-length",
    "content-type",
    "accept",
    "transfer-encoding",
    "connection",
    "x-push-validator-secret",
    "te",
    "upgrade",
    "expect",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "trailer",
];

/// How push-auth talks to the validator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    /// `{"provider": …}` in, `{"claims": {…}}` out.
    #[default]
    Json,
    /// Forward-auth: an empty `POST`, the claims in response headers.
    Headers,
}

/// Where and how to reach an external token validator. `Debug` never shows the secret
/// or the values of the static request headers.
#[derive(Clone, PartialEq, Eq)]
pub struct ValidatorSettings {
    /// The endpoint: `https://`, or `http://` to a loopback host.
    pub url: String,
    /// The whole request's time limit, connecting included.
    pub timeout: Duration,
    /// Sent as `X-Push-Validator-Secret` when set.
    pub secret: Option<String>,
    /// The wire protocol.
    pub protocol: Protocol,
    /// Sent with every request, after push-auth's own headers.
    pub request_headers: Vec<(HeaderName, HeaderValue)>,
    /// Carries the sign-in attempt's request id; `None` sends none.
    pub correlation_header: Option<HeaderName>,
    /// The lower-case name prefix of plain claim headers (headers protocol).
    pub claim_prefix: String,
    /// The lower-case name prefix of base64 claim headers; `None` reads none.
    pub encoded_claim_prefix: Option<String>,
}

impl std::fmt::Debug for ValidatorSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let request_headers: Vec<(&str, &str)> = self
            .request_headers
            .iter()
            .map(|(name, _)| (name.as_str(), "<redacted>"))
            .collect();
        formatter
            .debug_struct("ValidatorSettings")
            .field("url", &self.url)
            .field("timeout", &self.timeout)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("protocol", &self.protocol)
            .field("request_headers", &request_headers)
            .field("correlation_header", &self.correlation_header)
            .field("claim_prefix", &self.claim_prefix)
            .field("encoded_claim_prefix", &self.encoded_claim_prefix)
            .finish()
    }
}

impl ValidatorSettings {
    /// The validator at `url` with every other setting at its default: a two-second
    /// timeout, no secret, the JSON protocol, no static headers, `X-Request-Id`, and the
    /// `Subject-Token-Claim-` and `Subject-Token-Encoded-Claim-` prefixes.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            secret: None,
            protocol: Protocol::Json,
            request_headers: Vec::new(),
            correlation_header: Some(HeaderName::from_static(DEFAULT_CORRELATION_HEADER)),
            claim_prefix: DEFAULT_CLAIM_PREFIX.to_owned(),
            encoded_claim_prefix: Some(DEFAULT_ENCODED_CLAIM_PREFIX.to_owned()),
        }
    }

    /// Reads `PUSH_ENTRA_VALIDATOR_*`: `None` when the URL is unset or empty. Errors name
    /// the variable; they show the secret file's path, never its content, no static
    /// header value and no URL with user info.
    pub(in crate::entra) fn from_lookup(
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, ConfigError> {
        // Unset and empty are the same, except where empty disables a default.
        let raw = |name: &str| lookup(name).map(|value| value.trim().to_owned());
        let get = |name: &str| raw(name).filter(|value| !value.is_empty());
        let timeout = get(TIMEOUT_VARIABLE);
        let secret_file = get(SECRET_VARIABLE);
        let Some(url) = get(URL_VARIABLE) else {
            let only_with_url = "set only with PUSH_ENTRA_VALIDATOR_URL";
            if let Some(timeout) = timeout {
                return Err(invalid(TIMEOUT_VARIABLE, &timeout, only_with_url));
            }
            if let Some(path) = secret_file {
                return Err(invalid(SECRET_VARIABLE, &path, only_with_url));
            }
            for name in [
                PROTOCOL_VARIABLE,
                CORRELATION_VARIABLE,
                PREFIX_VARIABLE,
                ENCODED_VARIABLE,
            ] {
                if let Some(value) = get(name) {
                    return Err(invalid(name, &value, only_with_url));
                }
            }
            if get(HEADERS_VARIABLE).is_some() {
                return Err(invalid(HEADERS_VARIABLE, REDACTED, only_with_url));
            }
            return Ok(None);
        };
        let url = validator_url(&url).map_err(|(shown, reason)| {
            invalid(URL_VARIABLE, if shown { &url } else { REDACTED }, reason)
        })?;
        let timeout = match timeout {
            None => Duration::from_millis(DEFAULT_TIMEOUT_MS),
            Some(value) => value
                .parse::<u64>()
                .ok()
                .filter(|millis| (100..=10_000).contains(millis))
                .map(Duration::from_millis)
                .ok_or_else(|| {
                    invalid(
                        TIMEOUT_VARIABLE,
                        &value,
                        "use a whole number of milliseconds from 100 to 10000",
                    )
                })?,
        };
        let secret = match secret_file {
            None => None,
            Some(path) => {
                Some(read_secret(&path).map_err(|reason| invalid(SECRET_VARIABLE, &path, reason))?)
            }
        };
        let protocol = match get(PROTOCOL_VARIABLE) {
            None => Protocol::Json,
            Some(value) => match value.to_ascii_lowercase().as_str() {
                "json" => Protocol::Json,
                "headers" => Protocol::Headers,
                _ => return Err(invalid(PROTOCOL_VARIABLE, &value, "use json or headers")),
            },
        };
        let correlation_header = match raw(CORRELATION_VARIABLE) {
            None => Some(HeaderName::from_static(DEFAULT_CORRELATION_HEADER)),
            Some(value) if value.is_empty() => None,
            Some(value) => {
                let name = header_name(&value)
                    .filter(|name| !RESERVED_HEADERS.contains(&name.as_str()))
                    .ok_or_else(|| {
                        invalid(
                            CORRELATION_VARIABLE,
                            &value,
                            "must be a header name (an RFC 9110 token) that push-auth does \
                             not set itself, or empty to send no request id",
                        )
                    })?;
                Some(name)
            }
        };
        let request_headers = match get(HEADERS_VARIABLE) {
            None => Vec::new(),
            Some(value) => request_headers(&value, correlation_header.as_ref())
                .map_err(|reason| invalid(HEADERS_VARIABLE, REDACTED, &reason))?,
        };
        let prefix_reason = "must be a header name prefix (an RFC 9110 token)";
        let claim_prefix = match get(PREFIX_VARIABLE) {
            None => DEFAULT_CLAIM_PREFIX.to_owned(),
            Some(value) => {
                prefix(&value).ok_or_else(|| invalid(PREFIX_VARIABLE, &value, prefix_reason))?
            }
        };
        let encoded_claim_prefix = match raw(ENCODED_VARIABLE) {
            None => Some(DEFAULT_ENCODED_CLAIM_PREFIX.to_owned()),
            Some(value) if value.is_empty() => None,
            Some(value) => {
                let encoded = prefix(&value)
                    .ok_or_else(|| invalid(ENCODED_VARIABLE, &value, prefix_reason))?;
                if claim_prefix.starts_with(&encoded) {
                    return Err(invalid(
                        ENCODED_VARIABLE,
                        &value,
                        "must not begin every plain claim header's name",
                    ));
                }
                Some(encoded)
            }
        };
        Ok(Some(Self {
            url,
            timeout,
            secret,
            protocol,
            request_headers,
            correlation_header,
            claim_prefix,
            encoded_claim_prefix,
        }))
    }
}

const REDACTED: &str = "<redacted>";

fn invalid(name: &'static str, value: &str, reason: &str) -> ConfigError {
    ConfigError::Invalid {
        name,
        value: value.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Whether `text` is an RFC 9110 token.
fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// `text` as a (lower-case) header name, if it is a token.
fn header_name(text: &str) -> Option<HeaderName> {
    is_token(text)
        .then(|| HeaderName::from_bytes(text.to_ascii_lowercase().as_bytes()).ok())
        .flatten()
}

/// A claim header name prefix, lower case.
fn prefix(text: &str) -> Option<String> {
    is_token(text).then(|| text.to_ascii_lowercase())
}

/// The static request headers of `PUSH_ENTRA_VALIDATOR_REQUEST_HEADERS`: a JSON object of
/// strings, else `Name: value` entries separated by newlines or `;`. A refusal names the
/// header when its name is a token, never a value.
fn request_headers(
    text: &str,
    correlation: Option<&HeaderName>,
) -> Result<Vec<(HeaderName, HeaderValue)>, String> {
    let entries: Vec<(String, String)> = if text.starts_with('{') {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        let entries = deserializer
            .deserialize_map(Entries)
            .and_then(|entries| deserializer.end().map(|()| entries))
            .map_err(|_| "is not a JSON object of strings".to_owned())?;
        entries
            .into_iter()
            .map(|(name, value)| match value {
                Value::String(value) => Ok((name, value)),
                _ if is_token(name.trim()) => {
                    Err(format!("header {}: the value is not a string", name.trim()))
                }
                _ => Err("a value is not a string".to_owned()),
            })
            .collect::<Result<_, _>>()?
    } else {
        text.split(['\n', ';'])
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                entry
                    .split_once(':')
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .ok_or_else(|| "an entry is not `Name: value`".to_owned())
            })
            .collect::<Result<_, _>>()?
    };
    if entries.len() > MAX_REQUEST_HEADERS {
        return Err(format!("at most {MAX_REQUEST_HEADERS} headers"));
    }
    let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
    for (name, value) in entries {
        let name = name.trim();
        let header =
            header_name(name).ok_or_else(|| "a header name is not an RFC 9110 token".to_owned())?;
        let refused = |reason: &str| format!("header {name}: {reason}");
        if RESERVED_HEADERS.contains(&header.as_str()) || correlation == Some(&header) {
            return Err(refused("push-auth sets this header itself"));
        }
        if headers.iter().any(|(known, _)| *known == header) {
            return Err(refused("given twice"));
        }
        let value = value.trim();
        if value.is_empty()
            || value.chars().count() > MAX_HEADER_VALUE_CHARS
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ' || byte == b'\t')
        {
            return Err(refused(
                "the value must be 1 to 1024 visible ASCII characters, spaces or tabs",
            ));
        }
        let value = HeaderValue::from_str(value).map_err(|_| refused("the value is not valid"))?;
        headers.push((header, value));
    }
    Ok(headers)
}

/// A JSON object's entries in order, duplicates kept (a map would keep the last).
struct Entries;

impl<'de> Visitor<'de> for Entries {
    type Value = Vec<(String, Value)>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries = Vec::new();
        while let Some(entry) = map.next_entry::<String, Value>()? {
            entries.push(entry);
        }
        Ok(entries)
    }
}

/// The validator endpoint, or whether the value may be shown and why it is refused.
fn validator_url(value: &str) -> Result<String, (bool, &'static str)> {
    const REASON: &str = "must be an https:// URL (http:// only for a loopback host: \
                          127.0.0.1, [::1], localhost), without user info or fragment";
    let url = Url::parse(value).map_err(|_| (!value.contains('@'), REASON))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err((false, REASON));
    }
    let scheme_ok = match url.scheme() {
        "https" => true,
        "http" => is_loopback(&url),
        _ => false,
    };
    if !scheme_ok || url.host_str().is_none() || url.fragment().is_some() {
        return Err((true, REASON));
    }
    Ok(url.to_string())
}

pub(super) fn is_loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"))
}

/// The trimmed content of the secret file.
fn read_secret(path: &str) -> Result<String, &'static str> {
    let content = std::fs::read_to_string(path).map_err(|_| "cannot be read")?;
    let secret = content.trim();
    if secret.is_empty() {
        return Err("the file is empty");
    }
    if secret.len() > MAX_SECRET_BYTES || !secret.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("the secret must be 1 to 1024 visible ASCII characters");
    }
    Ok(secret.to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::*;
    use crate::entra::EntraSettings;

    const TENANT: &str = "33333333-0000-4000-8000-000000000003";
    const API: &str = "11111111-0000-4000-8000-000000000001";
    const URL: &str = "PUSH_ENTRA_VALIDATOR_URL";
    const TIMEOUT: &str = "PUSH_ENTRA_VALIDATOR_TIMEOUT_MS";
    const SECRET: &str = "PUSH_ENTRA_VALIDATOR_SECRET_FILE";

    fn settings(variables: &[(&str, &str)]) -> Result<EntraSettings, ConfigError> {
        let mut all: HashMap<String, String> = [
            ("PUSH_ENTRA_ALLOWED_TENANTS", TENANT),
            ("PUSH_ENTRA_CLIENT_ID", API),
        ]
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
        for (name, value) in variables {
            all.insert(name.to_string(), value.to_string());
        }
        EntraSettings::from_lookup(&|name: &str| all.get(name).cloned())
    }

    fn validator(variables: &[(&str, &str)]) -> Result<Option<ValidatorSettings>, ConfigError> {
        settings(variables).map(|settings| settings.validator)
    }

    fn refused_as(variables: &[(&str, &str)], name: &str) -> ConfigError {
        let error = validator(variables).unwrap_err();
        assert!(
            matches!(&error, ConfigError::Invalid { name: refused, .. } if *refused == name),
            "{error:?}"
        );
        error
    }

    /// A file of the system's temporary directory, removed when dropped.
    struct SecretFile(PathBuf);

    impl SecretFile {
        fn new(content: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("push-validator-{}.secret", Uuid::now_v7()));
            std::fs::write(&path, content).unwrap();
            Self(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for SecretFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn without_a_url_there_is_no_validator() {
        assert_eq!(validator(&[]).unwrap(), None);
        assert_eq!(validator(&[(URL, "  ")]).unwrap(), None);
    }

    #[test]
    fn the_url_is_https_or_loopback_http_with_a_path_and_query_allowed() {
        for (value, expected) in [
            (
                "http://127.0.0.1:9000/validate",
                "http://127.0.0.1:9000/validate",
            ),
            ("http://[::1]:9000/v", "http://[::1]:9000/v"),
            ("http://localhost:9000", "http://localhost:9000/"),
            (
                " https://validator.example/api/check?tier=1 ",
                "https://validator.example/api/check?tier=1",
            ),
        ] {
            let found = validator(&[(URL, value)]).unwrap().unwrap();
            assert_eq!(found.url, expected);
            assert_eq!(found.timeout, Duration::from_millis(2000));
            assert_eq!(found.secret, None);
        }
        for bad in [
            "http://validator.example/",
            "ftp://127.0.0.1/",
            "https://h.example/#f",
            "not a url",
            "/validate",
        ] {
            refused_as(&[(URL, bad)], URL);
        }
        let error = refused_as(&[(URL, "https://u:p@h.example/")], URL);
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("p@") && !shown.contains("u:p"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
        refused_as(&[(URL, "https://user@h.example/")], URL);
    }

    #[test]
    fn an_unparsable_url_with_user_info_is_not_shown() {
        let error = refused_as(&[(URL, "https://user:s3cret@host:99999/")], URL);
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("s3cret"), "{shown}");
        assert!(
            shown.contains(URL) && shown.contains("<redacted>"),
            "{shown}"
        );
        let error = refused_as(&[(URL, "https://host:99999/")], URL);
        assert!(
            format!("{error}").contains("https://host:99999/"),
            "{error}"
        );
    }

    #[test]
    fn the_timeout_is_100_to_10000_milliseconds() {
        let url = (URL, "http://127.0.0.1:9000/validate");
        for (value, millis) in [("100", 100), ("10000", 10000), (" 2500 ", 2500)] {
            let found = validator(&[url, (TIMEOUT, value)]).unwrap().unwrap();
            assert_eq!(found.timeout, Duration::from_millis(millis));
        }
        for bad in ["99", "10001", "1.5", "x", "-1"] {
            refused_as(&[url, (TIMEOUT, bad)], TIMEOUT);
        }
    }

    #[test]
    fn the_secret_is_read_from_a_file_trimmed_and_never_shown() {
        let url = (URL, "http://127.0.0.1:9000/validate");
        let file = SecretFile::new("  s3cret \n");
        let found = validator(&[url, (SECRET, file.path())]).unwrap().unwrap();
        assert_eq!(found.secret.as_deref(), Some("s3cret"));
        let entra = settings(&[url, (SECRET, file.path())]).unwrap();
        let shown = format!("{entra:?}");
        assert!(
            !shown.contains("s3cret") && shown.contains("<redacted>"),
            "{shown}"
        );

        let blank = SecretFile::new(" \n\t ");
        refused_as(&[url, (SECRET, blank.path())], SECRET);
        let spaced = SecretFile::new("two words");
        let error = refused_as(&[url, (SECRET, spaced.path())], SECRET);
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("two words"), "{shown}");
        assert!(matches!(error, ConfigError::Invalid { value, .. } if value == spaced.path()));
        let long = SecretFile::new(&"a".repeat(1025));
        let error = refused_as(&[url, (SECRET, long.path())], SECRET);
        assert!(!format!("{error}").contains(&"a".repeat(100)));
        let missing =
            std::env::temp_dir().join(format!("push-validator-{}.missing", Uuid::now_v7()));
        refused_as(&[url, (SECRET, missing.to_str().unwrap())], SECRET);
    }

    #[test]
    fn the_timeout_and_secret_are_refused_without_a_url() {
        let file = SecretFile::new("s3cret");
        let error = refused_as(&[(TIMEOUT, "2000")], TIMEOUT);
        assert!(format!("{error}").contains(URL), "{error}");
        let error = refused_as(&[(SECRET, file.path())], SECRET);
        assert!(!format!("{error}").contains("s3cret"), "{error}");
        assert_eq!(validator(&[(TIMEOUT, " "), (SECRET, "")]).unwrap(), None);
    }

    const LOOPBACK: (&str, &str) = (URL, "http://127.0.0.1:9000/validate");
    const PROTOCOL: &str = "PUSH_ENTRA_VALIDATOR_PROTOCOL";
    const HEADERS: &str = "PUSH_ENTRA_VALIDATOR_REQUEST_HEADERS";
    const CORRELATION: &str = "PUSH_ENTRA_VALIDATOR_CORRELATION_HEADER";
    const PREFIX: &str = "PUSH_ENTRA_VALIDATOR_CLAIM_HEADER_PREFIX";
    const ENCODED: &str = "PUSH_ENTRA_VALIDATOR_ENCODED_CLAIM_HEADER_PREFIX";

    fn with_url(variables: &[(&str, &str)]) -> ValidatorSettings {
        let mut all = vec![LOOPBACK];
        all.extend_from_slice(variables);
        validator(&all).unwrap().unwrap()
    }

    fn pairs(settings: &ValidatorSettings) -> Vec<(String, String)> {
        settings
            .request_headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_owned()))
            .collect()
    }

    /// The error refusing `variables`, named after `name`, never showing `hidden`.
    fn refused_hiding(variables: &[(&str, &str)], name: &str, hidden: &str) -> String {
        let mut all = vec![LOOPBACK];
        all.extend_from_slice(variables);
        let error = refused_as(&all, name);
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains(hidden), "{shown}");
        assert!(shown.contains(name), "{shown}");
        shown
    }

    #[test]
    fn the_header_settings_have_defaults() {
        let found = with_url(&[]);
        assert_eq!(found.protocol, Protocol::Json);
        assert!(found.request_headers.is_empty());
        assert_eq!(
            found.correlation_header.as_ref().map(|name| name.as_str()),
            Some("x-request-id")
        );
        assert_eq!(found.claim_prefix, "subject-token-claim-");
        assert_eq!(
            found.encoded_claim_prefix.as_deref(),
            Some("subject-token-encoded-claim-")
        );
        assert_eq!(
            found,
            ValidatorSettings::new("http://127.0.0.1:9000/validate")
        );
    }

    #[test]
    fn the_protocol_is_json_or_headers() {
        for (value, protocol) in [
            ("json", Protocol::Json),
            ("headers", Protocol::Headers),
            (" HEADERS ", Protocol::Headers),
            ("Json", Protocol::Json),
            ("", Protocol::Json),
        ] {
            assert_eq!(with_url(&[(PROTOCOL, value)]).protocol, protocol, "{value}");
        }
        for bad in ["grpc", "header", "json,headers"] {
            refused_hiding(&[(PROTOCOL, bad)], PROTOCOL, "\u{0}");
        }
    }

    #[test]
    fn request_headers_parse_from_lines_semicolons_or_a_json_object() {
        let expected = vec![
            ("return-all-subject-token-claims".to_owned(), "1".to_owned()),
            ("x-tier".to_owned(), "gold plus".to_owned()),
        ];
        for value in [
            "Return-All-Subject-Token-Claims: 1\nX-Tier:  gold plus ",
            "Return-All-Subject-Token-Claims: 1\r\n\r\nX-Tier: gold plus\n",
            "Return-All-Subject-Token-Claims:1; X-Tier: gold plus;",
            r#" {"Return-All-Subject-Token-Claims": "1", "X-Tier": " gold plus "} "#,
        ] {
            assert_eq!(pairs(&with_url(&[(HEADERS, value)])), expected, "{value:?}");
        }
        assert!(with_url(&[(HEADERS, " ; \n ")]).request_headers.is_empty());
        // In the JSON form, `;` is part of a value, as the chart renders it.
        assert_eq!(
            pairs(&with_url(&[(
                HEADERS,
                r#"{"Prefer":"a=1; X-Injected: 2"}"#
            )])),
            vec![("prefer".to_owned(), "a=1; X-Injected: 2".to_owned())]
        );
        let shown = format!("{:?}", with_url(&[(HEADERS, "X-Key: s3cret-value")]));
        assert!(
            !shown.contains("s3cret-value") && shown.contains("<redacted>"),
            "{shown}"
        );
    }

    #[test]
    fn request_headers_are_refused_without_echoing_their_values() {
        for reserved in [
            "Authorization",
            "original-uri",
            "ORIGINAL-METHOD",
            "X-Forwarded-For",
            "Host",
            "Content-Length",
            "Content-Type",
            "Accept",
            "Transfer-Encoding",
            "Connection",
            "X-Push-Validator-Secret",
            "X-Request-Id",
            "TE",
            "Upgrade",
            "Expect",
            "Keep-Alive",
            "Proxy-Authorization",
            "Proxy-Connection",
            "Trailer",
        ] {
            let shown = refused_hiding(
                &[(HEADERS, &format!("{reserved}: s3cret-value"))],
                HEADERS,
                "s3cret-value",
            );
            assert!(shown.contains("<redacted>"), "{shown}");
        }
        refused_hiding(
            &[(CORRELATION, "X-Trace"), (HEADERS, "x-trace: s3cret-value")],
            HEADERS,
            "s3cret-value",
        );
        let too_many: String = (0..33).map(|index| format!("X-H{index}: v\n")).collect();
        for bad in [
            "X-A: s3cret-value\nx-a: s3cret-value",
            "X A: s3cret-value",
            "X-A s3cret-value",
            ": s3cret-value",
            "X-A: s3cret-value\u{1}",
            "X-A: s3cret-valueé",
            "X-A:",
            too_many.as_str(),
            r#"{"X-A": 1, "X-B": "s3cret-value"}"#,
            r#"{"X-A": "s3cret-value""#,
            r#"{"X-A": "s3cret-value", "x-a": "s3cret-value"}"#,
            r#"["s3cret-value"]"#,
        ] {
            refused_hiding(&[(HEADERS, bad)], HEADERS, "s3cret-value");
        }
        let long = format!("X-A: {}", "v".repeat(1025));
        refused_hiding(&[(HEADERS, &long)], HEADERS, &"v".repeat(100));
        assert_eq!(
            pairs(&with_url(&[(
                HEADERS,
                &format!("X-A: {}", "v".repeat(1024))
            )]))[0]
                .1
                .len(),
            1024
        );
        let mut all = vec![LOOPBACK];
        all.push((HEADERS, "X-Reserved: s3cret-value"));
        all.push((CORRELATION, ""));
        assert_eq!(validator(&all).unwrap().unwrap().request_headers.len(), 1);
    }

    #[test]
    fn the_correlation_header_may_be_renamed_or_disabled() {
        let renamed = with_url(&[(CORRELATION, " X-Correlation-Id ")]);
        assert_eq!(
            renamed
                .correlation_header
                .as_ref()
                .map(|name| name.as_str()),
            Some("x-correlation-id")
        );
        for empty in ["", "  "] {
            assert_eq!(with_url(&[(CORRELATION, empty)]).correlation_header, None);
        }
        for bad in [
            "X Trace",
            "X-Trace:",
            "Authorization",
            "original-uri",
            "x-push-validator-secret",
        ] {
            refused_hiding(&[(CORRELATION, bad)], CORRELATION, "\u{0}");
        }
    }

    #[test]
    fn the_claim_header_prefixes_are_tokens_and_the_encoded_one_may_be_disabled() {
        let found = with_url(&[(PREFIX, " X-Claim- "), (ENCODED, "X-Claim64-")]);
        assert_eq!(found.claim_prefix, "x-claim-");
        assert_eq!(found.encoded_claim_prefix.as_deref(), Some("x-claim64-"));
        let found = with_url(&[(PREFIX, ""), (ENCODED, " ")]);
        assert_eq!(found.claim_prefix, "subject-token-claim-");
        assert_eq!(found.encoded_claim_prefix, None);
        refused_hiding(&[(PREFIX, "X Claim")], PREFIX, "\u{0}");
        refused_hiding(&[(ENCODED, "X:Claim")], ENCODED, "\u{0}");
        // Every plain claim header would be taken for an encoded one.
        refused_hiding(&[(PREFIX, "X-Claim-"), (ENCODED, "x-")], ENCODED, "\u{0}");
    }

    #[test]
    fn the_header_settings_are_refused_without_a_url() {
        for (name, value) in [
            (PROTOCOL, "headers"),
            (HEADERS, "X-A: s3cret-value"),
            (CORRELATION, "X-Trace"),
            (PREFIX, "X-Claim-"),
            (ENCODED, "X-Claim64-"),
        ] {
            let error = refused_as(&[(name, value)], name);
            let shown = format!("{error} {error:?}");
            assert!(shown.contains(URL), "{shown}");
            assert!(!shown.contains("s3cret-value"), "{shown}");
        }
        assert_eq!(
            validator(&[
                (PROTOCOL, ""),
                (HEADERS, " "),
                (CORRELATION, ""),
                (ENCODED, "")
            ])
            .unwrap(),
            None
        );
    }
}
