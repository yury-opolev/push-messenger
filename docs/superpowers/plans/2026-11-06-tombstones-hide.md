# Tombstones, `peer_deleted` and hidden direct conversations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deleted accounts answer as tombstones in `GET /v1/users`, direct conversations carry
`peer_deleted`, and a user can hide and unhide a direct conversation on all devices
(spec `docs/superpowers/specs/2026-11-06-tombstones-hide-design.md`).

**Architecture:** auth-svc answers tombstones for disabled verified accounts among the ids
message-svc's Directory already returns, and announces deletions through a new outbox
event `user.deleted` that the worker relays to `Accounts.UserDeleted`. message-svc keeps
`deleted_user` and per-user `conversation_view` tables, reports both flags in listings,
filters hidden conversations, un-hides on new messages and publishes a `ConversationFlags`
notification that the gateway forwards as frame 53. push-sdk stores the flags and offers
hide/unhide.

**Tech Stack:** Rust 1.85, prost/tonic, sqlx/Postgres, axum, rusqlite (SDK), napi, cbindgen.

---

### Task 1: Protocol

**Files:** `crates/push-proto/proto/push/v1/frame.proto`, `crates/push-proto/proto/push/internal/v1/messaging.proto`

- [ ] `ListConversations.include_hidden = 2` (bool); `ConversationSummary.hidden = 12`,
  `peer_deleted = 13`; `Request.hide_conversation = 22` / `unhide_conversation = 23`
  (`HideConversation{bytes conversation_id = 1}`, `UnhideConversation{...}`), answered
  `Done`; server frame `ConversationFlags conversation_flags = 53`
  (`conversation_id = 1, hidden = 2, peer_deleted = 3`).
- [ ] Internal: `Notification.conversation_flags = 8` (`push.v1.ConversationFlags`);
  `Accounts.UserDeleted(AccountUser) returns (AccountsResponse)`.
- [ ] Build (`cargo check --workspace --locked`) and fix exhaustive matches the new arms break
  (message-svc `requests.rs answer`, gateway `hub.rs server_frame`, `FlakySync`-style test
  impls of `Accounts`).

### Task 2: Store contract (domain types, memory and Postgres)

**Files:** `crates/push-domain/src/{model.rs,store.rs,memory.rs}`,
`crates/push-store/migrations/0009_conversation_view.sql`, `crates/push-store/src/store.rs`,
`crates/push-testkit/src/contract/{mod.rs,conversation_view.rs}`

- [ ] Write contract cases first (`contract::conversation_view`): hide/unhide report whether
  the state changed; `unhide_all` returns exactly the users who had it hidden; flags list
  hidden rows, peer-deleted direct conversations and `changed_at`; `mark_user_deleted` is
  idempotent and keeps the first time; unknown user/conversation is a store error.
- [ ] Tx methods:
  ```rust
  async fn set_conversation_hidden(&mut self, conversation: ConversationId, user: UserId, hidden: bool, at: DateTime<Utc>) -> DomainResult<bool>;
  async fn unhide_conversation(&mut self, conversation: ConversationId, at: DateTime<Utc>) -> DomainResult<Vec<UserId>>;
  async fn conversation_flags(&mut self, user: UserId) -> DomainResult<Vec<ConversationFlags>>;
  async fn mark_user_deleted(&mut self, user: UserId, at: DateTime<Utc>) -> DomainResult<bool>;
  async fn direct_peers(&mut self, user: UserId) -> DomainResult<Vec<(ConversationId, UserId)>>;
  ```
  `ConversationFlags { conversation_id, hidden, peer_deleted, changed_at: Option<DateTime<Utc>> }`.
- [ ] Migration 0009: `messaging.deleted_user(user_id PK → known_user, deleted_at)`,
  `messaging.conversation_view(conversation_id, user_id, hidden, changed_at, PK, FK →
  conversation_member)`, index on `(user_id)`. New tables only.
- [ ] Run `cargo test -p push-domain --locked` (memory) and `cargo test -p push-store --locked`
  (Postgres; Docker or CI).

### Task 3: Domain behaviour

