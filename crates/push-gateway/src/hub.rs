//! What Redis tells the connections of this gateway: which to sync, what to forward, and
//! which to close.
//!
//! Every connection joins the hub under its device as soon as its token passed, before
//! message-svc is asked about the device: the first connection of a device makes the
//! subscriber subscribe to `kick:{device_id}`, and every connection waits (up to 2 s)
//! for that subscription to be confirmed, so a revocation or password reset that
//! message-svc commits after admitting the device cannot be missed. Once admitted, the
//! connection joins its user too (`notify:{user_id}`). The last connection to leave a
//! channel unsubscribes it.
//!
//! On `notify:{user_id}`, `EntriesAvailable` rings every connection of the user (rings
//! coalesce: a busy connection syncs once more afterwards, not once per ring), and receipts,
//! read positions and contact changes are forwarded to each as server frames. On
//! `kick:{device_id}`, the connections of the device are told to close, with two
//! exceptions:
//!
//! - a takeover closes only connections older than the newcomer it names. Connection ids
//!   are UUIDv7s taken when a connection starts watching its device, so they sort by time:
//!   an abandoned slow `Identify` that publishes its takeover after a newer connection did
//!   closes nobody newer;
//! - a password-reset kick spares a connection whose token was issued after the reset's
//!   second (the time auth-svc reset the password): it signed in after the reset, and a
//!   redelivered or late kick must not end it. A kick without a time closes them all.
//!
//! Redis is a doorbell, not the truth (spec §2): whenever the subscription is
//! re-established after a break, every connection is rung, since notifications may have
//! been lost meanwhile. A kick lost that way is caught when the connection renews its
//! token (`ReAuth`, within 30 minutes): message-svc refuses a revoked device and a token
//! older than a password reset.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use prost::Message;
use push_credentials::{AccessToken, RedisConnector};
use push_proto::internal::v1::{
    ContactChanged, Kick, KickReason, Notification, ReadPositionChanged, ReceiptChanged,
    notification::Kind, receipt_changed,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, ContactEvent, GroupReceipts, ReadPosition, Receipt, receipt};
use push_proto::{id, kick_channel, notify_channel};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How long a joining connection waits for its channel's subscription to become active.
const SUBSCRIBE_WAIT: Duration = Duration::from_secs(2);
/// How long a subscribe or unsubscribe may take before the connection is given up on.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait before reconnecting to Redis.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// How often the subscriber pings Redis: a connection that died without a word (a
/// half-open TCP connection) would otherwise look idle forever.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// How long a replaced subscriber keeps being read after the swap: what Redis sent it
/// before the new one was subscribed is still delivered (duplicates are harmless: rings
/// coalesce, kicks close once, frames carry state).
const RETIRE_GRACE: Duration = Duration::from_secs(2);
/// Server frames waiting for a busy connection; more are dropped (the client recovers
/// them with `ListConversations` and `ListContacts`).
const EVENT_BACKLOG: usize = 64;

/// The connections of this gateway, by device and user, and the Redis subscriber that
/// wakes them.
#[derive(Clone)]
pub struct Hub {
    members: Arc<Mutex<Members>>,
    commands: mpsc::UnboundedSender<Command>,
    connected: Arc<AtomicBool>,
}

#[derive(Default)]
struct Members {
    next_key: u64,
    by_key: HashMap<u64, Member>,
    /// Each channel, `notify:{user}` or `kick:{device}`, with at least one member.
    channels: HashMap<String, Channel>,
}

struct Channel {
    members: HashSet<u64>,
    /// Whether Redis is subscribed to the channel right now. Joiners wait for it; the
    /// subscriber sets it after subscribing and clears it when the connection breaks.
    active: watch::Sender<bool>,
}

struct Member {
    /// Connection id: a takeover names the newcomer by it.
    connection: Uuid,
    /// The `iat` of the connection's current token, in seconds since the Unix epoch.
    issued_at: i64,
    doorbell: mpsc::Sender<()>,
    events: mpsc::Sender<Body>,
    kicks: mpsc::Sender<KickReason>,
}

enum Command {
    Subscribe(String),
    Unsubscribe(String),
}

/// What a connection must do.
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    /// New entries may be waiting: sync.
    Ring,
    /// Send this server frame.
    Frame(Box<Body>),
    /// Close: the device was revoked, the password reset, or another connection took
    /// over.
    Kick(KickReason),
}

