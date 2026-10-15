//! The SDK's one error type. Every error has a stable, machine-readable [`code`] that
//! bindings pass on as a string, and says whether the same call may succeed later
//! ([`is_retryable`]).
//!
//! [`code`]: SdkError::code
//! [`is_retryable`]: SdkError::is_retryable

use push_proto::v1::ErrorCode;

/// Why an SDK call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SdkError {
    /// The input breaks a rule the server shares (spec §8.3): nothing was stored or sent.
    #[error("invalid: {0}")]
    Invalid(String),
    /// No device is signed in on this client: sign in (again).
    #[error("not signed in")]
    NotSignedIn,
    /// The server revoked this device: it does not reconnect until the app connects
    /// again.
    #[error("this device was removed")]
    DeviceRemoved,
    /// The same app data was opened elsewhere (another window, a copy restored on another
    /// machine) and that copy took over the connection. The device is still signed in;
    /// it does not reconnect until the app connects again.
    #[error("this device was opened elsewhere")]
    OpenedElsewhere,
    /// No connection to the server came up within the request timeout.
    #[error("offline: no connection to the server")]
    Offline,
    /// The connection dropped before the answer came; the request may or may not have
    /// been carried out.
    #[error("the connection dropped before the answer")]
    Disconnected,
    /// The server refused the request. `code` is the gateway's error code
    /// (`not_contact`, `invite_not_found`, …) or auth-svc's (`invalid_credentials`,
    /// `email_not_verified`, …).
    #[error("{code}: {message}")]
    Server {
        code: String,
        message: String,
        retryable: bool,
        /// Rate limited: when the same request may succeed.
        retry_after_s: Option<u32>,
    },
    /// The server could not be reached, or answered something unreadable.
    #[error("network: {0}")]
    Network(String),
    /// The local database failed.
    #[error("local storage: {0}")]
    Storage(String),
    /// The local database is encrypted and the key given is wrong, or none was given
    /// (or a key was given for a database that is not encrypted). Nothing was changed.
    #[error("the database key is wrong or missing")]
    WrongKey,
    /// A key was given, but the SQLite linked into the program cannot encrypt (another
    /// crate brought a plain SQLite): the key would be ignored and the data written in
    /// plaintext, so nothing was opened or changed.
    #[error(
        "database encryption is unavailable: the linked SQLite is not SQLite3 Multiple Ciphers"
    )]
    EncryptionUnavailable,
    /// The app's secret store failed.
    #[error("secret store: {0}")]
    Secrets(String),
    /// The client was closed.
    #[error("the client is closed")]
    Closed,
}

impl SdkError {
    /// The stable code of the error: the server's code for [`SdkError::Server`], else
    /// the variant's name in snake case.
    pub fn code(&self) -> &str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::NotSignedIn => "not_signed_in",
            Self::DeviceRemoved => "device_removed",
            Self::OpenedElsewhere => "opened_elsewhere",
            Self::Offline => "offline",
            Self::Disconnected => "disconnected",
            Self::Server { code, .. } => code,
            Self::Network(_) => "network",
            Self::Storage(_) => "storage",
            Self::WrongKey => "wrong_key",
            Self::EncryptionUnavailable => "encryption_unavailable",
            Self::Secrets(_) => "secrets",
            Self::Closed => "closed",
        }
    }

    /// Whether the same call may succeed later, unchanged.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Offline | Self::Disconnected | Self::Network(_) => true,
            Self::Server { retryable, .. } => *retryable,
            _ => false,
        }
    }

    /// A gateway `Error` frame.
    pub(crate) fn from_frame(error: &push_proto::v1::Error) -> Self {
        Self::Server {
            code: error_code_name(error.code()),
            message: error.message.clone(),
            retryable: error.retryable,
            retry_after_s: error.retry_after_s,
        }
    }
}

impl From<rusqlite::Error> for SdkError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

/// `ERROR_CODE_NOT_CONTACT` → `not_contact`.
pub(crate) fn error_code_name(code: ErrorCode) -> String {
    code.as_str_name()
        .trim_start_matches("ERROR_CODE_")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_errors_keep_the_server_code_and_say_whether_to_retry() {
        let frame = push_proto::v1::Error {
            code: ErrorCode::RateLimited.into(),
            message: "slow down".to_owned(),
            retryable: true,
            retry_after_s: Some(7),
            ..Default::default()
        };
        let error = SdkError::from_frame(&frame);
        assert_eq!(error.code(), "rate_limited");
        assert!(error.is_retryable());
        assert_eq!(
            error,
            SdkError::Server {
                code: "rate_limited".to_owned(),
                message: "slow down".to_owned(),
                retryable: true,
                retry_after_s: Some(7),
            }
        );
        assert_eq!(error_code_name(ErrorCode::NotContact), "not_contact");
    }

    #[test]
    fn local_errors_have_their_own_codes() {
        assert_eq!(SdkError::Offline.code(), "offline");
        assert!(SdkError::Offline.is_retryable());
        assert_eq!(SdkError::Invalid("empty".to_owned()).code(), "invalid");
        assert!(!SdkError::DeviceRemoved.is_retryable());
        assert_eq!(SdkError::OpenedElsewhere.code(), "opened_elsewhere");
        assert_eq!(SdkError::WrongKey.code(), "wrong_key");
        assert!(!SdkError::WrongKey.is_retryable());
        assert!(!SdkError::NotSignedIn.is_retryable());
    }
}
