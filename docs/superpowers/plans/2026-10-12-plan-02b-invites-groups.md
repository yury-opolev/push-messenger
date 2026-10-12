# Plan 2B — Invites and Groups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend `push-domain` with invite codes that make two users contacts in one step, groups of up to 100 members with admin and member roles, system entries for every membership and settings change, visibility ranges from real joins and leaves, and group-wide delivery and read receipts.

**Architecture:** Everything stays inside the `Messaging` service over the `Store`/`Tx` traits, one storage transaction per operation, events published only after commit. Every group change takes the conversation lock through `next_seq` and appends a `System` entry, so membership changes and messages are strictly ordered in one log. Group receipts are minimums over the current members of positions that skip each member's own messages; they are computed before and after each change inside the same transaction. Every new storage operation is a narrow `Tx` method with a written contract that the strict in-memory store implements now and Plan 3 implements on Postgres. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §3.2, §4.5, §5.5, §6, §7.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85), tokio, async-trait, chrono, uuid (v4 and v7), thiserror, serde and serde_json (new in Task 4, for system entry bodies).

---

## Before you start

- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

  ```powershell
  git switch main
  git pull
  git switch -c feature/plan-02b-invites-groups
  ```

- The toolchain is rustup-managed (`C:\Users\yurio\.cargo\bin`). If `cargo --version` prints
  something older than 1.85, or `cargo clippy` is missing, the shell has a stale PATH. Fix it
  for the current PowerShell session:

  ```powershell
  $env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
  ```

- **Edit files with the edit tool, and keep them LF.** The repository stores text with LF
  line endings (`.gitattributes`: `* text=auto eol=lf`). Change existing files with the
  edit tool, which keeps their line endings; never rewrite a file through PowerShell
  `Set-Content` or `Out-File`. Some file-creation tools on Windows write CRLF: convert a
  new file to LF if one did. Git normalises line endings on commit either way.
- **Do not pipe `cargo fmt`.** Run `cargo fmt --all -- --check` on its own and read the
  exit code; piping it into `Select-String` or `Out-Null` hides the diff and the failure.
  If it reports a diff, run `cargo fmt --all` and check again. The code in this plan is
  already formatted, so a diff usually means a copying mistake.
- Baseline: `cargo test -p push-domain` passes 170 tests (31 unit, 139 integration).
- **Testing rule:** every test in this plan is written before the code it tests, and is run
  once to watch it fail. The exceptions are marked: tests that pin behaviour the code
  already has.
- **Every task ends green.** The last step of each task runs all three gates:

  ```powershell
  cargo test -p push-domain
  cargo clippy -p push-domain --all-targets -- -D warnings
  cargo fmt --all -- --check
  ```

  Never add `#[allow(dead_code)]` (the existing one in `tests/common/mod.rs` stays). The
  tasks are ordered so that every helper is used by the task that introduces it.
- **Scope rule:** group invite links, admins deleting other members' messages, per-member
  read lists, mute, avatars and push notifications are out of scope. Do not add them.

## Key rules the code must preserve (from the spec)

- `seq` is gapless per conversation. Every failure path and every no-op before commit rolls
  back, so a refused or idempotent group change does not consume a `seq`.
- Never acknowledge before commit. Events are published only after commit, carry no message
  bodies, and losing one only delays a client.
- Positions only move forward. Delivery positions move only on `ack`.
- A block is never revealed to the blocked user — not even through an invite.
- Sends and membership changes are strictly ordered: every group change appends its system
  entry under the same conversation lock as messages, so a removed member never receives
  anything after their removal entry.
- Per-member positions in a group are private: only group-wide minimums leave the domain.
- The in-memory store stays strict: it enforces the same keys Postgres will, and every `Tx`
  method's documented behaviour (missing rows, ordering, upsert or insert, locking) is what
  it implements.

## Decisions made for this plan

These were settled while writing the plan. Do not reopen them during execution.

1. **Invite codes are 10 characters of Crockford base32** (`0-9A-Z` without `I`, `L`, `O`,
   `U`) taken from the 50 lowest bits of `Uuid::new_v4()` (which are all random). 50 bits
   is enough because redeeming is rate limited per account and per IP at the transport
   layer (spec §5.6). Typed codes are normalised before lookup: upper case, `-` and spaces
   dropped, `I` and `L` read as `1`, `O` as `0`. A code collision on insert (about one in
   10^15 per stored invite) is reported as the retryable `Conflict`; the domain does not
   loop.
2. **Invite limits:** 1 to 100 uses, 1 minute to 30 days, both inclusive. An invite is
   usable while `revoked_at` is null, `now < expires_at` and `uses < max_uses`.
3. **One error for every unusable invite.** Unknown, malformed, revoked, expired and used-up
   codes, and a code whose creator has blocked the redeemer, are all `InviteNotFound`, so
   redeeming is not an oracle. Redeeming one's own code is `Invalid(SelfTarget)`; a redeemer
   who blocked the creator gets `Blocked` (they know their own block). Users who are already
   accepted contacts succeed without using up the invite. Otherwise both rows become
   `Accepted` (replacing any pending request), `uses` grows by one and both users get
   `ContactChanged`.
4. **Lock order for redeem:** the invite row (`Tx::lock_invite`, `SELECT … FOR UPDATE`)
   first, then the contact pair. Nothing takes them in the other order.
5. **System entry bodies are compact, versioned JSON** (`serde_json`), internally tagged:
   `{"v":1,"type":"members_added","actor":"…","users":["…"]}`. JSON because every client
   language decodes it without generated code, a client can show an unknown `type` as a
   generic line, and fields can be added without a new version; the largest body (a group
   created with 99 others) is about 4 KB. A hand-written text format was rejected: it would
   need its own escaping rules for titles and a decoder in every SDK binding. Events:
   `created`, `members_added`, `member_removed`, `member_left`, `renamed`, `role_changed`,
   `admin_promoted`. Every event carries `actor`.
6. **System entries:** `kind = System`, `content_type = SystemEvent`, a new v7
   `client_msg_id`, `sender_id = actor` and `sender_device_id = None` for all of them (the
   group operations take users, not devices, and spec §3.2 says system rows have no
   device). For `AdminPromoted` the actor is the member whose leaving left no admin.
7. **A joining member's read position starts at `seq - 1`**, where `seq` is the entry that
   let them in (`Created` or `MembersAdded`), and so does every delivery position created
   for their active devices. `seq - 1` is exactly their floor (`visible_from_seq - 1`): they
   could not have read anything before it, nothing before it counts as unread, and it never
   lowers the position of a member who rejoins (their old `read_seq` is at most their old
   `left_at_seq`, which is below `seq`). A rejoining member's devices keep their old
   delivery positions (`create_cursor_if_absent`); every reader clamps at the floor, and an
   ack below the floor changes nothing — the guard Plan 2A could not test is tested here.
8. **Group-wide positions (spec §4.5).** A member's *effective* read position is just
   before the first live (not deleted) message from someone else after their `read_seq`,
   within their visible range, or the end of that range if there is none. Their effective
   delivered position is the same from their furthest device position (revoked devices
   included), and at least their effective read position. `all_read_seq` and
   `all_delivered_seq` are the minimums over the **current** members. So "every message up
   to N has been read by every member except its sender". They never decrease.
9. **When a group receipt is published.** Every operation that can move the group-wide
   positions computes them before and after the change, inside its transaction: `ack`,
   `read_up_to`, sends, edits, deletes and every membership change. A
   `ReceiptUpdate::Group` goes to all current members when either position moved over at
   least one live message. Moving only over edits, deletes or system entries tells nobody
   anything (spec §4.5), so a rename or an edit never sends one; a delete can (it may
   uncover a message that was already read). The domain publishes every change; sending at
   most one per second per conversation is the gateway's job.
10. **For appends, "before" is computed under the lock** with the log ending at `seq - 1`
    (`prepare_append` and `begin_change`), so concurrent appends cannot skew it. `Append`
    carries it as `group_before`; `Prepared::Ready` now boxes `Append`
    (`clippy::large_enum_variant`).
11. **`ConversationSummary.receipts: Receipts`** replaces `peer_delivered_seq` and
    `peer_read_seq`: `Receipts::Direct { peer_delivered_seq, peer_read_seq }` or
    `Receipts::Group { all_delivered_seq, all_read_seq }`. The summary also gains
    `admins: Vec<UserId>` (current admins, sorted; empty for direct conversations): a client
    must know who may rename or add, and roles are not otherwise listed.
12. **`ConversationState.positions: Positions`** replaces `members`:
    `Positions::Members(Vec<MemberState>)` for a direct conversation (as before) and
    `Positions::Group { all_delivered_seq, all_read_seq }` for a group. `last_seq` is capped
    at the caller's visible range.
13. **A former member lists a group as they last saw it**: `last_seq` at their `left_at_seq`,
    `updated_at` the time of the entry at which they left (so later activity does not show
    up in `changed_since`), `members` and `admins` empty, and group-wide positions capped at
    where they left. The title shown is the current one (see Deferred).
14. **`Event::ReadPositionChanged { user, conversation, read_seq }`** goes to the actor after
    `read_up_to` or a send moves their read position, so their other devices update unread
    counts. Event order is fixed: `EntriesAvailable`, `ReadPositionChanged`, direct
    `Receipt`, group `Receipt`.
15. **`read_up_to` and sends count receipts from the member's floor**
    (`max(read_seq, visible_from_seq - 1)`), as `ack` already does, so a message below a
    member's visible range is never reported as read.
16. **Group changes and their errors.** Precedence: `ConversationNotFound`, `NotAGroup`
    (direct conversations), then the actor (`NotMember`, `NotAdmin`), then the
    operation-specific checks (`GroupFull`, `NotContact`, `LastAdmin`, target `NotMember`).
    No-ops — adding current members, removing a former member, leaving twice, renaming to
    the same title, giving a member their current role — roll back and publish nothing.
    `create_group` has no idempotency key and is not idempotent. An admin cannot remove
    themselves (`Invalid(SelfTarget)`; they leave instead). The heir when the last admin
    leaves is the current member with the lowest `visible_from_seq`, then the lowest user id;
    the `AdminPromoted` entry follows the `MemberLeft` entry, so the leaver does not see it.
17. **`list_contacts` returns `Vec<ContactSummary { user_id, state }>`** instead of tuples.

## File structure

```
crates/push-domain/
├── Cargo.toml              + serde, serde_json
├── src/
│   ├── lib.rs              + new modules and re-exports
│   ├── error.rs            + InviteNotFound, GroupFull, NotAGroup, NotAdmin, LastAdmin
│   ├── validation.rs       + invite and title limits, validate_invite, validate_title
│   ├── ids.rs              serde (transparent UUID strings)
│   ├── model.rs            + ContactSummary, InviteRecord, Invite, Receipts, Positions;
│   │                         Role serde; ConversationSummary, ConversationState changed
│   ├── notify.rs           + Event::ReadPositionChanged, ReceiptUpdate::Group
│   ├── store.rs            + insert_invite, lock_invite, update_invite,
│   │                         first_live_message, set_title
│   ├── memory.rs           the new Tx methods
│   ├── system.rs           NEW: SystemEvent, its JSON encoding
│   ├── invites.rs          NEW: create_invite, revoke_invite, redeem_invite
│   ├── groups.rs           NEW: create_group, add_members, remove_member, leave_group,
│   │                         rename_group, set_role, GroupChange
│   ├── contacts.rs         ContactSummary; set_contact, contact_changed shared
│   ├── conversations.rs    receipts and admins in listings, former members
│   ├── read.rs             ReadPositionChanged, floor clamp, group positions and receipts,
│   │                         conversation_state for groups
│   ├── sync.rs             group receipts on ack
│   ├── send.rs             ReadPositionChanged, floor clamp, group receipts, boxed Append
│   └── edit.rs             group receipts on edit and delete
└── tests/
    ├── common/mod.rs       + member, group, system_events helpers
    ├── contacts.rs         ContactSummary
    ├── receipts.rs         ReadPositionChanged, floor clamp
    ├── listings.rs         Receipts, admins, group summaries
    ├── read_state.rs       Positions, group state
    ├── direct.rs, edit.rs  Positions
    ├── invites.rs          NEW
    ├── groups.rs           NEW: create, add
    ├── group_receipts.rs   NEW
    ├── membership.rs       NEW: remove, leave, rejoin, former members
    └── group_settings.rs   NEW: rename, roles
```

---

### Task 1: Plan 2A review leftovers

**Files:**
- Modify: `crates/push-domain/src/model.rs`
- Modify: `crates/push-domain/src/contacts.rs`
- Modify: `crates/push-domain/src/notify.rs`
- Modify: `crates/push-domain/src/read.rs`
- Modify: `crates/push-domain/src/send.rs`
- Modify: `crates/push-domain/src/lib.rs`
- Modify: `crates/push-domain/tests/contacts.rs`
- Modify: `crates/push-domain/tests/receipts.rs`

Three items from the Plan 2A review (decisions 14, 15 and 17): `list_contacts` returns a
struct, the actor's own devices learn that their read position moved, and `read_up_to` and
sends count receipts from the member's floor, as `ack` already does.

- [ ] **Step 1: Write the failing contact-listing test**

In `crates/push-domain/tests/contacts.rs`, replace the `use push_domain::{…}` line with:

```rust
use push_domain::{ContactState, ContactSummary, DomainError, Event, UserId, ValidationError};
```

and replace the test `contacts_are_listed_from_the_owners_side_only` with:

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

    let contact = |person: &Person, state| ContactSummary {
        user_id: person.user,
        state,
    };
    let mut expected = vec![
        contact(&bob, ContactState::Accepted),
        contact(&carol, ContactState::RequestedOut),
        contact(&dave, ContactState::Blocked),
    ];
    expected.sort_by_key(|summary| summary.user_id);
    assert_eq!(world.svc.list_contacts(alice.user).await, Ok(expected));
    assert_eq!(world.svc.list_contacts(dave.user).await, Ok(vec![]));
    assert_eq!(
        world.svc.list_contacts(carol.user).await,
        Ok(vec![contact(&alice, ContactState::RequestedIn)])
    );
}
```

- [ ] **Step 2: Write the failing receipt tests**

In `crates/push-domain/tests/receipts.rs`, add after the `receipt` helper:

```rust
fn read_moved(person: &Person, conversation: ConversationId, read_seq: i64) -> Event {
    Event::ReadPositionChanged {
        user: person.user,
        conversation,
        read_seq,
    }
}
```

Replace the test `reading_a_message_from_the_peer_sends_a_read_receipt_once` with:

```rust
#[tokio::test]
async fn reading_a_message_from_the_peer_sends_a_read_receipt_once() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.svc.ack(bob.device, chat, 1).await.unwrap();
    world.notifier.take();

    world.svc.read_up_to(bob.user, chat, 1).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [read_moved(&bob, chat, 1), receipt(chat, &alice, &bob, 1, 1)]
    );

    world.svc.read_up_to(bob.user, chat, 1).await.unwrap();
    assert!(world.notifier.take().is_empty());
}
```

Replace the test `reading_past_an_edit_sends_nothing` with:

```rust
#[tokio::test]
async fn reading_past_an_edit_tells_only_the_readers_devices() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    world.svc.read_up_to(bob.user, chat, 1).await.unwrap();
    world.edit(&alice, chat, &hello, "hello!").await;
    world.notifier.take();

    assert_eq!(world.svc.read_up_to(bob.user, chat, 2).await, Ok(true));
    assert_eq!(world.notifier.take(), [read_moved(&bob, chat, 2)]);
}
```

Replace the test
`replying_reports_that_the_peers_received_messages_were_read_and_so_delivered` with:

```rust
#[tokio::test]
async fn replying_reports_that_the_peers_received_messages_were_read_and_so_delivered() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&bob, chat, "hi").await;
    world.svc.ack(alice.device, chat, 1).await.unwrap();
    world.notifier.take();

    world.send(&alice, chat, "hello").await;
    assert_eq!(
        world.notifier.take(),
        [
            entries_for(&[&alice, &bob]),
            read_moved(&alice, chat, 1),
            receipt(chat, &bob, &alice, 1, 1)
        ]
    );
}
```

Append at the end of the file:

```rust
#[tokio::test]
async fn reading_never_reports_messages_below_the_readers_floor() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;
    world.set_visibility(chat, bob.user, 3, None).await;
    world.send(&bob, chat, "three").await;
    world.notifier.take();

    world.svc.read_up_to(bob.user, chat, 3).await.unwrap();
    assert_eq!(world.notifier.take(), [read_moved(&bob, chat, 3)]);
}

#[tokio::test]
async fn replying_never_reports_messages_below_the_senders_floor() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;
    world.set_visibility(chat, bob.user, 3, None).await;
    world.send(&bob, chat, "three").await;
    world.svc.ack(bob.device, chat, 3).await.unwrap();
    world.notifier.take();

    world.send(&bob, chat, "four").await;
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob]), read_moved(&bob, chat, 3)]
    );
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p push-domain --test contacts --test receipts`
Expected: compile errors — `unresolved import push_domain::ContactSummary` and
`no variant named ReadPositionChanged found for enum Event`.

- [ ] **Step 4: Add `ContactSummary` and return it from `list_contacts`**

In `crates/push-domain/src/model.rs`, insert after the `ContactState` enum:

```rust
/// One of a user's contact relationships, as that user sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContactSummary {
    /// The other user.
    pub user_id: UserId,
    pub state: ContactState,
}
```

In `crates/push-domain/src/contacts.rs`, replace `use crate::model::ContactState;` with
`use crate::model::{ContactState, ContactSummary};`, and replace `list_contacts` with:

```rust
    /// Every relationship `user` has, as they see it, sorted by peer. Includes the users
    /// they have blocked; never shows that someone blocked them.
    pub async fn list_contacts(&self, user: UserId) -> DomainResult<Vec<ContactSummary>> {
        let mut tx = self.store.begin().await?;
        let rows = tx.contacts_of(user).await?;
        tx.rollback().await?;
        let mut contacts: Vec<ContactSummary> = rows
            .into_iter()
            .map(|(user_id, state)| ContactSummary { user_id, state })
            .collect();
        contacts.sort_by_key(|contact| contact.user_id);
        Ok(contacts)
    }
```

In `crates/push-domain/src/lib.rs`, add `ContactSummary` to the `pub use model::{…}` list
after `ContactState`.

- [ ] **Step 5: Add `Event::ReadPositionChanged`**

In `crates/push-domain/src/notify.rs`, add a variant at the end of `enum Event`, after
`ContactChanged`:

```rust
    /// `user`'s own read position moved, by reading or by sending; their other devices
    /// update their unread counts. Published to `user` only.
    ReadPositionChanged {
        user: UserId,
        conversation: ConversationId,
        read_seq: i64,
    },