impl Hub {
    /// A hub whose subscriber task listens on Redis through `redis` until `shutdown`. With
    /// Entra ID tokens the subscriber is replaced after each new token, without a gap.
    pub fn start(redis: RedisConnector, shutdown: CancellationToken) -> (Self, JoinHandle<()>) {
        Self::start_with(redis, shutdown, PING_INTERVAL)
    }

    fn start_with(
        redis: RedisConnector,
        shutdown: CancellationToken,
        ping_interval: Duration,
    ) -> (Self, JoinHandle<()>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let hub = Self {
            members: Arc::default(),
            commands,
            connected: Arc::default(),
        };
        let subscriber = tokio::spawn(subscribe(
            redis,
            hub.clone(),
            receiver,
            shutdown,
            ping_interval,
        ));
        (hub, subscriber)
    }

    /// Whether the subscriber is connected to Redis.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Adds connection `connection` of `device`, whose token was issued at `issued_at`, to
    /// be kicked through `kick:{device}`. Waits up to 2 s for the subscription to become
    /// active, then returns anyway: without Redis nothing would be heard in time, and the
    /// connection's next `ReAuth` catches what a kick would have said.
    pub async fn watch_device(&self, device: Uuid, connection: Uuid, issued_at: i64) -> Membership {
        let (doorbell, rung) = mpsc::channel(1);
        let (events, frames) = mpsc::channel(EVENT_BACKLOG);
        let (kicks, kicked) = mpsc::channel(1);
        let member = Member {
            connection,
            issued_at,
            doorbell,
            events,
            kicks,
        };
        let (key, active) = {
            let mut members = lock(&self.members);
            let key = members.next_key;
            members.next_key += 1;
            members.by_key.insert(key, member);
            (key, self.enter(&mut members, kick_channel(device), key))
        };
        wait(active).await;
        Membership {
            key,
            connection,
            issued_at,
            channels: vec![kick_channel(device)],
            rung,
            frames,
            kicked,
            hub: self.clone(),
        }
    }

    /// Adds `key` to `channel`, asking the subscriber to subscribe if it is the first.
    /// Returns the channel's active signal, which every joiner waits on, not only the
    /// first: a later one must not be admitted while the subscription is pending or being
    /// restored after a break. Commands are sent under the lock, so the commands for a
    /// channel reach the subscriber in the order its members joined and left.
    fn enter(&self, members: &mut Members, channel: String, key: u64) -> watch::Receiver<bool> {
        let entry = members.channels.entry(channel.clone());
        let first = matches!(entry, std::collections::hash_map::Entry::Vacant(_));
        let joined = entry.or_insert_with(|| Channel {
            members: HashSet::new(),
            active: watch::channel(false).0,
        });
        joined.members.insert(key);
        let active = joined.active.subscribe();
        if first {
            let _ = self.commands.send(Command::Subscribe(channel));
        }
        active
    }

