//! Which connected devices to wake when Redis rings a user's channel.
//!
//! Every identified session joins the hub under its user. The first session of a user
//! makes the subscriber subscribe to `notify:{user_id}`; the last one to leave makes it
//! unsubscribe. A ring wakes every session of the user; rings coalesce, so a session that
//! is busy syncing syncs once more afterwards, not once per ring.
//!
//! Redis is a doorbell, not the truth (spec §2): whenever the subscription is
//! re-established after a break, every session is rung, since notifications may have been
//! lost meanwhile.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use prost::Message;
use push_proto::internal::v1::{Notification, notification::Kind};
use push_proto::notify_channel;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How long a joining session waits for its user's subscription to become active.
const SUBSCRIBE_WAIT: Duration = Duration::from_secs(2);
/// How long a subscribe or unsubscribe may take before the connection is given up on.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait before reconnecting to Redis.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// The sessions of this gateway, by user, and the Redis subscriber that wakes them.
#[derive(Clone)]
pub struct Hub {
    members: Arc<Mutex<Members>>,
    commands: mpsc::UnboundedSender<Command>,
    connected: Arc<AtomicBool>,
}

#[derive(Default)]
struct Members {
    next_key: u64,
    users: HashMap<Uuid, HashMap<u64, mpsc::Sender<()>>>,
}

enum Command {
    Subscribe(Uuid, oneshot::Sender<()>),
    Unsubscribe(Uuid),
}

impl Hub {
    /// A hub whose subscriber task listens on Redis through `client` until `shutdown`.
    pub fn start(client: redis::Client, shutdown: CancellationToken) -> (Self, JoinHandle<()>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let hub = Self {
            members: Arc::default(),
            commands,
            connected: Arc::default(),
        };
        let subscriber = tokio::spawn(subscribe(client, hub.clone(), receiver, shutdown));
        (hub, subscriber)
    }

    /// Whether the subscriber is connected to Redis.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Adds a session of `user`. Waits up to 2 s for the user's subscription to become
    /// active, then returns anyway: the session syncs after joining, which covers
    /// whatever a late subscription misses.
    pub async fn join(&self, user: Uuid) -> Membership {
        let (doorbell, rung) = mpsc::channel(1);
        let (key, subscribed) = {
            let mut members = lock(&self.members);
            let key = members.next_key;
            members.next_key += 1;
            let sessions = members.users.entry(user).or_default();
            let first = sessions.is_empty();
            sessions.insert(key, doorbell);
            // Sent under the lock, so subscribe and unsubscribe commands for a user reach
            // the subscriber in the order the sessions joined and left.
            let subscribed = first.then(|| {
                let (done, subscribed) = oneshot::channel();
                let _ = self.commands.send(Command::Subscribe(user, done));
                subscribed
            });
            (key, subscribed)
        };
        if let Some(subscribed) = subscribed {
            let _ = tokio::time::timeout(SUBSCRIBE_WAIT, subscribed).await;
        }
        Membership {
            user,
            key,
            rung,
            hub: self.clone(),
        }
    }

    /// Wakes every session of `user`.
    fn ring(&self, user: Uuid) {
        if let Some(sessions) = lock(&self.members).users.get(&user) {
            for doorbell in sessions.values() {
                // A full doorbell has already been rung and not answered yet.
                let _ = doorbell.try_send(());
            }
        }
    }

    /// Wakes every session.
    fn ring_all(&self) {
        for sessions in lock(&self.members).users.values() {
            for doorbell in sessions.values() {
                let _ = doorbell.try_send(());
            }
        }
    }

    /// The users with at least one session.
    fn users(&self) -> Vec<Uuid> {
        lock(&self.members).users.keys().copied().collect()
    }

