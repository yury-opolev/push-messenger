//! The errors of the auth service, each with the HTTP status and code the API answers.

use crate::rules::Invalid;

/// Why an auth operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("{}: {}", .0.field, .0.reason)]
    Invalid(Invalid),
    #[error("this handle is taken")]
    HandleTaken,
    #[error("wrong email or password")]
    InvalidCredentials,
    #[error("the email address is not verified yet")]
    EmailNotVerified,
    #[error("this link is not valid")]
    LinkInvalid,
    #[error("this link has expired")]
    LinkExpired,
    #[error("sign in again")]
    Unauthorized,
    #[error("the access token has expired")]
    TokenExpired,
    #[error("the account already has the most active devices allowed")]
    TooManyDevices,
    #[error("no such device")]
    DeviceNotFound,
    #[error("no such user")]
    UserNotFound,
    #[error("no such agent")]
    AgentNotFound,
    #[error("the owner already has the most agents allowed")]
    TooManyAgents,
    #[error(
        "agent enrollment is not permitted; a human owner and an unambiguous allowed tenant are required"
    )]
    EnrollmentNotPermitted,
    #[error("the enrollment code is invalid, expired or already used")]
    EnrollmentInvalid,
    #[error("this app identity is already bound to an account")]
    IdentityInUse,
    #[error("the Microsoft Entra ID token is not accepted")]
    EntraTokenInvalid,
    #[error("Microsoft Entra ID cannot be reached to check the token; try again")]
    EntraUnavailable,
    /// A token another identity provider issued is not accepted (spec 2026-10-19
    /// pluggable identity, D9); Entra ID's is `EntraTokenInvalid`.
    #[error("the identity token is not accepted")]
    TokenInvalid,
    #[error("the identity provider cannot check the token now; try again")]
    IdentityUnavailable,
    /// The admission refused an authenticated user (spec D4).
    #[error("this account may not sign in here")]
    SignInNotPermitted,
    #[error("the session has reached its maximum age; sign in again")]
    SessionExpired,
    #[error("too many requests; try again in {retry_after_s} s")]
    RateLimited { retry_after_s: u64 },
    /// message-svc refused a template definition, ref or review input; its message says
    /// which rule failed (spec 2026-10-26 rich message templates).
    #[error("{0}")]
    TemplateInvalid(String),
    #[error("no such template")]
    TemplateNotFound,
    /// The ref exists, the status does not allow the review, the publisher belongs to
    /// another account or too many submissions are pending; message-svc's message says
    /// which.
    #[error("{0}")]
    TemplateConflict(String),
    #[error("the service is unavailable; try again")]
    Unavailable,
}

impl AuthError {
    /// The stable, machine-readable code clients switch on.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::HandleTaken => "handle_taken",
            Self::InvalidCredentials => "invalid_credentials",
            Self::EmailNotVerified => "email_not_verified",
            Self::LinkInvalid => "link_invalid",
            Self::LinkExpired => "link_expired",
            Self::Unauthorized => "unauthorized",
            Self::TokenExpired => "token_expired",
            Self::TooManyDevices => "too_many_devices",
            Self::DeviceNotFound => "device_not_found",
            Self::UserNotFound => "user_not_found",
            Self::AgentNotFound => "agent_not_found",
            Self::TooManyAgents => "too_many_agents",
            Self::EnrollmentNotPermitted => "enrollment_not_permitted",
            Self::EnrollmentInvalid => "enrollment_invalid",
            Self::IdentityInUse => "identity_in_use",
            Self::EntraTokenInvalid => "entra_token_invalid",
            Self::EntraUnavailable => "entra_unavailable",
            Self::TokenInvalid => "token_invalid",
            Self::IdentityUnavailable => "identity_unavailable",
            Self::SignInNotPermitted => "sign_in_not_permitted",
            Self::SessionExpired => "session_expired",
            Self::RateLimited { .. } => "rate_limited",
            Self::TemplateInvalid(_) => "invalid",
            Self::TemplateNotFound => "not_found",
            Self::TemplateConflict(_) => "conflict",
            Self::Unavailable => "unavailable",
        }
    }

    /// The HTTP status of the answer.
    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_)
            | Self::LinkInvalid
            | Self::LinkExpired
            | Self::EnrollmentInvalid
            | Self::TemplateInvalid(_) => 400,
            Self::InvalidCredentials
            | Self::Unauthorized
            | Self::TokenExpired
            | Self::EntraTokenInvalid
            | Self::TokenInvalid
            | Self::SessionExpired => 401,
            Self::EmailNotVerified | Self::SignInNotPermitted | Self::EnrollmentNotPermitted => 403,
            Self::DeviceNotFound
            | Self::UserNotFound
            | Self::AgentNotFound
            | Self::TemplateNotFound => 404,
            Self::HandleTaken
            | Self::TooManyDevices
            | Self::TooManyAgents
            | Self::IdentityInUse
            | Self::TemplateConflict(_) => 409,
            Self::RateLimited { .. } => 429,
            Self::Unavailable | Self::EntraUnavailable | Self::IdentityUnavailable => 503,
        }
    }

    /// Whether the same request may succeed later without changes.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. }
                | Self::Unavailable
                | Self::EntraUnavailable
                | Self::IdentityUnavailable
        )
    }
}

impl From<Invalid> for AuthError {
    fn from(invalid: Invalid) -> Self {
        Self::Invalid(invalid)
    }
}

impl From<sqlx::Error> for AuthError {
    /// A database failure, logged here without parameters (they may hold an email).
    fn from(error: sqlx::Error) -> Self {
        let code = error
            .as_database_error()
            .and_then(|database| database.code())
            .map(|code| code.into_owned());
        tracing::error!(%error, code = ?code, "database error");
        Self::Unavailable
    }
}
