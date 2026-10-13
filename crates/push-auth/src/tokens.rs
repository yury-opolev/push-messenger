//! Sign-in credentials (spec §5.1): access tokens, signed JWTs (EdDSA) published as a
//! JWKS so the gateway can check them without calling auth-svc, and refresh tokens,
//! random strings stored only as SHA-256 hashes.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// How long an access token is valid.
pub const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);
/// How long a refresh token is valid.
pub const REFRESH_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The claims of an access token: "user `sub` on device `device_id`, email verified".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessClaims {
    pub sub: Uuid,
    pub device_id: Uuid,
    pub email_verified: bool,
    /// Expiry and issue time, in seconds since the Unix epoch.
    pub exp: i64,
    pub iat: i64,
    pub jti: Uuid,
    /// The gateway's URL: the only service that accepts these tokens.
    pub aud: String,
}

/// Why an access token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("the access token has expired")]
    Expired,
    #[error("the access token is not valid")]
    Invalid,
}

/// The PKCS#8 v1 prefix of an Ed25519 private key; the 32-byte seed follows it.
const PKCS8_ED25519_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// Issues and checks access tokens with one Ed25519 key.
#[derive(Clone)]
pub struct AccessTokens {
    encoding: EncodingKey,
    decoding: DecodingKey,
    /// The public key, base64url: the JWK's `x`.
    public_key: String,
    /// The RFC 7638 thumbprint of the public key, base64url.
    kid: String,
    audience: String,
}

impl std::fmt::Debug for AccessTokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessTokens")
            .field("kid", &self.kid)
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

impl AccessTokens {
    /// Tokens signed with the Ed25519 key whose 32-byte private seed is `seed`, for the
    /// audience `audience` (the gateway's URL).
    pub fn from_seed(seed: &[u8], audience: impl Into<String>) -> Result<Self, String> {
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| "the signing key must be a 32-byte Ed25519 seed".to_owned())?;
        let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed)
            .map_err(|_| "the signing key is not a valid Ed25519 seed".to_owned())?;
        let public_key = URL_SAFE_NO_PAD.encode(ring::signature::KeyPair::public_key(&pair));
        let mut der = PKCS8_ED25519_PREFIX.to_vec();
        der.extend_from_slice(&seed);
        let decoding = DecodingKey::from_ed_components(&public_key)
            .map_err(|_| "the public key cannot be loaded".to_owned())?;
        // RFC 7638: SHA-256 over the required members in lexicographic order.
        let thumbprint = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{public_key}"}}"#);
        let kid = URL_SAFE_NO_PAD.encode(Sha256::digest(thumbprint.as_bytes()));
        Ok(Self {
            encoding: EncodingKey::from_ed_der(&der),
            decoding,
            public_key,
            kid,
            audience: audience.into(),
        })
    }

    /// The key id in every token's header and in the JWKS.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// A new access token for `user` on `device`, issued at `now` (seconds since the Unix
    /// epoch), and its claims.
    pub fn issue(&self, user: Uuid, device: Uuid, now: i64) -> (String, AccessClaims) {
        let claims = AccessClaims {
            sub: user,
            device_id: device,
            email_verified: true,
            exp: now + ACCESS_TOKEN_TTL.as_secs() as i64,
            iat: now,
            jti: Uuid::now_v7(),
            aud: self.audience.clone(),
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        let token = jsonwebtoken::encode(&header, &claims, &self.encoding)
            .expect("signing with a valid Ed25519 key cannot fail");
        (token, claims)
    }

    /// The claims of `token` if it is one of ours, for our audience, and not expired
    /// (no leeway).
    pub fn verify(&self, token: &str) -> Result<AccessClaims, TokenError> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[&self.audience]);
        // Keep `exp` required: jsonwebtoken before 10.3 skips the check of a claim with
        // the wrong JSON type unless it is required (CVE-2026-25537). The gateway
        // (Plan 8) must require it too.
        validation.set_required_spec_claims(&["exp", "sub", "aud"]);
        validation.leeway = 0;
        jsonwebtoken::decode::<AccessClaims>(token, &self.decoding, &validation)
            .map(|data| data.claims)
            .map_err(|error| match error.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => TokenError::Expired,
                _ => TokenError::Invalid,
            })
    }

    /// The JSON Web Key Set with the public key, served at `GET /v1/auth/jwks`.
    pub fn jwks(&self) -> serde_json::Value {
        serde_json::json!({
            "keys": [{
                "kty": "OKP",
                "crv": "Ed25519",
                "alg": "EdDSA",
                "use": "sig",
                "kid": self.kid,
                "x": self.public_key,
            }]
        })
    }
}

