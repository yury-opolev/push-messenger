# Plan 7A — Message Service: the Core over gRPC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Everything push-domain implements and spec §4–§6 require, served by `push-message-svc` over gRPC for the gateway to call on behalf of an identified device: contacts (request, accept, decline, block, unblock, list), invites (create, redeem, revoke), direct conversations, groups of up to 100 with admins and members (create, add, remove, leave, rename, set role), send, sync, ack, read positions, edit and delete within 48 hours, history, conversation listings with unread counts and receipt positions. message-svc kicks devices on Redis when the worker relays a revocation or a password reset, and serves auth-svc a `Directory` API, behind auth-svc's own token, that says whose profiles a user may see; auth-svc uses it for `GET /v1/users?ids=…`. Review findings are folded in: registrations and joins of one user are serialised by a per-user lock (a duplicate `device.registered` could start a device past messages it never received), every domain error has a status and a client code of its own, error details follow gRPC's rich error model, and sync is documented as a read. Everything is tested against the in-memory store, Postgres and Redis containers. Plan 7B moves the smoke test onto the new API, removes the DEV ONLY `DevSetup` API, updates the chart and verifies it all on minikube.

**Architecture:** The client frames of spec §7.1 that Plan 4 did not define — `Edit`, `Delete`, `ReadUpTo`, and the request frames as one `Request` with a `oneof` body answered by a `Response` — are added to `push/v1/frame.proto`, the client contract; their `Frame` field numbers come with Plan 8, when the gateway carries them. message-svc's `Messaging` gRPC service gains `Edit`, `Delete`, `ReadUpTo` and `Request(RequestCall{user_id, push.v1.Request})`, so the gateway (Plan 8) forwards a request frame as it is and the answer, `Response` or `Error`, already carries its `request_id`; a new module `requests.rs` turns them into domain calls. A failed call is a gRPC status whose `grpc-status-details-bin` is a `google.rpc.Status` holding the client's `push.v1.Error` as a `google.protobuf.Any`. After a relayed revocation or password reset the domain publishes `Event::Kick`, which message-svc's Redis notifier sends as a `Kick{reason}` on `kick:{device_id}`. The new `Directory` service answers `VisibleUsers(user, ids)` from one SQL statement (`Tx::related_users`) and is guarded by `PUSH_DIRECTORY_TOKEN`, a token of its own (`InternalToken` replaces `AccountsToken`), so auth-svc cannot use the worker's `Accounts` API and the worker cannot read relationships. auth-svc calls it through a lazily connected tonic client and keeps its own database: it reads only its own profiles, for the ids message-svc allowed. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §2, §3.2, §4–§7, §12.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85). No new crates in the workspace: `push-auth` now depends on `push-proto` and `tonic` 0.14 (and `tokio-stream` for a test), already in the workspace; `google.rpc.Status` and `google.protobuf.Any` are two small hand-written `prost` messages.

---

## Before you start

