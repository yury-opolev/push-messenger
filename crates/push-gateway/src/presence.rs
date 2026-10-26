//! Presence leases in Redis, shared by every gateway replica (spec: activity and
//! presence, "Redis presence state"). Each connection holds a lease on its user's
//! `presence:{user}:conns`; the user is online while any lease lasts. A closed connection
//! keeps its lease for a grace, so a reconnect within it is never shown offline. On a
//! transition the gateway asks message-svc to announce the user's presence, which reads
//! the shared state itself.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use push_proto::id_bytes;
use push_proto::internal::v1::AnnouncePresenceRequest;
use push_proto::{presence_conns_key, presence_state_key};
use redis::aio::ConnectionManager;
use redis::{ErrorKind, RedisResult, ServerErrorKind};
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::server::MessagingApi;

/// How long a lease of a live connection lasts: three refreshes, so a gateway that died
/// leaves its users online for at most this long.
const LEASE: Duration = Duration::from_secs(90);
/// How often a live connection renews its lease.
const REFRESH: Duration = Duration::from_secs(30);
/// How long a user's announced state and last-seen minute are kept after their last write.
const STATE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// How much longer than the grace settling waits, so the lease has surely ended on Redis's
/// clock.
const SETTLE_MARGIN: Duration = Duration::from_millis(100);
/// How long one presence script may take.
const REDIS_TIMEOUT: Duration = Duration::from_millis(500);
/// How long one `AnnouncePresence` call may take.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(2);

/// Adds or renews a connection's lease (`ARGV[1]` its id, `ARGV[2]` the lease in ms) and
/// marks the user online, with the current minute as last seen (`ARGV[3]` the state's TTL
/// in seconds). Returns 1 when the user had no live lease before, counting this
/// connection's own: announce online. A refresh, or a reconnect inside another
/// connection's grace, returns 0; a user whose set expired with a dead gateway returns 1.
const UP: &str = r"
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now)
local before = redis.call('ZCARD', KEYS[1])
redis.call('ZADD', KEYS[1], now + tonumber(ARGV[2]), ARGV[1])
-- The set lives as long as its longest lease, never shorter.
local top = redis.call('ZRANGE', KEYS[1], -1, -1, 'WITHSCORES')
redis.call('PEXPIREAT', KEYS[1], tonumber(top[2]))
redis.call('HSET', KEYS[2], 'on', '1', 'seen', t[1] - (t[1] % 60))
redis.call('EXPIRE', KEYS[2], tonumber(ARGV[3]))
if before == 0 then return 1 end
return 0
";

/// A connection closed (`ARGV[1]` its id): its lease ends after the grace (`ARGV[2]` ms),
/// and the current minute is the last seen (`ARGV[3]` the state's TTL in seconds).
const DOWN: &str = r"
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
if redis.call('ZSCORE', KEYS[1], ARGV[1]) then
  redis.call('ZADD', KEYS[1], 'XX', now + tonumber(ARGV[2]), ARGV[1])
  -- A grace longer than what was left of the lease must not outlive the set.
  local top = redis.call('ZRANGE', KEYS[1], -1, -1, 'WITHSCORES')
  redis.call('PEXPIREAT', KEYS[1], tonumber(top[2]))
end
redis.call('HSET', KEYS[2], 'seen', t[1] - (t[1] % 60))
redis.call('EXPIRE', KEYS[2], tonumber(ARGV[3]))
return 0
";

/// After a closed connection's grace: prunes the ended leases and returns 1 when none is
/// left while the user is still announced online, which it clears: announce offline.
/// message-svc's read script does the same for users whose gateway never settled.
const SETTLE: &str = r"
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now)
if redis.call('ZCARD', KEYS[1]) == 0 and redis.call('HGET', KEYS[2], 'on') == '1' then
  redis.call('HSET', KEYS[2], 'on', '0')
  return 1
end
return 0
";

/// A Lua script run with `EVALSHA`, loaded with `SCRIPT LOAD` when Redis lacks it.
struct Script {
    source: &'static str,
    /// The script's SHA1 once Redis loaded it; cleared when Redis lost it.
    sha: Mutex<Option<String>>,
}

impl Script {
    const fn new(source: &'static str) -> Self {
        Self {
            source,
            sha: Mutex::new(None),
        }
    }

