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

/// Who an agent accepts contact requests from (spec 2026-11-06 agent connections, D1).
/// Set by the agent's owner only; an agent without a stored policy asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentPolicy {
    /// Every request waits for the owner.
    #[default]
    Ask,
    /// Agents owned by the same owner are accepted; the rest wait.
    AutoOwn,
    /// The owner's accepted contacts, and agents owned by the owner or by those contacts,
    /// are accepted; the rest wait.
    AutoOwnerContacts,
    /// Everyone is accepted.
    AutoAny,
    /// Everyone but the owner is refused, as a decline looks to the requester.
    RefuseAll,
}

impl AgentPolicy {
    /// The stored code, 1 to 5 in the order above.
    pub fn code(self) -> i16 {
        match self {
            Self::Ask => 1,
            Self::AutoOwn => 2,
            Self::AutoOwnerContacts => 3,
            Self::AutoAny => 4,
            Self::RefuseAll => 5,
        }
    }

    /// The policy of a stored code.
    pub fn from_code(code: i16) -> Option<Self> {
        Some(match code {
            1 => Self::Ask,
            2 => Self::AutoOwn,
            3 => Self::AutoOwnerContacts,
            4 => Self::AutoAny,
            5 => Self::RefuseAll,
            _ => return None,
        })
    }
}

/// What an agent may do of one kind of action on its own (spec 2026-11-09 proactive
/// agents, D1). Set by the agent's owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutgoingPolicy {
    /// Carried out at once, within the agent's limits.
    Allowed,
    /// Waits for the owner's approval.
    Ask,
    /// Refused (`NotAllowedByOwner`).
    Off,
}

impl OutgoingPolicy {
    /// The stored code, 1 to 3 in the order above.
    pub fn code(self) -> i16 {
        match self {
            Self::Allowed => 1,
            Self::Ask => 2,
            Self::Off => 3,
        }
    }

    /// The policy of a stored code.
    pub fn from_code(code: i16) -> Option<Self> {
        Some(match code {
            1 => Self::Allowed,
            2 => Self::Ask,
            3 => Self::Off,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Ask => "ask",
            Self::Off => "off",
        }
    }
}

/// Who reaches an agent's backend (spec 2026-11-09 proactive agents, D8); enforced by
/// the bridge, stored by message-svc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundPolicy {
    /// The owner only.
    OwnerOnly,
    /// The owner and the owner's other agents.
    OwnerAgents,
    /// Every accepted contact.
    Contacts,
}

impl InboundPolicy {
    /// The stored code, 1 to 3 in the order above.
    pub fn code(self) -> i16 {
        match self {
            Self::OwnerOnly => 1,
            Self::OwnerAgents => 2,
            Self::Contacts => 3,
        }
    }

    /// The policy of a stored code.
    pub fn from_code(code: i16) -> Option<Self> {
        Some(match code {
            1 => Self::OwnerOnly,
            2 => Self::OwnerAgents,
            3 => Self::Contacts,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnerOnly => "owner_only",
            Self::OwnerAgents => "owner_agents",
            Self::Contacts => "contacts",
        }
    }
}

/// An agent's settings, set by its owner. An agent without stored settings has the
/// defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSettings {
    pub inbound: InboundPolicy,
    /// Contact requests.
    pub requests: OutgoingPolicy,
    /// New direct chats with someone other than the owner.
    pub chats: OutgoingPolicy,
    /// Creating groups and adding members.
    pub groups: OutgoingPolicy,
    /// Calls to someone other than the owner.
    pub calls: OutgoingPolicy,
}

impl Default for AgentSettings {
    /// Inbound `owner_only`; requests and chats `ask`; groups and calls `off`.
    fn default() -> Self {
        Self {
            inbound: InboundPolicy::OwnerOnly,
            requests: OutgoingPolicy::Ask,
            chats: OutgoingPolicy::Ask,
            groups: OutgoingPolicy::Off,
            calls: OutgoingPolicy::Off,
        }
    }
}

impl AgentSettings {
    /// The policy that applies to actions of `kind`.
    pub fn policy(&self, kind: OutgoingKind) -> OutgoingPolicy {
        match kind {
            OutgoingKind::Request => self.requests,
            OutgoingKind::Chat => self.chats,
            OutgoingKind::Group | OutgoingKind::Invite => self.groups,
            OutgoingKind::Call => self.calls,
        }
    }
}

/// A change of some of an agent's outgoing policies; `None` keeps the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutgoingPolicyChange {
    pub requests: Option<OutgoingPolicy>,
    pub chats: Option<OutgoingPolicy>,
    pub groups: Option<OutgoingPolicy>,
    pub calls: Option<OutgoingPolicy>,
}

impl OutgoingPolicyChange {
    /// `settings` with this change applied.
    pub fn apply(self, settings: AgentSettings) -> AgentSettings {
        AgentSettings {
            inbound: settings.inbound,
            requests: self.requests.unwrap_or(settings.requests),
            chats: self.chats.unwrap_or(settings.chats),
            groups: self.groups.unwrap_or(settings.groups),
            calls: self.calls.unwrap_or(settings.calls),
        }
    }
}

/// What an agent starts on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OutgoingKind {
    /// A contact request (`target`).
    Request,
    /// A new direct chat with `target`.
    Chat,
    /// A new group (`title`, `members`).
    Group,
    /// Members added to a group (`conversation`, `members`).
    Invite,
    /// A call to `target`.
    Call,
}