- **Plan 6B is merged** (PR #11, `main` at `d9a5180`). This plan changes `push-domain`,
  `push-store`, `push-testkit`, `push-proto`, `push-message-svc` and `push-auth`, with test
  adjustments in `push-gateway`, `push-worker` and `push-smoke`. Plan 7B does the smoke
  test, the chart and minikube.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-07a-message-service
```

- Fix a stale PATH for the current PowerShell session (each new shell needs it again):

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
```

- **Docker must be running.** The tests start `postgres:17-alpine` and `redis:7-alpine`
  containers. A run killed half-way can leave containers behind (`docker ps -a`, then
  `docker rm -f <id>` for the `postgres` and `redis` ones only — never touch other
  containers on this machine).
- **Edit files with the edit tool, and keep them LF** (`.gitattributes`:
  `* text=auto eol=lf`). The file-creation tool may write CRLF; git normalises it on commit.
- **Do not pipe `cargo fmt`.** Run `cargo fmt --all -- --check` on its own and read the exit
  code. The code in this plan is formatted, so a diff means a copying mistake.
- **Replace blocks.** "Replace … with …" pairs below are exact copies of the file before and
  after the change, with two lines of context (more where two would not make the first block
  unique): find the first block in the file and replace it with the second. "Create" blocks
  are whole files.
- Baseline (after Plan 6B): `cargo test --workspace --locked` passes 777 tests.
- **Cargo.lock.** Task 8 adds dependencies to `push-auth`; it first runs
  `cargo test -p push-auth` without `--locked`, which records them; the gates then run with
  `--locked`. Never run `cargo update`.
- **Every task ends green:**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

  Never add `#[allow(...)]`: every item a task adds is used by its code, tests or public
  API.
- **Postgres tests connect to `127.0.0.1`, never `localhost`** (on Windows `localhost`
  tries IPv6 first and every connect costs about 2 s). Testcontainers' Postgres is
  `postgres`/`postgres`.
- **Scope rule:** no gateway behaviour changes (its frames, JWT checks, kicks and close
  codes are Plan 8; only its tests and one `Error` literal change here), no chart, Dockerfile
  or scripts (Plan 7B), and the DEV ONLY `DevSetup` API stays until Plan 7B removes it.

## Key rules the code must preserve

- **Each service owns its database** (spec §2). auth-svc never reads `push_messaging`:
  it asks message-svc's `Directory` API whose profiles a caller may see, then reads those
  profiles from its own `auth` schema.
- **Each internal API has its own token.** `Accounts` takes only the worker's
  (`PUSH_ACCOUNTS_TOKEN`), `Directory` only auth-svc's (`PUSH_DIRECTORY_TOKEN`), both
  compared in constant time; without a configured token the API refuses every call. The
  gateway has neither. Tokens are never logged, shown in `Debug` or echoed in errors.
- **Never acknowledge before commit; events only after commit.** Kicks too: a device is
  kicked after its revocation committed.
- **Redis carries only signals**: `notify:{user_id}` notifications and `kick:{device_id}`
  kicks with a reason, never a body or a profile.
- **A retry with the same `client_msg_id` is looked up before any permission check**
  (spec §4.2), for edits and deletes too: a retry whose first attempt succeeded is never
  refused.
- **Registrations and joins of one user are serialised** (`Tx::lock_users`), so every
  membership of a user gets a delivery position for every device of theirs, and a repeated
  `device.registered` never moves an existing position.
- **Errors never echo secrets or bodies.** Storage errors reach clients as a fixed
  "temporary storage problem"; malformed fields name the field, never its value.
- **After a block nobody can send, edit or delete in that direct conversation**, which
  stays as evidence (owner decision); invites answer one `INVITE_NOT_FOUND` for every
  failure, never revealing a block.

## Decisions made for this plan

These were settled while writing the plan and verified in a worktree. The owner's earlier
decisions apply (blocks freeze the evidence, sending marks as read only what the sending
device received, read implies delivered, admins and members with the longest-standing
member promoted when the last admin leaves, aggregate group receipts, one invite error,
48-hour windows, no "delete for me"). Do not reopen any of this during execution.

1. **Plan 7 is split.** 7A (this plan) delivers the services, tested against containers;
   7B moves the smoke test onto the new API, removes `DevSetup`, updates the chart and
   verifies it on minikube.
2. **The gRPC surface.** `Messaging` keeps `Identify`, `Send`, `Sync` and `Ack` and gains
   `Edit`, `Delete` (both answered with the entry's `SendAck`), `ReadUpTo` and
   `Request(RequestCall)`, which carries any request frame (`push.v1.Request`: a
   `request_id` and a `oneof` body of the 18 requests of spec §7.1) and answers a
   `push.v1.Response`. The messages are defined once, in `frame.proto`, the client
   contract; Plan 8 adds them to `Frame` and the gateway forwards them as they are. One
   `Request` call instead of 18 RPCs keeps the gateway free of per-request code (it is
   transport only, spec §2). message-svc puts the `request_id` in the `Response` and in the
   `Error` of a failed request; for `Send`, `Edit` and `Delete` the gateway adds the
   `client_msg_id` to the error, as it does for `Send` today.
3. **The per-user lock (must-fix).** `Tx::lock_users(&[UserId])` takes transaction-scoped
   advisory locks on a namespaced FNV-1a hash of each user id, sorted and deduplicated, in
   one statement. `register_device` and `device_registered` take their user's lock before
   inserting the device and reading the memberships; `open_direct`, `create_group` and
   `add_members` take the lock of every user they add, once, before listing their devices.
   Lock order: contact pair → conversation (`next_seq`) → users → conversation positions;
   a registration takes only its user's. Redeeming an invite makes contacts and joins no
   conversation, and a rejoin is `add_members`, so these are all the join paths. The
   concurrency case of Task 1 holds the join between its member row and its commit while a
   registration runs, then repeats the registration: without the lock it fails with
   "the new device skipped seq 3: it got []".
4. **Errors are exhaustive and follow gRPC's rich error model.** `domain_error` has no
   catch-all: each of the 22 `DomainError`s maps to a gRPC code and a client `ErrorCode`,
   12 of them new (`USER_NOT_FOUND`, `NO_PENDING_REQUEST`, `BLOCKED`, `TARGET_NOT_FOUND`,
   `NOT_SENDER`, `ALREADY_DELETED`, `WINDOW_EXPIRED`, `INVITE_NOT_FOUND`, `GROUP_FULL`,
   `NOT_A_GROUP`, `NOT_ADMIN`, `LAST_ADMIN`); only the retryable ones share
   `UNAVAILABLE`. `UnknownUser` becomes `NOT_FOUND`/`USER_NOT_FOUND` (it was reported as an
   unknown device). The details are a `google.rpc.Status` whose `details` hold the
   `push.v1.Error` as a `google.protobuf.Any` with type URL
   `type.googleapis.com/push.v1.Error` — the standard content of `grpc-status-details-bin`,
   which grpcurl and every gRPC library can read — rather than a custom metadata key,
   which every tool would need to be taught. The two messages are hand-written `prost`
   structs in `push-proto`: no new crate.
5. **Kicks.** The domain publishes `Event::Kick { devices, reason }` after the commit:
   `device_revoked` kicks the device every time it is relayed (a repeated kick is
   harmless), and the new `password_reset` kicks every active device of the user and
   changes nothing — the devices stay registered (spec §5.4 revokes the refresh tokens; the
   device signs in again). message-svc publishes `Kick { reason }` on `kick:{device_id}`;
   the reason (`DEVICE_REVOKED`, `PASSWORD_RESET`) lets Plan 8 choose the close code.
6. **The contact check behind `GET /v1/users?ids=…` is message-svc's `Directory` API**, a
   separate gRPC service with `VisibleUsers(user_id, user_ids)`, guarded by a token of its
   own (`PUSH_DIRECTORY_TOKEN`) that only auth-svc and message-svc get — not the Accounts
   token, so auth-svc cannot register devices and the worker cannot read relationships. A
   caller may see: themselves; every user they have a contact row for, in any state
   (requests either way, accepted contacts, users they blocked); and every user who is or
   was a member of a conversation they are or were a member of. Someone who blocked the
   caller has deleted the caller's row, so they stay hidden unless they share a
   conversation. At most 100 ids; one SQL statement (`Tx::related_users`).
7. **auth-svc's `GET /v1/users?ids=…`** takes 1 to 100 comma-separated user ids, asks
   `Directory`, and answers `{"users": [{user_id, handle, display_name}]}` for the verified
   users among the allowed ones, sorted by id; other and unknown ids are left out, so the
   answer never says which ids exist. When message-svc does not answer it fails closed with
   a retryable 503 `unavailable`. `PUSH_MESSAGE_SVC_URL` (plain `http://` only, like the
   worker's) and `PUSH_DIRECTORY_TOKEN` are set together or not at all; without them
   auth-svc warns at start-up and the endpoint answers 503. The client connects lazily
   (2 s connect, 5 s call timeout), so auth-svc starts without message-svc. No rate limit:
   ids are random 128-bit values and only related users' profiles are ever shown.
8. **Sync stays a read.** Delivery positions move only on `Ack` (spec §4.7); a sync
   repeated before the client acks returns the same entries, which the proto now says. No
   "after" position is added: it would let a client that never acks be pushed its whole
   backlog, whereas the gateway already skips entries it sent on the connection
   (`Session::fresh`) and syncs again after an ack only when the ack moved the position
   and the last sync left entries behind.
9. **`/readyz` needs no reordering.** `main` binds the gRPC listener before calling
   `serve`, so a client that connects as soon as `/readyz` answers waits in the listen
   backlog until the server accepts, a moment later. `serve` now documents that both
   listeners must be bound already.
10. **`InternalToken` replaces `AccountsToken`**: one interceptor, constructed per API
    (`InternalToken::accounts`, `InternalToken::directory`) with fixed refusal texts.
11. **`ContactState` moves to `push.v1`**, where clients meet it (`Contact`,
    `ContactList`); the internal `ContactChanged` notification uses it, with the same
    numbers, so nothing changes on the wire.
12. **`requests.rs`** holds the client frames beyond `Send` and `Ack`, with its tests in
    `requests/tests.rs`; `grpc.rs` delegates to it.

## Review fixes folded in

| Finding | Where |
|---|---|
| MUST-FIX: a registration and a join of the same user interleave under READ COMMITTED; a repeated `device.registered` then starts the device at the end of the log | Task 1 (decision 3), proven by a Postgres concurrency case that fails without the fix |
| `convert.rs` has a catch-all `DomainError` arm | Task 2: exhaustive, every error its own code |
| No tests for malformed ids on `Identify`, `Sync` and `Ack` | Task 2 |
| No tests for `DevSetup` errors | `DevSetup` is removed in Plan 7B; its replacements, the real requests, test malformed ids, unknown users and self-targets (Tasks 4–6) |
| `grpc-status-details-bin` holds a raw `push.v1.Error` | Task 2: `google.rpc.Status` with an `Any` (decision 4) |
| Sync advances only on an ack: undocumented | Task 2: the `Sync` RPC's comment and a test (decision 8) |
| `/readyz` turns ready before the gRPC serve call | Task 2: checked, the listener is pre-bound; documented (decision 9) |
| The `Identify` oracle (`UNKNOWN_DEVICE` vs `UNAVAILABLE`) | Plan 8, which turns them into close codes (Deferred) |

## Owner decisions needed

The plan proceeds with the recommendation in each case; a different answer changes a line
of documentation (and, for 1, a constant in Plan 8), not this plan's code.

1. **The close code for a password-reset kick.** Spec §7.4 gives 4003 ("another
   connection took over, or device revoked: stop") for revocations. After a password reset
   the device is not revoked but its refresh token is, so the client must sign in again —
   **recommended: 4001** ("sign in again"). Plan 8 applies it; this plan only sends the
   reason.
2. **Profiles of former co-members.** This plan shows a user the profiles of everyone who
   is or was in a conversation with them, so a former group member's name still shows in
   the history. **Recommended: keep.** The alternative, current members only, would leave
   old messages without names.
3. **Groups in the first release.** Spec §13 left open whether group endpoints ship in the
   first release. message-svc serves them from this plan; **recommended: the gateway
   exposes them in Plan 8 too.**

## File structure

```
crates/push-domain/src/store.rs, memory.rs    + Tx::lock_users, Tx::related_users
crates/push-domain/src/service.rs             registrations take the user lock; kicks; password_reset
crates/push-domain/src/conversations.rs, groups.rs   joins take the users' locks
crates/push-domain/src/notify.rs, lib.rs      + Event::Kick, KickReason
crates/push-domain/src/contacts.rs            + visible_users
crates/push-store/src/locks.rs, store.rs      user locks; related_users in one statement
crates/push-testkit/src/concurrency.rs        + a registration racing a join
crates/push-testkit/src/scenarios/devices.rs, contacts.rs   kicks; profile visibility
crates/push-testkit/src/contract/contacts.rs  + related_users
crates/push-proto/proto/push/v1/frame.proto   Edit, Delete, ReadUpTo, Request, Response and
                                                their messages; 12 error codes; request_id
crates/push-proto/proto/push/internal/v1/messaging.proto   Messaging + Edit, Delete,
                                                ReadUpTo, Request; Kick; Directory
crates/push-proto/src/codec.rs, lib.rs        google.rpc.Status details; kick_channel
crates/push-message-svc/src/convert.rs        exhaustive errors; contact_state
crates/push-message-svc/src/requests.rs       NEW: Edit, Delete, ReadUpTo and the requests
crates/push-message-svc/src/requests/tests.rs NEW: the client's frames over the API
crates/push-message-svc/src/grpc.rs           delegations; DirectoryApi; InternalToken
crates/push-message-svc/src/notify.rs         kicks on kick:{device_id}
crates/push-message-svc/src/config.rs, server.rs, lib.rs   PUSH_DIRECTORY_TOKEN; Directory
crates/push-message-svc/tests/service.rs      kicks, the client API and Directory on Postgres
crates/push-auth/Cargo.toml                   + push-proto, tonic; dev: tokio-stream
crates/push-auth/src/directory.rs             NEW: Directory, GrpcDirectory, NoDirectory
crates/push-auth/src/rules.rs, profiles.rs, api.rs   GET /v1/users?ids=…
crates/push-auth/src/config.rs, server.rs, lib.rs    PUSH_MESSAGE_SVC_URL, PUSH_DIRECTORY_TOKEN
crates/push-auth/tests/api.rs                 profiles only for related users
crates/push-gateway/src/session.rs, tests/gateway.rs   the new Error field; the new RPCs
crates/push-worker/tests/relay.rs, server.rs  InternalToken
crates/push-smoke/tests/local.rs              message-svc's new configuration field
README.md, spec, roadmap                      docs
```

---

### Task 1: Registrations and joins of one user are serialised (must-fix)

**Files:**
- Modify: `crates/push-testkit/src/concurrency.rs`, `crates/push-domain/src/store.rs`,
  `src/memory.rs`, `src/service.rs`, `src/conversations.rs`, `src/groups.rs`,
  `crates/push-store/src/locks.rs`, `src/store.rs`

- [ ] **Step 1: The race, as a failing concurrency case**

A holder transaction writes an uncommitted delivery position for Carol's first device, so
`add_members` stops in the middle — after it wrote Carol's member row and listed her
devices, before it commits — while Carol's new phone is relayed; then the relay repeats.
In `crates/push-testkit/src/concurrency.rs`:

Replace

```rust
use std::sync::Arc;

use chrono::TimeDelta;
```

with

```rust
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;
```

Replace

```rust
}

async fn a_deadlock_is_reported_as_a_retryable_conflict<B: Backend>(backend: &B) {
    let store = backend.store().await;
```

with

```rust
}

/// A device relayed while its user joins a conversation must still receive everything
/// from the join on, even when the relay repeats `device.registered` (at least once).
/// Without a lock on the user, each transaction misses the other's uncommitted row: the
/// join finds no new device, the registration no new membership, and the repeated
/// registration then starts the device at the end of the log, past entries it never
/// received.
async fn a_device_registered_during_a_join_misses_nothing<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob]).await;
    world.befriend(&alice, &carol).await;
    let phone = DeviceId::new();

    // An uncommitted position of Carol's first device makes the join wait in the middle:
    // after it has written Carol's member row and listed her devices, before it commits.
    let mut holder = world.store.begin().await.unwrap();
    holder
        .create_cursor_if_absent(carol.device, group, 0)
        .await
        .unwrap();
    let join = {
        let world = Arc::clone(&world);
        tokio::spawn(async move {
            world
                .svc
                .add_members(alice.user, group, &[carol.user])
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let registration = {
        let world = Arc::clone(&world);
        tokio::spawn(async move { world.svc.device_registered(phone, carol.user).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    holder.rollback().await.unwrap();
    join.await.unwrap().unwrap();
    registration.await.unwrap().unwrap();

    let sent = world.send(&alice, group, "after the join").await;
    // The worker relays at least once: the registration arrives again.
    world
        .svc
        .device_registered(phone, carol.user)
        .await
        .unwrap();
    let synced = world.svc.sync(phone, 100).await.unwrap().entries;
    assert!(
        synced.iter().any(|entry| entry.seq == sent.seq),
        "the new device skipped seq {}: it got {:?}",
        sent.seq,
        seqs(&synced)
    );
}

async fn a_deadlock_is_reported_as_a_retryable_conflict<B: Backend>(backend: &B) {
    let store = backend.store().await;
```

Replace

```rust
        concurrent_acks_by_different_members_produce_the_group_receipt,
        concurrent_registrations_of_one_device_give_it_one_owner,
        a_deadlock_is_reported_as_a_retryable_conflict,
    ]
```

with

```rust
        concurrent_acks_by_different_members_produce_the_group_receipt,
        concurrent_registrations_of_one_device_give_it_one_owner,
        a_device_registered_during_a_join_misses_nothing,
        a_deadlock_is_reported_as_a_retryable_conflict,
    ]
```

```powershell
cargo test -p push-store --test postgres -- concurrency::
```

Expected: 8 cases pass and
`concurrency::a_device_registered_during_a_join_misses_nothing` FAILS with
`the new device skipped seq 3: it got []`: the registration did not see the uncommitted
member row, the join did not see the new device, and the repeated registration started
the device after the message.

- [ ] **Step 2: The lock in the storage contract and both stores**

In `crates/push-domain/src/store.rs` (the lock order and why positions are no longer
missing):

Replace

```rust
//! [`Tx::next_seq`] first, acks and reads take only the positions lock.
//!
//! # Delivery positions
//!
//! Positions are created eagerly, so a device normally has one for every membership of its
//! user. One can still be missing: a device registered concurrently with a membership
//! change reads the memberships before the new member row commits. Readers fall back to
//! `visible_from_seq - 1`, so [`Tx::sync_positions`] starts from the memberships and
//! joins the positions to them, never the other way round.
//!
//! [`DomainError::Conflict`]: crate::error::DomainError::Conflict
```

with

```rust
//! [`Tx::next_seq`] first, acks and reads take only the positions lock.
//!
//! User locks ([`Tx::lock_users`]) come after the contact-pair lock (`open_direct`) and
//! the conversation lock (adding members) and before the positions lock; a device
//! registration takes only its user's lock. A transaction takes its user locks in one
//! call, so no two transactions wait for each other's user locks in a cycle.
//!
//! # Delivery positions
//!
//! Positions are created eagerly, so a device has one for every membership of its user:
//! registrations and joins of the same user are serialised by [`Tx::lock_users`]. Readers
//! still treat a missing position as `visible_from_seq - 1`, so [`Tx::sync_positions`]
//! starts from the memberships and joins the positions to them, never the other way
//! round.
//!
//! [`DomainError::Conflict`]: crate::error::DomainError::Conflict
```

Replace

```rust
    /// The order of the result is unspecified.
    async fn devices_of_user(&mut self, user: UserId) -> DomainResult<Vec<DeviceId>>;

    // --- Contacts: one row per direction -----------------------------------------------
```

with

```rust
    /// The order of the result is unspecified.
    async fn devices_of_user(&mut self, user: UserId) -> DomainResult<Vec<DeviceId>>;
    /// Serialises, until the transaction ends, every operation that pairs these users'
    /// devices with their conversations: registering a device of one of them, and making
    /// one of them a member of a conversation. Each such transaction calls it once, before
    /// it reads the devices or the memberships, so a registration and a join of the same
    /// user never miss each other's uncommitted rows. The locks are taken in one fixed
    /// order, whatever the order of `users`; duplicates are ignored.
    /// Postgres: transaction-scoped advisory locks on a namespaced hash of each user id.
    async fn lock_users(&mut self, users: &[UserId]) -> DomainResult<()>;

    // --- Contacts: one row per direction -----------------------------------------------
```

Replace

```rust
    async fn members(&mut self, conversation: ConversationId) -> DomainResult<Vec<Member>>;
    /// Every conversation the user is, or was, a member of.
    /// A device registered concurrently with a membership change may lack a delivery
    /// position; callers treat a missing position as `visible_from_seq - 1`.
    /// Each returned `Conversation` (including `last_seq`) must be read in the same
    /// statement as its `Member` row, so `last_seq` is consistent with the membership.
```

with

```rust
    async fn members(&mut self, conversation: ConversationId) -> DomainResult<Vec<Member>>;
    /// Every conversation the user is, or was, a member of.
    /// Each returned `Conversation` (including `last_seq`) must be read in the same
    /// statement as its `Member` row, so `last_seq` is consistent with the membership.
```

In `crates/push-domain/src/memory.rs`:

Replace

```rust
    }

    async fn get_contact(
        &mut self,
```

with

```rust
    }

    async fn lock_users(&mut self, _users: &[UserId]) -> DomainResult<()> {
        // Every in-memory transaction already holds the store-wide lock.
        Ok(())
    }

    async fn get_contact(
        &mut self,
```

In `crates/push-store/src/locks.rs`:

Replace

```rust
}

#[cfg(test)]
mod tests {
```

with

```rust
}

/// The locks serialising device registrations with conversation joins, one per user,
/// sorted and without duplicates: every transaction takes them in the same order.
pub(crate) fn users(users: &[UserId]) -> Vec<i64> {
    let mut keys: Vec<i64> = users
        .iter()
        .map(|user| fnv1a(b"user-devices", &[user.0]))
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
```

Replace

```rust
            fnv1a(b"contact-pair", &[id])
        );
    }
}
```

with

```rust
            fnv1a(b"contact-pair", &[id])
        );
        assert_ne!(
            users(&[UserId(id)]),
            [conversation_positions(ConversationId(id))]
        );
    }

    #[test]
    fn user_locks_are_sorted_and_unique_in_any_order() {
        let (a, b) = (UserId::new(), UserId::new());
        let keys = users(&[b, a, b]);
        assert_eq!(keys, users(&[a, b]));
        assert_eq!(keys.len(), 2);
        assert!(keys[0] < keys[1]);
    }
}
```

In `crates/push-store/src/store.rs`:

Replace

```rust
    }

    async fn get_contact(
        &mut self,
```

with

```rust
    }

    async fn lock_users(&mut self, users: &[UserId]) -> DomainResult<()> {
        let keys = locks::users(users);
        // A function scan over the array yields its elements in order, so the locks are
        // taken in the sorted order of their keys.
        sqlx::query("SELECT pg_advisory_xact_lock(key) FROM unnest($1::bigint[]) AS key")
            .bind(keys)
            .execute(self.conn())
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn get_contact(
        &mut self,
```

- [ ] **Step 3: Every registration and join takes it**

In `crates/push-domain/src/service.rs`:

Replace

```rust
            return Err(DomainError::UnknownUser);
        }
        if !tx.insert_device(device, user).await? {
            return Err(DomainError::UnknownDevice);
```

with

```rust
            return Err(DomainError::UnknownUser);
        }
        tx.lock_users(&[user]).await?;
        if !tx.insert_device(device, user).await? {
            return Err(DomainError::UnknownDevice);
```

Replace

```rust
        let mut tx = self.store.begin().await?;
        tx.insert_user(user).await?;
        if tx.insert_device(device, user).await? {
            create_cursors(&mut tx, device, user).await?;
```

with

```rust
        let mut tx = self.store.begin().await?;
        tx.insert_user(user).await?;
        tx.lock_users(&[user]).await?;
        if tx.insert_device(device, user).await? {
            create_cursors(&mut tx, device, user).await?;
```

Replace

```rust
/// Starts a new device up to date in every conversation of its user, including ones the
/// user has left (up to where they left).
async fn create_cursors<T: Tx>(tx: &mut T, device: DeviceId, user: UserId) -> DomainResult<()> {
    for (conversation, member) in tx.memberships(user).await? {
```

with

```rust
/// Starts a new device up to date in every conversation of its user, including ones the
/// user has left (up to where they left). The caller holds the user's lock
/// ([`Tx::lock_users`]), so no membership of the user is being created meanwhile: every
/// membership gets a position, here or by the join.
async fn create_cursors<T: Tx>(tx: &mut T, device: DeviceId, user: UserId) -> DomainResult<()> {
    for (conversation, member) in tx.memberships(user).await? {
```

In `crates/push-domain/src/conversations.rs`:

Replace

```rust
        }

        for user in [me, peer] {
            tx.insert_member(Member {
```

with

```rust
        }

        tx.lock_users(&[me, peer]).await?;
        for user in [me, peer] {
            tx.insert_member(Member {
```

In `crates/push-domain/src/groups.rs`:

Replace

```rust
        tx.insert_entry(system_entry(id, seq, &created, now))
            .await?;
        join(&mut tx, id, creator, Role::Admin, seq).await?;
        for other in &others {
```

with

```rust
        tx.insert_entry(system_entry(id, seq, &created, now))
            .await?;
        let mut users = others.clone();
        users.push(creator);
        tx.lock_users(&users).await?;
        join(&mut tx, id, creator, Role::Admin, seq).await?;
        for other in &others {
```

Replace

```rust
        tx.commit().await?;

        let mut users = others;
        users.push(creator);
        users.sort();
        self.publish(vec![Event::EntriesAvailable { users }]).await;
```

with

```rust
        tx.commit().await?;

        users.sort();
        self.publish(vec![Event::EntriesAvailable { users }]).await;
```

Replace

```rust
            users: added.clone(),
        };
        let seq = change.append(&event).await?;
        for user in added {
```

with

```rust
            users: added.clone(),
        };
        change.tx.lock_users(&added).await?;
        let seq = change.append(&event).await?;
        for user in added {
```

Replace

```rust
/// Nothing before it is visible or unread: the read position starts at `seq - 1`, and so
/// does every active device's delivery position. A device that already has a position (a
/// member rejoining) keeps it; readers never look below `visible_from_seq - 1`.
async fn join<T: Tx>(
    tx: &mut T,
```

with

```rust
/// Nothing before it is visible or unread: the read position starts at `seq - 1`, and so
/// does every active device's delivery position. A device that already has a position (a
/// member rejoining) keeps it; readers never look below `visible_from_seq - 1`. The
/// caller holds `user`'s lock ([`Tx::lock_users`]), so no device of theirs is being
/// registered meanwhile.
async fn join<T: Tx>(
    tx: &mut T,
```

```powershell
cargo test -p push-store
```

Expected: everything passes, the 9 concurrency cases and the new unit test
`user_locks_are_sorted_and_unique_in_any_order` included. The in-memory suite needs no
change: its transactions are serialised already.

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "fix(domain): serialise device registrations with joins per user"
```

Expected: 779 tests pass, no warnings, no diff.

---

### Task 2: Exhaustive errors as `google.rpc.Status` details; malformed ids; sync and readiness documented

**Files:**
- Modify: `crates/push-proto/proto/push/v1/frame.proto`,
  `proto/push/internal/v1/messaging.proto`, `crates/push-proto/src/codec.rs`, `src/lib.rs`,
  `crates/push-message-svc/src/convert.rs`, `src/grpc.rs`, `src/server.rs`

- [ ] **Step 1: The tests, failing**

The details are a `google.rpc.Status` with the error as an `Any`; a bare `push.v1.Error`
or an `Any` of another type is no client error. In `crates/push-proto/src/codec.rs`:

Replace

```rust
/// A gRPC status carrying `error` for the client, as message-svc answers.
pub fn error_status(code: tonic::Code, error: Error) -> tonic::Status {
    let message = error.message.clone();
    tonic::Status::with_details(code, message, error.encode_to_vec().into())
}
```

with

```rust
/// A gRPC status carrying `error` for the client, as message-svc answers.
///
/// The details (`grpc-status-details-bin`) follow gRPC's rich error model: a
/// `google.rpc.Status` whose `details` hold the `push.v1.Error` as a `google.protobuf.Any`
/// (type URL [`ERROR_TYPE_URL`]), so any gRPC tool can read them.
pub fn error_status(code: tonic::Code, error: Error) -> tonic::Status {
    let message = error.message.clone();
    let details = RpcStatus {
        code: code as i32,
        message: message.clone(),
        details: vec![Any {
            type_url: ERROR_TYPE_URL.to_owned(),
            value: error.encode_to_vec(),
        }],
    };
    tonic::Status::with_details(code, message, details.encode_to_vec().into())
}
```

Replace

```rust
/// unreachable, or failed before answering — becomes a retryable `UNAVAILABLE`.
pub fn error_from_status(status: &tonic::Status) -> Error {
    match Error::decode(status.details()) {
        Ok(error) if error.code() != ErrorCode::Unspecified => error,
        _ => Error {
            code: ErrorCode::Unavailable.into(),
```

with

```rust
/// unreachable, or failed before answering — becomes a retryable `UNAVAILABLE`.
pub fn error_from_status(status: &tonic::Status) -> Error {
    let carried = RpcStatus::decode(status.details())
        .ok()
        .and_then(|rich| {
            rich.details
                .into_iter()
                .find(|detail| detail.type_url == ERROR_TYPE_URL)
        })
        .and_then(|detail| Error::decode(detail.value.as_slice()).ok());
    match carried {
        Some(error) if error.code() != ErrorCode::Unspecified => error,
        _ => Error {
            code: ErrorCode::Unavailable.into(),
```

Replace

```rust
}

#[cfg(test)]
mod tests {
```

with

```rust
}

/// The type URL of a `push.v1.Error` packed in a `google.protobuf.Any`.
pub const ERROR_TYPE_URL: &str = "type.googleapis.com/push.v1.Error";

/// `google.rpc.Status`: what gRPC's rich error model puts in `grpc-status-details-bin`.
#[derive(Clone, PartialEq, prost::Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<Any>,
}

/// `google.protobuf.Any`.
#[derive(Clone, PartialEq, prost::Message)]
struct Any {
    #[prost(string, tag = "1")]
    type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
}

#[cfg(test)]
mod tests {
```

Replace

```rust
        assert!(error.retryable);
    }
}
```

with

```rust
        assert!(error.retryable);
    }

    #[test]
    fn the_details_are_a_google_rpc_status_with_the_error_as_any() {
        let error = Error {
            code: ErrorCode::NotMember.into(),
            message: "not a member of this conversation".to_owned(),
            retryable: false,
            client_msg_id: None,
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
        let rich = RpcStatus::decode(status.details()).unwrap();
        assert_eq!(rich.code, tonic::Code::FailedPrecondition as i32);
        assert_eq!(rich.message, "not a member of this conversation");
        assert_eq!(rich.details.len(), 1);
        assert_eq!(rich.details[0].type_url, ERROR_TYPE_URL);
        assert_eq!(Error::decode(rich.details[0].value.as_slice()), Ok(error));

        // A bare push.v1.Error, or an Any of another type, is not a client error.
        let bare = tonic::Status::with_details(
            tonic::Code::FailedPrecondition,
            "bare",
            Error {
                code: ErrorCode::NotMember.into(),
                ..Error::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert_eq!(error_from_status(&bare).code(), ErrorCode::Unavailable);
        let other = RpcStatus {
            code: tonic::Code::FailedPrecondition as i32,
            message: "other".to_owned(),
            details: vec![Any {
                type_url: "type.googleapis.com/google.rpc.ErrorInfo".to_owned(),
                value: rich.details[0].value.clone(),
            }],
        };
        let other = tonic::Status::with_details(
            tonic::Code::FailedPrecondition,
            "other",
            other.encode_to_vec().into(),
        );
        assert_eq!(error_from_status(&other).code(), ErrorCode::Unavailable);
    }
}
```

Every domain error has its own status and client code; only the retryable ones share one.
Replace the test `domain_errors_become_client_errors` in
`crates/push-message-svc/src/convert.rs`, together with the mapping it tests (decision 4):

Replace

```rust
}

/// The status for a failed domain operation. Storage problems are reported without their
/// details, which the store has already logged.
pub(crate) fn domain_error(error: DomainError) -> Status {
    let (code, error_code) = match &error {
        DomainError::Invalid(_) => (Code::InvalidArgument, ErrorCode::Invalid),
        DomainError::UnknownUser | DomainError::UnknownDevice => {
            (Code::PermissionDenied, ErrorCode::UnknownDevice)
        }
        DomainError::DeviceNotKnownYet => (Code::Unavailable, ErrorCode::Unavailable),
        DomainError::ConversationNotFound => (Code::NotFound, ErrorCode::ConversationNotFound),
        DomainError::NotMember => (Code::FailedPrecondition, ErrorCode::NotMember),
        DomainError::NotContact => (Code::FailedPrecondition, ErrorCode::NotContact),
        DomainError::ClientIdConflict => (Code::AlreadyExists, ErrorCode::ClientIdConflict),
        DomainError::SeqOutOfRange => (Code::OutOfRange, ErrorCode::SeqOutOfRange),
        DomainError::Conflict => (Code::Aborted, ErrorCode::Unavailable),
        DomainError::Store(_) => (Code::Unavailable, ErrorCode::Unavailable),
        _ => (Code::FailedPrecondition, ErrorCode::Rejected),
    };
    let message = match &error {
```

with

```rust
}

/// The status for a failed domain operation: every [`DomainError`] has a gRPC code and a
/// client error code of its own (this match has no catch-all, so a new error must be
/// mapped). Storage problems are reported without their details, which the store has
/// already logged.
pub(crate) fn domain_error(error: DomainError) -> Status {
    let (code, error_code) = match &error {
        DomainError::Invalid(_) => (Code::InvalidArgument, ErrorCode::Invalid),
        DomainError::UnknownUser => (Code::NotFound, ErrorCode::UserNotFound),
        DomainError::UnknownDevice => (Code::PermissionDenied, ErrorCode::UnknownDevice),
        DomainError::DeviceNotKnownYet => (Code::Unavailable, ErrorCode::Unavailable),
        DomainError::ConversationNotFound => (Code::NotFound, ErrorCode::ConversationNotFound),
        DomainError::NotMember => (Code::FailedPrecondition, ErrorCode::NotMember),
        DomainError::NotContact => (Code::FailedPrecondition, ErrorCode::NotContact),
        DomainError::NoPendingRequest => (Code::FailedPrecondition, ErrorCode::NoPendingRequest),
        DomainError::Blocked => (Code::FailedPrecondition, ErrorCode::Blocked),
        DomainError::ClientIdConflict => (Code::AlreadyExists, ErrorCode::ClientIdConflict),
        DomainError::SeqOutOfRange => (Code::OutOfRange, ErrorCode::SeqOutOfRange),
        DomainError::TargetNotFound => (Code::NotFound, ErrorCode::TargetNotFound),
        DomainError::NotSender => (Code::PermissionDenied, ErrorCode::NotSender),
        DomainError::AlreadyDeleted => (Code::FailedPrecondition, ErrorCode::AlreadyDeleted),
        DomainError::WindowExpired => (Code::FailedPrecondition, ErrorCode::WindowExpired),
        DomainError::InviteNotFound => (Code::NotFound, ErrorCode::InviteNotFound),
        DomainError::GroupFull => (Code::FailedPrecondition, ErrorCode::GroupFull),
        DomainError::NotAGroup => (Code::FailedPrecondition, ErrorCode::NotAGroup),
        DomainError::NotAdmin => (Code::PermissionDenied, ErrorCode::NotAdmin),
        DomainError::LastAdmin => (Code::FailedPrecondition, ErrorCode::LastAdmin),
        DomainError::Conflict => (Code::Aborted, ErrorCode::Unavailable),
        DomainError::Store(_) => (Code::Unavailable, ErrorCode::Unavailable),
    };
    let message = match &error {
```

Replace

```rust
    #[test]
    fn domain_errors_become_client_errors() {
        let status = domain_error(DomainError::NotContact);
        assert_eq!(status.code(), Code::FailedPrecondition);
        let error = error_from_status(&status);
        assert_eq!(error.code(), ErrorCode::NotContact);
        assert_eq!(error.message, "the users are not accepted contacts");
        assert!(!error.retryable);

        let conflict = error_from_status(&domain_error(DomainError::Conflict));
        assert_eq!(conflict.code(), ErrorCode::Unavailable);
        assert!(conflict.retryable);

        let store = domain_error(DomainError::Store("relation missing".into()));
        assert_eq!(store.code(), Code::Unavailable);
        let store = error_from_status(&store);
        assert!(store.retryable);
        assert!(!store.message.contains("relation"), "{}", store.message);

        let device = domain_error(DomainError::UnknownDevice);
        assert_eq!(device.code(), Code::PermissionDenied);
        assert_eq!(error_from_status(&device).code(), ErrorCode::UnknownDevice);

        let not_yet = domain_error(DomainError::DeviceNotKnownYet);
        assert_eq!(not_yet.code(), Code::Unavailable);
        let not_yet = error_from_status(&not_yet);
        assert_eq!(not_yet.code(), ErrorCode::Unavailable);
        assert!(not_yet.retryable);
    }
}
```

with

```rust
    #[test]
    fn every_domain_error_has_its_own_status_and_client_error() {
        use push_domain::ValidationError;

        let cases = [
            (
                DomainError::Invalid(ValidationError::EmptyBody),
                Code::InvalidArgument,
                ErrorCode::Invalid,
            ),
            (
                DomainError::UnknownUser,
                Code::NotFound,
                ErrorCode::UserNotFound,
            ),
            (
                DomainError::UnknownDevice,
                Code::PermissionDenied,
                ErrorCode::UnknownDevice,
            ),
            (
                DomainError::DeviceNotKnownYet,
                Code::Unavailable,
                ErrorCode::Unavailable,
            ),
            (
                DomainError::ConversationNotFound,
                Code::NotFound,
                ErrorCode::ConversationNotFound,
            ),
            (
                DomainError::NotMember,
                Code::FailedPrecondition,
                ErrorCode::NotMember,
            ),
            (
                DomainError::NotContact,
                Code::FailedPrecondition,
                ErrorCode::NotContact,
            ),
            (
                DomainError::NoPendingRequest,
                Code::FailedPrecondition,
                ErrorCode::NoPendingRequest,
            ),
            (
                DomainError::Blocked,
                Code::FailedPrecondition,
                ErrorCode::Blocked,
            ),
            (
                DomainError::ClientIdConflict,
                Code::AlreadyExists,
                ErrorCode::ClientIdConflict,
            ),
            (
                DomainError::SeqOutOfRange,
                Code::OutOfRange,
                ErrorCode::SeqOutOfRange,
            ),
            (
                DomainError::TargetNotFound,
                Code::NotFound,
                ErrorCode::TargetNotFound,
            ),
            (
                DomainError::NotSender,
                Code::PermissionDenied,
                ErrorCode::NotSender,
            ),
            (
                DomainError::AlreadyDeleted,
                Code::FailedPrecondition,
                ErrorCode::AlreadyDeleted,
            ),
            (
                DomainError::WindowExpired,
                Code::FailedPrecondition,
                ErrorCode::WindowExpired,
            ),
            (
                DomainError::InviteNotFound,
                Code::NotFound,
                ErrorCode::InviteNotFound,
            ),
            (
                DomainError::GroupFull,
                Code::FailedPrecondition,
                ErrorCode::GroupFull,
            ),
            (
                DomainError::NotAGroup,
                Code::FailedPrecondition,
                ErrorCode::NotAGroup,
            ),
            (
                DomainError::NotAdmin,
                Code::PermissionDenied,
                ErrorCode::NotAdmin,
            ),
            (
                DomainError::LastAdmin,
                Code::FailedPrecondition,
                ErrorCode::LastAdmin,
            ),
            (DomainError::Conflict, Code::Aborted, ErrorCode::Unavailable),
            (
                DomainError::Store("relation missing".into()),
                Code::Unavailable,
                ErrorCode::Unavailable,
            ),
        ];
        let mut client_codes = std::collections::HashSet::new();
        for (domain, code, error_code) in cases {
            let retryable = matches!(
                domain,
                DomainError::DeviceNotKnownYet | DomainError::Conflict | DomainError::Store(_)
            );
            let message = domain.to_string();
            let status = domain_error(domain);
            assert_eq!(status.code(), code, "{message}");
            let error = error_from_status(&status);
            assert_eq!(error.code(), error_code, "{message}");
            assert_eq!(error.retryable, retryable, "{message}");
            client_codes.insert(error_code);
        }
        // Only the retryable errors share a client code.
        assert_eq!(client_codes.len(), 20);

        let store = error_from_status(&domain_error(DomainError::Store("relation".into())));
        assert!(!store.message.contains("relation"), "{}", store.message);
        let contact = error_from_status(&domain_error(DomainError::NotContact));
        assert_eq!(contact.message, "the users are not accepted contacts");
    }
}
```

Malformed ids on `Identify`, `Sync` and `Ack` name their field, and a sync repeated without
an ack returns the same entries. In `crates/push-message-svc/src/grpc.rs`:

Replace

```rust
    }

    fn account_device(user: UserId, device: DeviceId) -> Request<AccountDevice> {
        Request::new(AccountDevice {
```

with

```rust
    }

    /// The error a call answered with, which must be `INVALID_ARGUMENT` naming `field`.
    fn assert_invalid(status: Status, field: &str) {
        assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
        let error = error_from_status(&status);
        assert_eq!(error.code(), ErrorCode::Invalid);
        assert!(!error.retryable);
        assert!(error.message.starts_with(field), "{}", error.message);
    }

    #[tokio::test]
    async fn malformed_ids_on_identify_sync_and_ack_are_invalid() {
        let world = world();
        let (_alice, bob, conversation) = chat(&world).await;
        let short = vec![1, 2, 3];

        let identify = |user_id: Vec<u8>, device_id: Vec<u8>| {
            Request::new(IdentifyRequest { user_id, device_id })
        };
        let status = world
            .api
            .identify(identify(short.clone(), id_bytes(bob.device.0)))
            .await
            .unwrap_err();
        assert_invalid(status, "user_id");
        let status = world
            .api
            .identify(identify(id_bytes(bob.user.0), Vec::new()))
            .await
            .unwrap_err();
        assert_invalid(status, "device_id");

        let status = world
            .api
            .sync(Request::new(SyncRequest {
                device_id: short.clone(),
                limit: 0,
            }))
            .await
            .unwrap_err();
        assert_invalid(status, "device_id");

        let ack = |device_id: Vec<u8>, conversation_id: Vec<u8>| {
            Request::new(AckRequest {
                device_id,
                ack: Some(Ack {
                    conversation_id,
                    seq: 0,
                }),
            })
        };
        let status = world
            .api
            .ack(ack(short.clone(), id_bytes(conversation.0)))
            .await
            .unwrap_err();
        assert_invalid(status, "device_id");
        let status = world
            .api
            .ack(ack(id_bytes(bob.device.0), short))
            .await
            .unwrap_err();
        assert_invalid(status, "conversation_id");
        let status = world
            .api
            .ack(Request::new(AckRequest {
                device_id: id_bytes(bob.device.0),
                ack: None,
            }))
            .await
            .unwrap_err();
        assert_invalid(status, "ack");
    }

    #[tokio::test]
    async fn a_sync_without_an_ack_returns_the_same_entries() {
        let world = world();
        let (alice, bob, conversation) = chat(&world).await;
        for body in ["one", "two"] {
            let request = send_request(&alice, conversation, body);
            world.api.send(Request::new(request)).await.unwrap();
        }
        let first = sync(&world, bob.device, 0).await;
        assert_eq!(first.entries.len(), 2);
        assert_eq!(sync(&world, bob.device, 0).await, first);
    }

    fn account_device(user: UserId, device: DeviceId) -> Request<AccountDevice> {
        Request::new(AccountDevice {
```

```powershell
cargo test -p push-proto -p push-message-svc --lib
```

Expected: compile errors — no `ErrorCode::UserNotFound` and the other new codes, which
`frame.proto` does not have yet.

- [ ] **Step 2: The new codes, the rich error model and the docs**

In `crates/push-proto/proto/push/v1/frame.proto`:

Replace

```protobuf
  // Any other rejection; `message` says which.
  ERROR_CODE_REJECTED = 10;
}
```

with

```protobuf
  // Any other rejection; `message` says which.
  ERROR_CODE_REJECTED = 10;
  // The other user does not exist (or has not verified their email yet).
  ERROR_CODE_USER_NOT_FOUND = 11;
  // Accept or Decline without a request from that user.
  ERROR_CODE_NO_PENDING_REQUEST = 12;
  // The caller has blocked the other user.
  ERROR_CODE_BLOCKED = 13;
  // Edit or Delete: no such message in the conversation.
  ERROR_CODE_TARGET_NOT_FOUND = 14;
  // Edit or Delete of someone else's message.
  ERROR_CODE_NOT_SENDER = 15;
  ERROR_CODE_ALREADY_DELETED = 16;
  // Edit or Delete after the allowed time (48 hours by default).
  ERROR_CODE_WINDOW_EXPIRED = 17;
  // Unknown, revoked, expired or used up: one answer for all, so redeeming reveals nothing.
  ERROR_CODE_INVITE_NOT_FOUND = 18;
  // A group has at most 100 members.
  ERROR_CODE_GROUP_FULL = 19;
  ERROR_CODE_NOT_A_GROUP = 20;
  ERROR_CODE_NOT_ADMIN = 21;
  // A group must keep at least one admin.
  ERROR_CODE_LAST_ADMIN = 22;
}
```

In `crates/push-proto/proto/push/internal/v1/messaging.proto` (the details format, and
sync as a read — decision 8):

Replace

```protobuf
// device the gateway verified, or auth-svc announced; message-svc trusts them.
//
// Errors are gRPC statuses whose `details` hold an encoded `push.v1.Error`, which the
// gateway forwards to the client.

syntax = "proto3";
```

with

```protobuf
// device the gateway verified, or auth-svc announced; message-svc trusts them.
//
// Errors are gRPC statuses whose details (`grpc-status-details-bin`) follow gRPC's rich
// error model: a `google.rpc.Status` with the client's `push.v1.Error` packed as a
// `google.protobuf.Any` (type URL `type.googleapis.com/push.v1.Error`). The gateway
// forwards that error to the client.

syntax = "proto3";
```

Replace

```protobuf
  // Appends a text message; answers after commit.
  rpc Send(SendRequest) returns (push.v1.SendAck);
  // Entries after the device's delivery positions, oldest first per conversation.
  rpc Sync(SyncRequest) returns (SyncResponse);
  // Records that the device holds everything up to a seq.
```

with

```protobuf
  // Appends a text message; answers after commit.
  rpc Send(SendRequest) returns (push.v1.SendAck);
  // Entries after the device's delivery positions, oldest first per conversation. A read:
  // it never moves a position. Positions move only on Ack (spec §4.7), so a Sync repeated
  // before the client acks returns the same entries again; a caller that syncs on every
  // notification skips the entries it already sent on the connection (the gateway does).
  rpc Sync(SyncRequest) returns (SyncResponse);
  // Records that the device holds everything up to a seq.
```

In `crates/push-proto/src/lib.rs`:

Replace

```rust
pub use codec::{
    FrameError, InvalidId, MAX_FRAME_BYTES, decode_frame, encode_frame, error_from_status,
    error_status, id, id_bytes, notify_channel,
};
```

with

```rust
pub use codec::{
    ERROR_TYPE_URL, FrameError, InvalidId, MAX_FRAME_BYTES, decode_frame, encode_frame,
    error_from_status, error_status, id, id_bytes, notify_channel,
};
```

In `crates/push-message-svc/src/server.rs` (decision 9):

Replace

```rust
/// Runs the service until `shutdown` is cancelled, then finishes the calls in progress
/// and closes the database pool.
///
/// The health endpoints answer at once: `/healthz` always, `/readyz` only once the
/// database is migrated and answers a ping, and never after shutdown began. Postgres and
/// Redis are retried until they answer, so the service may start before them.
pub async fn serve(
    config: Config,
```

with

```rust
/// Runs the service until `shutdown` is cancelled, then finishes the calls in progress
/// and closes the database pool. `grpc` and `health` must be bound already.
///
/// The health endpoints answer at once: `/healthz` always, `/readyz` only once the
/// database is migrated and answers a ping and Redis is connected, and never after
/// shutdown began. Postgres and Redis are retried until they answer, so the service may
/// start before them.
pub async fn serve(
    config: Config,
```

Replace

```rust
    let _ = ready.store.set(store.clone());

    let messaging = Arc::new(Messaging::new(store.clone(), SystemClock, notifier));
    let dev_setup = config
```

with

```rust
    let _ = ready.store.set(store.clone());

    // Ready before `serve` is called: `grpc` is already bound and listening (the caller
    // binds it, as `main` does), so a client that connects as soon as /readyz answers
    // waits in the listen backlog until the server accepts, a moment later.
    let messaging = Arc::new(Messaging::new(store.clone(), SystemClock, notifier));
    let dev_setup = config
```

```powershell
cargo test -p push-proto -p push-message-svc -p push-gateway
```

Expected: everything passes, among them `the_details_are_a_google_rpc_status_with_the_error_as_any`,
`every_domain_error_has_its_own_status_and_client_error`,
`malformed_ids_on_identify_sync_and_ack_are_invalid` and
`a_sync_without_an_ack_returns_the_same_entries`; the gateway reads the new details
through `error_from_status`, unchanged.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "fix(message-svc): exhaustive errors as google.rpc.Status details"
```

Expected: 782 tests pass, no warnings, no diff.

---

### Task 3: Kicks on relayed revocations and password resets

**Files:**
- Modify: `crates/push-testkit/src/scenarios/devices.rs`,
  `crates/push-domain/src/notify.rs`, `src/lib.rs`, `src/service.rs`,
  `crates/push-proto/proto/push/internal/v1/messaging.proto`,
  `crates/push-proto/src/codec.rs`, `src/lib.rs`, `crates/push-message-svc/src/notify.rs`,
  `src/grpc.rs`, `crates/push-message-svc/tests/service.rs`

- [ ] **Step 1: The scenarios, failing**

A relayed revocation kicks the device every time, a refused one kicks nobody; a password
reset kicks every active device of the user and leaves them registered (decision 5). In
`crates/push-testkit/src/scenarios/devices.rs`:

Replace

```rust
use crate::{Backend, Case, World, bodies, case_list};
use push_domain::{DeviceId, DomainError, UserId};

async fn registering_a_device_for_an_unknown_user_fails<B: Backend>(backend: &B) {
```

with

```rust
use crate::{Backend, Case, World, bodies, case_list};
use push_domain::{DeviceId, DomainError, Event, KickReason, UserId};

async fn registering_a_device_for_an_unknown_user_fails<B: Backend>(backend: &B) {
```

Replace

```rust
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

with

```rust
}

async fn a_relayed_revocation_kicks_the_device<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let tablet = world.add_device(&alice).await;
    world.notifier.take();
    world.svc.device_revoked(tablet, alice.user).await.unwrap();
    // Every copy of the event kicks again: harmless, and nothing is lost if the first
    // kick was.
    world.svc.device_revoked(tablet, alice.user).await.unwrap();
    let kick = Event::Kick {
        devices: vec![tablet],
        reason: KickReason::DeviceRevoked,
    };
    assert_eq!(world.notifier.take(), vec![kick.clone(), kick]);

    // A refused revocation kicks nobody.
    let bob = world.person().await;
    world.notifier.take();
    assert!(
        world
            .svc
            .device_revoked(bob.device, alice.user)
            .await
            .is_err()
    );
    assert_eq!(world.notifier.take(), []);
}

async fn a_password_reset_kicks_every_active_device_of_the_user<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let laptop = world.add_device(&alice).await;
    let revoked = world.add_device(&alice).await;
    world.svc.device_revoked(revoked, alice.user).await.unwrap();
    world.notifier.take();

    world.svc.password_reset(alice.user).await.unwrap();
    let mut devices = vec![alice.device, laptop];
    devices.sort();
    assert_eq!(
        world.notifier.take(),
        [Event::Kick {
            devices,
            reason: KickReason::PasswordReset,
        }]
    );
    // The devices stay registered: a new sign-in on them works.
    assert_eq!(world.svc.identify(laptop, alice.user).await, Ok(()));
    assert_eq!(world.svc.identify(bob.device, bob.user).await, Ok(()));

    // A user message-svc has not heard of has nothing to kick.
    world.svc.password_reset(UserId::new()).await.unwrap();
    assert_eq!(world.notifier.take(), []);
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

Replace

```rust
        a_relayed_revocation_closes_an_active_device,
        relayed_events_for_another_users_device_are_refused,
    ]
}
```

with

```rust
        a_relayed_revocation_closes_an_active_device,
        relayed_events_for_another_users_device_are_refused,
        a_relayed_revocation_kicks_the_device,
        a_password_reset_kicks_every_active_device_of_the_user,
    ]
}
```

```powershell
cargo test -p push-testkit
```

Expected: compile errors — no `Event::Kick`, no `KickReason`, no method `password_reset`.

- [ ] **Step 2: The domain**

In `crates/push-domain/src/notify.rs`:

Replace

```rust
//!
//! After a commit, the service publishes typed events: which users have new log entries,
//! whose delivery or read position moved, and whose contacts changed. Events are best
//! effort and ephemeral. They carry no message bodies, and losing one only delays a
//! client, because everything they announce is durable and can be read back:
//!
//! - new entries through `sync`;
```

with

```rust
//!
//! After a commit, the service publishes typed events: which users have new log entries,
//! whose delivery or read position moved, whose contacts changed, and which devices must
//! disconnect. Events are best effort and ephemeral. They carry no message bodies, and
//! losing one only delays a client, because everything they announce is durable and can
//! be read back:
//!
//! - new entries through `sync`;
```

Replace

```rust
//!   appended). Receipt positions do not move `updated_at`, so the busy conversation row
//!   is not written on every ack and read. A reconnecting client refreshes its ticks with
//!   one listing without `changed_since`.

use std::sync::{Arc, Mutex};
```

with

```rust
//!   appended). Receipt positions do not move `updated_at`, so the busy conversation row
//!   is not written on every ack and read. A reconnecting client refreshes its ticks with
//!   one listing without `changed_since`;
//! - a kick through `identify`, which refuses a revoked device, and through the access
//!   token, which a password reset stops renewing: a lost kick leaves a socket open until
//!   the gateway next checks the device or the token (Plan 8).

use std::sync::{Arc, Mutex};
```

Replace

```rust
use async_trait::async_trait;

use crate::ids::{ConversationId, UserId};
use crate::model::ContactState;
```

with

```rust
use async_trait::async_trait;

use crate::ids::{ConversationId, DeviceId, UserId};
use crate::model::ContactState;
```

Replace

```rust
        read_seq: i64,
    },
}
```

with

```rust
        read_seq: i64,
    },
    /// These devices must be disconnected at once (spec §5.3, §5.4). Published to the
    /// devices, not to their users. Repeating a kick is harmless.
    Kick {
        /// Sorted.
        devices: Vec<DeviceId>,
        reason: KickReason,
    },
}

/// Why devices are kicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickReason {
    /// The device was revoked: it must stop.
    DeviceRevoked,
    /// The user's password was reset: every refresh token is revoked, so the device must
    /// sign in again.
    PasswordReset,
}
```

In `crates/push-domain/src/lib.rs`:

Replace

```rust
    Invite, InviteRecord, Member, MemberState, Positions, Receipts, Role, SendReceipt, direct_key,
};
pub use notify::{Event, NoopNotifier, Notifier, ReceiptUpdate, RecordingNotifier};
pub use send::SendText;
pub use service::{Messaging, Policy};
```

with

```rust
    Invite, InviteRecord, Member, MemberState, Positions, Receipts, Role, SendReceipt, direct_key,
};
pub use notify::{Event, KickReason, NoopNotifier, Notifier, ReceiptUpdate, RecordingNotifier};
pub use send::SendText;
pub use service::{Messaging, Policy};
```

In `crates/push-domain/src/service.rs`:

Replace

```rust
use crate::ids::{DeviceId, UserId};
use crate::model::Member;
use crate::notify::{Event, Notifier};
use crate::store::{Store, Tx};
```

with

```rust
use crate::ids::{DeviceId, UserId};
use crate::model::Member;
use crate::notify::{Event, KickReason, Notifier};
use crate::store::{Store, Tx};
```

Replace

```rust
    /// Idempotent and correct in any order: a revocation that arrives before the
    /// registration records the device as revoked, so the registration cannot bring it
    /// back. A device id that belongs to another user is `UnknownDevice`.
    pub async fn device_revoked(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
```

with

```rust
    /// Idempotent and correct in any order: a revocation that arrives before the
    /// registration records the device as revoked, so the registration cannot bring it
    /// back. A device id that belongs to another user is `UnknownDevice`. After the commit
    /// the device is kicked, every time: a repeated kick is harmless.
    pub async fn device_revoked(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
```

Replace

```rust
        }
        tx.revoke_device(device).await?;
        tx.commit().await
    }
```

with

```rust
        }
        tx.revoke_device(device).await?;
        tx.commit().await?;
        self.publish(vec![Event::Kick {
            devices: vec![device],
            reason: KickReason::DeviceRevoked,
        }])
        .await;
        Ok(())
    }

    /// auth-svc reset `user`'s password (`user.password_reset`, relayed by the worker):
    /// every active device of the user is kicked, to sign in again with the new password.
    /// The devices stay registered. Changes nothing, so a repeated event only kicks again;
    /// a user message-svc has not heard of has no devices to kick.
    pub async fn password_reset(&self, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        let mut devices = tx.devices_of_user(user).await?;
        tx.rollback().await?;
        if !devices.is_empty() {
            devices.sort();
            self.publish(vec![Event::Kick {
                devices,
                reason: KickReason::PasswordReset,
            }])
            .await;
        }
        Ok(())
    }
```

```powershell
cargo test -p push-domain -p push-testkit
```

Expected: the in-memory suite passes, the 2 new scenarios included.

- [ ] **Step 3: Kicks on Redis**

The `Kick` message and its channel. In
`crates/push-proto/proto/push/internal/v1/messaging.proto`:

Replace

```protobuf
  rpc DeviceRegistered(AccountDevice) returns (AccountsResponse);
  // device.revoked: Identify refuses the device from now on, even when this arrives
  // before the registration.
  rpc DeviceRevoked(AccountDevice) returns (AccountsResponse);
  // user.password_reset: every device of the user is to be kicked. Accepted and
  // otherwise ignored until message-svc publishes kicks (Plan 7) and the gateway closes
  // kicked sockets and refuses older tokens (Plan 8).
  rpc PasswordReset(AccountUser) returns (AccountsResponse);
}
```

with

```protobuf
  rpc DeviceRegistered(AccountDevice) returns (AccountsResponse);
  // device.revoked: Identify refuses the device from now on, even when this arrives
  // before the registration; the device is kicked (`kick:{device_id}`).
  rpc DeviceRevoked(AccountDevice) returns (AccountsResponse);
  // user.password_reset: every active device of the user is kicked
  // (`kick:{device_id}`), to sign in again. The gateway closes kicked sockets and refuses
  // tokens older than the reset in Plan 8.
  rpc PasswordReset(AccountUser) returns (AccountsResponse);
}
```

Replace

```protobuf
}

message RegisterUserRequest {
  bytes user_id = 1;
```

with

```protobuf
}

// Published on the Redis channel `kick:{device_id}` (spec §5.3, §5.4): the gateway holding
// the device closes its socket at once (Plan 8). Never carries anything but the reason.
message Kick {
  KickReason reason = 1;
}

enum KickReason {
  KICK_REASON_UNSPECIFIED = 0;
  // The device was revoked: the client stops (close code 4003).
  KICK_REASON_DEVICE_REVOKED = 1;
  // The user's password was reset: the client signs in again.
  KICK_REASON_PASSWORD_RESET = 2;
}

message RegisterUserRequest {
  bytes user_id = 1;
```

In `crates/push-proto/src/codec.rs`:

Replace

```rust
}

/// A gRPC status carrying `error` for the client, as message-svc answers.
///
```

with

```rust
}

/// The Redis channel message-svc publishes a device's
/// [`Kick`](crate::internal::v1::Kick) on, and the gateway holding the device subscribes
/// to: `kick:{device_id}`.
pub fn kick_channel(device: Uuid) -> String {
    format!("kick:{device}")
}

/// A gRPC status carrying `error` for the client, as message-svc answers.
///
```

Replace

```rust
    #[test]
    fn a_users_channel_is_named_after_the_user() {
        let user = Uuid::parse_str("0192f5a0-0000-7000-8000-000000000001").unwrap();
        assert_eq!(
```

with

```rust
    #[test]
    fn channels_are_named_after_their_user_or_device() {
        let user = Uuid::parse_str("0192f5a0-0000-7000-8000-000000000001").unwrap();
        assert_eq!(
```

Replace

```rust
            "notify:0192f5a0-0000-7000-8000-000000000001"
        );
    }
```

with

```rust
            "notify:0192f5a0-0000-7000-8000-000000000001"
        );
        assert_eq!(
            kick_channel(user),
            "kick:0192f5a0-0000-7000-8000-000000000001"
        );
    }
```

In `crates/push-proto/src/lib.rs`:

Replace

```rust
pub use codec::{
    ERROR_TYPE_URL, FrameError, InvalidId, MAX_FRAME_BYTES, decode_frame, encode_frame,
    error_from_status, error_status, id, id_bytes, notify_channel,
};
```

with

```rust
pub use codec::{
    ERROR_TYPE_URL, FrameError, InvalidId, MAX_FRAME_BYTES, decode_frame, encode_frame,
    error_from_status, error_status, id, id_bytes, kick_channel, notify_channel,
};
```

The notifier publishes each kick on the device's channel. In
`crates/push-message-svc/src/notify.rs`:

Replace

```rust
use async_trait::async_trait;
use prost::Message;
use push_domain::{ContactState, Event, Notifier, ReceiptUpdate};
use push_proto::internal::v1::{
    self as internal, ContactChanged, DirectReceipt, EntriesAvailable, GroupReceipt, Notification,
    ReadPositionChanged, ReceiptChanged, notification::Kind, receipt_changed,
};
use push_proto::{id_bytes, notify_channel};
use redis::aio::ConnectionManager;

/// Publishes each event as small [`Notification`]s on the Redis channels of the users it
/// concerns (`notify:{user_id}`). Never a message body: a gateway that hears
/// `EntriesAvailable` syncs the user's devices.
///
/// Best effort, as `Notifier` requires: a failed publish is logged and dropped, since
```

with

```rust
use async_trait::async_trait;
use prost::Message;
use push_domain::{ContactState, Event, KickReason, Notifier, ReceiptUpdate};
use push_proto::internal::v1::{
    self as internal, ContactChanged, DirectReceipt, EntriesAvailable, GroupReceipt, Kick,
    Notification, ReadPositionChanged, ReceiptChanged, notification::Kind, receipt_changed,
};
use push_proto::{id_bytes, kick_channel, notify_channel};
use redis::aio::ConnectionManager;

/// Publishes each event as small [`Notification`]s on the Redis channels of the users it
/// concerns (`notify:{user_id}`), and kicks as [`Kick`]s on the channels of the devices
/// (`kick:{device_id}`). Never a message body: a gateway that hears `EntriesAvailable`
/// syncs the user's devices.
///
/// Best effort, as `Notifier` requires: a failed publish is logged and dropped, since
```

Replace

```rust
                messages.push(message(user.0, kind));
            }
            _ => {}
        }
```

with

```rust
                messages.push(message(user.0, kind));
            }
            Event::Kick { devices, reason } => {
                let kick = Kick {
                    reason: kick_reason(*reason).into(),
                };
                for device in devices {
                    messages.push((kick_channel(device.0), kick.encode_to_vec()));
                }
            }
            _ => {}
        }
```

Replace

```rust
}

fn message(user: uuid::Uuid, kind: Kind) -> (String, Vec<u8>) {
    let notification = Notification { kind: Some(kind) };
```

with

```rust
}

fn kick_reason(reason: KickReason) -> internal::KickReason {
    match reason {
        KickReason::DeviceRevoked => internal::KickReason::DeviceRevoked,
        KickReason::PasswordReset => internal::KickReason::PasswordReset,
    }
}

fn message(user: uuid::Uuid, kind: Kind) -> (String, Vec<u8>) {
    let notification = Notification { kind: Some(kind) };
```

Replace

```rust
#[cfg(test)]
mod tests {
    use push_domain::{ConversationId, UserId};

    use super::*;
```

with

```rust
#[cfg(test)]
mod tests {
    use push_domain::{ConversationId, DeviceId, UserId};

    use super::*;
```

Replace

```rust
        assert!(notifications(&[]).is_empty());
    }
}
```

with

```rust
        assert!(notifications(&[]).is_empty());
    }

    #[test]
    fn a_kick_goes_to_each_device_with_its_reason() {
        let (phone, laptop) = (DeviceId::new(), DeviceId::new());
        let messages = notifications(&[
            Event::Kick {
                devices: vec![phone, laptop],
                reason: KickReason::PasswordReset,
            },
            Event::Kick {
                devices: vec![phone],
                reason: KickReason::DeviceRevoked,
            },
        ]);
        let decoded: Vec<(String, internal::KickReason)> = messages
            .iter()
            .map(|(channel, payload)| {
                let kick = Kick::decode(payload.as_slice()).unwrap();
                (channel.clone(), kick.reason())
            })
            .collect();
        assert_eq!(
            decoded,
            vec![
                (kick_channel(phone.0), internal::KickReason::PasswordReset),
                (kick_channel(laptop.0), internal::KickReason::PasswordReset),
                (kick_channel(phone.0), internal::KickReason::DeviceRevoked),
            ]
        );
    }
}
```

`PasswordReset` now calls the domain. In `crates/push-message-svc/src/grpc.rs`:

Replace

```rust
    }

    /// Accepted and otherwise ignored for now: message-svc publishes kicks in Plan 7, and
    /// the gateway acts on them (and refuses tokens older than the reset) in Plan 8.
    async fn password_reset(
        &self,
        request: Request<AccountUser>,
    ) -> Result<Response<AccountsResponse>, Status> {
        parse_id(&request.into_inner().user_id, "user_id", UserId)?;
        Ok(Response::new(AccountsResponse {}))
    }
```

with

```rust
    }

    async fn password_reset(
        &self,
        request: Request<AccountUser>,
    ) -> Result<Response<AccountsResponse>, Status> {
        let user = parse_id(&request.into_inner().user_id, "user_id", UserId)?;
        self.messaging
            .password_reset(user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AccountsResponse {}))
    }
```

Replace

```rust
#[cfg(test)]
mod tests {
    use push_domain::{Event, InMemoryStore, RecordingNotifier, SystemClock};
    use push_proto::internal::v1::accounts_server::Accounts as AccountsRpc;
    use push_proto::internal::v1::dev_setup_server::DevSetup;
```

with

```rust
#[cfg(test)]
mod tests {
    use push_domain::{Event, InMemoryStore, KickReason, RecordingNotifier, SystemClock};
    use push_proto::internal::v1::accounts_server::Accounts as AccountsRpc;
    use push_proto::internal::v1::dev_setup_server::DevSetup;
```

Replace

```rust
            Err(Code::PermissionDenied)
        );
        world
            .accounts
```

with

```rust
            Err(Code::PermissionDenied)
        );
        world.notifier.take();
        world
            .accounts
```

Replace

```rust
            .await
            .unwrap();
        assert_eq!(identify(&world, user, phone).await, Ok(()));
    }
```

with

```rust
            .await
            .unwrap();
        // The phone is kicked, to sign in again; the revoked tablet is not active.
        assert_eq!(
            world.notifier.take(),
            [Event::Kick {
                devices: vec![phone],
                reason: KickReason::PasswordReset,
            }]
        );
        assert_eq!(identify(&world, user, phone).await, Ok(()));
    }
```

A revocation over real Postgres rings the device's Redis channel. In
`crates/push-message-svc/tests/service.rs`:

Replace

```rust
//! The whole service on real Postgres and Redis (Docker must be running): readiness, the
//! dev setup calls, the worker's account events, a send that rings the recipient's Redis
//! channel, sync, ack, and graceful shutdown.

use std::net::SocketAddr;
```

with

```rust
//! The whole service on real Postgres and Redis (Docker must be running): readiness, the
//! dev setup calls, the worker's account events (a revocation kicks the device on Redis),
//! a send that rings the recipient's Redis channel, sync, ack, and graceful shutdown.

use std::net::SocketAddr;
```

Replace

```rust
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AccountDevice, AckRequest, IdentifyRequest, MakeContactsRequest, Notification,
    OpenDirectRequest, RegisterDeviceRequest, RegisterUserRequest, SendRequest, SyncRequest,
    notification::Kind,
};
use push_proto::v1::{Ack, ContentType, Send};
use push_proto::{id_bytes, notify_channel};
use push_store::StoreOptions;
use testcontainers_modules::postgres::Postgres;
```

with

```rust
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AccountDevice, AckRequest, IdentifyRequest, Kick, KickReason, MakeContactsRequest,
    Notification, OpenDirectRequest, RegisterDeviceRequest, RegisterUserRequest, SendRequest,
    SyncRequest, notification::Kind,
};
use push_proto::v1::{Ack, ContentType, Send};
use push_proto::{id_bytes, kick_channel, notify_channel};
use push_store::StoreOptions;
use testcontainers_modules::postgres::Postgres;
```

Replace

```rust
    accounts.device_registered(event.clone()).await.unwrap();
    messaging.identify(identify.clone()).await.unwrap();
    accounts.device_revoked(event).await.unwrap();
    let status = messaging.identify(identify).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
```

with

```rust
    accounts.device_registered(event.clone()).await.unwrap();
    messaging.identify(identify.clone()).await.unwrap();

    // A revocation kicks the device through its Redis channel.
    let mut bell = redis::Client::open(running.redis_url.as_str())
        .unwrap()
        .get_async_pubsub()
        .await
        .unwrap();
    bell.subscribe(kick_channel(device)).await.unwrap();
    accounts.device_revoked(event).await.unwrap();
    let kicked = tokio::time::timeout(Duration::from_secs(5), bell.on_message().next())
        .await
        .expect("the device's channel rings within 5 s")
        .unwrap();
    let kick = Kick::decode(kicked.get_payload_bytes()).unwrap();
    assert_eq!(kick.reason(), KickReason::DeviceRevoked);
    let status = messaging.identify(identify).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
```

```powershell
cargo test -p push-proto -p push-message-svc
cargo test -p push-store --test postgres -- devices::
```

Expected: everything passes, among them `a_kick_goes_to_each_device_with_its_reason` and
`accounts_are_served_and_dev_setup_is_absent_unless_enabled` with its kick.

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "feat(message-svc): kick revoked devices and devices of a reset password"
```

Expected: 787 tests pass, no warnings, no diff.

---

### Task 4: Edit, delete, read, history and listings over gRPC

**Files:**
- Create: `crates/push-message-svc/src/requests.rs`,
  `crates/push-message-svc/src/requests/tests.rs`
- Modify: `crates/push-proto/proto/push/v1/frame.proto`,
  `proto/push/internal/v1/messaging.proto`, `crates/push-proto/src/codec.rs`,
  `crates/push-message-svc/src/convert.rs`, `src/grpc.rs`, `src/lib.rs`,
  `crates/push-gateway/src/session.rs`, `crates/push-gateway/tests/gateway.rs`

- [ ] **Step 1: The contract**

`Edit`, `Delete`, `ReadUpTo`, `Request` and `Response` with the listing and history
requests, and `request_id` on `Error` (decision 2). In
`crates/push-proto/proto/push/v1/frame.proto`:

Replace

```protobuf
  }
  // 5-31: the other client frames of spec §7.1 (ReAuth, Edit, Delete, ReadUpTo and the
  // request frames); 37-63: the other server frames of §7.2. Assigned as they are added.
}
```

with

```protobuf
  }
  // 5-31: the other client frames of spec §7.1 (ReAuth, Edit, Delete, ReadUpTo and the
  // request frames); 37-63: the other server frames of §7.2. Assigned as they are added:
  // Edit, Delete, ReadUpTo, Request and Response are defined below and message-svc serves
  // them over gRPC (Plan 7); the gateway carries them in a Frame from Plan 8.
}
```

Replace

```protobuf
}

// The entry is durably stored. Also the answer to Edit and Delete (later plans).
message SendAck {
  bytes client_msg_id = 1;
```

with

```protobuf
}

// The entry is durably stored. Also the answer to Edit and Delete.
message SendAck {
  bytes client_msg_id = 1;
```

Replace

```protobuf
}

message Ping {}
```

with

```protobuf
}

// Changes the text of one of the sender's own messages (spec §4.6), within 48 hours of
// sending it. Answered with the edit entry's SendAck.
message Edit {
  // The edit's own UUIDv7, chosen before the first attempt and reused on every retry.
  bytes client_msg_id = 1;
  bytes conversation_id = 2;
  // The message to change: its server id, or the client_msg_id it was sent with when the
  // client has not learned the server id yet.
  oneof target {
    bytes target_message_id = 3;
    bytes target_client_msg_id = 4;
  }
  bytes body = 5;
  optional int64 client_ts = 6;
}

// Erases one of the sender's own messages (spec §4.6), within 48 hours of sending it.
// Answered with the delete entry's SendAck.
message Delete {
  bytes client_msg_id = 1;
  bytes conversation_id = 2;
  oneof target {
    bytes target_message_id = 3;
    bytes target_client_msg_id = 4;
  }
}

// "I have read everything up to seq in this conversation." Positions only move forward.
message ReadUpTo {
  bytes conversation_id = 1;
  int64 seq = 2;
}

// A request (spec §7.1). It is answered by a Response, or an Error, with its request_id.
message Request {
  // Chosen by the client.
  uint64 request_id = 1;
  oneof body {
    ListConversations list_conversations = 2;
    FetchHistory fetch_history = 3;
  }
}

// The answer to a Request.
message Response {
  uint64 request_id = 1;
  oneof body {
    ConversationList conversations = 2;
    HistoryPage history = 3;
  }
}

// The caller's conversations, current and left, sorted by id, with positions, unread
// counts and receipt positions. A direct conversation with a user the caller has blocked
// is left out.
message ListConversations {
  // Only conversations created or appended to after this time. Pass the time of the
  // previous listing minus a safety window of a few seconds; leave it out to refresh every
  // conversation's receipt positions, which do not count as changes.
  optional int64 changed_since = 1;
}

enum ConversationKind {
  CONVERSATION_KIND_UNSPECIFIED = 0;
  CONVERSATION_KIND_DIRECT = 1;
  CONVERSATION_KIND_GROUP = 2;
}

message ConversationList {
  repeated ConversationSummary conversations = 1;
}

message ConversationSummary {
  bytes conversation_id = 1;
  ConversationKind kind = 2;
  // Groups only.
  optional string title = 3;
  // The highest seq the caller may see: the end of the log, or where the caller left.
  int64 last_seq = 4;
  // The caller's own read position.
  int64 read_seq = 5;
  // Messages from others after read_seq, at most 100 ("99+").
  uint32 unread = 6;
  oneof receipts {
    DirectReceipts direct = 7;
    GroupReceipts group = 8;
  }
  // When the conversation was created or last appended to (as the caller last saw it, for
  // a group they left).
  int64 updated_at = 9;
  // Current members, sorted; empty for a group the caller left.
  repeated bytes members = 10;
  // Current admins of a group, sorted.
  repeated bytes admins = 11;
}

// The other member's positions; delivered is never below read.
message DirectReceipts {
  int64 peer_delivered_seq = 1;
  int64 peer_read_seq = 2;
}

// Every message up to these positions has been delivered to (read by) every current
// member other than its sender; never per-member positions.
message GroupReceipts {
  int64 all_delivered_seq = 1;
  int64 all_read_seq = 2;
}

// Older messages and system entries of a conversation, in their current state: never
// edit or delete entries. Limited to what the caller may see.
message FetchHistory {
  bytes conversation_id = 1;
  // Exclusive; leave it out for the newest page.
  optional int64 before_seq = 2;
  // At most 100; 0 means 100.
  uint32 limit = 3;
}

message HistoryPage {
  // Oldest first.
  repeated Deliver entries = 1;
  // Older entries exist before this page.
  bool has_more = 2;
}

message Ping {}
```

Replace

```protobuf
  // Whether the same frame may succeed if sent again unchanged.
  bool retryable = 3;
  // Set when the error answers a Send.
  optional bytes client_msg_id = 4;
}
```

with

```protobuf
  // Whether the same frame may succeed if sent again unchanged.
  bool retryable = 3;
  // Set when the error answers a Send, Edit or Delete.
  optional bytes client_msg_id = 4;
  // Set when the error answers a Request.
  optional uint64 request_id = 5;
}
```

In `crates/push-proto/proto/push/internal/v1/messaging.proto`:

Replace

```protobuf
  // Records that the device holds everything up to a seq.
  rpc Ack(AckRequest) returns (AckResponse);
}
```

with

```protobuf
  // Records that the device holds everything up to a seq.
  rpc Ack(AckRequest) returns (AckResponse);
  // Changes the text of one of the user's messages; answers after commit with the edit
  // entry's SendAck.
  rpc Edit(EditRequest) returns (push.v1.SendAck);
  // Erases one of the user's messages; answers after commit with the delete entry's
  // SendAck.
  rpc Delete(DeleteRequest) returns (push.v1.SendAck);
  // Moves the user's read position in a conversation.
  rpc ReadUpTo(ReadUpToRequest) returns (ReadUpToResponse);
  // Any request frame (spec §7.1), on behalf of the user. The Response carries the
  // request's request_id, and so does the Error of a failed call.
  rpc Request(RequestCall) returns (push.v1.Response);
}
```

Replace

```protobuf
}

