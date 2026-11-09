//! The storage boundary. Every service operation runs inside one [`Tx`].
//!
//! Implementations must:
//! - run at READ COMMITTED: each statement sees everything committed before it plus this
//!   transaction's own writes. Serialisation comes from row locks, not from snapshots;
//! - make [`Tx::next_seq`] lock the conversation until the transaction ends, so exactly one
//!   transaction at a time can append to a conversation;
//! - make [`Tx::lock_contact_pair`] serialise all contact operations between two users;
//! - keep the transaction usable after an insert reports [`InsertOutcome::Duplicate`]
//!   (on Postgres: `ON CONFLICT DO NOTHING`, never a caught unique violation);
//! - report deadlocks and serialisation failures as [`DomainError::Conflict`], which
//!   callers may retry;
//! - enforce the schema's foreign and primary keys, failing with `DomainError::Store`;
//! - after any method fails, expect the caller to roll back: the transaction may be
//!   unusable (on Postgres every later statement fails), so no caller continues after an
//!   error;
//! - roll back a transaction that is dropped without [`Tx::commit`].
//!
//! # Locking
//!
//! A send takes the conversation lock through [`Tx::next_seq`] and reads contacts WITHOUT
//! the pair lock. A send that read `Accepted` before a concurrent block commits is ordered
//! before the block; that is accepted. `open_direct` takes the pair lock and never locks an
//! existing conversation, so the lock order cannot form a cycle.
//!
//! Redeeming an invite locks the invite row ([`Tx::lock_invite`]) first and the contact
//! pair second. No operation takes them in the other order.
//!
//! A group's positions lock ([`Tx::lock_conversation_positions`]) comes after its
//! conversation lock: appends and group changes take the conversation lock through
//! [`Tx::next_seq`] first, acks and reads take only the positions lock.
//!
//! User locks ([`Tx::lock_users`]) come last. The global order is: contact pair /
//! conversation row ([`Tx::next_seq`]) → positions → users. `add_members`
//! takes the positions lock while it reads the group's positions, before it locks the
//! users it adds, so never take user locks before the positions lock in a path that also
//! takes positions. A device registration takes only its user's lock. Presence
//! publications take the lock of the user whose presence they publish (`contact_block`
//! after the pair lock), so they are serialised per user. A transaction takes
//! its user locks in one call, so no two transactions wait for each other's user locks in
//! a cycle.
//!
//! The outgoing-policy gate of an agent's action (spec 2026-11-09 proactive agents) takes
//! the agent's user lock. A contact request takes it after the pair lock and takes no
//! other user lock. `open_direct` and `create_group` lock every user they will lock, the
//! agent included, in one call before the gate (so the gate's own call only takes a lock
//! already held); `create_group` does so before it inserts the group, whose conversation
//! lock nobody else can wait for yet. `add_members` locks the agent with the users it adds,
//! after the positions lock, as before. Postgres advisory locks are re-entrant, so taking
//! a held user lock again never waits.
//!
//! Template operations lock in the order publisher ([`Tx::claim_publisher`]) → version
//! ([`Tx::lock_template`]) → change counter ([`Tx::next_template_change`]). A submission
//! first takes its submitter's lock ([`Tx::lock_users`]), before the publisher, so one
//! account's submissions are serialised and cannot both pass the pending cap. That puts a
//! user lock first, unlike the messaging paths, which take user locks last. A message that
//! uses a template (a RICH send or edit, an ANSWER) takes the version's share lock
//! ([`Tx::template_for_send`]) after its conversation lock, so a revocation cannot commit
//! between the check and the message. No cycle can form: a review takes the version's
//! row lock and then the counter and never a conversation lock; a submission takes no
//! conversation lock and inserts versions nobody can use yet; and the messaging paths take
//! the version share lock last, holding no user lock when they take it.
//!
//! # Delivery positions
//!
//! Positions are created eagerly, so a device has one for every membership of its user:
//! registrations and joins of the same user are serialised by [`Tx::lock_users`]. Readers
//! still treat a missing position as `visible_from_seq - 1`, so [`Tx::sync_positions`]
//! starts from the memberships and joins the positions to them, never the other way
//! round.
//!
//! [`DomainError::Conflict`]: crate::error::DomainError::Conflict

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::calls::{CallHistoryRow, HistoryCursor};
use crate::error::DomainResult;
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{
    AgentPolicy, AgentSettings, ContactState, Conversation, ConversationFlags,
    ConversationOverview, DeviceState, Entry, GroupPositions, InviteRecord, Member, OutgoingAction,
    OutgoingError, OutgoingKind, OutgoingState, PublisherOwner, StoredTemplate, TemplateEvent,
    TemplateStatus,
};
use crate::templates::TemplateRef;

