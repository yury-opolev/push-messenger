# Agent connection policy and read-only owner view Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Agents accept requests under a policy their owner chooses, owners act on waiting
requests and read everything their agents do (read-only), and every peer gets the notice
data (spec `docs/superpowers/specs/2026-11-06-agent-connections-and-owner-view.md`).

**Architecture:** message-svc stores the policy, resolves requests to agents in
`Messaging::contact_request`, counts agent limits in the database and releases refused
requests with a sweeper. It serves a new internal `AgentOwners` gRPC API that re-checks
ownership. auth-svc serves the owner routes under `/v1/agents/{agent_id}/…`, checks
ownership in one query, joins the owner's name into agent profiles and writes audit lines.
The gateway forwards `AgentContactEvent` (frame 54). push-sdk and push-bridge follow.

**Tech Stack:** Rust 1.85 (no let-chains), prost/tonic, sqlx/Postgres, axum, Redis,
rusqlite (SDK), napi, cbindgen.

**Delivery:** six PRs, each merged green before the next starts (main stays green):

| PR | Contents | Tasks |
|---|---|---|
| A | this plan and the spec (docs only) | — |
| B | protocol additions, profile owner fields, policy store and migration | 1–3 |
| C | request resolution, agent rules, limits, sweeper, owner actions (gRPC + HTTP), event | 4–7 |
| D | read-only owner view and audit | 8–9 |
| E | agent-to-agent call refusal, deletion cleanup, push-sdk, bindings, push-bridge | 10–13 |
| F | docs, CHANGELOG, chart, whole-stack acceptance test | 14–15 |

Gates for every PR: `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets --locked -- -D warnings`, the touched crates'
`cargo test -p … --locked` (Postgres suites need Docker; CI runs them), and
`cargo +1.85 build --workspace --locked` when available. A new SQL migration in
push-auth-core needs `cargo clean -p push-auth-core` locally (no build.rs).

---

## PR B

### Task 1: Protocol

**Files:** `crates/push-proto/proto/push/v1/frame.proto`,
`crates/push-proto/proto/push/internal/v1/messaging.proto`

- [ ] `ErrorCode.ERROR_CODE_NOT_ALLOWED = 31`, `ERROR_CODE_OWNER_DECIDES = 32`.
- [ ] `message AgentContactEvent { bytes agent_user_id = 1; bytes user_id = 2; ContactState state = 3; }`,
  `Frame.agent_contact_event = 54`.
- [ ] Internal: `enum AgentPolicy` (ASK 1 … REFUSE_ALL 5), `Notification.agent_contact_changed = 9`
  (`push.v1.AgentContactEvent`), `service AgentOwners` with the nine RPCs of spec §5.2 and
  their messages (`AgentRef{owner_user_id, agent_user_id}`, `SetAgentPolicyRequest{agent, policy}`,
  `AgentPolicyReply{policy}`, `WaitingRequests{repeated WaitingRequest{user_id, requested_at}}`,
  `ResolveRequest{agent, user_id, ResolveAction action}`, `AgentPeerRequest{agent, user_id}`,
  `AgentOwnersDone{}`, `AgentConversations{repeated AgentConversation}`,
  `AgentHistoryRequest{agent, conversation_id, optional before_seq, limit}`,
  `AgentCallHistoryRequest{agent, optional CallHistoryCursor before, limit}`).
- [ ] `cargo check --workspace --locked`; fix exhaustive matches (gateway `hub.rs`
  notification → frame, SDK frame dispatch ignores 54 for now, message-svc error tables).

### Task 2: Profile owner fields (auth-svc)

**Files:** `crates/push-auth-core/src/profiles.rs`, `src/api.rs`, `src/agents.rs`,
tests `crates/push-auth-core/src/tests/agents.rs`, `crates/push-auth-core/tests/api.rs`

- [ ] Test first: an agent's profile in `GET /v1/users`, `/v1/users/lookup` and the agent's
  `/v1/users/me` carries `owner_display_name`/`owner_handle` of its owner; a rename of the
  owner shows at once; people and tombstones carry `null`; an agent of a disabled owner
  carries `null`.
- [ ] `Profile { owner_display_name: Option<String>, owner_handle: Option<String> }`, filled by a
  `LEFT JOIN auth.user_account owner ON owner.user_id = account.owner_user_id AND owner.disabled_at IS NULL`.
- [ ] `cargo test -p push-auth-core --locked`.

### Task 3: Policy store (domain, memory, Postgres)

**Files:** `crates/push-domain/src/{model.rs,store.rs,memory.rs}`,
`crates/push-store/migrations/0010_agent_connections.sql`, `crates/push-store/src/store.rs`,
`crates/push-testkit/src/contract/{mod.rs,agents.rs}`

- [ ] Contract cases first: policy defaults to none (`ask`), set/replace keeps the latest;
  refused rows: insert keeps the first `release_at`, `due_refusals(now)` returns due rows,
  delete; request log: append, `count_since`, oldest since, prune; `pending_out_count`,
  `waiting_out_count` (peer is an agent with a `requested_in` row back), `waiting_in_count`;
  `message_counts(user)`.
