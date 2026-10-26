//! Domain records. These mirror the `messaging` schema in the spec (§3.2).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::templates::TemplateRef;

/// One direction of a contact relationship, as seen by its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactState {
    RequestedOut,
    RequestedIn,
    Accepted,
    Blocked,
}

/// One of a user's contact relationships, as that user sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContactSummary {
    /// The other user.
    pub user_id: UserId,
    pub state: ContactState,
}

/// What `Identify` reads about a connecting device, in one statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceState {
    /// The owner.
    pub user: UserId,
    /// Not revoked.
    pub active: bool,
    /// When message-svc last heard that the owner's password was reset, if ever.
    pub password_reset_at: Option<DateTime<Utc>>,
}

/// A stored invite (spec §5.5). Redeeming it makes the redeemer an accepted contact of
/// its creator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteRecord {
    pub code: String,
    /// The creator.
    pub user_id: UserId,
    pub expires_at: DateTime<Utc>,
    pub max_uses: u32,
    pub uses: u32,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl InviteRecord {
    /// Whether the invite can be redeemed at `now`: not revoked, not expired and not used
    /// up.
    pub fn is_usable(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && now < self.expires_at && self.uses < self.max_uses
    }
}

/// What the creator of an invite learns about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    /// Ten characters of Crockford base32, shown as a link or QR code.
    pub code: String,
    pub expires_at: DateTime<Utc>,
    pub max_uses: u32,
    pub uses: u32,
}

impl From<&InviteRecord> for Invite {
    fn from(record: &InviteRecord) -> Self {
        Self {
            code: record.code.clone(),
            expires_at: record.expires_at,
            max_uses: record.max_uses,
            uses: record.uses,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationKind {
    Direct,
    Group,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// A card from an approved template: JSON `{tpl, params, fallback}`.
    Rich,
    /// An answer to a card in the same conversation: JSON `{card, values, fallback}`.
    Answer,
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
    /// Cards (RICH messages with an interactive block): the `seq` of the first valid
    /// answer, once answered.
    pub answer_seq: Option<i64>,
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
    /// How far the others got, for delivery and read ticks.
    pub receipts: Receipts,
    /// When the conversation was created or last had an entry appended. Acks and reads do
    /// not move it.
    pub updated_at: DateTime<Utc>,
    /// Current members, sorted.
    pub members: Vec<UserId>,
    /// Current admins of a group, sorted. Empty for a direct conversation.
    pub admins: Vec<UserId>,
}

/// Receipt positions in a conversation listing. Delivered is never below read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receipts {
    /// The other member's positions; delivered is the furthest across their devices.
    Direct {
        peer_delivered_seq: i64,
        peer_read_seq: i64,
    },
    /// The group-wide positions over the current members (see
    /// [`ReceiptUpdate::Group`](crate::notify::ReceiptUpdate::Group)), never above what the
    /// listing user may see. Zero for a group without members.
    Group {
        all_delivered_seq: i64,
        all_read_seq: i64,
    },
}

/// Group-wide positions (spec §4.5): every live message up to `read` has been read, and
/// every one up to `delivered` delivered, by every current member other than its sender.
/// `delivered` is never below `read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupPositions {
    pub delivered: i64,
    pub read: i64,
}

/// What a conversation listing needs about one of a user's memberships. The store reads
/// these for all of a user's memberships at once (`Tx::conversation_overviews`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationOverview {
    pub conversation: Conversation,
    /// The listing user's own member row.
    pub member: Member,
    /// The current members, sorted.
    pub members: Vec<UserId>,
    /// The current members with the admin role, sorted.
    pub admins: Vec<UserId>,
    /// A direct conversation's other member, with their `delivered_seq`. `None` for a
    /// group, or if there is no other member.
    pub peer: Option<(Member, i64)>,
    /// Live messages (`kind = Message`, not deleted) from others with
    /// `max(read_seq, visible_from_seq - 1) < seq <= member.visible_upper(last_seq)`, at
    /// most the cap given to the store.
    pub unread: u32,
    /// A group's group-wide positions with its log ending at its `last_seq`; `None` for a
    /// direct conversation or a group without current members.
    pub group: Option<GroupPositions>,
    /// For a former member, the `server_ts` of the entry at their `left_at_seq`; `None`
    /// for a current member, or if that entry does not exist.
    pub left_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationState {
    pub conversation_id: ConversationId,
    /// The highest `seq` the caller may see.
    pub last_seq: i64,
    pub positions: Positions,
}

/// Delivery and read positions in a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Positions {
    /// A direct conversation: the positions of both members.
    Members(Vec<MemberState>),
    /// A group: only the group-wide positions, as in [`Receipts::Group`]. A member's own
    /// position in a group is private.
    Group {
        all_delivered_seq: i64,
        all_read_seq: i64,
    },
}

/// The unique key of the direct conversation between two users, independent of order.
pub fn direct_key(a: UserId, b: UserId) -> [u8; 32] {
    let (low, high) = if a <= b { (a, b) } else { (b, a) };
    let mut key = [0u8; 32];
    key[..16].copy_from_slice(low.0.as_bytes());
    key[16..].copy_from_slice(high.0.as_bytes());
    key
}

/// Where a template version stands in review. `pending → approved | rejected` and
/// `approved → revoked`; `rejected` and `revoked` are final.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TemplateStatus {
    Pending,
    Approved,
    Rejected,
    Revoked,
}

impl TemplateStatus {
    /// Whether a version may move from this status to `to`.
    pub fn can_become(self, to: TemplateStatus) -> bool {
        matches!(
            (self, to),
            (Self::Pending, Self::Approved)
                | (Self::Pending, Self::Rejected)
                | (Self::Approved, Self::Revoked)
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Revoked => "revoked",
        }
    }
}

/// A template version in the registry. Its identity and definition never change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTemplate {
    pub template: TemplateRef,
    pub title: String,
    /// The canonical JSON definition.
    pub definition: String,
    pub status: TemplateStatus,
    /// The submitting account; `None` for a built-in.
    pub submitted_by: Option<UserId>,
    pub submitted_at: DateTime<Utc>,
    /// The last review.
    pub reviewed_at: Option<DateTime<Utc>>,
    /// The catalog's change number, set when the version is approved, revoked or seeded.
    pub change_seq: Option<i64>,
}

/// What happened to a template version, in its audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateAction {
    Submit,
    Approve,
    Reject,
    Revoke,
    /// A built-in was stored at start-up.
    Seed,
}

impl TemplateAction {
    pub const ALL: [TemplateAction; 5] = [
        Self::Submit,
        Self::Approve,
        Self::Reject,
        Self::Revoke,
        Self::Seed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Submit => "submit",
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::Revoke => "revoke",
            Self::Seed => "seed",
        }
    }
}

/// One row of a template version's audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateEvent {
    pub template: TemplateRef,
    pub action: TemplateAction,
    /// The account that submitted.
    pub actor: Option<UserId>,
    /// The reviewer's label, for reviews.
    pub reviewer: Option<String>,
    pub note: Option<String>,
    pub at: DateTime<Utc>,
}

/// Who owns a template publisher id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublisherOwner {
    /// The server: the built-ins' publisher.
    Server,
    User(UserId),
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
