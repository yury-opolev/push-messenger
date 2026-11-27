//! The header protocol's claims (design H6, H7): response headers named
//! `<prefix><claim>` carry a claim as text, `<encoded prefix><claim>` as base64 of UTF-8
//! text. Limits are checked before anything is decoded.

use base64::Engine;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Map, Value};

/// The most claim headers in one answer.
pub(super) const MAX_CLAIM_HEADERS: usize = 64;
/// The longest claim header value, in bytes.
pub(super) const MAX_CLAIM_VALUE_BYTES: usize = 8 * 1024;
/// The most bytes of claim header names and values in one answer.
pub(super) const MAX_CLAIM_BYTES: usize = 32 * 1024;

/// Why an answer's claim headers give no claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClaimError {
    /// A limit is exceeded: the validator is misconfigured. Says which.
    TooLarge(&'static str),
    /// A claim header cannot be read or typed.
    Malformed,
}

/// The claims of the claim headers among `headers`: `prefix` and `encoded_prefix` are
/// lower case. A claim with an encoded header is read from its encoded headers only.
pub(super) fn claims(
    headers: &HeaderMap,
    prefix: &str,
    encoded_prefix: Option<&str>,
) -> Result<Map<String, Value>, ClaimError> {
    // Every limit first: nothing is decoded for an answer over one.
    let mut found: Vec<(&str, bool, &HeaderValue)> = Vec::new();
    let mut total = 0;
    for (name, value) in headers {
        let name = name.as_str();
        let (claim, encoded) = match encoded_prefix.and_then(|encoded| name.strip_prefix(encoded)) {
            Some(claim) => (claim, true),
            None => match name.strip_prefix(prefix) {
                Some(claim) => (claim, false),
                None => continue,
            },
        };
        if found.len() == MAX_CLAIM_HEADERS {
            return Err(ClaimError::TooLarge("more than 64 claim headers"));
        }
        if value.len() > MAX_CLAIM_VALUE_BYTES {
            return Err(ClaimError::TooLarge("a claim header value over 8 KiB"));
        }
        total += name.len() + value.len();
        if total > MAX_CLAIM_BYTES {
            return Err(ClaimError::TooLarge("claim headers over 32 KiB"));
        }
        found.push((claim, encoded, value));
    }
    let mut claims = Map::new();
    for (index, (claim, ..)) in found.iter().enumerate() {
        if found[..index].iter().any(|(seen, ..)| seen == claim) {
            continue;
        }
        if claim.is_empty() {
            return Err(ClaimError::Malformed);
        }
        let encoded = found
            .iter()
            .any(|(other, encoded, _)| other == claim && *encoded);
        let values = found
            .iter()
            .filter(|(other, is_encoded, _)| other == claim && *is_encoded == encoded)
            .map(|(_, _, value)| {
                if encoded {
                    decoded(value)
                } else {
                    plain(value)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        claims.insert((*claim).to_owned(), typed(claim, &values)?);
    }
    Ok(claims)
}

/// Base64 that may or may not be padded.
const LENIENT: GeneralPurposeConfig =
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
const STANDARD_LENIENT: GeneralPurpose = GeneralPurpose::new(&alphabet::STANDARD, LENIENT);
const URL_SAFE_LENIENT: GeneralPurpose = GeneralPurpose::new(&alphabet::URL_SAFE, LENIENT);

/// A plain claim header's trimmed text: visible ASCII, spaces and tabs.
fn plain(value: &HeaderValue) -> Result<String, ClaimError> {
    value
        .to_str()
        .map(|text| text.trim().to_owned())
        .map_err(|_| ClaimError::Malformed)
}

/// An encoded claim header's trimmed text: base64 (standard or URL-safe) of UTF-8.
fn decoded(value: &HeaderValue) -> Result<String, ClaimError> {
    let text = value.as_bytes().trim_ascii();
    let bytes = STANDARD_LENIENT
        .decode(text)
        .or_else(|_| URL_SAFE_LENIENT.decode(text))
        .map_err(|_| ClaimError::Malformed)?;
    String::from_utf8(bytes)
        .map(|text| text.trim().to_owned())
        .map_err(|_| ClaimError::Malformed)
}

/// A claim's JSON value from the texts of its headers (design H7).
fn typed(claim: &str, values: &[String]) -> Result<Value, ClaimError> {
    match (claim, values) {
        ("exp" | "nbf" | "iat", [value]) => integer(value),
        ("exp" | "nbf" | "iat", _) => Err(ClaimError::Malformed),
        ("scp", _) => Ok(Value::String(items(values)?.join(" "))),
        ("roles", _) => Ok(strings(items(values)?)),
        ("aud", _) => {
            let mut audiences = items(values)?;
            Ok(match audiences.len() {
                1 => Value::String(audiences.remove(0)),
                _ => strings(audiences),
            })
        }
        (_, [value]) if value.starts_with('[') => Ok(strings(items(values)?)),
        (_, [value]) => Ok(Value::String(value.clone())),
        _ => Err(ClaimError::Malformed),
    }
}

/// An optionally signed decimal integer.
fn integer(text: &str) -> Result<Value, ClaimError> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ClaimError::Malformed);
    }
    if let Ok(number) = text.parse::<i64>() {
        return Ok(Value::from(number));
    }
    if text.starts_with('-') {
        return Err(ClaimError::Malformed);
    }
    digits
        .parse::<u64>()
        .map(Value::from)
        .map_err(|_| ClaimError::Malformed)
}

