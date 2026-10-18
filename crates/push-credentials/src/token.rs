//! An access token and where tokens come from.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use tokio::time::Instant;

/// A bearer token and when it stops working. Its `Debug` never shows the token.
#[derive(Clone)]
pub struct AccessToken {
    secret: Arc<str>,
    issued_at: Instant,
    expires_at: Instant,
}

impl AccessToken {
    /// A token received now that lives `lifetime`.
    pub fn new(secret: impl Into<Arc<str>>, lifetime: Duration) -> Self {
        let issued_at = Instant::now();
        Self {
            secret: secret.into(),
            issued_at,
            expires_at: issued_at + lifetime,
        }
    }

    /// The token itself: the password of the connection. Never log it.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    pub fn issued_at(&self) -> Instant {
        self.issued_at
    }

    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    /// The `oid` claim of a JWT access token (the principal's object id), read without
    /// checking the signature: the token came straight from the issuer, and the claim
    /// only names the Redis user it is presented for.
    pub fn object_id(&self) -> Option<String> {
        let payload = self.secret.split('.').nth(1)?;
        let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
        let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        claims.get("oid")?.as_str().map(str::to_owned)
    }
}

impl PartialEq for AccessToken {
    fn eq(&self, other: &Self) -> bool {
        self.secret == other.secret
    }
}

impl Eq for AccessToken {}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessToken")
            .field("secret", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Why no token could be had. Never carries a token, an assertion or the issuer's
/// free-text description (which may echo request data).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("the Azure identity is not configured: {0}")]
    Config(String),
    #[error("cannot read the federated token file: {0}")]
    File(String),
    #[error("the token endpoint could not be reached: {0}")]
    Unreachable(String),
    #[error("the token endpoint refused the request: HTTP {status}{}", code.as_deref().map(|code| format!(" ({code})")).unwrap_or_default())]
    Refused { status: u16, code: Option<String> },
    #[error("the token endpoint answered something that is not a token")]
    Malformed,
}

/// Something that issues access tokens for a scope.
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn fetch(&self, scope: &str) -> Result<AccessToken, TokenError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: &str) -> String {
        format!(
            "{}.{}.signature",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#),
            URL_SAFE_NO_PAD.encode(claims)
        )
    }

    #[test]
    fn the_object_id_comes_from_the_payload_and_the_debug_output_hides_the_token() {
        let token = AccessToken::new(jwt(r#"{"oid":"5f1c","tid":"t"}"#), Duration::from_secs(60));
        assert_eq!(token.object_id().as_deref(), Some("5f1c"));
        assert!(!format!("{token:?}").contains(token.secret()));
        assert_eq!(
            AccessToken::new("opaque", Duration::from_secs(1)).object_id(),
            None
        );
        assert_eq!(
            AccessToken::new(jwt("{}"), Duration::from_secs(1)).object_id(),
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_expires_after_its_lifetime() {
        let token = AccessToken::new("t", Duration::from_secs(10));
        assert!(!token.is_expired());
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(token.is_expired());
    }

    #[test]
    fn errors_name_the_status_and_code_only() {
        let refused = TokenError::Refused {
            status: 401,
            code: Some("invalid_client".to_owned()),
        };
        assert_eq!(
            refused.to_string(),
            "the token endpoint refused the request: HTTP 401 (invalid_client)"
        );
    }
}