- [ ] `AgentPolicy` enum (`Ask, AutoOwn, AutoOwnerContacts, AutoAny, RefuseAll`) with
  `as_i16`/`from_i16`.
- [ ] Tx methods:
  ```rust
  async fn agent_policy(&mut self, agent: UserId) -> DomainResult<Option<AgentPolicy>>;
  async fn set_agent_policy(&mut self, agent: UserId, policy: AgentPolicy, at: DateTime<Utc>) -> DomainResult<()>;
  async fn put_refusal(&mut self, requester: UserId, agent: UserId, release_at: DateTime<Utc>) -> DomainResult<()>;
  async fn delete_refusal(&mut self, requester: UserId, agent: UserId) -> DomainResult<bool>;
  async fn due_refusals(&mut self, now: DateTime<Utc>, limit: u32) -> DomainResult<Vec<(UserId, UserId)>>;
  async fn log_agent_request(&mut self, agent: UserId, peer: UserId, at: DateTime<Utc>) -> DomainResult<()>;
  async fn agent_requests_since(&mut self, agent: UserId, since: DateTime<Utc>) -> DomainResult<(u32, Option<DateTime<Utc>>)>;
  async fn prune_agent_requests(&mut self, before: DateTime<Utc>) -> DomainResult<u64>;
  async fn outgoing_counts(&mut self, user: UserId) -> DomainResult<(u32, u32)>; // pending, waiting at agents
  async fn waiting_in_count(&mut self, agent: UserId) -> DomainResult<u32>;
  async fn message_counts(&mut self, user: UserId) -> DomainResult<Vec<(ConversationId, i64)>>;
  async fn contact_since(&mut self, owner: UserId, peer: UserId) -> DomainResult<Option<DateTime<Utc>>>;
  ```
- [ ] Migration 0010 as spec §8 with the reverse-SQL comment.
- [ ] `cargo test -p push-domain -p push-testkit --locked`; `cargo test -p push-store --locked`.

## PR C

### Task 4: Request resolution and agent rules (domain)

**Files:** new `crates/push-domain/src/agents.rs`, `contacts.rs`, `invites.rs`, `error.rs`,
`notify.rs`, `lib.rs`; scenarios `crates/push-testkit/src/scenarios/agents.rs`

- [ ] Scenarios first, each policy against a person and an agent requester: `ask` holds
  (requester `RequestedOut`, agent `RequestedIn`, owner event); `auto_own` accepts a
  sibling agent and holds a person; `auto_owner_contacts` accepts the owner's contact, its
  agents and the owner's agents, holds a stranger; `auto_any` accepts; `refuse_all`
  answers `RequestedOut`, agent sees nothing, no owner event, and after `release_refusals`
  the requester's row is gone with no event to anyone; the owner is always accepted;
  agent accept/decline → `OwnerDecides`; agent blocking its owner → `NotAllowed`; agent
  blocking a waiting requester → `OwnerDecides`; agent `CreateInvite` → `NotAllowed`;
  redeeming an agent's invite → `InviteNotFound`; queue of 200 → refused.
- [ ] `DomainError::{NotAllowed, OwnerDecides, RateLimited { retry_after_s: u32 }}`;
  `Event::AgentContact { owner, agent, user, state }` published with every
  `ContactChanged` of an owned agent.
- [ ] `Messaging::with_refusal_delay(min, max)`; `release_refusals(now)` returns how many.
- [ ] `cargo test -p push-domain -p push-testkit --locked`.

### Task 5: Agent limits (domain)

**Files:** `crates/push-domain/src/agents.rs`, scenarios

- [ ] Scenarios: the 21st request in an hour is `RateLimited` with `retry_after_s` ≤ 3600;
  the 101st in a day; the 51st pending; the 11th waiting at agents; people are not
  limited here; a request to an unknown id counts.
- [ ] Implement under `lock_users(&[agent])`.

### Task 6: message-svc (requests, AgentOwners actions, sweeper, notifier) and gateway

**Files:** `crates/push-message-svc/src/{convert.rs,requests.rs,grpc.rs,notify.rs,server.rs,config.rs}`,
new `src/owners.rs`; `crates/push-gateway/src/hub.rs`

- [ ] Tests first: error mapping (`OWNER_DECIDES`, `NOT_ALLOWED`, `RATE_LIMITED` with
  `retry_after_s`); `AgentOwners` policy/waiting/resolve/contacts/remove, `NOT_FOUND` for a
  non-owner or deleted agent; notifier publishes `agent_contact_changed` on `notify:{owner}`;
  gateway forwards it as frame 54.
- [ ] Domain owner actions: `owner_policy`, `owner_set_policy`, `owner_waiting`,
  `owner_resolve`, `owner_contacts`, `owner_remove_contact` (each checks
  `agent_owner(agent) == owner` and not deleted, else `NotFound`-like
  `DomainError::UnknownUser` mapped to `NOT_FOUND`).