/// The items of a multi-valued claim: each value a JSON array of strings, else a list
/// split on commas and white space; empty items dropped.
fn items(values: &[String]) -> Result<Vec<String>, ClaimError> {
    let mut items = Vec::new();
    for value in values {
        if value.starts_with('[') {
            let array: Vec<String> =
                serde_json::from_str(value).map_err(|_| ClaimError::Malformed)?;
            items.extend(
                array
                    .into_iter()
                    .map(|item| item.trim().to_owned())
                    .filter(|item| !item.is_empty()),
            );
        } else {
            items.extend(
                value
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|item| !item.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    Ok(items)
}

fn strings(items: Vec<String>) -> Value {
    Value::Array(items.into_iter().map(Value::String).collect())
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use reqwest::header::{HeaderName, HeaderValue};
    use serde_json::json;

    use super::*;

    const PREFIX: &str = "subject-token-claim-";
    const ENCODED: &str = "subject-token-encoded-claim-";

    fn map(headers: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn decoded(headers: &[(&str, &str)]) -> Result<Map<String, Value>, ClaimError> {
        claims(&map(headers), PREFIX, Some(ENCODED))
    }

    fn value(headers: &[(&str, &str)], claim: &str) -> Value {
        decoded(headers).unwrap()[claim].clone()
    }

    #[test]
    fn plain_claims_are_trimmed_strings_and_numbers_under_lower_case_names() {
        let found = decoded(&[
            (
                "Subject-Token-Claim-TID",
                " 33333333-0000-4000-8000-000000000003 ",
            ),
            ("subject-token-claim-exp", "1700000000"),
            ("subject-token-claim-nbf", "-5"),
            ("subject-token-claim-iat", "+7"),
            ("subject-token-claim-azpacr", "0"),
            ("subject-token-claim-Custom_Thing", "a b"),
            ("content-type", "text/plain"),
            ("x-other", "ignored"),
        ])
        .unwrap();
        assert_eq!(
            Value::Object(found),
            json!({
                "tid": "33333333-0000-4000-8000-000000000003",
                "exp": 1_700_000_000,
                "nbf": -5,
                "iat": 7,
                "azpacr": "0",
                "custom_thing": "a b",
            })
        );
        assert_eq!(
            value(
                &[("subject-token-claim-exp", "18446744073709551615")],
                "exp"
            ),
            json!(u64::MAX)
        );
    }

    #[test]
    fn encoded_claims_are_base64_of_utf8_and_win_over_plain_ones() {
        let name = "Zoë Ångström 测试";
        for encoded in [
            STANDARD.encode(name),
            STANDARD.encode(name).trim_end_matches('=').to_owned(),
            URL_SAFE_NO_PAD.encode(name),
        ] {
            assert_eq!(
                value(&[("subject-token-encoded-claim-name", &encoded)], "name"),
                name
            );
        }
        // The standard and URL-safe alphabets differ in `+` / `-`.
        let symbols = "\u{fb}\u{ef}\u{be}";
        assert!(STANDARD.encode(symbols).contains('+'));
        for encoded in [STANDARD.encode(symbols), URL_SAFE_NO_PAD.encode(symbols)] {
            assert_eq!(
                value(&[("subject-token-encoded-claim-x", &encoded)], "x"),
                symbols
            );
        }
        assert_eq!(
            value(
                &[
                    ("subject-token-claim-name", "Plain"),
                    (
                        "subject-token-encoded-claim-name",
                        &STANDARD.encode("Encoded")
                    ),
                ],
                "name"
            ),
            "Encoded"
        );
        for bad in ["not base64!", "a", &STANDARD.encode([0xff, 0xfe, 0x41])] {
            assert_eq!(
                decoded(&[("subject-token-encoded-claim-name", bad)]),
                Err(ClaimError::Malformed),
                "{bad}"
            );
        }
        // Disabled, the encoded prefix is no claim header at all.
        let found = claims(
            &map(&[("subject-token-encoded-claim-name", "Wm9l")]),
            PREFIX,
            None,
        )
        .unwrap();
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn multi_valued_claims_read_json_arrays_lists_and_repeated_headers() {
        let roles = json!(["Agent.Run", "Messages.Send"]);
        for headers in [
            vec![(
                "subject-token-claim-roles",
                r#"["Agent.Run", "Messages.Send"]"#,
            )],
            vec![("subject-token-claim-roles", "Agent.Run,Messages.Send")],
            vec![("subject-token-claim-roles", " Agent.Run ,, Messages.Send ")],
            vec![("subject-token-claim-roles", "Agent.Run Messages.Send")],
            vec![
                ("subject-token-claim-roles", "Agent.Run"),
                ("subject-token-claim-roles", "Messages.Send"),
            ],
            vec![
                ("subject-token-claim-roles", r#"["Agent.Run"]"#),
                ("subject-token-claim-roles", "Messages.Send"),
            ],
        ] {
            assert_eq!(value(&headers, "roles"), roles, "{headers:?}");
        }
        assert_eq!(
            value(&[("subject-token-claim-roles", "One")], "roles"),
            json!(["One"])
        );
        assert_eq!(
            value(&[("subject-token-claim-aud", "api-1")], "aud"),
            json!("api-1")
        );
        assert_eq!(
            value(&[("subject-token-claim-aud", r#"["api-1"]"#)], "aud"),
            json!("api-1")
        );
        assert_eq!(
            value(&[("subject-token-claim-aud", "api-1, api-2")], "aud"),
            json!(["api-1", "api-2"])
        );
        assert_eq!(
            value(
                &[("subject-token-claim-groups", r#"["g1","g2"]"#)],
                "groups"
            ),
            json!(["g1", "g2"])
        );
        for scopes in [
            r#"["access_as_user","Other.Read"]"#,
            "access_as_user,Other.Read",
            "access_as_user  Other.Read",
        ] {
            assert_eq!(
                value(&[("subject-token-claim-scp", scopes)], "scp"),
                json!("access_as_user Other.Read"),
                "{scopes}"
            );
        }
        for bad in [r#"["a", 1]"#, r#"["a""#, r#"[1, 2]"#] {
            assert_eq!(
                decoded(&[("subject-token-claim-roles", bad)]),
                Err(ClaimError::Malformed),
                "{bad}"
            );
        }
    }

    #[test]
    fn single_valued_claims_appear_once_and_times_are_integers() {
        for headers in [
            vec![
                ("subject-token-claim-oid", "a"),
                ("subject-token-claim-oid", "b"),
            ],
            vec![
                ("subject-token-encoded-claim-oid", "YQ"),
                ("subject-token-encoded-claim-oid", "Yg"),
            ],
            vec![
                ("subject-token-claim-exp", "1"),
                ("subject-token-claim-exp", "2"),
            ],
            vec![("subject-token-claim-exp", "1.5")],
            vec![("subject-token-claim-exp", "tomorrow")],
            vec![("subject-token-claim-exp", "")],
            vec![("subject-token-claim-nbf", "1e3")],
            vec![("subject-token-claim-iat", "99999999999999999999999")],
            vec![("subject-token-claim-exp", "[1]")],
            vec![("subject-token-claim-", "nameless")],
            vec![("subject-token-encoded-claim-", "YQ")],
        ] {
            assert_eq!(decoded(&headers), Err(ClaimError::Malformed), "{headers:?}");
        }
    }

    #[test]
    fn plain_values_are_visible_ascii() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("subject-token-claim-name"),
            HeaderValue::from_bytes("Zo\u{eb}".as_bytes()).unwrap(),
        );
        assert_eq!(
            claims(&headers, PREFIX, Some(ENCODED)),
            Err(ClaimError::Malformed)
        );
    }

    #[test]
    fn limits_are_checked_before_anything_is_decoded() {
        let names: Vec<String> = (0..65)
            .map(|index| format!("subject-token-claim-c{index}"))
            .collect();
        let mut many: Vec<(&str, &str)> = names.iter().map(|name| (name.as_str(), "v")).collect();
        assert!(matches!(decoded(&many), Err(ClaimError::TooLarge(_))));
        many.pop();
        assert_eq!(decoded(&many).unwrap().len(), 64);
        // Other headers do not count.
        many.push(("x-other", "v"));
        assert!(decoded(&many).is_ok());

        let large = "v".repeat(MAX_CLAIM_VALUE_BYTES + 1);
        assert!(matches!(
            decoded(&[
                ("subject-token-claim-name", &large),
                ("subject-token-encoded-claim-x", "!")
            ]),
            Err(ClaimError::TooLarge(_))
        ));
        let fits = "v".repeat(MAX_CLAIM_VALUE_BYTES);
        assert!(decoded(&[("subject-token-claim-name", &fits)]).is_ok());
        let total: Vec<String> = (0..5)
            .map(|index| format!("subject-token-claim-big{index}"))
            .collect();
        let five: Vec<(&str, &str)> = total
            .iter()
            .map(|name| (name.as_str(), fits.as_str()))
            .collect();
        assert!(matches!(decoded(&five), Err(ClaimError::TooLarge(_))));
        assert!(decoded(&five[..3]).is_ok());
    }
}
