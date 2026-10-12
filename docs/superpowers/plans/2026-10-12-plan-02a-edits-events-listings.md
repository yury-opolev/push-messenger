# Plan 2A — Edits, Events and Listings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend `push-domain` with typed transport events (entries available, receipts, contact changes), message edits and deletes with real erasure, delivery and read receipts for direct conversations, history pages that hold only messages, and conversation and contact listings with unread counts.

**Architecture:** Everything stays inside the `Messaging` service over the `Store`/`Tx` traits, one storage transaction per operation, events published only after commit. Edits and deletes are log entries appended through the same locked path as sends (`prepare_append`), and they update the target message row in place, so sync and history always see a message in its current state. Every new storage operation is a narrow `Tx` method with a written contract that the strict in-memory store implements now and Plan 3 implements on Postgres. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §3.2, §4.4–4.8, §5.5, §7.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85), tokio, async-trait, chrono, uuid (v7), thiserror. No new dependencies.

---

## Before you start

- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

  ```powershell
  git switch main
  git pull
  git switch -c feature/plan-02a-edits-events-listings
  ```

- The toolchain is rustup-managed (`C:\Users\yurio\.cargo\bin`). If `cargo --version` prints
  something older than 1.85, or `cargo clippy` is missing, the shell has a stale PATH. Fix it
  for the current PowerShell session:

  ```powershell
  $env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
  ```

- Baseline: `cargo test -p push-domain` passes 100 tests (22 unit, 78 integration).
- **Testing rule:** every test in this plan is written before the code it tests, and is run
  once to watch it fail. The one exception is marked: a test that pins behaviour Plan 1
  already has.
- **Every task ends green.** The last step of each task runs all three gates:

  ```powershell
  cargo test -p push-domain
  cargo clippy -p push-domain --all-targets -- -D warnings
  cargo fmt --all -- --check
  ```

  If `cargo fmt --all -- --check` reports a diff, run `cargo fmt --all` and check again.
  Never add `#[allow(dead_code)]` (the existing one in `tests/common/mod.rs` stays).
- **Scope rule:** invites and groups (create, add, remove, leave, rename, roles, system
  entries, the 100-member cap, group receipt aggregates) are Plan 2B. Do not add them.

## Key rules the code must preserve (from the spec)

- `seq` is gapless per conversation. Every failure path before commit rolls back, so an
  edit or delete that is refused does not consume a `seq`.
- A retry with a stored `client_msg_id` returns the original receipt **before** any
  permission, target or window check.
- Never acknowledge before commit. Events are published only after commit, carry no message
  bodies, and losing one only delays a client.
- Positions only move forward. Delivery positions move only on `ack`.
- A message is always stored, synced and listed in its current state. Erasure is real:
  after an edit only the current text exists; after a delete no text exists.
- A block is never revealed to the blocked user.
- The in-memory store stays strict: it enforces the same keys Postgres will, and every `Tx`
  method's documented behaviour (missing rows, ordering, upsert or insert, locking) is what
  it implements.

## Decisions made for this plan

These were settled while writing the plan. Do not reopen them during execution.

1. **Decline is announced only to the decliner.** The requester's row disappears silently,
   exactly as when they are blocked, so a decline and a block look the same to them.
2. **Sending can produce a read receipt.** Sending marks the conversation read for the
   sender (Plan 1). When that passes over a message from the other member of a direct
   conversation, the other member gets a `Receipt`, as spec §4.5 requires for any read
   position change.
3. **A `client_msg_id` reused for a different kind of entry is a `ClientIdConflict`.** A
   retry returns the original receipt only if the stored entry has the same sender **and**
   the same kind (message, edit or delete).
4. **Edits and deletes do not move the sender's read position.** Unread counts only count
   messages from others, so nothing depends on it.
5. **`updated_at` moves inside `next_seq(id, at)`.** One `UPDATE … SET last_seq = last_seq + 1,
   updated_at = $2 RETURNING last_seq` locks, counts and stamps in one statement, and a
   rollback undoes both.
6. **`entries_after` gains `targets_from`**, so the store skips edits and deletes of messages
   below the reader's visible range and a sync page is full whenever enough visible entries
   exist. Sync now asks for one entry more than it has room for, so `more` is exact.
7. **`entries_before` is replaced by `messages_before`**, which returns only `Message` and
   `System` entries. History was its only caller.
8. **The target must be inside the caller's visible range**; otherwise `TargetNotFound`.
9. **Error precedence for edit and delete:** `TargetNotFound`, then `NotSender`, then
   `AlreadyDeleted`, then `WindowExpired`. The window is inclusive: exactly 48 hours is
   still allowed.
10. **One counting method serves receipts and unread counts:**
    `count_incoming_messages(conversation, user, after, up_to, cap)` counts live messages
    from others. Receipts ask for at most 1; unread counts for at most `MAX_UNREAD` (100).
    A deleted message therefore triggers no receipt.
11. **`Event` and `ReceiptUpdate` are `#[non_exhaustive]`**, so Plan 2B can add
    `ReceiptUpdate::Group { all_delivered_seq, all_read_seq }` (and new events) without
    breaking callers.
12. **`RecordingNotifier::take` returns one flat `Vec<Event>`**, and the domain sorts every
    user list it publishes, so tests compare events exactly.
13. **`list_conversations` also lists conversations the user has left**, with `last_seq`
    capped at where they left. What former members see beyond that is refined in Plan 2B.
14. **`changed_since` reflects content changes only.** `updated_at` moves when a
    conversation is created or an entry is appended, never on an ack or a read, so the busy
    conversation row is not written on every receipt. Instead, a summary of a direct
    conversation carries the other member's positions (`peer_delivered_seq`,
    `peer_read_seq`), and a reconnecting client refreshes its ticks with one
    `list_conversations` without `changed_since`. Plan 2B replaces or extends these fields
    for groups.
15. **Read implies delivered.** Receipts and summaries report the delivered position as at
    least the read position.

## File structure

```
crates/push-domain/
├── src/
│   ├── lib.rs              + new modules and re-exports
│   ├── error.rs            + TargetNotFound, NotSender, AlreadyDeleted, WindowExpired,
│   │                         is_retryable
│   ├── validation.rs       + MAX_UNREAD
│   ├── model.rs            + Conversation::updated_at, ConversationSummary
│   ├── notify.rs           Event, ReceiptUpdate, Notifier::publish (replaces notify_users)
│   ├── store.rs            + entry_by_message_id, update_entry, erase_edit_bodies,
│   │                         count_incoming_messages, messages_before, contacts_of;
│   │                         next_seq(id, at); entries_after(.., targets_from, ..)
│   ├── memory.rs           the new Tx methods
│   ├── service.rs          + Policy, with_policy, publish, current_member_ids
│   ├── contacts.rs         contact events, list_contacts
│   ├── conversations.rs    + list_conversations
│   ├── send.rs             send_text on the shared prepare_append
│   ├── edit.rs             NEW: edit_text, delete_message, MessageRef
│   ├── sync.rs             visible-range filter for changes, delivery receipts on ack
│   ├── read.rs             read receipts, direct_receipt helper
│   └── history.rs          HistoryPage, messages only
└── tests/
    ├── common/mod.rs       + entries_for, conversation, with_policy, edit, delete helpers
    ├── contacts.rs         + contact events, list_contacts
    ├── send.rs             events instead of user lists, updated_at
    ├── sync.rs             + new-device position pin
    ├── history.rs          HistoryPage
    ├── visibility.rs       + hidden changes
    ├── edit.rs             NEW
    ├── delete.rs           NEW
    ├── receipts.rs         NEW
    └── listings.rs         NEW
```

---

### Task 1: Plan 1 review leftovers

**Files:**
- Modify: `crates/push-domain/src/error.rs`
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/model.rs`
- Modify: `crates/push-domain/tests/sync.rs`

Four small items from the Plan 1 review: a retryable flag for errors, two inaccurate doc
comments, and a pin for where a new device starts in a conversation its user has left.

- [ ] **Step 1: Write the failing unit test**

Append to the end of `crates/push-domain/src/error.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_conflict_is_retryable() {
        assert!(DomainError::Conflict.is_retryable());
        assert!(!DomainError::NotMember.is_retryable());
        assert!(!DomainError::Store("connection lost".into()).is_retryable());
    }
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p push-domain --lib error`
Expected: compile error — `no method named is_retryable found for enum DomainError`.

- [ ] **Step 3: Implement `is_retryable`**

In `crates/push-domain/src/error.rs`, insert between the enum and the `DomainResult` alias:

```rust
impl DomainError {
    /// Whether the same operation may succeed if the caller tries it again unchanged.
    /// Only `Conflict` is retryable: every other error repeats until something changes.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Conflict)
    }
}
```

- [ ] **Step 4: Run it to see it pass**

Run: `cargo test -p push-domain --lib error`
Expected: `1 passed`.

- [ ] **Step 5: Pin where a new device starts in a left conversation**

This pins behaviour Plan 1 already has, so it passes at once. In
`crates/push-domain/tests/sync.rs`, replace the test
`a_new_device_gets_nothing_from_a_conversation_already_left` with:

```rust
#[tokio::test]
async fn a_new_device_gets_nothing_from_a_conversation_already_left() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }
    world.set_visibility(chat, bob.user, 1, Some(2)).await;

    let new_device = world.add_device(&bob).await;
    assert_eq!(world.position(new_device, chat).await, Some(2));
    let batch = world.svc.sync(new_device, 100).await.unwrap();
    assert!(batch.entries.is_empty());
    let history = world.svc.history(bob.user, chat, None, 10).await.unwrap();
    assert_eq!(seqs(&history), [1, 2]);
}
```

Run: `cargo test -p push-domain --test sync`
Expected: `15 passed`.

- [ ] **Step 6: Fix the two doc comments**

In `crates/push-domain/src/store.rs`, replace the `# Delivery positions` section of the
module doc (from the heading down to `memberships rather than joining only on existing
positions.`) with:

```rust
//! # Delivery positions
//!
//! Positions are created eagerly, so a device normally has one for every membership of its
//! user. One can still be missing: a device registered concurrently with a membership
//! change reads the memberships before the new member row commits. Readers fall back to
//! `visible_from_seq - 1`, so sync iterates memberships rather than joining only on
//! existing positions.
```

In `crates/push-domain/src/model.rs`, replace the doc comment of
`MemberState::delivered_seq` (`/// Furthest delivery position across the member's active
devices.`) with:

```rust
    /// Furthest delivery position across all of the member's devices. Positions of revoked
    /// devices count: a delivery that happened stays delivered.
```

- [ ] **Step 7: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 101 tests pass, no warnings, no formatting diff.

- [ ] **Step 8: Commit**

```powershell
git add crates/push-domain
git commit -m "fix(domain): retryable errors and Plan 1 review leftovers"
```

---

### Task 2: Typed events replace the doorbell

**Files:**
- Modify: `crates/push-domain/src/notify.rs` (rewrite)
- Modify: `crates/push-domain/src/service.rs`
- Modify: `crates/push-domain/src/send.rs`
- Modify: `crates/push-domain/src/lib.rs`
- Modify: `crates/push-domain/tests/common/mod.rs`
- Modify: `crates/push-domain/tests/send.rs`

`Notifier::notify_users(&[UserId])` becomes `Notifier::publish(&[Event])`. A send publishes
`EntriesAvailable` for the current members. `Receipt` and `ContactChanged` are defined now
and published from Tasks 3 and 10.

- [ ] **Step 1: Add the test helper**

In `crates/push-domain/tests/common/mod.rs`, add `Event` to the `use push_domain::{…}` list,
and append at the end of the file:

```rust
/// The event that tells these people to sync.
pub fn entries_for(people: &[&Person]) -> Event {
    let mut users: Vec<UserId> = people.iter().map(|person| person.user).collect();
    users.sort();
    Event::EntriesAvailable { users }
}
```

- [ ] **Step 2: Rewrite the send tests that inspected notifications**

In `crates/push-domain/tests/send.rs`:

Replace the `use` lines at the top with:

```rust
use chrono::{TimeZone, Utc};
use common::{Person, World, entries_for, text};
use push_domain::{
    ConversationId, DeviceId, DomainError, Event, InMemoryStore, ManualClock, Messaging, Notifier,
    SendText, Store, Tx, UserId, ValidationError,
};
use std::sync::Arc;
```

Replace the `CommitProbe` doc comment, struct and `impl Notifier for CommitProbe` with:

```rust
/// Proves events are published after commit: it begins its own transaction (which would
/// wait forever on the in-memory store's lock if the sender's were still open) and checks
/// the new message is already visible.
struct CommitProbe {
    store: InMemoryStore,
    observed: std::sync::Mutex<Vec<i64>>,
}

#[async_trait::async_trait]
impl Notifier for CommitProbe {
    async fn publish(&self, events: &[Event]) {
        let Some(Event::EntriesAvailable { users }) = events.first() else {
            return;
        };
        let mut tx = tokio::time::timeout(std::time::Duration::from_secs(2), self.store.begin())
            .await
            .expect("published while the sending transaction was still open")
            .unwrap();
        // The conversation's last_seq as seen by an independent transaction.
        let memberships = tx.memberships(users[0]).await.unwrap();
        let mut observed = self.observed.lock().unwrap();
        observed.extend(
            memberships
                .iter()
                .map(|(conversation, _)| conversation.last_seq),
        );
    }
}
```

Rename the test `members_are_notified_only_after_the_send_commits` to
`events_are_published_only_after_the_send_commits` (its body is unchanged), and rename
`a_retry_is_not_notified_again` to `a_retry_publishes_nothing` (body unchanged).

Replace the test `members_are_notified_only_after_a_successful_send` with:

```rust
#[tokio::test]
async fn a_send_tells_every_member_to_sync_and_a_failed_one_tells_nobody() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.notifier.take();

    world.send(&alice, chat, "hi").await;
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &bob])]);

    let _ = world.svc.send_text(text(&alice, chat, "")).await;
    assert!(world.notifier.take().is_empty());
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p push-domain --test send`
Expected: compile error — `unresolved import push_domain::Event`.

- [ ] **Step 4: Rewrite the notifier**

Replace the whole of `crates/push-domain/src/notify.rs` with:

```rust
//! Events for the transport layer.
//!
//! After a commit, the service publishes typed events: which users have new log entries,
//! whose delivery or read position moved, and whose contacts changed. Events are best
//! effort and ephemeral. They carry no message bodies, and losing one only delays a
//! client, because everything they announce is durable and can be read back:
//!
//! - new entries through `sync`;
//! - contact changes through `list_contacts`;
//! - receipt positions through `list_conversations` called without `changed_since`.
//!   `changed_since` reflects content changes only (a conversation created or an entry
//!   appended). Receipt positions do not move `updated_at`, so the busy conversation row
//!   is not written on every ack and read. A reconnecting client refreshes its ticks with
//!   one listing without `changed_since`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::ids::{ConversationId, UserId};
use crate::model::ContactState;

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// New log entries exist for these users; their devices should sync.
    EntriesAvailable { users: Vec<UserId> },
    /// Delivery or read positions changed in a conversation; push a receipt to `to`.
    Receipt {
        conversation: ConversationId,
        to: Vec<UserId>,
        receipt: ReceiptUpdate,
    },
    /// A contact relationship changed as seen by `user`. `None` means no relationship.
    ContactChanged {
        user: UserId,
        peer: UserId,
        state: Option<ContactState>,
    },
}

/// The positions a receipt reports.
///
/// Group conversations will report aggregates in a variant of their own, so match this
/// enum with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReceiptUpdate {
    /// A direct conversation: the positions of the member whose position moved.
    /// `delivered_seq` is never below `read_seq`: a message read is a message delivered.
    Direct {
        user_id: UserId,
        delivered_seq: i64,
        read_seq: i64,
    },
}

#[async_trait]
pub trait Notifier: Send + Sync {
    /// Called only after the triggering transaction committed. Best effort: it must not
    /// fail the operation that triggered it.
    async fn publish(&self, events: &[Event]);
}

/// Discards events.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopNotifier;

#[async_trait]
impl Notifier for NoopNotifier {
    async fn publish(&self, _events: &[Event]) {}
}

/// Records events. For tests.
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    events: Mutex<Vec<Event>>,
}

impl RecordingNotifier {
    /// Returns and clears every event published so far, in publication order.
    pub fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().expect("notifier lock poisoned"))
    }
}

#[async_trait]
impl Notifier for RecordingNotifier {
    async fn publish(&self, events: &[Event]) {
        self.events
            .lock()
            .expect("notifier lock poisoned")
            .extend_from_slice(events);
    }
}

#[async_trait]
impl<T: Notifier + ?Sized> Notifier for Arc<T> {
    async fn publish(&self, events: &[Event]) {
        (**self).publish(events).await;
    }
}
```

