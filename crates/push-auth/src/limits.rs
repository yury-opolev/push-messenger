//! Rate limits (spec §5.6): per account and per IP, on signup, login, verification resend,
//! password reset, opening email links and handle lookup, and on Entra ID sign-in. Accounts are never locked; a
//! request over a limit is refused with `rate_limited` and a retry delay.
//!
//! Counters are fixed windows in Redis (`INCR`, then `EXPIRE … NX`), keyed by a hash of
//! the subject, so no email address or IP is stored in Redis. An IPv6 client is counted
//! by its /64 prefix, which one subscriber usually holds whole; an IPv4 address (also
//! mapped into IPv6) whole. Login counts per account **and** IP, so nobody can lock a user
//! out from another address, under a higher ceiling per account against guessing from
//! many addresses.
//!
//! When Redis does not answer within 500 ms the request is refused with a retryable
//! `unavailable` and a warning logged (fail closed): without counters nothing would slow
//! down signup floods, email bombing or guessing. Refresh is not limited, so signed-in
//! devices keep working.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::error::AuthError;

/// What is being limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Signup,
    Login,
    ResendVerification,
    ForgotPassword,
    /// Opening a verification or reset link.
    OpenLink,
    HandleLookup,
    /// `POST /v1/auth/entra`, counted per IP before the token is checked.
    EntraSignIn,
    /// `POST /v1/auth/entra`, counted per Entra ID identity once the token is accepted.
    EntraAccount,
    /// `PATCH /v1/users/me` with a handle: per IP and per user.
    HandleChange,
    /// The admin API with the admin token, per IP.
    Admin,
    /// Calls of the admin API without the right token, per IP; counted apart from
    /// [`Action::Admin`], so they never lock the token's holder out.
    AdminAuthFailure,
}

/// Whom a limit counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The client's IP (an IPv6 /64).
    Ip,
    /// The account: an email address or a user id.
    Account,
    /// The account from one IP.
    AccountAndIp,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::Ip => "ip",
            Self::Account => "account",
            Self::AccountAndIp => "account_ip",
        }
    }
}

const HOUR: u64 = 3600;
const QUARTER: u64 = 900;
const DAY: u64 = 24 * HOUR;

/// At most `max` requests per `seconds`.
const fn per(max: u32, seconds: u64) -> Limit {
    Limit {
        max,
        window: Duration::from_secs(seconds),
    }
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Signup => "signup",
            Self::Login => "login",
            Self::ResendVerification => "resend",
            Self::ForgotPassword => "forgot",
            Self::OpenLink => "link",
            Self::HandleLookup => "lookup",
            Self::EntraSignIn => "entra",
            Self::EntraAccount => "entra_account",
            Self::HandleChange => "handle_change",
            Self::Admin => "admin",
            Self::AdminAuthFailure => "admin_auth_failure",
        }
    }

    /// The limits of the action, in the order they are counted. A refused request is not
    /// counted by the limits after the one that refused it, so a guesser stopped per
    /// account and IP does not use up the account's ceiling.
    pub fn limits(self) -> &'static [(Scope, Limit)] {
        const SIGNUP: &[(Scope, Limit)] =
            &[(Scope::Ip, per(20, HOUR)), (Scope::Account, per(5, HOUR))];
        const LOGIN: &[(Scope, Limit)] = &[
            (Scope::Ip, per(60, QUARTER)),
            (Scope::AccountAndIp, per(10, QUARTER)),
            (Scope::Account, per(100, HOUR)),
        ];
        const EMAIL: &[(Scope, Limit)] =
            &[(Scope::Ip, per(20, HOUR)), (Scope::Account, per(3, HOUR))];
        const LINK: &[(Scope, Limit)] = &[(Scope::Ip, per(60, HOUR))];
        // Spec §5.5: 20 lookups per hour per user.
        const LOOKUP: &[(Scope, Limit)] =
            &[(Scope::Ip, per(120, HOUR)), (Scope::Account, per(20, HOUR))];
        // Like login per IP; per identity once the token is known to be good.
        const ENTRA: &[(Scope, Limit)] = &[(Scope::Ip, per(60, QUARTER))];
        const ENTRA_ACCOUNT: &[(Scope, Limit)] = &[(Scope::Account, per(20, QUARTER))];
        // Handles are public names: a few changes a day, so nobody cycles through them.
        const HANDLE_CHANGE: &[(Scope, Limit)] =
            &[(Scope::Ip, per(30, HOUR)), (Scope::Account, per(5, DAY))];
        const ADMIN: &[(Scope, Limit)] = &[(Scope::Ip, per(60, QUARTER))];
        const ADMIN_AUTH_FAILURE: &[(Scope, Limit)] = &[(Scope::Ip, per(20, QUARTER))];
        match self {
            Self::Signup => SIGNUP,
            Self::Login => LOGIN,
            Self::ResendVerification | Self::ForgotPassword => EMAIL,
            Self::OpenLink => LINK,
            Self::HandleLookup => LOOKUP,
            Self::EntraSignIn => ENTRA,
            Self::EntraAccount => ENTRA_ACCOUNT,
            Self::HandleChange => HANDLE_CHANGE,
            Self::Admin => ADMIN,
            Self::AdminAuthFailure => ADMIN_AUTH_FAILURE,
        }
    }
}

