# Plan 1 — Messaging Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `push-domain`, the storage- and transport-independent messaging core: contacts, direct conversations, sending with gapless sequence numbers and idempotent retry, per-device sync with cumulative acks, read positions, and history.

**Architecture:** All rules live in a `Messaging` service that runs each operation inside one storage transaction, reached through `Store`/`Tx` traits. This plan ships an in-memory store whose transactions are fully serialised, so the rules can be tested exhaustively without a database; Plan 3 implements the same traits on Postgres. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §3.2, §4, §5.5.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85), tokio, async-trait, chrono, uuid (v7), thiserror.

---

## Before you start

- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`.
- The toolchain is rustup-managed (`C:\Users\yurio\.cargo\bin`). If `cargo --version` prints
  something older than 1.85, or `cargo clippy` is missing, the shell has a stale PATH. Fix it
  for the current PowerShell session:

  ```powershell
  $env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
  ```

- **Testing rule:** every test in this plan is written before the code it tests, and is run
  once to watch it fail.
- **Scope rule:** this plan covers direct (1:1) conversations only. Edit, delete, invites and
  groups are Plan 2. Do not add them early.

## Key rules the code must preserve (from the spec)

- `seq` is gapless per conversation. A retried send rolls back so `last_seq` is not consumed.
- Never acknowledge (return a receipt) before the transaction commits. Notify after commit.
- Delivery positions move only on `ack` from that device — never on send.
- Positions only move forward.
- Delivery positions are created eagerly: when a device registers (at each conversation's
  current `last_seq`) and when a conversation is created (at 0).

## File structure

```
Cargo.toml                                  workspace
README.md
.github/workflows/ci.yml                    fmt, clippy, tests
crates/push-domain/
├── Cargo.toml
├── src/
│   ├── lib.rs              module list and re-exports
│   ├── ids.rs              UserId, DeviceId, ConversationId, MessageId, ClientMsgId
│   ├── error.rs            DomainError
│   ├── validation.rs       limits and text validation
│   ├── model.rs            Conversation, Member, Entry, ContactState, receipts
│   ├── clock.rs            Clock, SystemClock, ManualClock
│   ├── notify.rs           Notifier, NoopNotifier, RecordingNotifier
│   ├── store.rs            Store and Tx traits
│   ├── memory.rs           InMemoryStore
│   ├── service.rs          Messaging, users and devices
│   ├── contacts.rs         contact requests, accept, decline, block
│   ├── conversations.rs    open_direct and lookup helpers
│   ├── send.rs             send_text
│   ├── sync.rs             sync and ack
│   ├── read.rs             read_up_to and conversation_state
│   └── history.rs          history pages
└── tests/
    ├── common/mod.rs       World test harness
    ├── devices.rs
    ├── contacts.rs
    ├── direct.rs
    ├── send.rs
    ├── sync.rs
    ├── read_state.rs
    └── history.rs
```

---

### Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml`
- Create: `README.md`
- Create: `crates/push-domain/Cargo.toml`
- Create: `crates/push-domain/src/lib.rs`

- [ ] **Step 1: Create the workspace manifest**

`Cargo.toml`:

```toml
[workspace]
resolver = "3"
members = ["crates/push-domain"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "MIT"
repository = "https://github.com/yury-opolev/push-messenger"

[workspace.dependencies]
async-trait = "0.1"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
thiserror = "2"
tokio = { version = "1", features = ["sync"] }
uuid = { version = "1", features = ["v7"] }

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

- [ ] **Step 2: Create the crate manifest**

`crates/push-domain/Cargo.toml`:

```toml
[package]
name = "push-domain"
description = "Messaging rules for push-messenger, independent of storage and transport."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
async-trait.workspace = true
chrono.workspace = true
thiserror.workspace = true
tokio.workspace = true
uuid.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }

[lints]
workspace = true
```

- [ ] **Step 3: Create the library root**

`crates/push-domain/src/lib.rs`:

```rust
//! Messaging rules for push-messenger.
//!
//! This crate holds the domain logic — contacts, conversations, the message log, delivery
//! and read positions — independent of storage and transport. Storage is reached through
//! the `Store` and `Tx` traits.
```

- [ ] **Step 4: Create the README**

`README.md`:

```markdown
# push-messenger

A minimal, expandable messenger backend and client SDK, written in Rust.
We build the server side and an SDK; third parties build the user interfaces.

- Design: [`docs/superpowers/specs/2026-10-11-push-messenger-design.md`](docs/superpowers/specs/2026-10-11-push-messenger-design.md)
- Roadmap: [`docs/superpowers/plans/2026-10-11-roadmap.md`](docs/superpowers/plans/2026-10-11-roadmap.md)

## Build and test

    cargo test --workspace

## License

MIT
```

- [ ] **Step 5: Build**

Run: `cargo build --workspace`
Expected: `Finished` with no warnings, and a `Cargo.lock` created.

- [ ] **Step 6: Commit**

```powershell
git add Cargo.toml Cargo.lock README.md crates/push-domain
git commit -m "build: add Rust workspace with push-domain crate"
```

---

### Task 2: Identifiers, validation and errors

**Files:**
- Create: `crates/push-domain/src/ids.rs`
- Create: `crates/push-domain/src/validation.rs`
- Create: `crates/push-domain/src/error.rs`
- Modify: `crates/push-domain/src/lib.rs`

- [ ] **Step 1: Write the failing validation tests**

`crates/push-domain/src/validation.rs`:

```rust
//! Input rules shared by the server and, later, the SDK.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_text() {
        assert_eq!(validate_text("hello".as_bytes()), Ok(()));
    }

    #[test]
    fn rejects_an_empty_body() {
        assert_eq!(validate_text(b""), Err(ValidationError::EmptyBody));
    }

    #[test]
    fn accepts_a_body_exactly_at_the_limit() {
        assert_eq!(validate_text(&vec![b'a'; MAX_TEXT_BYTES]), Ok(()));
    }

    #[test]
    fn rejects_a_body_one_byte_over_the_limit() {
        assert_eq!(
            validate_text(&vec![b'a'; MAX_TEXT_BYTES + 1]),
            Err(ValidationError::BodyTooLarge {
                actual: MAX_TEXT_BYTES + 1,
                limit: MAX_TEXT_BYTES,
            })
        );
    }

    #[test]
    fn rejects_invalid_utf8() {
        assert_eq!(validate_text(&[0xff, 0xfe]), Err(ValidationError::NotUtf8));
    }
}
```

Add to `crates/push-domain/src/lib.rs`:

```rust
pub mod validation;
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p push-domain --lib validation`
Expected: compile error — `validate_text`, `ValidationError` and `MAX_TEXT_BYTES` not found.

- [ ] **Step 3: Implement validation**

Insert above the `#[cfg(test)]` block in `crates/push-domain/src/validation.rs`:

```rust
/// Largest text message body, in bytes.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// Largest history page.
pub const MAX_HISTORY_PAGE: u32 = 100;
/// Largest batch returned by one sync.
pub const MAX_SYNC_BATCH: u32 = 500;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("message body is empty")]
    EmptyBody,
    #[error("message body is {actual} bytes; the limit is {limit}")]
    BodyTooLarge { actual: usize, limit: usize },
    #[error("message body is not valid UTF-8")]
    NotUtf8,
    #[error("a user cannot target themselves")]
    SelfTarget,
}

/// Checks a text message body.
pub fn validate_text(body: &[u8]) -> Result<(), ValidationError> {
    if body.is_empty() {
        return Err(ValidationError::EmptyBody);
    }
    if body.len() > MAX_TEXT_BYTES {
        return Err(ValidationError::BodyTooLarge {
            actual: body.len(),
            limit: MAX_TEXT_BYTES,
        });
    }
    if std::str::from_utf8(body).is_err() {
        return Err(ValidationError::NotUtf8);
    }
    Ok(())
}
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p push-domain --lib validation`
Expected: `5 passed`.

- [ ] **Step 5: Add identifiers**

`crates/push-domain/src/ids.rs`:

```rust
//! Strongly typed identifiers. All are UUIDs; new ones are v7 (time-ordered), which keeps
//! database indexes compact.

use std::fmt;

use uuid::Uuid;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub Uuid);

        impl $name {
            /// A new, time-ordered identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// A verified account.
    UserId
);
id_type!(
    /// A registered device of an account.
    DeviceId
);
id_type!(
    /// A direct or group conversation.
    ConversationId
);
id_type!(
    /// A log entry, assigned by the server.
    MessageId
);
id_type!(
    /// A log entry's idempotency key, chosen by the client before the first attempt.
    ClientMsgId
);
```

- [ ] **Step 6: Add the error type**

`crates/push-domain/src/error.rs`:

```rust
use crate::validation::ValidationError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    #[error("invalid input: {0}")]
    Invalid(#[from] ValidationError),
    #[error("unknown user")]
    UnknownUser,
    #[error("unknown or revoked device")]
    UnknownDevice,
    #[error("conversation not found")]
    ConversationNotFound,
    #[error("not a member of this conversation")]
    NotMember,
    #[error("the users are not accepted contacts")]
    NotContact,
    #[error("there is no pending contact request")]
    NoPendingRequest,
    #[error("the contact is blocked")]
    Blocked,
    #[error("this client message id was already used by another sender")]
    ClientIdConflict,
    #[error("the sequence number is outside the conversation")]
    SeqOutOfRange,
    #[error("storage error: {0}")]
    Store(String),
}

pub type DomainResult<T> = Result<T, DomainError>;
```

