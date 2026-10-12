//! Domain records. These mirror the `messaging` schema in the spec (§3.2).

use chrono::{DateTime, Utc};

use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};

/// One direction of a contact relationship, as seen by its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactState {
    RequestedOut,
    RequestedIn,
    Accepted,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationKind {
    Direct,
    Group,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    Member,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Message,
    Edit,
    Delete,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Text,
    SystemEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    /// Direct conversations only; unique per pair of users.
    pub direct_key: Option<[u8; 32]>,
    pub title: Option<String>,
    pub created_by: UserId,
    /// Highest `seq` written; the single-writer counter.
    pub last_seq: i64,
    pub created_at: DateTime<Utc>,
    /// When the conversation was created or last had an entry appended.
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub conversation_id: ConversationId,
    pub user_id: UserId,
    pub role: Role,
    /// First `seq` this member may see.
    pub visible_from_seq: i64,
    /// Last `seq` visible after leaving; `None` while a current member.
    pub left_at_seq: Option<i64>,
    pub read_seq: i64,
}

impl Member {
    pub fn is_current(&self) -> bool {
        self.left_at_seq.is_none()
    }

    /// Highest `seq` this member may see, given the conversation's `last_seq`.
    pub fn visible_upper(&self, last_seq: i64) -> i64 {
        self.left_at_seq.unwrap_or(last_seq)
    }
}

/// One entry in a conversation's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub conversation_id: ConversationId,
    pub seq: i64,
    pub message_id: MessageId,
    pub client_msg_id: ClientMsgId,
    pub kind: EntryKind,
    /// Edit and delete entries: the `seq` of the message they change.
    pub target_seq: Option<i64>,
    pub sender_id: UserId,
    pub sender_device_id: Option<DeviceId>,
    pub content_type: ContentType,
    pub body: Option<Vec<u8>>,
    pub client_ts: Option<DateTime<Utc>>,
    pub server_ts: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub edit_seq: Option<i64>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// What the sender learns once its message is durably stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReceipt {
    pub client_msg_id: ClientMsgId,
    pub message_id: MessageId,
    pub seq: i64,
    pub server_ts: DateTime<Utc>,
}

impl From<&Entry> for SendReceipt {
    fn from(entry: &Entry) -> Self {
        Self {
            client_msg_id: entry.client_msg_id,
            message_id: entry.message_id,
            seq: entry.seq,
            server_ts: entry.server_ts,
        }
    }
}

/// Delivery and read positions of one member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberState {
    pub user_id: UserId,
    /// Furthest delivery position across all of the member's devices. Positions of revoked
    /// devices count: a delivery that happened stays delivered. Reported as at least
    /// `read_seq`, since reading implies delivery.
    pub delivered_seq: i64,
    pub read_seq: i64,
}

/// One conversation in a user's conversation list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSummary {
    pub conversation_id: ConversationId,
    pub kind: ConversationKind,
    pub title: Option<String>,
    /// The highest `seq` the user may see: the conversation's `last_seq`, or where the
    /// user left it.
    pub last_seq: i64,
    /// The user's own read position.
    pub read_seq: i64,
    /// Live messages from others after `read_seq` within the visible range, at most
    /// `MAX_UNREAD`.
    pub unread: u32,
    /// Direct conversations: the other member's delivered position, the furthest across
    /// their devices, and never below `peer_read_seq`. Zero for other conversations; Plan
    /// 2B replaces or extends these fields with group aggregates.
    pub peer_delivered_seq: i64,
    /// Direct conversations: the other member's read position. Zero for other
    /// conversations.
    pub peer_read_seq: i64,
    /// When the conversation was created or last had an entry appended. Acks and reads do
    /// not move it.
    pub updated_at: DateTime<Utc>,
    /// Current members, sorted.
    pub members: Vec<UserId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationState {
    pub conversation_id: ConversationId,
    pub last_seq: i64,
    pub members: Vec<MemberState>,
}

/// The unique key of the direct conversation between two users, independent of order.
pub fn direct_key(a: UserId, b: UserId) -> [u8; 32] {
    let (low, high) = if a <= b { (a, b) } else { (b, a) };
    let mut key = [0u8; 32];
    key[..16].copy_from_slice(low.0.as_bytes());
    key[16..].copy_from_slice(high.0.as_bytes());
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_key_is_the_same_whichever_user_opens_the_chat() {
        let (a, b) = (UserId::new(), UserId::new());
        assert_eq!(direct_key(a, b), direct_key(b, a));
    }

    #[test]
    fn direct_key_differs_between_pairs() {
        let (a, b, c) = (UserId::new(), UserId::new(), UserId::new());
        assert_ne!(direct_key(a, b), direct_key(a, c));
    }
}
