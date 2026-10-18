//! Microsoft Entra ID sign-in (spec 2026-10-19): auth-svc checks an Entra ID access token
//! issued for its API and finds the user by the token's immutable `(tid, oid)`. It never
//! redeems authorization codes and holds no client secret. Tokens, object ids, names and
//! email addresses are never logged.

mod claims;
mod settings;

use uuid::Uuid;

pub use settings::{
    CONSUMERS_TENANT, DEFAULT_AUTHORITY_HOST, DEFAULT_REQUIRED_SCOPE, EntraSettings, Providers,
    Tenants,
};

/// Why a token was refused. Logged by name; the client only ever hears
/// `entra_token_invalid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Malformed,
    Algorithm,
    Tenant,
    Issuer,
    Signature,
    Audience,
    Expired,
    NotYetValid,
    Version,
    AppOnly,
    Scope,
    Role,
    ClientApp,
    Subject,
}

impl Reason {
    /// The name in the security log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::Algorithm => "algorithm",
            Self::Tenant => "tenant",
            Self::Issuer => "issuer",
            Self::Signature => "signature",
            Self::Audience => "audience",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::Version => "version",
            Self::AppOnly => "app_only",
            Self::Scope => "scope",
            Self::Role => "role",
            Self::ClientApp => "client_app",
            Self::Subject => "subject",
        }
    }
}

/// Why no identity came out of a token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntraError {
    #[error("the Entra ID token is refused ({})", .0.as_str())]
    Rejected(Reason),
    /// No usable signing keys: the authority could not be reached or answered nonsense.
    #[error("Microsoft Entra ID's signing keys are not available: {0}")]
    Unavailable(String),
}

/// The user an accepted token names. `Debug` hides everything but the tenant.
#[derive(Clone, PartialEq, Eq)]
pub struct EntraIdentity {
    pub tenant_id: Uuid,
    /// The user's object id: immutable, the same for every app of the tenant.
    pub object_id: Uuid,
    /// The `name` claim: a display-name suggestion for a new account only.
    pub name: Option<String>,
    /// `email`, else `preferred_username`: informational, never used to find an account.
    pub email: Option<String>,
}

impl std::fmt::Debug for EntraIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EntraIdentity")
            .field("tenant_id", &self.tenant_id)
            .field("object_id", &"<redacted>")
            .field("name", &self.name.as_ref().map(|_| "<redacted>"))
            .field("email", &self.email.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}