```

- [ ] **Step 6: Publish it from `read_up_to` and count from the floor**

In `crates/push-domain/src/read.rs`, replace `read_up_to` with:

```rust
    /// Records that `user` has read everything up to `seq`. Returns whether the position
    /// moved; it never moves backwards. When it moves, the user's devices learn the new
    /// position, and the other member of a direct conversation gets a read receipt if the
    /// move passed over a message from them.
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
        // Messages below the member's floor were never visible to them, so they are never
        // reported as read.
        let read_before = member.read_seq.max(member.visible_from_seq - 1);
        let receipt = direct_receipt(&mut tx, &found, user, read_before, seq).await?;
        tx.commit().await?;

        let mut events = Vec::new();
        if moved {
            events.push(Event::ReadPositionChanged {
                user,
                conversation,
                read_seq: seq,
            });
        }
        events.extend(receipt);
        self.publish(events).await;
        Ok(moved)
    }
```

- [ ] **Step 7: Publish it from `send_text` and count from the floor**

In `crates/push-domain/src/send.rs`, replace `send_text` with:

```rust
    /// Appends a text message. A retry with the same `client_msg_id` returns the original
    /// receipt, stores nothing new and publishes nothing — even if the sender has since lost
    /// the right to send.
    ///
    /// Advances the sender's read position to what the sending device has already received
    /// (its delivery position): replying marks as read everything the user could have seen,
    /// never messages their device has not received yet. When it moves, the sender's
    /// devices learn the new position, and if it passes over a message from the other
    /// member of a direct conversation, they get a read receipt.
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
        let device_position = tx
            .get_cursor(command.device, conversation.id)
            .await?
            .unwrap_or(0);
        let read_moved = tx
            .raise_read_seq(conversation.id, command.sender, device_position)
            .await?;
        let read_receipt = direct_receipt(
            &mut tx,
            &conversation,
            command.sender,
            member.read_seq.max(member.visible_from_seq - 1),
            device_position,
        )
        .await?;
        tx.commit().await?;

        let mut events = vec![Event::EntriesAvailable {
            users: current_member_ids(&members),
        }];
        if read_moved {
            events.push(Event::ReadPositionChanged {
                user: command.sender,
                conversation: conversation.id,
                read_seq: device_position,
            });
        }
        events.extend(read_receipt);
        self.publish(events).await;
        Ok(receipt)
    }
```

- [ ] **Step 8: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 172 tests pass (31 unit, 141 integration; `receipts` 10), no warnings, no
formatting diff. Without the floor clamp the two new floor tests fail with an extra
`Receipt` event: the clamp is what they test.

- [ ] **Step 9: Commit**

```powershell
git add crates/push-domain
git commit -m "fix(domain): plan 2A review leftovers"
```

---

### Task 2: Create and revoke invites

**Files:**
- Modify: `Cargo.toml` (workspace: uuid `v4`)
- Modify: `crates/push-domain/src/validation.rs`
- Modify: `crates/push-domain/src/model.rs`
- Modify: `crates/push-domain/src/error.rs`
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/memory.rs`
- Create: `crates/push-domain/src/invites.rs`
- Modify: `crates/push-domain/src/lib.rs`
- Create: `crates/push-domain/tests/invites.rs`

Spec §5.5: `CreateInvite{expires_in, max_uses}` returns a short code; codes expire, have a
use limit and can be revoked (decisions 1, 2 and 4). The stored row is
`invite(code PK, user_id, expires_at, max_uses, uses, revoked_at)`. Three `Tx` methods
serve it: insert, lock-and-read, and update of the two mutable fields.

- [ ] **Step 1: Write the failing validation tests**

In `crates/push-domain/src/validation.rs`, append inside `mod tests`:

```rust
    #[test]
    fn invite_limits_are_inclusive() {
        let shortest = TimeDelta::seconds(MIN_INVITE_LIFETIME_SECS);
        let longest = TimeDelta::seconds(MAX_INVITE_LIFETIME_SECS);
        assert_eq!(validate_invite(shortest, 1), Ok(()));
        assert_eq!(validate_invite(longest, MAX_INVITE_USES), Ok(()));
    }

    #[test]
    fn rejects_invites_outside_the_limits() {
        let day = TimeDelta::days(1);
        for uses in [0, MAX_INVITE_USES + 1] {
            assert_eq!(
                validate_invite(day, uses),
                Err(ValidationError::InviteUses {
                    actual: uses,
                    limit: MAX_INVITE_USES,
                })
            );
        }
        for seconds in [MIN_INVITE_LIFETIME_SECS - 1, MAX_INVITE_LIFETIME_SECS + 1] {
            assert_eq!(
                validate_invite(TimeDelta::seconds(seconds), 1),
                Err(ValidationError::InviteLifetime {
                    actual: seconds,
                    min: MIN_INVITE_LIFETIME_SECS,
                    max: MAX_INVITE_LIFETIME_SECS,
                })
            );
        }
    }
```

Run: `cargo test -p push-domain --lib validation`
Expected: compile errors — `cannot find function validate_invite`, `cannot find value
MIN_INVITE_LIFETIME_SECS`.

- [ ] **Step 2: Implement the invite limits**

In `crates/push-domain/src/validation.rs`, add below the module doc line, separated from it
by a blank line:

```rust
use chrono::TimeDelta;
```

add after `MAX_UNREAD`:

```rust
/// Most uses one invite allows.
pub const MAX_INVITE_USES: u32 = 100;
/// Shortest invite lifetime, in seconds: one minute.
pub const MIN_INVITE_LIFETIME_SECS: i64 = 60;
/// Longest invite lifetime, in seconds: 30 days.
pub const MAX_INVITE_LIFETIME_SECS: i64 = 30 * 24 * 60 * 60;
```

add two variants at the end of `ValidationError`, after `SelfTarget`:

```rust
    #[error("an invite allows 1 to {limit} uses, not {actual}")]
    InviteUses { actual: u32, limit: u32 },
    #[error("an invite lasts {min} to {max} seconds, not {actual}")]
    InviteLifetime { actual: i64, min: i64, max: i64 },
```

and add after `validate_text`:

```rust
/// Checks the limits of a new invite: 1 to `MAX_INVITE_USES` uses, and a lifetime from
/// `MIN_INVITE_LIFETIME_SECS` to `MAX_INVITE_LIFETIME_SECS`, both inclusive.
pub fn validate_invite(expires_in: TimeDelta, max_uses: u32) -> Result<(), ValidationError> {
    if !(1..=MAX_INVITE_USES).contains(&max_uses) {
        return Err(ValidationError::InviteUses {
            actual: max_uses,
            limit: MAX_INVITE_USES,
        });
    }
    if expires_in < TimeDelta::seconds(MIN_INVITE_LIFETIME_SECS)
        || expires_in > TimeDelta::seconds(MAX_INVITE_LIFETIME_SECS)
    {
        return Err(ValidationError::InviteLifetime {
            actual: expires_in.num_seconds(),
            min: MIN_INVITE_LIFETIME_SECS,
            max: MAX_INVITE_LIFETIME_SECS,
        });
    }
    Ok(())
}
```

In `crates/push-domain/src/lib.rs`, replace the `pub use validation::{…};` block with:

```rust
pub use validation::{
    MAX_HISTORY_PAGE, MAX_INVITE_LIFETIME_SECS, MAX_INVITE_USES, MAX_SYNC_BATCH, MAX_TEXT_BYTES,
    MAX_UNREAD, MIN_INVITE_LIFETIME_SECS, ValidationError, validate_invite, validate_text,
};
```

Run: `cargo test -p push-domain --lib validation`
Expected: `7 passed`.

- [ ] **Step 3: Write the failing storage tests**

In `crates/push-domain/src/memory.rs`, append inside `mod tests`:

```rust
    fn invite(code: &str, user: UserId) -> InviteRecord {
        InviteRecord {
            code: code.into(),
            user_id: user,
            expires_at: start() + TimeDelta::days(1),
            max_uses: 1,
            uses: 0,
            revoked_at: None,
        }
    }

    #[tokio::test]
    async fn an_invite_code_is_stored_once() {
        let store = InMemoryStore::new();
        let (first, second) = (UserId::new(), UserId::new());
        let mut tx = store.begin().await.unwrap();
        tx.insert_user(first).await.unwrap();
        tx.insert_user(second).await.unwrap();

        assert_eq!(
            tx.insert_invite(invite("ABCDEFGHJK", first)).await,
            Ok(InsertOutcome::Inserted)
        );
        assert_eq!(
            tx.insert_invite(invite("ABCDEFGHJK", second)).await,
            Ok(InsertOutcome::Duplicate)
        );
        assert_eq!(
            tx.lock_invite("ABCDEFGHJK").await,
            Ok(Some(invite("ABCDEFGHJK", first)))
        );
        assert_eq!(tx.lock_invite("0000000000").await, Ok(None));
        assert!(matches!(
            tx.insert_invite(invite("0000000000", UserId::new())).await,
            Err(DomainError::Store(_))
        ));
    }

    #[tokio::test]
    async fn update_invite_changes_only_uses_and_revocation() {
        let store = InMemoryStore::new();
        let user = UserId::new();
        let mut tx = store.begin().await.unwrap();
        tx.insert_user(user).await.unwrap();
        tx.insert_invite(invite("ABCDEFGHJK", user)).await.unwrap();

        let mut changed = invite("ABCDEFGHJK", UserId::new());
        changed.uses = 1;
        changed.revoked_at = Some(start());
        changed.max_uses = 9;
        tx.update_invite(changed).await.unwrap();

        let stored = tx.lock_invite("ABCDEFGHJK").await.unwrap().unwrap();
        assert_eq!((stored.uses, stored.revoked_at), (1, Some(start())));
        assert_eq!((stored.user_id, stored.max_uses), (user, 1));
        assert!(matches!(
            tx.update_invite(invite("0000000000", user)).await,
            Err(DomainError::Store(_))
        ));
    }
```

Run: `cargo test -p push-domain --lib memory`
Expected: compile errors — `cannot find struct InviteRecord`, `no method named
insert_invite`.

- [ ] **Step 4: Add the invite records**

In `crates/push-domain/src/model.rs`, insert after `ContactSummary`:

```rust
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
```

In `crates/push-domain/src/lib.rs`, replace the `pub use model::{…};` block with:

```rust
pub use model::{
    ContactState, ContactSummary, ContentType, Conversation, ConversationKind, ConversationState,
    ConversationSummary, Entry, EntryKind, Invite, InviteRecord, Member, MemberState, Role,
    SendReceipt, direct_key,
};
```

- [ ] **Step 5: Add the invite storage contract**

In `crates/push-domain/src/store.rs`, replace
`use crate::model::{ContactState, Conversation, Entry, Member};` with:

```rust
use crate::model::{ContactState, Conversation, Entry, InviteRecord, Member};
```

In the module doc's `# Locking` section, add after the paragraph that ends `so the lock
order cannot form a cycle.`:

```rust
//!
//! Redeeming an invite locks the invite row ([`Tx::lock_invite`]) first and the contact
//! pair second. No operation takes them in the other order.
```

and in `trait Tx`, insert after `contacts_of` (leave a blank line before the section
comment):

```rust
    // --- Invites -----------------------------------------------------------------------

    /// Stores a new invite. Returns `Duplicate`, writing nothing, if an invite with the same
    /// code exists (Postgres: `ON CONFLICT (code) DO NOTHING`). Fails with
    /// `DomainError::Store` if the creator is not a known user.
    async fn insert_invite(&mut self, invite: InviteRecord) -> DomainResult<InsertOutcome>;
    /// The invite with this code, or `None`. Locks the invite row until the transaction
    /// ends (`SELECT … FOR UPDATE`), so redeems and revokes of one invite are serialised and
    /// its use count cannot be exceeded.
    async fn lock_invite(&mut self, code: &str) -> DomainResult<Option<InviteRecord>>;
    /// Overwrites `uses` and `revoked_at` of the stored invite with the same code. Every
    /// other field of `invite` is ignored. Fails with `DomainError::Store` if there is no
    /// such invite.
    async fn update_invite(&mut self, invite: InviteRecord) -> DomainResult<()>;
```

- [ ] **Step 6: Implement it in memory**

In `crates/push-domain/src/memory.rs`, replace
`use crate::model::{ContactState, Conversation, Entry, EntryKind, Member};` with:

```rust
use crate::model::{ContactState, Conversation, Entry, EntryKind, InviteRecord, Member};
```

add a field to `struct State` after `contacts`:

```rust
    invites: HashMap<String, InviteRecord>,
```

and add to `impl Tx for InMemoryTx`, after `contacts_of`:

```rust
    async fn insert_invite(&mut self, invite: InviteRecord) -> DomainResult<InsertOutcome> {
        if !self.work.users.contains(&invite.user_id) {
            return Err(missing("user"));
        }
        if self.work.invites.contains_key(&invite.code) {
            return Ok(InsertOutcome::Duplicate);
        }
        self.work.invites.insert(invite.code.clone(), invite);
        Ok(InsertOutcome::Inserted)
    }

    async fn lock_invite(&mut self, code: &str) -> DomainResult<Option<InviteRecord>> {
        // Every in-memory transaction already holds the store-wide lock.
        Ok(self.work.invites.get(code).cloned())
    }

    async fn update_invite(&mut self, invite: InviteRecord) -> DomainResult<()> {
        let stored =
            self.work.invites.get_mut(&invite.code).ok_or_else(|| {
                DomainError::Store("update of an invite that does not exist".into())
            })?;
        stored.uses = invite.uses;
        stored.revoked_at = invite.revoked_at;
        Ok(())
    }
```

Run: `cargo test -p push-domain --lib memory`
Expected: `24 passed`.

- [ ] **Step 7: Write the failing service tests**

Create `crates/push-domain/tests/invites.rs`:

```rust
mod common;

use chrono::TimeDelta;
use common::World;
use push_domain::{
    Clock, DomainError, MAX_INVITE_LIFETIME_SECS, MAX_INVITE_USES, UserId, ValidationError,
};

fn day() -> TimeDelta {
    TimeDelta::days(1)
}

#[tokio::test]
async fn an_invite_has_a_ten_character_code_and_its_limits() {
    let world = World::new();
    let alice = world.person().await;

    let invite = world.svc.create_invite(alice.user, day(), 3).await.unwrap();
    assert_eq!(invite.code.len(), 10);
    assert_eq!(invite.expires_at, world.clock.now() + day());
    assert_eq!((invite.max_uses, invite.uses), (3, 0));
    let other = world.svc.create_invite(alice.user, day(), 3).await.unwrap();
    assert_ne!(other.code, invite.code);
}

#[tokio::test]
async fn invites_outside_the_limits_are_refused() {
    let world = World::new();
    let alice = world.person().await;

    assert_eq!(
        world
            .svc
            .create_invite(alice.user, day(), MAX_INVITE_USES + 1)
            .await,
        Err(DomainError::Invalid(ValidationError::InviteUses {
            actual: MAX_INVITE_USES + 1,
            limit: MAX_INVITE_USES,
        }))
    );
    let too_long = TimeDelta::seconds(MAX_INVITE_LIFETIME_SECS + 1);
    assert!(matches!(
        world.svc.create_invite(alice.user, too_long, 1).await,
        Err(DomainError::Invalid(ValidationError::InviteLifetime { .. }))
    ));
}

#[tokio::test]
async fn an_unknown_user_cannot_create_an_invite() {
    let world = World::new();
    assert_eq!(
        world.svc.create_invite(UserId::new(), day(), 1).await,
        Err(DomainError::UnknownUser)
    );
}

#[tokio::test]
async fn only_the_creator_can_revoke_and_revoking_twice_is_harmless() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let invite = world.svc.create_invite(alice.user, day(), 1).await.unwrap();

    assert_eq!(
        world.svc.revoke_invite(bob.user, &invite.code).await,
        Err(DomainError::InviteNotFound)
    );
    assert_eq!(
        world.svc.revoke_invite(alice.user, &invite.code).await,
        Ok(())
    );
    assert_eq!(
        world.svc.revoke_invite(alice.user, &invite.code).await,
        Ok(())
    );
    assert_eq!(
        world.svc.revoke_invite(alice.user, "0000000000").await,
        Err(DomainError::InviteNotFound)
    );
}
```

Run: `cargo test -p push-domain --test invites`
Expected: compile errors — `no method named create_invite`, `no variant named
InviteNotFound`.

- [ ] **Step 8: Implement creating and revoking**

In the workspace `Cargo.toml`, replace the `uuid` line under `[workspace.dependencies]`
with:

```toml
uuid = { version = "1", features = ["v4", "v7"] }
```

In `crates/push-domain/src/error.rs`, add a variant after `WindowExpired`:

```rust
    /// The invite does not exist, was revoked, has expired or is used up. One error for
    /// all of these, so redeeming is not an oracle.
    #[error("invite not found")]
    InviteNotFound,
```

Create `crates/push-domain/src/invites.rs`:

```rust
//! Invite links and QR codes (spec §5.5).
//!
//! A code is ten characters of Crockford base32: 50 random bits. That is plenty because
//! redeeming is rate limited per account and per IP at the transport layer (spec §5.6):
//! guessing one live code among a million would take about a billion attempts.

use chrono::TimeDelta;
use uuid::Uuid;

use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::{Invite, InviteRecord};
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::validate_invite;

/// Crockford's base32 alphabet: digits and letters without I, L, O and U.
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters in an invite code.
const CODE_LEN: usize = 10;

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Creates an invite that `user` can share as a link or QR code. It lasts `expires_in`
    /// and can be redeemed `max_uses` times.
    ///
    /// A new code that collides with an existing one (about one chance in 10^15 per stored
    /// invite) fails with the retryable `Conflict`.
    pub async fn create_invite(
        &self,
        user: UserId,
        expires_in: TimeDelta,
        max_uses: u32,
    ) -> DomainResult<Invite> {
        validate_invite(expires_in, max_uses)?;
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(user).await? {
            return Err(DomainError::UnknownUser);
        }
        let record = InviteRecord {
            code: invite_code(Uuid::new_v4()),
            user_id: user,
            expires_at: self.clock.now() + expires_in,
            max_uses,
            uses: 0,
            revoked_at: None,
        };
        let invite = Invite::from(&record);
        if tx.insert_invite(record).await? == InsertOutcome::Duplicate {
            return Err(DomainError::Conflict);
        }
        tx.commit().await?;
        Ok(invite)
    }

    /// Revokes one of `user`'s invites. Revoking again does nothing. Someone else's code,
    /// or an unknown one, is `InviteNotFound`.
    pub async fn revoke_invite(&self, user: UserId, code: &str) -> DomainResult<()> {
        let code = normalize_code(code).ok_or(DomainError::InviteNotFound)?;
        let mut tx = self.store.begin().await?;
        let mut invite = tx
            .lock_invite(&code)
            .await?
            .filter(|invite| invite.user_id == user)
            .ok_or(DomainError::InviteNotFound)?;
        if invite.revoked_at.is_none() {
            invite.revoked_at = Some(self.clock.now());
            tx.update_invite(invite).await?;
            tx.commit().await?;
        }
        Ok(())
    }
}