- [ ] **Step 7: Export the new modules**

`crates/push-domain/src/lib.rs` — below the doc comment, the full module section is now:

```rust
pub mod error;
pub mod ids;
pub mod validation;

pub use error::{DomainError, DomainResult};
pub use ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
pub use validation::{
    MAX_HISTORY_PAGE, MAX_SYNC_BATCH, MAX_TEXT_BYTES, ValidationError, validate_text,
};
```

- [ ] **Step 8: Build and test**

Run: `cargo test -p push-domain`
Expected: `5 passed`, no warnings.

- [ ] **Step 9: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): add identifiers, validation rules and error type"
```

---

### Task 3: Domain model, clock, notifier and storage traits

**Files:**
- Create: `crates/push-domain/src/model.rs`
- Create: `crates/push-domain/src/clock.rs`
- Create: `crates/push-domain/src/notify.rs`
- Create: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/lib.rs`

- [ ] **Step 1: Write the failing model and clock tests**

`crates/push-domain/src/model.rs`:

```rust
//! Domain records. These mirror the `messaging` schema in the spec (§3.2).

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
```

`crates/push-domain/src/clock.rs`:

```rust
//! Time source, injectable so tests control it.

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, TimeZone};

    #[test]
    fn manual_clock_returns_the_time_it_was_given_and_advances() {
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let clock = ManualClock::new(start);
        assert_eq!(clock.now(), start);
        clock.advance(TimeDelta::minutes(5));
        assert_eq!(clock.now(), start + TimeDelta::minutes(5));
    }
}
```

Add to `crates/push-domain/src/lib.rs` module list:

```rust
pub mod clock;
pub mod model;
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p push-domain --lib`
Expected: compile errors — `direct_key`, `ManualClock`, `Utc` not found.

- [ ] **Step 3: Implement the model**

Insert above the `#[cfg(test)]` block in `crates/push-domain/src/model.rs`:

```rust
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
    /// Furthest delivery position across the member's active devices.
    pub delivered_seq: i64,
    pub read_seq: i64,
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
```

- [ ] **Step 4: Implement the clock**

Insert above the `#[cfg(test)]` block in `crates/push-domain/src/clock.rs`:

```rust
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeDelta, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The real wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that only moves when told to. For tests.
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<DateTime<Utc>>,
}

impl ManualClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    pub fn set(&self, to: DateTime<Utc>) {
        *self.now.lock().expect("clock lock poisoned") = to;
    }

    pub fn advance(&self, by: TimeDelta) {
        *self.now.lock().expect("clock lock poisoned") += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("clock lock poisoned")
    }
}

impl<T: Clock + ?Sized> Clock for Arc<T> {
    fn now(&self) -> DateTime<Utc> {
        (**self).now()
    }
}
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p push-domain --lib`
Expected: `8 passed`.

- [ ] **Step 6: Add the notifier**

`crates/push-domain/src/notify.rs`:

```rust
//! The doorbell. After a commit, the service tells the transport layer which users have
//! something new. It carries no message content, and losing a notification only delays
//! delivery: the next sync reads the log.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::ids::UserId;

#[async_trait]
pub trait Notifier: Send + Sync {
    /// Best effort; must not fail the operation that triggered it.
    async fn notify_users(&self, users: &[UserId]);
}

/// Discards notifications.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopNotifier;

#[async_trait]
impl Notifier for NoopNotifier {
    async fn notify_users(&self, _users: &[UserId]) {}
}

/// Records notifications. For tests.
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    calls: Mutex<Vec<Vec<UserId>>>,
}

impl RecordingNotifier {
    /// Returns and clears the notifications recorded so far, one list per call.
    pub fn take(&self) -> Vec<Vec<UserId>> {
        std::mem::take(&mut *self.calls.lock().expect("notifier lock poisoned"))
    }
}

#[async_trait]
impl Notifier for RecordingNotifier {
    async fn notify_users(&self, users: &[UserId]) {
        self.calls
            .lock()
            .expect("notifier lock poisoned")
            .push(users.to_vec());
    }
}

#[async_trait]
impl<T: Notifier + ?Sized> Notifier for Arc<T> {
    async fn notify_users(&self, users: &[UserId]) {
        (**self).notify_users(users).await;
    }
}
```

- [ ] **Step 7: Add the storage traits**

`crates/push-domain/src/store.rs`:

```rust
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
    async fn get_conversation(&mut self, id: ConversationId)
    -> DomainResult<Option<Conversation>>;
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
```

- [ ] **Step 8: Export the new modules**

The module section of `crates/push-domain/src/lib.rs` is now:

```rust
pub mod clock;
pub mod error;
pub mod ids;
pub mod model;
pub mod notify;
pub mod store;
pub mod validation;

pub use clock::{Clock, ManualClock, SystemClock};
pub use error::{DomainError, DomainResult};
pub use ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
pub use model::{
    ContactState, ContentType, Conversation, ConversationKind, ConversationState, Entry,
    EntryKind, Member, MemberState, Role, SendReceipt, direct_key,
};
pub use notify::{NoopNotifier, Notifier, RecordingNotifier};
pub use store::{InsertOutcome, Store, Tx};
pub use validation::{
    MAX_HISTORY_PAGE, MAX_SYNC_BATCH, MAX_TEXT_BYTES, ValidationError, validate_text,
};
```

- [ ] **Step 9: Build and test**

Run: `cargo test -p push-domain`
Expected: `8 passed`, no warnings.

- [ ] **Step 10: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): add model, clock, notifier and storage traits"
```

---

### Task 4: In-memory store

**Files:**
- Create: `crates/push-domain/src/memory.rs`
- Modify: `crates/push-domain/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

`crates/push-domain/src/memory.rs`:

```rust
//! An in-memory [`Store`] for tests.
//!
//! Each transaction holds a store-wide lock and works on a copy of the state, so
//! transactions are fully serialised and rollback is free. Never begin a second
//! transaction while one is open in the same task: it would deadlock.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::MessageId;
    use crate::model::{ContentType, ConversationKind, EntryKind};
    use chrono::{TimeZone, Utc};

    fn conversation() -> Conversation {
        Conversation {
            id: ConversationId::new(),
            kind: ConversationKind::Direct,
            direct_key: None,
            title: None,
            created_by: UserId::new(),
            last_seq: 0,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        }
    }

    fn entry(conversation: ConversationId, seq: i64, client_msg_id: ClientMsgId) -> Entry {
        Entry {
            conversation_id: conversation,
            seq,
            message_id: MessageId::new(),
            client_msg_id,
            kind: EntryKind::Message,
            target_seq: None,
            sender_id: UserId::new(),
            sender_device_id: None,
            content_type: ContentType::Text,
            body: Some(b"hi".to_vec()),
            client_ts: None,
            server_ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        }
    }

    fn seqs(entries: &[Entry]) -> Vec<i64> {
        entries.iter().map(|entry| entry.seq).collect()
    }

    #[tokio::test]
    async fn committed_changes_are_visible_to_later_transactions() {
        let store = InMemoryStore::new();
        let user = UserId::new();
        let mut tx = store.begin().await.unwrap();
        tx.insert_user(user).await.unwrap();
        tx.commit().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert!(tx.user_exists(user).await.unwrap());
    }

    #[tokio::test]
    async fn rolled_back_and_dropped_transactions_leave_no_trace() {
        let store = InMemoryStore::new();
        let (rolled_back, dropped) = (UserId::new(), UserId::new());

        let mut tx = store.begin().await.unwrap();
        tx.insert_user(rolled_back).await.unwrap();
        tx.rollback().await.unwrap();
        {
            let mut tx = store.begin().await.unwrap();
            tx.insert_user(dropped).await.unwrap();
        }

        let mut tx = store.begin().await.unwrap();
        assert!(!tx.user_exists(rolled_back).await.unwrap());
        assert!(!tx.user_exists(dropped).await.unwrap());
    }

    #[tokio::test]
    async fn next_seq_counts_from_one_and_a_rollback_gives_the_number_back() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;

        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        assert_eq!(tx.next_seq(id).await.unwrap(), 1);
        tx.commit().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(tx.next_seq(id).await.unwrap(), 2);
        tx.rollback().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(tx.next_seq(id).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn a_second_direct_conversation_for_the_same_pair_is_a_duplicate() {
        let store = InMemoryStore::new();
        let mut first = conversation();
        first.direct_key = Some([7; 32]);
        let mut second = conversation();
        second.direct_key = Some([7; 32]);

        let mut tx = store.begin().await.unwrap();
        assert_eq!(
            tx.insert_conversation(first.clone()).await.unwrap(),
            InsertOutcome::Inserted
        );
        assert_eq!(
            tx.insert_conversation(second).await.unwrap(),
            InsertOutcome::Duplicate
        );
        assert_eq!(tx.find_direct([7; 32]).await.unwrap(), Some(first.id));
    }

    #[tokio::test]
    async fn a_reused_client_msg_id_is_a_duplicate() {
        let store = InMemoryStore::new();
        let conversation = ConversationId::new();
        let client_msg_id = ClientMsgId::new();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(
            tx.insert_entry(entry(conversation, 1, client_msg_id))
                .await
                .unwrap(),
            InsertOutcome::Inserted
        );
        assert_eq!(
            tx.insert_entry(entry(conversation, 2, client_msg_id))
                .await
                .unwrap(),
            InsertOutcome::Duplicate
        );
        let original = tx
            .entry_by_client_id(conversation, client_msg_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.seq, 1);
    }

    #[tokio::test]
    async fn entry_ranges_respect_their_bounds() {
        let store = InMemoryStore::new();
        let conversation = ConversationId::new();
        let mut tx = store.begin().await.unwrap();
        for seq in 1..=5 {
            tx.insert_entry(entry(conversation, seq, ClientMsgId::new()))
                .await
                .unwrap();
        }

        assert_eq!(
            seqs(&tx.entries_after(conversation, 1, 4, 10).await.unwrap()),
            vec![2, 3, 4]
        );
        assert_eq!(
            seqs(&tx.entries_after(conversation, 1, 5, 2).await.unwrap()),
            vec![2, 3]
        );
        assert_eq!(
            seqs(&tx.entries_after(conversation, 5, 5, 10).await.unwrap()),
            Vec::<i64>::new()
        );
        assert_eq!(
            seqs(&tx.entries_before(conversation, 5, 1, 2).await.unwrap()),
            vec![3, 4]
        );
        assert_eq!(
            seqs(&tx.entries_before(conversation, 3, 2, 10).await.unwrap()),
            vec![2]
        );
        assert_eq!(
            seqs(&tx.entries_before(conversation, 1, 1, 10).await.unwrap()),
            Vec::<i64>::new()
        );
    }

    #[tokio::test]
    async fn positions_only_move_forward() {
        let store = InMemoryStore::new();
        let (device, conversation) = (DeviceId::new(), ConversationId::new());
        let mut tx = store.begin().await.unwrap();

        tx.create_cursor_if_absent(device, conversation, 3)
            .await
            .unwrap();
        tx.create_cursor_if_absent(device, conversation, 9)
            .await
            .unwrap();
        assert_eq!(tx.get_cursor(device, conversation).await.unwrap(), Some(3));

        assert!(tx.raise_cursor(device, conversation, 5).await.unwrap());
        assert!(!tx.raise_cursor(device, conversation, 4).await.unwrap());
        assert_eq!(tx.get_cursor(device, conversation).await.unwrap(), Some(5));
    }

    #[tokio::test]
    async fn revoked_devices_have_no_owner_and_are_not_listed() {
        let store = InMemoryStore::new();
        let user = UserId::new();
        let (kept, revoked) = (DeviceId::new(), DeviceId::new());
        let mut tx = store.begin().await.unwrap();
        tx.insert_device(kept, user).await.unwrap();
        tx.insert_device(revoked, user).await.unwrap();
        tx.revoke_device(revoked).await.unwrap();

        assert_eq!(tx.device_owner(kept).await.unwrap(), Some(user));
        assert_eq!(tx.device_owner(revoked).await.unwrap(), None);
        assert_eq!(tx.devices_of_user(user).await.unwrap(), vec![kept]);
    }
}
```

