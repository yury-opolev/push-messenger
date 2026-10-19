//! Pluggable identity (spec 2026-10-19 pluggable identity, D2 to D5): who vouches for a
//! user ([`Authenticator`]) and who may sign in ([`Admission`]). The built-in Microsoft
//! Entra ID check ([`crate::entra::EntraSignIn`]) is one authenticator; a binary of one's
//! own passes others to [`crate::serve_with`]. Tokens, subjects, names and email
//! addresses are never logged.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};

/// The provider id of the built-in Microsoft Entra ID check.
pub const ENTRA_PROVIDER: &str = "entra";

/// The longest session an authenticator may grant: 30 days, as for Entra ID.
pub const MAX_SESSION_LIMIT: Duration = Duration::from_secs(720 * 3600);

/// The longest issuer the account store keeps, in characters.
pub const MAX_ISSUER_CHARS: usize = 256;
/// The longest subject the account store keeps, in characters.
pub const MAX_SUBJECT_CHARS: usize = 128;

/// A user an [`Authenticator`] vouches for. Accounts are found by `(provider, issuer,
/// subject)`, never by the address. `Debug` shows the provider and the issuer only.
#[derive(Clone, PartialEq)]
pub struct Principal {
    /// The authenticator's provider id ([`is_provider_id`]), e.g. `entra`.
    pub provider: String,
    /// The stable key of the issuer (Entra ID: the tenant id, a lower-case GUID); 1 to 256
    /// characters.
    pub issuer: String,
    /// The user's immutable id at that issuer (Entra ID: `oid`); 1 to 128 characters.
    pub subject: String,
    /// A display-name suggestion for a new account; informational.
    pub name: Option<String>,
    /// The user's address; informational, shown to its owner only, never used to find an
    /// account.
    pub email: Option<String>,
    /// Every verified claim of the token, for an [`Admission`] to decide on.
    pub claims: Map<String, Value>,
}

impl fmt::Debug for Principal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Principal")
            .field("provider", &self.provider)
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

/// Why an authenticator gave no principal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthenticationError {
    /// The token is not acceptable; the reason goes to the security log only.
    #[error("the token is refused ({0})")]
    Rejected(&'static str),
    /// The provider cannot check tokens now (its keys cannot be fetched, …). The text
    /// must not hold the token or its claims.
    #[error("the identity provider is unavailable: {0}")]
    Unavailable(String),
}

/// Checks a token and names its user (spec D3). An implementation must check the
/// signature and its algorithm, the issuer, the audience (this API), the lifetime and that
/// the token is a delegated user token; it must name a stable, immutable subject, and
/// never log the token or its claims.
#[async_trait]
pub trait Authenticator: Send + Sync + fmt::Debug {
    /// The provider id ([`is_provider_id`]): the route is `/v1/auth/{provider}`.
    fn provider(&self) -> &str;
    /// How long a device signed in through it stays signed in: 1 s to 720 h.
    fn max_session(&self) -> Duration;
    /// The user `token` names, if it is acceptable.
    async fn authenticate(&self, token: &str) -> Result<Principal, AuthenticationError>;
    /// Prepares at start-up (fetches keys, …); failures are logged, never fatal.
    async fn warm(&self) {}
}

/// Whether a principal may sign in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    Admit,
    /// Answered 403 `sign_in_not_permitted`; no account is found or created.
    Refuse,
    /// The decision cannot be made now: answered 503, retryable.
    Unavailable,
}

/// Decides, after authentication and before any account is found or created, whether a
/// principal may sign in (spec D4): membership of an Entra group, an app role, an
/// allow-list in one's own store. It sees every verified claim.
#[async_trait]
pub trait Admission: Send + Sync + fmt::Debug {
    async fn admit(&self, principal: &Principal) -> AdmissionDecision;
}

/// A provider id: a lower-case letter, then up to 31 lower-case letters, digits or `-`.
pub fn is_provider_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some('a'..='z'))
        && id.len() <= 32
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'))
}

/// A claim kept for display only: trimmed, 1 to 254 characters, no control characters.
pub(crate) fn informational(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    let fits =
        !value.is_empty() && value.chars().count() <= 254 && !value.chars().any(char::is_control);
    fits.then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_are_short_lower_case_names() {
        let longest = "a".repeat(32);
        for id in ["entra", "acme-id", "a", "x9", longest.as_str()] {
            assert!(is_provider_id(id), "{id}");
        }
        let too_long = "a".repeat(33);
        for id in [
            "",
            "Acme",
            "9a",
            "-a",
            "a b",
            "a_b",
            "a:b",
            "é",
            too_long.as_str(),
        ] {
            assert!(!is_provider_id(id), "{id}");
        }
    }
}
