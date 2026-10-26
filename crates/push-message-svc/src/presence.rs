//! Reading the live presence of users, which the gateways record in Redis (spec: activity
//! and presence, "Redis presence state").

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::DateTime;
use push_domain::{Clock, DomainResult, Messaging, Notifier, PresenceView, Store, UserId};
use push_proto::{presence_conns_key, presence_state_key};
use redis::aio::ConnectionManager;
use redis::{ErrorKind, RedisResult, ServerErrorKind};

/// The live presence could not be read (Redis failed or timed out). Nothing read this way
/// may be pushed: a guess would tell contacts a wrong state until the next transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("presence is unavailable")]
pub struct PresenceUnavailable;

/// The live presence of users, as the gateways recorded it in Redis.
#[async_trait]
pub trait PresenceStates: Send + Sync {
    /// The state of each of `users` (same order): `Online` or `Offline { last_seen }`;
    /// never `Unknown`. `flipped` lists users this read found newly offline (their
    /// gateway died without settling): the caller announces them. When the state cannot
    /// be read, `PresenceUnavailable` (logged here).
    async fn states(
        &self,
        users: &[UserId],
    ) -> Result<(Vec<PresenceView>, Vec<UserId>), PresenceUnavailable>;
}

/// Prunes the expired leases of one user and reports `{online, seen, flipped}`: online
/// while a lease is left, `seen` the last-seen minute (unix seconds, empty when unknown),
/// and flipped when no lease is left but the user was still announced online, which this
/// clears so only one reader announces it. Uses Redis `TIME`, never a replica's clock.
const READ_SCRIPT: &str = r"
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now)
local live = redis.call('ZCARD', KEYS[1])
local seen = redis.call('HGET', KEYS[2], 'seen') or ''
local flipped = 0
if live == 0 and redis.call('HGET', KEYS[2], 'on') == '1' then
  redis.call('HSET', KEYS[2], 'on', '0')
  flipped = 1
end
return {live > 0 and 1 or 0, seen, flipped}
";

/// The longest a presence read may take: a stuck Redis must not hang requests.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Reads presence with [`READ_SCRIPT`], one `EVALSHA` per user in one pipeline. Cluster
/// safe: each call touches only one user's two keys, which share a hash tag.
#[derive(Clone)]
pub struct RedisPresence {
    connection: ConnectionManager,
    /// The script's SHA1 once Redis loaded it; cleared when Redis lost it.
    sha: Arc<Mutex<Option<String>>>,
}

impl RedisPresence {
    pub(crate) fn new(connection: ConnectionManager) -> Self {
        Self {
            connection,
            sha: Arc::default(),
        }
    }

    /// The raw `{online, seen, flipped}` of each user. Loads the script when Redis does
    /// not have it, once.
    async fn read(&self, users: &[UserId]) -> RedisResult<Vec<(i64, String, i64)>> {
        match self.read_loaded(users).await {
            Err(error) if error.kind() == ErrorKind::Server(ServerErrorKind::NoScript) => {
                self.sha.lock().expect("presence script lock").take();
                self.read_loaded(users).await
            }
            result => result,
        }
    }

    async fn read_loaded(&self, users: &[UserId]) -> RedisResult<Vec<(i64, String, i64)>> {
        let sha = self.sha().await?;
        let mut pipeline = redis::pipe();
        for user in users {
            pipeline
                .cmd("EVALSHA")
                .arg(&sha)
                .arg(2)
                .arg(presence_conns_key(user.0))
                .arg(presence_state_key(user.0));
        }
        // A script run twice prunes and flips nothing new, so sending again is harmless.
        let pipeline = &pipeline;
        push_credentials::retry_if_dropped(|| async move {
            pipeline.query_async(&mut self.connection.clone()).await
        })
        .await
    }

    /// The SHA1 of the loaded script, loading it first if needed.
    async fn sha(&self) -> RedisResult<String> {
        if let Some(sha) = self.sha.lock().expect("presence script lock").clone() {
            return Ok(sha);
        }
        let mut load = redis::cmd("SCRIPT");
        load.arg("LOAD").arg(READ_SCRIPT);
        let load = &load;
        let sha: String = push_credentials::retry_if_dropped(|| async move {
            load.query_async(&mut self.connection.clone()).await
        })
        .await?;
        *self.sha.lock().expect("presence script lock") = Some(sha.clone());
        Ok(sha)
    }
}