Add to the module list in `crates/push-domain/src/lib.rs`:

```rust
pub mod memory;
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p push-domain --lib memory`
Expected: compile errors — `InMemoryStore` and the imported types not found.

- [ ] **Step 3: Implement the store**

Insert above the `#[cfg(test)]` block in `crates/push-domain/src/memory.rs`:

```rust
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, DeviceId, UserId};
use crate::model::{ContactState, Conversation, Entry, Member};
use crate::store::{InsertOutcome, Store, Tx};

#[derive(Debug, Clone, Default)]
struct State {
    users: HashSet<UserId>,
    devices: HashMap<DeviceId, DeviceRow>,
    contacts: HashMap<(UserId, UserId), ContactState>,
    conversations: HashMap<ConversationId, Conversation>,
    direct_index: HashMap<[u8; 32], ConversationId>,
    members: BTreeMap<(ConversationId, UserId), Member>,
    entries: BTreeMap<(ConversationId, i64), Entry>,
    client_ids: HashMap<(ConversationId, ClientMsgId), i64>,
    cursors: HashMap<(DeviceId, ConversationId), i64>,
}

#[derive(Debug, Clone, Copy)]
struct DeviceRow {
    user: UserId,
    revoked: bool,
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryStore {
    state: Arc<Mutex<State>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug)]
pub struct InMemoryTx {
    guard: OwnedMutexGuard<State>,
    work: State,
}

#[async_trait]
impl Store for InMemoryStore {
    type Tx = InMemoryTx;

    async fn begin(&self) -> DomainResult<InMemoryTx> {
        let guard = Arc::clone(&self.state).lock_owned().await;
        let work = guard.clone();
        Ok(InMemoryTx { guard, work })
    }
}

#[async_trait]
impl Tx for InMemoryTx {
    async fn commit(self) -> DomainResult<()> {
        let InMemoryTx { mut guard, work } = self;
        *guard = work;
        Ok(())
    }

    async fn rollback(self) -> DomainResult<()> {
        Ok(())
    }

    async fn insert_user(&mut self, user: UserId) -> DomainResult<()> {
        self.work.users.insert(user);
        Ok(())
    }

    async fn user_exists(&mut self, user: UserId) -> DomainResult<bool> {
        Ok(self.work.users.contains(&user))
    }

    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<()> {
        self.work.devices.entry(device).or_insert(DeviceRow {
            user,
            revoked: false,
        });
        Ok(())
    }

    async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()> {
        if let Some(row) = self.work.devices.get_mut(&device) {
            row.revoked = true;
        }
        Ok(())
    }

    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        Ok(self
            .work
            .devices
            .get(&device)
            .filter(|row| !row.revoked)
            .map(|row| row.user))
    }

    async fn devices_of_user(&mut self, user: UserId) -> DomainResult<Vec<DeviceId>> {
        let mut devices: Vec<DeviceId> = self
            .work
            .devices
            .iter()
            .filter(|(_, row)| row.user == user && !row.revoked)
            .map(|(id, _)| *id)
            .collect();
        devices.sort();
        Ok(devices)
    }

    async fn get_contact(
        &mut self,
        owner: UserId,
        peer: UserId,
    ) -> DomainResult<Option<ContactState>> {
        Ok(self.work.contacts.get(&(owner, peer)).copied())
    }

    async fn put_contact(
        &mut self,
        owner: UserId,
        peer: UserId,
        state: ContactState,
    ) -> DomainResult<()> {
        self.work.contacts.insert((owner, peer), state);
        Ok(())
    }

    async fn delete_contact(&mut self, owner: UserId, peer: UserId) -> DomainResult<()> {
        self.work.contacts.remove(&(owner, peer));
        Ok(())
    }

    async fn find_direct(&mut self, key: [u8; 32]) -> DomainResult<Option<ConversationId>> {
        Ok(self.work.direct_index.get(&key).copied())
    }

    async fn insert_conversation(
        &mut self,
        conversation: Conversation,
    ) -> DomainResult<InsertOutcome> {
        if let Some(key) = conversation.direct_key {
            if self.work.direct_index.contains_key(&key) {
                return Ok(InsertOutcome::Duplicate);
            }
            self.work.direct_index.insert(key, conversation.id);
        }
        self.work
            .conversations
            .insert(conversation.id, conversation);
        Ok(InsertOutcome::Inserted)
    }

    async fn get_conversation(
        &mut self,
        id: ConversationId,
    ) -> DomainResult<Option<Conversation>> {
        Ok(self.work.conversations.get(&id).cloned())
    }

    async fn next_seq(&mut self, id: ConversationId) -> DomainResult<i64> {
        let conversation = self
            .work
            .conversations
            .get_mut(&id)
            .ok_or(DomainError::ConversationNotFound)?;
        conversation.last_seq += 1;
        Ok(conversation.last_seq)
    }

    async fn insert_member(&mut self, member: Member) -> DomainResult<()> {
        self.work
            .members
            .insert((member.conversation_id, member.user_id), member);
        Ok(())
    }

    async fn get_member(
        &mut self,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<Option<Member>> {
        Ok(self.work.members.get(&(conversation, user)).cloned())
    }

    async fn members(&mut self, conversation: ConversationId) -> DomainResult<Vec<Member>> {
        Ok(self
            .work
            .members
            .values()
            .filter(|member| member.conversation_id == conversation)
            .cloned()
            .collect())
    }

    async fn memberships(&mut self, user: UserId) -> DomainResult<Vec<(Conversation, Member)>> {
        Ok(self
            .work
            .members
            .values()
            .filter(|member| member.user_id == user)
            .filter_map(|member| {
                self.work
                    .conversations
                    .get(&member.conversation_id)
                    .map(|conversation| (conversation.clone(), member.clone()))
            })
            .collect())
    }

    async fn raise_read_seq(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        seq: i64,
    ) -> DomainResult<bool> {
        let member = self
            .work
            .members
            .get_mut(&(conversation, user))
            .ok_or(DomainError::NotMember)?;
        if seq > member.read_seq {
            member.read_seq = seq;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn insert_entry(&mut self, entry: Entry) -> DomainResult<InsertOutcome> {
        let key = (entry.conversation_id, entry.client_msg_id);
        if self.work.client_ids.contains_key(&key) {
            return Ok(InsertOutcome::Duplicate);
        }
        self.work.client_ids.insert(key, entry.seq);
        self.work
            .entries
            .insert((entry.conversation_id, entry.seq), entry);
        Ok(InsertOutcome::Inserted)
    }

    async fn entry_by_client_id(
        &mut self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
    ) -> DomainResult<Option<Entry>> {
        Ok(self
            .work
            .client_ids
            .get(&(conversation, client_msg_id))
            .and_then(|seq| self.work.entries.get(&(conversation, *seq)))
            .cloned())
    }

    async fn entries_after(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        if after >= up_to {
            return Ok(Vec::new());
        }
        Ok(self
            .work
            .entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .take(limit as usize)
            .map(|(_, entry)| entry.clone())
            .collect())
    }

    async fn entries_before(
        &mut self,
        conversation: ConversationId,
        before: i64,
        from: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        if from >= before {
            return Ok(Vec::new());
        }
        let mut page: Vec<Entry> = self
            .work
            .entries
            .range((conversation, from)..(conversation, before))
            .rev()
            .take(limit as usize)
            .map(|(_, entry)| entry.clone())
            .collect();
        page.reverse();
        Ok(page)
    }

    async fn get_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
    ) -> DomainResult<Option<i64>> {
        Ok(self.work.cursors.get(&(device, conversation)).copied())
    }

    async fn create_cursor_if_absent(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<()> {
        self.work.cursors.entry((device, conversation)).or_insert(seq);
        Ok(())
    }

    async fn raise_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<bool> {
        match self.work.cursors.get_mut(&(device, conversation)) {
            Some(position) if seq > *position => {
                *position = seq;
                Ok(true)
            }
            Some(_) => Ok(false),
            None => {
                self.work.cursors.insert((device, conversation), seq);
                Ok(true)
            }
        }
    }
}
```

