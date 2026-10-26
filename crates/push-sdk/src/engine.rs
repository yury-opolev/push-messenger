//! What rides on a connection, kept apart from the socket so it can be tested without one
//! (spec §4, §8.3, §8.4): deliveries are stored and then acknowledged in batches, the
//! outbox is sent one row per conversation in order, receipts, read positions and contact
//! changes are applied, and each change becomes an event for the UI. The connection
//! (`connection.rs`) feeds the [`Engine`] the frames it receives and sends the frames the
//! engine asks for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use push_domain::{ClientMsgId, ConversationId, UserId};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, ListContacts, ListConversations, ReadUpTo, request, response};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::Instant;
use uuid::Uuid;

use crate::SdkError;
use crate::model::{Activity, ActivityState, Contact, ContactState, Event, OutboxKind, Presence};
use crate::policy::{Acks, Backoff};
use crate::store::{Applied, Store, bytes};

/// How many events wait for the app at most. An app that stops reading loses the newer
/// ones and is told so with [`Event::EventsLost`]; the store holds everything anyway.
pub(crate) const EVENT_QUEUE: usize = 10_000;

/// What the connection and the app's calls share: the store, the event stream, and the
/// presence sharing setting this client last learned.
pub(crate) struct Shared {
    store: Mutex<Store>,
    events: Mutex<Queue>,
    /// The user and whether they share presence, as last answered to this client; `None`
    /// (unknown) counts as sharing.
    presence_sharing: Mutex<Option<(UserId, bool)>>,
}

/// The sending end of the event queue.
struct Queue {
    sender: mpsc::Sender<Event>,
    /// Events were dropped and `EventsLost` is in the queue for them.
    lost: bool,
}

impl Shared {
    /// `events` is bounded ([`EVENT_QUEUE`]).
    pub fn new(store: Store, events: mpsc::Sender<Event>) -> Self {
        Self {
            store: Mutex::new(store),
            events: Mutex::new(Queue {
                sender: events,
                lost: false,
            }),
            presence_sharing: Mutex::new(None),
        }
    }

    /// Records the presence sharing setting of `user` that the server answered.
    pub fn learned_presence_sharing(&self, user: UserId, share: bool) {
        *self
            .presence_sharing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((user, share));
    }

    /// Whether a presence change of `about` is dropped: the signed-in user is known to
    /// hide their presence, and `about` is someone else. The opt-out is reciprocal, but an
    /// announcement the server computed just before the hide committed can still arrive.
    fn drops_presence_of(&self, about: UserId) -> bool {
        let learned = *self
            .presence_sharing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match learned {
            Some((hider, false)) if hider != about => {
                // A setting learned for another user (signed out since) does not count.
                self.store().owner().ok().flatten() == Some(hider)
            }
            _ => false,
        }
    }

    /// The store. Each use is short: one statement or one transaction.
    pub fn store(&self) -> MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Tells the UI. An app that dropped its event stream is not told. When only one
    /// place is left in the queue, events are dropped and the last place takes one
    /// `EventsLost`, which tells the app to reload from the store once it reads again.
    pub fn emit(&self, event: Event) {
        let mut queue = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.sender.capacity() > 1 {
            queue.lost = false;
            let _ = queue.sender.try_send(event);
        } else if !queue.lost {
            queue.lost = true;
            if let Err(TrySendError::Full(_)) = queue.sender.try_send(Event::EventsLost) {
                tracing::warn!("the event queue is full");
            }
        }
    }
}

/// Most outbox rows one wake sends: the rest follow as `SendAck`s come back, so a full
/// outbox never holds the connection in the frame budget for long.
const MAX_SENDS_AT_ONCE: usize = 20;

/// Most `ReadUpTo` frames one wake produces, and most `Ack`s one flush produces: the rest
/// go on later turns of the connection, so reads of the socket (a `Pong` among them) are
/// never held back by a long run of frames waiting for the frame budget.
pub(crate) const MAX_FRAMES_PER_TURN: usize = 20;

/// The engine's own requests have ids from here up; the app's count up from 1.
pub(crate) const INTERNAL_REQUESTS: u64 = 1 << 63;

/// One of the engine's own requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Internal {
    Conversations,
    Contacts,
}

pub(crate) struct Engine {
    shared: Arc<Shared>,
    acks: Acks,
    /// Read positions sent as `ReadUpTo` on this connection.
    reads_sent: HashMap<ConversationId, i64>,
    next_internal: u64,
    internal: HashMap<u64, Internal>,
    /// Another conversation listing is wanted once the one in flight is answered.
    relist: bool,
    /// A delivery could not be stored: the connection must end (see `on_deliver`).
    broken: bool,
    /// The last wake left read positions unsent: it is wanted again.
    backlog: bool,
}