In `crates/push-domain/src/lib.rs`, replace the `notify` re-export with:

```rust
pub use notify::{Event, NoopNotifier, Notifier, ReceiptUpdate, RecordingNotifier};
```

- [ ] **Step 5: Add the publishing helpers to the service**

In `crates/push-domain/src/service.rs`, replace the imports with:

```rust
use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::{DeviceId, UserId};
use crate::model::Member;
use crate::notify::{Event, Notifier};
use crate::store::{Store, Tx};
```

Add this method inside `impl<S, C, N> Messaging<S, C, N>`, just before `identify`:

```rust
    /// Publishes events after a commit. Publishes nothing when there is nothing to say.
    pub(crate) async fn publish(&self, events: Vec<Event>) {
        if !events.is_empty() {
            self.notifier.publish(&events).await;
        }
    }
```

Append at the end of the file:

```rust
/// The ids of the current members, sorted so events are deterministic.
pub(crate) fn current_member_ids(members: &[Member]) -> Vec<UserId> {
    let mut users: Vec<UserId> = members
        .iter()
        .filter(|member| member.is_current())
        .map(|member| member.user_id)
        .collect();
    users.sort();
    users
}
```

- [ ] **Step 6: Publish from `send_text`**

In `crates/push-domain/src/send.rs`, replace the two imports

```rust
use crate::notify::Notifier;
use crate::service::{Messaging, require_device};
```

with:

```rust
use crate::notify::{Event, Notifier};
use crate::service::{Messaging, current_member_ids, require_device};
```

In the doc comment of `send_text`, replace `notifies nobody` with `publishes nothing`.
Replace the notification at the end of `send_text`

```rust
        let recipients: Vec<UserId> = members
            .iter()
            .filter(|member| member.is_current())
            .map(|member| member.user_id)
            .collect();
        self.notifier.notify_users(&recipients).await;
```

with:

```rust
        self.publish(vec![Event::EntriesAvailable {
            users: current_member_ids(&members),
        }])
        .await;
```

- [ ] **Step 7: Run the tests to see them pass**

Run: `cargo test -p push-domain --test send`
Expected: `13 passed`.

- [ ] **Step 8: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 101 tests pass, no warnings, no formatting diff.

- [ ] **Step 9: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): publish typed events instead of a bare doorbell"
```

---

### Task 3: Contact events

**Files:**
- Modify: `crates/push-domain/src/contacts.rs` (rewrite)
- Modify: `crates/push-domain/tests/contacts.rs`

Each contact operation publishes `ContactChanged` to every user whose own view changed —
except that a block is never revealed: the blocked user gets no event for being blocked,
a request hidden by a block produces no event for the blocker until they unblock, and a
decline is announced only to the decliner (decision 1), so it looks like a block to the
requester. An operation that changes nothing publishes nothing.

- [ ] **Step 1: Write the failing tests**

In `crates/push-domain/tests/contacts.rs`, replace the two `use` lines at the top with the
following, which also adds a helper:

```rust
use common::{Person, World};
use push_domain::{ContactState, DomainError, Event, UserId, ValidationError};

fn changed(user: &Person, peer: &Person, state: Option<ContactState>) -> Event {
    Event::ContactChanged {
        user: user.user,
        peer: peer.user,
        state,
    }
}
```

Append to the end of the file:

```rust
#[tokio::test]
async fn a_request_and_its_acceptance_are_announced_to_both_users() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);

    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [
            changed(&alice, &bob, Some(ContactState::RequestedOut)),
            changed(&bob, &alice, Some(ContactState::RequestedIn)),
        ]
    );

    world
        .svc
        .contact_accept(bob.user, alice.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [
            changed(&bob, &alice, Some(ContactState::Accepted)),
            changed(&alice, &bob, Some(ContactState::Accepted)),
        ]
    );
}

#[tokio::test]
async fn crossing_requests_announce_acceptance_to_both_users() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    world.notifier.take();

    world
        .svc
        .contact_request(bob.user, alice.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [
            changed(&bob, &alice, Some(ContactState::Accepted)),
            changed(&alice, &bob, Some(ContactState::Accepted)),
        ]
    );
}

#[tokio::test]
async fn a_request_that_changes_nothing_publishes_nothing() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    world.notifier.take();

    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn a_decline_is_announced_only_to_the_user_who_declined() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    world.notifier.take();

    world
        .svc
        .contact_decline(bob.user, alice.user)
        .await
        .unwrap();
    assert_eq!(world.notifier.take(), [changed(&bob, &alice, None)]);
}

#[tokio::test]
async fn a_block_is_announced_only_to_the_blocker() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;
    world.notifier.take();

    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [changed(&alice, &bob, Some(ContactState::Blocked))]
    );
}

#[tokio::test]
async fn a_request_hidden_by_a_block_is_announced_only_when_the_block_is_lifted() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    world.notifier.take();

    world
        .svc
        .contact_request(bob.user, alice.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [changed(&bob, &alice, Some(ContactState::RequestedOut))]
    );

    world
        .svc
        .contact_unblock(alice.user, bob.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [changed(&alice, &bob, Some(ContactState::RequestedIn))]
    );
}

#[tokio::test]
async fn unblocking_someone_who_is_not_blocked_publishes_nothing() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_unblock(alice.user, bob.user)
        .await
        .unwrap();
    assert!(world.notifier.take().is_empty());
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p push-domain --test contacts`
Expected: 5 of the new tests fail on their first `assert_eq!` (`left: []`). The 16 existing
tests pass, and so do `a_request_that_changes_nothing_publishes_nothing` and
`unblocking_someone_who_is_not_blocked_publishes_nothing`, which guard against publishing
too much.

- [ ] **Step 3: Publish contact changes**

Replace the whole of `crates/push-domain/src/contacts.rs` with:

```rust
use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::ContactState;
use crate::notify::{Event, Notifier};
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
        let (result, their_new) = match (mine, theirs) {
            (Some(ContactState::Blocked), _) => return Err(DomainError::Blocked),
            (Some(ContactState::Accepted), _) => (ContactState::Accepted, theirs),
            // Hide the block: the requester sees an ordinary pending request, and the
            // blocker hears nothing until they unblock.
            (_, Some(ContactState::Blocked)) => (ContactState::RequestedOut, theirs),
            (_, Some(ContactState::RequestedOut)) => {
                (ContactState::Accepted, Some(ContactState::Accepted))
            }
            _ => (ContactState::RequestedOut, Some(ContactState::RequestedIn)),
        };
        let mut events = Vec::new();
        if set_contact(&mut tx, from, to, Some(result)).await? {
            events.push(contact_changed(from, to, Some(result)));
        }
        if set_contact(&mut tx, to, from, their_new).await? {
            events.push(contact_changed(to, from, their_new));
        }
        tx.commit().await?;
        self.publish(events).await;
        Ok(result)
    }

    /// Accepts a request that `peer` sent to `me`.
    pub async fn contact_accept(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
        }
        let accepted = Some(ContactState::Accepted);
        let mut events = Vec::new();
        for (owner, other) in [(me, peer), (peer, me)] {
            if set_contact(&mut tx, owner, other, accepted).await? {
                events.push(contact_changed(owner, other, accepted));
            }
        }
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }

    /// Declines a request that `peer` sent to `me`.
    ///
    /// The requester's pending request disappears without an event: a decline must look
    /// the same to them as a block.
    pub async fn contact_decline(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
        }
        set_contact(&mut tx, me, peer, None).await?;
        if tx.get_contact(peer, me).await? == Some(ContactState::RequestedOut) {
            set_contact(&mut tx, peer, me, None).await?;
        }
        tx.commit().await?;
        self.publish(vec![contact_changed(me, peer, None)]).await;
        Ok(())
    }

    /// Blocks `peer`. Ends any contact; `peer`'s side is removed unless they also block.
    /// `peer` is never told.
    pub async fn contact_block(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        let blocked = Some(ContactState::Blocked);
        let mut events = Vec::new();
        if set_contact(&mut tx, me, peer, blocked).await? {
            events.push(contact_changed(me, peer, blocked));
        }
        if tx.get_contact(peer, me).await? != blocked {
            set_contact(&mut tx, peer, me, None).await?;
        }
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }

    /// Removes a block. A request the block had hidden becomes an ordinary pending request.
    pub async fn contact_unblock(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        let mut events = Vec::new();
        if tx.get_contact(me, peer).await? == Some(ContactState::Blocked) {
            let restored = match tx.get_contact(peer, me).await? {
                Some(ContactState::RequestedOut) => Some(ContactState::RequestedIn),
                _ => None,
            };
            set_contact(&mut tx, me, peer, restored).await?;
            events.push(contact_changed(me, peer, restored));
        }
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
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
        tx.lock_contact_pair(a, b).await?;
        Ok(tx)
    }
}

/// Whether both directions are `Accepted`.
pub(crate) async fn are_contacts<T: Tx>(tx: &mut T, a: UserId, b: UserId) -> DomainResult<bool> {
    Ok(tx.get_contact(a, b).await? == Some(ContactState::Accepted)
        && tx.get_contact(b, a).await? == Some(ContactState::Accepted))
}

/// Writes `owner`'s view of `peer`; `None` removes it. Returns whether it changed.
async fn set_contact<T: Tx>(
    tx: &mut T,
    owner: UserId,
    peer: UserId,
    state: Option<ContactState>,
) -> DomainResult<bool> {
    if tx.get_contact(owner, peer).await? == state {
        return Ok(false);
    }
    match state {
        Some(state) => tx.put_contact(owner, peer, state).await?,
        None => tx.delete_contact(owner, peer).await?,
    }
    Ok(true)
}

fn contact_changed(user: UserId, peer: UserId, state: Option<ContactState>) -> Event {
    Event::ContactChanged { user, peer, state }
}
```

`set_contact` writes only when the state differs, so "did this user's view change" is
answered by the store row itself.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p push-domain --test contacts`
Expected: `23 passed`.

- [ ] **Step 5: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 108 tests pass, no warnings, no formatting diff.

- [ ] **Step 6: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): publish contact changes without revealing blocks"
```

---

### Task 4: Track when a conversation last changed

**Files:**
- Modify: `crates/push-domain/src/model.rs`
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/memory.rs`
- Modify: `crates/push-domain/src/conversations.rs`
- Modify: `crates/push-domain/src/send.rs`
- Modify: `crates/push-domain/tests/common/mod.rs`
- Modify: `crates/push-domain/tests/send.rs`

`Conversation` gains `updated_at`, set when the conversation is created and whenever an
entry is appended. `list_conversations{changed_since}` (Task 12) filters on it. It moves in
`next_seq`, which already locks the row (decision 5).

- [ ] **Step 1: Write the failing unit test**

In `crates/push-domain/src/memory.rs`, inside `mod tests`, replace the two imports after
`use super::*;` and the `conversation()` helper with:

```rust
    use crate::model::{ContentType, ConversationKind, EntryKind, Role};
    use chrono::{TimeDelta, TimeZone};

    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn conversation() -> Conversation {
        Conversation {
            id: ConversationId::new(),
            kind: ConversationKind::Direct,
            direct_key: None,
            title: None,
            created_by: UserId::new(),
            last_seq: 0,
            created_at: start(),
            updated_at: start(),
        }
    }
```

In the `entry()` helper, replace `server_ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),`
with `server_ts: start(),`.

Replace the test `next_seq_counts_from_one_and_a_rollback_gives_the_number_back` with the
following two tests:

```rust
    #[tokio::test]
    async fn next_seq_counts_from_one_and_a_rollback_gives_the_number_back() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;

        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        assert_eq!(tx.next_seq(id, start()).await.unwrap(), 1);
        tx.commit().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(tx.next_seq(id, start()).await.unwrap(), 2);
        tx.rollback().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(tx.next_seq(id, start()).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn next_seq_moves_updated_at_and_a_rollback_restores_it() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let (first, second) = (start() + TimeDelta::hours(1), start() + TimeDelta::hours(2));

        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        tx.next_seq(id, first).await.unwrap();
        tx.commit().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        tx.next_seq(id, second).await.unwrap();
        tx.rollback().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        let stored = tx.get_conversation(id).await.unwrap().unwrap();
        assert_eq!(stored.updated_at, first);
    }
```

In the test `operations_on_missing_rows_fail_as_documented`, replace
`tx.next_seq(ConversationId::new()).await` with `tx.next_seq(ConversationId::new(), start()).await`.

- [ ] **Step 2: Write the failing integration test**

In `crates/push-domain/tests/common/mod.rs`, add `Conversation` to the `use push_domain::{…}`
list and add this method inside `impl World`, after `position`:

```rust
    /// A conversation row, read straight from the store.
    pub async fn conversation(&self, id: ConversationId) -> Conversation {
        let mut tx = self.store.begin().await.unwrap();
        let conversation = tx.get_conversation(id).await.unwrap().unwrap();
        tx.rollback().await.unwrap();
        conversation
    }
```

In `crates/push-domain/tests/send.rs`, replace the `use` lines at the top with:

```rust
use chrono::{TimeDelta, TimeZone, Utc};
use common::{Person, World, entries_for, text};
use push_domain::{
    Clock, ConversationId, DeviceId, DomainError, Event, InMemoryStore, ManualClock, Messaging,
    Notifier, SendText, Store, Tx, UserId, ValidationError,
};
use std::sync::Arc;
```

and append:

```rust
#[tokio::test]
async fn opening_and_sending_set_the_conversations_updated_at() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let opened_at = world.clock.now();
    let chat = world.direct_chat(&alice, &bob).await;
    assert_eq!(world.conversation(chat).await.updated_at, opened_at);

    world.clock.advance(TimeDelta::minutes(5));
    world.send(&alice, chat, "hi").await;
    assert_eq!(
        world.conversation(chat).await.updated_at,
        opened_at + TimeDelta::minutes(5)
    );
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p push-domain`
Expected: compile errors — `struct Conversation has no field named updated_at` and
`this method takes 1 argument but 2 arguments were supplied` for `next_seq`.

- [ ] **Step 4: Add the field and the new `next_seq` contract**

In `crates/push-domain/src/model.rs`, add the field at the end of `struct Conversation`,
after `created_at`:

```rust
    /// When the conversation was created or last had an entry appended.
    pub updated_at: DateTime<Utc>,
```

In `crates/push-domain/src/store.rs`, add `use chrono::{DateTime, Utc};` after
`use async_trait::async_trait;`, and replace the `next_seq` declaration with:

```rust
    /// Locks the conversation, increments `last_seq`, sets `updated_at` to `at` and
    /// returns the new `last_seq`. Both changes are undone if the transaction rolls back.
    /// Fails with `ConversationNotFound` if the conversation does not exist.
    async fn next_seq(&mut self, id: ConversationId, at: DateTime<Utc>) -> DomainResult<i64>;
```

