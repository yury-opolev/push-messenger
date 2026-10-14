//! Access tokens the gateway must refuse, signed with auth-svc's own key so that only the
//! claim under test is wrong: one that expired, one for another audience. The smoke test
//! reads the key from the deployment's Secret; it never prints it or the tokens.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::json;
use uuid::Uuid;

/// The PKCS#8 v1 prefix of an Ed25519 private key; the 32-byte seed follows it.
const PKCS8_ED25519_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// Signs tokens as auth-svc does, with its key and key id.
pub(crate) struct Forger {
    key: EncodingKey,
    kid: String,
}

impl Forger {
    /// `signing_key` is auth-svc's seed, base64 of 32 bytes; the key id is taken from a
    /// token auth-svc issued, so the gateway finds the key.
    pub fn new(signing_key: &str, issued: &str) -> anyhow::Result<Self> {
        let seed = STANDARD
            .decode(signing_key.trim())
            .ok()
            .filter(|seed| seed.len() == 32)
            .context("the signing key is not base64 of 32 bytes")?;
        let mut der = PKCS8_ED25519_PREFIX.to_vec();
        der.extend_from_slice(&seed);
        let kid = jsonwebtoken::decode_header(issued)
            .context("decode an access token's header")?
            .kid
            .context("auth-svc's access token has no kid")?;
        Ok(Self {
            key: EncodingKey::from_ed_der(&der),
            kid,
        })
    }

    /// A token for `user` on `device` that expired `expired_s` seconds ago (negative: that
    /// expires in as many seconds), for `audience`.
    pub fn token(&self, user: Uuid, device: Uuid, expired_s: i64, audience: &str) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() as i64);
        let exp = now - expired_s;
        let claims = json!({
            "sub": user,
            "device_id": device,
            "email_verified": true,
            "exp": exp,
            "iat": exp - 900,
            "jti": Uuid::now_v7(),
            "aud": audience,
        });
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, &claims, &self.key)
            .expect("signing with a valid Ed25519 key cannot fail")
    }
}