/// At most `max` requests per `window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max: u32,
    pub window: Duration,
}

/// The answer of a counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allowed,
    Limited { retry_after_s: u64 },
}

/// A store of fixed-window counters.
#[async_trait]
pub trait Counter: Send + Sync {
    /// Counts one request against `key` and says whether it is within `limit`.
    async fn hit(&self, key: &str, limit: Limit) -> Result<Outcome, String>;
}

/// The rate limits of the API over one counter store.
#[derive(Clone)]
pub struct Limits {
    counter: Arc<dyn Counter>,
}

impl std::fmt::Debug for Limits {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Limits")
    }
}

impl Limits {
    pub fn new(counter: Arc<dyn Counter>) -> Self {
        Self { counter }
    }

    /// Counts a request for `action` from `ip` about `account` (an email address or a
    /// user id); `RateLimited` if any of its limits is exceeded. A failing counter
    /// refuses the request with the retryable `Unavailable`.
    pub async fn check(
        &self,
        action: Action,
        ip: Option<IpAddr>,
        account: Option<&str>,
    ) -> Result<(), AuthError> {
        let ip = ip.map(ip_subject);
        let account = account.map(|account| account.trim().to_lowercase());
        for &(scope, limit) in action.limits() {
            let subject = match (scope, &ip, &account) {
                (Scope::Ip, Some(ip), _) => ip.clone(),
                (Scope::Account, _, Some(account)) => account.clone(),
                // Length-prefixed, so no account and IP pair can pose as another.
                (Scope::AccountAndIp, Some(ip), Some(account)) => {
                    format!("{}:{account}{ip}", account.len())
                }
                _ => continue,
            };
            match self.counter.hit(&key(action, scope, &subject), limit).await {
                Ok(Outcome::Allowed) => {}
                Ok(Outcome::Limited { retry_after_s }) => {
                    return Err(AuthError::RateLimited { retry_after_s });
                }
                Err(error) => {
                    tracing::warn!(%error, action = action.name(), "rate limit not checked; refusing the request");
                    return Err(AuthError::Unavailable);
                }
            }
        }
        Ok(())
    }
}

/// What an IP is counted as: an IPv4 address whole, also when mapped into IPv6; an IPv6
/// address by its /64 prefix, since one subscriber usually gets a whole /64.
fn ip_subject(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => {
            let segments = address.segments();
            format!(
                "{:x}:{:x}:{:x}:{:x}::/64",
                segments[0], segments[1], segments[2], segments[3]
            )
        }
    }
}

/// `rl:{action}:{scope}:{hash}`: the subject is hashed, never stored.
fn key(action: Action, scope: Scope, subject: &str) -> String {
    let digest = Sha256::digest(subject.as_bytes());
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("rl:{}:{}:{hex}", action.name(), scope.name())
}