Add to the re-exports in `crates/push-domain/src/lib.rs`:

```rust
pub use memory::InMemoryStore;
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p push-domain --lib memory`
Expected: `8 passed`.

- [ ] **Step 5: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): add serialised in-memory store for tests"
```

---

### Task 5: Messaging service, users and devices

**Files:**
- Create: `crates/push-domain/src/service.rs`
- Create: `crates/push-domain/tests/common/mod.rs`
- Create: `crates/push-domain/tests/devices.rs`
- Modify: `crates/push-domain/src/lib.rs`

- [ ] **Step 1: Create the test harness**

`crates/push-domain/tests/common/mod.rs`:

```rust
//! Shared test harness: a `Messaging` service over the in-memory store, a manual clock and
//! a recording notifier.

#![allow(dead_code)]

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use push_domain::{DeviceId, InMemoryStore, ManualClock, Messaging, RecordingNotifier, UserId};

pub type Svc = Messaging<InMemoryStore, Arc<ManualClock>, Arc<RecordingNotifier>>;

/// A verified user and one of their devices.
#[derive(Debug, Clone, Copy)]
pub struct Person {
    pub user: UserId,
    pub device: DeviceId,
}

pub struct World {
    pub svc: Svc,
    pub clock: Arc<ManualClock>,
    pub notifier: Arc<RecordingNotifier>,
}

impl World {
    pub fn new() -> Self {
        let clock = Arc::new(ManualClock::new(
            Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
        ));
        let notifier = Arc::new(RecordingNotifier::default());
        let svc = Messaging::new(
            InMemoryStore::new(),
            Arc::clone(&clock),
            Arc::clone(&notifier),
        );
        Self {
            svc,
            clock,
            notifier,
        }
    }

    /// Registers a verified user with one device.
    pub async fn person(&self) -> Person {
        let person = Person {
            user: UserId::new(),
            device: DeviceId::new(),
        };
        self.svc.register_user(person.user).await.unwrap();
        self.svc
            .register_device(person.device, person.user)
            .await
            .unwrap();
        person
    }

    /// Registers another device for an existing person.
    pub async fn add_device(&self, person: &Person) -> DeviceId {
        let device = DeviceId::new();
        self.svc.register_device(device, person.user).await.unwrap();
        device
    }
}
```

- [ ] **Step 2: Write the failing tests**

`crates/push-domain/tests/devices.rs`:

```rust
mod common;

use common::World;
use push_domain::{DeviceId, DomainError, UserId};

#[tokio::test]
async fn registering_a_device_for_an_unknown_user_fails() {
    let world = World::new();
    let result = world
        .svc
        .register_device(DeviceId::new(), UserId::new())
        .await;
    assert_eq!(result, Err(DomainError::UnknownUser));
}

#[tokio::test]
async fn identify_accepts_the_users_own_active_device() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}

