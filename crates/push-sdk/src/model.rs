//! The data the SDK hands to apps: plain values with no lifetimes or generics, so the
//! bindings can copy them across the FFI boundary.
//!
//! Times are microseconds since the Unix epoch, UTC, as on the wire; `seq` numbers are the
//! conversation log's positions (spec §4.8).
//!
//! `Debug` never shows what people wrote: message bodies and group titles appear as their
//! length only, so a `{:?}` in an app's log carries no message content.

use push_domain::{ClientMsgId, ConversationId, MessageId, UserId};
use push_proto::v1;

use crate::SdkError;

/// What kind of conversation it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConversationKind {
    /// Two users who are accepted contacts.
    Direct,
    /// A group. Not offered to apps in the first release: no operation creates one.
    Group,
    /// Learned from a delivery before any listing; the next listing tells.
    Unknown,
}

impl ConversationKind {
    pub(crate) fn code(self) -> i64 {
        match self {
            Self::Unknown => 0,
            Self::Direct => 1,
            Self::Group => 2,
        }
    }

    pub(crate) fn from_code(code: i64) -> Self {
        match code {
            1 => Self::Direct,
            2 => Self::Group,
            _ => Self::Unknown,
        }
    }

    pub(crate) fn from_wire(kind: v1::ConversationKind) -> Self {
        match kind {
            v1::ConversationKind::Direct => Self::Direct,
            v1::ConversationKind::Group => Self::Group,
            v1::ConversationKind::Unspecified => Self::Unknown,
        }
    }
}

/// What `Debug` shows instead of a text: its length.
pub(crate) struct Redacted(pub usize);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "<{} bytes>", self.0)
    }
}

pub(crate) fn redacted(text: &Option<String>) -> Option<Redacted> {
    text.as_ref().map(|text| Redacted(text.len()))
}

/// A conversation as this device knows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    /// The other member of a direct conversation.
    pub peer: Option<UserId>,
    /// Groups only.
    pub title: Option<String>,
    /// The highest `seq` known in the log.
    pub last_seq: i64,
    /// The user's own read position.
    pub read_seq: i64,
    /// Messages from others after `read_seq`, at most 100 ("99+").
    pub unread: u32,
    /// Direct: everything up to here reached one of the peer's devices.
    pub peer_delivered_seq: i64,
    /// Direct: everything up to here was read by the peer.
    pub peer_read_seq: i64,
    /// When it was created or last appended to.
    pub updated_at: i64,
}

impl std::fmt::Debug for Conversation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Conversation")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("peer", &self.peer)
            .field("title", &redacted(&self.title))
            .field("last_seq", &self.last_seq)
            .field("read_seq", &self.read_seq)
            .field("unread", &self.unread)
            .field("peer_delivered_seq", &self.peer_delivered_seq)
            .field("peer_read_seq", &self.peer_read_seq)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// What a message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// Text from a user.
    Text,
    /// A system entry: its body is a small JSON document (spec §6).
    System,
}

/// Where one of the user's own messages stands (spec §4.4). Others' messages are
/// [`MessageStatus::Received`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MessageStatus {
    /// In the outbox, not stored by the server yet.
    Pending,
    /// Stored by the server.
    Sent,
    /// Reached one of the peer's devices.
    Delivered,
    /// The peer read it.
    Read,
    /// Someone else's message.
    Received,
}

/// A message in a conversation, in its current state.
#[derive(Clone, PartialEq, Eq)]
pub struct Message {
    pub conversation_id: ConversationId,
    /// Chosen by the sending device; stable from the moment it was queued.
    pub client_msg_id: ClientMsgId,
    /// The server's id, once stored.
    pub message_id: Option<MessageId>,
    /// The position in the log, once stored.
    pub seq: Option<i64>,
    pub sender_id: UserId,
    pub kind: MessageKind,
    /// The text; `None` once deleted.
    pub body: Option<String>,
    pub client_ts: Option<i64>,
    pub server_ts: Option<i64>,
    pub edited_at: Option<i64>,
    pub deleted: bool,
    pub status: MessageStatus,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Message")
            .field("conversation_id", &self.conversation_id)
            .field("client_msg_id", &self.client_msg_id)
            .field("message_id", &self.message_id)
            .field("seq", &self.seq)
            .field("sender_id", &self.sender_id)
            .field("kind", &self.kind)
            .field("body", &redacted(&self.body))
            .field("client_ts", &self.client_ts)
            .field("server_ts", &self.server_ts)
            .field("edited_at", &self.edited_at)
            .field("deleted", &self.deleted)
            .field("status", &self.status)
            .finish()
    }
}

