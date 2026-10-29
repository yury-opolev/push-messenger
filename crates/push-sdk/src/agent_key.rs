//! Agent-key sign-in (docs/agent-key-sign-in.md): an owned agent proves itself with a
//! P-256 key the app keeps. The SDK builds the claims of every assertion; the app's
//! [`AgentSigner`] signs them, so the private key never reaches the SDK.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// How long the assertions the SDK asks for live: two minutes (auth-svc allows at most 300 s).
pub const AGENT_ASSERTION_LIFETIME_S: i64 = 120;

/// The claims of one agent-key assertion. The SDK builds them, with a new `jti` every time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentClaims {
    /// auth-svc's agent-key audience.
    pub aud: String,
    /// Issued at, in seconds since the epoch.
    pub iat: i64,
    /// Expires at, in seconds since the epoch.
    pub exp: i64,
    /// The assertion's single-use id.
    pub jti: String,
}

impl AgentClaims {
    /// `iat = now_s`, `exp = now_s + AGENT_ASSERTION_LIFETIME_S`, `jti` = 32 lowercase hex
    /// characters from a new v4 UUID.
    pub fn new(audience: &str, now_s: i64) -> Self {
        Self {
            aud: audience.to_owned(),
            iat: now_s,
            exp: now_s + AGENT_ASSERTION_LIFETIME_S,
            jti: Uuid::new_v4().simple().to_string(),
        }
    }

    /// The claims as compact JSON: `{"aud":…,"iat":…,"exp":…,"jti":…}`.
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"aud":{},"iat":{},"exp":{},"jti":{}}}"#,
            json_string(&self.aud),
            self.iat,
            self.exp,
            json_string(&self.jti)
        )
    }
}

fn json_string(value: &str) -> String {
    serde_json::Value::from(value).to_string()
}

/// A failure of the app's signer: `code` is the key provider's code (e.g. `NTE_BAD_KEYSET`,
/// `0x80090016`), never key material.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct SignerError {
    pub code: String,
    pub message: String,
}

/// Signs agent-key assertions with a P-256 private key the app keeps (ideally
/// non-exportable, in OS key storage). Called on a blocking thread.
pub trait AgentSigner: Send + Sync {
    /// The compact JWS (`header.payload.signature`) for `claims`, as
    /// docs/agent-key-sign-in.md specifies.
    fn sign(&self, claims: &AgentClaims) -> Result<String, SignerError>;
}

/// A P-256 public key: the 32-byte coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentPublicKey {
    pub x: [u8; 32],
    pub y: [u8; 32],
}

impl AgentPublicKey {
    /// The RFC 7638 thumbprint: base64url SHA-256 of
    /// `{"crv":"P-256","kty":"EC","x":…,"y":…}`. auth-svc knows the agent by it.
    pub fn thumbprint(&self) -> String {
        let canonical = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            URL_SAFE_NO_PAD.encode(self.x),
            URL_SAFE_NO_PAD.encode(self.y)
        );
        URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
    }
}

/// Builds a compact JWS for signers: the header
/// `{"alg":"ES256","typ":"push-agent+jwt","jwk":{kty,crv,x,y}}`, the claims, and `sign`
/// called over the ASCII signing input, returning the 64-byte `r || s`.
pub fn assemble_assertion(
    key: &AgentPublicKey,
    claims: &AgentClaims,
    sign: impl FnOnce(&[u8]) -> Result<[u8; 64], SignerError>,
) -> Result<String, SignerError> {
    let header = format!(
        r#"{{"alg":"ES256","typ":"push-agent+jwt","jwk":{{"kty":"EC","crv":"P-256","x":"{}","y":"{}"}}}}"#,
        URL_SAFE_NO_PAD.encode(key.x),
        URL_SAFE_NO_PAD.encode(key.y)
    );
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(claims.to_json())
    );
    let signature = sign(signing_input.as_bytes())?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn decode(part: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    fn coordinate(text: &str) -> [u8; 32] {
        URL_SAFE_NO_PAD.decode(text).unwrap().try_into().unwrap()
    }

    /// The P-256 base point, as in docs/agent-key-sign-in.md.
    fn base_point() -> AgentPublicKey {
        AgentPublicKey {
            x: coordinate("axfR8uEsQkf4vOblY6RA8ncDfYEt6zOg9KE5RdiYwpY"),
            y: coordinate("T-NC4v4af5uO5-tKfA-eFivOM1drMV7Oy7ZAaDe_UfU"),
        }
    }

    #[test]
    fn claims_live_two_minutes_with_a_new_jti_each_time() {
        let claims = AgentClaims::new("https://push.test", 1_793_000_000);
        assert_eq!(
            (claims.iat, claims.exp),
            (1_793_000_000, 1_793_000_000 + AGENT_ASSERTION_LIFETIME_S)
        );
        assert_eq!(claims.jti.len(), 32);
        assert!(
            claims
                .jti
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
        let other = AgentClaims::new("https://push.test", 1_793_000_000);
        assert_ne!(claims.jti, other.jti);
    }

    #[test]
    fn claims_serialize_compactly_in_order() {
        let claims = AgentClaims {
            aud: "https://push.test".to_owned(),
            iat: 1_793_000_000,
            exp: 1_793_000_120,
            jti: "0f8e4c2b9a7d4e1f8c3b6a5d2e9f1a7c".to_owned(),
        };
        assert_eq!(
            claims.to_json(),
            r#"{"aud":"https://push.test","iat":1793000000,"exp":1793000120,"jti":"0f8e4c2b9a7d4e1f8c3b6a5d2e9f1a7c"}"#
        );
    }

    #[test]
    fn the_thumbprint_of_the_base_point_matches_the_guide() {
        assert_eq!(
            base_point().thumbprint(),
            "xx0BcA-wMohw8atYDJOe6peGModklG2wRHBlXHMvl0M"
        );
    }

    #[test]
    fn an_assembled_assertion_has_the_header_claims_and_signature() {
        let claims = AgentClaims::new("https://push.test", 1_793_000_000);
        let mut signed = Vec::new();
        let token = assemble_assertion(&base_point(), &claims, |input| {
            signed = input.to_vec();
            Ok([7; 64])
        })
        .unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(signed, format!("{}.{}", parts[0], parts[1]).into_bytes());
        assert_eq!(
            decode(parts[0]),
            json!({"alg": "ES256", "typ": "push-agent+jwt", "jwk": {
                "kty": "EC", "crv": "P-256",
                "x": "axfR8uEsQkf4vOblY6RA8ncDfYEt6zOg9KE5RdiYwpY",
                "y": "T-NC4v4af5uO5-tKfA-eFivOM1drMV7Oy7ZAaDe_UfU"}})
        );
        assert_eq!(
            URL_SAFE_NO_PAD.decode(parts[1]).unwrap(),
            claims.to_json().into_bytes()
        );
        assert_eq!(URL_SAFE_NO_PAD.decode(parts[2]).unwrap(), vec![7; 64]);

        let refused = assemble_assertion(&base_point(), &claims, |_| {
            Err(SignerError {
                code: "NTE_BAD_KEYSET".to_owned(),
                message: "sign failed".to_owned(),
            })
        })
        .unwrap_err();
        assert_eq!(refused.to_string(), "NTE_BAD_KEYSET: sign failed");
    }
}
