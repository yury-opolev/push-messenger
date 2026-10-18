//! Datastore credentials for push-messenger (spec 2026-10-18): a password URL, or
//! Microsoft Entra ID access tokens from Azure workload identity or managed identity,
//! refreshed before they expire and handed to PostgreSQL (by `push-store`) and Redis (by
//! [`RedisConnector`]). Tokens are fetched with plain OAuth 2.0 requests over rustls: no
//! Azure SDK and no OpenSSL. Tokens and assertions are never logged.

mod azure;
mod mode;
mod redis_connect;
pub mod startup;
mod token;
mod tokens;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod tests;

pub use azure::{
    AzureConfig, DEFAULT_AUTHORITY_HOST, IMDS_ENDPOINT, ManagedIdentity, WorkloadIdentity,
};
pub use mode::AuthMode;
pub use redis_connect::{
    OpenError, REDIS_SCOPE, RedisConnector, RedisTarget, is_permanent_redis, is_refused,
};
pub use startup::azure_for;
pub use token::{AccessToken, TokenError, TokenSource};
pub use tokens::{Tokens, refresh_at};

/// A configuration variable that is missing or not valid. Services turn it into their
/// own error, redacting the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigProblem {
    Missing(&'static str),
    Invalid { name: &'static str, reason: String },
}

/// Makes ring the process's rustls provider, before any TLS connection: redis, sqlx and
/// reqwest then agree on it. Idempotent.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Whether `error` or one of its causes is a server certificate the client rejected (an
/// unknown issuer, a wrong name, an expired certificate): configuration, not an outage.
/// Other TLS failures (a reset or closed connection during the handshake) are not.
pub fn is_certificate_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause = Some(error);
    while let Some(current) = cause {
        if let Some(tls) = current.downcast_ref::<rustls::Error>() {
            if matches!(
                tls,
                rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented
            ) {
                return true;
            }
        }
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            if let Some(inner) = io.get_ref() {
                if is_certificate_error(inner) {
                    return true;
                }
            }
        }
        cause = current.source();
    }
    false
}