/// The invite code made from the 50 lowest bits of `random`. A version 4 UUID has 62
/// random bits there.
fn invite_code(random: Uuid) -> String {
    let (_, bits) = random.as_u64_pair();
    (0..CODE_LEN)
        .map(|index| char::from(CODE_ALPHABET[((bits >> (5 * index)) & 31) as usize]))
        .collect()
}

/// A code as typed by a person, in canonical form: upper case, without `-` or spaces, with
/// Crockford's look-alikes read as digits (`I` and `L` as `1`, `O` as `0`). `None` if it
/// cannot be a code.
fn normalize_code(typed: &str) -> Option<String> {
    let code: String = typed
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        })
        .collect();
    let valid = code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b));
    valid.then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_takes_five_bits_per_character_from_the_lowest_bits() {
        let counting = (0..10u128).fold(0, |bits, index| bits | (index << (5 * index)));
        assert_eq!(invite_code(Uuid::from_u128(counting)), "0123456789");
        assert_eq!(invite_code(Uuid::from_u128(0)), "0000000000");
        assert_eq!(invite_code(Uuid::from_u128(u128::MAX)), "ZZZZZZZZZZ");
    }

    #[test]
    fn new_codes_use_only_the_alphabet() {
        for _ in 0..100 {
            let code = invite_code(Uuid::new_v4());
            assert_eq!(normalize_code(&code), Some(code));
        }
    }

    #[test]
    fn typed_codes_are_normalized() {
        assert_eq!(normalize_code("abcd-efgh jk"), Some("ABCDEFGHJK".into()));
        assert_eq!(normalize_code("o1il0OIL23"), Some("0111001123".into()));
    }

    #[test]
    fn impossible_codes_are_refused() {
        assert_eq!(normalize_code("ABCDEFGHJ"), None);
        assert_eq!(normalize_code("ABCDEFGHJKM"), None);
        assert_eq!(normalize_code("ABCDEFGHJU"), None);
        assert_eq!(normalize_code("ABCDEFGHJ!"), None);
    }
}
```

In `crates/push-domain/src/lib.rs`, add `mod invites;` after `pub mod ids;`.

- [ ] **Step 9: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 184 tests pass (39 unit, 145 integration; `invites` 4), no warnings, no
formatting diff. `Cargo.lock` does not change: `getrandom` is already there for v7.

- [ ] **Step 10: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/push-domain
git commit -m "feat(domain): create and revoke invites"
```

---

### Task 3: Redeem invites

**Files:**
- Modify: `crates/push-domain/src/contacts.rs`
- Modify: `crates/push-domain/src/invites.rs`
- Modify: `crates/push-domain/tests/invites.rs`

Spec §5.5: `RedeemInvite{code}` makes both users accepted contacts at once — sharing the
code is the consent. Decisions 3 and 4 fix the checks and their order: the invite row is
locked first, then the contact pair; every unusable code, and a block by the creator, is
the same `InviteNotFound`.

- [ ] **Step 1: Write the failing tests**

In `crates/push-domain/tests/invites.rs`, replace everything above the first `#[tokio::test]`
with:

```rust
mod common;

use chrono::TimeDelta;
use common::{Person, World};
use push_domain::{
    Clock, ContactState, DomainError, Event, MAX_INVITE_LIFETIME_SECS, MAX_INVITE_USES, UserId,
    ValidationError,
};

fn day() -> TimeDelta {
    TimeDelta::days(1)
}

fn accepted(user: &Person, peer: &Person) -> Event {
    Event::ContactChanged {
        user: user.user,
        peer: peer.user,
        state: Some(ContactState::Accepted),
    }
}

/// A new invite from `person`, valid for a day.
async fn invite(world: &World, person: &Person, max_uses: u32) -> String {
    world
        .svc
        .create_invite(person.user, day(), max_uses)
        .await
        .unwrap()
        .code
}
```

and append at the end of the file:

```rust
#[tokio::test]
async fn redeeming_makes_both_users_contacts_and_tells_both() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let code = invite(&world, &alice, 1).await;
    world.notifier.take();

    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Ok(alice.user)
    );
    assert_eq!(
        world.notifier.take(),
        [accepted(&alice, &bob), accepted(&bob, &alice)]
    );
    assert!(world.svc.open_direct(bob.user, alice.user).await.is_ok());
}

#[tokio::test]
async fn a_typed_code_may_use_lower_case_dashes_and_look_alikes() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let code = invite(&world, &alice, 1).await;
    let typed = format!("{}-{}", &code[..5], &code[5..])
        .to_lowercase()
        .replace('0', "o")
        .replace('1', "l");

    assert_eq!(
        world.svc.redeem_invite(bob.user, &typed).await,
        Ok(alice.user)
    );
}

#[tokio::test]
async fn an_invite_can_be_used_only_max_uses_times() {
    let world = World::new();
    let (alice, bob, carol, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let code = invite(&world, &alice, 2).await;

    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Ok(alice.user)
    );
    assert_eq!(
        world.svc.redeem_invite(carol.user, &code).await,
        Ok(alice.user)
    );
    assert_eq!(
        world.svc.redeem_invite(dave.user, &code).await,
        Err(DomainError::InviteNotFound)
    );
}

#[tokio::test]
async fn an_invite_expires_at_its_expiry_time() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let code = invite(&world, &alice, 1).await;

    world.clock.advance(day());
    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Err(DomainError::InviteNotFound)
    );
}

#[tokio::test]
async fn a_revoked_or_unknown_invite_is_not_found() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let code = invite(&world, &alice, 1).await;
    world.svc.revoke_invite(alice.user, &code).await.unwrap();

    for code in [code.as_str(), "0000000000", "not a code"] {
        assert_eq!(
            world.svc.redeem_invite(bob.user, code).await,
            Err(DomainError::InviteNotFound)
        );
    }
}

#[tokio::test]
async fn redeeming_your_own_invite_is_invalid() {
    let world = World::new();
    let alice = world.person().await;
    let code = invite(&world, &alice, 1).await;

    assert_eq!(
        world.svc.redeem_invite(alice.user, &code).await,
        Err(DomainError::Invalid(ValidationError::SelfTarget))
    );
}

#[tokio::test]
async fn a_block_by_the_creator_looks_like_an_unknown_invite_and_uses_nothing() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let code = invite(&world, &alice, 1).await;
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    world.notifier.take();

    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Err(DomainError::InviteNotFound)
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(
        world.svc.redeem_invite(carol.user, &code).await,
        Ok(alice.user)
    );
}

#[tokio::test]
async fn a_redeemer_who_blocked_the_creator_is_told_so() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let code = invite(&world, &alice, 1).await;
    world.svc.contact_block(bob.user, alice.user).await.unwrap();

    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Err(DomainError::Blocked)
    );
}

#[tokio::test]
async fn existing_contacts_redeem_without_using_the_invite() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    world.befriend(&alice, &bob).await;
    let code = invite(&world, &alice, 1).await;
    world.notifier.take();

    assert_eq!(
        world.svc.redeem_invite(bob.user, &code).await,
        Ok(alice.user)
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(
        world.svc.redeem_invite(carol.user, &code).await,
        Ok(alice.user)
    );
}

#[tokio::test]
async fn redeeming_replaces_a_pending_request() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(bob.user, alice.user)
        .await
        .unwrap();
    let code = invite(&world, &alice, 1).await;
    world.notifier.take();

    world.svc.redeem_invite(bob.user, &code).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [accepted(&alice, &bob), accepted(&bob, &alice)]
    );
    for (me, peer) in [(&alice, &bob), (&bob, &alice)] {
        assert_eq!(
            world.svc.contact_state(me.user, peer.user).await,
            Ok(Some(ContactState::Accepted))
        );
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p push-domain --test invites`
Expected: compile error — `no method named redeem_invite found`.

- [ ] **Step 3: Share the contact helpers**

In `crates/push-domain/src/contacts.rs`, make two private helpers crate-visible: change
`async fn set_contact<T: Tx>(` to `pub(crate) async fn set_contact<T: Tx>(`, and
`fn contact_changed(` to `pub(crate) fn contact_changed(`. Their bodies do not change.

- [ ] **Step 4: Implement redeeming**

In `crates/push-domain/src/invites.rs`, replace the `use crate::…` lines with:

```rust
use crate::clock::Clock;
use crate::contacts::{are_contacts, contact_changed, set_contact};
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::{ContactState, Invite, InviteRecord};
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::{ValidationError, validate_invite};
```

and add to the `impl` block, after `revoke_invite`:

```rust
    /// Makes `redeemer` and the creator of the invite accepted contacts (spec §5.5):
    /// sharing the code is the consent. Returns the creator.
    ///
    /// An unknown, revoked, expired or used-up code is `InviteNotFound`, and so is a code
    /// whose creator has blocked the redeemer: redeeming is not an oracle and never
    /// reveals a block. A redeemer who has blocked the creator gets `Blocked`. Users who
    /// are already contacts succeed without using up the invite. Any pending request
    /// between the two is replaced by the accepted contact.
    pub async fn redeem_invite(&self, redeemer: UserId, code: &str) -> DomainResult<UserId> {
        let code = normalize_code(code).ok_or(DomainError::InviteNotFound)?;
        let now = self.clock.now();
        let mut tx = self.store.begin().await?;
        let mut invite = tx
            .lock_invite(&code)
            .await?
            .filter(|invite| invite.is_usable(now))
            .ok_or(DomainError::InviteNotFound)?;
        let creator = invite.user_id;
        if creator == redeemer {
            return Err(ValidationError::SelfTarget.into());
        }
        if !tx.user_exists(redeemer).await? {
            return Err(DomainError::UnknownUser);
        }
        tx.lock_contact_pair(creator, redeemer).await?;
        if tx.get_contact(creator, redeemer).await? == Some(ContactState::Blocked) {
            return Err(DomainError::InviteNotFound);
        }
        if tx.get_contact(redeemer, creator).await? == Some(ContactState::Blocked) {
            return Err(DomainError::Blocked);
        }
        if are_contacts(&mut tx, creator, redeemer).await? {
            tx.rollback().await?;
            return Ok(creator);
        }

        let accepted = Some(ContactState::Accepted);
        let mut events = Vec::new();
        for (owner, peer) in [(creator, redeemer), (redeemer, creator)] {
            if set_contact(&mut tx, owner, peer, accepted).await? {
                events.push(contact_changed(owner, peer, accepted));
            }
        }
        invite.uses += 1;
        tx.update_invite(invite).await?;
        tx.commit().await?;
        self.publish(events).await;
        Ok(creator)
    }
```

- [ ] **Step 5: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 194 tests pass (39 unit, 155 integration; `invites` 14), no warnings, no
formatting diff.

- [ ] **Step 6: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): redeem invites"
```

---

### Task 4: System events for group logs

**Files:**
- Modify: `Cargo.toml` (workspace: serde, serde_json, uuid `serde`)
- Modify: `crates/push-domain/Cargo.toml`
- Modify: `Cargo.lock` (by cargo)
- Modify: `crates/push-domain/src/ids.rs`
- Modify: `crates/push-domain/src/model.rs`
- Create: `crates/push-domain/src/system.rs`
- Modify: `crates/push-domain/src/lib.rs`

Spec §6: every membership change is a system entry in the log. This task defines what such
an entry records and how its body is encoded (decisions 5 and 6): compact JSON with a format
version and a type tag. Ids serialise as plain UUID strings and roles as `"admin"` and
`"member"`.

- [ ] **Step 1: Add the dependencies**

In the workspace `Cargo.toml`, under `[workspace.dependencies]`, add after the `chrono`
line:

```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

and replace the `uuid` line with:

```toml
uuid = { version = "1", features = ["serde", "v4", "v7"] }
```

In `crates/push-domain/Cargo.toml`, under `[dependencies]`, add after
`chrono.workspace = true`:

```toml
serde.workspace = true
serde_json.workspace = true
```

Run: `cargo build -p push-domain`
Expected: it downloads serde, serde_json and their dependencies, updates `Cargo.lock` and
builds. Their minimum Rust versions are at most 1.71, below our 1.85.

- [ ] **Step 2: Write the failing tests**

Create `crates/push-domain/src/system.rs` with the module doc and the tests:

```rust
//! System entries: membership and settings changes recorded in a group's log (spec §6).
//!
//! A system entry has `kind = System` and `content_type = SystemEvent`, and its body is
//! the event as compact JSON with a format version and a type tag:
//!
//! ```text
//! {"v":1,"type":"members_added","actor":"0190…","users":["0190…","0190…"]}
//! ```
//!
//! JSON, because every client language decodes it without generated code, a client can
//! show an unknown `type` as a generic line, and fields can be added without a new
//! version. The largest body, a group created with 99 other members, is about 4 KB.

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn user(n: u128) -> UserId {
        UserId(Uuid::from_u128(n))
    }

    #[test]
    fn the_body_is_compact_json_with_a_version_and_a_type() {
        let event = SystemEvent::RoleChanged {
            actor: user(1),
            user: user(2),
            role: Role::Admin,
        };
        assert_eq!(
            String::from_utf8(event.encode()).unwrap(),
            concat!(
                r#"{"v":1,"type":"role_changed","#,
                r#""actor":"00000000-0000-0000-0000-000000000001","#,
                r#""user":"00000000-0000-0000-0000-000000000002","role":"admin"}"#
            )
        );
    }

    #[test]
    fn every_event_survives_a_round_trip() {
        let events = [
            SystemEvent::Created {
                actor: user(1),
                title: "Trip \"2026\"".into(),
                users: vec![user(2), user(3)],
            },
            SystemEvent::MembersAdded {
                actor: user(1),
                users: vec![user(4)],
            },
            SystemEvent::MemberRemoved {
                actor: user(1),
                user: user(4),
            },
            SystemEvent::MemberLeft { actor: user(2) },
            SystemEvent::Renamed {
                actor: user(1),
                title: "Trip".into(),
            },
            SystemEvent::RoleChanged {
                actor: user(1),
                user: user(3),
                role: Role::Member,
            },
            SystemEvent::AdminPromoted {
                actor: user(1),
                user: user(3),
            },
        ];
        for event in events {
            assert_eq!(SystemEvent::decode(&event.encode()), Some(event));
        }
    }

    #[test]
    fn other_versions_types_and_bodies_are_not_decoded() {
        let actor = r#""actor":"00000000-0000-0000-0000-000000000001""#;
        for body in [
            format!(r#"{{"v":2,"type":"member_left",{actor}}}"#),
            format!(r#"{{"v":1,"type":"pinned",{actor}}}"#),
            "hello".to_string(),
        ] {
            assert_eq!(SystemEvent::decode(body.as_bytes()), None);
        }
    }
}
```

In `crates/push-domain/src/lib.rs`, add `pub mod system;` after `mod sync;`.

Run: `cargo test -p push-domain --lib system`
Expected: compile errors — `cannot find type UserId`, `failed to resolve: use of undeclared
type SystemEvent`.

- [ ] **Step 3: Make ids and roles serialisable**

In `crates/push-domain/src/ids.rs`, replace the module doc and the imports with:

```rust
//! Strongly typed identifiers. All are UUIDs; new ones are v7 (time-ordered), which keeps
//! database indexes compact. They serialise as plain UUID strings.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
```

and, inside the `id_type!` macro, replace
`#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]` with:

```rust
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
```

In `crates/push-domain/src/model.rs`, add `use serde::{Deserialize, Serialize};` after
`use chrono::{DateTime, Utc};`, and replace the derive line of `enum Role` with:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
```

- [ ] **Step 4: Implement the events and their encoding**

In `crates/push-domain/src/system.rs`, insert between the module doc and `#[cfg(test)]`:

```rust
use serde::{Deserialize, Serialize};

use crate::ids::UserId;
use crate::model::Role;

/// The format version written into every system entry body.
pub const SYSTEM_EVENT_VERSION: u32 = 1;

/// What a system entry records. `actor` is the user whose action caused it; it is also
/// the entry's `sender_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemEvent {
    /// `actor` created the group titled `title` with `users` as its other first members.
    Created {
        actor: UserId,
        title: String,
        users: Vec<UserId>,
    },
    /// `actor` added `users`.
    MembersAdded { actor: UserId, users: Vec<UserId> },
    /// `actor` removed `user`.
    MemberRemoved { actor: UserId, user: UserId },
    /// `actor` left.
    MemberLeft { actor: UserId },
    /// `actor` renamed the group to `title`.
    Renamed { actor: UserId, title: String },
    /// `actor` gave `user` the role `role`.
    RoleChanged {
        actor: UserId,
        user: UserId,
        role: Role,
    },
    /// No admin remained after `actor` left, so `user` became an admin automatically.
    AdminPromoted { actor: UserId, user: UserId },
}

#[derive(Serialize, Deserialize)]
struct Envelope<E> {
    v: u32,
    #[serde(flatten)]
    event: E,
}

impl SystemEvent {
    /// The user whose action caused the event.
    pub fn actor(&self) -> UserId {
        match self {
            Self::Created { actor, .. }
            | Self::MembersAdded { actor, .. }
            | Self::MemberRemoved { actor, .. }
            | Self::MemberLeft { actor }
            | Self::Renamed { actor, .. }
            | Self::RoleChanged { actor, .. }
            | Self::AdminPromoted { actor, .. } => *actor,
        }
    }

    /// The entry body: compact JSON.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(&Envelope {
            v: SYSTEM_EVENT_VERSION,
            event: self,
        })
        .expect("a system event always serialises")
    }

    /// Reads an entry body. `None` for another format version, an unknown type or a body
    /// that is not a system event.
    pub fn decode(body: &[u8]) -> Option<Self> {
        let envelope: Envelope<Self> = serde_json::from_slice(body).ok()?;
        (envelope.v == SYSTEM_EVENT_VERSION).then_some(envelope.event)
    }
}

```

In `crates/push-domain/src/lib.rs`, add after `pub use sync::SyncBatch;`:

```rust
pub use system::{SYSTEM_EVENT_VERSION, SystemEvent};
```

Run: `cargo test -p push-domain --lib system`
Expected: `3 passed`.

- [ ] **Step 5: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 197 tests pass (42 unit, 155 integration), no warnings, no formatting diff.

- [ ] **Step 6: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/push-domain
git commit -m "feat(domain): system events for group logs"
```