/// How long a Redis call may take before the request is refused without it.
pub const REDIS_TIMEOUT: Duration = Duration::from_millis(500);

/// Counters in Redis.
#[derive(Clone)]
pub struct RedisCounter {
    connection: ConnectionManager,
    shutdown: CancellationToken,
}

impl RedisCounter {
    /// Connects to Redis at `url`; the connection reconnects by itself. Calls give up
    /// when `shutdown` is cancelled.
    pub async fn connect(url: &str, shutdown: CancellationToken) -> redis::RedisResult<Self> {
        Self::with_connector(&push_credentials::RedisConnector::password(url)?, shutdown).await
    }

    /// Counters through `connector` (password or Entra ID).
    pub async fn with_connector(
        connector: &push_credentials::RedisConnector,
        shutdown: CancellationToken,
    ) -> redis::RedisResult<Self> {
        let connection = connector
            .connection_manager(redis::aio::ConnectionManagerConfig::new())
            .await?;
        Ok(Self {
            connection,
            shutdown,
        })
    }
}

#[async_trait]
impl Counter for RedisCounter {
    async fn hit(&self, key: &str, limit: Limit) -> Result<Outcome, String> {
        let window = limit.window.as_secs().max(1);
        let mut pipeline = redis::pipe();
        pipeline
            .atomic()
            .incr(key, 1)
            .cmd("EXPIRE")
            .arg(key)
            .arg(window)
            .arg("NX")
            .ignore()
            .ttl(key);
        // Sent again once if the connection had been closed: a hit counted twice, once,
        // only tightens the limit a little.
        let (pipeline, connection) = (&pipeline, &self.connection);
        let call = push_credentials::retry_if_dropped(|| async move {
            pipeline
                .query_async::<(u64, i64)>(&mut connection.clone())
                .await
        });
        let (count, ttl) = tokio::select! {
            () = self.shutdown.cancelled() => return Err("shutting down".to_owned()),
            result = tokio::time::timeout(REDIS_TIMEOUT, call) => result
                .map_err(|_| "redis did not answer in time".to_owned())?
                .map_err(|error| error.to_string())?,
        };
        if count <= u64::from(limit.max) {
            Ok(Outcome::Allowed)
        } else {
            Ok(Outcome::Limited {
                retry_after_s: ttl.max(1) as u64,
            })
        }
    }
}

/// Counters in this process, for tests and single-instance local runs.
#[derive(Debug, Default)]
pub struct MemoryCounter {
    counts: Mutex<HashMap<String, (u32, Instant)>>,
}