// Published on the Redis channel `notify:{user_id}` after a commit (spec §4.3). Never
// carries message bodies: a lost notification only delays the client.
```

with

```protobuf
}

message EditRequest {
  bytes user_id = 1;
  bytes device_id = 2;
  push.v1.Edit edit = 3;
}

message DeleteRequest {
  bytes user_id = 1;
  bytes device_id = 2;
  push.v1.Delete delete = 3;
}

message ReadUpToRequest {
  bytes user_id = 1;
  push.v1.ReadUpTo read = 2;
}

message ReadUpToResponse {
  // Whether the user's read position moved.
  bool moved = 1;
}

message RequestCall {
  bytes user_id = 1;
  push.v1.Request request = 2;
}

// Published on the Redis channel `notify:{user_id}` after a commit (spec §4.3). Never
// carries message bodies: a lost notification only delays the client.
```

The new `Error` field must be set wherever an `Error` is built. In
`crates/push-proto/src/codec.rs`:

Replace

```rust
            retryable: true,
            client_msg_id: None,
        },
    }
```

with

```rust
            retryable: true,
            client_msg_id: None,
            request_id: None,
        },
    }
```

Replace

```rust
            retryable: false,
            client_msg_id: Some(id_bytes(Uuid::now_v7())),
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
```

with

```rust
            retryable: false,
            client_msg_id: Some(id_bytes(Uuid::now_v7())),
            request_id: Some(7),
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
```

Replace

```rust
            retryable: false,
            client_msg_id: None,
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
```

with

```rust
            retryable: false,
            client_msg_id: None,
            request_id: None,
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
```

In `crates/push-message-svc/src/convert.rs`:

Replace

```rust
            retryable: false,
            client_msg_id: None,
        },
    )
```

with

```rust
            retryable: false,
            client_msg_id: None,
            request_id: None,
        },
    )
```

Replace

```rust
            retryable: error.is_retryable() || matches!(error, DomainError::Store(_)),
            client_msg_id: None,
        },
    )
```

with

```rust
            retryable: error.is_retryable() || matches!(error, DomainError::Store(_)),
            client_msg_id: None,
            request_id: None,
        },
    )