---

### Task 5: Create groups

**Files:**
- Modify: `crates/push-domain/src/validation.rs`
- Modify: `crates/push-domain/src/error.rs`
- Create: `crates/push-domain/src/groups.rs`
- Modify: `crates/push-domain/src/lib.rs`
- Modify: `crates/push-domain/tests/common/mod.rs`
- Create: `crates/push-domain/tests/groups.rs`

Spec §6: `CreateGroup{title, member_ids[]}` — every member must be an accepted contact of
the creator, and the creator becomes an admin. The log starts with a `Created` system entry
at `seq` 1; every member sees the group from there and every active device of every member
starts at 0 (decision 7). Group sends need current membership only: there is no contact
check in a group, so blocks between members do not affect it (`prepare_append` already
checks contacts for direct conversations only).

- [ ] **Step 1: Write the failing title tests**

In `crates/push-domain/src/validation.rs`, append inside `mod tests`:

```rust
    #[test]
    fn a_title_is_trimmed_and_may_have_exactly_the_limit() {
        assert_eq!(validate_title("  Trip  "), Ok("Trip".to_string()));
        let longest = "é".repeat(MAX_TITLE_CHARS);
        assert_eq!(validate_title(&longest), Ok(longest.clone()));
    }

    #[test]
    fn rejects_empty_and_long_titles() {
        assert_eq!(validate_title(" \t "), Err(ValidationError::EmptyTitle));
        assert_eq!(
            validate_title(&"a".repeat(MAX_TITLE_CHARS + 1)),
            Err(ValidationError::TitleTooLong {
                actual: MAX_TITLE_CHARS + 1,
                limit: MAX_TITLE_CHARS,
            })
        );
    }
```

Run: `cargo test -p push-domain --lib validation`
Expected: compile errors — `cannot find function validate_title`, `cannot find value
MAX_TITLE_CHARS`.

- [ ] **Step 2: Implement the title rule and the group limit**

In `crates/push-domain/src/validation.rs`, add after `MAX_INVITE_LIFETIME_SECS`:

```rust
/// Longest group title, in characters, after trimming.
pub const MAX_TITLE_CHARS: usize = 100;
/// Most members a group can have, admins included.
pub const MAX_GROUP_MEMBERS: usize = 100;
```

add two variants at the end of `ValidationError`, after `InviteLifetime`:

```rust
    #[error("a group title cannot be empty")]
    EmptyTitle,
    #[error("a group title is {actual} characters; the limit is {limit}")]
    TitleTooLong { actual: usize, limit: usize },
```

and add after `validate_invite`:

```rust
/// Checks a group title and returns it without leading and trailing whitespace. It must
/// keep 1 to `MAX_TITLE_CHARS` characters.
pub fn validate_title(title: &str) -> Result<String, ValidationError> {
    let title = title.trim();
    let chars = title.chars().count();
    if chars == 0 {
        return Err(ValidationError::EmptyTitle);
    }
    if chars > MAX_TITLE_CHARS {
        return Err(ValidationError::TitleTooLong {
            actual: chars,
            limit: MAX_TITLE_CHARS,
        });
    }
    Ok(title.to_string())
}
```

In `crates/push-domain/src/lib.rs`, replace the `pub use validation::{…};` block with:

```rust
pub use validation::{
    MAX_GROUP_MEMBERS, MAX_HISTORY_PAGE, MAX_INVITE_LIFETIME_SECS, MAX_INVITE_USES, MAX_SYNC_BATCH,
    MAX_TEXT_BYTES, MAX_TITLE_CHARS, MAX_UNREAD, MIN_INVITE_LIFETIME_SECS, ValidationError,
    validate_invite, validate_text, validate_title,
};
```

Run: `cargo test -p push-domain --lib validation`
Expected: `9 passed`.

- [ ] **Step 3: Add the test helpers**

In `crates/push-domain/tests/common/mod.rs`, replace the `use push_domain::{…};` block with:

```rust
use push_domain::{
    ClientMsgId, Conversation, ConversationId, DeleteMessage, DeviceId, EditText, Entry, Event,
    InMemoryStore, ManualClock, Member, MessageRef, Messaging, Policy, RecordingNotifier,
    SendReceipt, SendText, Store, SystemEvent, Tx, UserId,
};
```

add to `impl World`, after `conversation`:

```rust
    /// A member row, read straight from the store.
    pub async fn member(&self, conversation: ConversationId, person: &Person) -> Option<Member> {
        let mut tx = self.store.begin().await.unwrap();
        let member = tx.get_member(conversation, person.user).await.unwrap();
        tx.rollback().await.unwrap();
        member
    }
```

add to `impl World`, after `direct_chat`:

```rust
    /// Makes `admin` a contact of every other member and creates a group titled "Group".
    pub async fn group(&self, admin: &Person, members: &[&Person]) -> ConversationId {
        for member in members {
            self.befriend(admin, member).await;
        }
        let ids: Vec<UserId> = members.iter().map(|member| member.user).collect();
        self.svc
            .create_group(admin.user, "Group", &ids)
            .await
            .unwrap()
    }
```

and add after the free function `seqs`:

```rust
/// The system events recorded by these entries; `None` for other entries.
pub fn system_events(entries: &[Entry]) -> Vec<Option<SystemEvent>> {
    entries
        .iter()
        .map(|entry| SystemEvent::decode(entry.body.as_deref().unwrap_or_default()))
        .collect()
}
```

- [ ] **Step 4: Write the failing group tests**

Create `crates/push-domain/tests/groups.rs`:

```rust
mod common;

use common::{World, entries_for, seqs, system_events, text};
use push_domain::{
    Clock, ContentType, DomainError, EntryKind, MAX_GROUP_MEMBERS, Role, SystemEvent, UserId,
    ValidationError,
};

#[tokio::test]
async fn a_new_group_starts_with_a_created_entry_seen_by_every_member() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    world.befriend(&alice, &bob).await;
    world.befriend(&alice, &carol).await;
    world.notifier.take();

    let group = world
        .svc
        .create_group(alice.user, "  Trip  ", &[carol.user, bob.user])
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
    let mut others = vec![bob.user, carol.user];
    others.sort();
    for person in [&alice, &bob, &carol] {
        let batch = world.svc.sync(person.device, 100).await.unwrap();
        assert_eq!(seqs(&batch.entries), [1]);
        let entry = &batch.entries[0];
        assert_eq!(
            (entry.kind, entry.content_type, entry.sender_id),
            (EntryKind::System, ContentType::SystemEvent, alice.user)
        );
        assert_eq!(entry.sender_device_id, None);
        assert_eq!(
            system_events(&batch.entries),
            [Some(SystemEvent::Created {
                actor: alice.user,
                title: "Trip".into(),
                users: others.clone(),
            })]
        );
    }
    let stored = world.conversation(group).await;
    assert_eq!(stored.title.as_deref(), Some("Trip"));
    assert_eq!((stored.last_seq, stored.updated_at), (1, world.clock.now()));
}

#[tokio::test]
async fn the_creator_is_the_admin_and_everyone_sees_the_group_from_its_start() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let tablet = world.add_device(&bob).await;
    let group = world.group(&alice, &[&bob]).await;

    let admin = world.member(group, &alice).await.unwrap();
    let member = world.member(group, &bob).await.unwrap();
    assert_eq!((admin.role, member.role), (Role::Admin, Role::Member));
    for row in [admin, member] {
        assert_eq!(
            (row.visible_from_seq, row.left_at_seq, row.read_seq),
            (1, None, 0)
        );
    }
    for device in [alice.device, bob.device, tablet] {
        assert_eq!(world.position(device, group).await, Some(0));
    }
}

#[tokio::test]
async fn duplicates_and_the_creator_in_the_member_list_are_ignored() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;

    let group = world
        .svc
        .create_group(alice.user, "Pair", &[bob.user, alice.user, bob.user])
        .await
        .unwrap();
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(
        system_events(&batch.entries),
        [Some(SystemEvent::Created {
            actor: alice.user,
            title: "Pair".into(),
            users: vec![bob.user],
        })]
    );
    assert!(world.member(group, &bob).await.is_some());
}

#[tokio::test]
async fn a_group_title_must_not_be_blank() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(
        world.svc.create_group(alice.user, "   ", &[]).await,
        Err(DomainError::Invalid(ValidationError::EmptyTitle))
    );
}

#[tokio::test]
async fn every_member_must_be_an_accepted_contact_of_the_creator() {
    let world = World::new();
    let (alice, bob, carol) = (
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

    assert_eq!(
        world
            .svc
            .create_group(alice.user, "Trip", &[bob.user, carol.user])
            .await,
        Err(DomainError::NotContact)
    );
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
async fn a_group_holds_at_most_a_hundred_members() {
    let world = World::new();
    let alice = world.person().await;
    let mut friends = Vec::new();
    for _ in 1..MAX_GROUP_MEMBERS {
        let friend = world.person().await;
        world.befriend(&alice, &friend).await;
        friends.push(friend.user);
    }

    assert!(
        world
            .svc
            .create_group(alice.user, "Full", &friends)
            .await
            .is_ok()
    );
    friends.push(UserId::new());
    assert_eq!(
        world
            .svc
            .create_group(alice.user, "Too many", &friends)
            .await,
        Err(DomainError::GroupFull)
    );
}

#[tokio::test]
async fn members_chat_without_being_contacts_and_blocks_do_not_matter() {
    let world = World::new();
    let (alice, bob, carol, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.svc.contact_block(bob.user, carol.user).await.unwrap();

    world.send(&carol, group, "hi bob").await;
    world.send(&bob, group, "hi carol").await;
    assert_eq!(
        world.svc.send_text(text(&dave, group, "let me in")).await,
        Err(DomainError::NotMember)
    );
    let history = world.svc.history(dave.user, group, None, 10).await;
    assert_eq!(history, Err(DomainError::NotMember));
}
```

Run: `cargo test -p push-domain --test groups`
Expected: compile errors — `no method named create_group`, `no variant named GroupFull`.

- [ ] **Step 5: Implement creating a group**

In `crates/push-domain/src/error.rs`, add a variant after `InviteNotFound`:

```rust
    #[error("a group has at most 100 members")]
    GroupFull,
```

Create `crates/push-domain/src/groups.rs`:

```rust
//! Groups of up to 100 members with admin and member roles (spec §6).

use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, MessageId, UserId};
use crate::model::{ContentType, Conversation, ConversationKind, Entry, EntryKind, Member, Role};
use crate::notify::{Event, Notifier};
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::system::SystemEvent;
use crate::validation::{MAX_GROUP_MEMBERS, validate_title};

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Creates a group titled `title` (trimmed) with `creator` as its admin and `members`
    /// as members. Every other member must be an accepted contact of the creator.
    /// Duplicates, and the creator in `members`, are ignored. A group has at most
    /// `MAX_GROUP_MEMBERS` members, the creator included.
    ///
    /// The log starts with a `Created` system entry at `seq` 1. Every member sees the group
    /// from there, and every active device of every member starts just before it.
    pub async fn create_group(
        &self,
        creator: UserId,
        title: &str,
        members: &[UserId],
    ) -> DomainResult<ConversationId> {
        let title = validate_title(title)?;
        let mut others: Vec<UserId> = members
            .iter()
            .copied()
            .filter(|member| *member != creator)
            .collect();
        others.sort();
        others.dedup();
        if others.len() + 1 > MAX_GROUP_MEMBERS {
            return Err(DomainError::GroupFull);
        }

        let mut tx = self.store.begin().await?;
        if !tx.user_exists(creator).await? {
            return Err(DomainError::UnknownUser);
        }
        for other in &others {
            if !are_contacts(&mut tx, creator, *other).await? {
                return Err(DomainError::NotContact);
            }
        }
        let now = self.clock.now();
        let conversation = Conversation {
            id: ConversationId::new(),
            kind: ConversationKind::Group,
            direct_key: None,
            title: Some(title.clone()),
            created_by: creator,
            last_seq: 0,
            created_at: now,
            updated_at: now,
        };
        let id = conversation.id;
        // A group has no direct key, so the insert cannot be a duplicate.
        tx.insert_conversation(conversation).await?;
        let seq = tx.next_seq(id, now).await?;
        let created = SystemEvent::Created {
            actor: creator,
            title,
            users: others.clone(),
        };
        tx.insert_entry(system_entry(id, seq, &created, now))
            .await?;
        join(&mut tx, id, creator, Role::Admin, seq).await?;
        for other in &others {
            join(&mut tx, id, *other, Role::Member, seq).await?;
        }
        tx.commit().await?;

        let mut users = others;
        users.push(creator);
        users.sort();
        self.publish(vec![Event::EntriesAvailable { users }]).await;
        Ok(id)
    }
}

/// Makes `user` a current member who sees the log from `seq`, the entry that let them in.
/// Nothing before it is visible or unread: the read position starts at `seq - 1`, and so
/// does every active device's delivery position. A device that already has a position (a
/// member rejoining) keeps it; readers never look below `visible_from_seq - 1`.
async fn join<T: Tx>(
    tx: &mut T,
    conversation: ConversationId,
    user: UserId,
    role: Role,
    seq: i64,
) -> DomainResult<()> {
    tx.insert_member(Member {
        conversation_id: conversation,
        user_id: user,
        role,
        visible_from_seq: seq,
        left_at_seq: None,
        read_seq: seq - 1,
    })
    .await?;
    for device in tx.devices_of_user(user).await? {
        tx.create_cursor_if_absent(device, conversation, seq - 1)
            .await?;
    }
    Ok(())
}

/// The log entry recording `event` at `seq`. Its sender is the event's actor; no device
/// sent it.
fn system_entry(
    conversation: ConversationId,
    seq: i64,
    event: &SystemEvent,
    now: DateTime<Utc>,
) -> Entry {
    Entry {
        conversation_id: conversation,
        seq,
        message_id: MessageId::new(),
        client_msg_id: ClientMsgId::new(),
        kind: EntryKind::System,
        target_seq: None,
        sender_id: event.actor(),
        sender_device_id: None,
        content_type: ContentType::SystemEvent,
        body: Some(event.encode()),
        client_ts: None,
        server_ts: now,
        edited_at: None,
        edit_seq: None,
        deleted_at: None,
    }
}
```

In `crates/push-domain/src/lib.rs`, add `mod groups;` after `pub mod error;`.

- [ ] **Step 6: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 206 tests pass (44 unit, 162 integration; `groups` 7), no warnings, no
formatting diff.

- [ ] **Step 7: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): create groups"
```

---

### Task 6: Group receipts

**Files:**
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/memory.rs`
- Modify: `crates/push-domain/src/notify.rs`
- Modify: `crates/push-domain/src/read.rs`
- Modify: `crates/push-domain/src/sync.rs`
- Modify: `crates/push-domain/src/send.rs`
- Modify: `crates/push-domain/src/edit.rs`
- Create: `crates/push-domain/tests/group_receipts.rs`

Spec §4.4–4.5 for groups (decisions 8, 9 and 10): only the group-wide minimums over the
current members are pushed, as `ReceiptUpdate::Group { all_delivered_seq, all_read_seq }`.
A member's effective position skips their own messages, so "every message up to N has been
read by every member except its sender". One new `Tx` method finds the first live message
after a position, which serves both the effective positions and the "did it move over a
real message" check. Every path that moves a position or appends an entry computes the
positions before and after, inside its transaction.

- [ ] **Step 1: Write the failing storage test**

In `crates/push-domain/src/memory.rs`, append inside `mod tests`:

```rust
    #[tokio::test]
    async fn the_first_live_message_skips_changes_deleted_messages_and_the_excluded_sender() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let me = UserId::new();
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let entries = [
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
            entry(id, 5, ClientMsgId::new()),
        ];
        for entry in entries {
            tx.insert_entry(entry).await.unwrap();
        }

        assert_eq!(tx.first_live_message(id, 0, 5, Some(me)).await, Ok(Some(1)));
        assert_eq!(tx.first_live_message(id, 1, 5, Some(me)).await, Ok(Some(5)));
        assert_eq!(tx.first_live_message(id, 1, 5, None).await, Ok(Some(2)));
        assert_eq!(tx.first_live_message(id, 2, 4, None).await, Ok(None));
        assert_eq!(tx.first_live_message(id, 5, 5, None).await, Ok(None));
    }
```

Run: `cargo test -p push-domain --lib memory`
Expected: compile error — `no method named first_live_message`.

- [ ] **Step 2: Add `first_live_message`**

In `crates/push-domain/src/store.rs`, add to `trait Tx` after `count_incoming_messages`:

```rust
    /// The lowest `seq` with `after < seq <= up_to` of a message (`kind = Message`) that is
    /// not deleted and, if `not_from` is given, was sent by someone else. `None` if there is
    /// none. On Postgres one range scan of the primary key: `… ORDER BY seq LIMIT 1`.
    async fn first_live_message(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        not_from: Option<UserId>,
    ) -> DomainResult<Option<i64>>;
```

In `crates/push-domain/src/memory.rs`, add to `impl Tx for InMemoryTx` after
`count_incoming_messages`:

```rust
    async fn first_live_message(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        not_from: Option<UserId>,
    ) -> DomainResult<Option<i64>> {
        if after >= up_to {
            return Ok(None);
        }
        Ok(self
            .work
            .entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .map(|(_, entry)| entry)
            .find(|entry| {
                entry.kind == EntryKind::Message
                    && entry.deleted_at.is_none()
                    && not_from.is_none_or(|user| entry.sender_id != user)
            })
            .map(|entry| entry.seq))
    }
```

Run: `cargo test -p push-domain --lib memory`
Expected: `25 passed`.

- [ ] **Step 3: Write the failing receipt tests**

Create `crates/push-domain/tests/group_receipts.rs`:

