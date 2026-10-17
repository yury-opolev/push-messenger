//! Rate limits of the gateway: the requests that let someone guess or probe (redeeming
//! invites, contact requests, creating invites), `Identify` per device (spec §7.3),
//! writes (`Send`, `Edit`, `Delete`) per user, and frames per connection.
//!
//! Request limits are fixed windows in Redis (`INCR`, then `EXPIRE … NX`), keyed by a
//! hash of the subject, so no user id or IP is stored in Redis; they count across every
//! gateway. When Redis does not answer within 500 ms the request is refused with a
//! retryable `UNAVAILABLE` and a warning (fail closed, like auth-svc): without counters
//! nothing would stop someone guessing invite codes. `Identify` and writes fail open
//! instead: signed-in devices must keep connecting and messaging while Redis is down
//! (spec §12, test 4); meanwhile writes are bounded only by the frame budget.
//!
//! The frame budget is per connection, in memory: a token bucket of 100 frames refilled
//! at 20 a second. A connection that runs dry is closed with 4004.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use push_proto::v1::request;
use redis::aio::ConnectionManager;
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// What is being limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// `RedeemInvite`: guessing codes.
    RedeemInvite,
    /// `ContactRequest`: probing user ids, or flooding users with requests.
    ContactRequest,
    /// `CreateInvite`: filling the invite table.
    CreateInvite,
    /// `Identify`, per device: reconnect loops.
    Identify,
    /// `Send`, `Edit` and `Delete`, per user across their devices: flooding conversations
    /// and storage.
    Write,
}

/// Whom a limit counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The client's IP (an IPv6 /64).
    Ip,
    User,
    Device,
}

/// At most `max` per `window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max: u32,
    pub window: Duration,
}

const fn per(max: u32, seconds: u64) -> Limit {
    Limit {
        max,
        window: Duration::from_secs(seconds),
    }
}

const HOUR: u64 = 3600;
const MINUTE: u64 = 60;

impl Action {
    /// The action a request is limited as, if any.
    pub fn of(body: &request::Body) -> Option<Self> {
        match body {
            request::Body::RedeemInvite(_) => Some(Self::RedeemInvite),
            request::Body::ContactRequest(_) => Some(Self::ContactRequest),
            request::Body::CreateInvite(_) => Some(Self::CreateInvite),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::RedeemInvite => "redeem",
            Self::ContactRequest => "contact",
            Self::CreateInvite => "invite",
            Self::Identify => "identify",
            Self::Write => "write",
        }
    }

    /// The limits of the action, in the order they are counted. A request refused by one
    /// is not counted by the later ones.
    pub fn limits(self) -> &'static [(Scope, Limit)] {
        // 50-bit codes: 20 guesses an hour per user and 60 per address. The user is
        // counted first, so a user over their own limit does not use up the quota of an
        // address shared behind NAT.
        const REDEEM: &[(Scope, Limit)] =
            &[(Scope::User, per(20, HOUR)), (Scope::Ip, per(60, HOUR))];
        const CONTACT: &[(Scope, Limit)] =
            &[(Scope::User, per(30, HOUR)), (Scope::Ip, per(120, HOUR))];
        const INVITE: &[(Scope, Limit)] = &[(Scope::User, per(20, HOUR))];
        const IDENTIFY: &[(Scope, Limit)] = &[(Scope::Device, per(30, MINUTE))];
        // Two a second on average, in bursts: far above a person typing, on all their
        // devices together; the hour caps a sustained flood.
        const WRITE: &[(Scope, Limit)] = &[
            (Scope::User, per(120, MINUTE)),
            (Scope::User, per(3000, HOUR)),
        ];
        match self {
            Self::RedeemInvite => REDEEM,
            Self::ContactRequest => CONTACT,
            Self::CreateInvite => INVITE,
            Self::Identify => IDENTIFY,
            Self::Write => WRITE,
        }
    }
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::Ip => "ip",
            Self::User => "user",
            Self::Device => "device",
        }
    }
}

/// The answer of a counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allowed,
    Limited { retry_after_s: u32 },
}

/// A store of fixed-window counters.
#[async_trait]
pub trait Counter: Send + Sync {
    /// Counts one hit against `key`; whether it is within `limit`.
    async fn hit(&self, key: &str, limit: Limit) -> Result<Outcome, String>;
}

/// Why a limit refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Over the limit: retry after this many seconds.
    Limited { retry_after_s: u32 },
    /// The counters could not be reached.
    Unavailable,
}

/// The gateway's rate limits over one counter store.
#[derive(Clone)]
pub struct Limits {
    counter: Arc<dyn Counter>,
}

impl std::fmt::Debug for Limits {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Limits")
    }
}

/// Who did something: the subjects an action may be counted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Subject {
    pub ip: Option<IpAddr>,
    pub user: Option<Uuid>,
    pub device: Option<Uuid>,
}

impl Limits {
    pub fn new(counter: Arc<dyn Counter>) -> Self {
        Self { counter }
    }