```

In `crates/push-gateway/src/session.rs`:

Replace

```rust
        retryable: false,
        client_msg_id: None,
    })
}
```

with

```rust
        retryable: false,
        client_msg_id: None,
        request_id: None,
    })
}
```

- [ ] **Step 2: The tests, failing**

The tests set users, contacts and conversations up straight through the domain and drive
the API as the gateway will. Create `crates/push-message-svc/src/requests/tests.rs`:

```rust
//! The client's frames over the gRPC API, on the in-memory store. Users, devices and
//! contacts are set up straight through the domain.

use std::sync::Arc;

use chrono::{TimeDelta, TimeZone, Utc};
use push_domain::{
    ClientMsgId, ConversationId, DeviceId, Event, InMemoryStore, ManualClock, Messaging,
    ReceiptUpdate, RecordingNotifier, UserId,
};
use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
use push_proto::internal::v1::{
    DeleteRequest, EditRequest, ReadUpToRequest, RequestCall, SendRequest,
};
use push_proto::v1::{
    self, ContentType, ConversationKind, Delete, Deliver, DirectReceipts, Edit, EntryKind,
    ErrorCode, FetchHistory, ListConversations, ReadUpTo, SendAck, conversation_summary, delete,
    edit, request, response,
};
use push_proto::{error_from_status, id_bytes};
use tonic::{Code, Request, Status};

use crate::grpc::MessagingApi;

type Svc = Messaging<InMemoryStore, Arc<ManualClock>, Arc<RecordingNotifier>>;

struct World {
    api: MessagingApi<InMemoryStore, Arc<ManualClock>, Arc<RecordingNotifier>>,
    svc: Arc<Svc>,
    clock: Arc<ManualClock>,
    notifier: Arc<RecordingNotifier>,
}

fn world() -> World {
    let clock = Arc::new(ManualClock::new(
        Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap(),
    ));
    let notifier = Arc::new(RecordingNotifier::default());
    let svc = Arc::new(Messaging::new(
        InMemoryStore::default(),
        Arc::clone(&clock),
        Arc::clone(&notifier),
    ));
    World {
        api: MessagingApi::new(Arc::clone(&svc)),
        svc,
        clock,
        notifier,
    }
}

/// A user with one device.
#[derive(Clone, Copy)]
struct Person {
    user: UserId,
    device: DeviceId,
}

impl World {
    async fn person(&self) -> Person {
        let person = Person {
            user: UserId::new(),
            device: DeviceId::new(),
        };
        self.svc
            .device_registered(person.device, person.user)
            .await
            .unwrap();
        person
    }

    async fn befriend(&self, a: &Person, b: &Person) {
        self.svc.contact_request(a.user, b.user).await.unwrap();
        self.svc.contact_accept(b.user, a.user).await.unwrap();
    }

    /// Two contacts with their direct conversation.
    async fn chat(&self) -> (Person, Person, ConversationId) {
        let (alice, bob) = (self.person().await, self.person().await);
        self.befriend(&alice, &bob).await;
        let chat = self.svc.open_direct(alice.user, bob.user).await.unwrap();
        (alice, bob, chat)
    }

    async fn send(&self, from: &Person, conversation: ConversationId, body: &str) -> SendAck {
        let request = SendRequest {
            user_id: id_bytes(from.user.0),
            device_id: id_bytes(from.device.0),
            send: Some(v1::Send {
                client_msg_id: id_bytes(ClientMsgId::new().0),
                conversation_id: id_bytes(conversation.0),
                content_type: ContentType::Text.into(),
                body: body.as_bytes().to_vec(),
                client_ts: None,
            }),
        };
        self.api
            .send(Request::new(request))
            .await
            .unwrap()
            .into_inner()
    }

    async fn edit(
        &self,
        from: &Person,
        conversation: ConversationId,
        target: edit::Target,
        body: &str,
    ) -> Result<SendAck, Status> {
        let request = EditRequest {
            user_id: id_bytes(from.user.0),
            device_id: id_bytes(from.device.0),
            edit: Some(Edit {
                client_msg_id: id_bytes(ClientMsgId::new().0),
                conversation_id: id_bytes(conversation.0),
                target: Some(target),
                body: body.as_bytes().to_vec(),
                client_ts: None,
            }),
        };
        self.api
            .edit(Request::new(request))
            .await
            .map(|response| response.into_inner())
    }

    async fn delete(
        &self,
        from: &Person,
        conversation: ConversationId,
        target: Option<delete::Target>,
    ) -> Result<SendAck, Status> {
        let request = DeleteRequest {
            user_id: id_bytes(from.user.0),
            device_id: id_bytes(from.device.0),
            delete: Some(Delete {
                client_msg_id: id_bytes(ClientMsgId::new().0),
                conversation_id: id_bytes(conversation.0),
                target,
            }),
        };
        self.api
            .delete(Request::new(request))
            .await
            .map(|response| response.into_inner())
    }

    async fn read(
        &self,
        reader: &Person,
        conversation: ConversationId,
        seq: i64,
    ) -> Result<bool, Status> {
        let request = ReadUpToRequest {
            user_id: id_bytes(reader.user.0),
            read: Some(ReadUpTo {
                conversation_id: id_bytes(conversation.0),
                seq,
            }),
        };
        self.api
            .read_up_to(Request::new(request))
            .await
            .map(|response| response.into_inner().moved)
    }

    /// Sends a request frame as `from`, numbered 42.
    async fn request(&self, from: &Person, body: request::Body) -> Result<response::Body, Status> {
        let call = RequestCall {
            user_id: id_bytes(from.user.0),
            request: Some(v1::Request {
                request_id: 42,
                body: Some(body),
            }),
        };
        let response = self.api.request(Request::new(call)).await?.into_inner();
        assert_eq!(response.request_id, 42);
        Ok(response.body.unwrap())
    }

    async fn sync(&self, device: DeviceId) -> Vec<Deliver> {
        self.svc
            .sync(device, 100)
            .await
            .unwrap()
            .entries
            .into_iter()
            .map(crate::convert::deliver)
            .collect()
    }
}

/// The client error of a failed call: its code, and whether it carries request 42.
fn failure(status: &Status) -> (ErrorCode, bool) {
    let error = error_from_status(status);
    (error.code(), error.request_id == Some(42))
}

fn message_id(ack: &SendAck) -> edit::Target {
    edit::Target::TargetMessageId(ack.message_id.clone())
}

fn list_conversations(changed_since: Option<i64>) -> request::Body {
    request::Body::ListConversations(ListConversations { changed_since })
}

#[tokio::test]
async fn an_edit_and_a_delete_are_stored_as_entries_and_delivered() {
    let world = world();
    let (alice, bob, chat) = world.chat().await;
    let sent = world.send(&alice, chat, "helo").await;

    let edited = world
        .edit(&alice, chat, message_id(&sent), "hello")
        .await
        .unwrap();
    assert_eq!(edited.seq, 2);
    let entries = world.sync(bob.device).await;
    assert_eq!(entries[0].body.as_deref(), Some(b"hello".as_slice()));
    assert_eq!(entries[0].edit_seq, Some(2));
    assert_eq!(entries[1].kind(), EntryKind::Edit);
    assert_eq!(entries[1].target_seq, Some(1));

    // The sender may name the message by the client_msg_id it was sent with.
    let target = delete::Target::TargetClientMsgId(sent.client_msg_id.clone());
    let deleted = world.delete(&alice, chat, Some(target)).await.unwrap();
    assert_eq!(deleted.seq, 3);
    let entries = world.sync(bob.device).await;
    assert!(entries[0].deleted);
    assert_eq!(entries[0].body, None);
    assert_eq!(entries[1].body, None, "the edit's text is erased too");
    assert_eq!(entries[2].kind(), EntryKind::Delete);
}

#[tokio::test]
async fn edits_and_deletes_are_refused_with_their_own_errors() {
    let world = world();
    let (alice, bob, chat) = world.chat().await;
    let sent = world.send(&alice, chat, "mine").await;

    let by_bob = world.edit(&bob, chat, message_id(&sent), "theirs").await;
    assert_eq!(by_bob.unwrap_err().code(), Code::PermissionDenied);
    let unknown = edit::Target::TargetMessageId(id_bytes(ClientMsgId::new().0));
    let status = world.edit(&alice, chat, unknown, "x").await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::TargetNotFound);
    let status = world.delete(&alice, chat, None).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(error_from_status(&status).message, "target is required");

    world
        .clock
        .advance(TimeDelta::hours(48) + TimeDelta::seconds(1));
    let status = world
        .edit(&alice, chat, message_id(&sent), "late")
        .await
        .unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::WindowExpired);
    let late = delete::Target::TargetMessageId(sent.message_id.clone());
    let status = world.delete(&alice, chat, Some(late)).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::WindowExpired);
}

#[tokio::test]
async fn reading_moves_the_position_and_tells_both_sides() {
    let world = world();
    let (alice, bob, chat) = world.chat().await;
    world.send(&alice, chat, "one").await;
    world.notifier.take();

    assert!(world.read(&bob, chat, 1).await.unwrap());
    let events = world.notifier.take();
    assert!(events.contains(&Event::ReadPositionChanged {
        user: bob.user,
        conversation: chat,
        read_seq: 1,
    }));
    assert!(events.contains(&Event::Receipt {
        conversation: chat,
        to: vec![alice.user],
        receipt: ReceiptUpdate::Direct {
            user_id: bob.user,
            delivered_seq: 1,
            read_seq: 1,
        },
    }));
    assert!(!world.read(&bob, chat, 1).await.unwrap());
    let status = world.read(&bob, chat, 2).await.unwrap_err();
    assert_eq!(status.code(), Code::OutOfRange);
}

#[tokio::test]
async fn a_listing_has_positions_unread_counts_and_receipts() {
    let world = world();
    let (alice, bob, chat) = world.chat().await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;
    world.read(&bob, chat, 1).await.unwrap();

    let response::Body::Conversations(listed) =
        world.request(&bob, list_conversations(None)).await.unwrap()
    else {
        panic!("not a conversation list");
    };
    assert_eq!(listed.conversations.len(), 1);
    let summary = &listed.conversations[0];
    assert_eq!(summary.conversation_id, id_bytes(chat.0));
    assert_eq!(summary.kind(), ConversationKind::Direct);
    assert_eq!(
        (summary.last_seq, summary.read_seq, summary.unread),
        (2, 1, 1)
    );
    let mut members = vec![id_bytes(alice.user.0), id_bytes(bob.user.0)];
    members.sort();
    assert_eq!(summary.members, members);

    let response::Body::Conversations(alices) = world
        .request(&alice, list_conversations(None))
        .await
        .unwrap()
    else {
        panic!("not a conversation list");
    };
    assert_eq!(
        alices.conversations[0].receipts,
        Some(conversation_summary::Receipts::Direct(DirectReceipts {
            peer_delivered_seq: 1,
            peer_read_seq: 1,
        }))
    );

    // Nothing changed after the last append.
    let later = list_conversations(Some(summary.updated_at));
    let response::Body::Conversations(changed) = world.request(&bob, later).await.unwrap() else {
        panic!("not a conversation list");
    };
    assert!(changed.conversations.is_empty());
}

#[tokio::test]
async fn history_pages_back_through_messages_in_their_current_state() {
    let world = world();
    let (alice, bob, chat) = world.chat().await;
    let first = world.send(&alice, chat, "one").await;
    world.send(&bob, chat, "two").await;
    world.send(&alice, chat, "three").await;
    world
        .edit(&alice, chat, message_id(&first), "one!")
        .await
        .unwrap();

    let fetch = |before_seq| {
        request::Body::FetchHistory(FetchHistory {
            conversation_id: id_bytes(chat.0),
            before_seq,
            limit: 2,
        })
    };
    let response::Body::History(newest) = world.request(&bob, fetch(None)).await.unwrap() else {
        panic!("not a history page");
    };
    let seqs: Vec<i64> = newest.entries.iter().map(|entry| entry.seq).collect();
    assert_eq!(seqs, [2, 3], "the edit entry is not history");
    assert!(newest.has_more);
    let response::Body::History(oldest) = world.request(&bob, fetch(Some(2))).await.unwrap() else {
        panic!("not a history page");
    };
    assert_eq!(oldest.entries.len(), 1);
    assert_eq!(oldest.entries[0].body.as_deref(), Some(b"one!".as_slice()));
    assert!(!oldest.has_more);
}

#[tokio::test]
async fn a_failed_request_carries_its_request_id() {
    let world = world();
    let alice = world.person().await;
    let unknown = request::Body::FetchHistory(FetchHistory {
        conversation_id: id_bytes(ConversationId::new().0),
        before_seq: None,
        limit: 0,
    });
    let status = world.request(&alice, unknown).await.unwrap_err();
    assert_eq!(status.code(), Code::NotFound);
    assert_eq!(failure(&status), (ErrorCode::ConversationNotFound, true));

    let malformed = request::Body::FetchHistory(FetchHistory {
        conversation_id: vec![1, 2, 3],
        before_seq: None,
        limit: 0,
    });
    let status = world.request(&alice, malformed).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::Invalid, true));

    let empty = RequestCall {
        user_id: id_bytes(alice.user.0),
        request: Some(v1::Request {
            request_id: 42,
            body: None,
        }),
    };
    let status = world.api.request(Request::new(empty)).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::Invalid, true));
}
```

and declare the module they belong to in `crates/push-message-svc/src/lib.rs`:

Replace

```rust
//! push-message-svc: the messaging core of push-messenger (spec §2) as a gRPC service.
//!
//! It owns the `messaging` schema (through `push-store`), answers the gateway's calls,
//! records the users and devices the worker relays from auth-svc and, after every commit,
//! rings the affected users' Redis channels so their gateways sync them.

mod config;
```

with

```rust
//! push-message-svc: the messaging core of push-messenger (spec §2) as a gRPC service.
//!
//! It owns the `messaging` schema (through `push-store`), answers the gateway's calls on
//! behalf of connected devices (every client frame of spec §7.1), records the users and
//! devices the worker relays from auth-svc and, after every commit, rings the affected
//! users' Redis channels so their gateways sync them, and kicks revoked devices.

mod config;
```

Replace

```rust
mod grpc;
mod notify;
mod server;
```

with

```rust
mod grpc;
mod notify;
mod requests;
mod server;
```

```powershell
cargo test -p push-message-svc --lib
```

Expected: a compile error — `file not found for module requests`.

- [ ] **Step 3: The frames, into the domain and back**

Create `crates/push-message-svc/src/requests.rs`:

```rust
//! The client's frames beyond Send and Ack (spec §7.1): Edit, Delete, ReadUpTo and the
//! request frames, from protobuf to the domain and back. The gateway calls them on behalf
//! of the user and device it verified.

use push_domain::{
    ClientMsgId, Clock, ConversationId, ConversationKind, ConversationSummary, DeleteMessage,
    DeviceId, EditText, MAX_HISTORY_PAGE, MessageId, MessageRef, Messaging, Notifier, Receipts,
    Store, UserId,
};
use push_proto::internal::v1::{
    DeleteRequest, EditRequest, ReadUpToRequest, ReadUpToResponse, RequestCall,
};
use push_proto::v1::{
    self, ConversationList, DirectReceipts, GroupReceipts, HistoryPage, SendAck,
    conversation_summary, delete, edit, request, response,
};
use push_proto::{error_from_status, error_status, id_bytes};
use tonic::Status;

use crate::convert::{deliver, domain_error, from_micros, invalid, micros, parse_id, send_ack};

/// `Edit`: the edit entry's receipt, after commit.
pub(crate) async fn edit<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    call: EditRequest,
) -> Result<SendAck, Status> {
    let sender = parse_id(&call.user_id, "user_id", UserId)?;
    let device = parse_id(&call.device_id, "device_id", DeviceId)?;
    let edit = call.edit.ok_or_else(|| invalid("edit is required"))?;
    let target = match edit.target {
        Some(edit::Target::TargetMessageId(id)) => by_message_id(&id)?,
        Some(edit::Target::TargetClientMsgId(id)) => by_client_msg_id(&id)?,
        None => return Err(invalid("target is required")),
    };
    let command = EditText {
        sender,
        device,
        conversation: parse_id(&edit.conversation_id, "conversation_id", ConversationId)?,
        client_msg_id: parse_id(&edit.client_msg_id, "client_msg_id", ClientMsgId)?,
        target,
        body: edit.body,
        client_ts: edit.client_ts.map(from_micros).transpose()?,
    };
    let receipt = messaging.edit_text(command).await.map_err(domain_error)?;
    Ok(send_ack(&receipt))
}

/// `Delete`: the delete entry's receipt, after commit.
pub(crate) async fn delete<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    call: DeleteRequest,
) -> Result<SendAck, Status> {
    let sender = parse_id(&call.user_id, "user_id", UserId)?;
    let device = parse_id(&call.device_id, "device_id", DeviceId)?;
    let delete = call.delete.ok_or_else(|| invalid("delete is required"))?;
    let target = match delete.target {
        Some(delete::Target::TargetMessageId(id)) => by_message_id(&id)?,
        Some(delete::Target::TargetClientMsgId(id)) => by_client_msg_id(&id)?,
        None => return Err(invalid("target is required")),
    };
    let command = DeleteMessage {
        sender,
        device,
        conversation: parse_id(&delete.conversation_id, "conversation_id", ConversationId)?,
        client_msg_id: parse_id(&delete.client_msg_id, "client_msg_id", ClientMsgId)?,
        target,
    };
    let receipt = messaging
        .delete_message(command)
        .await
        .map_err(domain_error)?;
    Ok(send_ack(&receipt))
}

fn by_message_id(id: &[u8]) -> Result<MessageRef, Status> {
    Ok(MessageRef::Id(parse_id(
        id,
        "target_message_id",
        MessageId,
    )?))
}

fn by_client_msg_id(id: &[u8]) -> Result<MessageRef, Status> {
    Ok(MessageRef::ClientId(parse_id(
        id,
        "target_client_msg_id",
        ClientMsgId,
    )?))
}

/// `ReadUpTo`: whether the user's read position moved.
pub(crate) async fn read_up_to<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    call: ReadUpToRequest,
) -> Result<ReadUpToResponse, Status> {
    let user = parse_id(&call.user_id, "user_id", UserId)?;
    let read = call.read.ok_or_else(|| invalid("read is required"))?;
    let conversation = parse_id(&read.conversation_id, "conversation_id", ConversationId)?;
    let moved = messaging
        .read_up_to(user, conversation, read.seq)
        .await
        .map_err(domain_error)?;
    Ok(ReadUpToResponse { moved })
}

/// A request frame: its `Response`, or a status whose error carries its `request_id`.
pub(crate) async fn request<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    call: RequestCall,
) -> Result<v1::Response, Status> {
    let request = call.request.ok_or_else(|| invalid("request is required"))?;
    let request_id = request.request_id;
    let answered = async {
        let user = parse_id(&call.user_id, "user_id", UserId)?;
        let body = request
            .body
            .ok_or_else(|| invalid("the request has no body"))?;
        answer(messaging, user, body).await
    }
    .await;
    match answered {
        Ok(body) => Ok(v1::Response {
            request_id,
            body: Some(body),
        }),
        Err(status) => {
            let mut error = error_from_status(&status);
            error.request_id = Some(request_id);
            Err(error_status(status.code(), error))
        }
    }
}

/// What the domain answers to one request.
async fn answer<S: Store, C: Clock, N: Notifier>(
    messaging: &Messaging<S, C, N>,
    user: UserId,
    body: request::Body,
) -> Result<response::Body, Status> {
    Ok(match body {
        request::Body::ListConversations(list) => {
            let since = list.changed_since.map(from_micros).transpose()?;
            let summaries = messaging
                .list_conversations(user, since)
                .await
                .map_err(domain_error)?;
            response::Body::Conversations(ConversationList {
                conversations: summaries.into_iter().map(summary).collect(),
            })
        }
        request::Body::FetchHistory(fetch) => {
            let conversation = parse_id(&fetch.conversation_id, "conversation_id", ConversationId)?;
            let limit = match fetch.limit {
                0 => MAX_HISTORY_PAGE,
                limit => limit,
            };
            let page = messaging
                .history(user, conversation, fetch.before_seq, limit)
                .await
                .map_err(domain_error)?;
            response::Body::History(HistoryPage {
                entries: page.entries.into_iter().map(deliver).collect(),
                has_more: page.has_more,
            })
        }
    })
}

fn summary(summary: ConversationSummary) -> v1::ConversationSummary {
    let receipts = match summary.receipts {
        Receipts::Direct {
            peer_delivered_seq,
            peer_read_seq,
        } => conversation_summary::Receipts::Direct(DirectReceipts {
            peer_delivered_seq,
            peer_read_seq,
        }),
        Receipts::Group {
            all_delivered_seq,
            all_read_seq,
        } => conversation_summary::Receipts::Group(GroupReceipts {
            all_delivered_seq,
            all_read_seq,
        }),
    };
    let kind = match summary.kind {
        ConversationKind::Direct => v1::ConversationKind::Direct,
        ConversationKind::Group => v1::ConversationKind::Group,
    };
    v1::ConversationSummary {
        conversation_id: id_bytes(summary.conversation_id.0),
        kind: kind.into(),
        title: summary.title,
        last_seq: summary.last_seq,
        read_seq: summary.read_seq,
        unread: summary.unread,
        receipts: Some(receipts),
        updated_at: micros(summary.updated_at),
        members: users(&summary.members),
        admins: users(&summary.admins),
    }
}

fn users(users: &[UserId]) -> Vec<Vec<u8>> {
    users.iter().map(|user| id_bytes(user.0)).collect()
}

#[cfg(test)]
mod tests;
```

`MessagingApi` delegates. In `crates/push-message-svc/src/grpc.rs`:

Replace

```rust
use push_proto::id_bytes;
use push_proto::internal::v1::{
    AccountDevice, AccountUser, AccountsResponse, AckRequest, AckResponse, IdentifyRequest,
    IdentifyResponse, MakeContactsRequest, MakeContactsResponse, OpenDirectRequest,
    OpenDirectResponse, RegisterDeviceRequest, RegisterDeviceResponse, RegisterUserRequest,
    RegisterUserResponse, SendRequest, SyncRequest, SyncResponse, accounts_server,
    dev_setup_server, messaging_server,
};
use push_proto::v1::{ContentType, SendAck};
```

with

```rust
use push_proto::id_bytes;
use push_proto::internal::v1::{
    AccountDevice, AccountUser, AccountsResponse, AckRequest, AckResponse, DeleteRequest,
    EditRequest, IdentifyRequest, IdentifyResponse, MakeContactsRequest, MakeContactsResponse,
    OpenDirectRequest, OpenDirectResponse, ReadUpToRequest, ReadUpToResponse,
    RegisterDeviceRequest, RegisterDeviceResponse, RegisterUserRequest, RegisterUserResponse,
    RequestCall, SendRequest, SyncRequest, SyncResponse, accounts_server, dev_setup_server,
    messaging_server,
};
use push_proto::v1::{ContentType, SendAck};
```

Replace

```rust
use crate::convert::{deliver, domain_error, from_micros, invalid, parse_id, send_ack};

/// The `Messaging` service: what the gateway calls for a connected device. The gateway
```

with

```rust
use crate::convert::{deliver, domain_error, from_micros, invalid, parse_id, send_ack};
use crate::requests;

/// The `Messaging` service: what the gateway calls for a connected device. The gateway
```

Replace

```rust
        Ok(Response::new(AckResponse { moved }))
    }
}
```

with

```rust
        Ok(Response::new(AckResponse { moved }))
    }

    async fn edit(&self, request: Request<EditRequest>) -> Result<Response<SendAck>, Status> {
        requests::edit(&self.messaging, request.into_inner())
            .await
            .map(Response::new)
    }

    async fn delete(&self, request: Request<DeleteRequest>) -> Result<Response<SendAck>, Status> {
        requests::delete(&self.messaging, request.into_inner())
            .await
            .map(Response::new)
    }

    async fn read_up_to(
        &self,
        request: Request<ReadUpToRequest>,
    ) -> Result<Response<ReadUpToResponse>, Status> {
        requests::read_up_to(&self.messaging, request.into_inner())
            .await
            .map(Response::new)
    }

    async fn request(
        &self,
        request: Request<RequestCall>,
    ) -> Result<Response<push_proto::v1::Response>, Status> {
        requests::request(&self.messaging, request.into_inner())
            .await
            .map(Response::new)
    }
}
```

The gateway's tests wrap `MessagingApi` in a service of their own, which must forward the
new calls. In `crates/push-gateway/tests/gateway.rs`:

Replace

```rust
use push_proto::internal::v1::messaging_server::{self, MessagingServer};
use push_proto::internal::v1::{
    AckRequest, AckResponse, IdentifyRequest, IdentifyResponse, MakeContactsRequest,
    OpenDirectRequest, RegisterDeviceRequest, RegisterUserRequest, SendRequest, SyncRequest,
    SyncResponse,
};
```

with

```rust
use push_proto::internal::v1::messaging_server::{self, MessagingServer};
use push_proto::internal::v1::{
    AckRequest, AckResponse, DeleteRequest, EditRequest, IdentifyRequest, IdentifyResponse,
    MakeContactsRequest, OpenDirectRequest, ReadUpToRequest, ReadUpToResponse,
    RegisterDeviceRequest, RegisterUserRequest, RequestCall, SendRequest, SyncRequest,
    SyncResponse,
};
```

Replace

```rust
        self.api.ack(request).await
    }
}
```

with

```rust
        self.api.ack(request).await
    }

    async fn edit(&self, request: Request<EditRequest>) -> Result<Response<SendAck>, Status> {
        self.api.edit(request).await
    }

    async fn delete(&self, request: Request<DeleteRequest>) -> Result<Response<SendAck>, Status> {
        self.api.delete(request).await
    }

    async fn read_up_to(
        &self,
        request: Request<ReadUpToRequest>,
    ) -> Result<Response<ReadUpToResponse>, Status> {
        self.api.read_up_to(request).await
    }

    async fn request(
        &self,
        request: Request<RequestCall>,
    ) -> Result<Response<push_proto::v1::Response>, Status> {
        self.api.request(request).await
    }
}
```

```powershell
cargo test -p push-message-svc -p push-gateway
```

Expected: everything passes, the 6 new tests included (edits and deletes stored and
delivered, refused with their own errors and after 48 hours, reads telling both sides,
listings with unread counts and receipts, history without edit entries, a failed request
carrying its `request_id`).

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "feat(message-svc): edit, delete, read, history and listings over gRPC"
```

Expected: 793 tests pass, no warnings, no diff.

---

### Task 5: Contacts, invites and direct conversations over gRPC

**Files:**
- Modify: `crates/push-proto/proto/push/v1/frame.proto`,
  `proto/push/internal/v1/messaging.proto`, `crates/push-message-svc/src/convert.rs`,
  `src/notify.rs`, `src/requests.rs`, `src/requests/tests.rs`

- [ ] **Step 1: The tests, failing**

A request is pending until accepted; a direct conversation needs an accepted contact;
after a block nobody can edit or delete, nobody is told, and the chat stays; an invite
makes contacts, is read case-insensitively, hides the creator's block and stops when
revoked. In `crates/push-message-svc/src/requests/tests.rs`:

Replace

```rust
};
use push_proto::v1::{
    self, ContentType, ConversationKind, Delete, Deliver, DirectReceipts, Edit, EntryKind,
    ErrorCode, FetchHistory, ListConversations, ReadUpTo, SendAck, conversation_summary, delete,
    edit, request, response,
};
use push_proto::{error_from_status, id_bytes};
```

with

```rust
};
use push_proto::v1::{
    self, Contact, ContactAccept, ContactBlock, ContactRequest, ContactState, ContactUnblock,
    ContentType, ConversationKind, CreateInvite, Delete, Deliver, DirectReceipts, Done, Edit,
    EntryKind, ErrorCode, FetchHistory, InviteRedeemed, ListContacts, ListConversations,
    OpenDirect, ReadUpTo, RedeemInvite, RevokeInvite, SendAck, conversation_summary, delete, edit,
    request, response,
};
use push_proto::{error_from_status, id_bytes};
```