    /// The members of `channel`.
    fn members_of<'a>(members: &'a Members, channel: &str) -> impl Iterator<Item = &'a Member> {
        members
            .channels
            .get(channel)
            .into_iter()
            .flat_map(|channel| &channel.members)
            .filter_map(|key| members.by_key.get(key))
    }

    /// Wakes every connection on `channel`.
    fn ring(&self, channel: &str) {
        let members = lock(&self.members);
        for member in Self::members_of(&members, channel) {
            // A full doorbell has already been rung and not answered yet.
            let _ = member.doorbell.try_send(());
        }
    }

    /// Hands a server frame to every connection on `channel`.
    fn forward(&self, channel: &str, frame: &Body) {
        let members = lock(&self.members);
        for member in Self::members_of(&members, channel) {
            if member.events.try_send(frame.clone()).is_err() {
                tracing::warn!("a connection is not keeping up; a server frame was dropped");
            }
        }
    }

    /// Tells the connections on `channel` to close: for a takeover only those older than
    /// the newcomer, for a password reset only those whose token is not newer than it.
    fn kick(&self, channel: &str, kick: &Kick) {
        let members = lock(&self.members);
        let newcomer = id(&kick.connection_id).ok();
        for member in Self::members_of(&members, channel) {
            let closes = match kick.reason() {
                KickReason::TakenOver => newcomer.is_some_and(|newer| member.connection < newer),
                KickReason::PasswordReset => {
                    kick.reset_at == 0 || member.issued_at <= kick.reset_at
                }
                _ => true,
            };
            if closes {
                let _ = member.kicks.try_send(kick.reason());
            }
        }
    }

    /// Wakes every connection.
    fn ring_all(&self) {
        for member in lock(&self.members).by_key.values() {
            let _ = member.doorbell.try_send(());
        }
    }

    /// The channels with at least one member.
    fn channels(&self) -> Vec<String> {
        lock(&self.members).channels.keys().cloned().collect()
    }

    /// Marks `channels` as subscribed (or not) in Redis. Channels nobody listens to any
    /// more are skipped.
    fn set_active<'a>(&self, channels: impl IntoIterator<Item = &'a String>, active: bool) {
        let members = lock(&self.members);
        for channel in channels {
            if let Some(channel) = members.channels.get(channel) {
                channel.active.send_replace(active);
            }
        }
    }

    /// Marks every channel as not subscribed: the Redis connection broke.
    fn deactivate_all(&self) {
        for channel in lock(&self.members).channels.values() {
            channel.active.send_replace(false);
        }
    }

    /// Acts on a message from Redis. Malformed ones are logged and dropped.
    fn dispatch(&self, channel: &str, payload: &[u8]) {
        if let Some(rest) = channel.strip_prefix("notify:") {
            if Uuid::parse_str(rest).is_err() {
                tracing::warn!(channel, "message on an unexpected channel");
                return;
            }
            match Notification::decode(payload) {
                Ok(Notification {
                    kind: Some(Kind::EntriesAvailable(_)),
                }) => self.ring(channel),
                Ok(Notification { kind: Some(kind) }) => {
                    if let Some(frame) = server_frame(kind) {
                        self.forward(channel, &frame);
                    }
                }
                Ok(Notification { kind: None }) => {}
                Err(error) => tracing::warn!(%error, channel, "malformed notification"),
            }
        } else if let Some(rest) = channel.strip_prefix("kick:") {
            if Uuid::parse_str(rest).is_err() {
                tracing::warn!(channel, "message on an unexpected channel");
                return;
            }
            match Kick::decode(payload) {
                Ok(kick) if kick.reason() != KickReason::Unspecified => self.kick(channel, &kick),
                Ok(_) => tracing::warn!(channel, "a kick without a reason"),
                Err(error) => tracing::warn!(%error, channel, "malformed kick"),
            }
        } else {
            tracing::warn!(channel, "message on an unexpected channel");
        }
    }
}

/// The server frame a notification becomes, if any.
fn server_frame(kind: Kind) -> Option<Body> {
    Some(match kind {
        Kind::EntriesAvailable(_) => return None,
        Kind::ReceiptChanged(ReceiptChanged {
            conversation_id,
            positions,
        }) => {
            let positions = match positions? {
                receipt_changed::Positions::Direct(direct) => {
                    receipt::Positions::Direct(v1::DirectReceipt {
                        user_id: direct.user_id,
                        delivered_seq: direct.delivered_seq,
                        read_seq: direct.read_seq,
                    })
                }
                receipt_changed::Positions::Group(group) => {
                    receipt::Positions::Group(GroupReceipts {
                        all_delivered_seq: group.all_delivered_seq,
                        all_read_seq: group.all_read_seq,
                    })
                }
            };
            Body::Receipt(Receipt {
                conversation_id,
                positions: Some(positions),
            })
        }
        Kind::ContactChanged(ContactChanged { peer_id, state }) => {
            Body::ContactEvent(ContactEvent {
                user_id: peer_id,
                state,
            })
        }
        Kind::ReadPositionChanged(ReadPositionChanged {
            conversation_id,
            read_seq,
        }) => Body::ReadPosition(ReadPosition {
            conversation_id,
            read_seq,
        }),
    })
}

fn lock(members: &Mutex<Members>) -> MutexGuard<'_, Members> {
    members.lock().expect("hub lock poisoned")
}

/// Waits up to [`SUBSCRIBE_WAIT`] for a channel's subscription to become active.
async fn wait(mut active: watch::Receiver<bool>) {
    let _ = tokio::time::timeout(SUBSCRIBE_WAIT, async {
        let _ = active.wait_for(|active| *active).await;
    })
    .await;
}

/// One connection's place in the hub. Dropping it leaves every channel it joined.
pub struct Membership {
    key: u64,
    connection: Uuid,
    issued_at: i64,
    channels: Vec<String>,
    rung: mpsc::Receiver<()>,
    frames: mpsc::Receiver<Body>,
    kicked: mpsc::Receiver<KickReason>,
    hub: Hub,
}