```rust
mod common;

use common::{Person, World, delete, entries_for};
use push_domain::{ConversationId, Event, MessageRef, ReceiptUpdate};

fn group_receipt(
    conversation: ConversationId,
    to: &[&Person],
    all_delivered_seq: i64,
    all_read_seq: i64,
) -> Event {
    let mut users: Vec<_> = to.iter().map(|person| person.user).collect();
    users.sort();
    Event::Receipt {
        conversation,
        to: users,
        receipt: ReceiptUpdate::Group {
            all_delivered_seq,
            all_read_seq,
        },
    }
}

fn read_moved(person: &Person, conversation: ConversationId, read_seq: i64) -> Event {
    Event::ReadPositionChanged {
        user: person.user,
        conversation,
        read_seq,
    }
}

/// A group of alice (admin), bob and carol; `seq` 1 is its `Created` entry.
async fn trio() -> (World, Person, Person, Person, ConversationId) {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.notifier.take();
    (world, alice, bob, carol, group)
}

#[tokio::test]
async fn a_group_receipt_waits_for_every_other_member() {
    let (world, alice, bob, carol, group) = trio().await;
    world.send(&alice, group, "hi").await;
    world.notifier.take();
    let everyone = [&alice, &bob, &carol];

    world.svc.ack(bob.device, group, 2).await.unwrap();
    assert!(world.notifier.take().is_empty());
    world.svc.ack(carol.device, group, 2).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [group_receipt(group, &everyone, 2, 1)]
    );

    world.svc.read_up_to(bob.user, group, 2).await.unwrap();
    assert_eq!(world.notifier.take(), [read_moved(&bob, group, 2)]);
    world.svc.read_up_to(carol.user, group, 2).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [
            read_moved(&carol, group, 2),
            group_receipt(group, &everyone, 2, 2)
        ]
    );
}

#[tokio::test]
async fn ones_own_messages_never_hold_the_group_back() {
    let (world, alice, bob, carol, group) = trio().await;
    world.send(&alice, group, "one").await;
    world.send(&bob, group, "two").await;
    world.svc.read_up_to(carol.user, group, 3).await.unwrap();
    world.svc.read_up_to(alice.user, group, 3).await.unwrap();
    world.notifier.take();

    world.svc.read_up_to(bob.user, group, 3).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [
            read_moved(&bob, group, 3),
            group_receipt(group, &[&alice, &bob, &carol], 3, 3)
        ]
    );
}

#[tokio::test]
async fn replying_in_a_group_can_complete_a_read() {
    let (world, alice, bob, carol, group) = trio().await;
    world.send(&alice, group, "one").await;
    world.svc.ack(bob.device, group, 2).await.unwrap();
    world.svc.read_up_to(bob.user, group, 2).await.unwrap();
    world.svc.ack(carol.device, group, 2).await.unwrap();
    world.notifier.take();

    world.send(&carol, group, "reply").await;
    let everyone = [&alice, &bob, &carol];
    assert_eq!(
        world.notifier.take(),
        [
            entries_for(&everyone),
            read_moved(&carol, group, 2),
            group_receipt(group, &everyone, 2, 2)
        ]
    );
}

#[tokio::test]
async fn edits_send_no_group_receipt() {
    let (world, alice, bob, carol, group) = trio().await;
    let hello = world.send(&alice, group, "hello").await;
    world.svc.read_up_to(bob.user, group, 2).await.unwrap();
    world.svc.read_up_to(carol.user, group, 2).await.unwrap();
    world.notifier.take();

    world.edit(&alice, group, &hello, "hello!").await;
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
}

#[tokio::test]
async fn deleting_the_message_someone_had_not_read_can_complete_a_read() {
    let (world, alice, bob, carol, group) = trio().await;
    let unread = world.send(&alice, group, "one").await;
    world.send(&carol, group, "two").await;
    world.svc.read_up_to(alice.user, group, 3).await.unwrap();
    world.svc.read_up_to(bob.user, group, 3).await.unwrap();
    world.notifier.take();

    world
        .svc
        .delete_message(delete(&alice, group, MessageRef::Id(unread.message_id)))
        .await
        .unwrap();
    let everyone = [&alice, &bob, &carol];
    assert_eq!(
        world.notifier.take(),
        [
            entries_for(&everyone),
            group_receipt(group, &everyone, 4, 4)
        ]
    );
}
```

How the numbers come about, for the first test: after alice's message (`seq` 2), alice is
at 2 (nothing from others), bob and carol at 1 (message 2 unread and undelivered). Bob's
ack moves only bob: the minimum stays 1. Carol's ack moves the delivered minimum to 2 over
message 2, so a receipt `(2, 1)` goes out. In the last test, carol's unread message 2 is
deleted; her own message 3 no longer holds anyone back, so both minimums jump from 1 to 4
over the live message 3.

Run: `cargo test -p push-domain --test group_receipts`
Expected: compile error — `no variant named Group found for enum ReceiptUpdate`.

- [ ] **Step 4: Add `ReceiptUpdate::Group`**

In `crates/push-domain/src/notify.rs`, replace the doc comment of `enum ReceiptUpdate`
(`/// The positions a receipt reports.` and the two lines about groups) with:

```rust
/// The positions a receipt reports.
///
/// Match this enum with a wildcard arm: later versions may report positions of other
/// kinds.
```

and add a variant after `Direct`:

```rust
    /// A group: every live message up to `all_read_seq` has been read, and every one up to
    /// `all_delivered_seq` delivered, by every current member other than its sender. Never
    /// per-member positions. `all_delivered_seq` is never below `all_read_seq`.
    ///
    /// The transport sends at most one of these per second per conversation, keeping the
    /// latest (spec §4.5); the domain publishes one for every change.
    Group {
        all_delivered_seq: i64,
        all_read_seq: i64,
    },
```

- [ ] **Step 5: Compute the group-wide positions**

In `crates/push-domain/src/read.rs`, replace
`use crate::model::{Conversation, ConversationKind, ConversationState, MemberState};` with:

```rust
use crate::model::{Conversation, ConversationKind, ConversationState, Member, MemberState};
```

and append at the end of the file:

```rust
/// Group-wide positions (spec §4.5): every live message up to `read` has been read, and
/// every one up to `delivered` delivered, by every current member other than its sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GroupPositions {
    pub(crate) delivered: i64,
    pub(crate) read: i64,
}

/// The group-wide positions of `conversation` with its log ending at `last_seq`, the
/// minimum over its current members of their effective positions. `None` for a direct
/// conversation or a group without members.
///
/// A member's effective position skips their own messages, edits, deletes and system
/// entries: it lies just before the first live message from someone else after their
/// position, within their visible range, or at the end of that range if there is none.
/// The delivered position is at least the read one, since reading implies delivery.
pub(crate) async fn group_positions<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    last_seq: i64,
) -> DomainResult<Option<GroupPositions>> {
    if conversation.kind != ConversationKind::Group {
        return Ok(None);
    }
    let mut all: Option<GroupPositions> = None;
    for member in tx.members(conversation.id).await? {
        if !member.is_current() {
            continue;
        }
        let read = effective_position(tx, &member, last_seq, member.read_seq).await?;
        let delivered_seq = tx.delivered_seq(conversation.id, member.user_id).await?;
        let delivered = if delivered_seq > member.read_seq {
            effective_position(tx, &member, last_seq, delivered_seq)
                .await?
                .max(read)
        } else {
            read
        };
        all = Some(match all {
            None => GroupPositions { delivered, read },
            Some(other) => GroupPositions {
                delivered: other.delivered.min(delivered),
                read: other.read.min(read),
            },
        });
    }
    Ok(all)
}

/// How far `member`, at `position`, has effectively got: see [`group_positions`].
async fn effective_position<T: Tx>(
    tx: &mut T,
    member: &Member,
    last_seq: i64,
    position: i64,
) -> DomainResult<i64> {
    let upper = member.visible_upper(last_seq);
    let from = position.max(member.visible_from_seq - 1);
    if from >= upper {
        return Ok(upper);
    }
    let first = tx
        .first_live_message(member.conversation_id, from, upper, Some(member.user_id))
        .await?;
    Ok(first.map_or(upper, |seq| seq - 1))
}

/// The receipt for a group's current members after a change, if its group-wide positions
/// moved from `before` over at least one live message: moving only over edits, deletes or
/// system entries tells nobody anything (spec §4.5). `None` for a direct conversation.
///
/// Reads the positions as this transaction sees them, so call it after the change, with
/// the log's new end as `last_seq`.
pub(crate) async fn group_receipt<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    last_seq: i64,
    before: Option<GroupPositions>,
) -> DomainResult<Option<Event>> {
    let Some(before) = before else {
        return Ok(None);
    };
    let Some(after) = group_positions(tx, conversation, last_seq).await? else {
        return Ok(None);
    };
    let mut news = false;
    for (from, to) in [
        (before.delivered, after.delivered),
        (before.read, after.read),
    ] {
        if !news && to > from {
            news = tx
                .first_live_message(conversation.id, from, to, None)
                .await?
                .is_some();
        }
    }
    if !news {
        return Ok(None);
    }
    let members = tx.members(conversation.id).await?;
    Ok(Some(Event::Receipt {
        conversation: conversation.id,
        to: current_member_ids(&members),
        receipt: ReceiptUpdate::Group {
            all_delivered_seq: after.delivered,
            all_read_seq: after.read,
        },
    }))
}
```

- [ ] **Step 6: Publish them from `read_up_to` and `ack`**

In `crates/push-domain/src/read.rs`, replace `read_up_to` with:

```rust
    /// Records that `user` has read everything up to `seq`. Returns whether the position
    /// moved; it never moves backwards. When it moves, the user's devices learn the new
    /// position, the other member of a direct conversation gets a read receipt if the move
    /// passed over a message from them, and the members of a group get its new group-wide
    /// positions if they moved.
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
        let group_before = group_positions(&mut tx, &found, found.last_seq).await?;
        let moved = tx.raise_read_seq(conversation, user, seq).await?;
        // Messages below the member's floor were never visible to them, so they are never
        // reported as read.
        let read_before = member.read_seq.max(member.visible_from_seq - 1);
        let receipt = direct_receipt(&mut tx, &found, user, read_before, seq).await?;
        let group = group_receipt(&mut tx, &found, found.last_seq, group_before).await?;
        tx.commit().await?;

        let mut events = Vec::new();
        if moved {
            events.push(Event::ReadPositionChanged {
                user,
                conversation,
                read_seq: seq,
            });
        }
        events.extend(receipt);
        events.extend(group);
        self.publish(events).await;
        Ok(moved)
    }
```

In `crates/push-domain/src/sync.rs`, replace `use crate::read::direct_receipt;` with:

```rust
use crate::read::{direct_receipt, group_positions, group_receipt};
```

and replace `ack` with:

```rust
    /// Records that the device holds everything up to `seq`. Returns whether the
    /// position moved; it never moves backwards. If the user's delivered position moved
    /// over a message from the other member of a direct conversation, they get a receipt;
    /// if a group's group-wide positions moved, its members get them.
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
        let group_before = group_positions(&mut tx, &found, found.last_seq).await?;
        // Count only from the member's floor: a rejoining member's old positions lie below
        // it.
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
        let group = group_receipt(&mut tx, &found, found.last_seq, group_before).await?;
        tx.commit().await?;
        self.publish(receipt.into_iter().chain(group).collect())
            .await;
        Ok(moved)
    }
```

- [ ] **Step 7: Compute "before" under the append lock**

In `crates/push-domain/src/send.rs`, replace `use crate::read::direct_receipt;` with:

```rust
use crate::read::{GroupPositions, direct_receipt, group_positions, group_receipt};
```

replace `struct Append` and `enum Prepared` with:

```rust
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
    /// A group's group-wide positions before this entry; `None` for a direct conversation.
    pub(crate) group_before: Option<GroupPositions>,
}

pub(crate) enum Prepared<T> {
    /// The `client_msg_id` is already stored: this is its original receipt.
    Retry(SendReceipt),
    Ready(Box<Append<T>>),
}
```

and replace `prepare_append` with:

```rust
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
        let group_before = group_positions(&mut tx, &conversation, seq - 1).await?;
        Ok(Prepared::Ready(Box::new(Append {
            tx,
            conversation,
            members,
            member,
            seq,
            now,
            group_before,
        })))
    }
```

`Append` now holds the positions, so `Prepared::Ready` boxes it: without the box,
`clippy::large_enum_variant` fails the build.

- [ ] **Step 8: Publish them from sends, edits and deletes**

In `crates/push-domain/src/send.rs`, replace `send_text` with:

```rust
    /// Appends a text message. A retry with the same `client_msg_id` returns the original
    /// receipt, stores nothing new and publishes nothing — even if the sender has since lost
    /// the right to send.
    ///
    /// Advances the sender's read position to what the sending device has already received
    /// (its delivery position): replying marks as read everything the user could have seen,
    /// never messages their device has not received yet. When it moves, the sender's
    /// devices learn the new position, and if it passes over a message from the other
    /// member of a direct conversation, they get a read receipt. A group's members get its
    /// group-wide positions if they moved.
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
            group_before,
        } = match prepared {
            Prepared::Retry(receipt) => return Ok(receipt),
            Prepared::Ready(append) => *append,
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
        let device_position = tx
            .get_cursor(command.device, conversation.id)
            .await?
            .unwrap_or(0);
        let read_moved = tx
            .raise_read_seq(conversation.id, command.sender, device_position)
            .await?;
        let read_receipt = direct_receipt(
            &mut tx,
            &conversation,
            command.sender,
            member.read_seq.max(member.visible_from_seq - 1),
            device_position,
        )
        .await?;
        let group = group_receipt(&mut tx, &conversation, seq, group_before).await?;
        tx.commit().await?;

        let mut events = vec![Event::EntriesAvailable {
            users: current_member_ids(&members),
        }];
        if read_moved {
            events.push(Event::ReadPositionChanged {
                user: command.sender,
                conversation: conversation.id,
                read_seq: device_position,
            });
        }
        events.extend(read_receipt);
        events.extend(group);
        self.publish(events).await;
        Ok(receipt)
    }
```

In `crates/push-domain/src/edit.rs`, add `use crate::read::group_receipt;` after
`use crate::notify::{Event, Notifier};`. In the doc comment of `delete_message`, replace
its last line (the one ending in ``fails with `AlreadyDeleted`.``) with:

```rust
    /// delete of a deleted message fails with `AlreadyDeleted`. If the deleted message held
    /// a group's group-wide positions back, its members get the new ones.
```

and replace `change_message` with:

```rust
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
            group_before,
        } = match prepared {
            Prepared::Retry(receipt) => return Ok(receipt),
            Prepared::Ready(append) => *append,
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
        let group = group_receipt(&mut tx, &conversation, seq, group_before).await?;
        tx.commit().await?;

        let mut events = vec![Event::EntriesAvailable {
            users: current_member_ids(&members),
        }];
        events.extend(group);
        self.publish(events).await;
        Ok(receipt)
    }
```

- [ ] **Step 9: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 212 tests pass (45 unit, 167 integration; `group_receipts` 5), no warnings, no
formatting diff.

