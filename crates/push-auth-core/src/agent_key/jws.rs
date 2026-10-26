//! The agent-key assertion (spec 2026-10-26 agent-key provider, D1 and D3): a compact
//! ES256 JWS whose protected header carries the agent's public P-256 key. Pure: the
//! caller gives the time and keeps the replay cache.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The longest assertion accepted, in bytes.
pub(crate) const MAX_TOKEN_BYTES: usize = 4096;
/// The longest lifetime, `exp - iat`, in seconds.
pub(crate) const MAX_LIFETIME_SECONDS: i64 = 300;
/// The clock skew allowed on `iat` and `exp`, in seconds.
pub(crate) const CLOCK_SKEW_SECONDS: i64 = 60;
/// The required `typ` header.
pub(crate) const TOKEN_TYPE: &str = "push-agent+jwt";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    alg: String,
    typ: String,
    jwk: Jwk,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Jwk {
    kty: String,
    crv: String,
    x: String,
    y: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claims {
    aud: String,
    iat: i64,
    exp: i64,
    jti: String,
}

/// A verified assertion: whose key signed it and its single-use id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assertion {
    /// The RFC 7638 SHA-256 thumbprint of the signing key, base64url (43 characters).
    pub thumbprint: String,
    pub jti: String,
    pub exp: i64,
    pub claims: Map<String, Value>,
}

/// Checks `token` at `now` (seconds since the epoch) for `audience`; the reason of a
/// refusal is a short constant for the security log.
pub(crate) fn verify(token: &str, audience: &str, now: i64) -> Result<Assertion, &'static str> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err("size");
    }
    let mut parts = token.split('.');
    let (Some(header_part), Some(payload_part), Some(signature_part), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("format");
    };
    let header_bytes = decode(header_part).ok_or("format")?;
    let payload = decode(payload_part).ok_or("format")?;
    let header: Header = parse_object(&header_bytes, Some("jwk")).ok_or("header")?;
    if header.alg != "ES256" {
        return Err("algorithm");
    }
    if header.typ != TOKEN_TYPE {
        return Err("type");
    }
    let Jwk { kty, crv, x, y } = header.jwk;
    if kty != "EC" || crv != "P-256" {
        return Err("key");
    }
    let coordinate = |value: &str| decode(value).filter(|bytes| bytes.len() == 32);
    let (x, y) = (coordinate(&x).ok_or("key")?, coordinate(&y).ok_or("key")?);
    let signature = decode(signature_part)
        .filter(|bytes| bytes.len() == 64)
        .ok_or("signature")?;
    let mut point = Vec::with_capacity(65);
    point.push(0x04);
    point.extend_from_slice(&x);
    point.extend_from_slice(&y);
    // ring refuses a point off the curve or at infinity, and r or s outside 1..n.
    let signed = &token[..header_part.len() + 1 + payload_part.len()];
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &point)
        .verify(signed.as_bytes(), &signature)
        .map_err(|_| "signature")?;
    // The object checks refuse the array form; the struct refuses duplicate and unknown
    // members; the map keeps the claims for admission.
    let claims: Claims = parse_object(&payload, None).ok_or("claims")?;
    let map: Map<String, Value> = serde_json::from_slice(&payload).map_err(|_| "claims")?;
    if !is_jti(&claims.jti) {
        return Err("claims");
    }
    if claims.aud != audience {
        return Err("audience");
    }
    if claims
        .exp
        .checked_sub(claims.iat)
        .is_none_or(|lifetime| lifetime <= 0 || lifetime > MAX_LIFETIME_SECONDS)
    {
        return Err("lifetime");
    }
    if claims.iat > now.saturating_add(CLOCK_SKEW_SECONDS) {
        return Err("not_yet_valid");
    }
    if claims.exp <= now.saturating_sub(CLOCK_SKEW_SECONDS) {
        return Err("expired");
    }
    Ok(Assertion {
        thumbprint: thumbprint(&x, &y),
        jti: claims.jti,
        exp: claims.exp,
        claims: map,
    })
}