impl Membership {
    /// The connection's id, which its takeover names.
    pub fn connection(&self) -> Uuid {
        self.connection
    }

    /// The connection renewed its token (`ReAuth`) with one issued at `issued_at`: a
    /// password-reset kick compares that one.
    pub fn renewed(&mut self, issued_at: i64) {
        if issued_at == self.issued_at {
            return;
        }
        self.issued_at = issued_at;
        if let Some(member) = lock(&self.hub.members).by_key.get_mut(&self.key) {
            member.issued_at = issued_at;
        }
    }

    /// Joins the connection's user, `notify:{user}`, once message-svc admitted the device.
    /// Waits up to 2 s for the subscription; the connection syncs after joining, which
    /// covers whatever a late subscription misses.
    pub async fn join_user(&mut self, user: Uuid) {
        let channel = notify_channel(user);
        let active = {
            let mut members = lock(&self.hub.members);
            self.hub.enter(&mut members, channel.clone(), self.key)
        };
        self.channels.push(channel);
        wait(active).await;
    }

    /// The next thing to do: a kick first, then a server frame, then a ring.
    pub async fn next(&mut self) -> Signal {
        tokio::select! {
            biased;
            Some(reason) = self.kicked.recv() => Signal::Kick(reason),
            Some(frame) = self.frames.recv() => Signal::Frame(Box::new(frame)),
            Some(()) = self.rung.recv() => Signal::Ring,
            // The hub keeps the senders while this membership exists; never reached.
            else => std::future::pending().await,
        }
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        let mut members = lock(&self.hub.members);
        members.by_key.remove(&self.key);
        for channel in &self.channels {
            if let Some(entry) = members.channels.get_mut(channel) {
                entry.members.remove(&self.key);
                if entry.members.is_empty() {
                    members.channels.remove(channel);
                    let _ = self
                        .hub
                        .commands
                        .send(Command::Unsubscribe(channel.clone()));
                }
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
                tracing::warn!("Redis did not answer the subscriber in time");
                Some(false)
            }
        },
    }
}

