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
//! # Delivery positions
//!
//! Positions are created eagerly, so a device normally has one for every membership of its
//! user. One can still be missing: a device registered concurrently with a membership
//! change reads the memberships before the new member row commits. Readers fall back to
//! `visible_from_seq - 1`, so [`Tx::sync_positions`] starts from the memberships and
//! joins the positions to them, never the other way round.
//!
//! [`DomainError::Conflict`]: crate::error::DomainError::Conflict

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::error::DomainResult;
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{
    ContactState, Conversation, ConversationOverview, Entry, GroupPositions, InviteRecord, Member,
};

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
    async fn user_exists(&mut self, user: UserId) -> DomainResult<bool>;
    /// Records an active device of `user` unless a device with this id exists (any owner,
    /// active or revoked), and returns whether the device is now an active device of
    /// `user`. Postgres checks after `INSERT … ON CONFLICT DO NOTHING`, which waits for a
    /// concurrent insert of the same id, so two registrations cannot both see `true` for
    /// different users. Fails with `DomainError::Store` if the device is new and the user
    /// is not known.
    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool>;
    /// Marks a device revoked. A no-op for an unknown device.
    async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()>;
    /// The owner of an active device; `None` if the device is unknown or revoked.
    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
    /// The user's active devices.
    /// The order of the result is unspecified.
    async fn devices_of_user(&mut self, user: UserId) -> DomainResult<Vec<DeviceId>>;

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
    /// A no-op if the row is absent.
    async fn delete_contact(&mut self, owner: UserId, peer: UserId) -> DomainResult<()>;

    /// Every row owned by `owner`, as `(peer, state)`. Empty for an unknown user.
    /// The order of the result is unspecified.
    async fn contacts_of(&mut self, owner: UserId) -> DomainResult<Vec<(UserId, ContactState)>>;

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
    /// A device registered concurrently with a membership change may lack a delivery
    /// position; callers treat a missing position as `visible_from_seq - 1`.
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
    /// `body`, `edited_at`, `edit_seq` and `deleted_at`. Every other field of `entry` is
    /// ignored. Fails with `DomainError::Store` if no such entry exists.
    async fn update_entry(&mut self, entry: Entry) -> DomainResult<()>;
    /// Sets `body` to null on every `Edit` entry whose `target_seq` is `target_seq`.
    /// A no-op if there are none. Postgres serves it from the index on
    /// `(conversation_id, target_seq)`.
    async fn erase_edit_bodies(
        &mut self,
        conversation: ConversationId,
        target_seq: i64,
    ) -> DomainResult<()>;
    /// Entries with `after < seq <= up_to`, oldest first, at most `limit`. Entries with a
    /// `target_seq` below `targets_from` (edits and deletes of messages outside the
    /// reader's visible range) are skipped and do not count towards `limit`.
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
    /// none. On Postgres one range scan of the primary key: `… ORDER BY seq LIMIT 1`.
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
}