#[tokio::test]
async fn identify_rejects_a_device_of_another_user() {
    let world = World::new();
    let alice = world.person().await;
    let bob = world.person().await;
    assert_eq!(
        world.svc.identify(bob.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

#[tokio::test]
async fn identify_rejects_a_revoked_device() {
    let world = World::new();
    let alice = world.person().await;
    world.svc.revoke_device(alice.device).await.unwrap();
    assert_eq!(
        world.svc.identify(alice.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

#[tokio::test]
async fn registering_the_same_user_and_device_twice_is_harmless() {
    let world = World::new();
    let alice = world.person().await;
    world.svc.register_user(alice.user).await.unwrap();
    world
        .svc
        .register_device(alice.device, alice.user)
        .await
        .unwrap();
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p push-domain --test devices`
Expected: compile error — `Messaging` not found in `push_domain`.

- [ ] **Step 4: Implement the service**

`crates/push-domain/src/service.rs`:

```rust
use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::{DeviceId, UserId};
use crate::notify::Notifier;
use crate::store::{Store, Tx};

/// The messaging service. Every operation runs in one storage transaction.
///
/// A transaction dropped without `commit` is rolled back, so an early return through `?`
/// never leaves partial changes behind.
#[derive(Debug)]
pub struct Messaging<S, C, N> {
    pub(crate) store: S,
    pub(crate) clock: C,
    pub(crate) notifier: N,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    pub fn new(store: S, clock: C, notifier: N) -> Self {
        Self {
            store,
            clock,
            notifier,
        }
    }

    /// Records a user whose email is verified. Idempotent.
    pub async fn register_user(&self, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.insert_user(user).await?;
        tx.commit().await
    }

    /// Records a newly registered device. Idempotent.
    ///
    /// The device starts up to date in every current conversation of its user; older
    /// messages are loaded through history.
    pub async fn register_device(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(user).await? {
            return Err(DomainError::UnknownUser);
        }
        tx.insert_device(device, user).await?;
        for (conversation, member) in tx.memberships(user).await? {
            if member.is_current() {
                tx.create_cursor_if_absent(device, conversation.id, conversation.last_seq)
                    .await?;
            }
        }
        tx.commit().await
    }

    pub async fn revoke_device(&self, device: DeviceId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.revoke_device(device).await?;
        tx.commit().await
    }

    /// Confirms that a connecting device is active and belongs to the user in its token.
    pub async fn identify(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        let owner = require_device(&mut tx, device).await?;
        tx.rollback().await?;
        if owner == user {
            Ok(())
        } else {
            Err(DomainError::UnknownDevice)
        }
    }
}

/// The owner of an active device, or `UnknownDevice`.
pub(crate) async fn require_device<T: Tx>(tx: &mut T, device: DeviceId) -> DomainResult<UserId> {
    tx.device_owner(device)
        .await?
        .ok_or(DomainError::UnknownDevice)
}
```

Add to `crates/push-domain/src/lib.rs` — module list:

```rust
mod service;
```

and re-exports:

```rust
pub use service::Messaging;
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p push-domain --test devices`
Expected: `5 passed`, no warnings.

- [ ] **Step 6: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): add messaging service with user and device registration"
```

---

### Task 6: Contacts

**Files:**
- Create: `crates/push-domain/src/contacts.rs`
- Create: `crates/push-domain/tests/contacts.rs`
- Modify: `crates/push-domain/tests/common/mod.rs`
- Modify: `crates/push-domain/src/lib.rs`

Rules (spec §5.5): one row per direction. A request creates `RequestedOut` for the sender
and `RequestedIn` for the recipient; crossing requests become `Accepted`. A block is
invisible to the blocked user: their request looks ordinary but never arrives. Blocking
ends an accepted contact.

- [ ] **Step 1: Write the failing tests**

`crates/push-domain/tests/contacts.rs`:

```rust
mod common;

use common::World;
use push_domain::{ContactState, DomainError, UserId, ValidationError};

#[tokio::test]
async fn request_then_accept_makes_both_sides_accepted() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);

    assert_eq!(
        world.svc.contact_request(alice.user, bob.user).await,
        Ok(ContactState::RequestedOut)
    );
    assert_eq!(
        world.svc.contact_state(bob.user, alice.user).await,
        Ok(Some(ContactState::RequestedIn))
    );

    world.svc.contact_accept(bob.user, alice.user).await.unwrap();
    assert_eq!(
        world.svc.contact_state(alice.user, bob.user).await,
        Ok(Some(ContactState::Accepted))
    );
    assert_eq!(
        world.svc.contact_state(bob.user, alice.user).await,
        Ok(Some(ContactState::Accepted))
    );
}

#[tokio::test]
async fn crossing_requests_become_accepted() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_request(alice.user, bob.user).await.unwrap();
    assert_eq!(
        world.svc.contact_request(bob.user, alice.user).await,
        Ok(ContactState::Accepted)
    );
    assert_eq!(
        world.svc.contact_state(alice.user, bob.user).await,
        Ok(Some(ContactState::Accepted))
    );
}

#[tokio::test]
async fn only_the_recipient_can_accept_a_request() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    assert_eq!(
        world.svc.contact_accept(alice.user, bob.user).await,
        Err(DomainError::NoPendingRequest)
    );
    world.svc.contact_request(alice.user, bob.user).await.unwrap();
    assert_eq!(
        world.svc.contact_accept(alice.user, bob.user).await,
        Err(DomainError::NoPendingRequest)
    );
}

#[tokio::test]
async fn declining_removes_the_request_on_both_sides() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_request(alice.user, bob.user).await.unwrap();
    world.svc.contact_decline(bob.user, alice.user).await.unwrap();
    assert_eq!(world.svc.contact_state(alice.user, bob.user).await, Ok(None));
    assert_eq!(world.svc.contact_state(bob.user, alice.user).await, Ok(None));
}

#[tokio::test]
async fn a_block_is_invisible_to_the_blocked_user() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_block(alice.user, bob.user).await.unwrap();

    assert_eq!(
        world.svc.contact_request(bob.user, alice.user).await,
        Ok(ContactState::RequestedOut)
    );
    assert_eq!(
        world.svc.contact_state(alice.user, bob.user).await,
        Ok(Some(ContactState::Blocked))
    );
}

#[tokio::test]
async fn the_blocker_cannot_send_requests() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert_eq!(
        world.svc.contact_request(alice.user, bob.user).await,
        Err(DomainError::Blocked)
    );
}

#[tokio::test]
async fn blocking_ends_an_accepted_contact() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert_eq!(world.svc.contact_state(bob.user, alice.user).await, Ok(None));
}

#[tokio::test]
async fn unblocking_allows_a_fresh_request() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    world.svc.contact_unblock(alice.user, bob.user).await.unwrap();
    assert_eq!(world.svc.contact_state(alice.user, bob.user).await, Ok(None));
    assert_eq!(
        world.svc.contact_request(alice.user, bob.user).await,
        Ok(ContactState::RequestedOut)
    );
}

#[tokio::test]
async fn a_user_cannot_request_themselves() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(
        world.svc.contact_request(alice.user, alice.user).await,
        Err(DomainError::Invalid(ValidationError::SelfTarget))
    );
}

#[tokio::test]
async fn requesting_an_unknown_user_fails() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(
        world.svc.contact_request(alice.user, UserId::new()).await,
        Err(DomainError::UnknownUser)
    );
}
```

Add this method inside `impl World` in `crates/push-domain/tests/common/mod.rs`:

```rust
    /// Makes two people accepted contacts.
    pub async fn befriend(&self, a: &Person, b: &Person) {
        self.svc.contact_request(a.user, b.user).await.unwrap();
        self.svc.contact_accept(b.user, a.user).await.unwrap();
    }
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p push-domain --test contacts`
Expected: compile errors — no method `contact_request` (and the other contact methods).

- [ ] **Step 3: Implement contacts**

`crates/push-domain/src/contacts.rs`:

```rust
use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::ContactState;
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::validation::ValidationError;

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Sends a contact request. Returns the requester's resulting state.
    pub async fn contact_request(&self, from: UserId, to: UserId) -> DomainResult<ContactState> {
        let mut tx = self.begin_pair(from, to).await?;
        let mine = tx.get_contact(from, to).await?;
        let theirs = tx.get_contact(to, from).await?;
        let result = match (mine, theirs) {
            (Some(ContactState::Blocked), _) => return Err(DomainError::Blocked),
            (Some(ContactState::Accepted), _) => ContactState::Accepted,
            (_, Some(ContactState::Blocked)) => {
                // Hide the block: the requester sees an ordinary pending request.
                tx.put_contact(from, to, ContactState::RequestedOut).await?;
                ContactState::RequestedOut
            }
            (_, Some(ContactState::RequestedOut)) => {
                tx.put_contact(from, to, ContactState::Accepted).await?;
                tx.put_contact(to, from, ContactState::Accepted).await?;
                ContactState::Accepted
            }
            _ => {
                tx.put_contact(from, to, ContactState::RequestedOut).await?;
                tx.put_contact(to, from, ContactState::RequestedIn).await?;
                ContactState::RequestedOut
            }
        };
        tx.commit().await?;
        Ok(result)
    }

    /// Accepts a request that `peer` sent to `me`.
    pub async fn contact_accept(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
        }
        tx.put_contact(me, peer, ContactState::Accepted).await?;
        tx.put_contact(peer, me, ContactState::Accepted).await?;
        tx.commit().await
    }

    /// Declines a request that `peer` sent to `me`.
    pub async fn contact_decline(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
        }
        tx.delete_contact(me, peer).await?;
        if tx.get_contact(peer, me).await? == Some(ContactState::RequestedOut) {
            tx.delete_contact(peer, me).await?;
        }
        tx.commit().await
    }

    /// Blocks `peer`. Ends any contact; `peer`'s side is removed unless they also block.
    pub async fn contact_block(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        tx.put_contact(me, peer, ContactState::Blocked).await?;
        if tx.get_contact(peer, me).await? != Some(ContactState::Blocked) {
            tx.delete_contact(peer, me).await?;
        }
        tx.commit().await
    }

    /// Removes a block. The users are then strangers again.
    pub async fn contact_unblock(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? == Some(ContactState::Blocked) {
            tx.delete_contact(me, peer).await?;
        }
        tx.commit().await
    }

    /// `me`'s view of the relationship with `peer`.
    pub async fn contact_state(
        &self,
        me: UserId,
        peer: UserId,
    ) -> DomainResult<Option<ContactState>> {
        let mut tx = self.store.begin().await?;
        let state = tx.get_contact(me, peer).await?;
        tx.rollback().await?;
        Ok(state)
    }

    /// Begins a transaction for an operation between two distinct, known users.
    async fn begin_pair(&self, a: UserId, b: UserId) -> DomainResult<S::Tx> {
        if a == b {
            return Err(ValidationError::SelfTarget.into());
        }
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(a).await? || !tx.user_exists(b).await? {
            return Err(DomainError::UnknownUser);
        }
        Ok(tx)
    }
}

/// Whether both directions are `Accepted`.
pub(crate) async fn are_contacts<T: Tx>(tx: &mut T, a: UserId, b: UserId) -> DomainResult<bool> {
    Ok(tx.get_contact(a, b).await? == Some(ContactState::Accepted)
        && tx.get_contact(b, a).await? == Some(ContactState::Accepted))
}
```

Add to the module list in `crates/push-domain/src/lib.rs`:

```rust
mod contacts;
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p push-domain --test contacts`
Expected: `10 passed`. A `dead_code` warning for `are_contacts` is expected until Task 7 uses it.

- [ ] **Step 5: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): add contact requests, accept, decline and block"
```

---

### Task 7: Direct conversations

**Files:**
- Create: `crates/push-domain/src/conversations.rs`
- Create: `crates/push-domain/tests/direct.rs`
- Modify: `crates/push-domain/tests/common/mod.rs`
- Modify: `crates/push-domain/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

`crates/push-domain/tests/direct.rs`:

```rust
mod common;

use common::World;
use push_domain::{DomainError, ValidationError};

#[tokio::test]
async fn a_direct_chat_requires_accepted_contacts() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_request(alice.user, bob.user).await.unwrap();
    assert_eq!(
        world.svc.open_direct(alice.user, bob.user).await,
        Err(DomainError::NotContact)
    );
}

#[tokio::test]
async fn both_users_get_the_same_direct_conversation() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;

    let from_alice = world.svc.open_direct(alice.user, bob.user).await.unwrap();
    let from_bob = world.svc.open_direct(bob.user, alice.user).await.unwrap();
    assert_eq!(from_alice, from_bob);
}

#[tokio::test]
async fn a_direct_chat_with_yourself_is_invalid() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(
        world.svc.open_direct(alice.user, alice.user).await,
        Err(DomainError::Invalid(ValidationError::SelfTarget))
    );
}
```

Add this method inside `impl World` in `crates/push-domain/tests/common/mod.rs`:

```rust
    /// Makes two people contacts and opens their direct conversation.
    pub async fn direct_chat(&self, a: &Person, b: &Person) -> push_domain::ConversationId {
        self.befriend(a, b).await;
        self.svc.open_direct(a.user, b.user).await.unwrap()
    }
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p push-domain --test direct`
Expected: compile error — no method `open_direct`.

- [ ] **Step 3: Implement direct conversations**

`crates/push-domain/src/conversations.rs`:

```rust
use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{Conversation, ConversationKind, Member, Role, direct_key};
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::ValidationError;

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Returns the direct conversation between two accepted contacts, creating it if needed.
    pub async fn open_direct(&self, me: UserId, peer: UserId) -> DomainResult<ConversationId> {
        if me == peer {
            return Err(ValidationError::SelfTarget.into());
        }
        let key = direct_key(me, peer);
        let mut tx = self.store.begin().await?;
        if !are_contacts(&mut tx, me, peer).await? {
            return Err(DomainError::NotContact);
        }
        if let Some(existing) = tx.find_direct(key).await? {
            tx.rollback().await?;
            return Ok(existing);
        }

        let conversation = Conversation {
            id: ConversationId::new(),
            kind: ConversationKind::Direct,
            direct_key: Some(key),
            title: None,
            created_by: me,
            last_seq: 0,
            created_at: self.clock.now(),
        };
        let id = conversation.id;
        if tx.insert_conversation(conversation).await? == InsertOutcome::Duplicate {
            // A concurrent request created it first; return that one.
            tx.rollback().await?;
            let mut tx = self.store.begin().await?;
            let existing = tx.find_direct(key).await?.ok_or_else(|| {
                DomainError::Store("direct conversation disappeared after a conflict".into())
            })?;
            tx.rollback().await?;
            return Ok(existing);
        }

        for user in [me, peer] {
            tx.insert_member(Member {
                conversation_id: id,
                user_id: user,
                role: Role::Member,
                visible_from_seq: 1,
                left_at_seq: None,
                read_seq: 0,
            })
            .await?;
            for device in tx.devices_of_user(user).await? {
                tx.create_cursor_if_absent(device, id, 0).await?;
            }
        }
        tx.commit().await?;
        Ok(id)
    }
}

pub(crate) async fn require_conversation<T: Tx>(
    tx: &mut T,
    id: ConversationId,
) -> DomainResult<Conversation> {
    tx.get_conversation(id)
        .await?
        .ok_or(DomainError::ConversationNotFound)
}

pub(crate) async fn require_member<T: Tx>(
    tx: &mut T,
    conversation: ConversationId,
    user: UserId,
) -> DomainResult<Member> {
    tx.get_member(conversation, user)
        .await?
        .ok_or(DomainError::NotMember)
}
```

Add to the module list in `crates/push-domain/src/lib.rs`:

```rust
mod conversations;
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p push-domain --test direct`
Expected: `3 passed`. `dead_code` warnings for `require_conversation` and `require_member`
are expected until Tasks 9–11 use them.

- [ ] **Step 5: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): open direct conversations between accepted contacts"
```

---

### Task 8: Sending text messages

**Files:**
- Create: `crates/push-domain/src/send.rs`
- Create: `crates/push-domain/tests/send.rs`
- Modify: `crates/push-domain/tests/common/mod.rs`
- Modify: `crates/push-domain/src/lib.rs`

The transaction order matters (spec §4.2):
1. lock the conversation and take the next `seq`;
2. check membership and contacts;
3. insert with the `client_msg_id` uniqueness rule;
4. on a duplicate, **roll back** (so `seq` stays gapless) and return the original;
5. move the sender's read position, commit, then notify.

- [ ] **Step 1: Extend the test harness**

Replace the `use push_domain::…` line at the top of `crates/push-domain/tests/common/mod.rs` with:

```rust
use push_domain::{
    ClientMsgId, ConversationId, DeviceId, Entry, InMemoryStore, ManualClock, Messaging,
    RecordingNotifier, SendReceipt, SendText, UserId,
};
```

Change the return type of `direct_chat` from `push_domain::ConversationId` to
`ConversationId`, and add this method inside `impl World`:

```rust
    /// Sends a text message and expects it to succeed.
    pub async fn send(&self, from: &Person, conversation: ConversationId, body: &str) -> SendReceipt {
        self.svc
            .send_text(text(from, conversation, body))
            .await
            .unwrap()
    }
```

Add these free functions at the end of the file:

```rust
/// A send command with a fresh client message id.
pub fn text(from: &Person, conversation: ConversationId, body: &str) -> SendText {
    SendText {
        sender: from.user,
        device: from.device,
        conversation,
        client_msg_id: ClientMsgId::new(),
        body: body.as_bytes().to_vec(),
        client_ts: None,
    }
}

pub fn bodies(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| String::from_utf8(entry.body.clone().unwrap_or_default()).unwrap())
        .collect()
}

pub fn seqs(entries: &[Entry]) -> Vec<i64> {
    entries.iter().map(|entry| entry.seq).collect()
}
```

- [ ] **Step 2: Write the failing tests**

`crates/push-domain/tests/send.rs`:

```rust
mod common;

use chrono::{TimeZone, Utc};
use common::{World, text};
use push_domain::{ConversationId, DomainError, SendText, ValidationError};

#[tokio::test]
async fn messages_get_consecutive_seq_numbers_starting_at_one() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    let first = world.send(&alice, chat, "one").await;
    let second = world.send(&bob, chat, "two").await;
    let third = world.send(&alice, chat, "three").await;
    assert_eq!([first.seq, second.seq, third.seq], [1, 2, 3]);
}

#[tokio::test]
async fn a_retry_returns_the_original_and_leaves_no_gap() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    let command = text(&alice, chat, "hello");
    let first = world.svc.send_text(command.clone()).await.unwrap();
    let retry = world.svc.send_text(command).await.unwrap();
    assert_eq!(retry, first);

    assert_eq!(world.send(&bob, chat, "hi").await.seq, 2);
}

#[tokio::test]
async fn another_sender_cannot_reuse_a_client_msg_id() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    let command = text(&alice, chat, "hello");
    world.svc.send_text(command.clone()).await.unwrap();
    let forged = SendText {
        sender: bob.user,
        device: bob.device,
        ..command
    };
    assert_eq!(
        world.svc.send_text(forged).await,
        Err(DomainError::ClientIdConflict)
    );
}

#[tokio::test]
async fn an_invalid_message_is_rejected_and_uses_no_seq() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    assert_eq!(
        world.svc.send_text(text(&alice, chat, "")).await,
        Err(DomainError::Invalid(ValidationError::EmptyBody))
    );
    assert_eq!(world.send(&alice, chat, "ok").await.seq, 1);
}