    /// Runs the script on `user`'s two keys. `retried` is set when a first attempt went
    /// out on a connection that closed, so the script may have run twice and its answer
    /// is not to be trusted.
    async fn run(
        &self,
        redis: &ConnectionManager,
        user: Uuid,
        args: &[String],
    ) -> RedisResult<Ran> {
        match self.run_loaded(redis, user, args).await {
            Err(error) if push_credentials::is_dropped_connection(&error) => {
                tracing::debug!(%error, "the Redis connection was closed; running once more");
                let value = self.run_loaded(redis, user, args).await?;
                Ok(Ran {
                    value,
                    retried: true,
                })
            }
            result => result.map(|value| Ran {
                value,
                retried: false,
            }),
        }
    }

    /// Runs the script, loading it once when Redis does not have it.
    async fn run_loaded(
        &self,
        redis: &ConnectionManager,
        user: Uuid,
        args: &[String],
    ) -> RedisResult<i64> {
        match self.evalsha(redis, user, args).await {
            Err(error) if error.kind() == ErrorKind::Server(ServerErrorKind::NoScript) => {
                self.sha.lock().expect("presence script lock").take();
                self.evalsha(redis, user, args).await
            }
            result => result,
        }
    }

    async fn evalsha(
        &self,
        redis: &ConnectionManager,
        user: Uuid,
        args: &[String],
    ) -> RedisResult<i64> {
        let sha = self.sha(redis).await?;
        redis::cmd("EVALSHA")
            .arg(sha)
            .arg(2)
            .arg(presence_conns_key(user))
            .arg(presence_state_key(user))
            .arg(args)
            .query_async(&mut redis.clone())
            .await
    }

    /// The SHA1 of the loaded script, loading it first if needed.
    async fn sha(&self, redis: &ConnectionManager) -> RedisResult<String> {
        if let Some(sha) = self.sha.lock().expect("presence script lock").clone() {
            return Ok(sha);
        }
        let mut load = redis::cmd("SCRIPT");
        load.arg("LOAD").arg(self.source);
        let load = &load;
        // Loading twice is harmless.
        let sha: String = push_credentials::retry_if_dropped(|| async move {
            load.query_async(&mut redis.clone()).await
        })
        .await?;
        *self.sha.lock().expect("presence script lock") = Some(sha.clone());
        Ok(sha)
    }
}

/// A script's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ran {
    value: i64,
    /// The script may have run twice: an earlier attempt's answer was lost.
    retried: bool,
}

impl Ran {
    /// Whether to announce: the script saw a transition, or may have seen it in the run
    /// whose answer was lost. Announcing is idempotent, so a doubt announces.
    fn transition(self) -> bool {
        self.value == 1 || self.retried
    }
}

/// How a bounded lease script ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Redis answered.
    Answered(Ran),
    /// It failed or timed out, possibly after the script ran on Redis (cancelling a sent
    /// command does not undo it), so a transition may have been lost.
    Unknown,
}

impl Outcome {
    /// Whether an UP or SETTLE outcome is to be announced: a transition, or a doubt, as
    /// announcing is idempotent and a lost transition would never be announced again.
    fn announces(self) -> bool {
        match self {
            Self::Answered(ran) => ran.transition(),
            Self::Unknown => true,
        }
    }
}

/// The three lease scripts on the gateway's Redis connection.
struct Leases {
    redis: ConnectionManager,
    up: Script,
    down: Script,
    settle: Script,
}

impl Leases {
    fn new(redis: ConnectionManager) -> Self {
        Self {
            redis,
            up: Script::new(UP),
            down: Script::new(DOWN),
            settle: Script::new(SETTLE),
        }
    }

    /// Adds or renews `connection`'s lease of `lease`.
    async fn up(&self, user: Uuid, connection: Uuid, lease: Duration) -> RedisResult<Ran> {
        let args = [connection.to_string(), millis(lease), seconds(STATE_TTL)];
        self.up.run(&self.redis, user, &args).await
    }

    /// Ends `connection`'s lease after `grace`.
    async fn down(&self, user: Uuid, connection: Uuid, grace: Duration) -> RedisResult<Ran> {
        let args = [connection.to_string(), millis(grace), seconds(STATE_TTL)];
        self.down.run(&self.redis, user, &args).await
    }

    /// Clears the online mark once no lease is left.
    async fn settle(&self, user: Uuid) -> RedisResult<Ran> {
        self.settle.run(&self.redis, user, &[]).await
    }
}

fn millis(duration: Duration) -> String {
    duration.as_millis().to_string()
}

fn seconds(duration: Duration) -> String {
    duration.as_secs().to_string()
}