Replace

```rust
    assert_eq!(failure(&status), (ErrorCode::Invalid, true));
}
```

with

```rust
    assert_eq!(failure(&status), (ErrorCode::Invalid, true));
}

async fn contacts(world: &World, of: &Person) -> Vec<Contact> {
    let list = request::Body::ListContacts(ListContacts {});
    let response::Body::Contacts(listed) = world.request(of, list).await.unwrap() else {
        panic!("not a contact list");
    };
    listed.contacts
}

fn contact(person: &Person, state: ContactState) -> Contact {
    Contact {
        user_id: id_bytes(person.user.0),
        state: state.into(),
    }
}

#[tokio::test]
async fn contacts_are_requested_accepted_and_blocked_and_a_block_freezes_the_chat() {
    let world = world();
    let (alice, bob) = (world.person().await, world.person().await);
    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(bob.user.0),
    });
    assert_eq!(
        world.request(&alice, ask).await.unwrap(),
        response::Body::Contact(contact(&bob, ContactState::RequestedOut))
    );
    assert_eq!(
        contacts(&world, &bob).await,
        [contact(&alice, ContactState::RequestedIn)]
    );

    // A direct conversation needs an accepted contact.
    let open = request::Body::OpenDirect(OpenDirect {
        peer_user_id: id_bytes(bob.user.0),
    });
    let status = world.request(&alice, open.clone()).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::NotContact, true));
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(alice.user.0),
    });
    assert_eq!(
        world.request(&bob, accept.clone()).await.unwrap(),
        response::Body::Done(Done {})
    );
    let status = world.request(&bob, accept).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NoPendingRequest);
    let response::Body::DirectOpened(opened) = world.request(&alice, open).await.unwrap() else {
        panic!("not an opened conversation");
    };
    let from_bob = request::Body::OpenDirect(OpenDirect {
        peer_user_id: id_bytes(alice.user.0),
    });
    assert_eq!(
        world.request(&bob, from_bob).await.unwrap(),
        response::Body::DirectOpened(opened.clone())
    );
    let chat = ConversationId(push_proto::id(&opened.conversation_id).unwrap());
    let sent = world.send(&alice, chat, "before the block").await;

    // After a block nobody can change the chat, which stays as evidence; nobody is told.
    let block = request::Body::ContactBlock(ContactBlock {
        user_id: id_bytes(alice.user.0),
    });
    world.request(&bob, block).await.unwrap();
    let status = world
        .edit(&alice, chat, message_id(&sent), "after the block")
        .await
        .unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NotContact);
    let target = delete::Target::TargetMessageId(sent.message_id.clone());
    let status = world.delete(&alice, chat, Some(target)).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NotContact);
    assert!(contacts(&world, &alice).await.is_empty());
    assert_eq!(
        contacts(&world, &bob).await,
        [contact(&alice, ContactState::Blocked)]
    );
    let entries = world.sync(bob.device).await;
    assert_eq!(
        entries[0].body.as_deref(),
        Some(b"before the block".as_slice())
    );

    let unblock = request::Body::ContactUnblock(ContactUnblock {
        user_id: id_bytes(alice.user.0),
    });
    world.request(&bob, unblock).await.unwrap();
    assert!(contacts(&world, &bob).await.is_empty());

    let stranger = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(UserId::new().0),
    });
    let status = world.request(&alice, stranger).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::UserNotFound);
    let own = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(alice.user.0),
    });
    let status = world.request(&alice, own).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::Invalid);
}

#[tokio::test]
async fn an_invite_makes_contacts_until_it_is_revoked_and_never_reveals_a_block() {
    let world = world();
    let (alice, carol, dave, erin) = (
        world.person().await,
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let create = request::Body::CreateInvite(CreateInvite {
        expires_in_s: 3600,
        max_uses: 3,
    });
    let response::Body::Invite(invite) = world.request(&alice, create).await.unwrap() else {
        panic!("not an invite");
    };
    assert_eq!(invite.code.len(), 10);
    assert_eq!((invite.max_uses, invite.uses), (3, 0));

    let redeem = |code: &str| {
        request::Body::RedeemInvite(RedeemInvite {
            code: code.to_owned(),
        })
    };
    // Codes are read case-insensitively.
    let lower = invite.code.to_lowercase();
    assert_eq!(
        world.request(&carol, redeem(&lower)).await.unwrap(),
        response::Body::InviteRedeemed(InviteRedeemed {
            user_id: id_bytes(alice.user.0),
        })
    );
    assert_eq!(
        contacts(&world, &carol).await,
        [contact(&alice, ContactState::Accepted)]
    );

    // The creator's block looks like an unknown code.
    world
        .svc
        .contact_block(alice.user, dave.user)
        .await
        .unwrap();
    let status = world
        .request(&dave, redeem(&invite.code))
        .await
        .unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::InviteNotFound, true));

    let revoke = request::Body::RevokeInvite(RevokeInvite {
        code: invite.code.clone(),
    });
    assert_eq!(
        world.request(&alice, revoke).await.unwrap(),
        response::Body::Done(Done {})
    );
    let status = world
        .request(&erin, redeem(&invite.code))
        .await
        .unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::InviteNotFound);

    let too_many = request::Body::CreateInvite(CreateInvite {
        expires_in_s: 3600,
        max_uses: 101,
    });
    let status = world.request(&alice, too_many).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::Invalid);
}
```

```powershell
cargo test -p push-message-svc --lib
```

Expected: compile errors — no `ContactRequest`, `Contact`, `ContactState`, `CreateInvite`
or the other new messages in `push.v1`.

- [ ] **Step 2: The contract**

`ContactState` moves to `push.v1` (decision 11). In
`crates/push-proto/proto/push/v1/frame.proto`:

Replace

```protobuf
    ListConversations list_conversations = 2;
    FetchHistory fetch_history = 3;
  }
}
```

with

```protobuf
    ListConversations list_conversations = 2;
    FetchHistory fetch_history = 3;
    OpenDirect open_direct = 4;
    ContactRequest contact_request = 5;
    ContactAccept contact_accept = 6;
    ContactDecline contact_decline = 7;
    ContactBlock contact_block = 8;
    ContactUnblock contact_unblock = 9;
    ListContacts list_contacts = 10;
    CreateInvite create_invite = 11;
    RedeemInvite redeem_invite = 12;
    RevokeInvite revoke_invite = 13;
  }
}
```

Replace

```protobuf
    ConversationList conversations = 2;
    HistoryPage history = 3;
  }
}

// The caller's conversations, current and left, sorted by id, with positions, unread
// counts and receipt positions. A direct conversation with a user the caller has blocked
```

with

```protobuf
    ConversationList conversations = 2;
    HistoryPage history = 3;
    DirectOpened direct_opened = 4;
    // ContactRequest: the caller's view of the relationship now.
    Contact contact = 5;
    ContactList contacts = 6;
    Invite invite = 7;
    InviteRedeemed invite_redeemed = 8;
    // A request that answers nothing but success.
    Done done = 9;
  }
}

message Done {}

// The caller's conversations, current and left, sorted by id, with positions, unread
// counts and receipt positions. A direct conversation with a user the caller has blocked
```

Replace

```protobuf
}

message Ping {}
```

with

```protobuf
}

// The direct conversation with an accepted contact, created the first time it is opened.
message OpenDirect {
  bytes peer_user_id = 1;
}

message DirectOpened {
  bytes conversation_id = 1;
}

// Asks another user to become a contact (spec §5.5). If they already asked the caller,
// both become accepted contacts at once.
message ContactRequest {
  bytes user_id = 1;
}

// Accepts the request the other user sent.
message ContactAccept {
  bytes user_id = 1;
}

// Declines the request the other user sent. To them a decline looks like a block.
message ContactDecline {
  bytes user_id = 1;
}

// Blocks the other user, ending any contact; they are never told. After a block neither
// user can send, edit or delete in their direct conversation, which stays as it was.
message ContactBlock {
  bytes user_id = 1;
}

// Removes a block. A request the block had hidden becomes an ordinary pending request.
message ContactUnblock {
  bytes user_id = 1;
}

enum ContactState {
  // No relationship.
  CONTACT_STATE_NONE = 0;
  CONTACT_STATE_REQUESTED_OUT = 1;
  CONTACT_STATE_REQUESTED_IN = 2;
  CONTACT_STATE_ACCEPTED = 3;
  CONTACT_STATE_BLOCKED = 4;
}

// One of the caller's relationships, as the caller sees it. Nobody is told that someone
// blocked them.
message Contact {
  bytes user_id = 1;
  ContactState state = 2;
}

message ListContacts {}

message ContactList {
  // Sorted by user id.
  repeated Contact contacts = 1;
}

// A code the caller can share as a link or QR code; redeeming it makes the redeemer an
// accepted contact of the caller.
message CreateInvite {
  // 60 seconds to 30 days.
  uint32 expires_in_s = 1;
  // 1 to 100.
  uint32 max_uses = 2;
}

message Invite {
  // Ten characters of Crockford base32.
  string code = 1;
  int64 expires_at = 2;
  uint32 max_uses = 3;
  uint32 uses = 4;
}

// Read case-insensitively; dashes and spaces are ignored. An unknown, revoked, expired or
// used-up code, and one whose creator has blocked the caller, are all INVITE_NOT_FOUND.
message RedeemInvite {
  string code = 1;
}

message InviteRedeemed {
  // The invite's creator, now an accepted contact.
  bytes user_id = 1;
}

// Revokes one of the caller's invites. Revoking again does nothing.
message RevokeInvite {
  string code = 1;
}

message Ping {}
```

In `crates/push-proto/proto/push/internal/v1/messaging.proto`:

Replace

```protobuf
}

enum ContactState {
  CONTACT_STATE_NONE = 0;
  CONTACT_STATE_REQUESTED_OUT = 1;
  CONTACT_STATE_REQUESTED_IN = 2;
  CONTACT_STATE_ACCEPTED = 3;
  CONTACT_STATE_BLOCKED = 4;
}

message ContactChanged {
  bytes peer_id = 1;
  ContactState state = 2;
}
```

with

```protobuf
}

message ContactChanged {
  bytes peer_id = 1;
  push.v1.ContactState state = 2;
}
```

- [ ] **Step 3: The requests**

The mapping of contact states moves to `convert.rs`, shared by the requests and the
notifier. In `crates/push-message-svc/src/convert.rs`:

Replace

```rust
use chrono::{DateTime, Utc};
use push_domain::{ContentType, DomainError, Entry, EntryKind, SendReceipt};
use push_proto::v1::{self, Deliver, ErrorCode, SendAck};
use push_proto::{error_status, id, id_bytes};
```

with

```rust
use chrono::{DateTime, Utc};
use push_domain::{ContactState, ContentType, DomainError, Entry, EntryKind, SendReceipt};
use push_proto::v1::{self, Deliver, ErrorCode, SendAck};
use push_proto::{error_status, id, id_bytes};
```

Replace

```rust
}

#[cfg(test)]
mod tests {
```

with

```rust
}

/// A user's view of a relationship; `None` is no relationship.
pub(crate) fn contact_state(state: Option<ContactState>) -> v1::ContactState {
    match state {
        None => v1::ContactState::None,
        Some(ContactState::RequestedOut) => v1::ContactState::RequestedOut,
        Some(ContactState::RequestedIn) => v1::ContactState::RequestedIn,
        Some(ContactState::Accepted) => v1::ContactState::Accepted,
        Some(ContactState::Blocked) => v1::ContactState::Blocked,
    }
}

#[cfg(test)]
mod tests {
```

In `crates/push-message-svc/src/notify.rs`:

Replace

```rust
use async_trait::async_trait;
use prost::Message;
use push_domain::{ContactState, Event, KickReason, Notifier, ReceiptUpdate};
use push_proto::internal::v1::{
    self as internal, ContactChanged, DirectReceipt, EntriesAvailable, GroupReceipt, Kick,
```

with

```rust
use async_trait::async_trait;
use prost::Message;
use push_domain::{Event, KickReason, Notifier, ReceiptUpdate};
use push_proto::internal::v1::{
    self as internal, ContactChanged, DirectReceipt, EntriesAvailable, GroupReceipt, Kick,
```

Replace

```rust
use redis::aio::ConnectionManager;

/// Publishes each event as small [`Notification`]s on the Redis channels of the users it
/// concerns (`notify:{user_id}`), and kicks as [`Kick`]s on the channels of the devices
```

with

```rust
use redis::aio::ConnectionManager;

use crate::convert::contact_state;

/// Publishes each event as small [`Notification`]s on the Redis channels of the users it
/// concerns (`notify:{user_id}`), and kicks as [`Kick`]s on the channels of the devices
```

Replace

```rust
}

fn contact_state(state: Option<ContactState>) -> internal::ContactState {
    match state {
        None => internal::ContactState::None,
        Some(ContactState::RequestedOut) => internal::ContactState::RequestedOut,
        Some(ContactState::RequestedIn) => internal::ContactState::RequestedIn,
        Some(ContactState::Accepted) => internal::ContactState::Accepted,
        Some(ContactState::Blocked) => internal::ContactState::Blocked,
    }
}

#[cfg(test)]
mod tests {
```

with

```rust
}

#[cfg(test)]
mod tests {
```

Replace

```rust
                    Kind::ContactChanged(ContactChanged {
                        peer_id: id_bytes(bob.0),
                        state: internal::ContactState::None.into(),
                    })
                ),
```

with

```rust
                    Kind::ContactChanged(ContactChanged {
                        peer_id: id_bytes(bob.0),
                        state: push_proto::v1::ContactState::None.into(),
                    })
                ),
```

In `crates/push-message-svc/src/requests.rs`:

Replace

```rust
//! of the user and device it verified.

use push_domain::{
    ClientMsgId, Clock, ConversationId, ConversationKind, ConversationSummary, DeleteMessage,
    DeviceId, EditText, MAX_HISTORY_PAGE, MessageId, MessageRef, Messaging, Notifier, Receipts,
    Store, UserId,
};
use push_proto::internal::v1::{
```

with

```rust
//! of the user and device it verified.

use chrono::TimeDelta;
use push_domain::{
    ClientMsgId, Clock, ConversationId, ConversationKind, ConversationSummary, DeleteMessage,
    DeviceId, DomainResult, EditText, MAX_HISTORY_PAGE, MessageId, MessageRef, Messaging, Notifier,
    Receipts, Store, UserId,
};
use push_proto::internal::v1::{
```

Replace

```rust
};
use push_proto::v1::{
    self, ConversationList, DirectReceipts, GroupReceipts, HistoryPage, SendAck,
    conversation_summary, delete, edit, request, response,
};
```

with

```rust
};
use push_proto::v1::{
    self, Contact, ContactList, ConversationList, DirectOpened, DirectReceipts, Done,
    GroupReceipts, HistoryPage, Invite, InviteRedeemed, ListContacts, SendAck,
    conversation_summary, delete, edit, request, response,
};
```

Replace

```rust
use tonic::Status;

use crate::convert::{deliver, domain_error, from_micros, invalid, micros, parse_id, send_ack};

/// `Edit`: the edit entry's receipt, after commit.
```

with

```rust
use tonic::Status;

use crate::convert::{
    contact_state, deliver, domain_error, from_micros, invalid, micros, parse_id, send_ack,
};

/// `Edit`: the edit entry's receipt, after commit.
```

Replace

```rust
            })
        }
    })
}

fn summary(summary: ConversationSummary) -> v1::ConversationSummary {
    let receipts = match summary.receipts {
```

with

```rust
            })
        }
        request::Body::OpenDirect(open) => {
            let peer = parse_id(&open.peer_user_id, "peer_user_id", UserId)?;
            let conversation = messaging
                .open_direct(user, peer)
                .await
                .map_err(domain_error)?;
            response::Body::DirectOpened(DirectOpened {
                conversation_id: id_bytes(conversation.0),
            })
        }
        request::Body::ContactRequest(contact) => {
            let state = messaging
                .contact_request(user, other(&contact.user_id)?)
                .await
                .map_err(domain_error)?;
            response::Body::Contact(Contact {
                user_id: contact.user_id,
                state: contact_state(Some(state)).into(),
            })
        }
        request::Body::ContactAccept(contact) => done(
            messaging
                .contact_accept(user, other(&contact.user_id)?)
                .await,
        )?,
        request::Body::ContactDecline(contact) => done(
            messaging
                .contact_decline(user, other(&contact.user_id)?)
                .await,
        )?,
        request::Body::ContactBlock(contact) => done(
            messaging
                .contact_block(user, other(&contact.user_id)?)
                .await,
        )?,
        request::Body::ContactUnblock(contact) => done(
            messaging
                .contact_unblock(user, other(&contact.user_id)?)
                .await,
        )?,
        request::Body::ListContacts(ListContacts {}) => {
            let contacts = messaging.list_contacts(user).await.map_err(domain_error)?;
            response::Body::Contacts(ContactList {
                contacts: contacts
                    .into_iter()
                    .map(|contact| Contact {
                        user_id: id_bytes(contact.user_id.0),
                        state: contact_state(Some(contact.state)).into(),
                    })
                    .collect(),
            })
        }
        request::Body::CreateInvite(create) => {
            let expires_in = TimeDelta::seconds(i64::from(create.expires_in_s));
            let invite = messaging
                .create_invite(user, expires_in, create.max_uses)
                .await
                .map_err(domain_error)?;
            response::Body::Invite(Invite {
                code: invite.code,
                expires_at: micros(invite.expires_at),
                max_uses: invite.max_uses,
                uses: invite.uses,
            })
        }
        request::Body::RedeemInvite(redeem) => {
            let creator = messaging
                .redeem_invite(user, &redeem.code)
                .await
                .map_err(domain_error)?;
            response::Body::InviteRedeemed(InviteRedeemed {
                user_id: id_bytes(creator.0),
            })
        }
        request::Body::RevokeInvite(revoke) => {
            done(messaging.revoke_invite(user, &revoke.code).await)?
        }
    })
}

/// The other user a contact request names.
fn other(user_id: &[u8]) -> Result<UserId, Status> {
    parse_id(user_id, "user_id", UserId)
}

/// The answer to a request that returns nothing but success.
fn done(result: DomainResult<()>) -> Result<response::Body, Status> {
    result.map_err(domain_error)?;
    Ok(response::Body::Done(Done {}))
}

fn summary(summary: ConversationSummary) -> v1::ConversationSummary {
    let receipts = match summary.receipts {
```

```powershell
cargo test -p push-message-svc
```

Expected: everything passes, the 2 new tests included.

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "feat(message-svc): contacts, invites and direct conversations over gRPC"
```

Expected: 795 tests pass, no warnings, no diff.

---

### Task 6: Groups over gRPC, and the client API on Postgres

**Files:**
- Modify: `crates/push-proto/proto/push/v1/frame.proto`,
  `crates/push-message-svc/src/requests.rs`, `src/requests/tests.rs`,
  `crates/push-message-svc/tests/service.rs`

- [ ] **Step 1: The tests, failing**

Only admins rename, add (their own contacts) and remove; there can be several admins and
the last one cannot step down; when the last admin leaves, the member who joined first
takes over; a group has at most 100 members; a direct conversation is not a group. In
`crates/push-message-svc/src/requests/tests.rs`:

Replace

```rust
};
use push_proto::v1::{
    self, Contact, ContactAccept, ContactBlock, ContactRequest, ContactState, ContactUnblock,
    ContentType, ConversationKind, CreateInvite, Delete, Deliver, DirectReceipts, Done, Edit,
    EntryKind, ErrorCode, FetchHistory, InviteRedeemed, ListContacts, ListConversations,
    OpenDirect, ReadUpTo, RedeemInvite, RevokeInvite, SendAck, conversation_summary, delete, edit,
    request, response,
};
use push_proto::{error_from_status, id_bytes};
```

with

```rust
};
use push_proto::v1::{
    self, AddMembers, Contact, ContactAccept, ContactBlock, ContactRequest, ContactState,
    ContactUnblock, ContentType, ConversationKind, CreateGroup, CreateInvite, Delete, Deliver,
    DirectReceipts, Done, Edit, EntryKind, ErrorCode, FetchHistory, InviteRedeemed, LeaveGroup,
    ListContacts, ListConversations, OpenDirect, ReadUpTo, RedeemInvite, RemoveMember, RenameGroup,
    RevokeInvite, Role, SendAck, SetRole, conversation_summary, delete, edit, request, response,
};
use push_proto::{error_from_status, id_bytes};
```

Replace

```rust
    let status = world.request(&alice, too_many).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::Invalid);
}
```

with

```rust
    let status = world.request(&alice, too_many).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::Invalid);
}

#[tokio::test]
async fn a_group_is_created_changed_and_handed_over_by_its_admins() {
    let world = world();
    let (alice, bob, carol, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
        world.person().await,
    );
    for other in [&bob, &carol, &dave] {
        world.befriend(&alice, other).await;
    }
    let create = request::Body::CreateGroup(CreateGroup {
        title: " Trip ".to_owned(),
        member_ids: vec![id_bytes(bob.user.0), id_bytes(carol.user.0)],
    });
    let response::Body::GroupCreated(created) = world.request(&alice, create).await.unwrap() else {
        panic!("not a created group");
    };
    let group = created.conversation_id;

    // Only admins change the group, and add only their own contacts.
    let rename = |title: &str| {
        request::Body::RenameGroup(RenameGroup {
            conversation_id: group.clone(),
            title: title.to_owned(),
        })
    };
    let status = world.request(&bob, rename("Bob's")).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::NotAdmin, true));
    world.request(&alice, rename("Holiday")).await.unwrap();
    let add = |person: &Person| {
        request::Body::AddMembers(AddMembers {
            conversation_id: group.clone(),
            user_ids: vec![id_bytes(person.user.0)],
        })
    };
    assert_eq!(
        world.request(&alice, add(&dave)).await.unwrap(),
        response::Body::Done(Done {})
    );
    let stranger = world.person().await;
    let status = world.request(&alice, add(&stranger)).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NotContact);

    // Several admins; the last one cannot step down.
    let set_role = |person: &Person, role: Role| {
        request::Body::SetRole(SetRole {
            conversation_id: group.clone(),
            user_id: id_bytes(person.user.0),
            role: role.into(),
        })
    };
    world
        .request(&alice, set_role(&bob, Role::Admin))
        .await
        .unwrap();
    world
        .request(&alice, set_role(&alice, Role::Member))
        .await
        .unwrap();
    let status = world
        .request(&bob, set_role(&bob, Role::Member))
        .await
        .unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::LastAdmin);
    let remove = request::Body::RemoveMember(RemoveMember {
        conversation_id: group.clone(),
        user_id: id_bytes(carol.user.0),
    });
    let status = world.request(&alice, remove.clone()).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NotAdmin);
    world.request(&bob, remove).await.unwrap();

    // The last admin leaves: the member who joined first, Alice, takes over.
    let leave = request::Body::LeaveGroup(LeaveGroup {
        conversation_id: group.clone(),
    });
    world.request(&bob, leave).await.unwrap();
    let response::Body::Conversations(listed) = world
        .request(&dave, list_conversations(None))
        .await
        .unwrap()
    else {
        panic!("not a conversation list");
    };
    let summary = &listed.conversations[0];
    assert_eq!(summary.kind(), ConversationKind::Group);
    assert_eq!(summary.title.as_deref(), Some("Holiday"));
    assert_eq!(summary.admins, [id_bytes(alice.user.0)]);
    let mut members = vec![id_bytes(alice.user.0), id_bytes(dave.user.0)];
    members.sort();
    assert_eq!(summary.members, members);
    assert!(matches!(
        summary.receipts,
        Some(conversation_summary::Receipts::Group(_))
    ));
}

#[tokio::test]
async fn group_requests_refuse_what_the_rules_forbid() {
    let world = world();
    let (alice, _bob, chat) = world.chat().await;
    let crowd = request::Body::CreateGroup(CreateGroup {
        title: "Crowd".to_owned(),
        member_ids: (0..100).map(|_| id_bytes(UserId::new().0)).collect(),
    });
    let status = world.request(&alice, crowd).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::GroupFull);

    let malformed = request::Body::CreateGroup(CreateGroup {
        title: "Odd".to_owned(),
        member_ids: vec![vec![1, 2, 3]],
    });
    let status = world.request(&alice, malformed).await.unwrap_err();
    let error = error_from_status(&status);
    assert_eq!(error.code(), ErrorCode::Invalid);
    assert!(error.message.starts_with("member_ids"), "{}", error.message);

    let rename = request::Body::RenameGroup(RenameGroup {
        conversation_id: id_bytes(chat.0),
        title: "Not a group".to_owned(),
    });
    let status = world.request(&alice, rename).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::NotAGroup);

    let unspecified = request::Body::SetRole(SetRole {
        conversation_id: id_bytes(chat.0),
        user_id: id_bytes(alice.user.0),
        role: Role::Unspecified.into(),
    });
    let status = world.request(&alice, unspecified).await.unwrap_err();
    assert_eq!(failure(&status), (ErrorCode::Invalid, true));
}
```

And the whole client API over real Postgres, with users relayed through `Accounts`: an
invite and a request make contacts, a group, a send, an edit, a read, history and a
listing whose group-wide receipts count only what every member read. In
`crates/push-message-svc/tests/service.rs`:

Replace

```rust
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AccountDevice, AckRequest, IdentifyRequest, Kick, KickReason, MakeContactsRequest,
    Notification, OpenDirectRequest, RegisterDeviceRequest, RegisterUserRequest, SendRequest,
    SyncRequest, notification::Kind,
};
use push_proto::v1::{Ack, ContentType, Send};
use push_proto::{id_bytes, kick_channel, notify_channel};
use push_store::StoreOptions;
```

with

```rust
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AccountDevice, AckRequest, EditRequest, IdentifyRequest, Kick, KickReason, MakeContactsRequest,
    Notification, OpenDirectRequest, ReadUpToRequest, RegisterDeviceRequest, RegisterUserRequest,
    RequestCall, SendRequest, SyncRequest, notification::Kind,
};
use push_proto::v1::{
    self, Ack, ContactAccept, ContactRequest, ContentType, CreateGroup, CreateInvite, Edit,
    FetchHistory, GroupReceipts, ListConversations, ReadUpTo, RedeemInvite, Send,
    conversation_summary, edit, request, response,
};
use push_proto::{id_bytes, kick_channel, notify_channel};
use push_store::StoreOptions;
```

Replace

```rust
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use uuid::Uuid;
```

with

```rust
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use uuid::Uuid;
```

Replace

```rust
    running.server.await.unwrap().unwrap();
}
```

with

```rust
    running.server.await.unwrap().unwrap();
}

/// The Accounts client the worker uses, with the internal token.
async fn accounts_client(
    url: String,
) -> AccountsClient<InterceptedService<Channel, impl Interceptor>> {
    let channel = Channel::from_shared(url).unwrap().connect().await.unwrap();
    let bearer: MetadataValue<_> = format!("Bearer {ACCOUNTS_TOKEN}").parse().unwrap();
    AccountsClient::with_interceptor(channel, move |mut request: Request<()>| {
        request
            .metadata_mut()
            .insert("authorization", bearer.clone());
        Ok(request)
    })
}

/// A request frame as `user`.
fn call(user: Uuid, body: request::Body) -> RequestCall {
    RequestCall {
        user_id: id_bytes(user),
        request: Some(v1::Request {
            request_id: 1,
            body: Some(body),
        }),
    }
}