#[tokio::test]
async fn only_members_can_send() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let chat = world.direct_chat(&alice, &bob).await;
    world.befriend(&alice, &carol).await;

    assert_eq!(
        world.svc.send_text(text(&carol, chat, "hi")).await,
        Err(DomainError::NotMember)
    );
    assert_eq!(world.send(&alice, chat, "still gapless").await.seq, 1);
}

#[tokio::test]
async fn a_blocked_contact_cannot_send() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.svc.contact_block(bob.user, alice.user).await.unwrap();

    assert_eq!(
        world.svc.send_text(text(&alice, chat, "hi")).await,
        Err(DomainError::NotContact)
    );
}

#[tokio::test]
async fn the_device_must_belong_to_the_sender() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    let command = SendText {
        device: bob.device,
        ..text(&alice, chat, "hi")
    };
    assert_eq!(
        world.svc.send_text(command).await,
        Err(DomainError::UnknownDevice)
    );
}

#[tokio::test]
async fn sending_to_an_unknown_conversation_fails() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(
        world
            .svc
            .send_text(text(&alice, ConversationId::new(), "hi"))
            .await,
        Err(DomainError::ConversationNotFound)
    );
}

#[tokio::test]
async fn members_are_notified_only_after_a_successful_send() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.notifier.take();

    world.send(&alice, chat, "hi").await;
    let calls = world.notifier.take();
    assert_eq!(calls.len(), 1);
    let mut notified = calls[0].clone();
    notified.sort();
    let mut expected = vec![alice.user, bob.user];
    expected.sort();
    assert_eq!(notified, expected);

    let _ = world.svc.send_text(text(&alice, chat, "")).await;
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn the_server_timestamp_comes_from_the_clock() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let at = Utc.with_ymd_and_hms(2026, 3, 4, 5, 6, 7).unwrap();
    world.clock.set(at);

    assert_eq!(world.send(&alice, chat, "hi").await.server_ts, at);
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p push-domain --test send`
Expected: compile errors — `SendText` not found, no method `send_text`.

- [ ] **Step 4: Implement sending**

`crates/push-domain/src/send.rs`:

```rust
use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::conversations::require_conversation;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{ContentType, ConversationKind, Entry, EntryKind, SendReceipt};
use crate::notify::Notifier;
use crate::service::{Messaging, require_device};
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::validate_text;

/// A text message to append. `sender` and `device` come from the verified token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendText {
    pub sender: UserId,
    pub device: DeviceId,
    pub conversation: ConversationId,
    /// Chosen by the client before the first attempt and reused on every retry.
    pub client_msg_id: ClientMsgId,
    pub body: Vec<u8>,
    pub client_ts: Option<DateTime<Utc>>,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Appends a text message. A retry with the same `client_msg_id` returns the original
    /// receipt and stores nothing new.
    pub async fn send_text(&self, command: SendText) -> DomainResult<SendReceipt> {
        validate_text(&command.body)?;

        let mut tx = self.store.begin().await?;
        if require_device(&mut tx, command.device).await? != command.sender {
            return Err(DomainError::UnknownDevice);
        }
        let conversation = require_conversation(&mut tx, command.conversation).await?;
        let seq = tx.next_seq(conversation.id).await?;

        let members = tx.members(conversation.id).await?;
        if !members
            .iter()
            .any(|member| member.user_id == command.sender && member.is_current())
        {
            return Err(DomainError::NotMember);
        }
        if conversation.kind == ConversationKind::Direct {
            for peer in members.iter().filter(|member| member.user_id != command.sender) {
                if !are_contacts(&mut tx, command.sender, peer.user_id).await? {
                    return Err(DomainError::NotContact);
                }
            }
        }

        let entry = Entry {
            conversation_id: conversation.id,
            seq,
            message_id: MessageId::new(),
            client_msg_id: command.client_msg_id,
            kind: EntryKind::Message,
            target_seq: None,
            sender_id: command.sender,
            sender_device_id: Some(command.device),
            content_type: ContentType::Text,
            body: Some(command.body),
            client_ts: command.client_ts,
            server_ts: self.clock.now(),
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        };
        let receipt = SendReceipt::from(&entry);

        if tx.insert_entry(entry).await? == InsertOutcome::Duplicate {
            // Rolling back returns `seq` to the counter, keeping the log gapless.
            tx.rollback().await?;
            return self
                .replay_send(command.conversation, command.client_msg_id, command.sender)
                .await;
        }
        tx.raise_read_seq(conversation.id, command.sender, seq)
            .await?;
        tx.commit().await?;

        let recipients: Vec<UserId> = members
            .iter()
            .filter(|member| member.is_current())
            .map(|member| member.user_id)
            .collect();
        self.notifier.notify_users(&recipients).await;
        Ok(receipt)
    }

    /// The receipt of an already stored entry, if it belongs to the same sender.
    async fn replay_send(
        &self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
        sender: UserId,
    ) -> DomainResult<SendReceipt> {
        let mut tx = self.store.begin().await?;
        let original = tx
            .entry_by_client_id(conversation, client_msg_id)
            .await?
            .ok_or_else(|| {
                DomainError::Store("duplicate entry disappeared after a conflict".into())
            })?;
        tx.rollback().await?;
        if original.sender_id != sender {
            return Err(DomainError::ClientIdConflict);
        }
        Ok(SendReceipt::from(&original))
    }
}
```

Add to `crates/push-domain/src/lib.rs` — module list:

```rust
mod send;
```

and re-exports:

```rust
pub use send::SendText;
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p push-domain --test send`
Expected: `10 passed`.

- [ ] **Step 6: Run the whole crate**

Run: `cargo test -p push-domain`
Expected: every test passes. The only warning allowed is `dead_code` for `require_member`.

- [ ] **Step 7: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): send text with gapless seq and idempotent retry"
```