In `crates/push-domain/src/memory.rs`, add `use chrono::{DateTime, Utc};` after
`use async_trait::async_trait;`, and replace `next_seq` in `impl Tx for InMemoryTx` with:

```rust
    async fn next_seq(&mut self, id: ConversationId, at: DateTime<Utc>) -> DomainResult<i64> {
        let conversation = self
            .work
            .conversations
            .get_mut(&id)
            .ok_or(DomainError::ConversationNotFound)?;
        conversation.last_seq += 1;
        conversation.updated_at = at;
        Ok(conversation.last_seq)
    }
```

- [ ] **Step 5: Stamp the time when creating and appending**

In `crates/push-domain/src/conversations.rs`, in `open_direct`, replace the
`let conversation = Conversation { … };` statement with:

```rust
        let now = self.clock.now();
        let conversation = Conversation {
            id: ConversationId::new(),
            kind: ConversationKind::Direct,
            direct_key: Some(key),
            title: None,
            created_by: me,
            last_seq: 0,
            created_at: now,
            updated_at: now,
        };
```

In `crates/push-domain/src/send.rs`, in `send_text`, replace

```rust
        let seq = tx.next_seq(conversation.id).await?;
```

with:

```rust
        let now = self.clock.now();
        let seq = tx.next_seq(conversation.id, now).await?;
```

and in the `Entry` literal replace `server_ts: self.clock.now(),` with `server_ts: now,`.

- [ ] **Step 6: Run the tests to see them pass**

Run: `cargo test -p push-domain`
Expected: 110 tests pass (24 unit, 86 integration).

- [ ] **Step 7: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 110 tests pass, no warnings, no formatting diff.

- [ ] **Step 8: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): track when a conversation last changed"
```

---

### Task 5: Storage for changing entries in place

**Files:**
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/memory.rs`

Three `Tx` methods that edits and deletes need: find an entry by its server id, overwrite
the mutable fields of an entry, and erase the text of every edit of a message. Postgres
will serve `erase_edit_bodies` from the spec's index on `(conversation_id, target_seq)`.

- [ ] **Step 1: Write the failing unit tests**

In the top-level imports of `crates/push-domain/src/memory.rs`, replace
`use crate::model::{ContactState, Conversation, Entry, Member};` with:

```rust
use crate::model::{ContactState, Conversation, Entry, EntryKind, Member};
```

That brings `EntryKind` into the whole file, so inside `mod tests` replace
`use crate::model::{ContentType, ConversationKind, EntryKind, Role};` with
`use crate::model::{ContentType, ConversationKind, Role};`, and append these tests at the
end of `mod tests`:

```rust
    #[tokio::test]
    async fn an_entry_is_found_by_message_id_only_in_its_own_conversation() {
        let store = InMemoryStore::new();
        let (home, elsewhere) = (conversation(), conversation());
        let (home_id, elsewhere_id) = (home.id, elsewhere.id);
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(home).await.unwrap();
        tx.insert_conversation(elsewhere).await.unwrap();
        let stored = entry(home_id, 1, ClientMsgId::new());
        tx.insert_entry(stored.clone()).await.unwrap();

        assert_eq!(
            tx.entry_by_message_id(home_id, stored.message_id)
                .await
                .unwrap(),
            Some(stored.clone())
        );
        assert_eq!(
            tx.entry_by_message_id(elsewhere_id, stored.message_id)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            tx.entry_by_message_id(home_id, MessageId::new())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn update_entry_changes_only_the_mutable_fields() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let original = entry(id, 1, ClientMsgId::new());
        tx.insert_entry(original.clone()).await.unwrap();

        let mut changed = original.clone();
        changed.body = None;
        changed.edited_at = Some(start() + TimeDelta::hours(1));
        changed.edit_seq = Some(4);
        changed.deleted_at = Some(start() + TimeDelta::hours(2));
        changed.sender_id = UserId::new();
        changed.kind = EntryKind::Edit;
        tx.update_entry(changed.clone()).await.unwrap();

        let stored = tx
            .entry_by_message_id(id, original.message_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.body, None);
        assert_eq!(stored.edited_at, changed.edited_at);
        assert_eq!(stored.edit_seq, Some(4));
        assert_eq!(stored.deleted_at, changed.deleted_at);
        assert_eq!(stored.sender_id, original.sender_id);
        assert_eq!(stored.kind, EntryKind::Message);
    }

    #[tokio::test]
    async fn updating_a_missing_entry_fails() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();

        assert!(matches!(
            tx.update_entry(entry(id, 1, ClientMsgId::new())).await,
            Err(DomainError::Store(_))
        ));
    }

    #[tokio::test]
    async fn erasing_edit_bodies_touches_only_edits_of_that_target() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let edit = |seq, target| Entry {
            kind: EntryKind::Edit,
            target_seq: Some(target),
            ..entry(id, seq, ClientMsgId::new())
        };
        tx.insert_entry(entry(id, 1, ClientMsgId::new()))
            .await
            .unwrap();
        tx.insert_entry(entry(id, 2, ClientMsgId::new()))
            .await
            .unwrap();
        tx.insert_entry(edit(3, 1)).await.unwrap();
        tx.insert_entry(edit(4, 2)).await.unwrap();
        tx.insert_entry(edit(5, 1)).await.unwrap();

        tx.erase_edit_bodies(id, 1).await.unwrap();
        let bodies: Vec<bool> = tx
            .entries_after(id, 0, 5, 10)
            .await
            .unwrap()
            .iter()
            .map(|entry| entry.body.is_some())
            .collect();
        assert_eq!(bodies, [true, true, false, true, false]);
    }
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p push-domain --lib memory`
Expected: compile error — `no method named entry_by_message_id found`.

- [ ] **Step 3: Declare the contracts**

In `crates/push-domain/src/store.rs`, replace the `ids` import with:

```rust
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
```

and add these declarations right after `entry_by_client_id`:

```rust
    /// The entry with this `message_id`; `None` if there is none or it belongs to another
    /// conversation.
    async fn entry_by_message_id(
        &mut self,
        conversation: ConversationId,
        message_id: MessageId,
    ) -> DomainResult<Option<Entry>>;
    /// Overwrites the mutable fields of the stored entry at `(conversation_id, seq)`:
    /// `body`, `edited_at`, `edit_seq` and `deleted_at`. Every other field of `entry` is
    /// ignored. Fails with `DomainError::Store` if no such entry exists.
    async fn update_entry(&mut self, entry: Entry) -> DomainResult<()>;
    /// Sets `body` to null on every `Edit` entry whose `target_seq` is `target_seq`.
    /// A no-op if there are none. Postgres serves it from the index on
    /// `(conversation_id, target_seq)`.
    async fn erase_edit_bodies(
        &mut self,
        conversation: ConversationId,
        target_seq: i64,
    ) -> DomainResult<()>;
```

- [ ] **Step 4: Implement them in memory**

`message_ids` becomes a map to the entry's key, so lookups by server id are direct. In
`struct State`, replace `message_ids: HashSet<MessageId>,` with:

```rust
    message_ids: HashMap<MessageId, (ConversationId, i64)>,
```

Replace `insert_entry` with:

```rust
    async fn insert_entry(&mut self, entry: Entry) -> DomainResult<InsertOutcome> {
        if !self.work.conversations.contains_key(&entry.conversation_id) {
            return Err(missing("conversation"));
        }
        let key = (entry.conversation_id, entry.client_msg_id);
        if self.work.client_ids.contains_key(&key) {
            return Ok(InsertOutcome::Duplicate);
        }
        if self
            .work
            .entries
            .contains_key(&(entry.conversation_id, entry.seq))
        {
            return Err(DomainError::Store(
                "primary key violation: an entry with this seq already exists".into(),
            ));
        }
        if self.work.message_ids.contains_key(&entry.message_id) {
            return Err(DomainError::Store(
                "unique violation: message_id already exists".into(),
            ));
        }
        self.work
            .message_ids
            .insert(entry.message_id, (entry.conversation_id, entry.seq));
        self.work.client_ids.insert(key, entry.seq);
        self.work
            .entries
            .insert((entry.conversation_id, entry.seq), entry);
        Ok(InsertOutcome::Inserted)
    }
```

Add these methods after `entry_by_client_id`:

```rust
    async fn entry_by_message_id(
        &mut self,
        conversation: ConversationId,
        message_id: MessageId,
    ) -> DomainResult<Option<Entry>> {
        Ok(self
            .work
            .message_ids
            .get(&message_id)
            .filter(|(owner, _)| *owner == conversation)
            .and_then(|key| self.work.entries.get(key))
            .cloned())
    }

    async fn update_entry(&mut self, entry: Entry) -> DomainResult<()> {
        let stored = self
            .work
            .entries
            .get_mut(&(entry.conversation_id, entry.seq))
            .ok_or_else(|| DomainError::Store("update of an entry that does not exist".into()))?;
        stored.body = entry.body;
        stored.edited_at = entry.edited_at;
        stored.edit_seq = entry.edit_seq;
        stored.deleted_at = entry.deleted_at;
        Ok(())
    }

    async fn erase_edit_bodies(
        &mut self,
        conversation: ConversationId,
        target_seq: i64,
    ) -> DomainResult<()> {
        for entry in self
            .work
            .entries
            .range_mut((conversation, i64::MIN)..=(conversation, i64::MAX))
            .map(|(_, entry)| entry)
            .filter(|entry| entry.kind == EntryKind::Edit && entry.target_seq == Some(target_seq))
        {
            entry.body = None;
        }
        Ok(())
    }
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p push-domain --lib memory`
Expected: `19 passed`.

- [ ] **Step 6: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 114 tests pass, no warnings, no formatting diff.

- [ ] **Step 7: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): storage for changing entries in place"
```

---

### Task 6: Edit messages

**Files:**
- Create: `crates/push-domain/src/edit.rs`
- Create: `crates/push-domain/tests/edit.rs`
- Modify: `crates/push-domain/src/send.rs` (rewrite)
- Modify: `crates/push-domain/src/service.rs`
- Modify: `crates/push-domain/src/error.rs`
- Modify: `crates/push-domain/src/lib.rs`
- Modify: `crates/push-domain/tests/common/mod.rs`

An edit has the same transaction shape as a send (spec §4.2, §4.6), so the start of
`send_text` moves into a shared `prepare_append`:

1. validate the body; check the device belongs to the sender;
2. lock the conversation and take the next `seq` (`next_seq`);
3. if the `client_msg_id` is stored, roll back and return the original receipt — before any
   permission check (same sender and same kind required, decision 3);
4. check the sender is a current member and, for direct chats, an accepted contact.

Then the edit resolves its target (a `Message` of this conversation inside the caller's
visible range), checks sender, deletion and window (decision 9), erases the text of older
edits, inserts the edit entry, updates the message row, commits and publishes
`EntriesAvailable`.

- [ ] **Step 1: Extend the test harness**

In `crates/push-domain/tests/common/mod.rs`, replace the imports after
`#![allow(dead_code)]` with:

```rust
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use push_domain::{
    ClientMsgId, Conversation, ConversationId, DeviceId, EditText, Entry, Event, InMemoryStore,
    ManualClock, MessageRef, Messaging, Policy, RecordingNotifier, SendReceipt, SendText, Store,
    Tx, UserId,
};
```

Replace `World::new` with these two constructors:

```rust
    pub fn new() -> Self {
        Self::with_policy(Policy::default())
    }

    pub fn with_policy(policy: Policy) -> Self {
        let clock = Arc::new(ManualClock::new(
            Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
        ));
        let notifier = Arc::new(RecordingNotifier::default());
        let store = InMemoryStore::new();
        let svc = Messaging::with_policy(
            store.clone(),
            Arc::clone(&clock),
            Arc::clone(&notifier),
            policy,
        );
        Self {
            svc,
            store,
            clock,
            notifier,
        }
    }
```

Add this method inside `impl World`, after `send`:

```rust
    /// Edits a message by its server id and expects it to succeed.
    pub async fn edit(
        &self,
        from: &Person,
        conversation: ConversationId,
        target: &SendReceipt,
        body: &str,
    ) -> SendReceipt {
        self.svc
            .edit_text(edit(
                from,
                conversation,
                MessageRef::Id(target.message_id),
                body,
            ))
            .await
            .unwrap()
    }
```

Add this free function before `text`:

```rust
/// An edit command with a fresh client message id.
pub fn edit(
    from: &Person,
    conversation: ConversationId,
    target: MessageRef,
    body: &str,
) -> EditText {
    EditText {
        sender: from.user,
        device: from.device,
        conversation,
        client_msg_id: ClientMsgId::new(),
        target,
        body: body.as_bytes().to_vec(),
        client_ts: None,
    }
}
```

- [ ] **Step 2: Write the failing tests**

`crates/push-domain/tests/edit.rs`:

```rust
mod common;

use chrono::TimeDelta;
use common::{Person, World, bodies, edit, entries_for, seqs, text};
use push_domain::{
    ClientMsgId, Clock, ConversationId, DomainError, EditText, Entry, EntryKind, MessageId,
    MessageRef, Policy, SendReceipt, ValidationError,
};

/// Alice and bob in a direct chat, with one message "hello" from alice at seq 1.
async fn chat_with_one_message() -> (World, Person, Person, ConversationId, SendReceipt) {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    (world, alice, bob, chat, hello)
}

fn by_id(target: &SendReceipt) -> MessageRef {
    MessageRef::Id(target.message_id)
}

async fn synced(world: &World, person: &Person) -> Vec<Entry> {
    world.svc.sync(person.device, 100).await.unwrap().entries
}

#[tokio::test]
async fn an_edit_is_an_entry_of_its_own_and_updates_the_message() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.clock.advance(TimeDelta::minutes(1));

    let receipt = world.edit(&alice, chat, &hello, "hello again").await;
    assert_eq!(receipt.seq, 2);

    let entries = synced(&world, &bob).await;
    assert_eq!(seqs(&entries), [1, 2]);
    assert_eq!(bodies(&entries), ["hello again", "hello again"]);
    let (message, change) = (&entries[0], &entries[1]);
    assert_eq!(message.kind, EntryKind::Message);
    assert_eq!(message.edited_at, Some(world.clock.now()));
    assert_eq!(message.edit_seq, Some(2));
    assert_eq!(change.kind, EntryKind::Edit);
    assert_eq!(change.target_seq, Some(1));
    assert_eq!(change.message_id, receipt.message_id);
}

#[tokio::test]
async fn only_the_latest_edit_keeps_its_text() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.edit(&alice, chat, &hello, "v1").await;
    world.edit(&alice, chat, &hello, "v2").await;

    let entries = synced(&world, &bob).await;
    let texts: Vec<Option<&[u8]>> = entries.iter().map(|entry| entry.body.as_deref()).collect();
    assert_eq!(
        texts,
        [Some(&b"v2"[..]), None, Some(&b"v2"[..])],
        "message, first edit, second edit"
    );
    assert_eq!(entries[0].edit_seq, Some(3));
}

#[tokio::test]
async fn a_message_can_be_targeted_by_its_client_msg_id() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let command = text(&alice, chat, "hello");
    let target = MessageRef::ClientId(command.client_msg_id);
    world.svc.send_text(command).await.unwrap();

    world
        .svc
        .edit_text(edit(&alice, chat, target, "fixed"))
        .await
        .unwrap();
    assert_eq!(bodies(&synced(&world, &bob).await)[0], "fixed");
}

#[tokio::test]
async fn an_edit_retry_returns_the_original_and_changes_nothing() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    let command = edit(&alice, chat, by_id(&hello), "v1");
    let first = world.svc.edit_text(command.clone()).await.unwrap();
    world.notifier.take();

    let retry = world.svc.edit_text(command).await.unwrap();
    assert_eq!(retry, first);
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.send(&alice, chat, "next").await.seq, 3);
}

#[tokio::test]
async fn an_edit_retry_succeeds_even_after_the_window_has_passed() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    let command = edit(&alice, chat, by_id(&hello), "v1");
    let first = world.svc.edit_text(command.clone()).await.unwrap();
    world.clock.advance(TimeDelta::days(3));

    assert_eq!(world.svc.edit_text(command).await, Ok(first));
}

#[tokio::test]
async fn only_the_sender_can_edit() {
    let (world, _alice, bob, chat, hello) = chat_with_one_message().await;
    assert_eq!(
        world
            .svc
            .edit_text(edit(&bob, chat, by_id(&hello), "mine now"))
            .await,
        Err(DomainError::NotSender)
    );
    assert_eq!(world.send(&bob, chat, "gapless").await.seq, 2);
}

#[tokio::test]
async fn the_sender_may_edit_from_another_device() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    let laptop = world.add_device(&alice).await;
    let command = EditText {
        device: laptop,
        ..edit(&alice, chat, by_id(&hello), "from the laptop")
    };

    world.svc.edit_text(command).await.unwrap();
    assert_eq!(bodies(&synced(&world, &bob).await)[0], "from the laptop");
}

#[tokio::test]
async fn the_target_must_be_a_message_of_this_conversation() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    let carol = world.person().await;
    let other_chat = world.direct_chat(&alice, &carol).await;
    let elsewhere = world.send(&alice, other_chat, "elsewhere").await;
    let change = world.edit(&alice, chat, &hello, "v1").await;

    for target in [
        MessageRef::Id(MessageId::new()),
        MessageRef::ClientId(ClientMsgId::new()),
        by_id(&elsewhere),
        by_id(&change),
    ] {
        assert_eq!(
            world.svc.edit_text(edit(&alice, chat, target, "x")).await,
            Err(DomainError::TargetNotFound)
        );
    }
}

#[tokio::test]
async fn a_target_outside_the_callers_visible_range_is_not_found() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    world.set_visibility(chat, alice.user, 2, None).await;

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "x"))
            .await,
        Err(DomainError::TargetNotFound)
    );
}

#[tokio::test]
async fn an_edit_is_allowed_until_the_window_closes() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    world.clock.advance(TimeDelta::hours(48));
    world.edit(&alice, chat, &hello, "just in time").await;

    world.clock.advance(TimeDelta::seconds(1));
    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "too late"))
            .await,
        Err(DomainError::WindowExpired)
    );
}

#[tokio::test]
async fn the_edit_window_comes_from_the_policy() {
    let world = World::with_policy(Policy {
        edit_window: TimeDelta::minutes(1),
        ..Policy::default()
    });
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    world.clock.advance(TimeDelta::minutes(2));

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "late"))
            .await,
        Err(DomainError::WindowExpired)
    );
}

#[tokio::test]
async fn an_invalid_edit_is_rejected_and_uses_no_seq() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), ""))
            .await,
        Err(DomainError::Invalid(ValidationError::EmptyBody))
    );
    assert_eq!(world.send(&alice, chat, "next").await.seq, 2);
}

#[tokio::test]
async fn a_blocked_sender_cannot_edit() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.svc.contact_block(bob.user, alice.user).await.unwrap();

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "x"))
            .await,
        Err(DomainError::NotContact)
    );
}

#[tokio::test]
async fn a_client_msg_id_cannot_be_reused_for_another_kind_of_entry() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let command = text(&alice, chat, "hello");
    let reused = command.client_msg_id;
    let hello = world.svc.send_text(command).await.unwrap();

    let clash = EditText {
        client_msg_id: reused,
        ..edit(&alice, chat, by_id(&hello), "x")
    };
    assert_eq!(
        world.svc.edit_text(clash).await,
        Err(DomainError::ClientIdConflict)
    );
}

#[tokio::test]
async fn an_edit_tells_every_member_to_sync() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.notifier.take();

    world.edit(&alice, chat, &hello, "v1").await;
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &bob])]);
}

#[tokio::test]
async fn an_edit_moves_the_conversations_updated_at() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    world.clock.advance(TimeDelta::minutes(3));

    world.edit(&alice, chat, &hello, "v1").await;
    assert_eq!(world.conversation(chat).await.updated_at, world.clock.now());
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p push-domain --test edit`
Expected: compile error — `unresolved imports push_domain::EditText, push_domain::MessageRef,
push_domain::Policy`.

- [ ] **Step 4: Add the errors**

In `crates/push-domain/src/error.rs`, a `client_msg_id` can now clash with an entry of
another kind from the same sender too (decision 3), so replace the message of
`ClientIdConflict`:

```rust
    #[error("this client message id was already used for a different entry")]
    ClientIdConflict,
```

and add these variants after `SeqOutOfRange`:

```rust
    #[error("the target message does not exist in this conversation")]
    TargetNotFound,
    #[error("only the sender of a message can change it")]
    NotSender,
    #[error("the message is already deleted")]
    AlreadyDeleted,
    #[error("the time allowed for this change has passed")]
    WindowExpired,
```

- [ ] **Step 5: Add the policy**

In `crates/push-domain/src/service.rs`, add `use chrono::TimeDelta;` followed by a blank
line above `use crate::clock::Clock;`. Replace the `Messaging` struct (with its doc comment)
by:

```rust
/// Server settings for changing messages after they are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// How long after sending its sender may edit a message.
    pub edit_window: TimeDelta,
    /// How long after sending its sender may delete a message.
    pub delete_window: TimeDelta,
}

impl Default for Policy {
    /// 48 hours for both.
    fn default() -> Self {
        Self {
            edit_window: TimeDelta::hours(48),
            delete_window: TimeDelta::hours(48),
        }
    }
}

/// The messaging service. Every operation runs in one storage transaction.
///
/// A transaction dropped without `commit` is rolled back, so an early return through `?`
/// never leaves partial changes behind.
#[derive(Debug)]
pub struct Messaging<S, C, N> {
    pub(crate) store: S,
    pub(crate) clock: C,
    pub(crate) notifier: N,
    pub(crate) policy: Policy,
}
```

Replace `Messaging::new` with:

```rust
    /// A service with the default [`Policy`].
    pub fn new(store: S, clock: C, notifier: N) -> Self {
        Self::with_policy(store, clock, notifier, Policy::default())
    }

    pub fn with_policy(store: S, clock: C, notifier: N, policy: Policy) -> Self {
        Self {
            store,
            clock,
            notifier,
            policy,
        }
    }
```

- [ ] **Step 6: Move the start of `send_text` into `prepare_append`**

Replace the whole of `crates/push-domain/src/send.rs` with:

```rust
use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::conversations::require_conversation;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{
    ContentType, Conversation, ConversationKind, Entry, EntryKind, Member, SendReceipt,
};
use crate::notify::{Event, Notifier};
use crate::service::{Messaging, current_member_ids, require_device};
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

/// An open transaction holding the conversation lock, allowed to append one entry at `seq`.
pub(crate) struct Append<T> {
    pub(crate) tx: T,
    pub(crate) conversation: Conversation,
    /// Every member row, current and former.
    pub(crate) members: Vec<Member>,
    /// The sender's member row.
    pub(crate) member: Member,
    pub(crate) seq: i64,
    pub(crate) now: DateTime<Utc>,
}

pub(crate) enum Prepared<T> {
    /// The `client_msg_id` is already stored: this is its original receipt.
    Retry(SendReceipt),
    Ready(Append<T>),
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Appends a text message. A retry with the same `client_msg_id` returns the original
    /// receipt, stores nothing new and publishes nothing — even if the sender has since lost
    /// the right to send.
    pub async fn send_text(&self, command: SendText) -> DomainResult<SendReceipt> {
        validate_text(&command.body)?;
        let prepared = self
            .prepare_append(
                command.sender,
                command.device,
                command.conversation,
                command.client_msg_id,
                EntryKind::Message,
            )
            .await?;
        let Append {
            mut tx,
            conversation,
            members,
            seq,
            now,
            ..
        } = match prepared {
            Prepared::Retry(receipt) => return Ok(receipt),
            Prepared::Ready(append) => append,
        };

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
            server_ts: now,
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        };
        let receipt = SendReceipt::from(&entry);

        if tx.insert_entry(entry).await? == InsertOutcome::Duplicate {
            // Backstop: the lookup in `prepare_append` normally catches retries. Rolling back
            // returns `seq` to the counter, keeping the log gapless.
            tx.rollback().await?;
            return self
                .replay_append(
                    conversation.id,
                    command.client_msg_id,
                    command.sender,
                    EntryKind::Message,
                )
                .await;
        }
        tx.raise_read_seq(conversation.id, command.sender, seq)
            .await?;
        tx.commit().await?;

        self.publish(vec![Event::EntriesAvailable {
            users: current_member_ids(&members),
        }])
        .await;
        Ok(receipt)
    }

    /// The common start of every append (spec §4.2): checks the device, locks the
    /// conversation and takes the next `seq`, answers a retry with its original receipt,
    /// then checks that the sender may write.
    pub(crate) async fn prepare_append(
        &self,
        sender: UserId,
        device: DeviceId,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
        kind: EntryKind,
    ) -> DomainResult<Prepared<S::Tx>> {
        let mut tx = self.store.begin().await?;
        if require_device(&mut tx, device).await? != sender {
            return Err(DomainError::UnknownDevice);
        }
        let conversation = require_conversation(&mut tx, conversation).await?;
        let now = self.clock.now();
        let seq = tx.next_seq(conversation.id, now).await?;

        // A non-member reusing an existing `client_msg_id` gets `ClientIdConflict`; that is
        // acceptable because ids are random and chosen by the client.
        // A retry of an entry that is already stored always gets its original receipt,
        // even if the sender has since lost the right to send. Checked under the
        // conversation lock taken by `next_seq`, so it cannot race a concurrent attempt.
        if let Some(original) = tx
            .entry_by_client_id(conversation.id, client_msg_id)
            .await?
        {
            tx.rollback().await?;
            return receipt_for(&original, sender, kind).map(Prepared::Retry);
        }

        let members = tx.members(conversation.id).await?;
        let member = members
            .iter()
            .find(|member| member.user_id == sender && member.is_current())
            .cloned()
            .ok_or(DomainError::NotMember)?;
        if conversation.kind == ConversationKind::Direct {
            for peer in members.iter().filter(|member| member.user_id != sender) {
                if !are_contacts(&mut tx, sender, peer.user_id).await? {
                    return Err(DomainError::NotContact);
                }
            }
        }
        Ok(Prepared::Ready(Append {
            tx,
            conversation,
            members,
            member,
            seq,
            now,
        }))
    }

    /// The receipt of an already stored entry, if it is the same sender's entry of the
    /// same kind.
    pub(crate) async fn replay_append(
        &self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
        sender: UserId,
        kind: EntryKind,
    ) -> DomainResult<SendReceipt> {
        let mut tx = self.store.begin().await?;
        let original = tx
            .entry_by_client_id(conversation, client_msg_id)
            .await?
            .ok_or_else(|| {
                DomainError::Store("duplicate entry disappeared after a conflict".into())
            })?;
        tx.rollback().await?;
        receipt_for(&original, sender, kind)
    }
}

/// The receipt of a stored entry, if `sender` stored it as an entry of `kind`.
fn receipt_for(original: &Entry, sender: UserId, kind: EntryKind) -> DomainResult<SendReceipt> {
    if original.sender_id != sender || original.kind != kind {
        return Err(DomainError::ClientIdConflict);
    }
    Ok(SendReceipt::from(original))
}
```

`replay_send` is now `replay_append` and also checks the kind; `receipt_for` takes the kind.

- [ ] **Step 7: Write `edit_text`**

`crates/push-domain/src/edit.rs`:

```rust
use chrono::{DateTime, TimeDelta, Utc};

use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{ContentType, Entry, EntryKind, Member, SendReceipt};
use crate::notify::{Event, Notifier};
use crate::send::{Append, Prepared};
use crate::service::{Messaging, current_member_ids};
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::validate_text;

/// The message an edit or delete changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRef {
    /// The id the server assigned.
    Id(MessageId),
    /// The sender's `client_msg_id`, for when the client has not learned the server id yet.
    ClientId(ClientMsgId),
}

/// Replaces the text of a message. `sender` and `device` come from the verified token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditText {
    pub sender: UserId,
    pub device: DeviceId,
    pub conversation: ConversationId,
    /// The edit's own idempotency key, reused on every retry.
    pub client_msg_id: ClientMsgId,
    pub target: MessageRef,
    pub body: Vec<u8>,
    pub client_ts: Option<DateTime<Utc>>,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Replaces the text of one of the sender's own messages (spec §4.6).
    ///
    /// The edit is a log entry of its own. In the same transaction the message row takes
    /// the new text, and the text of any earlier edit entry is erased, so only the current
    /// version is stored. A retry with the same `client_msg_id` returns the original
    /// receipt, before any permission or window check.
    pub async fn edit_text(&self, command: EditText) -> DomainResult<SendReceipt> {
        validate_text(&command.body)?;
        let prepared = self
            .prepare_append(
                command.sender,
                command.device,
                command.conversation,
                command.client_msg_id,
                EntryKind::Edit,
            )
            .await?;
        let Append {
            mut tx,
            conversation,
            members,
            member,
            seq,
            now,
        } = match prepared {
            Prepared::Retry(receipt) => return Ok(receipt),
            Prepared::Ready(append) => append,
        };
        let mut target = resolve_target(&mut tx, &member, command.target).await?;
        check_change(&target, command.sender, now, self.policy.edit_window)?;

        // Erase older edits first, so the only edit text left is the one inserted below.
        tx.erase_edit_bodies(conversation.id, target.seq).await?;
        let entry = Entry {
            conversation_id: conversation.id,
            seq,
            message_id: MessageId::new(),
            client_msg_id: command.client_msg_id,
            kind: EntryKind::Edit,
            target_seq: Some(target.seq),
            sender_id: command.sender,
            sender_device_id: Some(command.device),
            content_type: ContentType::Text,
            body: Some(command.body.clone()),
            client_ts: command.client_ts,
            server_ts: now,
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        };
        let receipt = SendReceipt::from(&entry);
        if tx.insert_entry(entry).await? == InsertOutcome::Duplicate {
            tx.rollback().await?;
            return self
                .replay_append(
                    conversation.id,
                    command.client_msg_id,
                    command.sender,
                    EntryKind::Edit,
                )
                .await;
        }
        target.body = Some(command.body);
        target.edited_at = Some(now);
        target.edit_seq = Some(seq);
        tx.update_entry(target).await?;
        tx.commit().await?;

        self.publish(vec![Event::EntriesAvailable {
            users: current_member_ids(&members),
        }])
        .await;
        Ok(receipt)
    }
}

/// The message `target` refers to, if it is a message the caller may see.
async fn resolve_target<T: Tx>(
    tx: &mut T,
    caller: &Member,
    target: MessageRef,
) -> DomainResult<Entry> {
    let conversation = caller.conversation_id;
    let found = match target {
        MessageRef::Id(id) => tx.entry_by_message_id(conversation, id).await?,
        MessageRef::ClientId(id) => tx.entry_by_client_id(conversation, id).await?,
    };
    found
        .filter(|entry| entry.kind == EntryKind::Message && entry.seq >= caller.visible_from_seq)
        .ok_or(DomainError::TargetNotFound)
}

/// Whether `sender` may still change `target` at `now`.
fn check_change(
    target: &Entry,
    sender: UserId,
    now: DateTime<Utc>,
    window: TimeDelta,
) -> DomainResult<()> {
    if target.sender_id != sender {
        return Err(DomainError::NotSender);
    }
    if target.deleted_at.is_some() {
        return Err(DomainError::AlreadyDeleted);
    }
    if now - target.server_ts > window {
        return Err(DomainError::WindowExpired);
    }
    Ok(())
}
```

