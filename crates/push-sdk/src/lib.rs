//! push-sdk: the client core of push-messenger, which third-party apps embed (spec §8).
//!
//! It owns everything below the user interface: signing up and in through auth-svc
//! (email and password, a Microsoft Entra ID access token the app obtained, or an
//! agent's key through the app's [`AgentSigner`]), the device's credentials (in a
//! [`SecretStore`] the app provides), the WebSocket
//! connection to the gateway with its reconnects, a local SQLite store of conversations,
//! messages and contacts (encrypted as a whole when the app gives a key), the outbox that
//! sends messages, edits and deletes exactly once, and one stream of events for the UI.
//!
//! The Node and C bindings (Plan 10) wrap this crate and add no logic. The public types
//! are therefore plain data: identifiers, strings, numbers and enums, with no lifetimes
//! or generics.

mod agent_key;
mod auth;
mod cards;
mod client;
mod connection;
mod credentials;
mod engine;
mod error;
mod key;
mod model;
mod policy;
mod secrets;
mod store;
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub mod testing;

pub use agent_key::{
    AGENT_ASSERTION_LIFETIME_S, AgentClaims, AgentPublicKey, AgentSigner, SignerError,
    assemble_assertion,
};
pub use auth::{Device, Platform, Profile};
pub use cards::{CardBook, CardCheck, CardRefusal, OpenCard};
pub use client::{Account, Client, Config, EDIT_WINDOW, Events};
pub use error::SdkError;
pub use key::DatabaseKey;
pub use model::{
    Activity, ActivityState, ConnectionState, Contact, ContactState, Conversation,
    ConversationKind, Event, Invite, Message, MessageKind, MessageStatus, OutboxKind, Presence,
    PresenceState,
};
pub use push_domain::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
pub use secrets::{MemorySecretStore, SECRET_KEYS, SecretError, SecretStore};

#[cfg(test)]
mod tests {
    /// build.rs compiles SQLite3 Multiple Ciphers and libsqlite3-sys links to it, not to a
    /// system SQLite that would ignore the database key.
    #[test]
    fn the_linked_sqlite_can_encrypt() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        let version: String = connection
            .query_row("SELECT sqlite3mc_version()", [], |row| row.get(0))
            .unwrap();
        assert!(version.starts_with("SQLite3 Multiple Ciphers"), "{version}");
    }
}