#[tokio::test]
async fn the_client_requests_run_on_postgres() {
    let running = start(false).await;
    let url = format!("http://{}", running.grpc);
    let mut accounts = accounts_client(url.clone()).await;
    let mut messaging = MessagingClient::connect(url).await.unwrap();
    let (alice, bob, carol) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let devices = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
    for (user, device) in [alice, bob, carol].into_iter().zip(devices) {
        accounts
            .device_registered(AccountDevice {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
            })
            .await
            .unwrap();
    }

    // Alice invites Bob; Carol asks Alice, who accepts.
    let create = request::Body::CreateInvite(CreateInvite {
        expires_in_s: 600,
        max_uses: 1,
    });
    let Some(response::Body::Invite(invite)) = messaging
        .request(call(alice, create))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not an invite");
    };
    let redeem = request::Body::RedeemInvite(RedeemInvite { code: invite.code });
    messaging.request(call(bob, redeem)).await.unwrap();
    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(alice),
    });
    messaging.request(call(carol, ask)).await.unwrap();
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(carol),
    });
    messaging.request(call(alice, accept)).await.unwrap();

    let group = request::Body::CreateGroup(CreateGroup {
        title: "Trip".to_owned(),
        member_ids: vec![id_bytes(bob), id_bytes(carol)],
    });
    let Some(response::Body::GroupCreated(created)) = messaging
        .request(call(alice, group))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not a created group");
    };
    let sent = messaging
        .send(SendRequest {
            user_id: id_bytes(alice),
            device_id: id_bytes(devices[0]),
            send: Some(Send {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: created.conversation_id.clone(),
                content_type: ContentType::Text.into(),
                body: b"helo".to_vec(),
                client_ts: None,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    messaging
        .edit(EditRequest {
            user_id: id_bytes(alice),
            device_id: id_bytes(devices[0]),
            edit: Some(Edit {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: created.conversation_id.clone(),
                target: Some(edit::Target::TargetMessageId(sent.message_id)),
                body: b"hello".to_vec(),
                client_ts: None,
            }),
        })
        .await
        .unwrap();
    let read = messaging
        .read_up_to(ReadUpToRequest {
            user_id: id_bytes(bob),
            read: Some(ReadUpTo {
                conversation_id: created.conversation_id.clone(),
                seq: sent.seq,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(read.moved);

    let fetch = request::Body::FetchHistory(FetchHistory {
        conversation_id: created.conversation_id.clone(),
        before_seq: None,
        limit: 0,
    });
    let Some(response::Body::History(page)) = messaging
        .request(call(carol, fetch))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not a history page");
    };
    let bodies: Vec<Option<&[u8]>> = page
        .entries
        .iter()
        .map(|entry| entry.body.as_deref())
        .collect();
    assert_eq!(bodies.len(), 2, "the created entry and the message");
    assert_eq!(bodies[1], Some(b"hello".as_slice()));

    let list = request::Body::ListConversations(ListConversations::default());
    let Some(response::Body::Conversations(listed)) = messaging
        .request(call(alice, list))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not a conversation list");
    };
    assert_eq!(listed.conversations.len(), 1);
    assert_eq!(listed.conversations[0].title.as_deref(), Some("Trip"));
    assert_eq!(
        listed.conversations[0].receipts,
        Some(conversation_summary::Receipts::Group(GroupReceipts {
            all_delivered_seq: 1,
            all_read_seq: 1,
        })),
        "Bob read the message, Carol did not: only the created entry counts as read by all"
    );

    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
}
```

```powershell
cargo test -p push-message-svc
```

Expected: compile errors — no `CreateGroup`, `AddMembers`, `SetRole`, `Role` or the other
group messages in `push.v1`.

- [ ] **Step 2: The contract and the requests**

In `crates/push-proto/proto/push/v1/frame.proto`:

Replace

```protobuf
    RedeemInvite redeem_invite = 12;
    RevokeInvite revoke_invite = 13;
  }
}
```

with

```protobuf
    RedeemInvite redeem_invite = 12;
    RevokeInvite revoke_invite = 13;
    CreateGroup create_group = 14;
    AddMembers add_members = 15;
    RemoveMember remove_member = 16;
    LeaveGroup leave_group = 17;
    RenameGroup rename_group = 18;
    SetRole set_role = 19;
  }
}
```

Replace

```protobuf
    // A request that answers nothing but success.
    Done done = 9;
  }
}
```

with

```protobuf
    // A request that answers nothing but success.
    Done done = 9;
    GroupCreated group_created = 10;
  }
}
```

Replace

```protobuf
}

message Ping {}
```

with

```protobuf
}

// A group (spec §6) with the caller as its admin. Every other member must be an accepted
// contact of the caller; a group has at most 100 members.
message CreateGroup {
  // 1 to 100 characters after trimming.
  string title = 1;
  repeated bytes member_ids = 2;
}

message GroupCreated {
  bytes conversation_id = 1;
}

// Admins only. The new members must be accepted contacts of the admin; they see the log
// from the entry that adds them.
message AddMembers {
  bytes conversation_id = 1;
  repeated bytes user_ids = 2;
}

// Admins only; an admin who wants to go leaves instead.
message RemoveMember {
  bytes conversation_id = 1;
  bytes user_id = 2;
}

// When the last admin leaves, the member who joined first becomes admin.
message LeaveGroup {
  bytes conversation_id = 1;
}

// Admins only.
message RenameGroup {
  bytes conversation_id = 1;
  string title = 2;
}

enum Role {
  ROLE_UNSPECIFIED = 0;
  ROLE_ADMIN = 1;
  ROLE_MEMBER = 2;
}

// Admins only. A group keeps at least one admin: the last one cannot be demoted.
message SetRole {
  bytes conversation_id = 1;
  bytes user_id = 2;
  Role role = 3;
}

message Ping {}
```

In `crates/push-message-svc/src/requests.rs`:

Replace

```rust
    ClientMsgId, Clock, ConversationId, ConversationKind, ConversationSummary, DeleteMessage,
    DeviceId, DomainResult, EditText, MAX_HISTORY_PAGE, MessageId, MessageRef, Messaging, Notifier,
    Receipts, Store, UserId,
};
use push_proto::internal::v1::{
```

with

```rust
    ClientMsgId, Clock, ConversationId, ConversationKind, ConversationSummary, DeleteMessage,
    DeviceId, DomainResult, EditText, MAX_HISTORY_PAGE, MessageId, MessageRef, Messaging, Notifier,
    Receipts, Role, Store, UserId,
};
use push_proto::internal::v1::{
```

Replace

```rust
};
use push_proto::v1::{
    self, Contact, ContactList, ConversationList, DirectOpened, DirectReceipts, Done,
    GroupReceipts, HistoryPage, Invite, InviteRedeemed, ListContacts, SendAck,
    conversation_summary, delete, edit, request, response,
```

with

```rust
};
use push_proto::v1::{
    self, Contact, ContactList, ConversationList, DirectOpened, DirectReceipts, Done, GroupCreated,
    GroupReceipts, HistoryPage, Invite, InviteRedeemed, ListContacts, SendAck,
    conversation_summary, delete, edit, request, response,
```

Replace

```rust
            done(messaging.revoke_invite(user, &revoke.code).await)?
        }
    })
}

/// The other user a contact request names.
fn other(user_id: &[u8]) -> Result<UserId, Status> {
```

with

```rust
            done(messaging.revoke_invite(user, &revoke.code).await)?
        }
        request::Body::CreateGroup(create) => {
            let members = user_ids(&create.member_ids, "member_ids")?;
            let conversation = messaging
                .create_group(user, &create.title, &members)
                .await
                .map_err(domain_error)?;
            response::Body::GroupCreated(GroupCreated {
                conversation_id: id_bytes(conversation.0),
            })
        }
        request::Body::AddMembers(add) => {
            let group = group(&add.conversation_id)?;
            let users = user_ids(&add.user_ids, "user_ids")?;
            done(messaging.add_members(user, group, &users).await)?
        }
        request::Body::RemoveMember(remove) => {
            let group = group(&remove.conversation_id)?;
            done(
                messaging
                    .remove_member(user, group, other(&remove.user_id)?)
                    .await,
            )?
        }
        request::Body::LeaveGroup(leave) => done(
            messaging
                .leave_group(user, group(&leave.conversation_id)?)
                .await,
        )?,
        request::Body::RenameGroup(rename) => {
            let group = group(&rename.conversation_id)?;
            done(messaging.rename_group(user, group, &rename.title).await)?
        }
        request::Body::SetRole(set) => {
            let group = group(&set.conversation_id)?;
            let role = match set.role() {
                v1::Role::Admin => Role::Admin,
                v1::Role::Member => Role::Member,
                v1::Role::Unspecified => return Err(invalid("role must be admin or member")),
            };
            done(
                messaging
                    .set_role(user, group, other(&set.user_id)?, role)
                    .await,
            )?
        }
    })
}

/// The group a request names.
fn group(conversation_id: &[u8]) -> Result<ConversationId, Status> {
    parse_id(conversation_id, "conversation_id", ConversationId)
}

/// A list of user ids; any malformed one is `INVALID_ARGUMENT` naming `field`.
fn user_ids(ids: &[Vec<u8>], field: &str) -> Result<Vec<UserId>, Status> {
    ids.iter().map(|id| parse_id(id, field, UserId)).collect()
}

/// The other user a contact request names.
fn other(user_id: &[u8]) -> Result<UserId, Status> {
```

```powershell
cargo test -p push-message-svc
```

Expected: everything passes, the 2 new unit tests and `the_client_requests_run_on_postgres`
included.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "feat(message-svc): groups over gRPC, and the client API on Postgres"
```

Expected: 798 tests pass, no warnings, no diff.

---

### Task 7: The `Directory` API, behind auth-svc's own token

**Files:**
- Modify: `crates/push-testkit/src/contract/contacts.rs`, `src/scenarios/contacts.rs`,
  `crates/push-domain/src/store.rs`, `src/memory.rs`, `src/contacts.rs`,
  `crates/push-store/src/store.rs`,
  `crates/push-proto/proto/push/internal/v1/messaging.proto`,
  `crates/push-message-svc/src/grpc.rs`, `src/config.rs`, `src/server.rs`, `src/lib.rs`,
  `crates/push-message-svc/tests/service.rs`, `crates/push-smoke/tests/local.rs`,
  `crates/push-worker/tests/relay.rs`, `crates/push-worker/tests/server.rs`

- [ ] **Step 1: The contract case and the scenario, failing**

`related_users` returns the candidates the user has a contact row for or shares a
conversation with, current or former, never the user (decision 6). In
`crates/push-testkit/src/contract/contacts.rs`:

Replace

```rust
use push_domain::{ContactState, DomainError, InsertOutcome, Store, Tx, UserId};

use super::{add_users, invite, start};
use crate::{Backend, Case, case_list};
```

with

```rust
use push_domain::{ContactState, DomainError, InsertOutcome, Store, Tx, UserId};

use super::{add_users, conversation, invite, member, start, store_with};
use crate::{Backend, Case, case_list};
```

Replace

```rust
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

with

```rust
}

async fn related_users_have_a_contact_row_of_the_user_or_share_a_conversation<B: Backend>(
    backend: &B,
) {
    let chat = conversation();
    let store = store_with(backend, chat.clone()).await;
    let [alice, bob, carol, dave, erin] = [(); 5].map(|()| UserId::new());
    add_users(&store, &[alice, bob, carol, dave, erin]).await;
    let mut tx = store.begin().await.unwrap();
    tx.put_contact(alice, bob, ContactState::RequestedOut)
        .await
        .unwrap();
    // Only Carol's row names Alice.
    tx.put_contact(carol, alice, ContactState::Blocked)
        .await
        .unwrap();
    tx.insert_member(member(chat.id, alice)).await.unwrap();
    let mut former = member(chat.id, dave);
    former.left_at_seq = Some(0);
    tx.insert_member(former).await.unwrap();

    let stranger = UserId::new();
    let candidates = [bob, carol, dave, erin, stranger, alice, bob];
    let mut related = tx.related_users(alice, &candidates).await.unwrap();
    related.sort();
    let mut expected = vec![bob, dave];
    expected.sort();
    assert_eq!(related, expected);
    assert_eq!(tx.related_users(carol, &[alice]).await, Ok(vec![alice]));
    assert_eq!(tx.related_users(erin, &[alice, bob]).await, Ok(vec![]));
    assert_eq!(tx.related_users(alice, &[]).await, Ok(vec![]));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

Replace

```rust
        an_invite_of_an_unknown_user_is_refused,
        update_invite_changes_only_uses_and_revocation,
    ]
}
```

with

```rust
        an_invite_of_an_unknown_user_is_refused,
        update_invite_changes_only_uses_and_revocation,
        related_users_have_a_contact_row_of_the_user_or_share_a_conversation,
    ]
}
```

Requests either way and shared groups make profiles visible; a block stays hidden. In
`crates/push-testkit/src/scenarios/contacts.rs`:

Replace

```rust
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

with

```rust
}

async fn profiles_are_visible_to_relations_and_conversation_members<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let (dave, erin, stranger) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    // Carol and Dave share Alice's group without being contacts.
    world.group(&alice, &[&carol, &dave]).await;
    world
        .svc
        .contact_request(erin.user, alice.user)
        .await
        .unwrap();
    world
        .svc
        .contact_block(alice.user, erin.user)
        .await
        .unwrap();

    let visible = |user: &Person, candidates: &[&Person]| {
        let candidates: Vec<UserId> = candidates.iter().map(|person| person.user).collect();
        let svc = &world.svc;
        let user = user.user;
        async move { svc.visible_users(user, &candidates).await.unwrap() }
    };
    let sorted = |people: &[&Person]| {
        let mut users: Vec<UserId> = people.iter().map(|person| person.user).collect();
        users.sort();
        users
    };
    assert_eq!(
        visible(&bob, &[&alice]).await,
        [alice.user],
        "a request, either way"
    );
    assert_eq!(
        visible(&carol, &[&dave, &carol, &stranger, &dave]).await,
        sorted(&[&carol, &dave])
    );
    assert_eq!(
        visible(&alice, &[&erin]).await,
        [erin.user],
        "Alice blocked Erin"
    );
    assert!(
        visible(&erin, &[&alice]).await.is_empty(),
        "the block stays hidden"
    );
    assert!(visible(&stranger, &[&alice, &bob]).await.is_empty());
}
pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

Replace

```rust
        unblocking_someone_who_is_not_blocked_publishes_nothing,
        contacts_are_listed_from_the_owners_side_only,
    ]
}
```

with

```rust
        unblocking_someone_who_is_not_blocked_publishes_nothing,
        contacts_are_listed_from_the_owners_side_only,
        profiles_are_visible_to_relations_and_conversation_members,
    ]
}
```

```powershell
cargo test -p push-testkit
```

Expected: compile errors — no method `related_users` on the transaction, no
`visible_users` on `Messaging`.

- [ ] **Step 2: The domain and both stores**

In `crates/push-domain/src/store.rs`:

Replace

```rust
    async fn contacts_of(&mut self, owner: UserId) -> DomainResult<Vec<(UserId, ContactState)>>;

    // --- Invites -----------------------------------------------------------------------
```

with

```rust
    async fn contacts_of(&mut self, owner: UserId) -> DomainResult<Vec<(UserId, ContactState)>>;

    /// The users among `candidates`, other than `user`, whom `user` has a contact row for
    /// (in any state), or who are or were members of a conversation `user` is or was a
    /// member of. Duplicates in `candidates` are ignored. One statement on Postgres. The
    /// order of the result is unspecified.
    async fn related_users(
        &mut self,
        user: UserId,
        candidates: &[UserId],
    ) -> DomainResult<Vec<UserId>>;

    // --- Invites -----------------------------------------------------------------------
```

In `crates/push-domain/src/memory.rs`:

Replace

```rust
    }

    async fn insert_invite(&mut self, invite: InviteRecord) -> DomainResult<InsertOutcome> {
        if !self.work.users.contains(&invite.user_id) {
```

with

```rust
    }

    async fn related_users(
        &mut self,
        user: UserId,
        candidates: &[UserId],
    ) -> DomainResult<Vec<UserId>> {
        let shared: HashSet<ConversationId> = self
            .work
            .members
            .keys()
            .filter(|(_, member)| *member == user)
            .map(|(conversation, _)| *conversation)
            .collect();
        let mut related: Vec<UserId> = candidates
            .iter()
            .copied()
            .filter(|candidate| *candidate != user)
            .filter(|candidate| {
                self.work.contacts.contains_key(&(user, *candidate))
                    || self.work.members.keys().any(|(conversation, member)| {
                        member == candidate && shared.contains(conversation)
                    })
            })
            .collect();
        related.sort();
        related.dedup();
        Ok(related)
    }

    async fn insert_invite(&mut self, invite: InviteRecord) -> DomainResult<InsertOutcome> {
        if !self.work.users.contains(&invite.user_id) {
```

In `crates/push-store/src/store.rs` (one statement, served by the primary key and the
`(user_id)` index of `conversation_member`):

Replace

```rust
    }

    async fn insert_invite(&mut self, invite: InviteRecord) -> DomainResult<InsertOutcome> {
        let inserted = sqlx::query(
```

with

```rust
    }

    async fn related_users(
        &mut self,
        user: UserId,
        candidates: &[UserId],
    ) -> DomainResult<Vec<UserId>> {
        let candidates: Vec<Uuid> = candidates.iter().map(|candidate| candidate.0).collect();
        let related: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT candidate FROM unnest($2::uuid[]) AS candidate \
             WHERE candidate <> $1 AND ( \
               EXISTS (SELECT 1 FROM messaging.contact \
                       WHERE owner_id = $1 AND peer_id = candidate) \
               OR EXISTS (SELECT 1 FROM messaging.conversation_member mine \
                          JOIN messaging.conversation_member theirs USING (conversation_id) \
                          WHERE mine.user_id = $1 AND theirs.user_id = candidate))",
        )
        .bind(user.0)
        .bind(candidates)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        Ok(related.into_iter().map(UserId).collect())
    }

    async fn insert_invite(&mut self, invite: InviteRecord) -> DomainResult<InsertOutcome> {
        let inserted = sqlx::query(
```

In `crates/push-domain/src/contacts.rs`:

Replace

```rust
    }

    /// Begins a transaction for an operation between two distinct, known users.
    async fn begin_pair(&self, a: UserId, b: UserId) -> DomainResult<S::Tx> {
```

with

```rust
    }

    /// The users among `candidates` whose profiles `user` may see (spec §5.5): `user`
    /// themselves, everyone they have a relationship with as they see it (requests either
    /// way, accepted contacts, users they blocked), and everyone they share or shared a
    /// conversation with. Sorted, without duplicates. Someone who blocked `user` stays
    /// hidden unless they share a conversation, so nobody learns of a block through it.
    pub async fn visible_users(
        &self,
        user: UserId,
        candidates: &[UserId],
    ) -> DomainResult<Vec<UserId>> {
        let mut tx = self.store.begin().await?;
        let mut visible = tx.related_users(user, candidates).await?;
        tx.rollback().await?;
        if candidates.contains(&user) {
            visible.push(user);
        }
        visible.sort();
        visible.dedup();
        Ok(visible)
    }

    /// Begins a transaction for an operation between two distinct, known users.
    async fn begin_pair(&self, a: UserId, b: UserId) -> DomainResult<S::Tx> {
```

```powershell
cargo test -p push-testkit
cargo test -p push-store --test postgres -- contract::contacts scenarios::contacts
```

Expected: both suites pass, each with 2 more cases than before.

- [ ] **Step 3: The `Directory` service and its token, tests first**

In `crates/push-proto/proto/push/internal/v1/messaging.proto`:

Replace

```protobuf
// The internal API of message-svc, called by the gateway (Messaging) and the worker
// (Accounts) (spec §2). Never exposed outside the cluster. Callers pass the user and
// device the gateway verified, or auth-svc announced; message-svc trusts them.
//
// Errors are gRPC statuses whose details (`grpc-status-details-bin`) follow gRPC's rich
```

with

```protobuf
// The internal API of message-svc, called by the gateway (Messaging), the worker
// (Accounts) and auth-svc (Directory) (spec §2). Never exposed outside the cluster.
// Callers pass the user and device the gateway verified, or auth-svc announced;
// message-svc trusts them.
//
// Errors are gRPC statuses whose details (`grpc-status-details-bin`) follow gRPC's rich
```

Replace

```protobuf
}

message IdentifyRequest {
  bytes user_id = 1;
```

with

```protobuf
}

// Whose profile a user may see (spec §5.5), for auth-svc's `GET /v1/users?ids=…`: auth-svc
// owns the profiles, message-svc the contacts and conversations. Every call carries
// auth-svc's own internal token as `authorization: Bearer …` metadata (not the worker's);
// without it the call is UNAUTHENTICATED.
service Directory {
  // The users among user_ids whose profile user_id may see: user_id itself, the users it
  // has a relationship with as it sees it (requests either way, accepted contacts, users
  // it blocked) and everyone it shares or shared a conversation with. At most 100 ids;
  // malformed ids are INVALID_ARGUMENT.
  rpc VisibleUsers(VisibleUsersRequest) returns (VisibleUsersResponse);
}

message IdentifyRequest {
  bytes user_id = 1;
```

Replace

```protobuf
message AccountsResponse {}
```

with

```protobuf
message AccountsResponse {}

message VisibleUsersRequest {
  bytes user_id = 1;
  repeated bytes user_ids = 2;
}

message VisibleUsersResponse {
  // Sorted, without duplicates.
  repeated bytes user_ids = 1;
}
```

In `crates/push-message-svc/src/grpc.rs`: `DirectoryApi`, `InternalToken` in place of
`AccountsToken` (decision 10), a `DirectoryApi` in the unit tests' world and two unit
tests:

Replace

```rust
    OpenDirectRequest, OpenDirectResponse, ReadUpToRequest, ReadUpToResponse,
    RegisterDeviceRequest, RegisterDeviceResponse, RegisterUserRequest, RegisterUserResponse,
    RequestCall, SendRequest, SyncRequest, SyncResponse, accounts_server, dev_setup_server,
    messaging_server,
};
use push_proto::v1::{ContentType, SendAck};
```

with

```rust
    OpenDirectRequest, OpenDirectResponse, ReadUpToRequest, ReadUpToResponse,
    RegisterDeviceRequest, RegisterDeviceResponse, RegisterUserRequest, RegisterUserResponse,
    RequestCall, SendRequest, SyncRequest, SyncResponse, VisibleUsersRequest, VisibleUsersResponse,
    accounts_server, dev_setup_server, directory_server, messaging_server,
};
use push_proto::v1::{ContentType, SendAck};
```

Replace

```rust
}

/// Guards the `Accounts` service: a call passes only with the shared internal token in its
/// metadata, `authorization: Bearer <token>`, compared in constant time. The
/// gateway does not have the token, so it cannot register devices even though it can
/// reach message-svc. Without a configured token every call is refused.
#[derive(Clone)]
pub struct AccountsToken {
    expected: Option<Arc<str>>,
}

impl AccountsToken {
    pub fn new(expected: Option<String>) -> Self {
        Self {
            expected: expected.map(Arc::from),
        }
    }
}

impl std::fmt::Debug for AccountsToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountsToken")
            .field("expected", &self.expected.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Interceptor for AccountsToken {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let Some(expected) = &self.expected else {
            return Err(Status::unauthenticated(
                "the Accounts API is off: PUSH_ACCOUNTS_TOKEN is not set",
            ));
        };
        let presented = request
```

with

```rust
}

/// The `Directory` service: whose profiles a user may see, for auth-svc's
/// `GET /v1/users?ids=…` (see [`Messaging::visible_users`]).
pub struct DirectoryApi<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
}

impl<S, C, N> DirectoryApi<S, C, N> {
    pub fn new(messaging: Arc<Messaging<S, C, N>>) -> Self {
        Self { messaging }
    }
}

/// Most user ids one `VisibleUsers` call may ask about.
pub const MAX_VISIBLE_USERS: usize = 100;

#[tonic::async_trait]
impl<S, C, N> directory_server::Directory for DirectoryApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn visible_users(
        &self,
        request: Request<VisibleUsersRequest>,
    ) -> Result<Response<VisibleUsersResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        if request.user_ids.len() > MAX_VISIBLE_USERS {
            return Err(invalid(format!(
                "user_ids: at most {MAX_VISIBLE_USERS} ids"
            )));
        }
        let candidates = request
            .user_ids
            .iter()
            .map(|id| parse_id(id, "user_ids", UserId))
            .collect::<Result<Vec<_>, _>>()?;
        let visible = self
            .messaging
            .visible_users(user, &candidates)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(VisibleUsersResponse {
            user_ids: visible.iter().map(|user| id_bytes(user.0)).collect(),
        }))
    }
}

/// Guards an internal service: a call passes only with its shared internal token in the
/// metadata, `authorization: Bearer <token>`, compared in constant time. Each service
/// has a token of its own, given only to its caller: the worker's for `Accounts`,
/// auth-svc's for `Directory`. The gateway has neither, so it cannot register devices or
/// read relationships even though it can reach message-svc. Without a configured token
/// every call is refused.
#[derive(Clone)]
pub struct InternalToken {
    expected: Option<Arc<str>>,
    /// Why every call is refused without a configured token.
    off: &'static str,
    /// Why a call without the right token is refused.
    refused: &'static str,
}

impl InternalToken {
    /// The worker's token for `Accounts` (`PUSH_ACCOUNTS_TOKEN`).
    pub fn accounts(expected: Option<String>) -> Self {
        Self {
            expected: expected.map(Arc::from),
            off: "the Accounts API is off: PUSH_ACCOUNTS_TOKEN is not set",
            refused: "the Accounts API needs the internal token",
        }
    }

    /// auth-svc's token for `Directory` (`PUSH_DIRECTORY_TOKEN`).
    pub fn directory(expected: Option<String>) -> Self {
        Self {
            expected: expected.map(Arc::from),
            off: "the Directory API is off: PUSH_DIRECTORY_TOKEN is not set",
            refused: "the Directory API needs the internal token",
        }
    }
}

impl std::fmt::Debug for InternalToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InternalToken")
            .field("expected", &self.expected.as_ref().map(|_| "<redacted>"))
            .field("off", &self.off)
            .finish()
    }
}

impl Interceptor for InternalToken {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let Some(expected) = &self.expected else {
            return Err(Status::unauthenticated(self.off));
        };
        let presented = request
```

Replace

```rust
        match presented {
            Some(token) if same(token.as_bytes(), expected.as_bytes()) => Ok(request),
            _ => Err(Status::unauthenticated(
                "the Accounts API needs the internal token",
            )),
        }
    }
```

with

```rust
        match presented {
            Some(token) if same(token.as_bytes(), expected.as_bytes()) => Ok(request),
            _ => Err(Status::unauthenticated(self.refused)),
        }
    }
```

Replace

```rust
    use push_proto::internal::v1::accounts_server::Accounts as AccountsRpc;
    use push_proto::internal::v1::dev_setup_server::DevSetup;
    use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
    use push_proto::v1::{Ack, ErrorCode, Send};
```

with

```rust
    use push_proto::internal::v1::accounts_server::Accounts as AccountsRpc;
    use push_proto::internal::v1::dev_setup_server::DevSetup;
    use push_proto::internal::v1::directory_server::Directory as DirectoryRpc;
    use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
    use push_proto::v1::{Ack, ErrorCode, Send};
```

Replace

```rust
    type Accounts = AccountsApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Setup = DevSetupApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;

    struct World {
```

with

```rust
    type Accounts = AccountsApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Setup = DevSetupApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Directory = DirectoryApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;

    struct World {
```

Replace

```rust
        accounts: Accounts,
        setup: Setup,
        notifier: Arc<RecordingNotifier>,
    }
```

with

```rust
        accounts: Accounts,
        setup: Setup,
        directory: Directory,
        notifier: Arc<RecordingNotifier>,
    }
```

Replace

```rust
            api: MessagingApi::new(Arc::clone(&messaging)),
            accounts: AccountsApi::new(Arc::clone(&messaging)),
            setup: DevSetupApi::new(messaging),
            notifier,
        }
```

with

