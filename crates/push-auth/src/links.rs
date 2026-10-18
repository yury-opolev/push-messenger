//! Signed, stateless tokens: the email links and the registration token (spec §5.1).
//!
//! A token is `payload.signature`: the payload is base64url JSON claims, the signature
//! an HMAC-SHA256 over the payload under the service's link key. Nothing is stored:
//! a verification link binds the email it was sent to, and every token binds a
//! fingerprint of the account's password hash when it was made (or, for an Entra ID
//! registration token, the identity that signed in, spec 2026-10-19 D7), so changing the password
//! (a reset, or a new signup over an unverified account) makes every earlier token
//! worthless, and a used reset link with it. Every token has a purpose, so one kind can
//! never stand in for another.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// What a signed token allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    /// Verify the email address of an account (spec §5.2).
    Verify,
    /// Set a new password (spec §5.4).
    Reset,
    /// Register one device for an account (spec §5.3).
    Register,
}

/// The claims of a signed token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkClaims {
    /// The account.
    pub sub: Uuid,
    pub purpose: Purpose,
    /// Expiry, in seconds since the Unix epoch.
    pub exp: i64,
    /// Verify links: the address the link was sent to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// [`password_stamp`] of the account's password hash when the token was made. Every
    /// email link and every email/password registration token carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pw: Option<String>,
    /// Entra ID registration tokens: the `auth.external_identity` row that signed in. A
    /// registration token carries either `pw` or `xid` with `sx`, never both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xid: Option<Uuid>,
    /// Entra ID registration tokens: when the device's session ends, in seconds since the
    /// Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sx: Option<i64>,
}

/// Why a token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("the token is malformed")]
    Malformed,
    #[error("the token's signature does not match")]
    Forged,
    #[error("the token is for another purpose")]
    WrongPurpose,
    #[error("the token has expired")]
    Expired,
}

/// Domain separation: the MAC covers this prefix and the payload.
const CONTEXT: &[u8] = b"push-link-v1.";

/// Signs and checks tokens with one HMAC key.
#[derive(Clone)]
pub struct LinkSigner {
    key: Vec<u8>,
}

impl std::fmt::Debug for LinkSigner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LinkSigner(<redacted>)")
    }
}

impl LinkSigner {
    /// Fewest bytes of key: as long as the SHA-256 output.
    pub const MIN_KEY_BYTES: usize = 32;

    /// A signer with `key`, which must have at least [`Self::MIN_KEY_BYTES`] bytes.
    pub fn new(key: Vec<u8>) -> Result<Self, String> {
        if key.len() < Self::MIN_KEY_BYTES {
            return Err(format!(
                "the link key must have at least {} bytes",
                Self::MIN_KEY_BYTES
            ));
        }
        Ok(Self { key })
    }

    fn mac(&self, payload: &str) -> Hmac<Sha256> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts keys of any length");
        mac.update(CONTEXT);
        mac.update(payload.as_bytes());
        mac
    }

    /// The token for `claims`.
    pub fn sign(&self, claims: &LinkClaims) -> String {
        let json = serde_json::to_vec(claims).expect("claims serialise");
        let payload = URL_SAFE_NO_PAD.encode(json);
        let signature = URL_SAFE_NO_PAD.encode(self.mac(&payload).finalize().into_bytes());
        format!("{payload}.{signature}")
    }

    /// The claims of `token` if its signature matches, it is for `purpose` and it has not
    /// expired at `now` (seconds since the Unix epoch). The signature is checked first,
    /// in constant time, so a forged token reveals nothing about its claims.
    pub fn verify(&self, token: &str, purpose: Purpose, now: i64) -> Result<LinkClaims, LinkError> {
        let (payload, signature) = token.split_once('.').ok_or(LinkError::Malformed)?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| LinkError::Malformed)?;
        self.mac(payload)
            .verify_slice(&signature)
            .map_err(|_| LinkError::Forged)?;
        let json = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| LinkError::Malformed)?;
        let claims: LinkClaims = serde_json::from_slice(&json).map_err(|_| LinkError::Malformed)?;
        if claims.purpose != purpose {
            return Err(LinkError::WrongPurpose);
        }
        if now >= claims.exp {
            return Err(LinkError::Expired);
        }
        Ok(claims)
    }
}

/// A short fingerprint of a password hash for signed tokens: it changes whenever the
/// password does, which makes every token made before worthless.
pub fn password_stamp(password_hash: &str) -> String {
    let digest = Sha256::digest(password_hash.as_bytes());
    URL_SAFE_NO_PAD.encode(&digest[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> LinkSigner {
        LinkSigner::new(vec![7; 32]).unwrap()
    }

    fn verify_claims() -> LinkClaims {
        LinkClaims {
            sub: Uuid::now_v7(),
            purpose: Purpose::Verify,
            exp: 1_000,
            email: Some("alice@example.com".to_owned()),
            pw: None,
            xid: None,
            sx: None,
        }
    }

    #[test]
    fn a_signed_token_verifies_until_it_expires() {
        let claims = verify_claims();
        let token = signer().sign(&claims);
        assert_eq!(signer().verify(&token, Purpose::Verify, 999), Ok(claims));
        assert_eq!(
            signer().verify(&token, Purpose::Verify, 1_000),
            Err(LinkError::Expired)
        );
    }

    #[test]
    fn a_token_for_one_purpose_is_refused_for_another() {
        let token = signer().sign(&verify_claims());
        assert_eq!(
            signer().verify(&token, Purpose::Register, 0),
            Err(LinkError::WrongPurpose)
        );
        assert_eq!(
            signer().verify(&token, Purpose::Reset, 0),
            Err(LinkError::WrongPurpose)
        );
    }

    #[test]
    fn a_changed_payload_or_another_key_is_a_forgery() {
        let token = signer().sign(&verify_claims());
        let (_, signature) = token.split_once('.').unwrap();
        let other = LinkClaims {
            email: Some("mallory@example.com".to_owned()),
            ..verify_claims()
        };
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&other).unwrap());
        assert_eq!(
            signer().verify(&format!("{payload}.{signature}"), Purpose::Verify, 0),
            Err(LinkError::Forged)
        );
        let stranger = LinkSigner::new(vec![8; 32]).unwrap();
        assert_eq!(
            stranger.verify(&token, Purpose::Verify, 0),
            Err(LinkError::Forged)
        );
    }

    #[test]
    fn garbage_is_malformed() {
        for token in ["", "abc", "abc.***", "a.b.c"] {
            assert_eq!(
                signer().verify(token, Purpose::Verify, 0),
                Err(LinkError::Malformed),
                "{token:?}"
            );
        }
    }

    #[test]
    fn short_keys_are_refused_and_never_shown() {
        assert!(LinkSigner::new(vec![1; 31]).is_err());
        assert_eq!(format!("{:?}", signer()), "LinkSigner(<redacted>)");
    }

    #[test]
    fn the_password_stamp_changes_with_the_hash() {
        assert_eq!(password_stamp("hash-1"), password_stamp("hash-1"));
        assert_ne!(password_stamp("hash-1"), password_stamp("hash-2"));
        assert_eq!(password_stamp("hash-1").len(), 22);
    }
}