/// The user's relationship with another user, as the user sees it (spec §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContactState {
    None,
    /// The user asked them.
    RequestedOut,
    /// They asked the user.
    RequestedIn,
    Accepted,
    /// The user blocked them.
    Blocked,
}

impl ContactState {
    pub(crate) fn from_wire(state: v1::ContactState) -> Self {
        match state {
            v1::ContactState::None => Self::None,
            v1::ContactState::RequestedOut => Self::RequestedOut,
            v1::ContactState::RequestedIn => Self::RequestedIn,
            v1::ContactState::Accepted => Self::Accepted,
            v1::ContactState::Blocked => Self::Blocked,
        }
    }

    pub(crate) fn code(self) -> i64 {
        match self {
            Self::None => 0,
            Self::RequestedOut => 1,
            Self::RequestedIn => 2,
            Self::Accepted => 3,
            Self::Blocked => 4,
        }
    }

    pub(crate) fn from_code(code: i64) -> Self {
        match code {
            1 => Self::RequestedOut,
            2 => Self::RequestedIn,
            3 => Self::Accepted,
            4 => Self::Blocked,
            _ => Self::None,
        }
    }
}

/// One of the user's relationships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Contact {
    pub user_id: UserId,
    pub state: ContactState,
}

/// An invite code to share as a link or QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    /// Ten characters of Crockford base32.
    pub code: String,
    pub expires_at: i64,
    pub max_uses: u32,
    pub uses: u32,
}

/// What an outbox entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutboxKind {
    Send,
    Edit,
    Delete,
}

impl OutboxKind {
    pub(crate) fn code(self) -> i64 {
        match self {
            Self::Send => 1,
            Self::Edit => 2,
            Self::Delete => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Self {
        match code {
            2 => Self::Edit,
            3 => Self::Delete,
            _ => Self::Send,
        }
    }
}

/// The connection to the gateway (spec §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    /// Not connecting: the app has not called `connect`, or called `disconnect`.
    Offline,
    /// Opening the socket and identifying.
    Connecting,
    /// `Ready`: deliveries flow and the outbox sends.
    Connected,
    /// Waiting before the next attempt, after attempt number `attempt` failed.
    Reconnecting { attempt: u32, retry_in_ms: u64 },
    /// The credentials are no longer accepted (a password reset, or the refresh token was
    /// refused): sign in again. Local data and the outbox are kept for the same user.
    SignedOut,
    /// The server revoked this device (close code 4003): the client stopped and
    /// reconnects only when the app calls `connect` again. Local data is kept.
    DeviceRemoved,
    /// Another copy of this app's data took over the device's connection (close code
    /// 4007): the same app opened twice, or restored on a second machine. The device is
    /// still signed in and local data is kept; the client stopped and reconnects only
    /// when the app calls `connect` again (which takes the connection back).
    OpenedElsewhere,
}

/// What the SDK tells the UI, in one ordered stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    ConnectionStateChanged(ConnectionState),
    /// A message the UI has not seen: the user's own just queued (the local echo), or one
    /// delivered.
    MessageAdded(Message),
    /// One of the user's messages was stored by the server: it has its `seq` now.
    MessageSent(Message),
    MessageEdited(Message),
    /// A message was deleted: its body is gone.
    MessageDeleted(Message),
    /// The server refused a send, edit or delete for good (not a contact, the time
    /// window passed, …). It left the outbox; a refused message's local echo is removed.
    SendFailed {
        conversation_id: ConversationId,
        client_msg_id: ClientMsgId,
        kind: OutboxKind,
        error: SdkError,
    },
    /// The peer's positions moved: the user's messages up to them are delivered or read.
    ReceiptChanged {
        conversation_id: ConversationId,
        peer_delivered_seq: i64,
        peer_read_seq: i64,
    },
    /// A conversation appeared or its summary changed (last message, read position,
    /// unread count).
    ConversationChanged(Conversation),
    ContactChanged(Contact),
    /// Sign in again (a password reset, or the credentials were refused). Also reported
    /// as [`ConnectionState::SignedOut`].
    SignedOut,
    /// This device was removed. Also reported as [`ConnectionState::DeviceRemoved`].
    DeviceRemoved,
    /// This device's data was opened elsewhere. Also reported as
    /// [`ConnectionState::OpenedElsewhere`].
    OpenedElsewhere,
    /// The app stopped reading events and the queue filled up (10,000 events): newer ones
    /// were dropped, and this event stands in their place. Nothing was lost from the
    /// store: reload what the UI shows from the client's local data (conversations,
    /// messages, contacts).
    EventsLost,
}