```rust
            api: MessagingApi::new(Arc::clone(&messaging)),
            accounts: AccountsApi::new(Arc::clone(&messaging)),
            setup: DevSetupApi::new(Arc::clone(&messaging)),
            directory: DirectoryApi::new(messaging),
            notifier,
        }
```

Replace

```rust
    fn accounts_calls_pass_only_with_the_internal_token() {
        let token = "a".repeat(32);
        let mut guard = AccountsToken::new(Some(token.clone()));
        assert!(
            guard
```

with

```rust
    fn accounts_calls_pass_only_with_the_internal_token() {
        let token = "a".repeat(32);
        let mut guard = InternalToken::accounts(Some(token.clone()));
        assert!(
            guard
```

Replace

```rust
            assert!(!status.message().contains(&token));
        }
        let mut off = AccountsToken::new(None);
        let status = off
            .call(with_authorization(&format!("Bearer {token}")))
```

with

```rust
            assert!(!status.message().contains(&token));
        }
        let mut off = InternalToken::accounts(None);
        let status = off
            .call(with_authorization(&format!("Bearer {token}")))
```

Replace

```rust
        assert!(!format!("{guard:?}").contains(&token));
    }
}
```

with

```rust
        assert!(!format!("{guard:?}").contains(&token));
    }
    #[tokio::test]
    async fn the_directory_answers_who_shares_a_contact_or_a_conversation() {
        let world = world();
        let (alice, bob, _chat) = chat(&world).await;
        let stranger = person(&world).await;
        let ask = |user: &Person, user_ids: Vec<Vec<u8>>| {
            Request::new(VisibleUsersRequest {
                user_id: id_bytes(user.user.0),
                user_ids,
            })
        };
        let candidates = vec![
            id_bytes(stranger.user.0),
            id_bytes(bob.user.0),
            id_bytes(alice.user.0),
        ];
        let visible = world
            .directory
            .visible_users(ask(&alice, candidates))
            .await
            .unwrap()
            .into_inner()
            .user_ids;
        let mut expected = vec![id_bytes(alice.user.0), id_bytes(bob.user.0)];
        expected.sort();
        assert_eq!(visible, expected);

        let status = world
            .directory
            .visible_users(ask(&alice, vec![vec![1, 2, 3]]))
            .await
            .unwrap_err();
        assert_invalid(status, "user_ids");
        let too_many = (0..=MAX_VISIBLE_USERS)
            .map(|_| id_bytes(UserId::new().0))
            .collect();
        let status = world
            .directory
            .visible_users(ask(&alice, too_many))
            .await
            .unwrap_err();
        assert_invalid(status, "user_ids");
    }

    #[test]
    fn each_internal_api_has_a_token_of_its_own() {
        let (directory_token, accounts_token) = ("d".repeat(32), "a".repeat(32));
        let mut directory = InternalToken::directory(Some(directory_token.clone()));
        let bearer = |token: &str| with_authorization(&format!("Bearer {token}"));
        assert!(directory.call(bearer(&directory_token)).is_ok());
        let status = directory.call(bearer(&accounts_token)).unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);
        assert_eq!(
            status.message(),
            "the Directory API needs the internal token"
        );
        let mut accounts = InternalToken::accounts(Some(accounts_token));
        assert!(accounts.call(bearer(&directory_token)).is_err());
        let status = InternalToken::directory(None)
            .call(bearer(&directory_token))
            .unwrap_err();
        assert_eq!(
            status.message(),
            "the Directory API is off: PUSH_DIRECTORY_TOKEN is not set"
        );
        assert!(!format!("{directory:?}").contains(&directory_token));
    }
}
```

In `crates/push-message-svc/src/config.rs` (`PUSH_DIRECTORY_TOKEN`, checked like the
Accounts token and never shown):

Replace

```rust
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_ACCOUNTS_TOKEN` | the worker's token for the `Accounts` API, 32 to 256 visible ASCII characters; without it `Accounts` refuses every call | none |
/// | `PUSH_DEV_SETUP` | `1` registers the DEV ONLY `DevSetup` service | off |
#[derive(Clone, PartialEq, Eq)]
```

with

```rust
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_ACCOUNTS_TOKEN` | the worker's token for the `Accounts` API, 32 to 256 visible ASCII characters; without it `Accounts` refuses every call | none |
/// | `PUSH_DIRECTORY_TOKEN` | auth-svc's token for the `Directory` API, 32 to 256 visible ASCII characters; without it `Directory` refuses every call | none |
/// | `PUSH_DEV_SETUP` | `1` registers the DEV ONLY `DevSetup` service | off |
#[derive(Clone, PartialEq, Eq)]
```

Replace

```rust
    pub store: StoreOptions,
    pub accounts_token: Option<String>,
    pub dev_setup: bool,
}
```

with

```rust
    pub store: StoreOptions,
    pub accounts_token: Option<String>,
    pub directory_token: Option<String>,
    pub dev_setup: bool,
}
```

Replace

```rust
                &self.accounts_token.as_ref().map(|_| "<redacted>"),
            )
            .field("dev_setup", &self.dev_setup)
            .finish()
```

with

```rust
                &self.accounts_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "directory_token",
                &self.directory_token.as_ref().map(|_| "<redacted>"),
            )
            .field("dev_setup", &self.dev_setup)
            .finish()
```

Replace

```rust
            )
        })?;
        let accounts_token = lookup("PUSH_ACCOUNTS_TOKEN").filter(|value| !value.is_empty());
        if let Some(token) = &accounts_token {
            check_token(token).map_err(|reason| redacted("PUSH_ACCOUNTS_TOKEN", reason))?;
        }
        Ok(Self {
            database_url,
```

with

```rust
            )
        })?;
        let accounts_token = token(&lookup, "PUSH_ACCOUNTS_TOKEN")?;
        let directory_token = token(&lookup, "PUSH_DIRECTORY_TOKEN")?;
        Ok(Self {
            database_url,
```

Replace

```rust
            store,
            accounts_token,
            dev_setup: dev_flag(&lookup, "PUSH_DEV_SETUP")?,
        })
```

with

```rust
            store,
            accounts_token,
            directory_token,
            dev_setup: dev_flag(&lookup, "PUSH_DEV_SETUP")?,
        })
```

Replace

```rust
}

/// A shared token fit for a header: 32 to 256 visible ASCII characters.
fn check_token(token: &str) -> Result<(), String> {
```

with

```rust
}

/// An internal token: unset or empty is `None`; otherwise it must pass [`check_token`].
fn token(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<Option<String>, ConfigError> {
    let token = lookup(name).filter(|value| !value.is_empty());
    if let Some(token) = &token {
        check_token(token).map_err(|reason| redacted(name, reason))?;
    }
    Ok(token)
}

/// A shared token fit for a header: 32 to 256 visible ASCII characters.
fn check_token(token: &str) -> Result<(), String> {
```

Replace

```rust
        assert_eq!(config.store, StoreOptions::default());
        assert_eq!(config.accounts_token, None);
        assert!(!config.dev_setup);
    }

    #[test]
    fn the_accounts_token_is_checked_and_never_shown() {
        let token = "t0ken-".repeat(6);
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_ACCOUNTS_TOKEN", &token));
        let configured = config(&variables).unwrap();
        assert_eq!(configured.accounts_token.as_deref(), Some(token.as_str()));
        assert!(!format!("{configured:?}").contains(&token));
        for bad in ["too-short", "with a space but long enough to pass 32"] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_ACCOUNTS_TOKEN", bad));
            let message = config(&variables).unwrap_err().to_string();
            assert!(message.contains("PUSH_ACCOUNTS_TOKEN"), "{message}");
            assert!(!message.contains(bad), "{message}");
        }
    }
```

with

```rust
        assert_eq!(config.store, StoreOptions::default());
        assert_eq!(config.accounts_token, None);
        assert_eq!(config.directory_token, None);
        assert!(!config.dev_setup);
    }

    #[test]
    fn the_internal_tokens_are_checked_and_never_shown() {
        let (accounts, directory) = ("t0ken-".repeat(6), "d1rectory-".repeat(4));
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_ACCOUNTS_TOKEN", &accounts));
        variables.push(("PUSH_DIRECTORY_TOKEN", &directory));
        let configured = config(&variables).unwrap();
        assert_eq!(
            configured.accounts_token.as_deref(),
            Some(accounts.as_str())
        );
        assert_eq!(
            configured.directory_token.as_deref(),
            Some(directory.as_str())
        );
        let shown = format!("{configured:?}");
        assert!(!shown.contains(&accounts) && !shown.contains(&directory));
        for name in ["PUSH_ACCOUNTS_TOKEN", "PUSH_DIRECTORY_TOKEN"] {
            for bad in ["too-short", "with a space but long enough to pass 32"] {
                let mut variables = REQUIRED.to_vec();
                variables.push((name, bad));
                let message = config(&variables).unwrap_err().to_string();
                assert!(message.contains(name), "{message}");
                assert!(!message.contains(bad), "{message}");
            }
        }
    }
```

Serve it, always, behind its token; without one it warns at start-up. In
`crates/push-message-svc/src/server.rs`:

Replace

```rust
use push_proto::internal::v1::accounts_server::AccountsServer;
use push_proto::internal::v1::dev_setup_server::DevSetupServer;
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_store::{PgStore, migrate_database};
```

with

```rust
use push_proto::internal::v1::accounts_server::AccountsServer;
use push_proto::internal::v1::dev_setup_server::DevSetupServer;
use push_proto::internal::v1::directory_server::DirectoryServer;
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_store::{PgStore, migrate_database};
```

Replace

```rust
use crate::config::Config;
use crate::grpc::{AccountsApi, AccountsToken, DevSetupApi, MessagingApi};
use crate::notify::RedisNotifier;
```

with

```rust
use crate::config::Config;
use crate::grpc::{AccountsApi, DevSetupApi, DirectoryApi, InternalToken, MessagingApi};
use crate::notify::RedisNotifier;
```

Replace

```rust
        );
    }
    let Some(store) = retry("postgres", &shutdown, || async {
        // Services migrate with `push_store::migrate_database(url)` on a dedicated
```

with

```rust
        );
    }
    if config.directory_token.is_none() {
        tracing::warn!(
            "PUSH_DIRECTORY_TOKEN is not set: the Directory API refuses every call, so \
             auth-svc cannot show profiles"
        );
    }
    let Some(store) = retry("postgres", &shutdown, || async {
        // Services migrate with `push_store::migrate_database(url)` on a dedicated
```

Replace

```rust
        ))))
        .add_service(AccountsServer::with_interceptor(
            AccountsApi::new(messaging),
            AccountsToken::new(config.accounts_token.clone()),
        ))
        .add_optional_service(dev_setup)
```

with

```rust
        ))))
        .add_service(AccountsServer::with_interceptor(
            AccountsApi::new(Arc::clone(&messaging)),
            InternalToken::accounts(config.accounts_token.clone()),
        ))
        .add_service(DirectoryServer::with_interceptor(
            DirectoryApi::new(messaging),
            InternalToken::directory(config.directory_token.clone()),
        ))
        .add_optional_service(dev_setup)
```

In `crates/push-message-svc/src/lib.rs`:

Replace

```rust
pub use config::{Config, ConfigError};
pub use grpc::{AccountsApi, AccountsToken, DevSetupApi, MessagingApi};
pub use notify::RedisNotifier;
pub use server::{init_logging, serve, terminated};
```

with

```rust
pub use config::{Config, ConfigError};
pub use grpc::{
    AccountsApi, DevSetupApi, DirectoryApi, InternalToken, MAX_VISIBLE_USERS, MessagingApi,
};
pub use notify::RedisNotifier;
pub use server::{init_logging, serve, terminated};
```

The worker's tests build message-svc's Accounts guard. In
`crates/push-worker/tests/relay.rs` and `crates/push-worker/tests/server.rs`, replace
`AccountsToken` with `InternalToken` in the `use push_message_svc::…` line, and
`AccountsToken::new(Some(` with `InternalToken::accounts(Some(`:

Replace

```rust
use push_domain::{InMemoryStore, Messaging, RecordingNotifier, SystemClock};
use push_email::{LogSender, Templates};
use push_message_svc::{AccountsApi, AccountsToken, MessagingApi};
use push_proto::id_bytes;
use push_proto::internal::v1::IdentifyRequest;
```

with

```rust
use push_domain::{InMemoryStore, Messaging, RecordingNotifier, SystemClock};
use push_email::{LogSender, Templates};
use push_message_svc::{AccountsApi, InternalToken, MessagingApi};
use push_proto::id_bytes;
use push_proto::internal::v1::IdentifyRequest;
```

Replace

```rust
        .add_service(AccountsServer::with_interceptor(
            AccountsApi::new(messaging),
            AccountsToken::new(Some(ACCOUNTS_TOKEN.to_owned())),
        ))
        .serve_with_incoming(TcpListenerStream::new(listener));
```

with

```rust
        .add_service(AccountsServer::with_interceptor(
            AccountsApi::new(messaging),
            InternalToken::accounts(Some(ACCOUNTS_TOKEN.to_owned())),
        ))
        .serve_with_incoming(TcpListenerStream::new(listener));
```

Replace

```rust
use push_domain::{InMemoryStore, Messaging, RecordingNotifier, SystemClock};
use push_email::ResendWebhooks;
use push_message_svc::{AccountsApi, AccountsToken, MessagingApi};
use push_proto::id_bytes;
use push_proto::internal::v1::IdentifyRequest;
```

with

```rust
use push_domain::{InMemoryStore, Messaging, RecordingNotifier, SystemClock};
use push_email::ResendWebhooks;
use push_message_svc::{AccountsApi, InternalToken, MessagingApi};
use push_proto::id_bytes;
use push_proto::internal::v1::IdentifyRequest;
```

Replace

```rust
            .add_service(AccountsServer::with_interceptor(
                AccountsApi::new(messaging),
                AccountsToken::new(Some(ACCOUNTS_TOKEN.to_owned())),
            ))
            .serve_with_incoming(TcpListenerStream::new(listener)),
```

with

```rust
            .add_service(AccountsServer::with_interceptor(
                AccountsApi::new(messaging),
                InternalToken::accounts(Some(ACCOUNTS_TOKEN.to_owned())),
            ))
            .serve_with_incoming(TcpListenerStream::new(listener)),
```

The smoke test's local stack builds message-svc's `Config`; it gets no Directory token
yet (Plan 7B wires auth-svc to it). In `crates/push-smoke/tests/local.rs`:

Replace

```rust
            store: StoreOptions::default(),
            accounts_token: Some(accounts_token.clone()),
            dev_setup: true,
        },
```

with

```rust
            store: StoreOptions::default(),
            accounts_token: Some(accounts_token.clone()),
            directory_token: None,
            dev_setup: true,
        },
```

Over real Postgres: whose profiles Carol may see, refused without auth-svc's token. In
`crates/push-message-svc/tests/service.rs`:

Replace

```rust
use push_proto::internal::v1::accounts_client::AccountsClient;
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AccountDevice, AckRequest, EditRequest, IdentifyRequest, Kick, KickReason, MakeContactsRequest,
    Notification, OpenDirectRequest, ReadUpToRequest, RegisterDeviceRequest, RegisterUserRequest,
    RequestCall, SendRequest, SyncRequest, notification::Kind,
};
use push_proto::v1::{
```

with

```rust
use push_proto::internal::v1::accounts_client::AccountsClient;
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::directory_client::DirectoryClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AccountDevice, AckRequest, EditRequest, IdentifyRequest, Kick, KickReason, MakeContactsRequest,
    Notification, OpenDirectRequest, ReadUpToRequest, RegisterDeviceRequest, RegisterUserRequest,
    RequestCall, SendRequest, SyncRequest, VisibleUsersRequest, notification::Kind,
};
use push_proto::v1::{
```

Replace

```rust
/// The worker's token for the `Accounts` API.
const ACCOUNTS_TOKEN: &str = "accounts-token-for-the-service-tests";

struct Running {
```

with

```rust
/// The worker's token for the `Accounts` API.
const ACCOUNTS_TOKEN: &str = "accounts-token-for-the-service-tests";
/// auth-svc's token for the `Directory` API.
const DIRECTORY_TOKEN: &str = "directory-token-for-the-service-tests";

struct Running {
```

Replace

```rust
        store: StoreOptions::default(),
        accounts_token: Some(ACCOUNTS_TOKEN.to_owned()),
        dev_setup,
    };
```

with

```rust
        store: StoreOptions::default(),
        accounts_token: Some(ACCOUNTS_TOKEN.to_owned()),
        directory_token: Some(DIRECTORY_TOKEN.to_owned()),
        dev_setup,
    };
```

Replace

```rust
        store: StoreOptions::default(),
        accounts_token: None,
        dev_setup: false,
    };
```

with

```rust
        store: StoreOptions::default(),
        accounts_token: None,
        directory_token: None,
        dev_setup: false,
    };
```

Replace

```rust
#[tokio::test]
async fn the_client_requests_run_on_postgres() {
    let running = start(false).await;
    let url = format!("http://{}", running.grpc);
```

with

```rust
#[tokio::test]
async fn the_client_requests_and_the_directory_run_on_postgres() {
    let running = start(false).await;
    let url = format!("http://{}", running.grpc);
```

Replace

```rust
    );

    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
```

with

```rust
    );

    // auth-svc asks whose profiles Carol may see: her contact Alice and Bob, from the
    // group, but not a stranger.
    let channel = Channel::from_shared(format!("http://{}", running.grpc))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let ask = VisibleUsersRequest {
        user_id: id_bytes(carol),
        user_ids: vec![id_bytes(Uuid::now_v7()), id_bytes(bob), id_bytes(alice)],
    };
    let status = DirectoryClient::new(channel.clone())
        .visible_users(ask.clone())
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    let bearer: MetadataValue<_> = format!("Bearer {DIRECTORY_TOKEN}").parse().unwrap();
    let mut directory =
        DirectoryClient::with_interceptor(channel, move |mut request: Request<()>| {
            request
                .metadata_mut()
                .insert("authorization", bearer.clone());
            Ok(request)
        });
    let visible = directory.visible_users(ask).await.unwrap().into_inner();
    let mut expected = vec![id_bytes(alice), id_bytes(bob)];
    expected.sort();
    assert_eq!(visible.user_ids, expected);

    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
```

```powershell
cargo test -p push-message-svc -p push-worker -p push-smoke
```

Expected: everything passes, among them
`the_directory_answers_who_shares_a_contact_or_a_conversation`,
`each_internal_api_has_a_token_of_its_own`, `the_internal_tokens_are_checked_and_never_shown`
and `the_client_requests_and_the_directory_run_on_postgres`.

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "feat(message-svc): the Directory API behind auth-svc's own token"
```

Expected: 804 tests pass, no warnings, no diff.

---

### Task 8: auth-svc's `GET /v1/users?ids=…`

**Files:**
- Create: `crates/push-auth/src/directory.rs`
- Modify: `crates/push-auth/Cargo.toml`, `src/rules.rs`, `src/profiles.rs`, `src/api.rs`,
  `src/config.rs`, `src/server.rs`, `src/lib.rs`, `crates/push-auth/tests/api.rs`,
  `Cargo.lock`

- [ ] **Step 1: The dependencies and the client**

In `crates/push-auth/Cargo.toml`:

Replace

```toml
hmac.workspace = true
jsonwebtoken.workspace = true
push-store.workspace = true
rand.workspace = true
```

with

```toml
hmac.workspace = true
jsonwebtoken.workspace = true
push-proto.workspace = true
push-store.workspace = true
rand.workspace = true
```

Replace

```toml
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
tokio-util.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
```

with

```toml
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
tokio-util.workspace = true
tonic.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
```

Replace

```toml
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["io-util"] }
tower.workspace = true
```

with

```toml
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["io-util"] }
tokio-stream.workspace = true
tower.workspace = true
```

The client of message-svc's `Directory`, with its own token, and a stand-in for an
auth-svc without message-svc; the unit test runs a small `Directory` server on
`127.0.0.1` that checks the token. Create `crates/push-auth/src/directory.rs`:

```rust
//! Whose profiles a caller may see (spec §5.5). Only message-svc knows who shares a
//! contact or a conversation, so auth-svc asks its internal `Directory` API, presenting
//! auth-svc's own internal token. Each service keeps its own database.

use std::time::Duration;

use async_trait::async_trait;
use push_proto::internal::v1::VisibleUsersRequest;
use push_proto::internal::v1::directory_client::DirectoryClient;
use push_proto::{id, id_bytes};
use tonic::codegen::InterceptedService;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};
use uuid::Uuid;

use crate::error::AuthError;

/// How long connecting to message-svc may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long one call may take; a profile request waits for it.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Answers whose profiles a user may see.
#[async_trait]
pub trait Directory: Send + Sync + std::fmt::Debug {
    /// The users among `candidates` whose profiles `caller` may see. `Unavailable` when
    /// there is no answer: no profile is shown without one.
    async fn visible_users(
        &self,
        caller: Uuid,
        candidates: &[Uuid],
    ) -> Result<Vec<Uuid>, AuthError>;
}

/// message-svc's `Directory` API over gRPC.
#[derive(Debug, Clone)]
pub struct GrpcDirectory {
    client: DirectoryClient<InterceptedService<Channel, Bearer>>,
}

impl GrpcDirectory {
    /// A client for message-svc at `url` (`http://host:50051`) that presents `token`,
    /// auth-svc's internal token for this API. It connects on first use and reconnects by
    /// itself, so auth-svc starts without waiting for message-svc.
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        let mut bearer: MetadataValue<Ascii> = format!("Bearer {token}")
            .parse()
            .map_err(|_| "the Directory token cannot be sent as metadata".to_owned())?;
        // Never indexed by HTTP/2 or shown in header dumps.
        bearer.set_sensitive(true);
        let channel = Endpoint::from_shared(url.to_owned())
            .map_err(|_| "not a valid message-svc URL".to_owned())?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(CALL_TIMEOUT)
            .connect_lazy();
        Ok(Self {
            client: DirectoryClient::with_interceptor(channel, Bearer(bearer)),
        })
    }
}

#[async_trait]
impl Directory for GrpcDirectory {
    async fn visible_users(
        &self,
        caller: Uuid,
        candidates: &[Uuid],
    ) -> Result<Vec<Uuid>, AuthError> {
        let request = VisibleUsersRequest {
            user_id: id_bytes(caller),
            user_ids: candidates.iter().map(|user| id_bytes(*user)).collect(),
        };
        let unavailable = |code| {
            tracing::warn!(?code, "message-svc's Directory API did not answer");
            AuthError::Unavailable
        };
        let response = self
            .client
            .clone()
            .visible_users(request)
            .await
            .map_err(|status| unavailable(status.code()))?;
        response
            .into_inner()
            .user_ids
            .iter()
            .map(|user| id(user).map_err(|_| unavailable(tonic::Code::Internal)))
            .collect()
    }
}

/// Without message-svc: every answer is `Unavailable`, so no profile is shown. For an
/// auth-svc started without `PUSH_MESSAGE_SVC_URL`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDirectory;

#[async_trait]
impl Directory for NoDirectory {
    async fn visible_users(
        &self,
        _caller: Uuid,
        _candidates: &[Uuid],
    ) -> Result<Vec<Uuid>, AuthError> {
        Err(AuthError::Unavailable)
    }
}

/// Adds auth-svc's internal token, as `authorization: Bearer <token>`, to every call.
#[derive(Clone)]
struct Bearer(MetadataValue<Ascii>);

impl std::fmt::Debug for Bearer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Bearer(<redacted>)")
    }
}

impl Interceptor for Bearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.0.clone());
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use push_proto::internal::v1::VisibleUsersResponse;
    use push_proto::internal::v1::directory_server::{self, DirectoryServer};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::Response;

    use super::*;

    const TOKEN: &str = "directory-token-for-the-auth-tests";

    /// A message-svc that lets everyone see the first candidate, and checks the token.
    struct FirstOnly;

    #[tonic::async_trait]
    impl directory_server::Directory for FirstOnly {
        async fn visible_users(
            &self,
            request: Request<VisibleUsersRequest>,
        ) -> Result<Response<VisibleUsersResponse>, Status> {
            let presented = request.metadata().get("authorization").cloned();
            if presented.as_ref().and_then(|value| value.to_str().ok())
                != Some(format!("Bearer {TOKEN}").as_str())
            {
                return Err(Status::unauthenticated("wrong token"));
            }
            let request = request.into_inner();
            Ok(Response::new(VisibleUsersResponse {
                user_ids: request.user_ids.into_iter().take(1).collect(),
            }))
        }
    }

    async fn message_svc() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(DirectoryServer::new(FirstOnly))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        url
    }

    #[tokio::test]
    async fn the_directory_is_asked_with_the_token_and_refusals_are_unavailable() {
        let url = message_svc().await;
        let (caller, first, second) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let directory = GrpcDirectory::new(&url, TOKEN).unwrap();
        assert_eq!(
            directory.visible_users(caller, &[first, second]).await,
            Ok(vec![first])
        );
        let wrong = GrpcDirectory::new(&url, "another-token-of-the-right-length").unwrap();
        assert_eq!(
            wrong.visible_users(caller, &[first]).await,
            Err(AuthError::Unavailable)
        );
        let unreachable = GrpcDirectory::new("http://127.0.0.1:1", TOKEN).unwrap();
        assert_eq!(
            unreachable.visible_users(caller, &[first]).await,
            Err(AuthError::Unavailable)
        );
        assert!(!format!("{directory:?}").contains(TOKEN));
        assert_eq!(
            NoDirectory.visible_users(caller, &[first]).await,
            Err(AuthError::Unavailable)
        );
    }
}
```

- [ ] **Step 2: The ids, the profiles and the endpoint, tests first**

In `crates/push-auth/src/rules.rs`:

Replace

```rust
//! What the API accepts: emails, handles, display names, passwords and devices.
//!
//! Each function trims and normalises its input and returns what is stored, or the
```

with

```rust
//! What the API accepts: emails, handles, display names, passwords, devices and user ids.
//!
//! Each function trims and normalises its input and returns what is stored, or the
```

Replace

```rust
use serde::{Deserialize, Serialize};

/// An input the API refuses: which field and why.
```

with

```rust
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An input the API refuses: which field and why.
```

Replace

```rust
/// Most characters of a display name or device name.
pub const MAX_NAME_CHARS: usize = 64;

/// An email address: trimmed, with the domain in lower case. Matching ignores case
```

with

```rust
/// Most characters of a display name or device name.
pub const MAX_NAME_CHARS: usize = 64;
/// Most user ids one `GET /v1/users?ids=…` may ask for.
pub const MAX_PROFILE_IDS: usize = 100;

/// An email address: trimmed, with the domain in lower case. Matching ignores case
```

Replace

```rust
}

/// What kind of device a client runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
```

with

```rust
}