- [ ] **Step 10: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): group receipts"
```

---

### Task 7: Group receipts in listings and conversation state

**Files:**
- Modify: `crates/push-domain/src/model.rs`
- Modify: `crates/push-domain/src/read.rs`
- Modify: `crates/push-domain/src/conversations.rs`
- Modify: `crates/push-domain/src/lib.rs`
- Modify: `crates/push-domain/tests/listings.rs`
- Modify: `crates/push-domain/tests/read_state.rs`
- Modify: `crates/push-domain/tests/direct.rs`
- Modify: `crates/push-domain/tests/edit.rs`

A reconnecting client refreshes its ticks with one `list_conversations` (spec §4.5), so a
group's summary must carry the group-wide positions, never per-member ones (decisions 11
and 12). `peer_delivered_seq` and `peer_read_seq` become `receipts: Receipts`; the summary
also lists the group's current admins. `conversation_state` returns `Positions`: both
members' positions in a direct conversation, only the group-wide positions in a group.

- [ ] **Step 1: Update the tests to the new shapes and add the group cases**

In `crates/push-domain/tests/listings.rs`, replace the `use push_domain::{…};` block with:

```rust
use push_domain::{
    Clock, ConversationId, ConversationKind, ConversationSummary, MAX_UNREAD, MessageRef, Receipts,
    UserId,
};
```

add after the `sorted` helper:

```rust
fn peer(peer_delivered_seq: i64, peer_read_seq: i64) -> Receipts {
    Receipts::Direct {
        peer_delivered_seq,
        peer_read_seq,
    }
}
```

replace the tests `a_new_direct_conversation_is_listed_for_both_members`,
`peer_positions_are_zero_until_the_peer_acks_or_reads` and
`peer_positions_are_listed_but_do_not_count_as_changes` with:

```rust
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
                receipts: peer(0, 0),
                updated_at: world.clock.now(),
                members: sorted(vec![alice.user, bob.user]),
                admins: vec![],
            }]
        );
    }
}
```

```rust
#[tokio::test]
async fn peer_positions_are_zero_until_the_peer_acks_or_reads() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(summary(&world, &alice, chat).await.receipts, peer(0, 0));
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
    assert_eq!(summary(&world, &alice, chat).await.receipts, peer(2, 0));

    world.svc.read_up_to(bob.user, chat, 3).await.unwrap();
    assert_eq!(
        summary(&world, &alice, chat).await.receipts,
        peer(3, 3),
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

and append at the end of the file:

```rust
#[tokio::test]
async fn a_group_is_listed_with_its_admins_and_group_wide_positions() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.send(&alice, group, "one").await;
    world.svc.read_up_to(bob.user, group, 2).await.unwrap();

    let alices = summary(&world, &alice, group).await;
    assert_eq!(
        alices,
        ConversationSummary {
            conversation_id: group,
            kind: ConversationKind::Group,
            title: Some("Group".into()),
            last_seq: 2,
            read_seq: 0,
            unread: 0,
            receipts: Receipts::Group {
                all_delivered_seq: 1,
                all_read_seq: 1,
            },
            updated_at: world.clock.now(),
            members: sorted(vec![alice.user, bob.user, carol.user]),
            admins: vec![alice.user],
        }
    );

    world.svc.read_up_to(carol.user, group, 2).await.unwrap();
    assert_eq!(
        summary(&world, &bob, group).await.receipts,
        Receipts::Group {
            all_delivered_seq: 2,
            all_read_seq: 2,
        }
    );
}

#[tokio::test]
async fn unread_in_a_group_counts_messages_from_everyone_else() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.send(&bob, group, "from bob").await;
    world.send(&carol, group, "from carol").await;

    assert_eq!(summary(&world, &alice, group).await.unread, 2);
    assert_eq!(summary(&world, &bob, group).await.unread, 1);
}
```

In `crates/push-domain/tests/read_state.rs`, replace the `use push_domain::{…};` line and
the `member` helper with:

```rust
use push_domain::{ConversationState, DomainError, MemberState, Positions, UserId};

fn member(state: &ConversationState, user: UserId) -> &MemberState {
    let Positions::Members(members) = &state.positions else {
        panic!("a direct conversation lists its members' positions");
    };
    members
        .iter()
        .find(|member| member.user_id == user)
        .expect("member present")
}
```

and append at the end of the file:

```rust
#[tokio::test]
async fn a_groups_state_holds_only_group_wide_positions() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.send(&alice, group, "one").await;
    world.svc.ack(bob.device, group, 2).await.unwrap();
    world.svc.ack(carol.device, group, 2).await.unwrap();
    world.svc.read_up_to(carol.user, group, 2).await.unwrap();

    let state = world.svc.conversation_state(bob.user, group).await.unwrap();
    assert_eq!(state.last_seq, 2);
    assert_eq!(
        state.positions,
        Positions::Group {
            all_delivered_seq: 2,
            all_read_seq: 1,
        }
    );
}
```

In `crates/push-domain/tests/direct.rs`, replace the `use push_domain::{…};` line with
`use push_domain::{DomainError, Positions, ValidationError};` and the test
`opening_again_adds_no_duplicate_members` with:

```rust
#[tokio::test]
async fn opening_again_adds_no_duplicate_members() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let again = world.svc.open_direct(alice.user, bob.user).await.unwrap();
    let from_bob = world.svc.open_direct(bob.user, alice.user).await.unwrap();
    assert_eq!(again, chat);
    assert_eq!(from_bob, chat);

    let state = world
        .svc
        .conversation_state(alice.user, chat)
        .await
        .unwrap();
    assert!(matches!(state.positions, Positions::Members(members) if members.len() == 2));
}
```

In `crates/push-domain/tests/edit.rs`, add `Positions` to the `use push_domain::{…}` list
after `Policy`, and replace the test `an_edit_leaves_the_senders_read_position_unchanged`
with:

```rust
#[tokio::test]
async fn an_edit_leaves_the_senders_read_position_unchanged() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.send(&bob, chat, "reply").await;
    let read_seq_of_alice = || async {
        let state = world
            .svc
            .conversation_state(alice.user, chat)
            .await
            .unwrap();
        let Positions::Members(members) = state.positions else {
            panic!("a direct conversation lists its members' positions");
        };
        members
            .iter()
            .find(|member| member.user_id == alice.user)
            .unwrap()
            .read_seq
    };
    let before = read_seq_of_alice().await;

    world.edit(&alice, chat, &hello, "v1").await;
    assert_eq!(read_seq_of_alice().await, before);
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p push-domain --test listings --test read_state --test direct --test edit`
Expected: compile errors — `unresolved imports push_domain::Receipts, push_domain::Positions`.

- [ ] **Step 3: Change the summary and the state**

In `crates/push-domain/src/model.rs`, in `struct ConversationSummary`, replace the fields
`peer_delivered_seq` and `peer_read_seq` and their doc comments with:

```rust
    /// How far the others got, for delivery and read ticks.
    pub receipts: Receipts,
```

and add a field at the end of the struct, after `members`:

```rust
    /// Current admins of a group, sorted. Empty for a direct conversation.
    pub admins: Vec<UserId>,
```

Then replace `struct ConversationState` with:

```rust
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
```

In `crates/push-domain/src/lib.rs`, replace the `pub use model::{…};` block with:

```rust
pub use model::{
    ContactState, ContactSummary, ContentType, Conversation, ConversationKind, ConversationState,
    ConversationSummary, Entry, EntryKind, Invite, InviteRecord, Member, MemberState, Positions,
    Receipts, Role, SendReceipt, direct_key,
};
```

- [ ] **Step 4: Positions as a member may see them**

In `crates/push-domain/src/read.rs`, replace the `use crate::model::{…};` line with:

```rust
use crate::model::{
    Conversation, ConversationKind, ConversationState, Member, MemberState, Positions,
};
```

add after `group_positions`:

```rust
/// The group-wide positions of a group as `member` may see them: never above the end of
/// their visible range, and zero for a group without members.
pub(crate) async fn group_positions_for<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    member: &Member,
) -> DomainResult<GroupPositions> {
    let upper = member.visible_upper(conversation.last_seq);
    let all = group_positions(tx, conversation, conversation.last_seq).await?;
    Ok(all.map_or(
        GroupPositions {
            delivered: 0,
            read: 0,
        },
        |all| GroupPositions {
            delivered: all.delivered.min(upper),
            read: all.read.min(upper),
        },
    ))
}
```

and replace `conversation_state` with:

```rust
    /// Delivery and read positions, as `user` may see them: both members' positions in a
    /// direct conversation, only the group-wide positions in a group.
    pub async fn conversation_state(
        &self,
        user: UserId,
        conversation: ConversationId,
    ) -> DomainResult<ConversationState> {
        let mut tx = self.store.begin().await?;
        let found = require_conversation(&mut tx, conversation).await?;
        let caller = require_member(&mut tx, conversation, user).await?;

        let positions = if found.kind == ConversationKind::Group {
            let group = group_positions_for(&mut tx, &found, &caller).await?;
            Positions::Group {
                all_delivered_seq: group.delivered,
                all_read_seq: group.read,
            }
        } else {
            let mut members = Vec::new();
            for member in tx.members(conversation).await? {
                let delivered_seq = tx.delivered_seq(conversation, member.user_id).await?;
                members.push(MemberState {
                    user_id: member.user_id,
                    delivered_seq: delivered_seq.max(member.read_seq),
                    read_seq: member.read_seq,
                });
            }
            Positions::Members(members)
        };
        tx.rollback().await?;
        Ok(ConversationState {
            conversation_id: conversation,
            last_seq: caller.visible_upper(found.last_seq),
            positions,
        })
    }
```

- [ ] **Step 5: List receipts and admins**

In `crates/push-domain/src/conversations.rs`, replace the `use crate::model::{…};` block
with:

```rust
use crate::model::{
    ContactState, Conversation, ConversationKind, ConversationSummary, Member, Receipts, Role,
    direct_key,
};
```

and add `use crate::read::group_positions_for;` after `use crate::notify::Notifier;`.

Replace `list_conversations` with:

```rust
    /// Every conversation `user` is or was a member of, with positions and unread counts,
    /// sorted by id. With `changed_since`, only those whose `updated_at` is later.
    ///
    /// `changed_since` reflects content changes only: a conversation created or an entry
    /// appended. Receipt positions (`receipts`) do not move `updated_at`, so the busy
    /// conversation row is not written on every ack and read. A reconnecting client
    /// refreshes its ticks with one listing without `changed_since`.
    ///
    /// Clients should pass as `changed_since` the time of their previous listing minus a
    /// safety window (a few seconds plus allowed clock drift): `updated_at` is stamped when
    /// a change starts, before it commits, so a change can commit after a listing that ran
    /// later than its stamp.
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
            let receipts = receipts(&mut tx, &conversation, &member, &members).await?;
            summaries.push(ConversationSummary {
                conversation_id: conversation.id,
                kind: conversation.kind,
                title: conversation.title,
                last_seq,
                read_seq: member.read_seq,
                unread,
                receipts,
                updated_at: conversation.updated_at,
                members: current_member_ids(&members),
                admins: current_admin_ids(&members),
            });
        }
        tx.rollback().await?;
        Ok(summaries)
    }
```

and replace the free function `peer_positions` with:

```rust
/// How far the others got, as `member` may see it: the other member's positions in a
/// direct conversation, with delivered at least read; the group-wide positions in a group.
async fn receipts<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    member: &Member,
    members: &[Member],
) -> DomainResult<Receipts> {
    if conversation.kind == ConversationKind::Group {
        let group = group_positions_for(tx, conversation, member).await?;
        return Ok(Receipts::Group {
            all_delivered_seq: group.delivered,
            all_read_seq: group.read,
        });
    }
    let Some(peer) = members.iter().find(|other| other.user_id != member.user_id) else {
        return Ok(Receipts::Direct {
            peer_delivered_seq: 0,
            peer_read_seq: 0,
        });
    };
    let delivered_seq = tx.delivered_seq(conversation.id, peer.user_id).await?;
    Ok(Receipts::Direct {
        peer_delivered_seq: delivered_seq.max(peer.read_seq),
        peer_read_seq: peer.read_seq,
    })
}

/// The ids of the current admins, sorted.
fn current_admin_ids(members: &[Member]) -> Vec<UserId> {
    let mut admins: Vec<UserId> = members
        .iter()
        .filter(|member| member.is_current() && member.role == Role::Admin)
        .map(|member| member.user_id)
        .collect();
    admins.sort();
    admins
}
```

- [ ] **Step 6: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 215 tests pass (45 unit, 170 integration; `listings` 11, `read_state` 12), no
warnings, no formatting diff.

- [ ] **Step 7: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): group receipts in listings and conversation state"
```

---

### Task 8: Add group members

**Files:**
- Modify: `crates/push-domain/src/error.rs`
- Modify: `crates/push-domain/src/groups.rs`
- Modify: `crates/push-domain/tests/groups.rs`

Spec §6: admins add members, who must be accepted contacts of the admin adding them; the
100-member limit is checked under the conversation lock; a new member sees history from
the "added" entry. Every group change from here on runs through `GroupChange`: it locks the
group through `next_seq`, holds the member rows, computes the group-wide positions before
the change, appends system entries, and on `finish` computes the receipt and the
`EntriesAvailable` event. An early return drops it, which rolls back and gives the `seq`
back (decision 16). The added member's read position and devices start at `seq - 1`
(decision 7). `register_device` already starts a new device at the end of every
membership; a test pins that for groups.

- [ ] **Step 1: Write the failing tests**

In `crates/push-domain/tests/groups.rs`, replace the `use push_domain::{…};` block with:

```rust
use push_domain::{
    Clock, ContentType, ConversationId, DomainError, EntryKind, MAX_GROUP_MEMBERS, Role,
    SystemEvent, UserId, ValidationError,
};
```

and append at the end of the file:

```rust
#[tokio::test]
async fn an_admin_adds_a_contact_who_sees_the_group_from_the_added_entry() {
    let world = World::new();
    let (alice, bob, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    world.send(&alice, group, "before").await;
    world.befriend(&alice, &dave).await;
    world.notifier.take();

    world
        .svc
        .add_members(alice.user, group, &[dave.user])
        .await
        .unwrap();
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &bob, &dave])]);
    let batch = world.svc.sync(dave.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [3]);
    assert_eq!(
        system_events(&batch.entries),
        [Some(SystemEvent::MembersAdded {
            actor: alice.user,
            users: vec![dave.user],
        })]
    );
    let history = world.svc.history(dave.user, group, None, 10).await.unwrap();
    assert_eq!(seqs(&history.entries), [3]);
    let row = world.member(group, &dave).await.unwrap();
    assert_eq!(
        (row.role, row.visible_from_seq, row.read_seq),
        (Role::Member, 3, 2)
    );
    assert_eq!(world.position(dave.device, group).await, Some(2));
}

#[tokio::test]
async fn messages_from_before_joining_are_neither_unread_nor_changed_for_the_new_member() {
    let world = World::new();
    let (alice, bob, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    let before = world.send(&alice, group, "before").await;
    world.befriend(&alice, &dave).await;
    world
        .svc
        .add_members(alice.user, group, &[dave.user])
        .await
        .unwrap();
    world.edit(&alice, group, &before, "before, edited").await;
    world.send(&alice, group, "after").await;

    let listed = world.svc.list_conversations(dave.user, None).await.unwrap();
    assert_eq!((listed[0].last_seq, listed[0].unread), (5, 1));
    let batch = world.svc.sync(dave.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [3, 5]);
}

#[tokio::test]
async fn only_a_current_admin_of_a_group_can_add() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    let chat = world.direct_chat(&alice, &carol).await;
    world.befriend(&bob, &carol).await;

    let add = |by: UserId, conversation| {
        let svc = &world.svc;
        async move { svc.add_members(by, conversation, &[carol.user]).await }
    };
    assert_eq!(add(bob.user, group).await, Err(DomainError::NotAdmin));
    assert_eq!(add(carol.user, group).await, Err(DomainError::NotMember));
    assert_eq!(add(alice.user, chat).await, Err(DomainError::NotAGroup));
    assert_eq!(
        add(alice.user, ConversationId::new()).await,
        Err(DomainError::ConversationNotFound)
    );
}

#[tokio::test]
async fn added_users_must_be_contacts_of_the_admin_and_current_members_are_skipped() {
    let world = World::new();
    let (alice, bob, erin) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    world.notifier.take();

    assert_eq!(
        world.svc.add_members(alice.user, group, &[erin.user]).await,
        Err(DomainError::NotContact)
    );
    assert_eq!(
        world
            .svc
            .add_members(alice.user, group, &[bob.user, alice.user])
            .await,
        Ok(())
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.conversation(group).await.last_seq, 1);
}

#[tokio::test]
async fn adding_respects_the_hundred_member_cap() {
    let world = World::new();
    let alice = world.person().await;
    let mut friends = Vec::new();
    for _ in 0..MAX_GROUP_MEMBERS {
        let friend = world.person().await;
        world.befriend(&alice, &friend).await;
        friends.push(friend.user);
    }
    let (last, first) = friends.split_last().unwrap();
    let group = world
        .svc
        .create_group(alice.user, "Almost full", &first[1..])
        .await
        .unwrap();

    assert_eq!(
        world
            .svc
            .add_members(alice.user, group, &[first[0], *last])
            .await,
        Err(DomainError::GroupFull)
    );
    assert_eq!(
        world.svc.add_members(alice.user, group, &[*last]).await,
        Ok(())
    );
}

#[tokio::test]
async fn a_device_registered_later_starts_at_the_end_of_the_group() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let group = world.group(&alice, &[&bob]).await;
    world.send(&alice, group, "one").await;
    world.send(&alice, group, "two").await;

    let tablet = world.add_device(&bob).await;
    assert_eq!(world.position(tablet, group).await, Some(3));
    assert!(
        world
            .svc
            .sync(tablet, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}
```

The second test is the real-join version of Plan 2A's visibility tests: the new member's
unread count ignores the message from before their join, and they never receive the edit
of it (`seq` 4), whose target lies below their visible range. The last test pins behaviour
`register_device` already has.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p push-domain --test groups`
Expected: compile errors — `no method named add_members`, `no variant named NotAdmin`.

- [ ] **Step 3: Add the errors**

In `crates/push-domain/src/error.rs`, add after `GroupFull`:

```rust
    #[error("this is not a group")]
    NotAGroup,
    #[error("only an admin of this group can do that")]
    NotAdmin,
```

- [ ] **Step 4: Add `GroupChange` and `add_members`**

In `crates/push-domain/src/groups.rs`, replace the `use crate::…` lines with:

```rust
use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::conversations::require_conversation;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, MessageId, UserId};
use crate::model::{ContentType, Conversation, ConversationKind, Entry, EntryKind, Member, Role};
use crate::notify::{Event, Notifier};
use crate::read::{GroupPositions, group_positions, group_receipt};
use crate::service::{Messaging, current_member_ids};
use crate::store::{Store, Tx};
use crate::system::SystemEvent;
use crate::validation::{MAX_GROUP_MEMBERS, validate_title};
```

add to the `impl` block, after `create_group`:

```rust
    /// Adds accepted contacts of `admin` to a group as members. Users who are already
    /// current members are skipped, so adding twice changes nothing; former members
    /// rejoin. A group has at most `MAX_GROUP_MEMBERS` current members.
    ///
    /// A `MembersAdded` entry records the change. The added members see the log from that
    /// entry on, with nothing before it unread, and every active device of theirs starts
    /// just before it.
    pub async fn add_members(
        &self,
        admin: UserId,
        conversation: ConversationId,
        users: &[UserId],
    ) -> DomainResult<()> {
        let mut change = self.begin_change(conversation).await?;
        change.require_admin(admin)?;
        let mut added: Vec<UserId> = users
            .iter()
            .copied()
            .filter(|user| !change.is_current(*user))
            .collect();
        added.sort();
        added.dedup();
        if added.is_empty() {
            return change.cancel().await;
        }
        if change.current_count() + added.len() > MAX_GROUP_MEMBERS {
            return Err(DomainError::GroupFull);
        }
        for user in &added {
            if !are_contacts(&mut change.tx, admin, *user).await? {
                return Err(DomainError::NotContact);
            }
        }
        let event = SystemEvent::MembersAdded {
            actor: admin,
            users: added.clone(),
        };
        let seq = change.append(&event).await?;
        for user in added {
            join(&mut change.tx, conversation, user, Role::Member, seq).await?;
        }
        let events = change.finish(None).await?;
        self.publish(events).await;
        Ok(())
    }

    /// Locks a group for a membership or settings change and takes the next `seq`.
    async fn begin_change(&self, conversation: ConversationId) -> DomainResult<GroupChange<S::Tx>> {
        let mut tx = self.store.begin().await?;
        let conversation = require_conversation(&mut tx, conversation).await?;
        if conversation.kind != ConversationKind::Group {
            return Err(DomainError::NotAGroup);
        }
        let now = self.clock.now();
        let seq = tx.next_seq(conversation.id, now).await?;
        let members = tx.members(conversation.id).await?;
        let before = group_positions(&mut tx, &conversation, seq - 1).await?;
        Ok(GroupChange {
            tx,
            conversation,
            members,
            next_seq: Some(seq),
            last_seq: seq - 1,
            now,
            before,
        })
    }
```

and add after the `impl` block (before the free function `join`):

```rust
/// An open transaction holding a group's lock, for one membership or settings change.
/// Dropping it without `finish` rolls back, which also gives back the `seq` it took.
struct GroupChange<T> {
    tx: T,
    conversation: Conversation,
    /// Every member row as it was when the lock was taken, current and former.
    members: Vec<Member>,
    /// The `seq` taken by `begin_change` and not used yet.
    next_seq: Option<i64>,
    /// The highest `seq` written so far.
    last_seq: i64,
    now: DateTime<Utc>,
    /// The group-wide positions before the change.
    before: Option<GroupPositions>,
}

impl<T: Tx> GroupChange<T> {
    /// `NotMember` unless `actor` is a current member, `NotAdmin` unless also an admin.
    fn require_admin(&self, actor: UserId) -> DomainResult<()> {
        match self.member(actor) {
            Some(member) if member.is_current() && member.role == Role::Admin => Ok(()),
            Some(member) if member.is_current() => Err(DomainError::NotAdmin),
            _ => Err(DomainError::NotMember),
        }
    }

    fn is_current(&self, user: UserId) -> bool {
        self.member(user).is_some_and(Member::is_current)
    }

    fn member(&self, user: UserId) -> Option<&Member> {
        self.members.iter().find(|member| member.user_id == user)
    }

    fn current_count(&self) -> usize {
        self.members
            .iter()
            .filter(|member| member.is_current())
            .count()
    }

    /// Appends a system entry recording `event` and returns its `seq`.
    async fn append(&mut self, event: &SystemEvent) -> DomainResult<i64> {
        let seq = match self.next_seq.take() {
            Some(seq) => seq,
            None => self.tx.next_seq(self.conversation.id, self.now).await?,
        };
        self.tx
            .insert_entry(system_entry(self.conversation.id, seq, event, self.now))
            .await?;
        self.last_seq = seq;
        Ok(seq)
    }

    /// Nothing to change: rolls back.
    async fn cancel(self) -> DomainResult<()> {
        self.tx.rollback().await
    }

    /// Commits, and returns the events to publish: new entries for the current members and
    /// `also_tell` (a member who just left and may see their own departure), and the new
    /// group-wide positions if they moved.
    async fn finish(mut self, also_tell: Option<UserId>) -> DomainResult<Vec<Event>> {
        let group =
            group_receipt(&mut self.tx, &self.conversation, self.last_seq, self.before).await?;
        let members = self.tx.members(self.conversation.id).await?;
        self.tx.commit().await?;

        let mut users = current_member_ids(&members);
        users.extend(also_tell);
        users.sort();
        let mut events = vec![Event::EntriesAvailable { users }];
        events.extend(group);
        Ok(events)
    }
}
```

- [ ] **Step 5: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 221 tests pass (45 unit, 176 integration; `groups` 13), no warnings, no
formatting diff.

- [ ] **Step 6: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): add group members"
```

---

### Task 9: Remove members and leave groups

**Files:**
- Modify: `crates/push-domain/src/groups.rs`
- Modify: `crates/push-domain/src/conversations.rs`
- Create: `crates/push-domain/tests/membership.rs`

Spec §6: admins remove members, members leave, a group always has an admin, and a group with
no members left is closed. The removal or leave entry is the last one the departing user can
see (`left_at_seq`), so they are told to sync it. If no admin remains, the heir is promoted
in the same transaction by an `AdminPromoted` entry right after the `MemberLeft` entry
(decision 16). This task also settles what a former member's listing shows (decision 13),
and tests the `ack` floor guard with a real rejoin (decision 7) — the test fails if the
`seq < visible_from_seq - 1` guard in `ack` is removed.

- [ ] **Step 1: Write the failing tests**

Create `crates/push-domain/tests/membership.rs`:

```rust
mod common;