In `crates/push-domain/src/lib.rs`, add `mod edit;` after `mod conversations;`, add
`pub use edit::{EditText, MessageRef};` after the `clock` re-export, and replace
`pub use service::Messaging;` with `pub use service::{Messaging, Policy};`.

- [ ] **Step 8: Run the tests to see them pass**

Run: `cargo test -p push-domain --test edit`
Expected: `16 passed`.

Run: `cargo test -p push-domain --test send`
Expected: `14 passed` — sending behaves as before.

- [ ] **Step 9: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 130 tests pass, no warnings, no formatting diff.

- [ ] **Step 10: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): edit messages within a time window"
```

---

### Task 7: Delete messages

**Files:**
- Modify: `crates/push-domain/src/edit.rs` (rewrite)
- Modify: `crates/push-domain/src/lib.rs`
- Modify: `crates/push-domain/tests/common/mod.rs`
- Create: `crates/push-domain/tests/delete.rs`

A delete is the same change as an edit with no new text: the message row gets `deleted_at`
and loses its text, and every edit of it loses its text too (real erasure, spec §4.6). A
retry of the same delete returns its original receipt; a new delete of a deleted message
fails with `AlreadyDeleted`, and so does an edit of one. `edit_text` and `delete_message`
now share `change_message`.

- [ ] **Step 1: Add the harness helper**

In `crates/push-domain/tests/common/mod.rs`, add `DeleteMessage` to the `use push_domain::{…}`
list and add this free function before `edit`:

```rust
/// A delete command with a fresh client message id.
pub fn delete(from: &Person, conversation: ConversationId, target: MessageRef) -> DeleteMessage {
    DeleteMessage {
        sender: from.user,
        device: from.device,
        conversation,
        client_msg_id: ClientMsgId::new(),
        target,
    }
}
```

- [ ] **Step 2: Write the failing tests**

`crates/push-domain/tests/delete.rs`:

```rust
mod common;

use chrono::TimeDelta;
use common::{Person, World, delete, edit, entries_for, seqs};
use push_domain::{
    Clock, ConversationId, DomainError, Entry, EntryKind, MessageRef, Policy, SendReceipt,
};

/// Alice and bob in a direct chat, with one message "hello" from alice at seq 1.
async fn chat_with_one_message() -> (World, Person, Person, ConversationId, SendReceipt) {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    (world, alice, bob, chat, hello)
}

fn by_id(target: &SendReceipt) -> MessageRef {
    MessageRef::Id(target.message_id)
}

async fn synced(world: &World, person: &Person) -> Vec<Entry> {
    world.svc.sync(person.device, 100).await.unwrap().entries
}

#[tokio::test]
async fn a_delete_erases_the_message_and_every_edit_of_it() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.edit(&alice, chat, &hello, "v1").await;
    world.edit(&alice, chat, &hello, "v2").await;
    world.clock.advance(TimeDelta::minutes(1));

    let receipt = world
        .svc
        .delete_message(delete(&alice, chat, by_id(&hello)))
        .await
        .unwrap();
    assert_eq!(receipt.seq, 4);

    let entries = synced(&world, &bob).await;
    assert_eq!(seqs(&entries), [1, 2, 3, 4]);
    assert!(entries.iter().all(|entry| entry.body.is_none()));
    assert_eq!(entries[0].deleted_at, Some(world.clock.now()));
    assert_eq!(entries[3].kind, EntryKind::Delete);
    assert_eq!(entries[3].target_seq, Some(1));
}

#[tokio::test]
async fn a_delete_retry_returns_the_original_and_changes_nothing() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    let command = delete(&alice, chat, by_id(&hello));
    let first = world.svc.delete_message(command.clone()).await.unwrap();
    world.notifier.take();

    assert_eq!(world.svc.delete_message(command).await, Ok(first));
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.send(&alice, chat, "next").await.seq, 3);
}

#[tokio::test]
async fn a_second_delete_of_the_same_message_is_refused() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    world
        .svc
        .delete_message(delete(&alice, chat, by_id(&hello)))
        .await
        .unwrap();

    assert_eq!(
        world
            .svc
            .delete_message(delete(&alice, chat, by_id(&hello)))
            .await,
        Err(DomainError::AlreadyDeleted)
    );
}

#[tokio::test]
async fn a_deleted_message_cannot_be_edited() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    world
        .svc
        .delete_message(delete(&alice, chat, by_id(&hello)))
        .await
        .unwrap();

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "back"))
            .await,
        Err(DomainError::AlreadyDeleted)
    );
}

#[tokio::test]
async fn only_the_sender_can_delete() {
    let (world, _alice, bob, chat, hello) = chat_with_one_message().await;
    assert_eq!(
        world
            .svc
            .delete_message(delete(&bob, chat, by_id(&hello)))
            .await,
        Err(DomainError::NotSender)
    );
}

#[tokio::test]
async fn a_delete_is_allowed_until_the_window_closes() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    let second = world.send(&alice, chat, "second").await;
    world.clock.advance(TimeDelta::hours(48));
    world
        .svc
        .delete_message(delete(&alice, chat, by_id(&hello)))
        .await
        .unwrap();

    world.clock.advance(TimeDelta::seconds(1));
    assert_eq!(
        world
            .svc
            .delete_message(delete(&alice, chat, by_id(&second)))
            .await,
        Err(DomainError::WindowExpired)
    );
}

#[tokio::test]
async fn the_delete_window_is_separate_from_the_edit_window() {
    let world = World::with_policy(Policy {
        edit_window: TimeDelta::minutes(1),
        ..Policy::default()
    });
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    world.clock.advance(TimeDelta::minutes(2));

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "late"))
            .await,
        Err(DomainError::WindowExpired)
    );
    assert!(
        world
            .svc
            .delete_message(delete(&alice, chat, by_id(&hello)))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_delete_tells_every_member_to_sync() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.notifier.take();

    world
        .svc
        .delete_message(delete(&alice, chat, by_id(&hello)))
        .await
        .unwrap();
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &bob])]);
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p push-domain --test delete`
Expected: compile error — `unresolved import push_domain::DeleteMessage`.

- [ ] **Step 4: Share the change path between edit and delete**

Replace the whole of `crates/push-domain/src/edit.rs` with:

```rust
use chrono::{DateTime, TimeDelta, Utc};

use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{ContentType, Entry, EntryKind, Member, SendReceipt};
use crate::notify::{Event, Notifier};
use crate::send::{Append, Prepared};
use crate::service::{Messaging, current_member_ids};
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::validate_text;

/// The message an edit or delete changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRef {
    /// The id the server assigned.
    Id(MessageId),
    /// The sender's `client_msg_id`, for when the client has not learned the server id yet.
    ClientId(ClientMsgId),
}

/// Replaces the text of a message. `sender` and `device` come from the verified token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditText {
    pub sender: UserId,
    pub device: DeviceId,
    pub conversation: ConversationId,
    /// The edit's own idempotency key, reused on every retry.
    pub client_msg_id: ClientMsgId,
    pub target: MessageRef,
    pub body: Vec<u8>,
    pub client_ts: Option<DateTime<Utc>>,
}

/// Erases a message. `sender` and `device` come from the verified token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteMessage {
    pub sender: UserId,
    pub device: DeviceId,
    pub conversation: ConversationId,
    /// The delete's own idempotency key, reused on every retry.
    pub client_msg_id: ClientMsgId,
    pub target: MessageRef,
}

/// An edit or a delete, after input validation.
struct Change {
    sender: UserId,
    device: DeviceId,
    conversation: ConversationId,
    client_msg_id: ClientMsgId,
    target: MessageRef,
    action: Action,
}

enum Action {
    Edit {
        body: Vec<u8>,
        client_ts: Option<DateTime<Utc>>,
    },
    Delete,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Replaces the text of one of the sender's own messages (spec §4.6).
    ///
    /// The edit is a log entry of its own. In the same transaction the message row takes
    /// the new text, and the text of any earlier edit entry is erased, so only the current
    /// version is stored. A retry with the same `client_msg_id` returns the original
    /// receipt, before any permission or window check.
    pub async fn edit_text(&self, command: EditText) -> DomainResult<SendReceipt> {
        validate_text(&command.body)?;
        self.change_message(Change {
            sender: command.sender,
            device: command.device,
            conversation: command.conversation,
            client_msg_id: command.client_msg_id,
            target: command.target,
            action: Action::Edit {
                body: command.body,
                client_ts: command.client_ts,
            },
        })
        .await
    }

    /// Erases one of the sender's own messages (spec §4.6).
    ///
    /// The delete is a log entry of its own. In the same transaction the message row is
    /// marked deleted and its text, and the text of every edit entry targeting it, is
    /// erased. A retry with the same `client_msg_id` returns the original receipt; a new
    /// delete of a deleted message fails with `AlreadyDeleted`.
    pub async fn delete_message(&self, command: DeleteMessage) -> DomainResult<SendReceipt> {
        self.change_message(Change {
            sender: command.sender,
            device: command.device,
            conversation: command.conversation,
            client_msg_id: command.client_msg_id,
            target: command.target,
            action: Action::Delete,
        })
        .await
    }

    async fn change_message(&self, change: Change) -> DomainResult<SendReceipt> {
        let (kind, window, body, client_ts) = match change.action {
            Action::Edit { body, client_ts } => (
                EntryKind::Edit,
                self.policy.edit_window,
                Some(body),
                client_ts,
            ),
            Action::Delete => (EntryKind::Delete, self.policy.delete_window, None, None),
        };
        let prepared = self
            .prepare_append(
                change.sender,
                change.device,
                change.conversation,
                change.client_msg_id,
                kind,
            )
            .await?;
        let Append {
            mut tx,
            conversation,
            members,
            member,
            seq,
            now,
        } = match prepared {
            Prepared::Retry(receipt) => return Ok(receipt),
            Prepared::Ready(append) => append,
        };
        let mut target = resolve_target(&mut tx, &member, change.target).await?;
        check_change(&target, change.sender, now, window)?;

        // Erase older edits first, so the only edit text left is the one inserted below.
        tx.erase_edit_bodies(conversation.id, target.seq).await?;
        let entry = Entry {
            conversation_id: conversation.id,
            seq,
            message_id: MessageId::new(),
            client_msg_id: change.client_msg_id,
            kind,
            target_seq: Some(target.seq),
            sender_id: change.sender,
            sender_device_id: Some(change.device),
            content_type: ContentType::Text,
            body: body.clone(),
            client_ts,
            server_ts: now,
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        };
        let receipt = SendReceipt::from(&entry);
        if tx.insert_entry(entry).await? == InsertOutcome::Duplicate {
            tx.rollback().await?;
            return self
                .replay_append(conversation.id, change.client_msg_id, change.sender, kind)
                .await;
        }

        // An edit stores the new text; a delete stores none.
        target.body = body;
        if kind == EntryKind::Edit {
            target.edited_at = Some(now);
            target.edit_seq = Some(seq);
        } else {
            target.deleted_at = Some(now);
        }
        tx.update_entry(target).await?;
        tx.commit().await?;

        self.publish(vec![Event::EntriesAvailable {
            users: current_member_ids(&members),
        }])
        .await;
        Ok(receipt)
    }
}

/// The message `target` refers to, if it is a message the caller may see.
async fn resolve_target<T: Tx>(
    tx: &mut T,
    caller: &Member,
    target: MessageRef,
) -> DomainResult<Entry> {
    let conversation = caller.conversation_id;
    let found = match target {
        MessageRef::Id(id) => tx.entry_by_message_id(conversation, id).await?,
        MessageRef::ClientId(id) => tx.entry_by_client_id(conversation, id).await?,
    };
    found
        .filter(|entry| entry.kind == EntryKind::Message && entry.seq >= caller.visible_from_seq)
        .ok_or(DomainError::TargetNotFound)
}

/// Whether `sender` may still change `target` at `now`.
fn check_change(
    target: &Entry,
    sender: UserId,
    now: DateTime<Utc>,
    window: TimeDelta,
) -> DomainResult<()> {
    if target.sender_id != sender {
        return Err(DomainError::NotSender);
    }
    if target.deleted_at.is_some() {
        return Err(DomainError::AlreadyDeleted);
    }
    if now - target.server_ts > window {
        return Err(DomainError::WindowExpired);
    }
    Ok(())
}
```

In `crates/push-domain/src/lib.rs`, replace the `edit` re-export with:

```rust
pub use edit::{DeleteMessage, EditText, MessageRef};
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p push-domain --test delete --test edit`
Expected: `8 passed` and `16 passed`.

- [ ] **Step 6: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 138 tests pass, no warnings, no formatting diff.

- [ ] **Step 7: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): delete messages with real erasure"
```

---

### Task 8: Never deliver changes to messages outside the visible range

**Files:**
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/memory.rs`
- Modify: `crates/push-domain/src/sync.rs`
- Modify: `crates/push-domain/tests/visibility.rs`

Spec §4.6: a member never receives an edit or delete whose `target_seq` is outside their
visible range. Groups make this real in Plan 2B; the harness's `set_visibility` tests it now.
The store filters (decision 6), so a page is full whenever enough visible entries exist;
sync asks for one entry beyond its room, so `more` is exact even when hidden entries end a
conversation.

- [ ] **Step 1: Write the failing unit test**

In `crates/push-domain/src/memory.rs`, append inside `mod tests`:

```rust
    #[tokio::test]
    async fn entries_after_skips_changes_to_messages_below_targets_from() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        for seq in 1..=3 {
            tx.insert_entry(entry(id, seq, ClientMsgId::new()))
                .await
                .unwrap();
        }
        for (seq, target) in [(4, 1), (5, 3), (6, 2)] {
            tx.insert_entry(Entry {
                kind: EntryKind::Edit,
                target_seq: Some(target),
                ..entry(id, seq, ClientMsgId::new())
            })
            .await
            .unwrap();
        }

        assert_eq!(
            seqs(&tx.entries_after(id, 2, 6, 3, 10).await.unwrap()),
            vec![3, 5]
        );
        assert_eq!(
            seqs(&tx.entries_after(id, 0, 6, 2, 3).await.unwrap()),
            vec![1, 2, 3],
            "the limit counts only returned entries"
        );
        assert_eq!(
            seqs(&tx.entries_after(id, 3, 6, 2, 2).await.unwrap()),
            vec![5, 6]
        );
    }
```

Every existing `entries_after` call in `mod tests` gains a `targets_from` argument of `0`
before the limit. Replace the test `entry_ranges_respect_their_bounds` with:

```rust
    #[tokio::test]
    async fn entry_ranges_respect_their_bounds() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let conversation = id;
        for seq in 1..=5 {
            tx.insert_entry(entry(conversation, seq, ClientMsgId::new()))
                .await
                .unwrap();
        }

        assert_eq!(
            seqs(&tx.entries_after(conversation, 1, 4, 0, 10).await.unwrap()),
            vec![2, 3, 4]
        );
        assert_eq!(
            seqs(&tx.entries_after(conversation, 1, 5, 0, 2).await.unwrap()),
            vec![2, 3]
        );
        assert_eq!(
            seqs(&tx.entries_after(conversation, 5, 5, 0, 10).await.unwrap()),
            Vec::<i64>::new()
        );
        assert_eq!(
            seqs(&tx.entries_before(conversation, 1, 5, 2).await.unwrap()),
            vec![3, 4]
        );
        assert_eq!(
            seqs(&tx.entries_before(conversation, 2, 3, 10).await.unwrap()),
            vec![2]
        );
        assert_eq!(
            seqs(&tx.entries_before(conversation, 1, 1, 10).await.unwrap()),
            Vec::<i64>::new()
        );
    }
