//! Messaging rules for push-messenger.
//!
//! This crate holds the domain logic — contacts, conversations, the message log, delivery
//! and read positions — independent of storage and transport. Storage is reached through
//! the `Store` and `Tx` traits.

pub mod error;
pub mod ids;
pub mod validation;

pub use error::{DomainError, DomainResult};
pub use ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
pub use validation::{
    MAX_HISTORY_PAGE, MAX_SYNC_BATCH, MAX_TEXT_BYTES, ValidationError, validate_text,
};
