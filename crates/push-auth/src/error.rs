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
    #[error("the Microsoft Entra ID token is not accepted")]
    EntraTokenInvalid,
    #[error("Microsoft Entra ID cannot be reached to check the token; try again")]
    EntraUnavailable,
    #[error("the session has reached its maximum age; sign in again")]
    SessionExpired,
    #[error("too many requests; try again in {retry_after_s} s")]
    RateLimited { retry_after_s: u64 },
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
            Self::EntraTokenInvalid => "entra_token_invalid",
            Self::EntraUnavailable => "entra_unavailable",
            Self::SessionExpired => "session_expired",
            Self::RateLimited { .. } => "rate_limited",
            Self::Unavailable => "unavailable",
        }
    }

    /// The HTTP status of the answer.
    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) | Self::LinkInvalid | Self::LinkExpired => 400,
            Self::InvalidCredentials
            | Self::Unauthorized
            | Self::TokenExpired
            | Self::EntraTokenInvalid
            | Self::SessionExpired => 401,
            Self::EmailNotVerified => 403,
            Self::DeviceNotFound | Self::UserNotFound => 404,
            Self::HandleTaken | Self::TooManyDevices => 409,
            Self::RateLimited { .. } => 429,
            Self::Unavailable | Self::EntraUnavailable => 503,
        }
    }

    /// Whether the same request may succeed later without changes.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Unavailable | Self::EntraUnavailable
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