```

and in the tests `a_message_id_cannot_be_inserted_twice` and
`erasing_edit_bodies_touches_only_edits_of_that_target` replace
`entries_after(id, 0, 5, 10)` with `entries_after(id, 0, 5, 0, 10)`.

- [ ] **Step 2: Write the failing integration tests**

In `crates/push-domain/tests/visibility.rs`, replace the `use` lines with:

```rust
use common::{Person, World, bodies, delete, seqs};
use push_domain::{ConversationId, DomainError, MessageRef};
```

and append:

```rust
#[tokio::test]
async fn changes_to_messages_below_the_visible_range_are_not_delivered() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let first = world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;
    let third = world.send(&alice, chat, "three").await;
    world.set_visibility(chat, bob.user, 3, None).await;

    world.edit(&alice, chat, &first, "one, edited").await;
    world.edit(&alice, chat, &third, "three, edited").await;
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [3, 5]);
    assert_eq!(bodies(&batch.entries), ["three, edited", "three, edited"]);
    assert!(!batch.more);
}

#[tokio::test]
async fn a_hidden_change_at_the_end_does_not_report_more() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let first = world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;
    world.set_visibility(chat, bob.user, 2, None).await;
    world
        .svc
        .delete_message(delete(&alice, chat, MessageRef::Id(first.message_id)))
        .await
        .unwrap();

    let batch = world.svc.sync(bob.device, 1).await.unwrap();
    assert_eq!(seqs(&batch.entries), [2]);
    assert!(!batch.more);
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p push-domain --lib memory`
Expected: compile error — `this method takes 4 arguments but 5 arguments were supplied`.

- [ ] **Step 4: Add `targets_from` to the contract and the in-memory store**

In `crates/push-domain/src/store.rs`, replace the `entries_after` declaration with:

```rust
    /// Entries with `after < seq <= up_to`, oldest first, at most `limit`. Entries with a
    /// `target_seq` below `targets_from` (edits and deletes of messages outside the
    /// reader's visible range) are skipped and do not count towards `limit`.
    async fn entries_after(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        targets_from: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>>;
```

In `crates/push-domain/src/memory.rs`, replace `entries_after` with:

```rust
    async fn entries_after(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        targets_from: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        if after >= up_to {
            return Ok(Vec::new());
        }
        Ok(self
            .work
            .entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .map(|(_, entry)| entry)
            .filter(|entry| entry.target_seq.is_none_or(|target| target >= targets_from))
            .take(limit as usize)
            .cloned()
            .collect())
    }
```

- [ ] **Step 5: Filter in sync and make `more` exact**

Replace `sync` in `crates/push-domain/src/sync.rs` (keep `ack` unchanged) with:

```rust
    /// Everything after the device's delivery positions, within each membership's
    /// visible range, up to `limit` entries.
    ///
    /// Edits and deletes of messages below the member's visible range are never delivered
    /// (spec §4.6). If such entries end a conversation, the device's position stays before
    /// them until it acks a later entry; nothing is lost, as they are never delivered.
    ///
    /// Sync does not advance anything: the position moves only when the device acks.
    /// Calling sync again without acking returns the same entries.
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
            // One entry beyond the room tells whether more is waiting.
            let mut page = tx
                .entries_after(
                    conversation.id,
                    position,
                    upper,
                    member.visible_from_seq,
                    room as u32 + 1,
                )
                .await?;
            if page.len() > room {
                page.truncate(room);
                entries.extend(page);
                more = true;
                break;
            }
            entries.extend(page);
        }
        tx.rollback().await?;
        Ok(SyncBatch { entries, more })
    }
```

- [ ] **Step 6: Run the tests to see them pass**

Run: `cargo test -p push-domain --lib memory`
Expected: `20 passed`.

Run: `cargo test -p push-domain --test visibility --test sync`
Expected: `7 passed` and `15 passed`.

- [ ] **Step 7: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 141 tests pass, no warnings, no formatting diff.

- [ ] **Step 8: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): never deliver changes to messages outside the visible range"
```

---

### Task 9: History pages hold messages only

**Files:**
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/memory.rs`
- Modify: `crates/push-domain/src/history.rs` (rewrite)
- Modify: `crates/push-domain/src/lib.rs`
- Modify: `crates/push-domain/tests/history.rs` (rewrite)
- Modify: `crates/push-domain/tests/sync.rs`
- Modify: `crates/push-domain/tests/visibility.rs`

Spec §4.6: history pages return only messages and system entries, in their current state.
The filter is in the store query (decision 7), so a page of `limit` is full whenever enough
messages exist. `history` now returns `HistoryPage { entries, has_more }`; it asks for one
entry more than the page to know whether older ones exist.

- [ ] **Step 1: Write the failing unit test**

In `crates/push-domain/src/memory.rs`, inside `mod tests`, replace the test
`entry_ranges_respect_their_bounds` with (its last three assertions now call
`messages_before`):

```rust
    #[tokio::test]
    async fn entry_ranges_respect_their_bounds() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let conversation = id;
        for seq in 1..=5 {
            tx.insert_entry(entry(conversation, seq, ClientMsgId::new()))
                .await
                .unwrap();
        }

        assert_eq!(
            seqs(&tx.entries_after(conversation, 1, 4, 0, 10).await.unwrap()),
            vec![2, 3, 4]
        );
        assert_eq!(
            seqs(&tx.entries_after(conversation, 1, 5, 0, 2).await.unwrap()),
            vec![2, 3]
        );
        assert_eq!(
            seqs(&tx.entries_after(conversation, 5, 5, 0, 10).await.unwrap()),
            Vec::<i64>::new()
        );
        assert_eq!(
            seqs(&tx.messages_before(conversation, 1, 5, 2).await.unwrap()),
            vec![3, 4]
        );
        assert_eq!(
            seqs(&tx.messages_before(conversation, 2, 3, 10).await.unwrap()),
            vec![2]
        );
        assert_eq!(
            seqs(&tx.messages_before(conversation, 1, 1, 10).await.unwrap()),
            Vec::<i64>::new()
        );
    }
```

and append:

```rust
    #[tokio::test]
    async fn messages_before_skips_edits_and_deletes() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let kinds = [
            EntryKind::Message,
            EntryKind::Edit,
            EntryKind::System,
            EntryKind::Delete,
            EntryKind::Message,
        ];
        for (seq, kind) in (1..).zip(kinds) {
            tx.insert_entry(Entry {
                kind,
                ..entry(id, seq, ClientMsgId::new())
            })
            .await
            .unwrap();
        }

        assert_eq!(
            seqs(&tx.messages_before(id, 1, 6, 2).await.unwrap()),
            vec![3, 5]
        );
        assert_eq!(
            seqs(&tx.messages_before(id, 1, 6, 10).await.unwrap()),
            vec![1, 3, 5]
        );
    }
```

- [ ] **Step 2: Write the failing integration tests**

Replace the whole of `crates/push-domain/tests/history.rs` with:

```rust
mod common;

use common::{Person, World, bodies, delete, seqs};
use push_domain::{ConversationId, DomainError, MAX_HISTORY_PAGE, MessageRef};

async fn chat_with_five_messages() -> (World, Person, Person, ConversationId) {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["1", "2", "3", "4", "5"] {
        world.send(&alice, chat, body).await;
    }
    (world, alice, bob, chat)
}

#[tokio::test]
async fn the_first_page_is_the_newest_messages_oldest_first() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    let page = world.svc.history(bob.user, chat, None, 2).await.unwrap();
    assert_eq!(seqs(&page.entries), [4, 5]);
    assert!(page.has_more);
}

#[tokio::test]
async fn pages_go_backwards_from_before_seq() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    let history = |before| world.svc.history(bob.user, chat, Some(before), 2);

    let page = history(4).await.unwrap();
    assert_eq!(seqs(&page.entries), [2, 3]);
    assert!(page.has_more);
    let page = history(2).await.unwrap();
    assert_eq!(seqs(&page.entries), [1]);
    assert!(!page.has_more);
    assert!(history(1).await.unwrap().entries.is_empty());
}

#[tokio::test]
async fn a_page_that_reaches_the_start_has_no_more() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    let page = world.svc.history(bob.user, chat, Some(3), 2).await.unwrap();
    assert_eq!(seqs(&page.entries), [1, 2]);
    assert!(!page.has_more);
}

#[tokio::test]
async fn the_page_size_is_at_least_one() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    let page = world.svc.history(bob.user, chat, None, 0).await.unwrap();
    assert_eq!(seqs(&page.entries), [5]);
}

#[tokio::test]
async fn only_members_can_read_history() {
    let (world, _alice, _bob, chat) = chat_with_five_messages().await;
    let carol = world.person().await;
    assert_eq!(
        world.svc.history(carol.user, chat, None, 10).await,
        Err(DomainError::NotMember)
    );
}

#[tokio::test]
async fn a_before_seq_beyond_the_end_returns_the_newest_page() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    let page = world
        .svc
        .history(bob.user, chat, Some(99), 2)
        .await
        .unwrap();
    assert_eq!(seqs(&page.entries), [4, 5]);
}

#[tokio::test]
async fn a_before_seq_at_or_below_one_returns_nothing() {
    let (world, _alice, bob, chat) = chat_with_five_messages().await;
    for before in [1, 0, -3] {
        let page = world
            .svc
            .history(bob.user, chat, Some(before), 10)
            .await
            .unwrap();
        assert!(page.entries.is_empty());
        assert!(!page.has_more);
    }
}

#[tokio::test]
async fn the_page_size_is_capped() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for _ in 0..=MAX_HISTORY_PAGE {
        world.send(&alice, chat, "x").await;
    }

    let page = world.svc.history(bob.user, chat, None, 1000).await.unwrap();
    assert_eq!(page.entries.len(), MAX_HISTORY_PAGE as usize);
    assert!(page.has_more);
}

#[tokio::test]
async fn history_holds_messages_in_their_current_state_and_no_changes() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let first = world.send(&alice, chat, "one").await;
    let second = world.send(&alice, chat, "two").await;
    world.edit(&alice, chat, &first, "one, edited").await;
    world
        .svc
        .delete_message(delete(&alice, chat, MessageRef::Id(second.message_id)))
        .await
        .unwrap();

    let page = world.svc.history(bob.user, chat, None, 10).await.unwrap();
    assert_eq!(seqs(&page.entries), [1, 2]);
    assert_eq!(bodies(&page.entries), ["one, edited", ""]);
    assert!(page.entries[1].deleted_at.is_some());
}

#[tokio::test]
async fn edits_do_not_take_room_in_a_page() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let first = world.send(&alice, chat, "one").await;
    let second = world.send(&alice, chat, "two").await;
    for body in ["a", "b", "c"] {
        world.edit(&alice, chat, &second, body).await;
    }

    let page = world.svc.history(bob.user, chat, None, 2).await.unwrap();
    assert_eq!(seqs(&page.entries), [first.seq, second.seq]);
    assert!(!page.has_more);
}
```

In `crates/push-domain/tests/sync.rs` (test
`a_new_device_gets_nothing_from_a_conversation_already_left`) and in
`crates/push-domain/tests/visibility.rs` (tests `sync_and_history_stop_at_left_at` and
`history_does_not_go_below_visible_from`), replace `seqs(&history)` with
`seqs(&history.entries)`.

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p push-domain`
Expected: compile errors — `no method named messages_before found` and
`no field entries on type Vec<Entry>`.

- [ ] **Step 4: Replace `entries_before` with `messages_before`**

In `crates/push-domain/src/store.rs`, replace the `entries_before` declaration with:

```rust
    /// The newest `limit` entries of kind `Message` or `System` with `from <= seq < before`,
    /// returned oldest first. Edit and delete entries are skipped and do not count towards
    /// `limit`.
    async fn messages_before(
        &mut self,
        conversation: ConversationId,
        from: i64,
        before: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>>;
```

In `crates/push-domain/src/memory.rs`, replace `entries_before` with:

```rust
    async fn messages_before(
        &mut self,
        conversation: ConversationId,
        from: i64,
        before: i64,
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
            .map(|(_, entry)| entry)
            .filter(|entry| matches!(entry.kind, EntryKind::Message | EntryKind::System))
            .take(limit as usize)
            .cloned()
            .collect();
        page.reverse();
        Ok(page)
    }
```

- [ ] **Step 5: Return a `HistoryPage`**

Replace the whole of `crates/push-domain/src/history.rs` with:

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

/// One page of history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    /// Messages and system entries in their current state, oldest first. Never edit or
    /// delete entries: their effect is already applied to the messages.
    pub entries: Vec<Entry>,
    /// Older entries visible to the caller exist before this page.
    pub has_more: bool,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Up to `limit` messages before `before_seq` (or the newest ones), limited to what
    /// `user` may see. `before_seq` is exclusive.
    pub async fn history(
        &self,
        user: UserId,
        conversation: ConversationId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> DomainResult<HistoryPage> {
        let limit = limit.clamp(1, MAX_HISTORY_PAGE);
        let mut tx = self.store.begin().await?;
        let found = require_conversation(&mut tx, conversation).await?;
        let member = require_member(&mut tx, conversation, user).await?;

        let end = member.visible_upper(found.last_seq) + 1;
        let before = before_seq.map_or(end, |seq| seq.min(end));
        // One entry beyond the page tells whether older ones exist.
        let mut entries = tx
            .messages_before(conversation, member.visible_from_seq, before, limit + 1)
            .await?;
        tx.rollback().await?;
        let has_more = entries.len() > limit as usize;
        if has_more {
            entries.remove(0);
        }
        Ok(HistoryPage { entries, has_more })
    }
}
```

In `crates/push-domain/src/lib.rs`, add after the `error` re-export:

```rust
pub use history::HistoryPage;
```

- [ ] **Step 6: Run the tests to see them pass**

Run: `cargo test -p push-domain --test history`
Expected: `10 passed`.

- [ ] **Step 7: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 145 tests pass, no warnings, no formatting diff.

- [ ] **Step 8: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): history pages hold messages only and report has_more"
```

---

### Task 10: Delivery and read receipts

**Files:**
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/memory.rs`
- Modify: `crates/push-domain/src/read.rs` (rewrite)
- Modify: `crates/push-domain/src/sync.rs`
- Modify: `crates/push-domain/src/send.rs`
- Create: `crates/push-domain/tests/receipts.rs`

Spec §4.5 for direct conversations: when a member's delivered or read position changes,
the other member gets `Receipt{conversation, user_id, delivered_seq, read_seq}`. Only a
move that passes over at least one live message from someone else counts — not one over
edits, deletes, system entries, deleted messages or one's own messages. Delivered is the
furthest position across the user's devices (`Tx::delivered_seq`), so an ack that moves one
device but not the user's maximum sends nothing. A message read is a message delivered, so
every receipt reports `delivered_seq` as at least `read_seq` (decision 15). Three paths
move a position: `ack` (delivered), `read_up_to` (read) and `send_text` (the sender's
read, decision 2). All use one helper, `direct_receipt`, built on
`count_incoming_messages` (decision 10). `ack` counts from the member's floor
(`visible_from_seq - 1`) even if a delivered position is lower, which matters once group
joins exist (Plan 2B).

- [ ] **Step 1: Write the failing unit test**

In `crates/push-domain/src/memory.rs`, append inside `mod tests`:

```rust
    #[tokio::test]
    async fn only_live_messages_from_others_are_counted_up_to_the_cap() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let me = UserId::new();
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let mut entries = vec![
            entry(id, 1, ClientMsgId::new()),
            Entry {
                sender_id: me,
                ..entry(id, 2, ClientMsgId::new())
            },
            Entry {
                deleted_at: Some(start()),
                ..entry(id, 3, ClientMsgId::new())
            },
            Entry {
                kind: EntryKind::Edit,
                ..entry(id, 4, ClientMsgId::new())
            },
        ];
        entries.extend((5..=7).map(|seq| entry(id, seq, ClientMsgId::new())));
        for entry in entries {
            tx.insert_entry(entry).await.unwrap();
        }

        assert_eq!(tx.count_incoming_messages(id, me, 0, 7, 100).await, Ok(4));
        assert_eq!(tx.count_incoming_messages(id, me, 1, 4, 100).await, Ok(0));
        assert_eq!(tx.count_incoming_messages(id, me, 0, 7, 2).await, Ok(2));
        assert_eq!(tx.count_incoming_messages(id, me, 7, 7, 100).await, Ok(0));
    }