    /// Acts on a message from Redis: `EntriesAvailable` on `notify:{user_id}` rings the
    /// user's sessions. Other notifications reach clients in a later plan (receipts,
    /// contact changes, read positions); malformed ones are logged and dropped.
    fn dispatch(&self, channel: &str, payload: &[u8]) {
        let Some(user) = channel
            .strip_prefix("notify:")
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            tracing::warn!(channel, "message on an unexpected channel");
            return;
        };
        match Notification::decode(payload) {
            Ok(Notification {
                kind: Some(Kind::EntriesAvailable(_)),
            }) => self.ring(user),
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, channel, "malformed notification"),
        }
    }
}

fn lock(members: &Mutex<Members>) -> MutexGuard<'_, Members> {
    members.lock().expect("hub lock poisoned")
}

/// One session's place in the hub. Dropping it leaves.
pub struct Membership {
    user: Uuid,
    key: u64,
    rung: mpsc::Receiver<()>,
    hub: Hub,
}

impl Membership {
    /// Waits until the session's user is rung.
    pub async fn rung(&mut self) {
        if self.rung.recv().await.is_none() {
            // The hub keeps the sender while this membership exists; never reached.
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        let mut members = lock(&self.hub.members);
        if let Some(sessions) = members.users.get_mut(&self.user) {
            sessions.remove(&self.key);
            if sessions.is_empty() {
                members.users.remove(&self.user);
                let _ = self.hub.commands.send(Command::Unsubscribe(self.user));
            }
        }
    }
}

/// Runs a Redis command with a time limit. `None` when `shutdown` came first; otherwise
/// whether the command succeeded, a timeout counting as a failure.
async fn bounded(
    shutdown: &CancellationToken,
    command: impl Future<Output = redis::RedisResult<()>>,
) -> Option<bool> {
    tokio::select! {
        () = shutdown.cancelled() => None,
        result = tokio::time::timeout(COMMAND_TIMEOUT, command) => match result {
            Ok(result) => Some(result.is_ok()),
            Err(_) => {
                tracing::warn!("Redis did not answer a subscription command in time");
                Some(false)
            }
        },
    }
}

/// The subscriber task: keeps one Redis pub/sub connection, follows the hub's subscribe
/// and unsubscribe commands, and dispatches what arrives. Reconnects after a break,
/// subscribes again to every user with a session and rings them all. Connecting is
/// retried while Redis is unavailable, and `shutdown` interrupts both the connection
/// attempt and the wait between attempts at once.
async fn subscribe(
    client: redis::Client,
    hub: Hub,
    mut commands: mpsc::UnboundedReceiver<Command>,
    shutdown: CancellationToken,
) {
    loop {
        let connection = tokio::select! {
            () = shutdown.cancelled() => return,
            connection = client.get_async_pubsub() => connection,
        };
        match connection {
            Ok(pubsub) => {
                let (mut sink, mut messages) = pubsub.split();
                let mut resubscribed = true;
                for user in hub.users() {
                    match bounded(&shutdown, sink.subscribe(notify_channel(user))).await {
                        None => return,
                        Some(true) => {}
                        Some(false) => {
                            resubscribed = false;
                            break;
                        }
                    }
                }
                if resubscribed {
                    hub.connected.store(true, Ordering::SeqCst);
                    tracing::info!("subscribed to Redis");
                    hub.ring_all();
                    loop {
                        tokio::select! {
                            () = shutdown.cancelled() => return,
                            command = commands.recv() => {
                                let succeeded = match command {
                                    None => return,
                                    Some(Command::Subscribe(user, done)) => {
                                        let Some(succeeded) =
                                            bounded(&shutdown, sink.subscribe(notify_channel(user))).await
                                        else {
                                            return;
                                        };
                                        // Sent even when the subscribe failed: the reconnect path
                                        // subscribes every user again and calls `ring_all`, so the
                                        // joining session does not need to wait any longer.
                                        let _ = done.send(());
                                        succeeded
                                    }
                                    Some(Command::Unsubscribe(user)) => {
                                        let Some(succeeded) =
                                            bounded(&shutdown, sink.unsubscribe(notify_channel(user))).await
                                        else {
                                            return;
                                        };
                                        succeeded
                                    }
                                };
                                if !succeeded {
                                    break;
                                }
                            }
                            message = messages.next() => match message {
                                None => break,
                                Some(message) => {
                                    hub.dispatch(message.get_channel_name(), message.get_payload_bytes());
                                }
                            },
                        }
                    }
                }
                hub.connected.store(false, Ordering::SeqCst);
                tracing::warn!("lost the Redis subscription; reconnecting");
            }
            Err(error) => tracing::warn!(%error, "cannot reach Redis; retrying"),
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(RECONNECT_DELAY) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use push_proto::internal::v1::{EntriesAvailable, ReadPositionChanged};

    use super::*;

    /// A hub without a subscriber task, for the bookkeeping alone.
    fn hub() -> (Hub, mpsc::UnboundedReceiver<Command>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let hub = Hub {
            members: Arc::default(),
            commands,
            connected: Arc::default(),
        };
        (hub, receiver)
    }

    fn entries_available() -> Vec<u8> {
        Notification {
            kind: Some(Kind::EntriesAvailable(EntriesAvailable {})),
        }
        .encode_to_vec()
    }

    fn rang(membership: &mut Membership) -> bool {
        membership.rung.try_recv().is_ok()
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_session_subscribes_and_the_last_unsubscribes() {
        let (hub, mut commands) = hub();
        let user = Uuid::now_v7();
        let first = hub.join(user).await;
        assert!(matches!(commands.try_recv(), Ok(Command::Subscribe(u, _)) if u == user));
        let second = hub.join(user).await;
        assert!(commands.try_recv().is_err(), "already subscribed");
        drop(first);
        assert!(commands.try_recv().is_err(), "a session is still there");
        drop(second);
        assert!(matches!(commands.try_recv(), Ok(Command::Unsubscribe(u)) if u == user));
        assert!(hub.users().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn entries_available_rings_only_that_users_sessions() {
        let (hub, _commands) = hub();
        let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
        let (mut alice_phone, mut alice_laptop) = (hub.join(alice).await, hub.join(alice).await);
        let mut bob_phone = hub.join(bob).await;
        hub.dispatch(&notify_channel(alice), &entries_available());
        assert!(rang(&mut alice_phone));
        assert!(rang(&mut alice_laptop));
        assert!(!rang(&mut bob_phone));
    }

    #[tokio::test(start_paused = true)]
    async fn rings_coalesce_until_answered() {
        let (hub, _commands) = hub();
        let user = Uuid::now_v7();
        let mut session = hub.join(user).await;
        for _ in 0..3 {
            hub.dispatch(&notify_channel(user), &entries_available());
        }
        assert!(rang(&mut session));
        assert!(!rang(&mut session));
    }

    #[tokio::test(start_paused = true)]
    async fn other_notifications_and_strange_channels_ring_nobody() {
        let (hub, _commands) = hub();
        let user = Uuid::now_v7();
        let mut session = hub.join(user).await;
        let read = Notification {
            kind: Some(Kind::ReadPositionChanged(ReadPositionChanged::default())),
        };
        hub.dispatch(&notify_channel(user), &read.encode_to_vec());
        hub.dispatch("notify:not-a-uuid", &entries_available());
        hub.dispatch(&format!("other:{user}"), &entries_available());
        hub.dispatch(&notify_channel(user), &[0xff, 0xff]);
        assert!(!rang(&mut session));
    }

    #[tokio::test(start_paused = true)]
    async fn ring_all_wakes_every_session() {
        let (hub, _commands) = hub();
        let mut a = hub.join(Uuid::now_v7()).await;
        let mut b = hub.join(Uuid::now_v7()).await;
        hub.ring_all();
        assert!(rang(&mut a));
        assert!(rang(&mut b));
    }
}