/// What tracking presence needs from the gateway. Cheap to clone.
#[derive(Clone)]
pub(crate) struct PresenceContext {
    leases: Arc<Leases>,
    messaging: MessagingApi,
    grace: Duration,
    shutdown: CancellationToken,
    tasks: TaskTracker,
}

impl PresenceContext {
    /// Leases on `redis` (the gateway's publisher), announcements through `messaging`, a
    /// closed connection's lease lasting `grace`, tasks on `tasks`, which shutdown waits
    /// for; after `shutdown` closed connections are not settled.
    pub(crate) fn new(
        redis: ConnectionManager,
        messaging: MessagingApi,
        grace: Duration,
        shutdown: CancellationToken,
        tasks: TaskTracker,
    ) -> Self {
        Self {
            leases: Arc::new(Leases::new(redis)),
            messaging,
            grace,
            shutdown,
            tasks,
        }
    }
}

/// Ends the tracking of a connection's presence when dropped.
pub(crate) struct PresenceGuard {
    _closed: DropGuard,
}

/// Starts tracking a connection's presence: `up` now and every 30 s, and, once the
/// returned guard is dropped, `down`, then after the grace `settle` (skipped on shutdown).
/// Transitions call `Messaging.AnnouncePresence`, as does an `up` or `settle` whose
/// outcome is unknown. Redis or message-svc failures are logged and otherwise ignored
/// (fail open; the connection is never affected).
pub(crate) fn track(context: PresenceContext, user: Uuid, connection: Uuid) -> PresenceGuard {
    let closed = CancellationToken::new();
    let tasks = context.tasks.clone();
    tasks.spawn(run(context, user, connection, closed.clone()));
    PresenceGuard {
        _closed: closed.drop_guard(),
    }
}

/// One connection's presence, step by step, so its steps never overtake each other.
async fn run(context: PresenceContext, user: Uuid, connection: Uuid, closed: CancellationToken) {
    loop {
        let up = context.leases.up(user, connection, LEASE);
        if bounded(up, user, "renew a presence lease")
            .await
            .announces()
        {
            announce(&context, user).await;
        }
        tokio::select! {
            () = closed.cancelled() => break,
            () = tokio::time::sleep(REFRESH) => {}
        }
    }
    let down = context.leases.down(user, connection, context.grace);
    bounded(down, user, "end a presence lease").await;
    // On shutdown reads stay correct without settling, and the clients normally
    // reconnect to another replica within the grace.
    tokio::select! {
        () = context.shutdown.cancelled() => return,
        () = tokio::time::sleep(context.grace + SETTLE_MARGIN) => {}
    }
    let settle = context.leases.settle(user);
    if bounded(settle, user, "settle presence").await.announces() {
        announce(&context, user).await;
    }
}

/// How a lease script ended; a failure or timeout is logged and its outcome unknown.
async fn bounded(
    script: impl Future<Output = RedisResult<Ran>>,
    user: Uuid,
    what: &str,
) -> Outcome {
    match tokio::time::timeout(REDIS_TIMEOUT, script).await {
        Ok(Ok(ran)) => Outcome::Answered(ran),
        Ok(Err(error)) => {
            tracing::warn!(%error, %user, "could not {what}");
            Outcome::Unknown
        }
        Err(_) => {
            tracing::warn!(%user, "could not {what}: timed out");
            Outcome::Unknown
        }
    }
}

/// Asks message-svc to announce `user`'s presence to their contacts (logged on failure:
/// the next transition or read corrects it).
async fn announce(context: &PresenceContext, user: Uuid) {
    let request = AnnouncePresenceRequest {
        user_id: id_bytes(user),
    };
    let mut messaging = context.messaging.clone();
    let call = messaging.announce_presence(request);
    match tokio::time::timeout(ANNOUNCE_TIMEOUT, call).await {
        Ok(Ok(_)) => {}
        Ok(Err(status)) => {
            tracing::warn!(code = ?status.code(), message = status.message(), %user, "could not announce presence");
        }
        Err(_) => tracing::warn!(%user, "could not announce presence: timed out"),
    }
}

#[cfg(test)]
mod tests {
    use testcontainers_modules::redis::Redis;
    use testcontainers_modules::testcontainers::runners::AsyncRunner;
    use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

    use super::*;