```

- [ ] **Step 2: Write the failing integration tests**

`crates/push-domain/tests/receipts.rs`:

```rust
mod common;

use common::{Person, World, delete, entries_for};
use push_domain::{ConversationId, Event, MessageRef, ReceiptUpdate};

fn receipt(
    conversation: ConversationId,
    to: &Person,
    from: &Person,
    delivered_seq: i64,
    read_seq: i64,
) -> Event {
    Event::Receipt {
        conversation,
        to: vec![to.user],
        receipt: ReceiptUpdate::Direct {
            user_id: from.user,
            delivered_seq,
            read_seq,
        },
    }
}

#[tokio::test]
async fn an_ack_over_a_message_from_the_peer_sends_a_delivery_receipt() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.notifier.take();

    world.svc.ack(bob.device, chat, 1).await.unwrap();
    assert_eq!(world.notifier.take(), [receipt(chat, &alice, &bob, 1, 0)]);
}

#[tokio::test]
async fn an_ack_that_does_not_move_the_users_delivered_position_sends_nothing() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let tablet = world.add_device(&bob).await;
    world.send(&alice, chat, "one").await;
    world.svc.ack(bob.device, chat, 1).await.unwrap();
    world.notifier.take();

    assert_eq!(world.svc.ack(tablet, chat, 1).await, Ok(true));
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn an_ack_over_only_ones_own_messages_sends_nothing() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.notifier.take();

    world.svc.ack(alice.device, chat, 1).await.unwrap();
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn an_ack_over_only_changes_or_deleted_messages_sends_nothing() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    world.svc.ack(bob.device, chat, 1).await.unwrap();
    world.edit(&alice, chat, &hello, "hello!").await;
    let oops = world.send(&alice, chat, "oops").await;
    world
        .svc
        .delete_message(delete(&alice, chat, MessageRef::Id(oops.message_id)))
        .await
        .unwrap();
    world.notifier.take();

    assert_eq!(world.svc.ack(bob.device, chat, 4).await, Ok(true));
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn reading_a_message_from_the_peer_sends_a_read_receipt_once() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.svc.ack(bob.device, chat, 1).await.unwrap();
    world.notifier.take();

    world.svc.read_up_to(bob.user, chat, 1).await.unwrap();
    assert_eq!(world.notifier.take(), [receipt(chat, &alice, &bob, 1, 1)]);

    world.svc.read_up_to(bob.user, chat, 1).await.unwrap();
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn reading_past_an_edit_sends_nothing() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    world.svc.read_up_to(bob.user, chat, 1).await.unwrap();
    world.edit(&alice, chat, &hello, "hello!").await;
    world.notifier.take();

    assert_eq!(world.svc.read_up_to(bob.user, chat, 2).await, Ok(true));
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn replying_reports_that_the_peers_messages_were_read_and_so_delivered() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&bob, chat, "hi").await;
    world.notifier.take();

    world.send(&alice, chat, "hello").await;
    assert_eq!(
        world.notifier.take(),
        [
            entries_for(&[&alice, &bob]),
            receipt(chat, &bob, &alice, 2, 2)
        ]
    );
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p push-domain --lib memory`
Expected: compile error — `no method named count_incoming_messages found`.

Run: `cargo test -p push-domain --test receipts`
Expected: the 3 tests that expect a receipt fail (`left: []`, or only the
`EntriesAvailable` event for
`replying_reports_that_the_peers_messages_were_read_and_so_delivered`); the 4 tests that
expect no event pass.

- [ ] **Step 4: Add the counting method**

In `crates/push-domain/src/store.rs`, add before `messages_before`:

```rust
    /// Counts entries with `after < seq <= up_to` that are messages (`kind = Message`), not
    /// deleted, and sent by someone other than `user`. Stops counting at `cap`, so the
    /// result is at most `cap`.
    async fn count_incoming_messages(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        after: i64,
        up_to: i64,
        cap: u32,
    ) -> DomainResult<u32>;
```

In `crates/push-domain/src/memory.rs`, add before `messages_before`:

```rust
    async fn count_incoming_messages(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        after: i64,
        up_to: i64,
        cap: u32,
    ) -> DomainResult<u32> {
        if after >= up_to {
            return Ok(0);
        }
        let count = self
            .work
            .entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .map(|(_, entry)| entry)
            .filter(|entry| {
                entry.kind == EntryKind::Message
                    && entry.deleted_at.is_none()
                    && entry.sender_id != user
            })
            .take(cap as usize)
            .count();
        Ok(count as u32)
    }
```

- [ ] **Step 5: Add `direct_receipt` and publish read receipts**

Replace the whole of `crates/push-domain/src/read.rs` with:

```rust
use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{Conversation, ConversationKind, ConversationState, MemberState};
use crate::notify::{Event, Notifier, ReceiptUpdate};
use crate::service::{Messaging, current_member_ids};
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
        let receipt = direct_receipt(&mut tx, &found, user, member.read_seq, seq).await?;
        tx.commit().await?;
        self.publish(receipt.into_iter().collect()).await;
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
            let delivered_seq = tx.delivered_seq(conversation, member.user_id).await?;
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

/// The receipt for the other members of a direct conversation after one of `user`'s
/// positions moved from `before` to `after` (spec §4.5). `None` unless the conversation is
/// direct and the move passed over at least one live message that someone else sent:
/// moving over edits, deletes, system entries or one's own messages tells nobody anything.
///
/// Reads both positions as this transaction sees them, so call it after the move. Reports
/// `delivered_seq` as at least `read_seq`, so read always implies delivered.
pub(crate) async fn direct_receipt<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    user: UserId,
    before: i64,
    after: i64,
) -> DomainResult<Option<Event>> {
    if conversation.kind != ConversationKind::Direct || after <= before {
        return Ok(None);
    }
    if tx
        .count_incoming_messages(conversation.id, user, before, after, 1)
        .await?
        == 0
    {
        return Ok(None);
    }
    let members = tx.members(conversation.id).await?;
    let read_seq = members
        .iter()
        .find(|member| member.user_id == user)
        .map(|member| member.read_seq)
        .ok_or(DomainError::NotMember)?;
    let to: Vec<UserId> = current_member_ids(&members)
        .into_iter()
        .filter(|member| *member != user)
        .collect();
    if to.is_empty() {
        return Ok(None);
    }
    let delivered_seq = tx.delivered_seq(conversation.id, user).await?;
    Ok(Some(Event::Receipt {
        conversation: conversation.id,
        to,
        receipt: ReceiptUpdate::Direct {
            user_id: user,
            delivered_seq: delivered_seq.max(read_seq),
            read_seq,
        },
    }))
}
```

- [ ] **Step 6: Publish delivery receipts on ack**

In `crates/push-domain/src/sync.rs`, add `use crate::read::direct_receipt;` after
`use crate::notify::Notifier;`, and replace `ack` with:

```rust
    /// Records that the device holds everything up to `seq`. Returns whether the
    /// position moved; it never moves backwards. If the user's delivered position moved
    /// over a message from the other member of a direct conversation, they get a receipt.
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
        if seq < member.visible_from_seq - 1 {
            return Ok(false);
        }
        // Count only from the member's floor; matters once group joins exist (Plan 2B).
        let delivered_before = tx
            .delivered_seq(conversation, user)
            .await?
            .max(member.visible_from_seq - 1);
        let moved = tx.raise_cursor(device, conversation, seq).await?;
        let receipt = direct_receipt(
            &mut tx,
            &found,
            user,
            delivered_before,
            delivered_before.max(seq),
        )
        .await?;
        tx.commit().await?;
        self.publish(receipt.into_iter().collect()).await;
        Ok(moved)
    }
```

- [ ] **Step 7: Publish the sender's read receipt on send**

In `crates/push-domain/src/send.rs`, add `use crate::read::direct_receipt;` after
`use crate::notify::{Event, Notifier};`, and replace `send_text` with:

```rust
    /// Appends a text message. A retry with the same `client_msg_id` returns the original
    /// receipt, stores nothing new and publishes nothing — even if the sender has since lost
    /// the right to send.
    ///
    /// Sending marks the conversation read for the sender; if that passes over a message
    /// from the other member of a direct conversation, they get a read receipt.
    pub async fn send_text(&self, command: SendText) -> DomainResult<SendReceipt> {
        validate_text(&command.body)?;
        let prepared = self
            .prepare_append(
                command.sender,
                command.device,
                command.conversation,
                command.client_msg_id,
                EntryKind::Message,
            )
            .await?;
        let Append {
            mut tx,
            conversation,
            members,
            member,
            seq,
            now,
        } = match prepared {
            Prepared::Retry(receipt) => return Ok(receipt),
            Prepared::Ready(append) => append,
        };

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
            server_ts: now,
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        };
        let receipt = SendReceipt::from(&entry);

        if tx.insert_entry(entry).await? == InsertOutcome::Duplicate {
            // Backstop: the lookup in `prepare_append` normally catches retries. Rolling back
            // returns `seq` to the counter, keeping the log gapless.
            tx.rollback().await?;
            return self
                .replay_append(
                    conversation.id,
                    command.client_msg_id,
                    command.sender,
                    EntryKind::Message,
                )
                .await;
        }
        tx.raise_read_seq(conversation.id, command.sender, seq)
            .await?;
        let read_receipt =
            direct_receipt(&mut tx, &conversation, command.sender, member.read_seq, seq).await?;
        tx.commit().await?;

        let mut events = vec![Event::EntriesAvailable {
            users: current_member_ids(&members),
        }];
        events.extend(read_receipt);
        self.publish(events).await;
        Ok(receipt)
    }
```

- [ ] **Step 8: Run the tests to see them pass**

Run: `cargo test -p push-domain --test receipts`
Expected: `7 passed`.

- [ ] **Step 9: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 153 tests pass, no warnings, no formatting diff.

- [ ] **Step 10: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): push delivery and read receipts in direct conversations"
```

---

### Task 11: List contacts