    /// Counts `action` by `subject`. A failing counter is `Unavailable`; the caller
    /// decides whether that refuses.
    pub async fn check(&self, action: Action, subject: Subject) -> Result<(), Refused> {
        for &(scope, limit) in action.limits() {
            let id = match scope {
                Scope::Ip => subject.ip.map(ip_subject),
                Scope::User => subject.user.map(|user| user.to_string()),
                Scope::Device => subject.device.map(|device| device.to_string()),
            };
            let Some(id) = id else {
                continue;
            };
            match self
                .counter
                .hit(&key(action, scope, limit, &id), limit)
                .await
            {
                Ok(Outcome::Allowed) => {}
                Ok(Outcome::Limited { retry_after_s }) => {
                    return Err(Refused::Limited { retry_after_s });
                }
                Err(error) => {
                    tracing::warn!(%error, action = action.name(), "rate limit not checked");
                    return Err(Refused::Unavailable);
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

/// `gw:{action}:{scope}:{hash}:{window_s}`: the subject is hashed, never stored; the
/// window tells apart two limits of one scope.
fn key(action: Action, scope: Scope, limit: Limit, subject: &str) -> String {
    let digest = Sha256::digest(subject.as_bytes());
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(
        "gw:{}:{}:{hex}:{}",
        action.name(),
        scope.name(),
        limit.window.as_secs()
    )
}

/// How long a counter may take before the request is decided without it.
pub const REDIS_TIMEOUT: Duration = Duration::from_millis(500);

/// Counters in Redis, shared by every gateway.
#[derive(Clone)]
pub struct RedisCounter {
    connection: ConnectionManager,
    shutdown: CancellationToken,
}

impl RedisCounter {
    /// Counters on `connection`; calls give up when `shutdown` is cancelled.
    pub fn new(connection: ConnectionManager, shutdown: CancellationToken) -> Self {
        Self {
            connection,
            shutdown,
        }
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
        let mut connection = self.connection.clone();
        let call = pipeline.query_async::<(u64, i64)>(&mut connection);
        let (count, ttl) = tokio::select! {
            () = self.shutdown.cancelled() => return Err("shutting down".to_owned()),
            result = tokio::time::timeout(REDIS_TIMEOUT, call) => result
                .map_err(|_| "redis did not answer in time".to_owned())?
                .map_err(|error| format!("{:?}", error.kind()))?,
        };
        if count <= u64::from(limit.max) {
            Ok(Outcome::Allowed)
        } else {
            Ok(Outcome::Limited {
                retry_after_s: u32::try_from(ttl.max(1)).unwrap_or(u32::MAX),
            })
        }
    }
}

/// A connection's frame budget: a token bucket.
///
/// A backlog costs one `Ack` per `Sync` batch (up to 500 entries) when the client acks only
/// the highest seq it holds, as the SDK must (Plan 9): 10,000 waiting entries are 20 acks,
/// far inside the burst, and the next batch comes only after the ack's round trip. A client
/// that acked every `Deliver` of a backlog would spend 500 frames a batch and be closed
/// with 4004.
#[derive(Debug, Clone)]
pub struct FrameBudget {
    tokens: f64,
    updated: Instant,
}

impl FrameBudget {
    /// Frames a connection may send at once.
    pub const BURST: f64 = 100.0;
    /// Frames a second a connection may keep sending.
    pub const PER_SECOND: f64 = 20.0;

    pub fn new() -> Self {
        Self {
            tokens: Self::BURST,
            updated: Instant::now(),
        }
    }

    /// Spends one frame; false when the budget is exhausted.
    pub fn spend(&mut self) -> bool {
        let now = Instant::now();
        let refill = now.duration_since(self.updated).as_secs_f64() * Self::PER_SECOND;
        self.tokens = (self.tokens + refill).min(Self::BURST);
        self.updated = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

impl Default for FrameBudget {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use push_proto::v1::{ContactRequest, CreateInvite, ListContacts, RedeemInvite};

    use super::*;

    /// Counters in this process.
    #[derive(Debug, Default)]
    struct MemoryCounter {
        counts: std::sync::Mutex<HashMap<String, (u32, Instant)>>,
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
                let left = ends.saturating_duration_since(now).as_secs().max(1);
                Ok(Outcome::Limited {
                    retry_after_s: u32::try_from(left).unwrap_or(u32::MAX),
                })
            }
        }
    }

    struct Broken;

    #[async_trait]
    impl Counter for Broken {
        async fn hit(&self, _key: &str, _limit: Limit) -> Result<Outcome, String> {
            Err("connection refused".to_owned())
        }
    }

    fn memory() -> Limits {
        Limits::new(Arc::new(MemoryCounter::default()))
    }

    fn from(ip: u8, user: Uuid) -> Subject {
        Subject {
            ip: Some(IpAddr::from([10, 0, 0, ip])),
            user: Some(user),
            device: None,
        }
    }

    #[test]
    fn only_guessing_and_probing_requests_are_limited() {
        let redeem = request::Body::RedeemInvite(RedeemInvite::default());
        let ask = request::Body::ContactRequest(ContactRequest::default());
        let invite = request::Body::CreateInvite(CreateInvite::default());
        let list = request::Body::ListContacts(ListContacts {});
        assert_eq!(Action::of(&redeem), Some(Action::RedeemInvite));
        assert_eq!(Action::of(&ask), Some(Action::ContactRequest));
        assert_eq!(Action::of(&invite), Some(Action::CreateInvite));
        assert_eq!(Action::of(&list), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_user_is_limited_across_addresses_and_an_address_across_users() {
        let limits = memory();
        let alice = Uuid::now_v7();
        for attempt in 0..20 {
            limits
                .check(Action::RedeemInvite, from(attempt, alice))
                .await
                .unwrap();
        }
        let refused = limits.check(Action::RedeemInvite, from(99, alice)).await;
        assert!(
            matches!(refused, Err(Refused::Limited { retry_after_s }) if retry_after_s > 3000),
            "{refused:?}"
        );
        // One address, many users: 60 an hour.
        for _ in 0..60 {
            let user = Uuid::now_v7();
            limits
                .check(Action::RedeemInvite, from(200, user))
                .await
                .unwrap();
        }
        assert!(
            limits
                .check(Action::RedeemInvite, from(200, Uuid::now_v7()))
                .await
                .is_err()
        );
        // The window ends.
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(
            limits
                .check(Action::RedeemInvite, from(99, alice))
                .await
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_user_over_their_limit_does_not_use_up_the_shared_address() {
        let limits = memory();
        let noisy = Uuid::now_v7();
        for _ in 0..20 {
            limits
                .check(Action::RedeemInvite, from(7, noisy))
                .await
                .unwrap();
        }
        // Refused by their own limit: these must not count against the address.
        for _ in 0..100 {
            assert!(
                limits
                    .check(Action::RedeemInvite, from(7, noisy))
                    .await
                    .is_err()
            );
        }
        // The address has used 20 of its 60; 40 other users still fit.
        for _ in 0..40 {
            limits
                .check(Action::RedeemInvite, from(7, Uuid::now_v7()))
                .await
                .unwrap();
        }
        assert!(
            limits
                .check(Action::RedeemInvite, from(7, Uuid::now_v7()))
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn identify_is_counted_per_device() {
        let limits = memory();
        let device = Subject {
            device: Some(Uuid::now_v7()),
            ..Subject::default()
        };
        for _ in 0..30 {
            limits.check(Action::Identify, device).await.unwrap();
        }
        assert!(matches!(
            limits.check(Action::Identify, device).await,
            Err(Refused::Limited { retry_after_s }) if retry_after_s <= 60
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn writes_are_counted_per_user_by_the_minute_and_the_hour() {
        let limits = memory();
        let alice = Uuid::now_v7();
        let writer = Subject {
            user: Some(alice),
            ..Subject::default()
        };
        for _ in 0..120 {
            limits.check(Action::Write, writer).await.unwrap();
        }
        assert!(matches!(
            limits.check(Action::Write, writer).await,
            Err(Refused::Limited { retry_after_s }) if retry_after_s <= 60
        ));
        // Another user is not affected.
        let bob = Subject {
            user: Some(Uuid::now_v7()),
            ..Subject::default()
        };
        limits.check(Action::Write, bob).await.unwrap();
        // 120 a minute for 24 more minutes reach the hour's 3,000; the 25th minute is
        // refused by the hour, not the minute.
        for _ in 0..24 {
            tokio::time::advance(Duration::from_secs(60)).await;
            for _ in 0..120 {
                limits.check(Action::Write, writer).await.unwrap();
            }
        }
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(matches!(
            limits.check(Action::Write, writer).await,
            Err(Refused::Limited { retry_after_s }) if retry_after_s > 60
        ));
    }

    #[tokio::test]
    async fn a_failing_counter_is_unavailable() {
        let limits = Limits::new(Arc::new(Broken));
        assert_eq!(
            limits
                .check(Action::ContactRequest, from(1, Uuid::now_v7()))
                .await,
            Err(Refused::Unavailable)
        );
    }

    #[test]
    fn keys_hash_the_subject() {
        let user = Uuid::now_v7().to_string();
        let key = key(Action::ContactRequest, Scope::User, per(30, HOUR), &user);
        assert!(key.starts_with("gw:contact:user:"), "{key}");
        assert!(key.ends_with(":3600"), "{key}");
        assert!(!key.contains(&user), "{key}");
        assert_eq!(
            ip_subject("::ffff:10.0.0.1".parse().unwrap()),
            ip_subject("10.0.0.1".parse().unwrap())
        );
        assert_eq!(
            ip_subject("2001:db8:1:2:3:4:5:6".parse().unwrap()),
            "2001:db8:1:2::/64"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_frame_budget_allows_bursts_and_a_steady_rate() {
        let mut budget = FrameBudget::new();
        for _ in 0..100 {
            assert!(budget.spend());
        }
        assert!(!budget.spend(), "the burst is used up");
        tokio::time::advance(Duration::from_millis(500)).await;
        for _ in 0..10 {
            assert!(budget.spend(), "20 a second come back");
        }
        assert!(!budget.spend());
    }
}