use chrono::TimeDelta;
use common::{Person, World, entries_for, seqs, system_events, text};
use push_domain::{
    Clock, ConversationId, DomainError, Event, ReceiptUpdate, Role, SystemEvent, ValidationError,
};

/// A group of alice (admin), bob and carol; `seq` 1 is its `Created` entry.
async fn trio() -> (World, Person, Person, Person, ConversationId) {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.notifier.take();
    (world, alice, bob, carol, group)
}

#[tokio::test]
async fn a_removed_member_receives_nothing_after_the_removal_entry() {
    let (world, alice, bob, carol, group) = trio().await;
    world.send(&alice, group, "one").await;
    world.notifier.take();

    world
        .svc
        .remove_member(alice.user, group, bob.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
    world.send(&alice, group, "after").await;
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &carol])]);

    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [1, 2, 3]);
    assert_eq!(
        system_events(&batch.entries[2..]),
        [Some(SystemEvent::MemberRemoved {
            actor: alice.user,
            user: bob.user,
        })]
    );
    let history = world.svc.history(bob.user, group, None, 10).await.unwrap();
    assert_eq!(seqs(&history.entries), [1, 2, 3]);
    assert_eq!(
        world.svc.send_text(text(&bob, group, "hello?")).await,
        Err(DomainError::NotMember)
    );
    assert_eq!(
        world.svc.ack(bob.device, group, 4).await,
        Err(DomainError::SeqOutOfRange)
    );
    assert_eq!(world.svc.read_up_to(bob.user, group, 3).await, Ok(true));
}

#[tokio::test]
async fn removing_the_member_who_held_the_group_back_completes_the_read() {
    let (world, alice, bob, carol, group) = trio().await;
    world.send(&alice, group, "one").await;
    world.svc.read_up_to(bob.user, group, 2).await.unwrap();
    world.notifier.take();

    world
        .svc
        .remove_member(alice.user, group, carol.user)
        .await
        .unwrap();
    let mut to = vec![alice.user, bob.user];
    to.sort();
    assert_eq!(
        world.notifier.take(),
        [
            entries_for(&[&alice, &bob, &carol]),
            Event::Receipt {
                conversation: group,
                to,
                receipt: ReceiptUpdate::Group {
                    all_delivered_seq: 3,
                    all_read_seq: 3,
                },
            }
        ]
    );
}

#[tokio::test]
async fn only_an_admin_removes_others_and_removing_twice_changes_nothing() {
    let (world, alice, bob, carol, group) = trio().await;
    let dave = world.person().await;

    assert_eq!(
        world.svc.remove_member(bob.user, group, carol.user).await,
        Err(DomainError::NotAdmin)
    );
    assert_eq!(
        world.svc.remove_member(alice.user, group, alice.user).await,
        Err(DomainError::Invalid(ValidationError::SelfTarget))
    );
    assert_eq!(
        world.svc.remove_member(alice.user, group, dave.user).await,
        Err(DomainError::NotMember)
    );
    world
        .svc
        .remove_member(alice.user, group, carol.user)
        .await
        .unwrap();
    world.notifier.take();

    assert_eq!(
        world.svc.remove_member(alice.user, group, carol.user).await,
        Ok(())
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.conversation(group).await.last_seq, 2);
}

#[tokio::test]
async fn a_member_leaves_and_leaving_twice_changes_nothing() {
    let (world, alice, bob, carol, group) = trio().await;

    world.svc.leave_group(bob.user, group).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(
        system_events(&batch.entries),
        [
            Some(SystemEvent::Created {
                actor: alice.user,
                title: "Group".into(),
                users: {
                    let mut users = vec![bob.user, carol.user];
                    users.sort();
                    users
                },
            }),
            Some(SystemEvent::MemberLeft { actor: bob.user })
        ]
    );
    assert_eq!(
        world.member(group, &bob).await.unwrap().left_at_seq,
        Some(2)
    );

    assert_eq!(world.svc.leave_group(bob.user, group).await, Ok(()));
    assert!(world.notifier.take().is_empty());
    let stranger = world.person().await;
    assert_eq!(
        world.svc.leave_group(stranger.user, group).await,
        Err(DomainError::NotMember)
    );
}

#[tokio::test]
async fn when_the_last_admin_leaves_the_longest_standing_member_is_promoted() {
    let world = World::new();
    let (alice, bob, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    world.befriend(&alice, &dave).await;
    world
        .svc
        .add_members(alice.user, group, &[dave.user])
        .await
        .unwrap();

    world.svc.leave_group(alice.user, group).await.unwrap();
    let batch = world.svc.sync(dave.device, 100).await.unwrap();
    assert_eq!(
        system_events(&batch.entries),
        [
            Some(SystemEvent::MembersAdded {
                actor: alice.user,
                users: vec![dave.user],
            }),
            Some(SystemEvent::MemberLeft { actor: alice.user }),
            Some(SystemEvent::AdminPromoted {
                actor: alice.user,
                user: bob.user,
            })
        ]
    );
    assert_eq!(batch.entries[2].sender_device_id, None);
    assert_eq!(world.member(group, &bob).await.unwrap().role, Role::Admin);
    let alices = world.svc.sync(alice.device, 100).await.unwrap();
    assert_eq!(seqs(&alices.entries), [1, 2, 3]);
}

#[tokio::test]
async fn among_members_who_joined_together_the_lowest_user_id_is_promoted() {
    let (world, alice, bob, carol, group) = trio().await;

    world.svc.leave_group(alice.user, group).await.unwrap();
    let heir = if bob.user < carol.user { &bob } else { &carol };
    assert_eq!(world.member(group, heir).await.unwrap().role, Role::Admin);
}

#[tokio::test]
async fn a_group_whose_last_member_left_is_closed() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let group = world.group(&alice, &[&bob]).await;
    world.svc.leave_group(bob.user, group).await.unwrap();

    world.svc.leave_group(alice.user, group).await.unwrap();
    assert_eq!(world.conversation(group).await.last_seq, 3);
    assert_eq!(
        world.svc.send_text(text(&alice, group, "anyone?")).await,
        Err(DomainError::NotMember),
        "a group whose last member left is closed"
    );
}

#[tokio::test]
async fn a_rejoining_member_sees_only_the_new_range_and_old_acks_change_nothing() {
    let (world, alice, bob, _carol, group) = trio().await;
    world.send(&alice, group, "one").await;
    world.send(&alice, group, "two").await;
    world.svc.ack(bob.device, group, 3).await.unwrap();
    world
        .svc
        .remove_member(alice.user, group, bob.user)
        .await
        .unwrap();
    world.send(&alice, group, "while away").await;
    world.send(&alice, group, "still away").await;
    world
        .svc
        .add_members(alice.user, group, &[bob.user])
        .await
        .unwrap();

    let row = world.member(group, &bob).await.unwrap();
    assert_eq!(
        (row.visible_from_seq, row.left_at_seq, row.read_seq),
        (7, None, 6)
    );
    assert_eq!(world.position(bob.device, group).await, Some(3));
    assert_eq!(world.svc.ack(bob.device, group, 5).await, Ok(false));
    assert_eq!(world.position(bob.device, group).await, Some(3));
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [7]);
    let history = world.svc.history(bob.user, group, None, 10).await.unwrap();
    assert_eq!(seqs(&history.entries), [7]);
}

#[tokio::test]
async fn a_former_member_lists_the_group_as_it_was_when_they_left() {
    let (world, alice, bob, _carol, group) = trio().await;
    world.clock.advance(TimeDelta::minutes(1));
    world.svc.leave_group(bob.user, group).await.unwrap();
    let left_at = world.clock.now();
    world.clock.advance(TimeDelta::minutes(1));
    world.send(&alice, group, "after bob left").await;

    let listed = world.svc.list_conversations(bob.user, None).await.unwrap();
    assert_eq!(listed.len(), 1);
    let bobs = &listed[0];
    assert_eq!((bobs.last_seq, bobs.updated_at), (2, left_at));
    assert!(bobs.members.is_empty() && bobs.admins.is_empty());
    assert!(
        world
            .svc
            .list_conversations(bob.user, Some(left_at))
            .await
            .unwrap()
            .is_empty()
    );
}
```

In the rejoin test, bob's device still holds position 3 from before his removal, below his
new floor of 6; `sync` clamps at the floor, and an ack of 5 — inside his old range, below
his new one — changes nothing.

Run: `cargo test -p push-domain --test membership`
Expected: compile errors — `no method named remove_member`, `no method named leave_group`.

- [ ] **Step 2: Implement removing and leaving**

In `crates/push-domain/src/groups.rs`, replace
`use crate::validation::{MAX_GROUP_MEMBERS, validate_title};` with:

```rust
use crate::validation::{MAX_GROUP_MEMBERS, ValidationError, validate_title};
```

add to the `impl` block of `Messaging`, after `add_members`:

```rust
    /// Removes a current member from a group. A `MemberRemoved` entry records it, and it is
    /// the last entry the removed user can see. Removing a former member again changes
    /// nothing; a user who was never a member is `NotMember`. Admins leave rather than
    /// remove themselves.
    pub async fn remove_member(
        &self,
        admin: UserId,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<()> {
        if admin == user {
            return Err(ValidationError::SelfTarget.into());
        }
        let mut change = self.begin_change(conversation).await?;
        change.require_admin(admin)?;
        let mut removed = change.member(user).cloned().ok_or(DomainError::NotMember)?;
        if !removed.is_current() {
            return change.cancel().await;
        }
        let event = SystemEvent::MemberRemoved { actor: admin, user };
        removed.left_at_seq = Some(change.append(&event).await?);
        change.put_member(removed).await?;
        let events = change.finish(Some(user)).await?;
        self.publish(events).await;
        Ok(())
    }

    /// `user` leaves a group. A `MemberLeft` entry records it, and it is the last entry they
    /// can see. Leaving again changes nothing.
    ///
    /// A group always has an admin: if none remains, the current member who joined first
    /// (lowest `visible_from_seq`, then lowest user id) becomes one, recorded by an
    /// `AdminPromoted` entry. When the last member leaves, the group is closed: nothing can
    /// be appended to it any more, because every append needs a current member.
    pub async fn leave_group(
        &self,
        user: UserId,
        conversation: ConversationId,
    ) -> DomainResult<()> {
        let mut change = self.begin_change(conversation).await?;
        let mut leaver = change.member(user).cloned().ok_or(DomainError::NotMember)?;
        if !leaver.is_current() {
            return change.cancel().await;
        }
        leaver.left_at_seq = Some(
            change
                .append(&SystemEvent::MemberLeft { actor: user })
                .await?,
        );
        change.put_member(leaver).await?;
        if let Some(mut heir) = change.heir() {
            let promoted = SystemEvent::AdminPromoted {
                actor: user,
                user: heir.user_id,
            };
            change.append(&promoted).await?;
            heir.role = Role::Admin;
            change.put_member(heir).await?;
        }
        let events = change.finish(Some(user)).await?;
        self.publish(events).await;
        Ok(())
    }
```

and add to `impl<T: Tx> GroupChange<T>`, after `current_count`:

```rust
    /// The member to promote when no current admin is left: the current member who joined
    /// first, then the lowest user id. `None` while an admin remains or nobody does.
    fn heir(&self) -> Option<Member> {
        let current = self.members.iter().filter(|member| member.is_current());
        if current.clone().any(|member| member.role == Role::Admin) {
            return None;
        }
        current
            .min_by_key(|member| (member.visible_from_seq, member.user_id))
            .cloned()
    }

    /// Stores a changed member row and keeps this change's view of the members up to date.
    async fn put_member(&mut self, member: Member) -> DomainResult<()> {
        if let Some(row) = self
            .members
            .iter_mut()
            .find(|row| row.user_id == member.user_id)
        {
            *row = member.clone();
        }
        self.tx.insert_member(member).await
    }
```

- [ ] **Step 3: List a left group as its former member last saw it**

In `crates/push-domain/src/conversations.rs`, replace `list_conversations` with:

```rust
    /// Every conversation `user` is or was a member of, with positions and unread counts,
    /// sorted by id. With `changed_since`, only those whose `updated_at` is later.
    ///
    /// `changed_since` reflects content changes only: a conversation created or an entry
    /// appended. Receipt positions (`receipts`) do not move `updated_at`, so the busy
    /// conversation row is not written on every ack and read. A reconnecting client
    /// refreshes its ticks with one listing without `changed_since`.
    ///
    /// Clients should pass as `changed_since` the time of their previous listing minus a
    /// safety window (a few seconds plus allowed clock drift): `updated_at` is stamped when
    /// a change starts, before it commits, so a change can commit after a listing that ran
    /// later than its stamp.
    ///
    /// A direct conversation with someone `user` has blocked is left out: blocking hides it
    /// from the blocker (spec §5.5). The blocked user still sees it.
    ///
    /// A group `user` has left is listed as they last saw it: up to the entry at which they
    /// left, with its time as `updated_at`, and without members or admins. Its title and
    /// group-wide positions are current, the positions capped at where they left.
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
            let receipts = receipts(&mut tx, &conversation, &member, &members).await?;
            let mut summary = ConversationSummary {
                conversation_id: conversation.id,
                kind: conversation.kind,
                title: conversation.title.clone(),
                last_seq,
                read_seq: member.read_seq,
                unread,
                receipts,
                updated_at: conversation.updated_at,
                members: current_member_ids(&members),
                admins: current_admin_ids(&members),
            };
            if let Some(left_at_seq) = member.left_at_seq {
                summary.updated_at = left_at_time(&mut tx, &conversation, left_at_seq).await?;
                if changed_since.is_some_and(|since| summary.updated_at <= since) {
                    continue;
                }
                summary.members.clear();
                summary.admins.clear();
            }
            summaries.push(summary);
        }
        tx.rollback().await?;
        Ok(summaries)
    }
```

and add after the free function `receipts`:

```rust
/// When a former member's view of a conversation last changed: the time of the entry at
/// which they left, or the conversation's creation if that entry does not exist.
async fn left_at_time<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    left_at_seq: i64,
) -> DomainResult<DateTime<Utc>> {
    let entries = tx
        .entries_after(conversation.id, left_at_seq - 1, left_at_seq, 0, 1)
        .await?;
    Ok(entries
        .first()
        .map_or(conversation.created_at, |entry| entry.server_ts))
}
```

The first `changed_since` check still runs on `conversation.updated_at`: the time a former
member left is never later than that, so it is a correct early skip.

- [ ] **Step 4: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 230 tests pass (45 unit, 185 integration; `membership` 9), no warnings, no
formatting diff.

- [ ] **Step 5: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): remove members and leave groups"
```

---

### Task 10: Rename groups and change roles

**Files:**
- Modify: `crates/push-domain/src/error.rs`
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/memory.rs`
- Modify: `crates/push-domain/src/groups.rs`
- Create: `crates/push-domain/tests/group_settings.rs`

Spec §6: admins rename the group and promote members to admin or demote admins to member; a
group always has at least one admin, so demoting the last one is refused (`LastAdmin`).
Renaming needs one more `Tx` method, `set_title`. Because a rename compares the new title
with the current one, `begin_change` now reads the conversation after taking the lock.

- [ ] **Step 1: Write the failing storage test**

In `crates/push-domain/src/memory.rs`, append inside `mod tests`:

```rust
    #[tokio::test]
    async fn set_title_changes_only_the_title() {
        let store = InMemoryStore::new();
        let original = conversation();
        let id = original.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(original.clone()).await.unwrap();

        tx.set_title(id, "Trip".into()).await.unwrap();
        let stored = tx.get_conversation(id).await.unwrap().unwrap();
        assert_eq!(
            stored,
            Conversation {
                title: Some("Trip".into()),
                ..original
            }
        );
        assert_eq!(
            tx.set_title(ConversationId::new(), "Trip".into()).await,
            Err(DomainError::ConversationNotFound)
        );
    }
```

Run: `cargo test -p push-domain --lib memory`
Expected: compile error — `no method named set_title`.

- [ ] **Step 2: Add `set_title`**

In `crates/push-domain/src/store.rs`, add to `trait Tx` after `next_seq`:

```rust
    /// Sets the title of the conversation. Fails with `ConversationNotFound` if it does not
    /// exist. Callers hold the conversation lock (taken by `next_seq`).
    async fn set_title(&mut self, id: ConversationId, title: String) -> DomainResult<()>;
