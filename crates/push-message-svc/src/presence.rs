//! Reading the live presence of users, which the gateways record in Redis (spec: activity
//! and presence, "Redis presence state").

use std::collections::{HashMap, HashSet};
use std::future::Future;
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
/// `in_doubt` when the read may still have run on Redis (it was sent before it failed or
/// timed out): a flip it made is then lost unless the caller announces every user read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("presence is unavailable")]
pub struct PresenceUnavailable {
    pub in_doubt: bool,
}

/// One answered presence read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresenceRead {
    /// The state of each user read (same order): `Online` or `Offline { last_seen }`;
    /// never `Unknown`.
    pub views: Vec<PresenceView>,
    /// Users this read found newly offline (their gateway died without settling): the
    /// caller announces them.
    pub flipped: Vec<UserId>,
    /// The read may have run more than once (it was sent again after a dropped
    /// connection), so a flip found by a run whose answer was lost is missing from
    /// `flipped`: the caller announces every user read.
    pub in_doubt: bool,
}

/// The live presence of users, as the gateways recorded it in Redis.
#[async_trait]
pub trait PresenceStates: Send + Sync {
    /// Reads the state of each of `users`. When the state cannot be read,
    /// `PresenceUnavailable` (logged here).
    async fn states(&self, users: &[UserId]) -> Result<PresenceRead, PresenceUnavailable>;
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
    /// not have it, once. Records in `progress` how far it got, which survives a timeout.
    async fn read(
        &self,
        users: &[UserId],
        progress: &Progress,
    ) -> RedisResult<Vec<(i64, String, i64)>> {
        match self.read_loaded(users, progress).await {
            Err(error) if error.kind() == ErrorKind::Server(ServerErrorKind::NoScript) => {
                // No script ran: Redis refused every call of the pipeline.
                progress.sent.store(false, Ordering::SeqCst);
                self.sha.lock().expect("presence script lock").take();
                self.read_loaded(users, progress).await
            }
            result => result,
        }
    }