**Files:** `crates/push-domain/src/{conversations.rs,send.rs,notify.rs,model.rs,lib.rs}`, new
`crates/push-domain/src/hiding.rs`, `crates/push-testkit/src/scenarios/{mod.rs,hiding.rs}`

- [ ] Scenarios first (`scenarios::hiding`): hide removes from the default listing, the peer
  still lists it, `include_hidden` returns it with `hidden: true`; unhide restores it; hide of
  a group is `Invalid`, of a non-member `NotMember`; hide publishes `ConversationFlags` to
  the user only; a peer's message un-hides and publishes; `changed_since` returns a
  conversation hidden after it; `user_deleted` sets `peer_deleted` for the peer, publishes
  to the peer, and is idempotent; `changed_since` returns the deleted peer's conversation.
- [ ] `ConversationSummary.hidden/peer_deleted`; `list_conversations_with(user,
  changed_since, include_hidden)` (old `list_conversations` = not including hidden);
  `hide_conversation(user, id)`, `unhide_conversation(user, id)`, `user_deleted(user)`;
  `Event::ConversationFlags { user, conversation, hidden, peer_deleted }`; send un-hides.
- [ ] `cargo test -p push-domain -p push-testkit --locked`.

### Task 4: message-svc, gateway, worker

**Files:** `crates/push-message-svc/src/{requests.rs,grpc.rs,notify*.rs}`,
`crates/push-gateway/src/hub.rs`, `crates/push-worker/src/accounts.rs`

- [ ] Tests first: request mapping (hide/unhide/include_hidden, flags in summaries),
  `UserDeleted` (records, publishes), Redis notification → `ConversationFlags` frame in the
  gateway, worker parses `user.deleted`.
- [ ] Implement; `cargo test -p push-message-svc -p push-gateway -p push-worker --locked`.

### Task 5: auth-svc tombstones and the outbox event

**Files:** `crates/push-auth-core/src/{profiles.rs,api.rs,agents.rs,outbox.rs}`,
`crates/push-auth-core/migrations/0010_user_deleted.sql`, tests in
`crates/push-auth-core/src/tests/agents.rs` and `crates/push-auth-core/tests/api.rs`

- [ ] Tests first: after an owner deletes an agent, a visible caller gets
  `{deleted: true, state: "deleted", kind: "agent", display_name: "Deleted agent",
  handle: "", owner_user_id: null}`; a caller the Directory does not return gets nothing;
  live profiles carry `deleted: false, state: "active"`; deleting writes `user.deleted`.
- [ ] Migration 0010: outbox kind check plus `user.deleted`; backfill one event per verified
  disabled account.
- [ ] `cargo test -p push-auth-core --locked`.

### Task 6: push-sdk and bindings

**Files:** `crates/push-sdk/src/{auth.rs,model.rs,store.rs,engine.rs,client/requests.rs,testing.rs}`,
`bindings/ffi/src/json.rs`, `bindings/c/src/client.rs`, `bindings/c/include/push_sdk.h`,
`bindings/node/{src/client.rs,index.js,index.d.ts,test/*}`

- [ ] `Profile.deleted: bool` (`#[serde(default)]`, `handle` may be empty).
- [ ] `Conversation.hidden`, `Conversation.peer_deleted`; store schema step adds both columns;
  listing asks `include_hidden: true` and stores them; frame 53 updates the store and emits
  `ConversationChanged`; `Client::hide_conversation` / `unhide_conversation` (request →
  `Done`, store updated).
- [ ] Bindings: JSON shapes (`hidden`, `peerDeleted`/`peer_deleted`, `deleted`), C functions
  `push_client_hide_conversation` / `push_client_unhide_conversation`, Node methods; regenerate
  the C header (`PUSH_SDK_BLESS_HEADER=1`).
- [ ] `cargo test -p push-sdk -p push-sdk-ffi -p push-sdk-c --locked`; `npm test` in
  `bindings/node` if Node is available.

### Task 7: Docs and CHANGELOG

- [ ] Design spec §5.5 (tombstones), §7.1/§7.2 tables (requests, frame), §8.2 (SDK);
  `docs/sdk-bindings.md`; `CHANGELOG.md` under Unreleased.
- [ ] Full gates: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo test --workspace --locked` (Postgres suites in CI).