/// Result of an insert guarded by a uniqueness rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    /// A row with the same unique key already exists; nothing was written.
    Duplicate,
}

#[async_trait]
pub trait Store: Send + Sync {
    type Tx: Tx;

    async fn begin(&self) -> DomainResult<Self::Tx>;
}

#[async_trait]
pub trait Tx: Send {
    async fn commit(self) -> DomainResult<()>;
    async fn rollback(self) -> DomainResult<()>;

    // --- Users and devices -------------------------------------------------------------

    /// Records a verified user. Idempotent.
    async fn insert_user(&mut self, user: UserId) -> DomainResult<()>;
    /// Records the immutable owner once. Returns true only on the first enrollment;
    /// replay must not restore contacts subsequently blocked by either party.
    /// Both users must exist; a different existing owner is a storage error.
    async fn record_agent_owner(&mut self, agent: UserId, owner: UserId) -> DomainResult<bool>;
    async fn user_exists(&mut self, user: UserId) -> DomainResult<bool>;
    /// Records that `user`'s password was reset at `at`, keeping the latest time, and
    /// returns the time kept. The user must exist.
    async fn record_password_reset(
        &mut self,
        user: UserId,
        at: DateTime<Utc>,
    ) -> DomainResult<DateTime<Utc>>;
    /// Records an active device of `user` unless a device with this id exists (any owner,
    /// active or revoked), and returns whether the device is now an active device of
    /// `user`. Postgres checks after `INSERT … ON CONFLICT DO NOTHING`, which waits for a
    /// concurrent insert of the same id, so two registrations cannot both see `true` for
    /// different users. Fails with `DomainError::Store` if the device is new and the user
    /// is not known.
    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool>;
    /// Marks a device revoked. A no-op for an unknown device.
    async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()>;
    /// The owner of a device, active or revoked; `None` if the device is unknown.
    async fn device_user(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
    /// The owner of a device, whether the device is still active and the owner's last
    /// password reset, read in one statement so they cannot disagree; `None` if the device
    /// is unknown.
    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<DeviceState>>;
    /// The owner of an active device; `None` if the device is unknown or revoked.
    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
    /// The user's active devices.
    /// The order of the result is unspecified.
    async fn devices_of_user(&mut self, user: UserId) -> DomainResult<Vec<DeviceId>>;
    /// Serialises, until the transaction ends, every operation that pairs these users'
    /// devices with their conversations: registering a device of one of them, and making
    /// one of them a member of a conversation. Each such transaction calls it once, before
    /// it reads the devices or the memberships, so a registration and a join of the same
    /// user never miss each other's uncommitted rows. The locks are taken in one fixed
    /// order, whatever the order of `users`; duplicates are ignored.
    /// Postgres: transaction-scoped advisory locks on a namespaced hash of each user id.
    async fn lock_users(&mut self, users: &[UserId]) -> DomainResult<()>;

    // --- Contacts: one row per direction -----------------------------------------------

    /// Serialises contact operations between two users until the transaction ends.
    /// Call it before reading either direction of the pair. The order of `a` and `b`
    /// does not matter.
    async fn lock_contact_pair(&mut self, a: UserId, b: UserId) -> DomainResult<()>;

    async fn get_contact(
        &mut self,
        owner: UserId,
        peer: UserId,
    ) -> DomainResult<Option<ContactState>>;
    /// Inserts or replaces the contact row.
    async fn put_contact(
        &mut self,
        owner: UserId,
        peer: UserId,
        state: ContactState,
    ) -> DomainResult<()>;
    /// Inserts or replaces the row with a `Blocked` one, recording whether `owner` could see
    /// `peer` when blocking (`related_users` counts the row only then). `put_contact`
    /// records `false`.
    async fn put_block(&mut self, owner: UserId, peer: UserId, sees_peer: bool)
    -> DomainResult<()>;
    /// A no-op if the row is absent.
    async fn delete_contact(&mut self, owner: UserId, peer: UserId) -> DomainResult<()>;

    /// Every row owned by `owner`, as `(peer, state)`. Empty for an unknown user.
    /// The order of the result is unspecified.
    async fn contacts_of(&mut self, owner: UserId) -> DomainResult<Vec<(UserId, ContactState)>>;

    /// The users among `candidates`, other than `user`, whom `user` has a contact row for
    /// in any state but `RequestedOut` (the user's own pending request grants nothing),
    /// except a `Blocked` row written without sight of the peer (see `put_block`), or
    /// who are or were members of a conversation `user` is or was a member of. Duplicates
    /// in `candidates` are ignored. One statement on Postgres. The order of the result is
    /// unspecified.
    async fn related_users(
        &mut self,
        user: UserId,
        candidates: &[UserId],
    ) -> DomainResult<Vec<UserId>>;

    // --- Presence ----------------------------------------------------------------------

    /// The users among `users` who hide their presence. Duplicates in `users` are ignored;
    /// unknown users hide nothing. The order of the result is unspecified.
    async fn presence_hidden(&mut self, users: &[UserId]) -> DomainResult<Vec<UserId>>;
    /// Hides (`true`) or shows `user`'s presence. Idempotent. Fails with
    /// `DomainError::Store` when hiding the presence of a user who is not known.
    async fn set_presence_hidden(&mut self, user: UserId, hidden: bool) -> DomainResult<()>;

    // --- Invites -----------------------------------------------------------------------

    /// Stores a new invite. Returns `Duplicate`, writing nothing, if an invite with the same
    /// code exists (Postgres: `ON CONFLICT (code) DO NOTHING`). Fails with
    /// `DomainError::Store` if the creator is not a known user.
    async fn insert_invite(&mut self, invite: InviteRecord) -> DomainResult<InsertOutcome>;
    /// The invite with this code, or `None`. Locks the invite row until the transaction
    /// ends (`SELECT … FOR UPDATE`), so redeems and revokes of one invite are serialised and
    /// its use count cannot be exceeded.
    async fn lock_invite(&mut self, code: &str) -> DomainResult<Option<InviteRecord>>;
    /// Overwrites `uses` and `revoked_at` of the stored invite with the same code. Every
    /// other field of `invite` is ignored. Fails with `DomainError::Store` if there is no
    /// such invite.
    async fn update_invite(&mut self, invite: InviteRecord) -> DomainResult<()>;

    // --- Conversations and members -----------------------------------------------------

    async fn find_direct(&mut self, key: [u8; 32]) -> DomainResult<Option<ConversationId>>;
    /// A conversation with an existing id fails with `DomainError::Store`; a direct key
    /// that already exists returns `Duplicate`.
    async fn insert_conversation(
        &mut self,
        conversation: Conversation,
    ) -> DomainResult<InsertOutcome>;
    async fn get_conversation(&mut self, id: ConversationId) -> DomainResult<Option<Conversation>>;
    /// Locks the conversation, increments `last_seq`, moves `updated_at` forward to `at`
    /// (never back: `GREATEST` on Postgres) and returns the new `last_seq`. Both changes are
    /// undone if the transaction rolls back.
    /// Fails with `ConversationNotFound` if the conversation does not exist.
    /// The conversation row lock is held until the transaction commits or rolls back.
    async fn next_seq(&mut self, id: ConversationId, at: DateTime<Utc>) -> DomainResult<i64>;
    /// Sets the title of the conversation. Fails with `ConversationNotFound` if it does not
    /// exist. Callers hold the conversation lock (taken by `next_seq`).
    async fn set_title(&mut self, id: ConversationId, title: String) -> DomainResult<()>;
    /// Inserts the member, replacing an existing row for the same conversation and user.
    async fn insert_member(&mut self, member: Member) -> DomainResult<()>;
    async fn get_member(
        &mut self,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<Option<Member>>;
    /// The order of the result is unspecified.
    async fn members(&mut self, conversation: ConversationId) -> DomainResult<Vec<Member>>;
    /// Every conversation the user is, or was, a member of.
    /// Each returned `Conversation` (including `last_seq`) must be read in the same
    /// statement as its `Member` row, so `last_seq` is consistent with the membership.
    /// The order of the result is unspecified.
    async fn memberships(&mut self, user: UserId) -> DomainResult<Vec<(Conversation, Member)>>;
    /// Moves `read_seq` forward to `seq`. Returns whether it moved.
    /// Fails with `NotMember` if the member does not exist.
    async fn raise_read_seq(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        seq: i64,
    ) -> DomainResult<bool>;
    /// Serialises changes to the conversation's delivery and read positions until the
    /// transaction ends. Every operation that compares a group's group-wide positions
    /// before and after a change calls it before reading "before", so two concurrent
    /// changes cannot both miss a move (under READ COMMITTED each would otherwise see the
    /// other's old position). It does not lock the conversation row: acks and reads do not
    /// wait for appends to commit. Appends take it after `next_seq`, never before.
    /// Postgres: a transaction-scoped advisory lock (`pg_advisory_xact_lock`) on a
    /// namespaced (tagged) hash of the conversation id, so sharing a key with another kind
    /// of lock, such as a contact pair's, is unlikely and would only add waiting.
    async fn lock_conversation_positions(
        &mut self,
        conversation: ConversationId,
    ) -> DomainResult<()>;

    // --- The log -----------------------------------------------------------------------

    /// `Duplicate`, writing nothing, if the conversation already has an entry with this
    /// `client_msg_id`. Otherwise fails with `DomainError::Store` if an entry with this
    /// `message_id` exists (in any conversation) or the conversation already has an entry
    /// at this `seq`.
    async fn insert_entry(&mut self, entry: Entry) -> DomainResult<InsertOutcome>;
    async fn entry_by_client_id(
        &mut self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
    ) -> DomainResult<Option<Entry>>;
    /// The entry with this `message_id`; `None` if there is none or it belongs to another
    /// conversation.
    async fn entry_by_message_id(
        &mut self,
        conversation: ConversationId,
        message_id: MessageId,
    ) -> DomainResult<Option<Entry>>;
    /// Overwrites the mutable fields of the stored entry at `(conversation_id, seq)`:
    /// `body`, `edited_at`, `edit_seq`, `deleted_at` and `answer_seq`. Every other field of
    /// `entry` is ignored. Fails with `DomainError::Store` if no such entry exists.
    async fn update_entry(&mut self, entry: Entry) -> DomainResult<()>;
    /// Sets `body` to null on every `Edit` entry whose `target_seq` is `target_seq`.
    /// A no-op if there are none. Postgres serves it from the index on
    /// `(conversation_id, target_seq)`.
    async fn erase_edit_bodies(
        &mut self,
        conversation: ConversationId,
        target_seq: i64,
    ) -> DomainResult<()>;
    /// Entries with `after < seq <= up_to`, oldest first, at most `limit`. Edits and
    /// deletes with a `target_seq` below `targets_from` (changes to messages outside the
    /// reader's visible range) are skipped and do not count towards `limit`. Messages that
    /// target an older entry (answers to cards) are new messages and are always returned.
    async fn entries_after(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        targets_from: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>>;
    /// Counts entries with `after < seq <= up_to` that are messages (`kind = Message`), not
    /// deleted, and sent by someone other than `user`. Stops counting at `cap`, so the
    /// result is at most `cap`.
    async fn count_incoming_messages(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        after: i64,
        up_to: i64,
        cap: u32,
    ) -> DomainResult<u32>;
    /// The lowest `seq` with `after < seq <= up_to` of a message (`kind = Message`) that is
    /// not deleted and, if `not_from` is given, was sent by someone else. `None` if there is
    /// none. On Postgres one range scan of the `message_live` partial index: `… ORDER BY seq LIMIT 1`.
    async fn first_live_message(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        not_from: Option<UserId>,
    ) -> DomainResult<Option<i64>>;
    /// The newest `limit` entries of kind `Message` or `System` with `from <= seq < before`,
    /// returned oldest first. Edit and delete entries are skipped and do not count towards
    /// `limit`.
    async fn messages_before(
        &mut self,
        conversation: ConversationId,
        from: i64,
        before: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>>;

    // --- Delivery positions ------------------------------------------------------------

    /// The device's delivery position in the conversation; `None` if it has none there.
    /// Revoked devices keep their positions.
    async fn get_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
    ) -> DomainResult<Option<i64>>;
    /// Creates the position if absent. Accepts revoked devices: their positions still
    /// count for `delivered_seq`.
    async fn create_cursor_if_absent(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<()>;
    /// Moves the position forward to `seq`, creating it if absent. Accepts revoked devices.
    /// Returns whether it moved; creating it counts as moving.
    async fn raise_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<bool>;
    /// The furthest delivery position across all of the user's devices, including revoked
    /// ones: a delivery that happened stays delivered. 0 if the user has no position.
    async fn delivered_seq(
        &mut self,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<i64>;

    /// Every member row of the conversation, current and former, each with that member's
    /// `delivered_seq`, delivered as returned by [`Tx::delivered_seq`] — revoked devices
    /// included; 0 if the member has no positions. One statement on Postgres. The order
    /// of the result is unspecified.
    async fn member_positions(
        &mut self,
        conversation: ConversationId,
    ) -> DomainResult<Vec<(Member, i64)>>;

    /// The memberships of `user` in which `device` has entries left to receive, each with
    /// the position to sync from: the device's delivery position raised to the member's
    /// floor (`visible_from_seq - 1`), also when the device has no position. Only rows
    /// with that position below `member.visible_upper(conversation.last_seq)` are
    /// returned. As in `memberships`, each conversation is read in the same statement as
    /// its member row. The order of the result is unspecified.
    async fn sync_positions(
        &mut self,
        user: UserId,
        device: DeviceId,
    ) -> DomainResult<Vec<(Conversation, Member, i64)>>;

    /// The group-wide positions of `conversation` with its log ending at `last_seq`: the
    /// minimums over its current members of their effective positions. `None` if it has
    /// no current members.
    ///
    /// A member's effective position from a raw position `p` skips their own messages,
    /// edits, deletes, system entries and deleted messages: with
    /// `from = max(p, visible_from_seq - 1)`, it is `last_seq` if `from >= last_seq`,
    /// otherwise the `seq` before the first live message from someone else with
    /// `from < seq <= last_seq` (see `first_live_message`), or `last_seq` if there is none.
    /// Their effective read position starts from `read_seq`; their effective delivered
    /// position from `max(delivered_seq, read_seq)`, so it is never below the read one.
    ///
    /// Postgres computes it in one statement, however many members the group has.
    async fn group_positions(
        &mut self,
        conversation: ConversationId,
        last_seq: i64,
    ) -> DomainResult<Option<GroupPositions>>;

    /// An overview of each conversation `user` is, or was, a member of whose `updated_at`
    /// is later than `changed_since` (every one without it). Unread counts stop at
    /// `unread_cap`. Each conversation is read in the same statement as the user's member
    /// row, as in `memberships`. One statement on Postgres, however many conversations
    /// there are. The order of the overviews is unspecified; within each, `members` and
    /// `admins` are sorted by user id (Postgres: `ORDER BY user_id`).
    async fn conversation_overviews(
        &mut self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
        unread_cap: u32,
    ) -> DomainResult<Vec<ConversationOverview>>;

    // --- Conversation flags --------------------------------------------------------------

    /// Hides (`true`) or shows `user`'s view of `conversation`, stamping `at` as the time
    /// of the change. Returns whether the state changed; an unchanged state writes nothing.
    /// No stored row means shown. Fails with `DomainError::Store` when `user` has no member
    /// row in `conversation`.
    async fn set_conversation_hidden(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        hidden: bool,
        at: DateTime<Utc>,
    ) -> DomainResult<bool>;
    /// Shows `conversation` again to every member who hid it, stamping `at`. Returns those
    /// members, sorted.
    async fn unhide_conversation(
        &mut self,
        conversation: ConversationId,
        at: DateTime<Utc>,
    ) -> DomainResult<Vec<UserId>>;
    /// `user`'s flags of each direct conversation `user` is a member of that `user` ever
    /// hid or unhid, or whose other member is recorded deleted. `changed_at` is the later of
    /// the last hide or unhide and the deletion. The order is unspecified.
    async fn conversation_flags(&mut self, user: UserId) -> DomainResult<Vec<ConversationFlags>>;
    /// Records that `user`'s account was deleted at `at`. Returns whether it was recorded
    /// now: a repeated call keeps the first time and returns `false`. Fails with
    /// `DomainError::Store` when `user` is not known.
    async fn mark_user_deleted(&mut self, user: UserId, at: DateTime<Utc>) -> DomainResult<bool>;
    /// The direct conversations `user` is a member of, each with its other member. The
    /// order is unspecified.
    async fn direct_peers(&mut self, user: UserId) -> DomainResult<Vec<(ConversationId, UserId)>>;

    /// Whether `user` is recorded deleted ([`Tx::mark_user_deleted`]).
    async fn is_user_deleted(&mut self, user: UserId) -> DomainResult<bool>;

    // --- Agent connections (spec 2026-11-06 agent connections and the owner view) -------

    /// The stored policy of `agent`; `None` when never set (the agent asks).
    async fn agent_policy(&mut self, agent: UserId) -> DomainResult<Option<AgentPolicy>>;
    /// Stores `agent`'s policy, replacing any earlier one. Fails with `DomainError::Store`
    /// when `agent` is not a recorded agent ([`Tx::record_agent_owner`]).
    async fn set_agent_policy(
        &mut self,
        agent: UserId,
        policy: AgentPolicy,
        at: DateTime<Utc>,
    ) -> DomainResult<()>;
    /// Removes `agent`'s policy, its request log, its settings, its outgoing actions and
    /// its action log (account deletion). Idempotent.
    async fn forget_agent(&mut self, agent: UserId) -> DomainResult<()>;
    /// The requests waiting at `agent`: its `RequestedIn` rows, with the time each row was
    /// last written, oldest first (ties by user id).
    async fn waiting_requests(
        &mut self,
        agent: UserId,
    ) -> DomainResult<Vec<(UserId, DateTime<Utc>)>>;
    /// How many `RequestedIn` rows `agent` has.
    async fn waiting_in_count(&mut self, agent: UserId) -> DomainResult<u32>;
    /// `user`'s pending outgoing requests (`RequestedOut` rows) and, among them, those
    /// whose peer is a recorded agent holding a `RequestedIn` row back (waiting for
    /// another owner).
    async fn outgoing_counts(&mut self, user: UserId) -> DomainResult<(u32, u32)>;
    /// Records a refused request, to be released at `release_at`. Returns whether it was
    /// recorded now: an existing one keeps its first `release_at`. Fails with
    /// `DomainError::Store` when either user is not known.
    async fn put_refusal(
        &mut self,
        requester: UserId,
        agent: UserId,
        release_at: DateTime<Utc>,
    ) -> DomainResult<bool>;
    /// Removes a refused request. Returns whether there was one.
    async fn delete_refusal(&mut self, requester: UserId, agent: UserId) -> DomainResult<bool>;
    /// Up to `limit` refused requests due at `now` (`release_at <= now`), as
    /// `(requester, agent)`, earliest first.
    async fn due_refusals(
        &mut self,
        now: DateTime<Utc>,
        limit: u32,
    ) -> DomainResult<Vec<(UserId, UserId)>>;
    /// Logs one outgoing request of `agent`.
    async fn log_agent_request(
        &mut self,
        agent: UserId,
        peer: UserId,
        at: DateTime<Utc>,
    ) -> DomainResult<()>;
    /// How many requests `agent` logged at `since` or later, and the oldest of them.
    async fn agent_requests_since(
        &mut self,
        agent: UserId,
        since: DateTime<Utc>,
    ) -> DomainResult<(u32, Option<DateTime<Utc>>)>;
    /// Deletes logged requests older than `before`. Returns how many.
    async fn prune_agent_requests(&mut self, before: DateTime<Utc>) -> DomainResult<u64>;

    // --- Proactive agents (spec 2026-11-09 proactive agents) ----------------------------

    /// `agent`'s settings; the defaults when never set.
    async fn agent_settings(&mut self, agent: UserId) -> DomainResult<AgentSettings>;
    /// Stores `agent`'s settings, replacing any earlier ones. Fails with
    /// `DomainError::Store` when `agent` is not a recorded agent.
    async fn set_agent_settings(
        &mut self,
        agent: UserId,
        settings: &AgentSettings,
        at: DateTime<Utc>,
    ) -> DomainResult<()>;
    /// Stores a new action. Fails with `DomainError::Store` when its id exists or its
    /// agent is not a known user.
    async fn insert_outgoing(&mut self, action: &OutgoingAction) -> DomainResult<()>;
    /// The waiting action that `action` asks again ([`OutgoingAction::same_as`]), if any.
    async fn find_waiting_outgoing(
        &mut self,
        action: &OutgoingAction,
    ) -> DomainResult<Option<OutgoingAction>>;
    /// The action with this id, in any state.
    async fn outgoing(&mut self, action_id: Uuid) -> DomainResult<Option<OutgoingAction>>;
    /// How many of `agent`'s actions wait.
    async fn waiting_outgoing_count(&mut self, agent: UserId) -> DomainResult<u32>;
    /// `agent`'s waiting actions and, with `resolved_since`, those resolved at that time or
    /// later; oldest first (ties by id).
    async fn list_outgoing(
        &mut self,
        agent: UserId,
        resolved_since: Option<DateTime<Utc>>,
    ) -> DomainResult<Vec<OutgoingAction>>;
    /// Moves the action from `from` to `to`, stamping `resolved_at` with `at`, setting
    /// `error` and, when given, `conversation`. Returns the action as now stored; `None`,
    /// changing nothing, when there is no such action or it is not in state `from`. On
    /// Postgres one conditional `UPDATE`, so two resolutions cannot both succeed.
    async fn resolve_outgoing(
        &mut self,
        action_id: Uuid,
        from: OutgoingState,
        to: OutgoingState,
        error: Option<OutgoingError>,
        conversation: Option<ConversationId>,
        at: DateTime<Utc>,
    ) -> DomainResult<Option<OutgoingAction>>;
    /// An approved, unused call of `agent` to `callee` resolved at `since` or later; the
    /// earliest resolved.
    async fn approved_call_grant(
        &mut self,
        agent: UserId,
        callee: UserId,
        since: DateTime<Utc>,
    ) -> DomainResult<Option<Uuid>>;
    /// Marks an approved call used at `at`. Returns whether it was unused until now.
    async fn use_outgoing(&mut self, action_id: Uuid, at: DateTime<Utc>) -> DomainResult<bool>;
    /// Ends up to `limit` actions waiting since `before` or earlier as `Expired` at `at`,
    /// oldest first, and returns them as now stored.
    async fn expire_outgoing(
        &mut self,
        before: DateTime<Utc>,
        at: DateTime<Utc>,
        limit: u32,
    ) -> DomainResult<Vec<OutgoingAction>>;
    /// Deletes the resolved actions resolved before `before`. Returns how many.
    async fn prune_outgoing(&mut self, before: DateTime<Utc>) -> DomainResult<u64>;
    /// Logs `count` actions of `kind` that `agent` carried out or queued at `at`.
    async fn log_agent_action(
        &mut self,
        agent: UserId,
        kind: OutgoingKind,
        count: u32,
        at: DateTime<Utc>,
    ) -> DomainResult<()>;
    /// The sum of the counts `agent` logged for `kind` at `since` or later, and the oldest
    /// of those log rows.
    async fn agent_actions_since(
        &mut self,
        agent: UserId,
        kind: OutgoingKind,
        since: DateTime<Utc>,
    ) -> DomainResult<(u32, Option<DateTime<Utc>>)>;
    /// Deletes action log rows older than `before`. Returns how many.
    async fn prune_agent_actions(&mut self, before: DateTime<Utc>) -> DomainResult<u64>;
    /// For each conversation `user` is or was a member of, the messages `user` can see
    /// (`kind = Message`, within the visible range) that are not deleted. Conversations
    /// without any are left out. The order is unspecified.
    async fn message_counts(&mut self, user: UserId) -> DomainResult<Vec<(ConversationId, i64)>>;

    // --- Templates ---------------------------------------------------------------------

    /// Whether `user` is an owned agent: [`Tx::record_agent_owner`] recorded it as the
    /// agent. Unknown users are not agents.
    async fn is_agent(&mut self, user: UserId) -> DomainResult<bool>;
    /// The owner recorded for an owned agent ([`Tx::record_agent_owner`]); `None` for
    /// anyone else, unknown users included.
    async fn agent_owner(&mut self, user: UserId) -> DomainResult<Option<UserId>>;
    /// The stored version, or `None`.
    async fn template(&mut self, template: &TemplateRef) -> DomainResult<Option<StoredTemplate>>;
    /// The stored version, or `None`, read for a message that uses it. Holds a share lock
    /// on the version's row until the transaction ends (`SELECT … FOR SHARE`), so a review
    /// that revokes it waits for the message to commit, and a message read after the
    /// revocation commits sees it revoked.
    async fn template_for_send(
        &mut self,
        template: &TemplateRef,
    ) -> DomainResult<Option<StoredTemplate>>;
    /// The stored version, or `None`. Locks the version's row until the transaction ends
    /// (`SELECT … FOR UPDATE`), so reviews of one version are serialised.
    async fn lock_template(
        &mut self,
        template: &TemplateRef,
    ) -> DomainResult<Option<StoredTemplate>>;
    /// Stores a new version. Returns `Duplicate`, writing nothing, if the ref exists, so a
    /// stored definition never changes (Postgres: `ON CONFLICT DO NOTHING`). The row must
    /// satisfy the status invariants even when it is a duplicate (Postgres checks them
    /// before it looks for a conflict): `reviewed_at` is set unless pending, `change_seq`
    /// is set exactly when approved or revoked, it is the last number taken
    /// ([`Tx::next_template_change`], in this transaction), and no other version holds
    /// that `change_seq`; otherwise it fails with `DomainError::Store`. Also fails with
    /// `DomainError::Store` if the publisher has not been claimed
    /// ([`Tx::claim_publisher`]) or the submitter is not a known user.
    async fn insert_template(&mut self, template: StoredTemplate) -> DomainResult<InsertOutcome>;
    /// Moves the version to `status`, setting `reviewed_at` to `at` and `change_seq`. Only
    /// `pending → approved`, `pending → rejected` and `approved → revoked` are allowed
    /// (see [`TemplateStatus::can_become`]); any other move, an unknown ref, a `change_seq`
    /// that is not set exactly for approved and revoked, that is not the last number taken
    /// ([`Tx::next_template_change`], in this transaction), or that another version holds,
    /// fails with `DomainError::Store`.
    async fn set_template_status(
        &mut self,
        template: &TemplateRef,
        status: TemplateStatus,
        at: DateTime<Utc>,
        change_seq: Option<i64>,
    ) -> DomainResult<()>;
    /// Takes the next change number of the catalog (1 first). Locks the single counter
    /// until the transaction ends, so change numbers commit in the order they are taken
    /// and a reader that has seen `N` never misses a change below `N`. Take it last, after
    /// the publisher and version locks.
    async fn next_template_change(&mut self) -> DomainResult<i64>;
    /// The last change number handed out and committed (or taken by this transaction); 0
    /// when none was.
    async fn last_template_change(&mut self) -> DomainResult<i64>;
    /// Approved and revoked versions with `change_seq > since`, by `change_seq`, at most
    /// `limit`.
    async fn templates_changed_since(
        &mut self,
        since: i64,
        limit: u32,
    ) -> DomainResult<Vec<StoredTemplate>>;
    /// Versions with this status (every version for `None`), newest submission first (ties
    /// by ref), at most `limit`.
    async fn templates_with_status(
        &mut self,
        status: Option<TemplateStatus>,
        limit: u32,
    ) -> DomainResult<Vec<StoredTemplate>>;
    /// How many of `user`'s submissions are pending.
    async fn pending_templates_of(&mut self, user: UserId) -> DomainResult<u32>;
    /// Records `owner` as the owner of `publisher` if the publisher is new, and returns the
    /// owner recorded, which is the first claimant's. Locks the publisher until the
    /// transaction ends, so submissions under one publisher are serialised. Take it before
    /// the version and counter locks (a submission takes its user's lock before it).
    /// Fails with `DomainError::Store` if a user owner is not a known user.
    async fn claim_publisher(
        &mut self,
        publisher: &str,
        owner: PublisherOwner,
    ) -> DomainResult<PublisherOwner>;
    /// Appends to the version's audit trail. Fails with `DomainError::Store` if the version
    /// does not exist.
    async fn insert_template_event(&mut self, event: TemplateEvent) -> DomainResult<()>;
    /// The version's audit trail, oldest first. Empty for an unknown ref.
    async fn template_events(&mut self, template: &TemplateRef)
    -> DomainResult<Vec<TemplateEvent>>;

    // --- Call history ------------------------------------------------------------------

    /// Stores call history rows. A row whose `(user, call_id)` is stored already is
    /// skipped, keeping the stored one (Postgres: `ON CONFLICT DO NOTHING`), so recording
    /// an ended call again changes nothing and never shows a hidden row again. Fails with
    /// `DomainError::Store` if a user or peer is not a known user.
    async fn insert_call_history(&mut self, rows: &[CallHistoryRow]) -> DomainResult<()>;
    /// `user`'s rows that are not hidden, newest first by `(ended_at, call_id)`: only
    /// those strictly before `before` and strictly after `since` when given, at most
    /// `limit`.
    async fn call_history(
        &mut self,
        user: UserId,
        before: Option<HistoryCursor>,
        since: Option<HistoryCursor>,
        limit: u32,
    ) -> DomainResult<Vec<CallHistoryRow>>;
    /// Hides `user`'s row of `call` as of `at`. Returns whether a visible row was hidden:
    /// false when there is no such row of `user`'s or it is hidden already. Never touches
    /// another user's row.
    async fn hide_call(
        &mut self,
        user: UserId,
        call: Uuid,
        at: DateTime<Utc>,
    ) -> DomainResult<bool>;
    /// Deletes at most `batch` rows, of any user, that ended before `cutoff`, hidden or
    /// not. Returns how many were deleted.
    async fn delete_call_history_before(
        &mut self,
        cutoff: DateTime<Utc>,
        batch: u32,
    ) -> DomainResult<u64>;
}