**Files:**
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/memory.rs`
- Modify: `crates/push-domain/src/contacts.rs`
- Modify: `crates/push-domain/tests/contacts.rs`

`ListContacts` (spec §7.1) returns the owner's own rows: whom they are connected to, whom
they asked, who asked them, and whom they blocked. Someone who blocked them is invisible,
because blocking removed the blocked user's row.

- [ ] **Step 1: Write the failing test**

Append to `crates/push-domain/tests/contacts.rs`:

```rust
#[tokio::test]
async fn contacts_are_listed_from_the_owners_side_only() {
    let world = World::new();
    let (alice, bob, carol, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
        world.person().await,
    );
    world.befriend(&alice, &bob).await;
    world
        .svc
        .contact_request(alice.user, carol.user)
        .await
        .unwrap();
    world
        .svc
        .contact_block(alice.user, dave.user)
        .await
        .unwrap();

    let mut expected = vec![
        (bob.user, ContactState::Accepted),
        (carol.user, ContactState::RequestedOut),
        (dave.user, ContactState::Blocked),
    ];
    expected.sort_by_key(|(peer, _)| *peer);
    assert_eq!(world.svc.list_contacts(alice.user).await, Ok(expected));
    assert_eq!(world.svc.list_contacts(dave.user).await, Ok(vec![]));
    assert_eq!(
        world.svc.list_contacts(carol.user).await,
        Ok(vec![(alice.user, ContactState::RequestedIn)])
    );
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test -p push-domain --test contacts`
Expected: compile error — `no method named list_contacts found`.

- [ ] **Step 3: Add `contacts_of`**

In `crates/push-domain/src/store.rs`, add after `delete_contact`:

```rust
    /// Every row owned by `owner`, as `(peer, state)`. Empty for an unknown user.
    /// The order of the result is unspecified.
    async fn contacts_of(&mut self, owner: UserId) -> DomainResult<Vec<(UserId, ContactState)>>;
```

In `crates/push-domain/src/memory.rs`, add before `find_direct`:

```rust
    async fn contacts_of(&mut self, owner: UserId) -> DomainResult<Vec<(UserId, ContactState)>> {
        Ok(self
            .work
            .contacts
            .iter()
            .filter(|((row_owner, _), _)| *row_owner == owner)
            .map(|((_, peer), state)| (*peer, *state))
            .collect())
    }
```

- [ ] **Step 4: Add `list_contacts`**

In `crates/push-domain/src/contacts.rs`, add inside the `impl` block, before `begin_pair`:

```rust
    /// Every relationship `user` has, as they see it, sorted by peer. Includes the users
    /// they have blocked; never shows that someone blocked them.
    pub async fn list_contacts(&self, user: UserId) -> DomainResult<Vec<(UserId, ContactState)>> {
        let mut tx = self.store.begin().await?;
        let mut contacts = tx.contacts_of(user).await?;
        tx.rollback().await?;
        contacts.sort_by_key(|(peer, _)| *peer);
        Ok(contacts)
    }
```

- [ ] **Step 5: Run it to see it pass**

Run: `cargo test -p push-domain --test contacts`
Expected: `24 passed`.

- [ ] **Step 6: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 154 tests pass, no warnings, no formatting diff.

- [ ] **Step 7: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): list contacts"
```

---

### Task 12: List conversations with unread counts

**Files:**
- Modify: `crates/push-domain/src/validation.rs`
- Modify: `crates/push-domain/src/model.rs`
- Modify: `crates/push-domain/src/conversations.rs`
- Modify: `crates/push-domain/src/lib.rs`
- Create: `crates/push-domain/tests/listings.rs`

`ListConversations{changed_since}` (spec §4.5, §7.1) gives a client, after connecting, its
conversations with positions and unread counts. `unread` counts live messages from others
after the user's read position, within their visible range, capped at 100 ("99+"); it is one
`count_incoming_messages` query per conversation. A direct conversation with someone the
user has blocked is left out of the blocker's list (spec §5.5); the blocked user still sees
it.

A summary of a direct conversation also carries how far the other member got
(`peer_delivered_seq`, `peer_read_seq`, from `Tx::delivered_seq` and their member row), with
delivered reported as at least read, exactly as receipts report it. This is how a client
recovers the receipts it missed while disconnected. `changed_since` reflects content changes
only (a conversation created or an entry appended): receipt positions do not move
`updated_at`, so the busy conversation row is not written on every ack and read. A
reconnecting client refreshes its ticks with one listing without `changed_since`. Plan 2B
adds group aggregates; for groups these two fields will be replaced or extended then.

- [ ] **Step 1: Write the failing tests**

`crates/push-domain/tests/listings.rs`:

```rust
mod common;

use chrono::TimeDelta;
use common::{Person, World, delete};
use push_domain::{
    Clock, ConversationId, ConversationKind, ConversationSummary, MAX_UNREAD, MessageRef, UserId,
};

async fn summary(world: &World, person: &Person, chat: ConversationId) -> ConversationSummary {
    world
        .svc
        .list_conversations(person.user, None)
        .await
        .unwrap()
        .into_iter()
        .find(|summary| summary.conversation_id == chat)
        .expect("conversation listed")
}

async fn listed(world: &World, person: &Person) -> Vec<ConversationId> {
    world
        .svc
        .list_conversations(person.user, None)
        .await
        .unwrap()
        .iter()
        .map(|summary| summary.conversation_id)
        .collect()
}

fn sorted(mut users: Vec<UserId>) -> Vec<UserId> {
    users.sort();
    users
}

#[tokio::test]
async fn a_new_direct_conversation_is_listed_for_both_members() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    for person in [&alice, &bob] {
        let listed = world
            .svc
            .list_conversations(person.user, None)
            .await
            .unwrap();
        assert_eq!(
            listed,
            [ConversationSummary {
                conversation_id: chat,
                kind: ConversationKind::Direct,
                title: None,
                last_seq: 0,
                read_seq: 0,
                unread: 0,
                peer_delivered_seq: 0,
                peer_read_seq: 0,
                updated_at: world.clock.now(),
                members: sorted(vec![alice.user, bob.user]),
            }]
        );
    }
}

#[tokio::test]
async fn unread_counts_messages_from_others_after_the_read_position() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }

    let bobs = summary(&world, &bob, chat).await;
    assert_eq!((bobs.last_seq, bobs.read_seq, bobs.unread), (3, 0, 3));
    world.svc.read_up_to(bob.user, chat, 2).await.unwrap();
    assert_eq!(summary(&world, &bob, chat).await.unread, 1);

    let alices = summary(&world, &alice, chat).await;
    assert_eq!((alices.read_seq, alices.unread), (3, 0));
}

#[tokio::test]
async fn unread_skips_changes_and_deleted_messages() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let kept = world.send(&alice, chat, "kept").await;
    let gone = world.send(&alice, chat, "gone").await;
    world.edit(&alice, chat, &kept, "kept, edited").await;
    world
        .svc
        .delete_message(delete(&alice, chat, MessageRef::Id(gone.message_id)))
        .await
        .unwrap();

    let bobs = summary(&world, &bob, chat).await;
    assert_eq!((bobs.last_seq, bobs.unread), (4, 1));
}

#[tokio::test]
async fn unread_is_capped() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for _ in 0..=MAX_UNREAD {
        world.send(&alice, chat, "x").await;
    }

    assert_eq!(summary(&world, &bob, chat).await.unread, MAX_UNREAD);
}

#[tokio::test]
async fn unread_and_last_seq_stay_within_the_visible_range() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for _ in 0..5 {
        world.send(&alice, chat, "x").await;
    }

    world.set_visibility(chat, bob.user, 3, None).await;
    let bobs = summary(&world, &bob, chat).await;
    assert_eq!((bobs.last_seq, bobs.unread), (5, 3));

    world.set_visibility(chat, bob.user, 1, Some(2)).await;
    let bobs = summary(&world, &bob, chat).await;
    assert_eq!((bobs.last_seq, bobs.unread), (2, 2));
}

#[tokio::test]
async fn changed_since_lists_only_conversations_updated_later() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let quiet = world.direct_chat(&alice, &bob).await;
    let busy = world.direct_chat(&alice, &carol).await;
    let since = world.clock.now();
    world.clock.advance(TimeDelta::minutes(1));
    world.send(&carol, busy, "news").await;

    let changed = world
        .svc
        .list_conversations(alice.user, Some(since))
        .await
        .unwrap();
    assert_eq!(
        changed
            .iter()
            .map(|summary| summary.conversation_id)
            .collect::<Vec<_>>(),
        [busy]
    );
    assert!(
        world
            .svc
            .list_conversations(alice.user, Some(world.clock.now()))
            .await
            .unwrap()
            .is_empty()
    );
    let mut all = vec![quiet, busy];
    all.sort();
    assert_eq!(listed(&world, &alice).await, all);
}

#[tokio::test]
async fn a_blocked_conversation_is_hidden_from_the_blocker_only() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert!(listed(&world, &alice).await.is_empty());
    assert_eq!(listed(&world, &bob).await, [chat]);

    world
        .svc
        .contact_unblock(alice.user, bob.user)
        .await
        .unwrap();
    assert_eq!(listed(&world, &alice).await, [chat]);
}

#[tokio::test]
async fn peer_positions_are_zero_until_the_peer_acks_or_reads() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    let alices = summary(&world, &alice, chat).await;
    assert_eq!((alices.peer_delivered_seq, alices.peer_read_seq), (0, 0));
}

#[tokio::test]
async fn peer_positions_are_listed_but_do_not_count_as_changes() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }
    let since = world.clock.now();
    world.clock.advance(TimeDelta::minutes(1));

    world.svc.ack(bob.device, chat, 2).await.unwrap();
    let alices = summary(&world, &alice, chat).await;
    assert_eq!((alices.peer_delivered_seq, alices.peer_read_seq), (2, 0));

    world.svc.read_up_to(bob.user, chat, 3).await.unwrap();
    let alices = summary(&world, &alice, chat).await;
    assert_eq!(
        (alices.peer_delivered_seq, alices.peer_read_seq),
        (3, 3),
        "read implies delivered"
    );
    assert!(
        world
            .svc
            .list_conversations(alice.user, Some(since))
            .await
            .unwrap()
            .is_empty(),
        "an ack or a read is not a content change"
    );
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p push-domain --test listings`
Expected: compile error — `unresolved imports push_domain::ConversationSummary,
push_domain::MAX_UNREAD`.

- [ ] **Step 3: Add the limit and the summary type**

In `crates/push-domain/src/validation.rs`, add after `MAX_SYNC_BATCH`:

```rust
/// Unread counts stop here; clients show "99+".
pub const MAX_UNREAD: u32 = 100;
```

In `crates/push-domain/src/model.rs`, add before `struct ConversationState`:

```rust
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
```

In `crates/push-domain/src/lib.rs`, replace the `model` and `validation` re-exports with:

```rust
pub use model::{
    ContactState, ContentType, Conversation, ConversationKind, ConversationState,
    ConversationSummary, Entry, EntryKind, Member, MemberState, Role, SendReceipt, direct_key,
};
```

```rust
pub use validation::{
    MAX_HISTORY_PAGE, MAX_SYNC_BATCH, MAX_TEXT_BYTES, MAX_UNREAD, ValidationError, validate_text,
};
```

- [ ] **Step 4: Add `list_conversations`**

In `crates/push-domain/src/conversations.rs`, replace the imports with:

```rust
use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{
    ContactState, Conversation, ConversationKind, ConversationSummary, Member, Role, direct_key,
};
use crate::notify::Notifier;
use crate::service::{Messaging, current_member_ids};
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::{MAX_UNREAD, ValidationError};
```

and add this `impl` block and the two helpers between the existing `impl` block and
`require_conversation`:

```rust
impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Every conversation `user` is or was a member of, with positions and unread counts,
    /// sorted by id. With `changed_since`, only those whose `updated_at` is later.
    ///
    /// `changed_since` reflects content changes only: a conversation created or an entry
    /// appended. Receipt positions (`peer_delivered_seq`, `peer_read_seq`) do not move
    /// `updated_at`, so the busy conversation row is not written on every ack and read. A
    /// reconnecting client refreshes its ticks with one listing without `changed_since`.
    ///
    /// A direct conversation with someone `user` has blocked is left out: blocking hides it
    /// from the blocker (spec §5.5). The blocked user still sees it.
    pub async fn list_conversations(
        &self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
    ) -> DomainResult<Vec<ConversationSummary>> {
        let mut tx = self.store.begin().await?;
        let mut memberships = tx.memberships(user).await?;
        memberships.sort_by_key(|(conversation, _)| conversation.id);

        let mut summaries = Vec::new();
        for (conversation, member) in memberships {
            if changed_since.is_some_and(|since| conversation.updated_at <= since) {
                continue;
            }
            let members = tx.members(conversation.id).await?;
            if conversation.kind == ConversationKind::Direct
                && has_blocked_a_member(&mut tx, user, &members).await?
            {
                continue;
            }
            let last_seq = member.visible_upper(conversation.last_seq);
            let unread_after = member.read_seq.max(member.visible_from_seq - 1);
            let unread = tx
                .count_incoming_messages(conversation.id, user, unread_after, last_seq, MAX_UNREAD)
                .await?;
            let (peer_delivered_seq, peer_read_seq) =
                peer_positions(&mut tx, &conversation, user, &members).await?;
            summaries.push(ConversationSummary {
                conversation_id: conversation.id,
                kind: conversation.kind,
                title: conversation.title,
                last_seq,
                read_seq: member.read_seq,
                unread,
                peer_delivered_seq,
                peer_read_seq,
                updated_at: conversation.updated_at,
                members: current_member_ids(&members),
            });
        }
        tx.rollback().await?;
        Ok(summaries)
    }
}

/// How far the other member of a direct conversation got, as `(delivered, read)`, with
/// delivered at least read. `(0, 0)` for other conversations: Plan 2B adds group
/// aggregates.
async fn peer_positions<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    user: UserId,
    members: &[Member],
) -> DomainResult<(i64, i64)> {
    if conversation.kind != ConversationKind::Direct {
        return Ok((0, 0));
    }
    let Some(peer) = members.iter().find(|member| member.user_id != user) else {
        return Ok((0, 0));
    };
    let delivered_seq = tx.delivered_seq(conversation.id, peer.user_id).await?;
    Ok((delivered_seq.max(peer.read_seq), peer.read_seq))
}

/// Whether `user` has blocked any other member.
async fn has_blocked_a_member<T: Tx>(
    tx: &mut T,
    user: UserId,
    members: &[Member],
) -> DomainResult<bool> {
    for member in members.iter().filter(|member| member.user_id != user) {
        if tx.get_contact(user, member.user_id).await? == Some(ContactState::Blocked) {
            return Ok(true);
        }
    }
    Ok(false)
}
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p push-domain --test listings`
Expected: `9 passed`.

- [ ] **Step 6: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 163 tests pass (31 unit, 132 integration), no warnings, no formatting diff.

- [ ] **Step 7: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): list conversations with unread counts"
```

---

### Task 13: Spec, roadmap and pull request

**Files:**
- Modify: `docs/superpowers/specs/2026-10-11-push-messenger-design.md`
- Modify: `docs/superpowers/plans/2026-10-11-roadmap.md`

- [ ] **Step 1: Bring the spec in line with the decisions**

In §3.2, in the `conversation` table, replace the `created_at` line with:

```sql
  created_at      timestamptz not null default now(),
  updated_at      timestamptz not null default now()   -- creation or last append
```

In §4.6, in the first check, replace the parenthesis `(deleting twice returns the first
result)` with `(a retry of the same delete returns its original result; a new delete, or
an edit, of a deleted message is refused)`, re-wrapping the lines at about 88 columns.

In §4.5, add two bullets after the "Position changes that pass only over …" bullet:

```markdown
- Sending marks the conversation read for the sender, so a reply can also produce a read
  receipt for the other member of a direct chat.
- A read implies delivery: a receipt never reports `delivered_seq` below `read_seq`.
```

and replace the last bullet of §4.5 ("After connecting, the client calls
`ListConversations{changed_since}` …") with:

```markdown
- After connecting, the client calls `ListConversations{changed_since}` to get the current
  positions and unread counts for conversations whose content changed. `changed_since`
  reflects content changes only: a conversation created or an entry appended. Receipt
  positions are not tracked by `updated_at`, so the busy conversation row is not written
  on every ack and read. A direct conversation's summary carries the other member's
  `peer_delivered_seq` and `peer_read_seq`, so a reconnecting client refreshes its ticks
  with one `ListConversations` without `changed_since`.
```

In §5.5, add at the end of the "Contact request" bullet: `A decline is announced only to
the user who declined; to the requester it looks the same as a block.`

- [ ] **Step 2: Split Plan 2 in the roadmap**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the row for plan 2 with:

```markdown
| 2A | Edits, events and listings | Typed events (entries available, receipts, contact changes), edit and delete with erasure, direct receipts, history of messages only, conversation and contact listings with unread counts | 1 | Done |
| 2B | Invites and groups | Invites, groups up to 100 with admin/member roles, system entries, visibility ranges from joins and leaves, group receipt aggregates | 2A | |
```

and in the row for plan 3 replace `| 1, 2 |` with `| 1, 2A, 2B |`.

- [ ] **Step 3: Run the workspace gates**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Expected: no formatting diff, no warnings, 163 tests pass.

- [ ] **Step 4: Commit and push**

```powershell
git add docs
git commit -m "docs: record plan 2A decisions in the spec and roadmap"
git push -u origin feature/plan-02a-edits-events-listings
```

- [ ] **Step 5: Open the pull request and wait for CI**

```powershell
gh pr create --base main --title "Plan 2A: edits, events and listings" --body "Implements docs/superpowers/plans/2026-10-12-plan-02a-edits-events-listings.md: typed events, edit and delete with erasure, direct receipts, history of messages only, conversation and contact listings with unread counts."
gh pr checks --watch
```

Expected: the `ci` workflow succeeds. Do not merge; the controller merges after the final
review.

---

## Spec coverage for this plan

| Spec item | Task |
|---|---|
| §3.2 `conversation.updated_at` (added by this plan) | 4, 13 |
| §4.2 retry answered before permission checks, now for every entry kind | 6 |
| §4.4 delivered = furthest position across all devices, revoked included (doc) | 1 |
| §4.5 direct receipts when delivered or read moves, only over real messages from others | 10 |
| §4.5 read implies delivered in receipts and summaries (added by this plan) | 10, 12, 13 |
| §4.5 `ListConversations{changed_since}` with positions and unread counts | 4, 12 |
| §4.5 receipts recovered after reconnect: peer positions in direct summaries, `changed_since` for content only (added by this plan) | 2, 12, 13 |
| §4.6 edit and delete are log entries with their own `seq` and `client_msg_id` | 6, 7 |
| §4.6 edit updates body, `edited_at`, `edit_seq`; previous edit text erased | 6 |
| §4.6 delete sets `deleted_at`, erases the message and every edit of it | 7 |
| §4.6 checks under the conversation lock: kind, not deleted, original sender on any device, 48 h windows as server settings | 6, 7 |
| §4.6 target by `target_message_id` or `target_client_msg_id` | 6 |
| §4.6 a message is delivered in its current state | 6, 7 |
| §4.6 history returns only messages and system entries | 9 |
| §4.6 visibility applies to the target | 8 |
| §4.6 unread counts: live messages from others, capped at 100 | 12 |
| §4.7 events only after commit, never a message body | 2 |
| §5.5 blocking hides the conversation from the blocker; a block is never revealed | 3, 12 |
| §7.1 Edit, Delete, ListConversations, ListContacts | 6, 7, 11, 12 |
| §7.2 SendAck for Edit and Delete, Receipt (direct), ContactEvent | 2, 3, 6, 7, 10 |
| Plan 1 review: `DomainError::is_retryable`, store and model doc fixes, new-device pin | 1 |

## Deferred to Plan 2B

- Invites: `CreateInvite`, `RedeemInvite`, `RevokeInvite`, expiry and use limits.
- Groups: `CreateGroup`, `AddMembers`, `RemoveMember`, `LeaveGroup`, `RenameGroup`,
  `SetRole`; admin and member roles with at least one admin; system entries for every
  membership change; the 100-member cap under the conversation lock.
- Group receipts: `ReceiptUpdate::Group { all_delivered_seq, all_read_seq }` over current
  members only, sent only when the aggregate changes, at most once per second per
  conversation; never per-member positions.
- `conversation_state` for groups (aggregates only, former members excluded).
- Group summaries in `list_conversations`: `peer_delivered_seq` and `peer_read_seq` are
  replaced or extended by the group aggregates (they are 0 for groups until then).
- What a former member sees in `list_conversations` (members, `updated_at`).
- A real test of the ack-below-floor guard once group joins exist.
- Telling a user's other devices that their own read position moved (today they learn it
  from `list_conversations` without `changed_since`).

Notes for Plan 3 (Postgres): implement the new `Tx` methods with the index on
`(conversation_id, target_seq) where target_seq is not null`; `next_seq` is one
`UPDATE … SET last_seq = last_seq + 1, updated_at = $2 RETURNING last_seq`;
`count_incoming_messages` is `SELECT count(*) FROM (SELECT 1 … LIMIT $cap)`.