impl Engine {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            acks: Acks::default(),
            reads_sent: HashMap::new(),
            next_internal: INTERNAL_REQUESTS,
            internal: HashMap::new(),
            relist: false,
            broken: false,
            backlog: false,
        }
    }

    /// A wake left read positions unsent; call [`Engine::wake`] again once the frames it
    /// produced are out.
    pub fn backlog(&self) -> bool {
        self.backlog
    }

    /// A delivery could not be stored: end the connection and reconnect with backoff.
    pub fn broken(&self) -> bool {
        self.broken
    }

    /// A connection is `Ready`: list conversations and contacts (spec §4.5, to catch up on
    /// receipts and changes missed while away), send unconfirmed read positions, and
    /// start the outbox. Rows left in flight by the previous connection go again with
    /// the same `client_msg_id`.
    pub fn start(&mut self, now_ms: i64, out: &mut Vec<Body>) {
        self.stop();
        self.ask(Internal::Conversations, out);
        self.ask(Internal::Contacts, out);
        self.wake(now_ms, out);
    }

    /// The connection ended.
    pub fn stop(&mut self) {
        if let Err(error) = self.shared.store().requeue_in_flight() {
            tracing::warn!(code = error.code(), "could not requeue the outbox");
        }
        self.acks.clear();
        self.reads_sent.clear();
        self.internal.clear();
        self.relist = false;
        self.broken = false;
        self.backlog = false;
    }

    /// The app queued something or marked something read.
    pub fn wake(&mut self, now_ms: i64, out: &mut Vec<Body>) {
        self.backlog = false;
        let reads = self.shared.store().unsynced_reads();
        match reads {
            Ok(reads) => {
                let mut produced = 0;
                for (conversation, seq) in reads {
                    if self
                        .reads_sent
                        .get(&conversation)
                        .is_some_and(|sent| *sent >= seq)
                    {
                        continue;
                    }
                    if produced == MAX_FRAMES_PER_TURN {
                        self.backlog = true;
                        break;
                    }
                    produced += 1;
                    self.reads_sent.insert(conversation, seq);
                    out.push(Body::ReadUpTo(ReadUpTo {
                        conversation_id: bytes(conversation.0),
                        seq,
                    }));
                }
            }
            Err(error) => tracing::warn!(code = error.code(), "could not read the read positions"),
        }
        self.pump(now_ms, out);
    }

    /// Sends the outbox rows that are due: the first of each conversation, one in flight
    /// at a time (spec §4.2, §8.4).
    fn pump(&mut self, now_ms: i64, out: &mut Vec<Body>) {
        let mut store = self.shared.store();
        let due = match store.due(now_ms) {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!(code = error.code(), "could not read the outbox");
                return;
            }
        };
        for row in due.into_iter().take(MAX_SENDS_AT_ONCE) {
            if let Err(error) = store.mark_in_flight(row.client_msg_id) {
                tracing::warn!(code = error.code(), "could not mark an outbox row");
                return;
            }
            out.push(row.frame());
        }
    }

    /// When the next outbox row waiting for a retry is due, in milliseconds since the
    /// Unix epoch, if it is later than `now_ms`.
    pub fn retry_due(&self, now_ms: i64) -> Option<i64> {
        self.shared
            .store()
            .next_due()
            .ok()
            .flatten()
            .filter(|due| *due > now_ms)
    }

    /// When the waiting acknowledgements are due.
    pub fn acks_due(&self) -> Option<Instant> {
        self.acks.due_at()
    }

    /// One `Ack` per conversation, naming the highest `seq` stored, at most
    /// [`MAX_FRAMES_PER_TURN`] of them; the rest stay due.
    pub fn flush_acks(&mut self, out: &mut Vec<Body>) {
        for (conversation, seq) in self.acks.take_up_to(MAX_FRAMES_PER_TURN) {
            out.push(Body::Ack(v1::Ack {
                conversation_id: bytes(conversation.0),
                seq,
            }));
        }
    }

    fn ask(&mut self, what: Internal, out: &mut Vec<Body>) {
        let request_id = self.next_internal;
        self.next_internal += 1;
        self.internal.insert(request_id, what);
        let body = match what {
            Internal::Conversations => {
                request::Body::ListConversations(ListConversations::default())
            }
            Internal::Contacts => request::Body::ListContacts(ListContacts {}),
        };
        out.push(Body::Request(v1::Request {
            request_id,
            body: Some(body),
        }));
    }

    /// Lists the conversations again, once the listing in flight is answered.
    fn relist(&mut self, out: &mut Vec<Body>) {
        if self
            .internal
            .values()
            .any(|what| *what == Internal::Conversations)
        {
            self.relist = true;
        } else {
            self.ask(Internal::Conversations, out);
        }
    }

    /// Handles a frame from the gateway. Frames that are the connection's business
    /// (answers to the app's requests, `AuthExpiring`, `Pong`, errors that name no outbox
    /// row) are handed back.
    pub fn on_frame(
        &mut self,
        body: Body,
        now: Instant,
        now_ms: i64,
        out: &mut Vec<Body>,
    ) -> Option<Body> {
        match body {
            Body::Deliver(entry) => self.on_deliver(&entry, now, out),
            Body::SendAck(ack) => self.on_send_ack(&ack, now_ms, out),
            Body::Error(error) if error.client_msg_id.is_some() => {
                self.on_send_error(&error, now_ms, out);
            }
            Body::Error(error)
                if error
                    .request_id
                    .is_some_and(|id| self.internal.contains_key(&id)) =>
            {
                let what = error.request_id.and_then(|id| self.internal.remove(&id));
                tracing::info!(?what, code = ?error.code(), "a listing failed; the next connection lists again");
            }
            Body::Response(answer) if self.internal.contains_key(&answer.request_id) => {
                let what = self.internal.remove(&answer.request_id);
                self.on_listing(what, answer.body, out);
            }
            Body::Receipt(receipt) => self.on_receipt(&receipt),
            Body::ReadPosition(position) => self.on_read_position(&position),
            Body::ContactEvent(event) => self.on_contact_event(&event, out),
            Body::ActivityEvent(event) => self.on_activity_event(&event, now_ms),
            Body::PresenceEvent(event) => self.on_presence_event(&event),
            other => return Some(other),
        }
        None
    }

    fn on_deliver(&mut self, entry: &v1::Deliver, now: Instant, out: &mut Vec<Body>) {
        let Ok(conversation) = Uuid::from_slice(&entry.conversation_id).map(ConversationId) else {
            tracing::warn!("a delivery names no conversation");
            return;
        };
        let result = {
            let mut store = self.shared.store();
            store.apply_deliver(entry).and_then(|applied| {
                let card = match &applied.0 {
                    Applied::Added(message) | Applied::Confirmed(message) => {
                        store.mark_answered(message)?
                    }
                    _ => None,
                };
                Ok((applied, card, store.received(conversation)?))
            })
        };
        let ((applied, changed, created), card, received) = match result {
            Ok(result) => result,
            // An entry with ids that are not ids can never be stored: skipped.
            Err(error @ SdkError::Network(_)) => {
                tracing::warn!(code = error.code(), "skipped a malformed delivery");
                return;
            }
            Err(error) => {
                // Not acknowledged, and nothing after it may be on this connection: the
                // gateway does not send an entry twice on one connection, and the next
                // Ack, naming a higher seq, would cover this one for good. The connection
                // ends; the next one syncs again from the last Ack.
                tracing::warn!(
                    code = error.code(),
                    "could not store a delivery: reconnecting"
                );
                self.broken = true;
                return;
            }
        };
        match applied {
            Applied::Added(message) => self.shared.emit(Event::MessageAdded(message)),
            Applied::Confirmed(message) => self.shared.emit(Event::MessageSent(message)),
            Applied::Edited(message) => self.shared.emit(Event::MessageEdited(message)),
            Applied::Deleted(message) => self.shared.emit(Event::MessageDeleted(message)),
            Applied::Duplicate | Applied::Skipped => {}
        }
        // The answer closed its card: the card shows as answered now.
        if let Some(card) = card {
            self.shared.emit(Event::MessageEdited(card));
        }
        if let Some(conversation) = changed {
            self.shared.emit(Event::ConversationChanged(conversation));
        }
        if created {
            // Its kind and peer come with the listing.
            self.relist(out);
        }
        // A replay is acknowledged again: the last ack may have been lost.
        self.acks.stored(conversation, received, now);
    }

    fn on_send_ack(&mut self, ack: &v1::SendAck, now_ms: i64, out: &mut Vec<Body>) {
        let acknowledged = self.shared.store().acknowledged(ack);
        let row = match acknowledged {
            Ok(Some(row)) => row,
            // Acknowledged before, on an earlier attempt.
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(code = error.code(), "could not record a SendAck");
                return;
            }
        };
        let events = {
            let mut store = self.shared.store();
            match (row.kind, row.target_client_msg_id) {
                (OutboxKind::Send, _) => store
                    .message(row.conversation_id, row.client_msg_id)
                    .and_then(|message| {
                        let Some(message) = message else {
                            return Ok(Vec::new());
                        };
                        // A stored answer is the card's first valid one: it closes the card.
                        let card = store.mark_answered(&message)?;
                        Ok(std::iter::once(Event::MessageSent(message))
                            .chain(card.map(Event::MessageEdited))
                            .collect())
                    }),
                (OutboxKind::Edit, Some(target)) => store
                    .edit(
                        row.conversation_id,
                        target,
                        row.body.as_deref().unwrap_or_default().as_bytes(),
                        ack.server_ts,
                        ack.seq,
                    )
                    .map(|applied| match applied {
                        Applied::Edited(message) => vec![Event::MessageEdited(message)],
                        _ => Vec::new(),
                    }),
                (OutboxKind::Delete, Some(target)) => store
                    .erase(row.conversation_id, target, ack.seq)
                    .map(|applied| match applied {
                        Applied::Deleted(message) => vec![Event::MessageDeleted(message)],
                        _ => Vec::new(),
                    }),
                (_, None) => Ok(Vec::new()),
            }
        };
        match events {
            Ok(events) => {
                for event in events {
                    self.shared.emit(event);
                }
            }
            Err(error) => tracing::warn!(code = error.code(), "could not apply a SendAck"),
        }
        self.pump(now_ms, out);
    }

    /// A send, edit or delete failed: retried later if the server says it may succeed,
    /// else it leaves the outbox (with whatever depended on it) and the UI is told.
    fn on_send_error(&mut self, error: &v1::Error, now_ms: i64, out: &mut Vec<Body>) {
        let Some(id) = error
            .client_msg_id
            .as_deref()
            .and_then(|id| Uuid::from_slice(id).ok())
            .map(ClientMsgId)
        else {
            return;
        };
        let outcome = {
            let mut store = self.shared.store();
            match store.outbox_row(id) {
                Ok(Some(row)) if error.retryable => {
                    let delay_ms = match error.retry_after_s {
                        Some(seconds) => i64::from(seconds) * 1000,
                        None => {
                            let delay = Backoff::OUTBOX.delay(row.attempts.saturating_sub(1));
                            i64::try_from(delay.as_millis()).unwrap_or(i64::MAX)
                        }
                    };
                    store.retry_at(id, now_ms + delay_ms).map(|()| Vec::new())
                }
                Ok(Some(_)) => store.refused(id),
                Ok(None) => Ok(Vec::new()),
                Err(error) => Err(error),
            }
        };
        match outcome {
            Ok(removed) => {
                for row in removed {
                    self.shared.emit(Event::SendFailed {
                        conversation_id: row.conversation_id,
                        client_msg_id: row.client_msg_id,
                        kind: row.kind,
                        error: SdkError::from_frame(error),
                    });
                }
            }
            Err(error) => tracing::warn!(code = error.code(), "could not record a failed send"),
        }
        self.pump(now_ms, out);
    }

    fn on_listing(
        &mut self,
        what: Option<Internal>,
        answer: Option<response::Body>,
        out: &mut Vec<Body>,
    ) {
        match (what, answer) {
            (Some(Internal::Conversations), Some(response::Body::Conversations(list))) => {
                for summary in &list.conversations {
                    let applied = self.shared.store().apply_summary(summary);
                    match applied {
                        Ok(Some(conversation)) => {
                            self.shared.emit(Event::ConversationChanged(conversation));
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(code = error.code(), "could not store a listing");
                        }
                    }
                }
                if std::mem::take(&mut self.relist) {
                    self.ask(Internal::Conversations, out);
                }
            }
            (Some(Internal::Contacts), Some(response::Body::Contacts(list))) => {
                let listed: Vec<Contact> = list
                    .contacts
                    .iter()
                    .filter_map(|contact| {
                        Some(Contact {
                            user_id: Uuid::from_slice(&contact.user_id).ok()?.into(),
                            state: ContactState::from_wire(contact.state()),
                        })
                    })
                    .collect();
                let replaced = self.shared.store().replace_contacts(&listed);
                match replaced {
                    Ok(changed) => {
                        for contact in changed {
                            self.shared.emit(Event::ContactChanged(contact));
                        }
                    }
                    Err(error) => tracing::warn!(code = error.code(), "could not store contacts"),
                }
            }
            _ => tracing::warn!("a listing was answered with something else"),
        }
    }

    /// Receipts can arrive in any order: the store keeps the highest positions, and
    /// only a move is reported.
    fn on_receipt(&mut self, receipt: &v1::Receipt) {
        let Ok(conversation) = Uuid::from_slice(&receipt.conversation_id).map(ConversationId)
        else {
            return;
        };
        let (delivered, read) = match &receipt.positions {
            Some(v1::receipt::Positions::Direct(direct)) => (direct.delivered_seq, direct.read_seq),
            Some(v1::receipt::Positions::Group(group)) => {
                (group.all_delivered_seq, group.all_read_seq)
            }
            None => return,
        };
        let applied = self
            .shared
            .store()
            .apply_receipt(conversation, delivered, read);
        match applied {
            Ok(Some(moved)) => self.shared.emit(Event::ReceiptChanged {
                conversation_id: moved.id,
                peer_delivered_seq: moved.peer_delivered_seq,
                peer_read_seq: moved.peer_read_seq,
            }),
            Ok(None) => {}
            Err(error) => tracing::warn!(code = error.code(), "could not store a receipt"),
        }
    }

    fn on_read_position(&mut self, position: &v1::ReadPosition) {
        let Ok(conversation) = Uuid::from_slice(&position.conversation_id).map(ConversationId)
        else {
            return;
        };
        let applied = self
            .shared
            .store()
            .apply_read_position(conversation, position.read_seq);
        match applied {
            Ok(Some(changed)) => self.shared.emit(Event::ConversationChanged(changed)),
            Ok(None) => {}
            Err(error) => tracing::warn!(code = error.code(), "could not store a read position"),
        }
    }

    /// A relationship changed. Block and unblock hide and show conversations without
    /// changing them, so the conversations are listed again (spec §4.5).
    fn on_contact_event(&mut self, event: &v1::ContactEvent, out: &mut Vec<Body>) {
        let Ok(user) = Uuid::from_slice(&event.user_id) else {
            return;
        };
        let contact = Contact {
            user_id: user.into(),
            state: ContactState::from_wire(event.state()),
        };
        let changed = self.shared.store().set_contact(contact);
        match changed {
            Ok(true) => self.shared.emit(Event::ContactChanged(contact)),
            Ok(false) => {}
            Err(error) => tracing::warn!(code = error.code(), "could not store a contact"),
        }
        self.relist(out);
    }

    /// Activity is reported, not stored: it expires `ttl_ms` after it arrived, on this
    /// device's clock.
    fn on_activity_event(&mut self, event: &v1::ActivityEvent, now_ms: i64) {
        let (Ok(conversation), Ok(user)) = (
            Uuid::from_slice(&event.conversation_id),
            Uuid::from_slice(&event.user_id),
        ) else {
            return;
        };
        let Some(state) = ActivityState::from_wire(event.state()) else {
            return;
        };
        self.shared.emit(Event::ActivityChanged(Activity {
            conversation_id: ConversationId(conversation),
            user_id: user.into(),
            state,
            label: event.label.clone(),
            expires_at_ms: now_ms.saturating_add(i64::from(event.ttl_ms)),
        }));
    }

    fn on_presence_event(&mut self, event: &v1::PresenceEvent) {
        if let Some(presence) = event.presence.as_ref().and_then(Presence::from_wire) {
            if !self.shared.drops_presence_of(presence.user_id) {
                self.shared.emit(Event::PresenceChanged(presence));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use push_domain::{MessageId, UserId};
    use push_proto::v1::{EntryKind, ErrorCode};

    use super::*;
    use crate::model::{ConversationKind, MessageKind, MessageStatus, PresenceState};
    use crate::store::tests::{answer_entry, card_entry, entry};

    struct Fixture {
        engine: Engine,
        shared: Arc<Shared>,
        events: mpsc::Receiver<Event>,
        me: UserId,
        peer: UserId,
        conversation: ConversationId,
    }

    fn fixture() -> Fixture {
        let mut store = Store::open(":memory:", None).unwrap();
        let (me, peer) = (UserId::new(), UserId::new());
        store.bind_owner(me).unwrap();
        let conversation = ConversationId::new();
        store.open_direct(conversation, peer).unwrap();
        let (sender, events) = mpsc::channel(EVENT_QUEUE);
        let shared = Arc::new(Shared::new(store, sender));
        Fixture {
            engine: Engine::new(shared.clone()),
            shared,
            events,
            me,
            peer,
            conversation,
        }
    }

    impl Fixture {
        fn events(&mut self) -> Vec<Event> {
            let mut events = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                events.push(event);
            }
            events
        }

        fn frame(&mut self, body: Body) -> (Option<Body>, Vec<Body>) {
            let mut out = Vec::new();
            let back = self.engine.on_frame(body, Instant::now(), 0, &mut out);
            (back, out)
        }

        fn queue(&self, text: &str) -> ClientMsgId {
            let id = ClientMsgId::new();
            self.shared
                .store()
                .queue_send(self.conversation, id, text, 1)
                .unwrap();
            id
        }
    }

    fn sends(out: &[Body]) -> Vec<Vec<u8>> {
        out.iter()
            .filter_map(|body| match body {
                Body::Send(send) => Some(send.client_msg_id.clone()),
                Body::Edit(edit) => Some(edit.client_msg_id.clone()),
                _ => None,
            })
            .collect()
    }

    fn send_ack(id: ClientMsgId, seq: i64) -> Body {
        Body::SendAck(v1::SendAck {
            client_msg_id: bytes(id.0),
            message_id: bytes(MessageId::new().0),
            seq,
            server_ts: 100 + seq,
        })
    }

    #[test]
    fn a_connection_lists_and_sends_one_row_per_conversation() {
        let mut f = fixture();
        let (a, _b) = (f.queue("a"), f.queue("b"));
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        let requests: Vec<_> = out
            .iter()
            .filter_map(|body| match body {
                Body::Request(request) => Some(request.request_id >= INTERNAL_REQUESTS),
                _ => None,
            })
            .collect();
        assert_eq!(
            requests,
            [true, true],
            "conversations and contacts are listed"
        );
        assert_eq!(sends(&out), [bytes(a.0)], "b waits for a");

        let (_, out) = f.frame(send_ack(a, 1));
        assert_eq!(sends(&out), [bytes(_b.0)]);
        let events = f.events();
        assert!(matches!(&events[..], [Event::MessageSent(message)]
            if message.seq == Some(1) && message.status == MessageStatus::Sent));
    }

    #[test]
    fn a_full_outbox_is_sent_twenty_rows_at_a_time() {
        let mut f = fixture();
        let mut first = Vec::new();
        for _ in 0..25 {
            let conversation = ConversationId::new();
            f.shared
                .store()
                .open_direct(conversation, UserId::new())
                .unwrap();
            let id = ClientMsgId::new();
            f.shared
                .store()
                .queue_send(conversation, id, "hi", 1)
                .unwrap();
            first.push(id);
        }
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        assert_eq!(sends(&out).len(), MAX_SENDS_AT_ONCE);
        let (_, out) = f.frame(send_ack(first[0], 1));
        assert_eq!(sends(&out).len(), 5, "an ack lets the rest go");
    }

    #[test]
    fn a_backlog_is_acknowledged_once_with_the_highest_seq() {
        let mut f = fixture();
        for seq in 1..=300 {
            f.frame(Body::Deliver(entry(
                f.conversation,
                f.peer,
                seq,
                EntryKind::Message,
            )));
        }
        let added = f
            .events()
            .into_iter()
            .filter(|event| matches!(event, Event::MessageAdded(_)))
            .count();
        assert_eq!(added, 300);
        // A replay of an entry held already is no new message, but is acknowledged again.
        f.frame(Body::Deliver(entry(
            f.conversation,
            f.peer,
            5,
            EntryKind::Message,
        )));
        assert!(f.events().is_empty());
        assert!(f.engine.acks_due().is_some());
        let mut out = Vec::new();
        f.engine.flush_acks(&mut out);
        assert_eq!(
            out,
            [Body::Ack(v1::Ack {
                conversation_id: bytes(f.conversation.0),
                seq: 300,
            })]
        );
    }

    #[test]
    fn a_retryable_error_retries_with_the_same_id_and_a_refusal_fails_the_row() {
        let mut f = fixture();
        let (a, b) = (f.queue("a"), f.queue("b"));
        let mut out = Vec::new();
        f.engine.start(1_000, &mut out);
        let busy = v1::Error {
            code: ErrorCode::Unavailable.into(),
            retryable: true,
            client_msg_id: Some(bytes(a.0)),
            ..Default::default()
        };
        let mut out = Vec::new();
        f.engine
            .on_frame(Body::Error(busy), Instant::now(), 1_000, &mut out);
        assert!(sends(&out).is_empty(), "a waits, and b waits behind it");
        let due = f.engine.retry_due(1_000).expect("a retry is scheduled");
        assert!((1_500..=2_000).contains(&due), "{due}");
        let mut out = Vec::new();
        f.engine.wake(due, &mut out);
        assert_eq!(sends(&out), [bytes(a.0)], "the same client_msg_id again");

        let refused = v1::Error {
            code: ErrorCode::NotContact.into(),
            message: "not a contact".to_owned(),
            client_msg_id: Some(bytes(a.0)),
            ..Default::default()
        };
        let mut out = Vec::new();
        f.engine
            .on_frame(Body::Error(refused), Instant::now(), due, &mut out);
        assert_eq!(sends(&out), [bytes(b.0)], "the next row proceeds");
        let events = f.events();
        assert!(
            matches!(&events[..], [Event::SendFailed { client_msg_id, kind: OutboxKind::Send, error, .. }]
            if *client_msg_id == a && error.code() == "not_contact")
        );
        assert!(
            f.shared
                .store()
                .message(f.conversation, a)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn an_edit_of_a_message_in_flight_waits_for_it_and_applies_on_its_ack() {
        let mut f = fixture();
        let original = f.queue("helo");
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        let message = f
            .shared
            .store()
            .message(f.conversation, original)
            .unwrap()
            .unwrap();
        let edit = ClientMsgId::new();
        f.shared
            .store()
            .queue_change(
                f.conversation,
                edit,
                OutboxKind::Edit,
                &message,
                Some("hello"),
                Some(original),
                2,
            )
            .unwrap();
        let mut out = Vec::new();
        f.engine.wake(0, &mut out);
        assert!(
            sends(&out).is_empty(),
            "the edit waits for the original's SendAck"
        );
        let (_, out) = f.frame(send_ack(original, 1));
        assert_eq!(sends(&out), [bytes(edit.0)]);
        f.events();
        f.frame(send_ack(edit, 2));
        let events = f.events();
        assert!(matches!(&events[..], [Event::MessageEdited(message)]
            if message.body.as_deref() == Some("hello") && message.edited_at == Some(102)));
    }

    #[test]
    fn an_answer_closes_its_card_and_the_card_is_reported_changed() {
        let mut f = fixture();
        let card = card_entry(f.conversation, f.me, 1);
        f.frame(Body::Deliver(card.clone()));
        f.events();
        f.frame(Body::Deliver(answer_entry(
            f.conversation,
            f.peer,
            2,
            &card,
        )));
        let events = f.events();
        assert!(matches!(&events[..], [
            Event::MessageAdded(answer),
            Event::MessageEdited(closed),
            Event::ConversationChanged(_),
        ] if answer.kind == MessageKind::Answer && answer.card_seq == Some(1)
            && closed.seq == Some(1) && closed.answer_seq == Some(2)));
    }

    #[test]
    fn the_ack_of_an_answer_closes_its_card() {
        let mut f = fixture();
        let card = card_entry(f.conversation, f.peer, 1);
        f.frame(Body::Deliver(card));
        let answer = ClientMsgId::new();
        f.shared
            .store()
            .queue_content(
                f.conversation,
                answer,
                MessageKind::Answer,
                "{}",
                Some(1),
                1,
            )
            .unwrap();
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        f.events();
        f.frame(send_ack(answer, 2));
        let events = f.events();
        assert!(
            matches!(&events[..], [Event::MessageSent(sent), Event::MessageEdited(closed)]
            if sent.client_msg_id == answer && closed.answer_seq == Some(2))
        );
    }

    #[test]
    fn receipts_report_only_forward_moves() {
        let mut f = fixture();
        let (conversation, peer) = (f.conversation, f.peer);
        let receipt = |delivered, read| {
            Body::Receipt(v1::Receipt {
                conversation_id: bytes(conversation.0),
                positions: Some(v1::receipt::Positions::Direct(v1::DirectReceipt {
                    user_id: bytes(peer.0),
                    delivered_seq: delivered,
                    read_seq: read,
                })),
            })
        };
        f.frame(receipt(5, 5));
        f.frame(receipt(4, 3));
        let events = f.events();
        assert_eq!(
            events,
            [Event::ReceiptChanged {
                conversation_id: f.conversation,
                peer_delivered_seq: 5,
                peer_read_seq: 5,
            }]
        );
    }

    #[test]
    fn a_contact_event_is_reported_and_the_conversations_are_listed_again() {
        let mut f = fixture();
        let (_, out) = f.frame(Body::ContactEvent(v1::ContactEvent {
            user_id: bytes(f.peer.0),
            state: v1::ContactState::Blocked.into(),
        }));
        assert!(matches!(&out[..], [Body::Request(request)]
            if matches!(request.body, Some(request::Body::ListConversations(_)))));
        assert_eq!(
            f.events(),
            [Event::ContactChanged(Contact {
                user_id: f.peer,
                state: ContactState::Blocked,
            })]
        );
    }

    #[test]
    fn an_activity_event_becomes_activity_changed_with_a_local_expiry() {
        let mut f = fixture();
        let (conversation, peer) = (f.conversation, f.peer);
        let activity = move |conversation_id: Vec<u8>, state: v1::ActivityState, ttl_ms| {
            Body::ActivityEvent(v1::ActivityEvent {
                conversation_id,
                user_id: bytes(peer.0),
                state: state.into(),
                label: Some("running tests".to_owned()),
                ttl_ms,
            })
        };
        let working = activity(bytes(conversation.0), v1::ActivityState::Working, 30_000);
        let idle = activity(bytes(conversation.0), v1::ActivityState::Idle, 0);
        let unspecified = activity(bytes(conversation.0), v1::ActivityState::Unspecified, 1);
        let nameless = activity(vec![1, 2], v1::ActivityState::Typing, 30_000);
        for body in [working, idle, unspecified, nameless] {
            let mut out = Vec::new();
            let back = f.engine.on_frame(body, Instant::now(), 5_000, &mut out);
            assert_eq!((back, out), (None, Vec::new()));
        }
        let changed = move |state, expires_at_ms| {
            Event::ActivityChanged(Activity {
                conversation_id: conversation,
                user_id: peer,
                state,
                label: Some("running tests".to_owned()),
                expires_at_ms,
            })
        };
        assert_eq!(
            f.events(),
            [
                changed(ActivityState::Working, 35_000),
                changed(ActivityState::Idle, 5_000),
            ]
        );
    }

    #[test]
    fn while_hiding_presence_only_the_users_own_presence_is_reported() {
        let mut f = fixture();
        let (me, peer) = (f.me, f.peer);
        let online = |user: UserId| {
            Body::PresenceEvent(v1::PresenceEvent {
                presence: Some(v1::Presence {
                    user_id: bytes(user.0),
                    state: v1::PresenceState::Online.into(),
                    last_seen: None,
                }),
            })
        };
        let changed = |user_id| {
            Event::PresenceChanged(Presence {
                user_id,
                state: PresenceState::Online,
                last_seen_ms: None,
            })
        };

        f.shared.learned_presence_sharing(me, false);
        f.frame(online(peer));
        f.frame(online(me));
        assert_eq!(f.events(), [changed(me)]);

        // Another user's setting (signed out since) does not hide anything.
        f.shared.learned_presence_sharing(UserId::new(), false);
        f.frame(online(peer));
        f.shared.learned_presence_sharing(me, true);
        f.frame(online(peer));
        assert_eq!(f.events(), [changed(peer), changed(peer)]);
    }

    #[test]
    fn a_presence_event_becomes_presence_changed() {
        let mut f = fixture();
        let peer = f.peer;
        let presence = |user_id: Vec<u8>, state: v1::PresenceState, last_seen| {
            Body::PresenceEvent(v1::PresenceEvent {
                presence: Some(v1::Presence {
                    user_id,
                    state: state.into(),
                    last_seen,
                }),
            })
        };
        f.frame(presence(bytes(peer.0), v1::PresenceState::Online, None));
        f.frame(presence(
            bytes(peer.0),
            v1::PresenceState::Offline,
            Some(1_700_000_040_000_000),
        ));
        f.frame(presence(bytes(peer.0), v1::PresenceState::Unknown, None));
        f.frame(presence(vec![7], v1::PresenceState::Online, None));
        let (back, out) = f.frame(Body::PresenceEvent(v1::PresenceEvent { presence: None }));
        assert_eq!((back, out), (None, Vec::new()));
        let changed = move |state, last_seen_ms| {
            Event::PresenceChanged(Presence {
                user_id: peer,
                state,
                last_seen_ms,
            })
        };
        assert_eq!(
            f.events(),
            [
                changed(PresenceState::Online, None),
                changed(PresenceState::Offline, Some(1_700_000_040_000)),
                changed(PresenceState::Unknown, None),
            ]
        );
    }

    #[test]
    fn listings_fill_in_a_conversation_learned_from_a_delivery() {
        let mut f = fixture();
        let unknown = ConversationId::new();
        let (_, out) = f.frame(Body::Deliver(entry(unknown, f.peer, 1, EntryKind::Message)));
        let Some(Body::Request(listing)) = out.first() else {
            panic!("a new conversation is listed");
        };
        let summary = v1::ConversationSummary {
            conversation_id: bytes(unknown.0),
            kind: v1::ConversationKind::Direct.into(),
            last_seq: 1,
            unread: 1,
            members: vec![bytes(f.me.0), bytes(f.peer.0)],
            updated_at: 101,
            ..Default::default()
        };
        f.events();
        let (back, _) = f.frame(Body::Response(v1::Response {
            request_id: listing.request_id,
            body: Some(response::Body::Conversations(v1::ConversationList {
                conversations: vec![summary],
            })),
        }));
        assert!(back.is_none());
        let events = f.events();
        assert!(
            matches!(&events[..], [Event::ConversationChanged(conversation)]
            if conversation.kind == ConversationKind::Direct && conversation.peer == Some(f.peer))
        );
        // An answer to the app's own request is handed back to the connection.
        let (back, _) = f.frame(Body::Response(v1::Response {
            request_id: 1,
            body: Some(response::Body::Done(v1::Done {})),
        }));
        assert!(back.is_some());
    }

    #[test]
    fn a_read_is_sent_once_and_confirmed_by_the_read_position() {
        let mut f = fixture();
        f.frame(Body::Deliver(entry(
            f.conversation,
            f.peer,
            1,
            EntryKind::Message,
        )));
        f.shared.store().mark_read(f.conversation, 1).unwrap();
        let mut out = Vec::new();
        f.engine.wake(0, &mut out);
        f.engine.wake(0, &mut out);
        assert_eq!(
            out,
            [Body::ReadUpTo(ReadUpTo {
                conversation_id: bytes(f.conversation.0),
                seq: 1,
            })]
        );
        f.frame(Body::ReadPosition(v1::ReadPosition {
            conversation_id: bytes(f.conversation.0),
            read_seq: 1,
        }));
        assert!(f.shared.store().unsynced_reads().unwrap().is_empty());
    }

    #[test]
    fn many_read_positions_and_acks_are_produced_a_turn_at_a_time() {
        let mut f = fixture();
        for _ in 0..45 {
            let conversation = ConversationId::new();
            let mut store = f.shared.store();
            store.open_direct(conversation, f.peer).unwrap();
            drop(store);
            f.frame(Body::Deliver(entry(
                conversation,
                f.peer,
                1,
                EntryKind::Message,
            )));
            f.shared.store().mark_read(conversation, 1).unwrap();
        }
        let reads = |out: &[Body]| {
            out.iter()
                .filter(|body| matches!(body, Body::ReadUpTo(_)))
                .count()
        };
        let mut turns = Vec::new();
        while turns.is_empty() || f.engine.backlog() {
            let mut out = Vec::new();
            f.engine.wake(0, &mut out);
            turns.push(reads(&out));
        }
        assert_eq!(turns, [20, 20, 5]);

        let mut acks = Vec::new();
        while f.engine.acks_due().is_some() {
            let mut out = Vec::new();
            f.engine.flush_acks(&mut out);
            acks.push(out.len());
        }
        assert_eq!(acks, [20, 20, 5]);
    }

    #[test]
    fn a_delivery_that_cannot_be_stored_ends_the_connection_unacknowledged() {
        let mut f = fixture();
        let deliver = |seq| Body::Deliver(entry(f.conversation, f.peer, seq, EntryKind::Message));
        let (first, second, third) = (deliver(1), deliver(2), deliver(3));
        f.frame(first);
        f.shared.store().set_read_only(true);
        f.frame(second.clone());
        // An Ack of 3 on this connection would acknowledge 2, which was never stored.
        assert!(f.engine.broken(), "the connection must end");
        f.engine.stop();

        // The next connection: the gateway syncs from the last Ack again.
        f.shared.store().set_read_only(false);
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        assert!(!f.engine.broken());
        f.frame(second);
        f.frame(third);
        let mut out = Vec::new();
        f.engine.flush_acks(&mut out);
        assert_eq!(
            out,
            [Body::Ack(v1::Ack {
                conversation_id: bytes(f.conversation.0),
                seq: 3,
            })]
        );
        let held = f.shared.store().messages(f.conversation, None, 10).unwrap();
        assert_eq!(held.len(), 3);
    }

    #[test]
    fn an_app_that_stops_reading_is_told_that_events_were_lost() {
        let (sender, mut events) = mpsc::channel(3);
        let shared = Shared::new(Store::open(":memory:", None).unwrap(), sender);
        for _ in 0..5 {
            shared.emit(Event::SignedOut);
        }
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            seen.push(event);
        }
        assert_eq!(
            seen,
            [Event::SignedOut, Event::SignedOut, Event::EventsLost]
        );
        shared.emit(Event::DeviceRemoved);
        assert_eq!(events.try_recv(), Ok(Event::DeviceRemoved));
    }

    #[test]
    fn a_dropped_connection_sends_the_row_in_flight_again() {
        let mut f = fixture();
        let a = f.queue("a");
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        assert_eq!(sends(&out), [bytes(a.0)]);
        f.engine.stop();
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        assert_eq!(sends(&out), [bytes(a.0)]);
        let _ = f.me;
    }
}