#[async_trait]
impl PresenceStates for RedisPresence {
    async fn states(
        &self,
        users: &[UserId],
    ) -> Result<(Vec<PresenceView>, Vec<UserId>), PresenceUnavailable> {
        if users.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let read = match tokio::time::timeout(READ_TIMEOUT, self.read(users)).await {
            Ok(Ok(read)) if read.len() == users.len() => Ok(read),
            Ok(Ok(read)) => Err(format!("{} answers for {} users", read.len(), users.len())),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("timed out".to_owned()),
        };
        let read = match read {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(%error, count = users.len(), "could not read presence");
                return Err(PresenceUnavailable);
            }
        };
        let mut flipped = Vec::new();
        let views = users
            .iter()
            .zip(read)
            .map(|(user, (online, seen, flip))| {
                if flip == 1 {
                    flipped.push(*user);
                }
                if online == 1 {
                    PresenceView::Online
                } else {
                    let last_seen = seen
                        .parse::<i64>()
                        .ok()
                        .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
                    PresenceView::Offline { last_seen }
                }
            })
            .collect();
        Ok((views, flipped))
    }
}

/// For tests: fixed states; everyone else offline without `last_seen`. Can be made
/// unavailable, as Redis is when it fails.
#[derive(Default)]
pub struct FixedPresence {
    views: Mutex<HashMap<UserId, PresenceView>>,
    flips: Mutex<HashSet<UserId>>,
    unavailable: AtomicBool,
}

impl FixedPresence {
    /// Sets `user`'s state.
    pub fn set(&self, user: UserId, view: PresenceView) {
        self.views.lock().expect("presence lock").insert(user, view);
    }

    /// Makes the next read of `user` find them newly offline (as when their gateway died):
    /// the read answers offline and reports the flip, once.
    pub fn flip(&self, user: UserId) {
        self.set(user, PresenceView::Offline { last_seen: None });
        self.flips.lock().expect("presence lock").insert(user);
    }

    /// Makes every read fail with `PresenceUnavailable` (or succeed again).
    pub fn set_unavailable(&self, unavailable: bool) {
        self.unavailable.store(unavailable, Ordering::SeqCst);
    }
}

#[async_trait]
impl PresenceStates for FixedPresence {
    async fn states(
        &self,
        users: &[UserId],
    ) -> Result<(Vec<PresenceView>, Vec<UserId>), PresenceUnavailable> {
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(PresenceUnavailable);
        }
        let views = self.views.lock().expect("presence lock");
        let mut flips = self.flips.lock().expect("presence lock");
        let states = users
            .iter()
            .map(|user| {
                views
                    .get(user)
                    .copied()
                    .unwrap_or(PresenceView::Offline { last_seen: None })
            })
            .collect();
        let flipped = users
            .iter()
            .filter(|user| flips.remove(user))
            .copied()
            .collect();
        Ok((states, flipped))
    }
}

/// Everyone offline without `last_seen`: for tools and tests that do not track presence.
pub struct OfflinePresence;

#[async_trait]
impl PresenceStates for OfflinePresence {
    async fn states(
        &self,
        users: &[UserId],
    ) -> Result<(Vec<PresenceView>, Vec<UserId>), PresenceUnavailable> {
        Ok((offline(users.len()), Vec::new()))
    }
}

/// `count` users offline without `last_seen`.
pub(crate) fn offline(count: usize) -> Vec<PresenceView> {
    vec![PresenceView::Offline { last_seen: None }; count]
}

/// `user`'s current state, read for an announcement; `None` when it cannot be read, so
/// nothing is published (logged). A flip this read finds is `user` itself, which the
/// announcement publishes anyway.
async fn current(presence: &dyn PresenceStates, user: UserId) -> Option<PresenceView> {
    match presence.states(&[user]).await {
        Ok((views, _)) => views.first().copied(),
        Err(PresenceUnavailable) => {
            tracing::warn!(user = %user.0, "presence unavailable; publishing nothing");
            None
        }
    }
}

/// Announces `user`'s current state to their presence audience; nothing when the state
/// cannot be read (the next transition or read corrects it).
pub(crate) async fn announce<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    presence: &dyn PresenceStates,
    user: UserId,
) -> DomainResult<()> {
    messaging
        .announce_presence(user, || current(presence, user))
        .await
}

/// Switches `user`'s presence sharing; showing publishes their current state, unless it
/// cannot be read (the setting still changes).
pub(crate) async fn set_sharing<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    presence: &dyn PresenceStates,
    user: UserId,
    share: bool,
) -> DomainResult<bool> {
    messaging
        .set_presence_sharing(user, share, || current(presence, user))
        .await
}

/// Announces each of `users`, whom a read found newly offline. Best effort: a failure is
/// logged, and the next transition or read corrects it. Flips found by these
/// announcements' own reads are the announced users themselves, so nothing recurses.
pub(crate) async fn announce_flipped<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    presence: &dyn PresenceStates,
    users: &[UserId],
) {
    for &user in users {
        if let Err(error) = announce(messaging, presence, user).await {
            tracing::warn!(%error, user = %user.0, "could not announce a user found offline");
        }
    }
}