impl OutgoingKind {
    pub const ALL: [OutgoingKind; 5] = [
        Self::Request,
        Self::Chat,
        Self::Group,
        Self::Invite,
        Self::Call,
    ];

    /// The stored code, 1 to 5 in the order above.
    pub fn code(self) -> i16 {
        match self {
            Self::Request => 1,
            Self::Chat => 2,
            Self::Group => 3,
            Self::Invite => 4,
            Self::Call => 5,
        }
    }

    /// The kind of a stored code.
    pub fn from_code(code: i16) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.code() == code)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Chat => "chat",
            Self::Group => "group",
            Self::Invite => "invite",
            Self::Call => "call",
        }
    }
}

/// Where an outgoing action stands. `Waiting` becomes one of the others once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutgoingState {
    Waiting,
    /// The owner approved it and it was carried out (a call: may ring once).
    Approved,
    Declined,
    /// Not decided within 24 hours.
    Expired,
    /// The owner approved it, but the normal rules refused it (`error`).
    Failed,
}

impl OutgoingState {
    pub const ALL: [OutgoingState; 5] = [
        Self::Waiting,
        Self::Approved,
        Self::Declined,
        Self::Expired,
        Self::Failed,
    ];

    /// The stored code, 1 to 5 in the order above.
    pub fn code(self) -> i16 {
        match self {
            Self::Waiting => 1,
            Self::Approved => 2,
            Self::Declined => 3,
            Self::Expired => 4,
            Self::Failed => 5,
        }
    }

    /// The state of a stored code.
    pub fn from_code(code: i16) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.code() == code)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Approved => "approved",
            Self::Declined => "declined",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }
}

/// Why an approved action failed: the error the normal rules answered when it was carried
/// out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutgoingError {
    NotContact,
    Blocked,
    NotAdmin,
    NotMember,
    GroupFull,
    NotAGroup,
    ConversationNotFound,
    NotAllowed,
    RateLimited,
    Invalid,
    /// Anything else.
    Failed,
}

impl OutgoingError {
    pub const ALL: [OutgoingError; 11] = [
        Self::NotContact,
        Self::Blocked,
        Self::NotAdmin,
        Self::NotMember,
        Self::GroupFull,
        Self::NotAGroup,
        Self::ConversationNotFound,
        Self::NotAllowed,
        Self::RateLimited,
        Self::Invalid,
        Self::Failed,
    ];

    /// The stored code, 1 to 11 in the order above.
    pub fn code(self) -> i16 {
        match self {
            Self::NotContact => 1,
            Self::Blocked => 2,
            Self::NotAdmin => 3,
            Self::NotMember => 4,
            Self::GroupFull => 5,
            Self::NotAGroup => 6,
            Self::ConversationNotFound => 7,
            Self::NotAllowed => 8,
            Self::RateLimited => 9,
            Self::Invalid => 10,
            Self::Failed => 11,
        }
    }

    /// The error of a stored code.
    pub fn from_code(code: i16) -> Option<Self> {
        Self::ALL.into_iter().find(|error| error.code() == code)
    }

    /// The name used in owner routes: the wire error code's name in snake case.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotContact => "not_contact",
            Self::Blocked => "blocked",
            Self::NotAdmin => "not_admin",
            Self::NotMember => "not_member",
            Self::GroupFull => "group_full",
            Self::NotAGroup => "not_a_group",
            Self::ConversationNotFound => "conversation_not_found",
            Self::NotAllowed => "not_allowed",
            Self::RateLimited => "rate_limited",
            Self::Invalid => "invalid",
            Self::Failed => "failed",
        }
    }
}

/// Something an agent started on its own that its owner decides on (spec 2026-11-09
/// proactive agents §3), or decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingAction {
    pub action_id: uuid::Uuid,
    pub agent: UserId,
    pub kind: OutgoingKind,
    /// Request, chat and call: the other user.
    pub target: Option<UserId>,
    /// Invite: the group. Chat and group once approved: the conversation opened or
    /// created.
    pub conversation: Option<ConversationId>,
    /// Group: its title.
    pub title: Option<String>,
    /// Group and invite: the members, sorted, without duplicates.
    pub members: Vec<UserId>,
    pub state: OutgoingState,
    /// `Failed` only.
    pub error: Option<OutgoingError>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    /// An approved call: when the agent used it to ring.
    pub used_at: Option<DateTime<Utc>>,
}

impl OutgoingAction {
    /// Whether `other` is the same action asked again: same agent, kind, target,
    /// conversation, title and members.
    pub fn same_as(&self, other: &OutgoingAction) -> bool {
        self.agent == other.agent
            && self.kind == other.kind
            && self.target == other.target
            && self.conversation == other.conversation
            && self.title == other.title
            && self.members == other.members
    }
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
    /// An agent started it on its own: a direct chat with someone other than its owner,
    /// or a group it created.
    pub started_by_agent: bool,
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
    /// The user hid this direct conversation (listed only when hidden ones are asked for).
    pub hidden: bool,
    /// Direct only: the other member's account was deleted.
    pub peer_deleted: bool,
    /// An agent started it on its own (see [`Conversation::started_by_agent`]).
    pub started_by_agent: bool,
}

/// A user's flags of one direct conversation, as the store keeps them: only conversations
/// the user ever hid or unhid, or whose other member was deleted, have flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationFlags {
    pub conversation_id: ConversationId,
    pub hidden: bool,
    pub peer_deleted: bool,
    /// When a flag last changed: the later of the last hide or unhide and the peer's
    /// deletion.
    pub changed_at: DateTime<Utc>,
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