#[async_trait]
impl Counter for MemoryCounter {
    async fn hit(&self, key: &str, limit: Limit) -> Result<Outcome, String> {
        let now = Instant::now();
        let mut counts = self.counts.lock().expect("not poisoned");
        let (count, ends) = counts
            .entry(key.to_owned())
            .or_insert((0, now + limit.window));
        if now >= *ends {
            *count = 0;
            *ends = now + limit.window;
        }
        *count += 1;
        if *count <= limit.max {
            Ok(Outcome::Allowed)
        } else {
            Ok(Outcome::Limited {
                retry_after_s: ends.saturating_duration_since(now).as_secs().max(1),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Broken;

    #[async_trait]
    impl Counter for Broken {
        async fn hit(&self, _key: &str, _limit: Limit) -> Result<Outcome, String> {
            Err("connection refused".to_owned())
        }
    }

    fn ip(last: u8) -> Option<IpAddr> {
        Some(IpAddr::from([10, 0, 0, last]))
    }

    fn memory() -> Limits {
        Limits::new(Arc::new(MemoryCounter::default()))
    }

    #[tokio::test]
    async fn an_account_is_limited_across_ips_and_an_ip_across_accounts() {
        let limits = memory();
        for attempt in 0..3 {
            limits
                .check(
                    Action::ResendVerification,
                    ip(attempt),
                    Some("a@example.com"),
                )
                .await
                .unwrap();
        }
        let refused = limits
            .check(Action::ResendVerification, ip(9), Some("A@Example.com "))
            .await;
        assert!(
            matches!(refused, Err(AuthError::RateLimited { retry_after_s }) if retry_after_s > 3000),
            "{refused:?}"
        );
        // Other accounts from one IP: 20 per hour.
        for attempt in 0..20 {
            limits
                .check(
                    Action::Signup,
                    ip(1),
                    Some(&format!("{attempt}@example.com")),
                )
                .await
                .unwrap();
        }
        assert!(
            limits
                .check(Action::Signup, ip(1), Some("new@example.com"))
                .await
                .is_err()
        );
        assert!(
            limits
                .check(Action::Signup, ip(2), Some("new@example.com"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn login_counts_per_account_and_ip_under_a_ceiling_per_account() {
        let limits = memory();
        for _ in 0..10 {
            limits
                .check(Action::Login, ip(1), Some("a@example.com"))
                .await
                .unwrap();
        }
        let refused = limits
            .check(Action::Login, ip(1), Some("a@example.com"))
            .await;
        assert!(
            matches!(refused, Err(AuthError::RateLimited { retry_after_s }) if retry_after_s <= 900),
            "{refused:?}"
        );
        // The owner, from another address, is not locked out.
        limits
            .check(Action::Login, ip(2), Some("a@example.com"))
            .await
            .unwrap();
        // Guessing from many addresses meets the ceiling: 100 per hour, 89 to go.
        for attempt in 0..89u8 {
            limits
                .check(Action::Login, ip(3 + attempt / 10), Some("a@example.com"))
                .await
                .unwrap();
        }
        let refused = limits
            .check(Action::Login, ip(12), Some("a@example.com"))
            .await;
        assert!(
            matches!(refused, Err(AuthError::RateLimited { retry_after_s }) if retry_after_s > 900),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn ipv6_clients_are_counted_by_their_64_prefix() {
        let limits = memory();
        for host in 0..20u16 {
            let address = IpAddr::from([0x2001, 0xdb8, 1, 2, host, 0, 0, 1]);
            limits
                .check(Action::OpenLink, Some(address), None)
                .await
                .unwrap();
        }
        for _ in 0..40 {
            limits
                .check(Action::OpenLink, "2001:db8:1:2::ffff".parse().ok(), None)
                .await
                .unwrap();
        }
        assert!(
            limits
                .check(Action::OpenLink, "2001:db8:1:2:9::9".parse().ok(), None)
                .await
                .is_err(),
            "60 per hour for the whole /64"
        );
        assert!(
            limits
                .check(Action::OpenLink, "2001:db8:1:3::1".parse().ok(), None)
                .await
                .is_ok(),
            "another /64"
        );
        assert_eq!(
            ip_subject("::ffff:10.0.0.1".parse().unwrap()),
            ip_subject("10.0.0.1".parse().unwrap())
        );
    }

    #[test]
    fn handle_lookups_follow_the_spec() {
        assert!(Action::HandleLookup.limits().contains(&(
            Scope::Account,
            Limit {
                max: 20,
                window: Duration::from_secs(3600)
            }
        )));
    }

    #[tokio::test]
    async fn a_failing_counter_refuses_with_a_retryable_error() {
        let limits = Limits::new(Arc::new(Broken));
        let refused = limits
            .check(Action::Login, ip(1), Some("a@example.com"))
            .await;
        assert_eq!(refused, Err(AuthError::Unavailable));
        assert!(AuthError::Unavailable.retryable());
        assert_eq!(AuthError::Unavailable.status(), 503);
    }

    #[test]
    fn keys_hash_the_subject() {
        let key = key(Action::Login, Scope::Account, "alice@example.com");
        assert!(key.starts_with("rl:login:account:"), "{key}");
        assert!(!key.contains("alice"), "{key}");
        assert_eq!(key.len(), "rl:login:account:".len() + 32);
    }
}
