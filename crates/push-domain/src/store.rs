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
//! - roll back a transaction that is dropped without [`Tx::commit`].
//!
//! [`DomainError::Conflict`]: crate::error::DomainError::Conflict

use async_trait::async_trait;

use crate::error::DomainResult;
use crate::ids::{ClientMsgId, ConversationId, DeviceId, UserId};
use crate::model::{ContactState, Conversation, Entry, Member};

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
    /// Records a device. Idempotent.
    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<()>;
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
    async fn put_contact(
        &mut self,
        owner: UserId,
        peer: UserId,
        state: ContactState,
    ) -> DomainResult<()>;
    async fn delete_contact(&mut self, owner: UserId, peer: UserId) -> DomainResult<()>;

    // --- Conversations and members -----------------------------------------------------

    async fn find_direct(&mut self, key: [u8; 32]) -> DomainResult<Option<ConversationId>>;
    /// `Duplicate` if a direct conversation with the same key already exists.
    async fn insert_conversation(
        &mut self,
        conversation: Conversation,
    ) -> DomainResult<InsertOutcome>;
    async fn get_conversation(&mut self, id: ConversationId) -> DomainResult<Option<Conversation>>;
    /// Locks the conversation, increments `last_seq` and returns the new value.
    /// Fails with `ConversationNotFound` if the conversation does not exist.
    async fn next_seq(&mut self, id: ConversationId) -> DomainResult<i64>;
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

    // --- The log -----------------------------------------------------------------------

    /// `Duplicate` if the conversation already has an entry with this `client_msg_id`.
    async fn insert_entry(&mut self, entry: Entry) -> DomainResult<InsertOutcome>;
    async fn entry_by_client_id(
        &mut self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
    ) -> DomainResult<Option<Entry>>;
    /// Entries with `after < seq <= up_to`, oldest first, at most `limit`.
    async fn entries_after(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>>;
    /// The newest `limit` entries with `from <= seq < before`, returned oldest first.
    async fn entries_before(
        &mut self,
        conversation: ConversationId,
        before: i64,
        from: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>>;

    // --- Delivery positions ------------------------------------------------------------

    async fn get_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
    ) -> DomainResult<Option<i64>>;
    async fn create_cursor_if_absent(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<()>;
    /// Moves the position forward to `seq`, creating it if absent.
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
}