---

### Task 9: Sync and cumulative acks

**Files:**
- Create: `crates/push-domain/src/sync.rs`
- Create: `crates/push-domain/tests/sync.rs`
- Modify: `crates/push-domain/src/lib.rs`

Each device has a position per conversation: "I have everything up to `seq`". `sync`
returns what lies beyond it, oldest first per conversation. `ack` moves the position
forward and never backward (spec §4.3).

- [ ] **Step 1: Write the failing tests**

`crates/push-domain/tests/sync.rs`:

```rust
mod common;

use common::{World, bodies, seqs};
use push_domain::DomainError;

#[tokio::test]
async fn the_recipient_receives_new_messages_in_order() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;

    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(bodies(&batch.entries), ["one", "two"]);
    assert_eq!(seqs(&batch.entries), [1, 2]);
    assert!(!batch.more);
}

#[tokio::test]
async fn the_senders_own_devices_also_receive_the_message() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    let batch = world.svc.sync(alice.device, 100).await.unwrap();
    assert_eq!(bodies(&batch.entries), ["one"]);
}

#[tokio::test]
async fn an_ack_is_cumulative_and_stops_redelivery() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }

    assert_eq!(world.svc.ack(bob.device, chat, 2).await, Ok(true));
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(bodies(&batch.entries), ["three"]);
}

#[tokio::test]
async fn an_ack_never_moves_backwards() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;

    assert_eq!(world.svc.ack(bob.device, chat, 2).await, Ok(true));
    assert_eq!(world.svc.ack(bob.device, chat, 1).await, Ok(false));
    assert!(
        world
            .svc
            .sync(bob.device, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

#[tokio::test]
async fn an_ack_beyond_the_last_message_is_rejected() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(
        world.svc.ack(bob.device, chat, 5).await,
        Err(DomainError::SeqOutOfRange)
    );
}

#[tokio::test]
async fn a_new_device_starts_up_to_date() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;

    let tablet = world.add_device(&bob).await;
    assert!(world.svc.sync(tablet, 100).await.unwrap().entries.is_empty());

    world.send(&alice, chat, "three").await;
    let batch = world.svc.sync(tablet, 100).await.unwrap();
    assert_eq!(bodies(&batch.entries), ["three"]);
}

#[tokio::test]
async fn sync_respects_the_batch_limit() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["1", "2", "3", "4", "5"] {
        world.send(&alice, chat, body).await;
    }

    let first = world.svc.sync(bob.device, 2).await.unwrap();
    assert_eq!(seqs(&first.entries), [1, 2]);
    assert!(first.more);

    world.svc.ack(bob.device, chat, 2).await.unwrap();
    let rest = world.svc.sync(bob.device, 10).await.unwrap();
    assert_eq!(seqs(&rest.entries), [3, 4, 5]);
    assert!(!rest.more);
}

#[tokio::test]
async fn sync_covers_every_conversation_of_the_user() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let with_alice = world.direct_chat(&alice, &bob).await;
    let with_carol = world.direct_chat(&carol, &bob).await;
    world.send(&alice, with_alice, "from alice").await;
    world.send(&carol, with_carol, "from carol").await;

    let mut received = bodies(&world.svc.sync(bob.device, 100).await.unwrap().entries);
    received.sort();
    assert_eq!(received, ["from alice", "from carol"]);
}

#[tokio::test]
async fn a_revoked_device_can_neither_sync_nor_ack() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.svc.revoke_device(bob.device).await.unwrap();

    assert_eq!(
        world.svc.sync(bob.device, 100).await,
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(
        world.svc.ack(bob.device, chat, 1).await,
        Err(DomainError::UnknownDevice)
    );
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p push-domain --test sync`
Expected: compile errors — no methods `sync` and `ack`.

- [ ] **Step 3: Implement sync and ack**

`crates/push-domain/src/sync.rs`:

```rust
use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, DeviceId};
use crate::model::Entry;
use crate::notify::Notifier;
use crate::service::{Messaging, require_device};
use crate::store::{Store, Tx};
use crate::validation::MAX_SYNC_BATCH;

/// Entries a device has not acknowledged yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncBatch {
    /// Oldest first within each conversation.
    pub entries: Vec<Entry>,
    /// More entries are waiting beyond this batch.
    pub more: bool,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Everything after the device's delivery positions, within each membership's
    /// visible range, up to `limit` entries.
    pub async fn sync(&self, device: DeviceId, limit: u32) -> DomainResult<SyncBatch> {
        let limit = limit.clamp(1, MAX_SYNC_BATCH) as usize;
        let mut tx = self.store.begin().await?;
        let user = require_device(&mut tx, device).await?;
        let mut memberships = tx.memberships(user).await?;
        memberships.sort_by_key(|(conversation, _)| conversation.id);

        let mut entries = Vec::new();
        let mut more = false;
        for (conversation, member) in memberships {
            let floor = member.visible_from_seq - 1;
            let position = tx
                .get_cursor(device, conversation.id)
                .await?
                .unwrap_or(floor)
                .max(floor);
            let upper = member.visible_upper(conversation.last_seq);
            if position >= upper {
                continue;
            }
            let room = limit - entries.len();
            if room == 0 {
                more = true;
                break;
            }
            let page = tx
                .entries_after(conversation.id, position, upper, room as u32)
                .await?;
            if page.last().map_or(position, |entry| entry.seq) < upper {
                more = true;
            }
            entries.extend(page);
        }
        tx.rollback().await?;
        Ok(SyncBatch { entries, more })
    }

    /// Records that the device holds everything up to `seq`. Returns whether the
    /// position moved; it never moves backwards.
    pub async fn ack(
        &self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<bool> {
        let mut tx = self.store.begin().await?;
        let user = require_device(&mut tx, device).await?;
        let found = require_conversation(&mut tx, conversation).await?;
        let member = require_member(&mut tx, conversation, user).await?;
        if seq < 0 || seq > member.visible_upper(found.last_seq) {
            return Err(DomainError::SeqOutOfRange);
        }
        let moved = tx.raise_cursor(device, conversation, seq).await?;
        tx.commit().await?;
        Ok(moved)
    }
}
```

Add to `crates/push-domain/src/lib.rs` — module list:

```rust
mod sync;
```

and re-exports:

```rust
pub use sync::SyncBatch;
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p push-domain --test sync`
Expected: `9 passed`, and no warnings left in the crate.

- [ ] **Step 5: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): add per-device sync and cumulative acks"
```

---

### Task 10: Read positions and conversation state

**Files:**
- Create: `crates/push-domain/src/read.rs`
- Create: `crates/push-domain/tests/read_state.rs`
- Modify: `crates/push-domain/src/lib.rs`

Receipts are derived from positions, never queued (spec §4.4): a member's
`delivered_seq` is the furthest position across their active devices, and `read_seq` is
their read position. Sending marks the conversation read for the sender, but must **not**
move any delivery position.

- [ ] **Step 1: Write the failing tests**

`crates/push-domain/tests/read_state.rs`:

```rust
mod common;

use common::World;
use push_domain::{ConversationState, DomainError, MemberState, UserId};

fn member(state: &ConversationState, user: UserId) -> &MemberState {
    state
        .members
        .iter()
        .find(|member| member.user_id == user)
        .expect("member present")
}

#[tokio::test]
async fn the_read_position_only_moves_forward() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }

    assert_eq!(world.svc.read_up_to(bob.user, chat, 2).await, Ok(true));
    assert_eq!(world.svc.read_up_to(bob.user, chat, 1).await, Ok(false));
    let state = world.svc.conversation_state(alice.user, chat).await.unwrap();
    assert_eq!(member(&state, bob.user).read_seq, 2);
}

#[tokio::test]
async fn reading_beyond_the_last_message_is_rejected() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(
        world.svc.read_up_to(bob.user, chat, 9).await,
        Err(DomainError::SeqOutOfRange)
    );
}

#[tokio::test]
async fn sending_marks_the_conversation_read_for_the_sender() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&bob, chat, "one").await;
    world.send(&alice, chat, "two").await;

    let state = world.svc.conversation_state(alice.user, chat).await.unwrap();
    assert_eq!(state.last_seq, 2);
    assert_eq!(member(&state, alice.user).read_seq, 2);
    assert_eq!(member(&state, bob.user).read_seq, 1);
}

#[tokio::test]
async fn sending_does_not_mark_anything_delivered() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&bob, chat, "one").await;
    world.send(&alice, chat, "two").await;

    let state = world.svc.conversation_state(bob.user, chat).await.unwrap();
    assert_eq!(member(&state, alice.user).delivered_seq, 0);
    assert_eq!(member(&state, bob.user).delivered_seq, 0);
}