    /// The scripts against a real Redis (Docker must be running).
    async fn leases() -> (ContainerAsync<Redis>, Leases) {
        let container = Redis::default()
            .with_tag("7-alpine")
            .start()
            .await
            .expect("start a Redis container (is Docker running?)");
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let client = redis::Client::open(format!("redis://127.0.0.1:{port}")).unwrap();
        let connection = client.get_connection_manager().await.unwrap();
        (container, Leases::new(connection))
    }

    fn value(ran: RedisResult<Ran>) -> i64 {
        let ran = ran.unwrap();
        assert!(!ran.retried);
        ran.value
    }

    /// How long `user`'s set of leases lasts, in ms.
    async fn pttl(leases: &Leases, user: Uuid) -> i64 {
        redis::cmd("PTTL")
            .arg(presence_conns_key(user))
            .query_async(&mut leases.redis.clone())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_first_lease_and_the_last_ones_end_are_transitions() {
        let (_container, leases) = leases().await;
        let (user, phone, laptop) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let grace = Duration::from_millis(200);

        assert_eq!(value(leases.up(user, phone, LEASE).await), 1);
        assert_eq!(value(leases.up(user, phone, LEASE).await), 0, "a refresh");
        assert_eq!(value(leases.up(user, laptop, LEASE).await), 0);
        value(leases.down(user, phone, grace).await);
        value(leases.down(user, laptop, grace).await);
        assert_eq!(value(leases.settle(user).await), 0, "within the grace");

        tokio::time::sleep(grace + SETTLE_MARGIN).await;
        assert_eq!(value(leases.settle(user).await), 1);
        assert_eq!(value(leases.settle(user).await), 0, "announced once");
        assert_eq!(value(leases.up(user, phone, LEASE).await), 1);
    }

    #[tokio::test]
    async fn a_reconnect_within_the_grace_is_no_transition() {
        let (_container, leases) = leases().await;
        let (user, first, second) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let grace = Duration::from_millis(200);

        assert_eq!(value(leases.up(user, first, LEASE).await), 1);
        value(leases.down(user, first, grace).await);
        assert_eq!(value(leases.up(user, second, LEASE).await), 0);
        tokio::time::sleep(grace + SETTLE_MARGIN).await;
        assert_eq!(value(leases.settle(user).await), 0);
    }

    /// The set expires with its longest lease, also when a grace outlasts what was left
    /// of the lease, and a user whose set expired (their gateway died) is online again.
    #[tokio::test]
    async fn the_set_lives_as_long_as_its_longest_lease() {
        let (_container, leases) = leases().await;
        let (user, connection) = (Uuid::now_v7(), Uuid::now_v7());
        value(
            leases
                .up(user, connection, Duration::from_millis(100))
                .await,
        );
        assert!((1..=100).contains(&pttl(&leases, user).await));
        value(leases.down(user, connection, Duration::from_secs(5)).await);
        assert!((4_000..=5_000).contains(&pttl(&leases, user).await));

        let dead = Uuid::now_v7();
        value(
            leases
                .up(dead, connection, Duration::from_millis(100))
                .await,
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(value(leases.up(dead, connection, LEASE).await), 1);
    }

    #[tokio::test]
    async fn a_lost_script_is_loaded_again() {
        let (_container, leases) = leases().await;
        let user = Uuid::now_v7();
        value(leases.settle(user).await);
        redis::cmd("SCRIPT")
            .arg("FLUSH")
            .exec_async(&mut leases.redis.clone())
            .await
            .unwrap();
        assert_eq!(value(leases.settle(user).await), 0);
    }

    #[test]
    fn a_doubtful_answer_announces() {
        let ran = |value, retried| Ran { value, retried }.transition();
        assert!(ran(1, false));
        assert!(!ran(0, false));
        assert!(ran(0, true));
    }

    #[test]
    fn an_unknown_outcome_announces() {
        let answered = |value, retried| Outcome::Answered(Ran { value, retried }).announces();
        assert!(answered(1, false));
        assert!(!answered(0, false));
        assert!(answered(0, true));
        assert!(Outcome::Unknown.announces());
    }

    /// A script that never answers in time is unknown, even though it may have run.
    #[tokio::test(start_paused = true)]
    async fn a_timed_out_script_is_unknown() {
        let never = std::future::pending::<RedisResult<Ran>>();
        assert_eq!(
            bounded(never, Uuid::now_v7(), "test").await,
            Outcome::Unknown
        );
        let failed = async { Err(redis::RedisError::from((ErrorKind::Io, "closed"))) };
        assert_eq!(
            bounded(failed, Uuid::now_v7(), "test").await,
            Outcome::Unknown
        );
    }
}
