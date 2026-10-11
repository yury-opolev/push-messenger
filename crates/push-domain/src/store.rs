//! The storage boundary. Every service operation runs inside one [`Tx`].
//!
//! Implementations must:
//! - give each transaction a consistent view;
//! - make [`Tx::next_seq`] lock the conversation until the transaction ends, so exactly one
//!   transaction at a time can append to a conversation;
//! - roll back a transaction that is dropped without [`Tx::commit`].

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
    async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()>;
    /// The owner of an active device; `None` if the device is unknown or revoked.
    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
    /// The user's active devices.
    async fn devices_of_user(&mut self, user: UserId) -> DomainResult<Vec<DeviceId>>;

    // --- Contacts: one row per direction -----------------------------------------------

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
    async fn next_seq(&mut self, id: ConversationId) -> DomainResult<i64>;
    async fn insert_member(&mut self, member: Member) -> DomainResult<()>;
    async fn get_member(
        &mut self,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<Option<Member>>;
    async fn members(&mut self, conversation: ConversationId) -> DomainResult<Vec<Member>>;
    /// Every conversation the user is, or was, a member of.
    async fn memberships(&mut self, user: UserId) -> DomainResult<Vec<(Conversation, Member)>>;
    /// Moves `read_seq` forward to `seq`. Returns whether it moved.
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
    /// Moves the position forward to `seq`, creating it if absent. Returns whether it moved.
    async fn raise_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<bool>;
}