/// A new refresh token: 32 random bytes, base64url. Only [`refresh_hash`] of it is
/// stored.
pub fn new_refresh_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The SHA-256 hash under which a refresh token is stored.
pub fn refresh_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUDIENCE: &str = "wss://api.example.com/v1/socket";

    fn tokens() -> AccessTokens {
        AccessTokens::from_seed(&[9; 32], AUDIENCE).unwrap()
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[test]
    fn an_issued_token_verifies_with_its_claims() {
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        let (token, claims) = tokens().issue(user, device, now());
        assert_eq!(claims.sub, user);
        assert_eq!(claims.device_id, device);
        assert!(claims.email_verified);
        assert_eq!(claims.exp - claims.iat, 900);
        assert_eq!(claims.aud, AUDIENCE);
        assert_eq!(tokens().verify(&token), Ok(claims));
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!(header.alg, Algorithm::EdDSA);
        assert_eq!(header.kid.as_deref(), Some(tokens().kid()));
    }

    #[test]
    fn expired_foreign_and_other_audience_tokens_are_refused() {
        let user = Uuid::now_v7();
        let (expired, _) = tokens().issue(user, user, now() - 901);
        assert_eq!(tokens().verify(&expired), Err(TokenError::Expired));
        let stranger = AccessTokens::from_seed(&[10; 32], AUDIENCE).unwrap();
        let (foreign, _) = stranger.issue(user, user, now());
        assert_eq!(tokens().verify(&foreign), Err(TokenError::Invalid));
        let elsewhere = AccessTokens::from_seed(&[9; 32], "wss://other/v1/socket").unwrap();
        let (other_audience, _) = elsewhere.issue(user, user, now());
        assert_eq!(tokens().verify(&other_audience), Err(TokenError::Invalid));
        assert_eq!(tokens().verify("not.a.token"), Err(TokenError::Invalid));
    }

    #[test]
    fn the_jwks_publishes_the_public_key_that_verifies_tokens() {
        let jwks = tokens().jwks();
        let key = &jwks["keys"][0];
        assert_eq!(key["kty"], "OKP");
        assert_eq!(key["crv"], "Ed25519");
        assert_eq!(key["alg"], "EdDSA");
        assert_eq!(key["kid"], tokens().kid());
        // What the gateway will do: build the key from the JWK and check a token.
        let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(key.clone()).unwrap();
        let decoding = DecodingKey::from_jwk(&jwk).unwrap();
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[AUDIENCE]);
        let (token, claims) = tokens().issue(Uuid::now_v7(), Uuid::now_v7(), now());
        let decoded = jsonwebtoken::decode::<AccessClaims>(&token, &decoding, &validation);
        assert_eq!(decoded.unwrap().claims, claims);
    }

    #[test]
    fn a_seed_must_have_32_bytes_and_debug_hides_the_key() {
        assert!(AccessTokens::from_seed(&[1; 31], AUDIENCE).is_err());
        let shown = format!("{:?}", tokens());
        assert!(shown.starts_with("AccessTokens { kid: "), "{shown}");
        assert!(!shown.contains("encoding"), "{shown}");
    }

    #[test]
    fn refresh_tokens_are_random_and_hashed_with_sha256() {
        let (first, second) = (new_refresh_token(), new_refresh_token());
        assert_eq!(first.len(), 43);
        assert_ne!(first, second);
        assert_eq!(refresh_hash(&first), refresh_hash(&first));
        assert_ne!(refresh_hash(&first), refresh_hash(&second));
    }
}