/// Resolves when the Redis token changed; never in password mode.
async fn rotated(rotations: &mut Option<watch::Receiver<AccessToken>>) {
    match rotations {
        Some(receiver) => {
            if receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

type Subscriber = (redis::aio::PubSubSink, redis::aio::PubSubStream);

/// A new subscriber, signed in with the current token and subscribed to the hub's
/// channels at this moment; `None` when it failed or `shutdown` came. The caller
/// reconciles the channels again before swapping.
async fn prepare(
    redis: RedisConnector,
    hub: Hub,
    shutdown: CancellationToken,
) -> Option<Subscriber> {
    let connect = tokio::select! {
        () = shutdown.cancelled() => return None,
        connect = tokio::time::timeout(COMMAND_TIMEOUT, redis.pubsub()) => connect,
    };
    let (mut sink, messages) = connect.ok()?.ok()?.split();
    for channel in hub.channels() {
        if bounded(&shutdown, sink.subscribe(&channel)).await != Some(true) {
            return None;
        }
    }
    Some((sink, messages))
}

/// The subscriber task: keeps one Redis pub/sub connection, follows the hub's subscribe
/// and unsubscribe commands, and dispatches what arrives. Pings Redis every
/// `ping_interval` and reconnects when it does not answer within 5 s, as after any break:
/// it subscribes again to every channel with a member and rings every connection.
/// Connecting is retried while Redis is unavailable, and `shutdown` interrupts both the
/// connection attempt and the wait between attempts at once.
///
/// With Entra ID tokens a subscribed connection cannot take a new token, so after each
/// rotation a replacement is prepared while the current subscriber keeps serving, and
/// swapped in once it holds every channel the hub has (make-before-break). A replacement
/// that fails keeps the current subscriber; when Azure closes it at token expiry, the
/// reconnect below takes over with the newest token.
async fn subscribe(
    redis: RedisConnector,
    hub: Hub,
    mut commands: mpsc::UnboundedReceiver<Command>,
    shutdown: CancellationToken,
    ping_interval: Duration,
) {
    let mut rotations = redis.rotations();
    loop {
        let connection = tokio::select! {
            () = shutdown.cancelled() => return,
            connection = redis.pubsub() => connection,
        };
        match connection {
            Ok(pubsub) => {
                let (mut sink, mut messages) = pubsub.split();
                // Commands queued while Redis was away (a join and a leave of the same
                // channel, over and over) are stale: the hub's channels are the truth.
                // Commands that arrive after this drain but before the snapshot are
                // harmless: they are replayed in order and subscribing or unsubscribing
                // twice changes nothing.
                while commands.try_recv().is_ok() {}
                let wanted = hub.channels();
                let mut resubscribed = true;
                for channel in &wanted {
                    match bounded(&shutdown, sink.subscribe(&channel)).await {
                        None => return,
                        Some(true) => {}
                        Some(false) => {
                            resubscribed = false;
                            break;
                        }
                    }
                }
                if resubscribed {
                    hub.set_active(&wanted, true);
                    hub.connected.store(true, Ordering::SeqCst);
                    tracing::info!("subscribed to Redis");
                    hub.ring_all();
                    // This connection signed in with the newest token: only later ones rotate.
                    if let Some(receiver) = rotations.as_mut() {
                        receiver.borrow_and_update();
                    }
                    let mut replacement: Option<
                        Pin<Box<dyn Future<Output = Option<Subscriber>> + Send>>,
                    > = None;
                    let mut retiring: Option<Subscriber> = None;
                    let mut retire_at = tokio::time::Instant::now();
                    let mut ping_at = tokio::time::Instant::now() + ping_interval;
                    loop {
                        tokio::select! {
                            () = shutdown.cancelled() => return,
                            () = rotated(&mut rotations), if replacement.is_none() => {
                                // The new token cannot reach a subscribed connection: prepare
                                // a new subscriber while this one keeps serving.
                                replacement = Some(Box::pin(prepare(
                                    redis.clone(),
                                    hub.clone(),
                                    shutdown.clone(),
                                )));
                            }
                            prepared = async { replacement.as_mut().expect("guarded").await }, if replacement.is_some() => {
                                replacement = None;
                                match prepared {
                                    Some((mut new_sink, new_messages)) => {
                                        // Channels joined while it was prepared: make the new
                                        // subscriber match the hub now (subscribing twice is
                                        // harmless).
                                        let mut complete = true;
                                        for channel in hub.channels() {
                                            match bounded(&shutdown, new_sink.subscribe(&channel)).await {
                                                None => return,
                                                Some(true) => {}
                                                Some(false) => {
                                                    complete = false;
                                                    break;
                                                }
                                            }
                                        }
                                        if complete {
                                            let old_sink = std::mem::replace(&mut sink, new_sink);
                                            let old_messages = std::mem::replace(&mut messages, new_messages);
                                            retiring = Some((old_sink, old_messages));
                                            retire_at = tokio::time::Instant::now() + RETIRE_GRACE;
                                            ping_at = tokio::time::Instant::now() + ping_interval;
                                            tracing::info!("replaced the Redis subscriber for a new token");
                                        } else {
                                            tracing::warn!("could not complete the new Redis subscriber; keeping the current one");
                                        }
                                    }
                                    None => tracing::warn!("could not prepare a new Redis subscriber; keeping the current one"),
                                }
                            }
                            retired = async { retiring.as_mut().expect("guarded").1.next().await }, if retiring.is_some() => match retired {
                                None => retiring = None,
                                Some(message) => {
                                    hub.dispatch(message.get_channel_name(), message.get_payload_bytes());
                                }
                            },
                            () = tokio::time::sleep_until(retire_at), if retiring.is_some() => {
                                retiring = None;
                            }
                            () = tokio::time::sleep_until(ping_at) => {
                                let ping = async { sink.ping::<redis::Value>().await.map(|_| ()) };
                                let Some(alive) = bounded(&shutdown, ping).await else {
                                    return;
                                };
                                if !alive {
                                    break;
                                }
                                ping_at = tokio::time::Instant::now() + ping_interval;
                            }
                            command = commands.recv() => {
                                let succeeded = match command {
                                    None => return,
                                    Some(Command::Subscribe(channel)) => {
                                        let Some(succeeded) =
                                            bounded(&shutdown, sink.subscribe(&channel)).await
                                        else {
                                            return;
                                        };
                                        if succeeded {
                                            hub.set_active([&channel], true);
                                        }
                                        succeeded
                                    }
                                    Some(Command::Unsubscribe(channel)) => {
                                        let Some(succeeded) =
                                            bounded(&shutdown, sink.unsubscribe(&channel)).await
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
                hub.deactivate_all();
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
    use push_proto::id_bytes;
    use push_proto::internal::v1::{DirectReceipt, EntriesAvailable};

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

    /// A connection of `user` on a new device.
    async fn connection(hub: &Hub, user: Uuid) -> (Membership, Uuid, Uuid) {
        let (device, id) = (Uuid::now_v7(), Uuid::now_v7());
        let mut membership = hub.watch_device(device, id, 0).await;
        membership.join_user(user).await;
        (membership, device, id)
    }

    fn pending(membership: &mut Membership) -> Option<Signal> {
        if let Ok(reason) = membership.kicked.try_recv() {
            return Some(Signal::Kick(reason));
        }
        if let Ok(frame) = membership.frames.try_recv() {
            return Some(Signal::Frame(Box::new(frame)));
        }
        membership.rung.try_recv().ok().map(|()| Signal::Ring)
    }

    fn subscribed(commands: &mut mpsc::UnboundedReceiver<Command>) -> Vec<String> {
        let mut channels = Vec::new();
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Subscribe(channel) => channels.push(format!("+{channel}")),
                Command::Unsubscribe(channel) => channels.push(format!("-{channel}")),
            }
        }
        channels
    }

    #[tokio::test(start_paused = true)]
    async fn the_device_is_watched_before_the_user_and_the_last_to_leave_unsubscribes() {
        let (hub, mut commands) = hub();
        let user = Uuid::now_v7();
        let (first, device, _) = connection(&hub, user).await;
        assert_eq!(
            subscribed(&mut commands),
            [
                format!("+{}", kick_channel(device)),
                format!("+{}", notify_channel(user))
            ]
        );
        let (second, other_device, _) = connection(&hub, user).await;
        assert_eq!(
            subscribed(&mut commands),
            [format!("+{}", kick_channel(other_device))],
            "the user is already subscribed"
        );
        drop(first);
        assert_eq!(
            subscribed(&mut commands),
            [format!("-{}", kick_channel(device))]
        );
        drop(second);
        let mut left = subscribed(&mut commands);
        left.sort();
        let mut expected = vec![
            format!("-{}", kick_channel(other_device)),
            format!("-{}", notify_channel(user)),
        ];
        expected.sort();
        assert_eq!(left, expected);
        assert!(hub.channels().is_empty());
    }

    /// Starts `watch_device` in a task and lets it run until it waits.
    async fn joining(hub: &Hub, device: Uuid) -> JoinHandle<Membership> {
        let hub = hub.clone();
        let joiner = tokio::spawn(async move { hub.watch_device(device, Uuid::now_v7(), 0).await });
        tokio::task::yield_now().await;
        joiner
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_joiner_waits_for_the_pending_subscription_too() {
        let (hub, mut commands) = hub();
        let device = Uuid::now_v7();
        let first = joining(&hub, device).await;
        let second = joining(&hub, device).await;
        assert_eq!(
            subscribed(&mut commands),
            [format!("+{}", kick_channel(device))],
            "one subscribe for both"
        );
        tokio::time::advance(Duration::from_millis(1500)).await;
        tokio::task::yield_now().await;
        assert!(!first.is_finished());
        assert!(!second.is_finished(), "admitted before the subscribe");

        hub.set_active(&[kick_channel(device)], true);
        tokio::time::advance(Duration::from_millis(10)).await;
        let _ = (first.await.unwrap(), second.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn a_joiner_during_a_reconnect_waits_until_the_resubscribe() {
        let (hub, _commands) = hub();
        let device = Uuid::now_v7();
        let first = joining(&hub, device).await;
        hub.set_active(&[kick_channel(device)], true);
        let _first = first.await.unwrap();

        // The Redis connection broke.
        hub.deactivate_all();
        let late = joining(&hub, device).await;
        tokio::time::advance(Duration::from_millis(1500)).await;
        tokio::task::yield_now().await;
        assert!(!late.is_finished(), "admitted while Redis was away");

        hub.set_active(&[kick_channel(device)], true);
        tokio::time::advance(Duration::from_millis(10)).await;
        let _ = late.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_joiner_gives_up_waiting_after_two_seconds() {
        let (hub, _commands) = hub();
        let joiner = joining(&hub, Uuid::now_v7()).await;
        tokio::time::advance(SUBSCRIBE_WAIT + Duration::from_millis(10)).await;
        let _ = joiner.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn entries_available_rings_only_that_users_connections() {
        let (hub, _commands) = hub();
        let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
        let (mut alice_phone, _, _) = connection(&hub, alice).await;
        let (mut alice_laptop, _, _) = connection(&hub, alice).await;
        let (mut bob_phone, _, _) = connection(&hub, bob).await;
        hub.dispatch(&notify_channel(alice), &entries_available());
        assert_eq!(pending(&mut alice_phone), Some(Signal::Ring));
        assert_eq!(pending(&mut alice_laptop), Some(Signal::Ring));
        assert_eq!(pending(&mut bob_phone), None);
    }

    #[tokio::test(start_paused = true)]
    async fn rings_coalesce_until_answered() {
        let (hub, _commands) = hub();
        let user = Uuid::now_v7();
        let (mut session, _, _) = connection(&hub, user).await;
        for _ in 0..3 {
            hub.dispatch(&notify_channel(user), &entries_available());
        }
        assert_eq!(pending(&mut session), Some(Signal::Ring));
        assert_eq!(pending(&mut session), None);
    }

    #[tokio::test(start_paused = true)]
    async fn receipts_read_positions_and_contact_changes_become_server_frames() {
        let (hub, _commands) = hub();
        let (user, peer) = (Uuid::now_v7(), Uuid::now_v7());
        let conversation = id_bytes(Uuid::now_v7());
        let (mut session, _, _) = connection(&hub, user).await;
        let notifications = [
            Kind::ReceiptChanged(ReceiptChanged {
                conversation_id: conversation.clone(),
                positions: Some(receipt_changed::Positions::Direct(DirectReceipt {
                    user_id: id_bytes(peer),
                    delivered_seq: 4,
                    read_seq: 3,
                })),
            }),
            Kind::ReadPositionChanged(ReadPositionChanged {
                conversation_id: conversation.clone(),
                read_seq: 9,
            }),
            Kind::ContactChanged(ContactChanged {
                peer_id: id_bytes(peer),
                state: v1::ContactState::Accepted.into(),
            }),
        ];
        for kind in notifications {
            let payload = Notification { kind: Some(kind) }.encode_to_vec();
            hub.dispatch(&notify_channel(user), &payload);
        }
        let frames = [
            Body::Receipt(Receipt {
                conversation_id: conversation.clone(),
                positions: Some(receipt::Positions::Direct(v1::DirectReceipt {
                    user_id: id_bytes(peer),
                    delivered_seq: 4,
                    read_seq: 3,
                })),
            }),
            Body::ReadPosition(ReadPosition {
                conversation_id: conversation,
                read_seq: 9,
            }),
            Body::ContactEvent(ContactEvent {
                user_id: id_bytes(peer),
                state: v1::ContactState::Accepted.into(),
            }),
        ];
        for frame in frames {
            assert_eq!(pending(&mut session), Some(Signal::Frame(Box::new(frame))));
        }
        assert_eq!(pending(&mut session), None);
    }

    fn takeover(newcomer: Uuid) -> Vec<u8> {
        Kick {
            reason: KickReason::TakenOver.into(),
            connection_id: id_bytes(newcomer),
            ..Kick::default()
        }
        .encode_to_vec()
    }

    #[tokio::test(start_paused = true)]
    async fn a_kick_closes_the_devices_connections_and_a_takeover_only_older_ones() {
        let (hub, _commands) = hub();
        let user = Uuid::now_v7();
        let (mut old, device, _) = connection(&hub, user).await;
        // An Identify that stalls: its id is older than the newcomer's.
        let stalled = Uuid::now_v7();
        let newcomer = Uuid::now_v7();
        let mut new = hub.watch_device(device, newcomer, 0).await;
        let (mut other, _, _) = connection(&hub, user).await;
        hub.dispatch(&kick_channel(device), &takeover(newcomer));
        assert_eq!(pending(&mut old), Some(Signal::Kick(KickReason::TakenOver)));
        assert_eq!(pending(&mut new), None);
        assert_eq!(pending(&mut other), None);

        // The stalled one passes at last and announces itself: it closes nobody newer.
        let mut late = hub.watch_device(device, stalled, 0).await;
        hub.dispatch(&kick_channel(device), &takeover(stalled));
        assert_eq!(pending(&mut new), None);
        assert_eq!(pending(&mut late), None);
        // A takeover that names no connection closes nobody.
        let anonymous = Kick {
            reason: KickReason::TakenOver.into(),
            ..Kick::default()
        };
        hub.dispatch(&kick_channel(device), &anonymous.encode_to_vec());
        assert_eq!(pending(&mut new), None);

        let revoked = Kick {
            reason: KickReason::DeviceRevoked.into(),
            ..Kick::default()
        };
        hub.dispatch(&kick_channel(device), &revoked.encode_to_vec());
        assert_eq!(
            pending(&mut new),
            Some(Signal::Kick(KickReason::DeviceRevoked))
        );
        assert_eq!(
            pending(&mut late),
            Some(Signal::Kick(KickReason::DeviceRevoked))
        );
        assert_eq!(pending(&mut other), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_password_reset_kick_spares_connections_signed_in_after_it() {
        let (hub, _commands) = hub();
        let device = Uuid::now_v7();
        let mut before = hub.watch_device(device, Uuid::now_v7(), 1_000).await;
        let mut after = hub.watch_device(device, Uuid::now_v7(), 1_200).await;
        let reset = |reset_at| {
            Kick {
                reason: KickReason::PasswordReset.into(),
                reset_at,
                ..Kick::default()
            }
            .encode_to_vec()
        };
        // Issued in the reset's second: the order is unknown, so it closes.
        hub.dispatch(&kick_channel(device), &reset(1_000));
        assert_eq!(
            pending(&mut before),
            Some(Signal::Kick(KickReason::PasswordReset))
        );
        assert_eq!(pending(&mut after), None);

        // A connection that renewed its token after a reset is spared by that reset too.
        before.renewed(1_300);
        hub.dispatch(&kick_channel(device), &reset(1_250));
        assert_eq!(pending(&mut before), None);
        assert_eq!(
            pending(&mut after),
            Some(Signal::Kick(KickReason::PasswordReset))
        );
        // A kick without a time closes every connection.
        hub.dispatch(&kick_channel(device), &reset(0));
        assert_eq!(
            pending(&mut before),
            Some(Signal::Kick(KickReason::PasswordReset))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn strange_messages_ring_and_kick_nobody() {
        let (hub, _commands) = hub();
        let user = Uuid::now_v7();
        let (mut session, device, _) = connection(&hub, user).await;
        hub.dispatch("notify:not-a-uuid", &entries_available());
        hub.dispatch(&format!("other:{user}"), &entries_available());
        hub.dispatch(&notify_channel(user), &[0xff, 0xff]);
        hub.dispatch(&kick_channel(device), &[0xff, 0xff]);
        hub.dispatch(&kick_channel(device), &Kick::default().encode_to_vec());
        assert_eq!(pending(&mut session), None);
    }

    /// A Redis that keeps the connection but stops answering, as behind a half-open TCP
    /// connection, is given up after one unanswered PING; the hub connects again and
    /// rings everyone, since notifications may have been lost meanwhile.
    #[tokio::test]
    async fn a_redis_that_stops_answering_is_left_and_everyone_rung() {
        use testcontainers_modules::redis::Redis;
        use testcontainers_modules::testcontainers::ImageExt;
        use testcontainers_modules::testcontainers::runners::AsyncRunner;

        let container = Redis::default()
            .with_tag("7-alpine")
            .start()
            .await
            .expect("start Redis (is Docker running?)");
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let url = format!("redis://127.0.0.1:{port}");
        let client = redis::Client::open(url.as_str()).unwrap();
        let shutdown = CancellationToken::new();
        let (hub, subscriber) = Hub::start_with(
            RedisConnector::password(&url).unwrap(),
            shutdown.clone(),
            Duration::from_millis(500),
        );
        for _ in 0..100 {
            if hub.is_connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(hub.is_connected());
        let (mut session, _, _) = connection(&hub, Uuid::now_v7()).await;

        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let () = redis::cmd("CLIENT")
            .arg("PAUSE")
            .arg(8000)
            .arg("ALL")
            .query_async(&mut admin)
            .await
            .unwrap();
        let signal = tokio::time::timeout(Duration::from_secs(20), session.next())
            .await
            .expect("rung after the reconnect");
        assert_eq!(signal, Signal::Ring);
        shutdown.cancel();
        subscriber.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn ring_all_wakes_every_connection() {
        let (hub, _commands) = hub();
        let (mut a, _, _) = connection(&hub, Uuid::now_v7()).await;
        let (mut b, _, _) = connection(&hub, Uuid::now_v7()).await;
        hub.ring_all();
        assert_eq!(pending(&mut a), Some(Signal::Ring));
        assert_eq!(pending(&mut b), Some(Signal::Ring));
    }
}