/// The RFC 7638 thumbprint of the P-256 key `(x, y)`: SHA-256 over the required members
/// in lexicographic order, without white space, base64url without padding.
pub(crate) fn thumbprint(x: &[u8], y: &[u8]) -> String {
    let canonical = format!(
        r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
        URL_SAFE_NO_PAD.encode(x),
        URL_SAFE_NO_PAD.encode(y)
    );
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

/// Parses `bytes` as `T` only if they are a JSON object whose `nested` member, when
/// present, is an object too: serde's derived structs also accept arrays.
fn parse_object<T: DeserializeOwned>(bytes: &[u8], nested: Option<&str>) -> Option<T> {
    let Value::Object(object) = serde_json::from_slice(bytes).ok()? else {
        return None;
    };
    if nested
        .and_then(|member| object.get(member))
        .is_some_and(|value| !value.is_object())
    {
        return None;
    }
    serde_json::from_slice(bytes).ok()
}

/// Strict base64url: no padding, no other alphabet, canonical trailing bits.
fn decode(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(value).ok()
}

/// 16 to 128 characters of `A-Z a-z 0-9 - _ .`.
fn is_jti(jti: &str) -> bool {
    (16..=128).contains(&jti.len())
        && jti
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_key::testing::TestKey;
    use serde_json::json;

    const AUD: &str = "https://api.example.com";
    const NOW: i64 = 1_800_000_000;
    const JTI: &str = "abcdefghijklmnop0123";

    fn claims() -> Value {
        json!({"aud": AUD, "iat": NOW, "exp": NOW + 120, "jti": JTI})
    }

    fn with(mut value: Value, member: &str, content: Value) -> Value {
        value[member] = content;
        value
    }

    fn without(mut value: Value, member: &str) -> Value {
        value.as_object_mut().unwrap().remove(member);
        value
    }

    fn encode(bytes: impl AsRef<[u8]>) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    /// The token's three parts, with the one at `index` replaced by `part`.
    fn replace_part(token: &str, index: usize, part: &str) -> String {
        let mut parts: Vec<&str> = token.split('.').collect();
        parts[index] = part;
        parts.join(".")
    }

    #[test]
    fn thumbprint_matches_an_independent_vector() {
        // The P-256 generator point (SEC 2); the vector was computed with Python hashlib.
        let x = hex("6B17D1F2E12C4247F8BCE6E563A440F277037D812DEB33A0F4A13945D898C296");
        let y = hex("4FE342E2FE1A7F9B8EE7EB4A7C0F9E162BCE33576B315ECECBB6406837BF51F5");
        assert_eq!(encode(&x), "axfR8uEsQkf4vOblY6RA8ncDfYEt6zOg9KE5RdiYwpY");
        assert_eq!(encode(&y), "T-NC4v4af5uO5-tKfA-eFivOM1drMV7Oy7ZAaDe_UfU");
        assert_eq!(
            thumbprint(&x, &y),
            "xx0BcA-wMohw8atYDJOe6peGModklG2wRHBlXHMvl0M"
        );
    }

    #[test]
    fn a_valid_assertion_names_its_key() {
        let key = TestKey::generate();
        let assertion = verify(&key.sign(&key.header(), &claims()), AUD, NOW).unwrap();
        assert_eq!(assertion.thumbprint, key.thumbprint());
        assert_eq!(assertion.thumbprint.len(), 43);
        assert_eq!(assertion.jti, JTI);
        assert_eq!(assertion.exp, NOW + 120);
        assert_eq!(assertion.claims["aud"], AUD);

        for (iat, exp) in [
            (NOW - 100, NOW + 200),
            (NOW + 60, NOW + 120),
            (NOW - 200, NOW - 59),
        ] {
            let claims = with(with(claims(), "iat", json!(iat)), "exp", json!(exp));
            let token = key.sign(&key.header(), &claims);
            assert!(verify(&token, AUD, NOW).is_ok(), "iat {iat}, exp {exp}");
        }
    }

    /// `a - b` for big-endian 32-byte integers, `a >= b`.
    fn subtract(a: &[u8], b: &[u8]) -> Vec<u8> {
        let mut difference = vec![0u8; 32];
        let mut borrow = 0i16;
        for at in (0..32).rev() {
            let mut digit = i16::from(a[at]) - i16::from(b[at]) - borrow;
            borrow = i16::from(digit < 0);
            if digit < 0 {
                digit += 256;
            }
            difference[at] = u8::try_from(digit).unwrap();
        }
        assert_eq!(borrow, 0);
        difference
    }

    #[test]
    fn a_high_s_signature_verifies() {
        // D1 accepts both s and n - s.
        let order = hex("FFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551");
        let key = TestKey::generate();
        let token = key.sign(&key.header(), &claims());
        let signature = URL_SAFE_NO_PAD
            .decode(token.split('.').nth(2).unwrap())
            .unwrap();
        let mut flipped = signature[..32].to_vec();
        flipped.extend(subtract(&order, &signature[32..]));
        assert_ne!(flipped, signature);
        let flipped = replace_part(&token, 2, &encode(flipped));
        assert_eq!(
            verify(&flipped, AUD, NOW).unwrap().thumbprint,
            key.thumbprint()
        );
    }

    #[test]
    fn refusals_have_their_reasons() {
        let key = TestKey::generate();
        let other = TestKey::generate();
        let header = key.header();
        let jwk = key.jwk();
        let sign = |claims: &Value| key.sign(&header, claims);
        let sign_header = |header: &Value| key.sign(header, &claims());
        let sign_jwk = |jwk: Value| sign_header(&with(header.clone(), "jwk", jwk));
        let times =
            |iat: i64, exp: i64| sign(&with(with(claims(), "iat", json!(iat)), "exp", json!(exp)));
        let valid = sign(&claims());
        let unsigned = format!(
            "{}.{}.",
            encode(with(header.clone(), "alg", json!("none")).to_string()),
            encode(claims().to_string())
        );
        let claims_text =
            |aud: &str| format!(r#"{aud}"iat":{NOW},"exp":{},"jti":"{JTI}"}}"#, NOW + 120);
        let jwk_text = jwk.to_string();
        // The last of x's 43 characters carries 2 unused bits, zero when canonical.
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let x = jwk["x"].as_str().unwrap();
        let last = ALPHABET
            .iter()
            .position(|&c| c == x.as_bytes()[42])
            .unwrap();
        assert_eq!(last & 3, 0);
        let loose_x = format!("{}{}", &x[..42], char::from(ALPHABET[last | 1]));
        let array_header = format!(
            r#"["ES256","{TOKEN_TYPE}",["EC","P-256",{},{}]]"#,
            jwk["x"], jwk["y"]
        );
        let array_jwk = with(
            header.clone(),
            "jwk",
            json!(["EC", "P-256", jwk["x"], jwk["y"]]),
        );
        let array_claims = json!([AUD, NOW, NOW + 120, JTI]);

        let cases: Vec<(&str, String, &str)> =
            vec![
            (
                "foreign audience",
                sign(&with(claims(), "aud", json!("https://evil.example"))),
                "audience",
            ),
            ("audience array", sign(&with(claims(), "aud", json!([AUD]))), "claims"),
            ("too long", times(NOW, NOW + 301), "lifetime"),
            ("no lifetime", times(NOW, NOW), "lifetime"),
            ("overflowing lifetime", times(i64::MIN, i64::MAX), "lifetime"),
            ("expired", times(NOW - 200, NOW - 60), "expired"),
            ("not yet valid", times(NOW + 61, NOW + 120), "not_yet_valid"),
            ("alg none", sign_header(&with(header.clone(), "alg", json!("none"))), "algorithm"),
            ("alg none, no signature", unsigned, "algorithm"),
            ("alg HS256", sign_header(&with(header.clone(), "alg", json!("HS256"))), "algorithm"),
            ("alg ES384", sign_header(&with(header.clone(), "alg", json!("ES384"))), "algorithm"),
            ("typ JWT", sign_header(&with(header.clone(), "typ", json!("JWT"))), "type"),
            ("no jwk", sign_header(&without(header.clone(), "jwk")), "header"),
            ("kid", sign_header(&with(header.clone(), "kid", json!("one"))), "header"),
            ("crit", sign_header(&with(header.clone(), "crit", json!(["exp"]))), "header"),
            (
                "jku",
                sign_header(&with(header.clone(), "jku", json!("https://evil.example/jwks"))),
                "header",
            ),
            (
                "x5u",
                sign_header(&with(header.clone(), "x5u", json!("https://evil.example/x5u"))),
                "header",
            ),
            ("x5c", sign_header(&with(header.clone(), "x5c", json!(["AAAA"]))), "header"),
            ("x5t", sign_header(&with(header.clone(), "x5t", json!("AAAA"))), "header"),
            ("b64", sign_header(&with(header.clone(), "b64", json!(false))), "header"),
            ("zip", sign_header(&with(header.clone(), "zip", json!("DEF"))), "header"),
            ("alg RS256", sign_header(&with(header.clone(), "alg", json!("RS256"))), "algorithm"),
            ("alg EdDSA", sign_header(&with(header.clone(), "alg", json!("EdDSA"))), "algorithm"),
            ("array header", key.sign_raw(&array_header, &claims().to_string()), "header"),
            ("array jwk", sign_header(&array_jwk), "header"),
            ("array claims", sign(&array_claims), "claims"),
            ("private jwk", sign_jwk(with(jwk.clone(), "d", json!("d".repeat(43)))), "header"),
            ("jwk kid", sign_jwk(with(jwk.clone(), "kid", json!("one"))), "header"),
            ("jwk use", sign_jwk(with(jwk.clone(), "use", json!("sig"))), "header"),
            ("jwk key_ops", sign_jwk(with(jwk.clone(), "key_ops", json!(["verify"]))), "header"),
            ("jwk alg", sign_jwk(with(jwk.clone(), "alg", json!("ES256"))), "header"),
            ("P-384", sign_jwk(with(jwk.clone(), "crv", json!("P-384"))), "key"),
            ("OKP", sign_jwk(with(jwk.clone(), "kty", json!("OKP"))), "key"),
            ("short x", sign_jwk(with(jwk.clone(), "x", json!(encode([0u8; 31])))), "key"),
            ("short y", sign_jwk(with(jwk.clone(), "y", json!(encode([0u8; 31])))), "key"),
            ("non-canonical x", sign_jwk(with(jwk.clone(), "x", json!(loose_x))), "key"),
            (
                // ring reports a point off the curve as a generic signature error.
                "off-curve point",
                sign_jwk(with(
                    with(jwk.clone(), "x", json!(encode([1u8; 32]))),
                    "y",
                    json!(encode([2u8; 32])),
                )),
                "signature",
            ),
            ("DER signature", replace_part(&valid, 2, &encode([0x30u8; 70])), "signature"),
            ("another key", other.sign(&header, &claims()), "signature"),
            (
                "tampered payload",
                replace_part(
                    &valid,
                    1,
                    &encode(with(claims(), "exp", json!(NOW + 300)).to_string()),
                ),
                "signature",
            ),
            ("extra claim", sign(&with(claims(), "sub", json!("x"))), "claims"),
            (
                "duplicate claim",
                key.sign_raw(
                    &header.to_string(),
                    &claims_text(&format!(r#"{{"aud":"{AUD}","aud":"{AUD}","#)),
                ),
                "claims",
            ),
            (
                "duplicate header member",
                key.sign_raw(
                    &format!(
                        r#"{{"alg":"ES256","alg":"ES256","typ":"{TOKEN_TYPE}","jwk":{jwk_text}}}"#
                    ),
                    &claims().to_string(),
                ),
                "header",
            ),
            ("short jti", sign(&with(claims(), "jti", json!("short"))), "claims"),
            (
                "jti with a space",
                sign(&with(claims(), "jti", json!("abcdefgh ijklmnop0123"))),
                "claims",
            ),
            ("fractional iat", sign(&with(claims(), "iat", json!(1.5))), "claims"),
            ("string iat", sign(&with(claims(), "iat", json!(NOW.to_string()))), "claims"),
            ("no jti", sign(&without(claims(), "jti")), "claims"),
            (
                "malformed payload, wrong signature",
                replace_part(&other.sign(&header, &claims()), 1, "a*b"),
                "format",
            ),
            ("oversized", "a".repeat(MAX_TOKEN_BYTES + 1), "size"),
            ("two parts", "a.b".to_owned(), "format"),
            ("four parts", format!("{valid}.x"), "format"),
            (
                "padded header",
                replace_part(&valid, 0, &format!("{}=", valid.split('.').next().unwrap())),
                "format",
            ),
        ];
        // The unchanged tokens pass, so each refusal is down to its one change.
        assert!(verify(&valid, AUD, NOW).is_ok());
        let raw_header = format!(r#"{{"alg":"ES256","typ":"{TOKEN_TYPE}","jwk":{jwk_text}}}"#);
        let raw_claims = claims_text(&format!(r#"{{"aud":"{AUD}","#));
        assert!(verify(&key.sign_raw(&raw_header, &raw_claims), AUD, NOW).is_ok());
        for (name, token, reason) in cases {
            assert_eq!(verify(&token, AUD, NOW), Err(reason), "{name}");
        }
    }
}