/// The ids of `GET /v1/users?ids=…`: 1 to 100 UUIDs, separated by commas, sorted and
/// without duplicates.
pub fn user_ids(raw: &str) -> Result<Vec<Uuid>, Invalid> {
    let mut ids = raw
        .split(',')
        .map(|id| Uuid::parse_str(id.trim()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("ids", "use user ids separated by commas"))?;
    ids.sort();
    ids.dedup();
    if ids.len() > MAX_PROFILE_IDS {
        return Err(invalid("ids", "ask for at most 100 users at once"));
    }
    Ok(ids)
}

/// What kind of device a client runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
```

Replace

```rust
    use super::*;

    #[test]
    fn emails_are_trimmed_and_their_domain_lower_cased() {
```

with

```rust
    use super::*;

    #[test]
    fn user_ids_are_a_comma_separated_list_of_at_most_100() {
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(user_ids(&format!("{b}, {a},{b}")), Ok(vec![a, b]));
        for bad in ["", "not-a-uuid", &format!("{a},,{b}")] {
            assert_eq!(user_ids(bad).unwrap_err().field, "ids", "{bad:?}");
        }
        let many: Vec<String> = (0..=MAX_PROFILE_IDS)
            .map(|_| Uuid::now_v7().to_string())
            .collect();
        assert_eq!(
            user_ids(&many.join(",")).unwrap_err().reason,
            "ask for at most 100 users at once"
        );
        assert_eq!(user_ids(&many[1..].join(",")).map(|ids| ids.len()), Ok(100));
    }

    #[test]
    fn emails_are_trimmed_and_their_domain_lower_cased() {
```

In `crates/push-auth/tests/api.rs`, a `Directory` stand-in the tests can relate users in
and switch off, and the endpoint's test:

Replace

```rust
//! through the router as the ingress would send them, with the client IP in `x-real-ip`.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
```

with

```rust
//! through the router as the ingress would send them, with the client IP in `x-real-ip`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use axum::Router;
use axum::body::Body;
```

Replace

```rust
use push_auth::password::Passwords;
use push_auth::tokens::AccessTokens;
use push_auth::{Auth, Settings, migrate_database};
use serde_json::{Value, json};
use sqlx::PgPool;
```

with

```rust
use push_auth::password::Passwords;
use push_auth::tokens::AccessTokens;
use push_auth::{Auth, AuthError, Directory, Settings, migrate_database};
use serde_json::{Value, json};
use sqlx::PgPool;
```

Replace

```rust
    router: Router,
    pool: PgPool,
}
```

with

```rust
    router: Router,
    pool: PgPool,
    directory: Arc<Relations>,
}

/// message-svc's `Directory` in miniature: who is related to whom, and whether it answers.
#[derive(Debug, Default)]
struct Relations {
    pairs: Mutex<Vec<(Uuid, Uuid)>>,
    down: AtomicBool,
}

impl Relations {
    /// `a` and `b` share a contact or a conversation.
    fn relate(&self, a: Uuid, b: Uuid) {
        self.pairs.lock().unwrap().extend([(a, b), (b, a)]);
    }
}

#[async_trait]
impl Directory for Relations {
    async fn visible_users(
        &self,
        caller: Uuid,
        candidates: &[Uuid],
    ) -> Result<Vec<Uuid>, AuthError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(AuthError::Unavailable);
        }
        let pairs = self.pairs.lock().unwrap();
        Ok(candidates
            .iter()
            .copied()
            .filter(|user| *user == caller || pairs.contains(&(caller, *user)))
            .collect())
    }
}
```

Replace

```rust
        },
    );
    let router = router(Api {
        auth,
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
    });
    World {
```

with

```rust
        },
    );
    let directory = Arc::new(Relations::default());
    let router = router(Api {
        auth,
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
        directory: Arc::clone(&directory) as Arc<dyn Directory>,
    });
    World {
```

Replace

```rust
        router,
        pool,
    }
}
```

with

```rust
        router,
        pool,
        directory,
    }
}
```

Replace

```rust
    assert_eq!(elsewhere.status, StatusCode::OK, "{}", elsewhere.text);
}
```

with

```rust
    assert_eq!(elsewhere.status, StatusCode::OK, "{}", elsewhere.text);
}

#[tokio::test]
async fn profiles_are_shown_only_to_related_users() {
    let world = world().await;
    let alice = with_device(&world, "alice").await;
    let bob = with_device(&world, "bob").await;
    let carol = with_device(&world, "carol").await;
    let id = |tokens: &Value| Uuid::parse_str(tokens["user_id"].as_str().unwrap()).unwrap();
    world.directory.relate(id(&alice), id(&bob));
    let access = alice["access_token"].as_str().unwrap();
    let ask = |ids: &[Uuid]| {
        let ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
        format!("/v1/users?ids={}", ids.join(","))
    };

    // Carol is not related to Alice, and an unknown id is left out like her.
    let unknown = Uuid::now_v7();
    let uri = ask(&[id(&carol), id(&bob), unknown, id(&alice)]);
    let answer = call(&world, Method::GET, &uri, Some(access), None).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text);
    assert_eq!(answer.headers["cache-control"], "no-store");
    let mut expected = vec![
        json!({"user_id": alice["user_id"], "handle": "alice", "display_name": "alice"}),
        json!({"user_id": bob["user_id"], "handle": "bob", "display_name": "bob"}),
    ];
    expected.sort_by_key(|user| user["user_id"].as_str().unwrap().to_owned());
    assert_eq!(answer.json(), json!({ "users": expected }));

    let answer = call(&world, Method::GET, "/v1/users?ids=x", Some(access), None).await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert_eq!(answer.json()["error"]["code"], "invalid");
    let answer = call(&world, Method::GET, &ask(&[id(&bob)]), None, None).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);

    // Without message-svc's answer no profile is shown.
    world.directory.down.store(true, Ordering::SeqCst);
    let answer = call(&world, Method::GET, &ask(&[id(&bob)]), Some(access), None).await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.json()["error"]["retryable"], true);
}
```

```powershell
cargo test -p push-auth
```

Expected: cargo records the new dependencies in `Cargo.lock`; compile errors in
`tests/api.rs` — `push_auth` exports no `Directory`, and `Api` has no field `directory`.

In `crates/push-auth/src/lib.rs`:

Replace

```rust
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.

pub mod api;
```

with

```rust
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change. Whose profiles a caller may see it asks message-svc, which
//! knows the contacts and conversations (spec §5.5).

pub mod api;
```

Replace

```rust
mod db;
mod devices;
mod error;
pub mod limits;
```

with

```rust
mod db;
mod devices;
mod directory;
mod error;
pub mod limits;
```

Replace

```rust
pub use db::{MIGRATOR, grant_worker_access, is_role_name, migrate_database};
pub use devices::{Caller, DeviceInfo, MAX_ACTIVE_DEVICES, REFRESH_GRACE, Tokens};
pub use error::AuthError;
pub use profiles::{OwnProfile, PublicProfile};
```

with

```rust
pub use db::{MIGRATOR, grant_worker_access, is_role_name, migrate_database};
pub use devices::{Caller, DeviceInfo, MAX_ACTIVE_DEVICES, REFRESH_GRACE, Tokens};
pub use directory::{Directory, GrpcDirectory, NoDirectory};
pub use error::AuthError;
pub use profiles::{OwnProfile, PublicProfile};
```

In `crates/push-auth/src/profiles.rs`:

Replace

```rust
//! Profiles and handles (spec §5.5): the caller's own profile, and finding a user by
//! @handle. Emails are never shown to anyone but their owner.

use sqlx::Row;
```

with

```rust
//! Profiles and handles (spec §5.5): the caller's own profile, finding a user by @handle,
//! and the profiles of users the caller shares a contact or a conversation with. Emails
//! are never shown to anyone but their owner.

use sqlx::Row;
```

Replace

```rust
        })
    }
}
```

with

```rust
        })
    }

    /// The profiles of the verified users among `users`, sorted by user id. The caller
    /// checks first that they may be shown (`GET /v1/users?ids=…` asks message-svc).
    pub async fn profiles(&self, users: &[Uuid]) -> Result<Vec<PublicProfile>, AuthError> {
        let rows = sqlx::query(
            "SELECT user_id, handle::text, display_name FROM auth.user_account \
             WHERE user_id = ANY($1) AND email_verified_at IS NOT NULL ORDER BY user_id",
        )
        .bind(users)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| PublicProfile {
                user_id: row.get(0),
                handle: row.get(1),
                display_name: row.get(2),
            })
            .collect())
    }
}
```

In `crates/push-auth/src/api.rs`:

Replace

```rust
//! The public HTTP API (spec §2, §5): `/v1/auth/*`, `/v1/devices/*` and `/v1/users/*`,
//! JSON in and out, plus the two small pages that email links open.
//!
```

with

```rust
//! The public HTTP API (spec §2, §5): `/v1/auth/*`, `/v1/devices/*` and `/v1/users*`,
//! JSON in and out, plus the two small pages that email links open.
//!
```

Replace

```rust
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Path, Query, Request, State};
```

with

```rust
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Path, Query, Request, State};
```

Replace

```rust
use crate::devices::{Caller, Tokens};
use crate::error::AuthError;
use crate::limits::{Action, Limits};
use crate::rules::Invalid;
use crate::service::{Auth, Signup};
```

with

```rust
use crate::devices::{Caller, Tokens};
use crate::directory::Directory;
use crate::error::AuthError;
use crate::limits::{Action, Limits};
use crate::rules::{self, Invalid};
use crate::service::{Auth, Signup};
```

Replace

```rust
    /// always overwrites (or, for `X-Forwarded-For`, appends to): its last entry is used.
    pub client_ip_header: Option<HeaderName>,
}
```

with

```rust
    /// always overwrites (or, for `X-Forwarded-For`, appends to): its last entry is used.
    pub client_ip_header: Option<HeaderName>,
    /// Who may see whose profile: message-svc's `Directory` API.
    pub directory: Arc<dyn Directory>,
}
```

Replace

```rust
        .route("/v1/devices", post(register_device).get(list_devices))
        .route("/v1/devices/{device_id}", delete(revoke_device))
        .route("/v1/users/me", get(own_profile).patch(set_display_name))
        .route("/v1/users/lookup", get(lookup))
```

with

```rust
        .route("/v1/devices", post(register_device).get(list_devices))
        .route("/v1/devices/{device_id}", delete(revoke_device))
        .route("/v1/users", get(profiles))
        .route("/v1/users/me", get(own_profile).patch(set_display_name))
        .route("/v1/users/lookup", get(lookup))
```

Replace

```rust
}

/// A small HTML page with the headers every link page needs.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
```

with

```rust
}

#[derive(Deserialize)]
struct IdsQuery {
    #[serde(default)]
    ids: String,
}

/// `GET /v1/users?ids=…`: the profiles of the listed users that the caller shares a
/// contact or a conversation with (message-svc decides), and the caller's own. Others are
/// left out, as are unknown ids: the answer never tells which ids exist.
async fn profiles(
    State(api): State<Api>,
    headers: HeaderMap,
    Query(query): Query<IdsQuery>,
) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let ids = rules::user_ids(&query.ids)?;
    let visible = api.directory.visible_users(caller.user_id, &ids).await?;
    let profiles = api.auth.profiles(&visible).await?;
    let users: Vec<Value> = profiles
        .iter()
        .map(|profile| {
            json!({
                "user_id": profile.user_id,
                "handle": profile.handle,
                "display_name": profile.display_name,
            })
        })
        .collect();
    Ok(Json(json!({ "users": users })).into_response())
}

/// A small HTML page with the headers every link page needs.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
```

- [ ] **Step 3: The configuration and start-up**

In `crates/push-auth/src/config.rs` (decision 7):

Replace

```rust
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_AUTH_WORKER_ROLE` | role the worker connects as: granted the outbox and the cleanups at start-up | none |
///
/// `http://` public URLs and `ws://` audiences are accepted only for local hosts
```

with

```rust
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_AUTH_WORKER_ROLE` | role the worker connects as: granted the outbox and the cleanups at start-up | none |
/// | `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc, `http://host:50051` (plain `http://` only), asked whose profiles a caller may see; without it `GET /v1/users` answers 503 | none |
/// | `PUSH_DIRECTORY_TOKEN` | the token message-svc requires for its `Directory` API, 32 to 256 visible ASCII characters; required with `PUSH_MESSAGE_SVC_URL` | none |
///
/// `http://` public URLs and `ws://` audiences are accepted only for local hosts
```

Replace

```rust
    pub store: StoreOptions,
    pub worker_role: Option<String>,
}
```

with

```rust
    pub store: StoreOptions,
    pub worker_role: Option<String>,
    pub message_svc_url: Option<String>,
    pub directory_token: Option<String>,
}
```

Replace

```rust
            .field("store", &self.store)
            .field("worker_role", &self.worker_role)
            .finish()
    }
```

with

```rust
            .field("store", &self.store)
            .field("worker_role", &self.worker_role)
            .field("message_svc_url", &self.message_svc_url)
            .field(
                "directory_token",
                &self.directory_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
```

Replace

```rust
            }
        };
        Ok(Self {
            database_url,
```

with

```rust
            }
        };
        let message_svc_url = lookup("PUSH_MESSAGE_SVC_URL").filter(|v| !v.is_empty());
        if let Some(url) = &message_svc_url {
            if !url.starts_with("http://") {
                return Err(ConfigError::Invalid {
                    name: "PUSH_MESSAGE_SVC_URL",
                    value: url.clone(),
                    reason: "must start with http:// (auth-svc speaks plain gRPC to \
                             message-svc inside the cluster and is built without TLS)"
                        .to_owned(),
                });
            }
        }
        let directory_token = lookup("PUSH_DIRECTORY_TOKEN").filter(|v| !v.is_empty());
        match (&message_svc_url, &directory_token) {
            (Some(_), None) => return Err(ConfigError::Missing("PUSH_DIRECTORY_TOKEN")),
            (None, Some(_)) => return Err(ConfigError::Missing("PUSH_MESSAGE_SVC_URL")),
            _ => {}
        }
        if let Some(token) = &directory_token {
            if !((32..=256).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_graphic())) {
                return Err(redacted(
                    "PUSH_DIRECTORY_TOKEN",
                    "use 32 to 256 visible ASCII characters".to_owned(),
                ));
            }
        }
        Ok(Self {
            database_url,
```

Replace

```rust
            store,
            worker_role,
        })
    }
```

with

```rust
            store,
            worker_role,
            message_svc_url,
            directory_token,
        })
    }
```

Replace

```rust
        assert_eq!(config.store, StoreOptions::default());
        assert_eq!(config.worker_role, None);
    }
```

with

```rust
        assert_eq!(config.store, StoreOptions::default());
        assert_eq!(config.worker_role, None);
        assert_eq!(config.message_svc_url, None);
        assert_eq!(config.directory_token, None);
    }
```

Replace

```rust
    }

    #[test]
    fn zero_hash_concurrency_is_refused() {
```

with

```rust
    }

    #[test]
    fn message_svc_comes_with_its_directory_token_and_plain_http() {
        let token = "d1rectory-d1rectory-d1rectory-d1rectory-";
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051"));
        variables.push(("PUSH_DIRECTORY_TOKEN", token));
        let configured = config(&variables).unwrap();
        assert_eq!(
            configured.message_svc_url.as_deref(),
            Some("http://message-svc:50051")
        );
        assert_eq!(configured.directory_token.as_deref(), Some(token));
        assert!(!format!("{configured:?}").contains(token));

        assert_eq!(
            config(&with("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051")),
            Err(ConfigError::Missing("PUSH_DIRECTORY_TOKEN"))
        );
        assert_eq!(
            config(&with("PUSH_DIRECTORY_TOKEN", token)),
            Err(ConfigError::Missing("PUSH_MESSAGE_SVC_URL"))
        );
        let mut tls = variables.clone();
        tls[REQUIRED.len()] = ("PUSH_MESSAGE_SVC_URL", "https://message-svc:50051");
        assert!(matches!(
            config(&tls),
            Err(ConfigError::Invalid {
                name: "PUSH_MESSAGE_SVC_URL",
                ..
            })
        ));
        let mut short = variables;
        short[REQUIRED.len() + 1] = ("PUSH_DIRECTORY_TOKEN", "too-short");
        let message = config(&short).unwrap_err().to_string();
        assert!(message.contains("PUSH_DIRECTORY_TOKEN"), "{message}");
        assert!(!message.contains("too-short"), "{message}");
    }

    #[test]
    fn zero_hash_concurrency_is_refused() {
```

In `crates/push-auth/src/server.rs`:

Replace

```rust
use crate::config::Config;
use crate::db::{grant_worker_access, migrate_database};
use crate::limits::{Limits, RedisCounter};
use crate::links::LinkSigner;
```

with

```rust
use crate::config::Config;
use crate::db::{grant_worker_access, migrate_database};
use crate::directory::{Directory, GrpcDirectory, NoDirectory};
use crate::limits::{Limits, RedisCounter};
use crate::links::LinkSigner;
```

Replace

```rust
        .map_err(anyhow::Error::msg)?;
    let links = LinkSigner::new(config.link_key.clone()).map_err(anyhow::Error::msg)?;
    let ready = Health {
        pool: Arc::new(OnceLock::new()),
```

with

```rust
        .map_err(anyhow::Error::msg)?;
    let links = LinkSigner::new(config.link_key.clone()).map_err(anyhow::Error::msg)?;
    let directory: Arc<dyn Directory> = match (&config.message_svc_url, &config.directory_token) {
        (Some(url), Some(token)) => {
            Arc::new(GrpcDirectory::new(url, token).map_err(anyhow::Error::msg)?)
        }
        _ => {
            tracing::warn!(
                "PUSH_MESSAGE_SVC_URL is not set: GET /v1/users answers 503, since only \
                 message-svc knows whose profiles a user may see"
            );
            Arc::new(NoDirectory)
        }
    };
    let ready = Health {
        pool: Arc::new(OnceLock::new()),
```

Replace

```rust
        limits: Limits::new(Arc::new(counter)),
        client_ip_header: config.client_ip_header.clone(),
    });
    tracing::info!(addr = %config.http_addr, "serving HTTP");
```

with

```rust
        limits: Limits::new(Arc::new(counter)),
        client_ip_header: config.client_ip_header.clone(),
        directory,
    });
    tracing::info!(addr = %config.http_addr, "serving HTTP");
```

```powershell
cargo test -p push-auth
```

Expected: everything passes, among them
`user_ids_are_a_comma_separated_list_of_at_most_100`,
`the_directory_is_asked_with_the_token_and_refusals_are_unavailable`,
`message_svc_comes_with_its_directory_token_and_plain_http` and
`profiles_are_shown_only_to_related_users`.

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock crates
git commit -m "feat(auth): GET /v1/users?ids= shows profiles message-svc allows"
```

Expected: 808 tests pass, no warnings, no diff.

---

### Task 9: README, spec, roadmap and pull request

**Files:**
- Modify: `README.md`, `docs/superpowers/specs/2026-10-11-push-messenger-design.md`,
  `docs/superpowers/plans/2026-10-11-roadmap.md`

CI needs no change: the `rust` and `msrv` jobs run the whole workspace and already pull
`postgres:17-alpine` and `redis:7-alpine`. The chart comes with Plan 7B.

- [ ] **Step 1: Document the configuration**

In `README.md`, in the `push-message-svc` table, insert after the row for
`PUSH_ACCOUNTS_TOKEN`:

```markdown
| `PUSH_DIRECTORY_TOKEN` | auth-svc's token for the `Directory` API, 32 to 256 visible ASCII characters; without it `Directory` refuses every call | none |
```

In the `push-auth` table, insert after its last row (the one for `PUSH_AUTH_WORKER_ROLE`):

```markdown
| `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc, `http://host:50051` (plain `http://` only), asked whose profiles a caller may see; without it `GET /v1/users` answers 503 | none |
| `PUSH_DIRECTORY_TOKEN` | message-svc's token for its `Directory` API, the same value as message-svc's; required with `PUSH_MESSAGE_SVC_URL` | none |
```

- [ ] **Step 2: Record the implementation in the spec**

In §3.2, in the implementation notes, replace the bullet that starts `- Transactions run at
READ COMMITTED.` with:

```markdown
- Transactions run at READ COMMITTED. `next_seq` is
  `UPDATE conversation SET last_seq = last_seq + 1, updated_at = greatest(updated_at, $at)
  … RETURNING last_seq`, whose row lock serialises appends. Contact pairs and a group's
  receipt positions are serialised with `pg_advisory_xact_lock` on a 64-bit FNV-1a hash of
  a per-kind tag and the ids; invites with `SELECT … FOR UPDATE`. Registering a device and
  joining a conversation (opening a direct one, creating a group, adding members) take a
  per-user advisory lock for every user concerned, sorted, in one statement (Plan 7A), so
  neither misses the other's uncommitted row and every membership has a delivery position
  for every device. Lock order: contact pair, conversation, users, conversation positions.
  Serialization failures and deadlocks are reported as a retryable conflict.
```

In §4.3, after item 5 of the numbered list (the one that sets
`delivered_seq = GREATEST(delivered_seq, seq)`), add a blank line and:

```markdown
`Sync` is a read: it never moves a position. A sync repeated before the device acks returns
the same entries; the gateway skips those it already sent on the connection, and syncs
again after an ack only when the ack moved the position and the last sync left entries
behind.
```

In §5.3, replace the paragraph that starts `Implementation note (Plan 6A):` with:

```markdown
Implementation notes (Plans 6A and 7A): message-svc's internal `Accounts` gRPC API takes the
relayed `user.verified`, `device.registered`, `device.revoked` and `user.password_reset`.
Every call is idempotent and any order ends in the same state: a registration also records
the user, and a revocation that arrives before the registration records the device as
revoked, so the registration cannot revive it. Every call needs the worker's internal token
(`authorization: Bearer <token>`, compared in constant time); without it, or when
message-svc has none, the call is `UNAUTHENTICATED`. A revoked device is refused by
`Identify` at once (`PERMISSION_DENIED`); a device not relayed yet is `UNAVAILABLE`
(`retryable`), which the gateway closes with 1011 until Plan 8 gives it a code of its own.
After committing a revocation message-svc publishes `Kick{reason: DEVICE_REVOKED}` on
`kick:{device_id}`, every time it is relayed; after a relayed password reset it publishes
`Kick{reason: PASSWORD_RESET}` for every active device of the user, which stay registered.
The gateway closes kicked sockets and refuses tokens older than a reset with Plan 8.
```

In §5.5, replace the bullet that starts `- **Profiles.** `GET /v1/users?ids=…` returns` with:

```markdown
- **Profiles.** `GET /v1/users?ids=…` returns handle and display name for user ids the
  caller shares a contact or conversation with. Implementation (Plan 7A): auth-svc asks
  message-svc's internal `Directory` API, with a token of its own, which ids the caller may
  see — the caller, every user the caller has a relationship with as the caller sees it
  (requests either way, accepted contacts, users the caller blocked) and every user who is
  or was in a conversation with the caller — and answers the verified profiles among them
  from its own database, sorted by id. 1 to 100 ids; other and unknown ids are left out;
  without message-svc's answer it fails closed with a retryable 503.
```

In §5.6, in the implementation notes, replace the bullet that starts `- **Profiles.**
`GET /v1/users/lookup` finds verified users only.` with:

```markdown
- **Profiles.** `GET /v1/users/lookup` finds verified users only. `GET /v1/users?ids=…`
  asks message-svc's `Directory` API (Plan 7A, §5.5).
```

In §7.6, after the bullet that starts `- A failed gRPC call carries the client's `Error`
frame`, add:

```markdown
- The details of a failed call (`grpc-status-details-bin`) are a `google.rpc.Status` whose
  `details` hold the `push.v1.Error` as a `google.protobuf.Any` with type URL
  `type.googleapis.com/push.v1.Error` (Plan 7A). Every domain error has a client
  `ErrorCode` of its own; only retryable errors share `UNAVAILABLE`.
- The client frames `Edit`, `Delete`, `ReadUpTo`, `Request` (a `request_id` and a `oneof`
  of the 18 request bodies of §7.1) and `Response` are defined in `frame.proto` (Plan 7A);
  message-svc's `Messaging` service serves them as `Edit`, `Delete`, `ReadUpTo` and
  `Request`, whose `Response` and `Error` carry the request's `request_id`. They join
  `Frame` with Plan 8.
```

In §13, replace the bullet `- Decide whether group endpoints ship in the first release or the
second.` with:

```markdown
- Group endpoints: message-svc serves them from Plan 7A; whether the gateway exposes them
  in the first release is to be confirmed with the owner (recommended: yes, in Plan 8).
```

- [ ] **Step 3: Mark plan 7A in review**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the empty status cell at the end
of the row for plan 7A (`| 7A | Message service: the core | … | 6B | |`) with `In review`.

- [ ] **Step 4: Final gates**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 test --workspace --locked
```

Expected: no diff, no warnings, 808 tests on both toolchains.

- [ ] **Step 5: Commit and push**

```powershell
git add README.md docs
git commit -m "docs: record plan 7A in the spec, README and roadmap"
git push -u origin feature/plan-07a-message-service
```

- [ ] **Step 6: Open the pull request and wait for CI**

```powershell
$body = @'
Implements docs/superpowers/plans/2026-10-13-plan-07a-message-service.md: message-svc serves the whole messaging core over gRPC for the gateway to call on behalf of an identified device. New client frames Edit, Delete, ReadUpTo, Request and Response (frame.proto; the gateway carries them in Plan 8) and Messaging RPCs Edit, Delete, ReadUpTo and Request cover contacts, invites, direct conversations, groups of up to 100 with admins, read positions, 48-hour edits and deletes, history and listings with unread counts and receipts. Relayed revocations and password resets kick devices on kick:{device_id}. A Directory API behind auth-svc's own token says whose profiles a user may see, and auth-svc serves GET /v1/users?ids= from it. Review fixes: a per-user advisory lock serialises device registrations with joins (a repeated device.registered could start a device past messages it never received; proven by a Postgres concurrency case), every domain error has its own code, error details are a google.rpc.Status with the client error as an Any, sync is documented as a read, and /readyz is checked against the pre-bound listener. 808 tests.

Plan 7B moves the smoke test onto the new API, removes DevSetup and verifies it on minikube.
'@
gh pr create --base main --title "Plan 7A: the message service core over gRPC" --body $body
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust`, `msrv`, `images`, `chart`). Do not merge; the
controller merges after the final review.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §2 each service owns its database; only the worker may call `Accounts` (and now only auth-svc `Directory`, each with its own token) | 7, 8 |
| §3.2 a delivery position for every membership of every device; lock order | 1 |
| §4.2–§4.4 send, sync, ack, read positions; sync never advances; receipts in listings | 2, 4, 6 |
| §4.5 read receipts and read positions on `ReadUpTo`; group aggregates only | 4, 6 |
| §4.6 edits and deletes as entries, within 48 hours, by the sender, frozen by a block; history without edit entries | 4, 5 |
| §5.3 kicks on relayed revocations | 3 |
| §5.4 every device kicked on a password reset | 3 |
| §5.5 contact requests, accept, decline, block, unblock; invites with one error; only accepted contacts chat; profiles for shared contacts and conversations | 5, 7, 8 |
| §6 groups of up to 100: admins and members, last admin cannot be demoted, the longest-standing member promoted, system entries | 6 |
| §7.1 the client frames and request frames | 4, 5, 6 |
| §7.6 errors in the status details; storage failures retryable | 2 |
| §12 integration tests against real Postgres and Redis; concurrency | 1, 3, 6, 7, 8 |

## Deferred to later plans

- **The smoke test, `DevSetup`, the chart and minikube** (Plan 7B): the smoke test drives
  the new API and the kicks, `DevSetup` goes, the chart passes the Directory token to
  auth-svc and message-svc and auth-svc's message-svc URL.
- **The gateway** (Plan 8): the new frames in `Frame`, `Response` and `Error` forwarding,
  `ReAuth`, close codes — closing kicked sockets (4003 for a revoked device; a
  password-reset kick per owner decision 1), refusing tokens older than a password reset,
  a close code of its own for a device not relayed yet, and turning `Identify`'s
  `UNKNOWN_DEVICE`/`UNAVAILABLE` difference into close codes without making it an oracle
  for device ids. Receipts, read positions and contact events reach clients then, too.
- **Rate limits on `RedeemInvite`** per account and per IP (spec §5.6), which only the
  gateway can count by IP (Plan 8). Invite codes have 50 random bits on that assumption.
- **A NetworkPolicy** so only the worker reaches `Accounts`, only auth-svc `Directory` and
  only the gateway `Messaging` (Plan 11). Until then the tokens are the only fence for the
  first two.
- **Rotating the internal tokens** without a restart (Plan 11, Key Vault).
- **Caching visibility in auth-svc**: every `GET /v1/users` asks message-svc; a short cache
  could come with load tests (Plan 11).
- **gRPC load balancing across message-svc replicas** (from Plan 4): auth-svc's client,
  like the gateway's, holds one connection.
