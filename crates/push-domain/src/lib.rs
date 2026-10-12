//! Messaging rules for push-messenger.
//!
//! This crate holds the domain logic — contacts, conversations, the message log, delivery
//! and read positions — independent of storage and transport. Storage is reached through
//! the `Store` and `Tx` traits.

pub mod clock;
mod contacts;
mod conversations;
mod edit;
pub mod error;
mod history;
pub mod ids;
pub mod memory;
pub mod model;
pub mod notify;
mod read;
mod send;
mod service;
pub mod store;
mod sync;
pub mod validation;

pub use clock::{Clock, ManualClock, SystemClock};
pub use edit::{DeleteMessage, EditText, MessageRef};
pub use error::{DomainError, DomainResult};
pub use history::HistoryPage;
pub use ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
pub use memory::InMemoryStore;
pub use model::{
    ContactState, ContentType, Conversation, ConversationKind, ConversationState, Entry, EntryKind,
    Member, MemberState, Role, SendReceipt, direct_key,
};
pub use notify::{Event, NoopNotifier, Notifier, ReceiptUpdate, RecordingNotifier};
pub use send::SendText;
pub use service::{Messaging, Policy};
pub use store::{InsertOutcome, Store, Tx};
pub use sync::SyncBatch;
pub use validation::{
    MAX_HISTORY_PAGE, MAX_SYNC_BATCH, MAX_TEXT_BYTES, ValidationError, validate_text,
};