#[tokio::test]
async fn delivered_is_the_furthest_position_across_a_users_devices() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let tablet = world.add_device(&bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }

    world.svc.ack(bob.device, chat, 1).await.unwrap();
    world.svc.ack(tablet, chat, 3).await.unwrap();
    let state = world.svc.conversation_state(alice.user, chat).await.unwrap();
    assert_eq!(member(&state, bob.user).delivered_seq, 3);
}

#[tokio::test]
async fn only_members_can_read_or_see_state() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(
        world.svc.conversation_state(carol.user, chat).await,
        Err(DomainError::NotMember)
    );
    assert_eq!(
        world.svc.read_up_to(carol.user, chat, 1).await,
        Err(DomainError::NotMember)
    );
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p push-domain --test read_state`
Expected: compile errors — no methods `read_up_to` and `conversation_state`.

- [ ] **Step 3: Implement read positions and state**

`crates/push-domain/src/read.rs`:

```rust
use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{ConversationState, MemberState};
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{Store, Tx};

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Records that `user` has read everything up to `seq`. Returns whether the position
    /// moved; it never moves backwards.
    pub async fn read_up_to(
        &self,
        user: UserId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<bool> {
        let mut tx = self.store.begin().await?;
        let found = require_conversation(&mut tx, conversation).await?;
        let member = require_member(&mut tx, conversation, user).await?;
        if seq < 0 || seq > member.visible_upper(found.last_seq) {
            return Err(DomainError::SeqOutOfRange);
        }
        let moved = tx.raise_read_seq(conversation, user, seq).await?;
        tx.commit().await?;
        Ok(moved)
    }

    /// Delivery and read positions of every member, as seen by `user`.
    pub async fn conversation_state(
        &self,
        user: UserId,
        conversation: ConversationId,
    ) -> DomainResult<ConversationState> {
        let mut tx = self.store.begin().await?;
        let found = require_conversation(&mut tx, conversation).await?;
        require_member(&mut tx, conversation, user).await?;

        let mut members = Vec::new();
        for member in tx.members(conversation).await? {
            let mut delivered_seq = 0;
            for device in tx.devices_of_user(member.user_id).await? {
                if let Some(position) = tx.get_cursor(device, conversation).await? {
                    delivered_seq = delivered_seq.max(position);
                }
            }
            members.push(MemberState {
                user_id: member.user_id,
                delivered_seq,
                read_seq: member.read_seq,
            });
        }
        tx.rollback().await?;
        Ok(ConversationState {
            conversation_id: conversation,
            last_seq: found.last_seq,
            members,
        })
    }
}
```

Add to the module list in `crates/push-domain/src/lib.rs`:

```rust
mod read;
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p push-domain --test read_state`
Expected: `6 passed`.

- [ ] **Step 5: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): add read positions and derived conversation state"
```

---

### Task 11: History

**Files:**
- Create: `crates/push-domain/src/history.rs`
- Create: `crates/push-domain/tests/history.rs`
- Modify: `crates/push-domain/src/lib.rs`

History pages go backwards from `before_seq` (or from the newest entry), are returned
oldest first, and stay inside the member's visible range. This is how a new device loads
older messages (spec §4.3).

- [ ] **Step 1: Write the failing tests**

`crates/push-domain/tests/history.rs`:

```rust
mod common;

use common::{World, seqs};
use push_domain::DomainError;

async fn chat_with_five_messages() -> (World, common::Person, push_domain::ConversationId) {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["1", "2", "3", "4", "5"] {
        world.send(&alice, chat, body).await;
    }
    (world, bob, chat)
}

#[tokio::test]
async fn the_first_page_is_the_newest_messages_oldest_first() {
    let (world, bob, chat) = chat_with_five_messages().await;
    let page = world.svc.history(bob.user, chat, None, 2).await.unwrap();
    assert_eq!(seqs(&page), [4, 5]);
}

#[tokio::test]
async fn pages_go_backwards_from_before_seq() {
    let (world, bob, chat) = chat_with_five_messages().await;
    let history = |before| world.svc.history(bob.user, chat, Some(before), 2);

    assert_eq!(seqs(&history(4).await.unwrap()), [2, 3]);
    assert_eq!(seqs(&history(2).await.unwrap()), [1]);
    assert!(history(1).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_page_size_is_at_least_one() {
    let (world, bob, chat) = chat_with_five_messages().await;
    let page = world.svc.history(bob.user, chat, None, 0).await.unwrap();
    assert_eq!(seqs(&page), [5]);
}

#[tokio::test]
async fn only_members_can_read_history() {
    let (world, _bob, chat) = chat_with_five_messages().await;
    let carol = world.person().await;
    assert_eq!(
        world.svc.history(carol.user, chat, None, 10).await,
        Err(DomainError::NotMember)
    );
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p push-domain --test history`
Expected: compile error — no method `history`.

- [ ] **Step 3: Implement history**

`crates/push-domain/src/history.rs`:

```rust
use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::DomainResult;
use crate::ids::{ConversationId, UserId};
use crate::model::Entry;
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::validation::MAX_HISTORY_PAGE;

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Up to `limit` entries before `before_seq` (or the newest ones), oldest first,
    /// limited to what `user` may see.
    pub async fn history(
        &self,
        user: UserId,
        conversation: ConversationId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        let limit = limit.clamp(1, MAX_HISTORY_PAGE);
        let mut tx = self.store.begin().await?;
        let found = require_conversation(&mut tx, conversation).await?;
        let member = require_member(&mut tx, conversation, user).await?;

        let end = member.visible_upper(found.last_seq) + 1;
        let before = before_seq.map_or(end, |seq| seq.min(end));
        let page = tx
            .entries_before(conversation, before, member.visible_from_seq, limit)
            .await?;
        tx.rollback().await?;
        Ok(page)
    }
}
```

Add to the module list in `crates/push-domain/src/lib.rs`:

```rust
mod history;
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p push-domain --test history`
Expected: `4 passed`.

- [ ] **Step 5: Check the final library root**

`crates/push-domain/src/lib.rs` should now read:

```rust
//! Messaging rules for push-messenger.
//!
//! This crate holds the domain logic — contacts, conversations, the message log, delivery
//! and read positions — independent of storage and transport. Storage is reached through
//! the `Store` and `Tx` traits.

pub mod clock;
pub mod error;
pub mod ids;
pub mod memory;
pub mod model;
pub mod notify;
pub mod store;
pub mod validation;

mod contacts;
mod conversations;
mod history;
mod read;
mod send;
mod service;
mod sync;

pub use clock::{Clock, ManualClock, SystemClock};
pub use error::{DomainError, DomainResult};
pub use ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
pub use memory::InMemoryStore;
pub use model::{
    ContactState, ContentType, Conversation, ConversationKind, ConversationState, Entry,
    EntryKind, Member, MemberState, Role, SendReceipt, direct_key,
};
pub use notify::{NoopNotifier, Notifier, RecordingNotifier};
pub use send::SendText;
pub use service::Messaging;
pub use store::{InsertOutcome, Store, Tx};
pub use sync::SyncBatch;
pub use validation::{
    MAX_HISTORY_PAGE, MAX_SYNC_BATCH, MAX_TEXT_BYTES, ValidationError, validate_text,
};
```

- [ ] **Step 6: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): add backwards history paging within visible range"
```

---

### Task 12: Quality gate and CI

**Files:**
- Create: `.github/workflows/ci.yml`

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Then: `cargo fmt --all -- --check`
Expected: no output.

- [ ] **Step 2: Lint**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: `Finished` with no warnings. Fix anything reported; do not silence lints with
`allow` unless the reason is written next to it.

- [ ] **Step 3: Full test run**

Run: `cargo test --workspace`
Expected: all tests pass — 16 unit tests in the library (validation 5, model 2, clock 1,
memory 8) plus 47 integration tests (devices 5, contacts 10, direct 3, send 10, sync 9,
read_state 6, history 4).

- [ ] **Step 4: Add CI**

`.github/workflows/ci.yml`:

```yaml
name: ci

on:
  push:
    branches: [main]
  pull_request:

permissions:
  contents: read

jobs:
  rust:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace --all-targets --locked -- -D warnings
      - run: cargo test --workspace --locked
```

- [ ] **Step 5: Commit and push**

```powershell
git add .github crates Cargo.lock
git commit -m "ci: check formatting, lints and tests"
git push
```

- [ ] **Step 6: Confirm CI is green**

Run: `gh run watch --exit-status`
Expected: the `ci` workflow succeeds.

---

## Spec coverage for this plan

| Spec item | Task |
|---|---|
| §4.2 gapless `seq`, rollback on retry, sender check on replay | 8 |
| §4.2 membership and accepted-contact checks for direct chats | 8 |
| §4.2 notify only after commit, no body | 3, 8 |
| §4.3 sync within visible range, cumulative ack, forward-only | 9 |
| §4.3 eager delivery positions on device registration and conversation creation | 5, 7 |
| §4.4 delivered derived from device positions; read positions; sending marks read | 10 |
| §4.7 delivery positions never move on send | 10 |
| §5.3 revoked devices rejected | 5, 9 |
| §5.5 contact request, accept, decline, block (invisible to the blocked user) | 6 |
| §5.5 only accepted contacts can chat; one direct conversation per pair | 7, 8 |
| History for new devices | 11 |

Deferred to Plan 2: edit and delete, invites, groups, unread counts, receipt fan-out rules.