    async fn read_loaded(
        &self,
        users: &[UserId],
        progress: &Progress,
    ) -> RedisResult<Vec<(i64, String, i64)>> {
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
        // A script run twice prunes and flips nothing new, so sending again is harmless,
        // but a flip found by a first run whose answer was lost is not reported again: the
        // read is then in doubt.
        let pipeline = &pipeline;
        progress
            .send(|| async move { pipeline.query_async(&mut self.connection.clone()).await })
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

/// How far a presence read got; kept outside the read so a timeout cannot lose it.
#[derive(Debug, Default)]
struct Progress {
    /// The scripts were sent to Redis, so they may have run.
    sent: AtomicBool,
    /// They were sent again after a dropped connection, so they may have run twice.
    retried: AtomicBool,
}

impl Progress {
    /// Sends with `call`, and once more when the connection was dropped, recording both.
    async fn send<T, F, Fut>(&self, mut call: F) -> RedisResult<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = RedisResult<T>>,
    {
        self.sent.store(true, Ordering::SeqCst);
        match call().await {
            Err(error) if push_credentials::is_dropped_connection(&error) => {
                tracing::debug!(%error, "the Redis connection was closed; sending once more");
                self.retried.store(true, Ordering::SeqCst);
                call().await
            }
            result => result,
        }
    }

    /// Whether a read that failed with `error` (`None`: timed out) may have run anyway.
    /// A refused connection sent nothing.
    fn failed_in_doubt(&self, error: Option<&redis::RedisError>) -> bool {
        self.retried.load(Ordering::SeqCst)
            || (self.sent.load(Ordering::SeqCst)
                && !error.is_some_and(redis::RedisError::is_connection_refusal))
    }
}

#[async_trait]
impl PresenceStates for RedisPresence {
    async fn states(&self, users: &[UserId]) -> Result<PresenceRead, PresenceUnavailable> {
        if users.is_empty() {
            return Ok(PresenceRead::default());
        }
        let progress = Progress::default();
        let read = match tokio::time::timeout(READ_TIMEOUT, self.read(users, &progress)).await {
            Ok(Ok(read)) if read.len() == users.len() => Ok(read),
            Ok(Ok(read)) => Err((
                format!("{} answers for {} users", read.len(), users.len()),
                true,
            )),
            Ok(Err(error)) => Err((error.to_string(), progress.failed_in_doubt(Some(&error)))),
            Err(_) => Err(("timed out".to_owned(), progress.failed_in_doubt(None))),
        };
        let read = match read {
            Ok(read) => read,
            Err((error, in_doubt)) => {
                tracing::warn!(%error, count = users.len(), in_doubt, "could not read presence");
                return Err(PresenceUnavailable { in_doubt });
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
        Ok(PresenceRead {
            views,
            flipped,
            in_doubt: progress.retried.load(Ordering::SeqCst),
        })
    }
}

/// For tests: fixed states; everyone else offline without `last_seen`. Can be made
/// unavailable, as Redis is when it fails, at once or after some reads.
#[derive(Default)]
pub struct FixedPresence {
    views: Mutex<HashMap<UserId, PresenceView>>,
    flips: Mutex<HashSet<UserId>>,
    unavailable: AtomicBool,
    /// The next read answers, but in doubt.
    answer_in_doubt: AtomicBool,
    /// The next read fails in doubt.
    fail_in_doubt: AtomicBool,
    /// Reads left before every read fails; unlimited when `None`.
    reads_left: Mutex<Option<usize>>,
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

    /// Makes the next read answer in doubt, as when it was sent again after a dropped
    /// connection.
    pub fn answer_in_doubt(&self) {
        self.answer_in_doubt.store(true, Ordering::SeqCst);
    }

    /// Makes the next read fail in doubt, as when it timed out after it was sent.
    pub fn fail_in_doubt(&self) {
        self.fail_in_doubt.store(true, Ordering::SeqCst);
    }

    /// Lets the next `reads` reads succeed, then makes every read fail.
    pub fn fail_after(&self, reads: usize) {
        *self.reads_left.lock().expect("presence lock") = Some(reads);
    }
}

#[async_trait]
impl PresenceStates for FixedPresence {
    async fn states(&self, users: &[UserId]) -> Result<PresenceRead, PresenceUnavailable> {
        let unavailable = PresenceUnavailable { in_doubt: false };
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(unavailable);
        }
        if self.fail_in_doubt.swap(false, Ordering::SeqCst) {
            return Err(PresenceUnavailable { in_doubt: true });
        }
        if let Some(left) = self.reads_left.lock().expect("presence lock").as_mut() {
            *left = left.checked_sub(1).ok_or(unavailable)?;
        }
        let views = self.views.lock().expect("presence lock");
        let mut flips = self.flips.lock().expect("presence lock");
        let views = users
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
        Ok(PresenceRead {
            views,
            flipped,
            in_doubt: self.answer_in_doubt.swap(false, Ordering::SeqCst),
        })
    }
}

/// Everyone offline without `last_seen`: for tools and tests that do not track presence.
pub struct OfflinePresence;

#[async_trait]
impl PresenceStates for OfflinePresence {
    async fn states(&self, users: &[UserId]) -> Result<PresenceRead, PresenceUnavailable> {
        Ok(PresenceRead {
            views: offline(users.len()),
            ..PresenceRead::default()
        })
    }
}

/// `count` users offline without `last_seen`.
pub(crate) fn offline(count: usize) -> Vec<PresenceView> {
    vec![PresenceView::Offline { last_seen: None }; count]
}

/// `user`'s current state, read for an announcement; `None` when it cannot be read, so
/// nothing is published (logged). A flip this read finds, or a doubt about one, is about
/// `user` itself, whom the announcement publishes anyway.
async fn current(presence: &dyn PresenceStates, user: UserId) -> Option<PresenceView> {
    current_or(presence, user, None).await
}

/// `user`'s current state, read for an announcement; `fallback` when it cannot be read
/// (logged), so `None` publishes nothing.
async fn current_or(
    presence: &dyn PresenceStates,
    user: UserId,
    fallback: Option<PresenceView>,
) -> Option<PresenceView> {
    match presence.states(&[user]).await {
        Ok(read) => read.views.first().copied(),
        Err(PresenceUnavailable { .. }) if fallback.is_some() => {
            tracing::warn!(user = %user.0, "presence unavailable; publishing the earlier read");
            fallback
        }
        Err(PresenceUnavailable { .. }) => {
            tracing::warn!(user = %user.0, "presence unavailable; publishing nothing");
            None
        }
    }
}

/// Announces `user`'s current state to their presence audience; nothing when the state
/// cannot be read (`user`'s next connect or disconnect announces it again).
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

/// Announces each of `users`, whom a read found newly offline, with the view that read
/// returned. That read cleared their online mark, so no other reader will announce them:
/// each is re-read inside the announcement's lock (a fresher state, such as a reconnect's
/// `Online`, wins), but when the re-read fails the first read's view is published rather
/// than nothing. Best effort: a store failure is logged and leaves contacts with the old
/// state until `user` next connects. Flips found by these announcements' own reads are
/// the announced users themselves, so nothing recurses.
pub(crate) async fn announce_flipped<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    presence: &dyn PresenceStates,
    users: &[(UserId, PresenceView)],
) {
    for &(user, read) in users {
        let announced = messaging
            .announce_presence(user, || current_or(presence, user, Some(read)))
            .await;
        if let Err(error) = announced {
            tracing::warn!(%error, user = %user.0, "could not announce a user found offline");
        }
    }
}

/// Announces each of `users`, whom a read in doubt may have found newly offline: that read
/// may have cleared their online mark without reporting it, so no other reader would
/// announce them. Each is re-read inside the announcement's lock and its fresh state is
/// published; when a re-read fails nothing is published for it, and the rest are skipped
/// too (presence is unavailable, so their re-reads would only wait for the same failure).
/// Best effort: such a double fault leaves contacts with the old state until the user's
/// next transition or a read that finds them flipped.
pub(crate) async fn announce_in_doubt<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    presence: &dyn PresenceStates,
    users: &[UserId],
) {
    for (done, &user) in users.iter().enumerate() {
        let unread = AtomicBool::new(false);
        let unread_flag = &unread;
        let announced = messaging
            .announce_presence(user, || async move {
                let view = current(presence, user).await;
                unread_flag.store(view.is_none(), Ordering::SeqCst);
                view
            })
            .await;
        if let Err(error) = announced {
            tracing::warn!(%error, user = %user.0, "could not announce a user read in doubt");
        }
        if unread.load(Ordering::SeqCst) {
            let skipped = users.len() - done - 1;
            if skipped > 0 {
                tracing::warn!(
                    skipped,
                    "presence unavailable; not announcing the rest in doubt"
                );
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(sent: bool, retried: bool) -> Progress {
        Progress {
            sent: AtomicBool::new(sent),
            retried: AtomicBool::new(retried),
        }
    }

    fn dropped() -> redis::RedisError {
        redis::RedisError::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
    }

    #[tokio::test]
    async fn a_read_sent_again_after_a_dropped_connection_is_in_doubt() {
        let answered = Progress::default();
        assert_eq!(answered.send(|| async { Ok(1) }).await, Ok(1));
        assert!(!answered.retried.load(Ordering::SeqCst));

        let resent = Progress::default();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let sent = resent
            .send(|| async {
                match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => Err(dropped()),
                    _ => Ok(2),
                }
            })
            .await;
        assert_eq!(sent, Ok(2));
        assert!(resent.retried.load(Ordering::SeqCst));
        assert!(resent.failed_in_doubt(None));
    }

    #[test]
    fn a_failed_read_is_in_doubt_only_when_it_may_have_run() {
        let reset = dropped();
        let refused =
            redis::RedisError::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert!(refused.is_connection_refusal());

        assert!(!progress(false, false).failed_in_doubt(None), "never sent");
        assert!(!progress(false, false).failed_in_doubt(Some(&reset)));
        assert!(
            progress(true, false).failed_in_doubt(None),
            "timed out after sending"
        );
        assert!(progress(true, false).failed_in_doubt(Some(&reset)));
        assert!(!progress(true, false).failed_in_doubt(Some(&refused)));
        assert!(
            progress(true, true).failed_in_doubt(Some(&refused)),
            "a first send was lost"
        );
    }
}