- [ ] Sweeper: every 30 s `release_refusals` + prune; env `PUSH_AGENT_REFUSAL_MIN_S`,
  `PUSH_AGENT_REFUSAL_MAX_S` (defaults 600, 21600; min ≤ max).
- [ ] `cargo test -p push-message-svc -p push-gateway --locked`.

### Task 7: auth-svc owner action routes

**Files:** new `crates/push-auth-core/src/owners.rs`, `src/api.rs`, `src/audit.rs`,
`src/error.rs`, `src/server.rs`, tests `src/tests/owners.rs`

- [ ] Tests first (stub `AgentOwners` server): every route `404 not_found` for a stranger,
  the agent, a contact, an unknown id and a deleted agent, without calling message-svc;
  the owner gets policy/requests/contacts with embedded profiles; bad policy `400 invalid`;
  owner as target `400 owner_contact`; audit events per route without content.
- [ ] `AuthError::{NotFound, OwnerContact}`; `trait AgentOwnerApi` with `GrpcAgentOwners` and
  `NoAgentOwners`; routes of spec §5.1; audit extension `OwnerAudit { agent, peer,
  conversation, count }` logged as `agent_id`, `peer_id`, `conversation_id`, `count`.
- [ ] `cargo test -p push-auth-core --locked`.

## PR D

### Task 8: Owner view in message-svc

**Files:** `crates/push-domain/src/agents.rs` (view functions), `crates/push-message-svc/src/owners.rs`

- [ ] Tests first: `owner_conversations` lists the agent's conversations with
  `message_count`, `last_activity_at`, peer; `owner_history` equals `history(agent, …)`;
  `owner_calls` equals the agent's call history; none moves the agent's `read_seq`,
  delivery cursor, hidden flag; non-owner `NOT_FOUND`.
- [ ] Implement `Conversations`, `History`, `CallHistory` RPCs.

### Task 9: Owner view routes and audit in auth-svc

**Files:** `crates/push-auth-core/src/owners.rs`, `api.rs`, `audit.rs`, `limits.rs`

- [ ] Tests first: the three GET routes and their JSON (spec §5.3); POST/PUT/PATCH/DELETE on
  them answer 405; `not_found` for non-owners; audit lines `agent.view.conversations`,
  `agent.view.history`, `agent.view.calls` with counts and no bodies; 600 an hour limit.
- [ ] Implement.

## PR E

### Task 10: Agent-to-agent calls

**Files:** `crates/push-domain/src/calls/policy.rs`, tests `calls/tests/policy.rs`,
`crates/push-message-svc/src/calls/*` (error mapping), `crates/push-gateway/tests/calls.rs`

- [ ] Test first: two agents that are accepted contacts → `NotAllowed`; not contacts →
  `NotContact`; owner ↔ agent unchanged.
- [ ] Implement in `permission`.

### Task 11: Deletion cleanup

**Files:** `crates/push-domain/src/hiding.rs` (`user_deleted`), store (`delete_contacts_of`),
scenarios

- [ ] Scenario first: deleting an agent removes its waiting requests, outgoing requests and
  contacts (peers' blocks kept), publishes `ContactChanged(None)` to changed peers, deletes
  its policy and log; owner actions and view then `NOT_FOUND`; conversations and
  `peer_deleted` stay.
- [ ] Implement.

### Task 12: push-sdk and bindings

**Files:** `crates/push-sdk/src/{auth.rs,model.rs,engine.rs,error.rs,client.rs}`, new
`src/client/owners.rs`; `bindings/ffi/src/json.rs`, `bindings/node/*`, `bindings/c/*`

- [ ] Tests first: profile owner fields parse (absent → None); frame 54 emits
  `AgentRequestWaiting` for `REQUESTED_IN` and `AgentContactChanged` otherwise; owner
  endpoints against a stub HTTP server (paths, `not_found` → `SdkError::NotFound`);
  `OWNER_DECIDES`/`NOT_ALLOWED` map to errors.
- [ ] Bindings: JSON of the new events and profile fields.

### Task 13: push-bridge

**Files:** `crates/push-bridge/src/core/{contacts.rs,mod.rs}`, `commands.rs`, `messenger.rs`

- [ ] Tests first: a contact's first text in a session gets the notice once; `/help` of a
  contact and of the owner mention the owner view; a request event is logged and nothing is
  accepted; rich replies from an agent peer never reach the backend.
- [ ] Implement; the owner's name comes from the agent's own profile.

## PR F

### Task 14: Docs, CHANGELOG, chart

- [ ] Main design §5.5 (policy, notice), §7.1/§7.2 (codes, frame 54), auth endpoints;
  `docs/agent-bridge.md` (§6 strangers/contacts, §9 security, §19 echo), `docs/calls.md`
  (agent-to-agent refusal), `docs/sdk-bindings.md`; CHANGELOG under Unreleased; chart
  `deploy/helm/push-messenger` message-svc env for the refusal delay.

### Task 15: Whole-stack acceptance test

**Files:** `crates/push-smoke/tests/agent_connections.rs`

- [ ] The scenario of spec §11 with Echo and a stand-in Coda; run with
  `cargo test -p push-smoke --test agent_connections --locked` (Docker).