```

In `crates/push-domain/src/memory.rs`, add to `impl Tx for InMemoryTx` after `next_seq`:

```rust
    async fn set_title(&mut self, id: ConversationId, title: String) -> DomainResult<()> {
        let conversation = self
            .work
            .conversations
            .get_mut(&id)
            .ok_or(DomainError::ConversationNotFound)?;
        conversation.title = Some(title);
        Ok(())
    }
```

Run: `cargo test -p push-domain --lib memory`
Expected: `26 passed`.

- [ ] **Step 3: Write the failing service tests**

Create `crates/push-domain/tests/group_settings.rs`:

```rust
mod common;

use chrono::TimeDelta;
use common::{Person, World, entries_for, seqs, system_events};
use push_domain::{Clock, ConversationId, DomainError, Role, SystemEvent, ValidationError};

/// A group of alice (admin), bob and carol; `seq` 1 is its `Created` entry.
async fn trio() -> (World, Person, Person, Person, ConversationId) {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.notifier.take();
    (world, alice, bob, carol, group)
}

#[tokio::test]
async fn an_admin_renames_the_group() {
    let (world, alice, bob, carol, group) = trio().await;
    world.clock.advance(TimeDelta::minutes(1));

    world
        .svc
        .rename_group(alice.user, group, "  Plans  ")
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
    let stored = world.conversation(group).await;
    assert_eq!(stored.title.as_deref(), Some("Plans"));
    assert_eq!((stored.last_seq, stored.updated_at), (2, world.clock.now()));
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(
        system_events(&batch.entries[1..]),
        [Some(SystemEvent::Renamed {
            actor: alice.user,
            title: "Plans".into(),
        })]
    );
}

#[tokio::test]
async fn renaming_to_the_same_title_changes_nothing_and_members_cannot_rename() {
    let (world, alice, bob, _carol, group) = trio().await;

    assert_eq!(
        world.svc.rename_group(alice.user, group, "Group").await,
        Ok(())
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.conversation(group).await.last_seq, 1);
    assert_eq!(
        world.svc.rename_group(bob.user, group, "Mine").await,
        Err(DomainError::NotAdmin)
    );
    assert_eq!(
        world.svc.rename_group(alice.user, group, " ").await,
        Err(DomainError::Invalid(ValidationError::EmptyTitle))
    );
}

#[tokio::test]
async fn a_promoted_member_can_act_as_admin_and_demote_others() {
    let (world, alice, bob, carol, group) = trio().await;

    world
        .svc
        .set_role(alice.user, group, bob.user, Role::Admin)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
    world
        .svc
        .set_role(bob.user, group, alice.user, Role::Member)
        .await
        .unwrap();
    assert_eq!(
        world.svc.rename_group(alice.user, group, "Mine").await,
        Err(DomainError::NotAdmin)
    );
    let batch = world.svc.sync(carol.device, 100).await.unwrap();
    assert_eq!(
        system_events(&batch.entries[1..]),
        [
            Some(SystemEvent::RoleChanged {
                actor: alice.user,
                user: bob.user,
                role: Role::Admin,
            }),
            Some(SystemEvent::RoleChanged {
                actor: bob.user,
                user: alice.user,
                role: Role::Member,
            })
        ]
    );
}

#[tokio::test]
async fn the_last_admin_cannot_be_demoted() {
    let (world, alice, bob, _carol, group) = trio().await;

    assert_eq!(
        world
            .svc
            .set_role(alice.user, group, alice.user, Role::Member)
            .await,
        Err(DomainError::LastAdmin)
    );
    assert_eq!(
        world
            .svc
            .set_role(alice.user, group, bob.user, Role::Member)
            .await,
        Ok(())
    );
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.conversation(group).await.last_seq, 1);
}

#[tokio::test]
async fn only_current_members_get_a_role() {
    let (world, alice, bob, _carol, group) = trio().await;
    world.svc.leave_group(bob.user, group).await.unwrap();

    assert_eq!(
        world
            .svc
            .set_role(alice.user, group, bob.user, Role::Admin)
            .await,
        Err(DomainError::NotMember)
    );
}

#[tokio::test]
async fn no_one_is_promoted_when_an_admin_leaves_and_another_remains() {
    let (world, alice, bob, carol, group) = trio().await;
    world
        .svc
        .set_role(alice.user, group, bob.user, Role::Admin)
        .await
        .unwrap();

    world.svc.leave_group(alice.user, group).await.unwrap();
    let batch = world.svc.sync(carol.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [1, 2, 3]);
    assert_eq!(
        world.member(group, &carol).await.unwrap().role,
        Role::Member
    );
}
```

Run: `cargo test -p push-domain --test group_settings`
Expected: compile errors — `no method named rename_group`, `no method named set_role`.

- [ ] **Step 4: Implement renaming and roles**

In `crates/push-domain/src/error.rs`, add after `NotAdmin`:

```rust
    #[error("a group must keep at least one admin")]
    LastAdmin,
```

In `crates/push-domain/src/groups.rs`, add to the `impl` block of `Messaging`, after
`leave_group`:

```rust
    /// Renames a group. The title is trimmed and must keep 1 to `MAX_TITLE_CHARS`
    /// characters. A `Renamed` entry records it; renaming to the current title changes
    /// nothing.
    pub async fn rename_group(
        &self,
        admin: UserId,
        conversation: ConversationId,
        title: &str,
    ) -> DomainResult<()> {
        let title = validate_title(title)?;
        let mut change = self.begin_change(conversation).await?;
        change.require_admin(admin)?;
        if change.conversation.title.as_deref() == Some(title.as_str()) {
            return change.cancel().await;
        }
        let event = SystemEvent::Renamed {
            actor: admin,
            title: title.clone(),
        };
        change.append(&event).await?;
        change.tx.set_title(conversation, title).await?;
        let events = change.finish(None).await?;
        self.publish(events).await;
        Ok(())
    }

    /// Gives a current member the role `role`. A `RoleChanged` entry records it; giving
    /// the role they already have changes nothing. Admins may demote themselves, but the
    /// last admin may not: a group always has an admin (`LastAdmin`).
    pub async fn set_role(
        &self,
        admin: UserId,
        conversation: ConversationId,
        user: UserId,
        role: Role,
    ) -> DomainResult<()> {
        let mut change = self.begin_change(conversation).await?;
        change.require_admin(admin)?;
        let mut member = change
            .member(user)
            .filter(|member| member.is_current())
            .cloned()
            .ok_or(DomainError::NotMember)?;
        if member.role == role {
            return change.cancel().await;
        }
        if role == Role::Member && change.admin_count() == 1 {
            return Err(DomainError::LastAdmin);
        }
        let event = SystemEvent::RoleChanged {
            actor: admin,
            user,
            role,
        };
        change.append(&event).await?;
        member.role = role;
        change.put_member(member).await?;
        let events = change.finish(None).await?;
        self.publish(events).await;
        Ok(())
    }
```

replace `begin_change` with:

```rust
    /// Locks a group for a membership or settings change and takes the next `seq`.
    async fn begin_change(&self, conversation: ConversationId) -> DomainResult<GroupChange<S::Tx>> {
        let mut tx = self.store.begin().await?;
        let now = self.clock.now();
        let seq = tx.next_seq(conversation, now).await?;
        // Read under the lock, so the title compared by a rename is the current one.
        let conversation = require_conversation(&mut tx, conversation).await?;
        if conversation.kind != ConversationKind::Group {
            return Err(DomainError::NotAGroup);
        }
        let members = tx.members(conversation.id).await?;
        let before = group_positions(&mut tx, &conversation, seq - 1).await?;
        Ok(GroupChange {
            tx,
            conversation,
            members,
            next_seq: Some(seq),
            last_seq: seq - 1,
            now,
            before,
        })
    }
```

(`next_seq` fails with `ConversationNotFound` for an unknown id, and a direct conversation
is refused with `NotAGroup` before anything is written; dropping the transaction gives the
`seq` back.) Finally add to `impl<T: Tx> GroupChange<T>`, after `current_count`:

```rust
    fn admin_count(&self) -> usize {
        self.members
            .iter()
            .filter(|member| member.is_current() && member.role == Role::Admin)
            .count()
    }
```

- [ ] **Step 5: Run all gates**

```powershell
cargo test -p push-domain
cargo clippy -p push-domain --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: 237 tests pass (46 unit, 191 integration; `group_settings` 6), no warnings, no
formatting diff.

- [ ] **Step 6: Commit**

```powershell
git add crates/push-domain
git commit -m "feat(domain): rename groups and change roles"
```

---

### Task 11: Spec, roadmap and pull request

**Files:**
- Modify: `docs/superpowers/specs/2026-10-11-push-messenger-design.md`
- Modify: `docs/superpowers/plans/2026-10-11-roadmap.md`

- [ ] **Step 1: Bring the spec in line with the decisions**

In §3.2, replace the `invite` table with:

```sql
invite (
  code       text primary key,             -- 10 chars of Crockford base32, also for QR
  user_id    uuid not null references known_user,
  expires_at timestamptz not null,
  max_uses   int not null default 1,       -- 1 to 100
  uses       int not null default 0,
  revoked_at timestamptz
)
```

In §4.5, replace the bullet that starts `- **Groups:** the server pushes only the group-wide
minimum` with:

```markdown
- **Groups:** the server pushes only the group-wide minimums `all_delivered_seq` and
  `all_read_seq`, taken over the current members: every message up to N has been
  delivered to (read by) every member other than its sender. A member's own messages never
  hold these back: each member counts as having reached the point just before the first
  live message from someone else that they have not received (read). message-svc
  publishes a change whenever a minimum moves over a live message; the gateway sends at
  most one per second per conversation, keeping the latest.
```

add a bullet after the bullet `- A read implies delivery: …`:

```markdown
- When a user's read position moves, by reading or by sending, their own devices are told
  with `ReadPosition{conversation_id, read_seq}`, so all of them show the same unread
  counts.
```

and replace the last bullet of §4.5 (`- After connecting, the client calls …`) with:

```markdown
- After connecting, the client calls `ListConversations{changed_since}` to get the current
  positions and unread counts for conversations whose content changed. `changed_since`
  reflects content changes only: a conversation created or an entry appended. Receipt
  positions are not tracked by `updated_at`, so the busy conversation row is not written
  on every ack and read. A conversation's summary carries its receipt positions — the
  other member's `peer_delivered_seq` and `peer_read_seq` in a direct chat, the group-wide
  `all_delivered_seq` and `all_read_seq` in a group — so a reconnecting client refreshes
  its ticks with one `ListConversations` without `changed_since`.
```

In §5.5, replace the bullet that starts `- **Invite link / QR code.**` with:

```markdown
- **Invite link / QR code.** `CreateInvite{expires_in, max_uses}` returns a short code,
  shown as a link or QR code. `RedeemInvite{code}` makes both users accepted contacts
  immediately: sharing the link is the consent. Codes expire (1 minute to 30 days), have a
  use limit (1 to 100) and can be revoked. A code is 10 characters of Crockford base32
  (50 random bits, enough because redeeming is rate limited), read case-insensitively. An
  unknown, revoked, expired or used-up code, and a code whose creator has blocked the
  redeemer, all give the same "invite not found", so redeeming reveals nothing. Users who
  are already contacts use nothing up; a pending request between the two becomes
  accepted.
```

In §6, replace the bullet that starts `- A group always has at least one admin.` with:

```markdown
- A group always has at least one admin: the last admin cannot be demoted. If the last
  admin leaves, the current member who joined first (lowest `visible_from_seq`, then
  lowest user id) becomes admin, recorded as a system entry. Admins remove others; an
  admin who wants to go leaves. A group with no members left is closed.
```

replace the bullet that starts `- Every membership change is a **system entry**` with:

```markdown
- Every membership change is a **system entry** in the log, written under the same
  conversation lock as messages. Sends and membership changes are therefore strictly
  ordered: a removed member never receives anything after the removal entry. A system
  entry (`kind = 4`, `content_type = 2`, no device) carries the event as compact JSON with
  a format version, for example `{"v":1,"type":"members_added","actor":"…","users":["…"]}`.
  The types are `created`, `members_added`, `member_removed`, `member_left`, `renamed`,
  `role_changed` and `admin_promoted`; clients show an unknown type as a generic line.
```

replace the bullet `- Adding a member requires them to be an accepted contact of the admin
adding them.` with:

```markdown
- Adding a member requires them to be an accepted contact of the admin adding them. Inside
  a group, members need not be contacts of each other, and blocks between them do not
  affect the group.
```

and replace the bullet that starts `- A new member sees history from` with:

```markdown
- A new member sees history from `visible_from_seq` (the seq of the "added" entry), and
  nothing before it counts as unread: their read position starts at
  `visible_from_seq - 1`. A member who leaves keeps history up to `left_at_seq` and still
  lists the group as it was when they left, without its member list. Rejoining sets a new
  `visible_from_seq` and clears `left_at_seq`.
```

In §7.2, add a row after the `Receipt` row:

```markdown
| ReadPosition | `conversation_id, read_seq` — the user's own read position moved |
```

- [ ] **Step 2: Mark plan 2B in review**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the empty status cell at the end
of the row for plan 2B (`| 2B | Invites and groups | … | 2A | |`) with `In review`, so the
row ends `| 2A | In review |`.

- [ ] **Step 3: Run the workspace gates**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Expected: no formatting diff, no warnings, 237 tests pass. If the 1.85 toolchain is
installed (`rustup toolchain list`), also run `cargo +1.85 test --workspace --locked`, which
is what the `msrv` CI job runs; expected: 237 tests pass.

- [ ] **Step 4: Commit and push**

```powershell
git add docs
git commit -m "docs: record plan 2B decisions in the spec and roadmap"
git push -u origin feature/plan-02b-invites-groups
```

- [ ] **Step 5: Open the pull request and wait for CI**

```powershell
gh pr create --base main --title "Plan 2B: invites and groups" --body "Implements docs/superpowers/plans/2026-10-12-plan-02b-invites-groups.md: invites (create, revoke, redeem), groups up to 100 members with admin and member roles, system entries for every membership and settings change, visibility from real joins and leaves, group-wide receipts in events, listings and conversation state, and the Plan 2A review leftovers."
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust` and `msrv` jobs). Do not merge; the controller
merges after the final review.

---

## Spec coverage for this plan

| Spec item | Task |
|---|---|
| §3.2 `invite` table, with `revoked_at` (added by this plan) | 2, 11 |
| §4.3 eager delivery positions for every device of a new member; a new device starts at the end of every membership | 5, 8 |
| §4.4 group delivered / read: every other member has it | 6 |
| §4.5 group receipts: minimums over current members, only when they move, never over edits, deletes or system entries; at most one per second per conversation (gateway) | 6, 11 |
| §4.5 receipt positions recovered after reconnect: group-wide positions in group summaries | 7, 11 |
| §4.5 a user's own devices learn that their read position moved (added by this plan) | 1, 11 |
| §5.5 `CreateInvite`, `RedeemInvite`, `RevokeInvite`; expiry, use limit, revocation; sharing is the consent | 2, 3, 11 |
| §5.5 a block is never revealed, also not through an invite | 3 |
| §6 `CreateGroup`: members are accepted contacts of the creator; the creator is admin | 5 |
| §6 admin and member roles; promote and demote; at least one admin; automatic promotion | 9, 10 |
| §6 every membership change is a system entry under the conversation lock | 5, 8, 9, 10 |
| §6 `visible_from_seq` from the added entry, `left_at_seq` on leaving, rejoining | 8, 9 |
| §6 the 100-member limit, checked under the conversation lock | 5, 8 |
| §6 adding requires a contact of the admin who adds | 8 |
| §6 a group with no members is closed | 9 |
| §6 group receipts are aggregated; no per-member positions | 6, 7 |
| §7.1 `ListContacts`, `CreateInvite`, `RedeemInvite`, `RevokeInvite`, `CreateGroup`, `AddMembers`, `RemoveMember`, `LeaveGroup`, `RenameGroup`, `SetRole` | 1, 2, 3, 5, 8, 9, 10 |
| §7.2 `Receipt` for groups; `ReadPosition` (added by this plan) | 1, 6, 11 |
| §12 adverse test 10: a removed member receives nothing after the removal entry | 9 |
| Plan 2A deferrals: group receipts, group summaries and state, former members in listings, the ack floor guard tested, the actor's own devices, `ContactSummary`, floor clamp in `read_up_to` and sends | 1, 6, 7, 9 |

## Deferred to later plans

- Out of scope by decision: group invite links, admins deleting other members' messages,
  per-member read lists, mute, avatars, push notifications.
- A former member sees the group's **current** title. Showing the title as of their
  departure needs the last `created`/`renamed` entry at or before `left_at_seq`.
- `create_group` has no idempotency key, so a retried `CreateGroup` whose first attempt
  succeeded creates a second group. Add a client-chosen id when the wire request is defined
  (Plans 4 and 7).
- Group changes carry no device (`sender_device_id` is null for every system entry). If a
  per-device audit is needed, the group operations must take the device from the token.
- Concurrent acks or reads by **different** members of one group run under READ COMMITTED
  and each sees the other's old position, so both can compute "no change" and the group
  receipt is skipped. Positions stay correct and the next change or a listing recovers
  them. Plan 3 decides whether to serialise group position changes (for example with a
  per-conversation advisory lock) or let the gateway's one-per-second coalescer recompute.
- Redeem rate limiting per account and per IP is a transport concern (Plan 8).
- Wire mapping of the new operations and events (`ReadPosition`, the group `Receipt`,
  group requests and system entry bodies) in `push-proto` (Plans 4, 7 and 8).

Notes for Plan 3 (Postgres):

- `invite`: `insert_invite` is `INSERT … ON CONFLICT (code) DO NOTHING`; `lock_invite` is
  `SELECT … FROM invite WHERE code = $1 FOR UPDATE`; `update_invite` is
  `UPDATE invite SET uses = $2, revoked_at = $3 WHERE code = $1` (0 rows is a store error).
  Redeem takes the invite row lock before the contact pair lock.
- `first_live_message` is `SELECT seq FROM message WHERE conversation_id = $1 AND seq > $2
  AND seq <= $3 AND kind = 1 AND deleted_at IS NULL AND ($4::uuid IS NULL OR sender_id <>
  $4) ORDER BY seq LIMIT 1`, a range scan of the primary key.
- `set_title` is `UPDATE conversation SET title = $2 WHERE conversation_id = $1`
  (0 rows is `ConversationNotFound`).
- Group-wide positions currently cost up to three statements per current member, twice
  per change (up to about 600 for a 100-member group). Compute them in one statement: a
  `LATERAL` subquery per current member running the `first_live_message` scan, joined with
  the members' maximum `device_cursor.delivered_seq`.
- System entry bodies are stored as `bytea` like every body; they are JSON but are never
  queried by the server.
