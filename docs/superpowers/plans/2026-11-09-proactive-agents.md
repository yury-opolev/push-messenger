# Proactive agents Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let agent backends start actions (contact requests, chats, groups, calls, owner
notes) under an owner-set, server-enforced outgoing policy with an ask queue, an inbound
policy for who reaches the backend, a loop guard, rate limits and `started_by_agent` marks.

**Architecture:** message-svc (`push-domain` + `push-store`) owns settings, the action queue,
the gate on existing requests and the limits; auth-svc adds owner routes and audit; the
protocol adds two error codes, two requests and two frames; push-sdk exposes them; the
bridge gets one action path used by Echo commands and by an MCP server it serves to Coda.

**Tech Stack:** Rust 1.85 workspace, tokio, prost/tonic, axum, sqlx/Postgres, Redis.

**Spec:** `docs/superpowers/specs/2026-11-09-proactive-agents.md` (section refs `§n` below).

---

## Conventions for every task

- Work in a git worktree off `origin/main` per PR branch (`git worktree add ..\push-proactive-N -b <branch> origin/main`).
- TDD: write the failing test first, run it, implement, run again.
- Before each commit: `cargo fmt --all`; before each push the CI set:
  `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo test --workspace --locked` (Postgres/Redis tests need Docker running), and for the
  bindings `cargo test --locked -p push-sdk-c -p push-sdk-ffi`.
- Every commit ends with `Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>`.
- Follow the existing agent-connection code as the template (it was built the same way):
  `push-domain/src/agents.rs`, `push-store/src/store.rs` (agent section),
  `push-message-svc/src/owners.rs`, `push-auth-core/src/owners.rs` + `audit.rs`,
  `push-sdk/src/owners.rs`, `push-testkit/src/{contract,scenarios}/agents.rs`.

## PR split

| PR | Branch | Tasks |
|---|---|---|
| 1 | `docs/proactive-agents-spec` | spec + this plan |
| 2 | `feat/proactive-agents-server` | Tasks 1–6 (model, store, domain gate/queue/limits, calls, protocol, message-svc) |
| 3 | `feat/proactive-agents-owner-routes` | Tasks 7–8 (auth-svc routes + audit, profiles in actions) |
| 4 | `feat/proactive-agents-sdk` | Task 9 |
| 5 | `feat/proactive-agents-bridge` | Tasks 10–14 (actions core, inbound, loop guard, echo, contact calls) |
| 6 | `feat/proactive-agents-coda-tools` | Tasks 15–16 (MCP server, Coda wiring) |
| 7 | `feat/proactive-agents-docs-e2e` | Tasks 17–18 (docs, CHANGELOG, chart check, whole-stack test) |

---

### Task 1: Domain model and store contract

**Files:**
- Modify: `crates/push-domain/src/model.rs` (new types), `crates/push-domain/src/store.rs` (Tx methods), `crates/push-domain/src/memory.rs`
- Create: `crates/push-store/migrations/0011_proactive_agents.sql`
- Modify: `crates/push-store/src/store.rs`, `crates/push-store/src/rows.rs`
- Test: `crates/push-testkit/src/contract/agents.rs` (extend) — runs against memory and Postgres

Types (model.rs):

```rust
pub enum OutgoingPolicy { Allowed = 1, Ask = 2, Off = 3 }      // code()/from_code()
pub enum InboundPolicy { OwnerOnly = 1, OwnerAgents = 2, Contacts = 3 }
pub struct AgentSettings { pub inbound: InboundPolicy, pub requests: OutgoingPolicy,
    pub chats: OutgoingPolicy, pub groups: OutgoingPolicy, pub calls: OutgoingPolicy }
impl Default for AgentSettings { /* OwnerOnly, Ask, Ask, Off, Off */ }
pub enum OutgoingKind { Request = 1, Chat = 2, Group = 3, Invite = 4, Call = 5 }
pub enum OutgoingState { Waiting = 1, Approved = 2, Declined = 3, Expired = 4, Failed = 5 }
pub struct OutgoingAction { pub action_id: Uuid, pub agent: UserId, pub kind: OutgoingKind,
    pub target: Option<UserId>, pub conversation: Option<ConversationId>,
    pub title: Option<String>, pub members: Vec<UserId>, pub state: OutgoingState,
    pub error: Option<ErrorKindCode /* the DomainError's wire code name, &'static str */>,
    pub created_at: DateTime<Utc>, pub resolved_at: Option<DateTime<Utc>>,
    pub used_at: Option<DateTime<Utc>> }
```
`Conversation` gains `started_by_agent: bool`; `ConversationSummary` gains
`started_by_agent: bool`; `CallHistoryRow` gains `started_by_agent: bool`.

Tx methods (store.rs), implemented in memory and Postgres:
`agent_settings(agent) -> AgentSettings` (default when no row),
`set_agent_settings(agent, &AgentSettings, at)`,
`insert_outgoing(&OutgoingAction)`, `find_waiting_outgoing(agent, kind, target, conversation, title, &sorted members) -> Option<OutgoingAction>`,
`outgoing(action_id) -> Option<OutgoingAction>`, `waiting_outgoing_count(agent) -> u32`,
`list_outgoing(agent, waiting_only: bool, resolved_since) -> Vec<OutgoingAction>` (oldest first),
`resolve_outgoing(action_id, state, error, conversation, at)`,
`approved_call_grant(agent, callee, since) -> Option<Uuid>` + `use_outgoing(action_id, at)`,
`expire_outgoing(before, at, limit) -> Vec<OutgoingAction>`, `prune_outgoing(resolved_before) -> u64`,
`log_agent_action(agent, kind, count, at)`, `agent_actions_since(agent, kind, since) -> (u32, Option<DateTime>)` (sum of count, oldest),
`prune_agent_actions(before) -> u64`.
`forget_agent` also deletes `agent_settings`, `outgoing_action`, `agent_action_log` rows.
Conversation insert writes `started_by_agent`; call-history insert writes it.

- [ ] Step 1: Contract tests: defaults when no settings row; set/get round trip; insert → find_waiting (dedupe key incl. sorted members) → list (waiting only / resolved since) → resolve → not found as waiting; expire picks only waiting older than cutoff; prune removes resolved older than cutoff; action log sum and oldest per kind; forget_agent removes all three; `started_by_agent` round-trips on conversation and call history.
- [ ] Step 2: Run `cargo test -p push-domain` and `cargo test -p push-store` — fail.
- [ ] Step 3: Migration 0011 exactly as spec §10 plus backfill:
```sql
UPDATE messaging.conversation c SET started_by_agent = true
FROM messaging.agent_owner a
WHERE a.agent_user_id = c.created_by
  AND (c.kind = <group code>
       OR NOT EXISTS (SELECT 1 FROM messaging.member m
                      WHERE m.conversation_id = c.conversation_id
                        AND m.user_id = a.owner_user_id));
UPDATE messaging.call_history h SET started_by_agent = true
WHERE (h.direction = 'out' AND EXISTS (SELECT 1 FROM messaging.agent_owner a WHERE a.agent_user_id = h.user_id))
   OR (h.direction = 'in'  AND EXISTS (SELECT 1 FROM messaging.agent_owner a WHERE a.agent_user_id = h.peer_user_id));
```
(adjust column names to the real schema in 0001/0008; reverse SQL in a header comment).
- [ ] Step 4: Implement memory + Postgres; tests pass.
- [ ] Step 5: Commit `feat: agent settings, outgoing actions and action log in the store`.

### Task 2: Domain gate, queue, limits, approvals

**Files:**
- Create: `crates/push-domain/src/outgoing.rs` (gate, limits, approve/decline, expiry, owner/agent events)
- Modify: `agents.rs` (constants: `AGENT_REQUESTS_PER_HOUR=10`, `AGENT_REQUESTS_PER_DAY=20`; release sweep also expires/prunes), `contacts.rs` (gate in `contact_request`), `conversations.rs` (gate in `open_direct`, set `started_by_agent`), `groups.rs` (gate in `create_group`/`add_members`, set `started_by_agent`), `error.rs` (`WaitingForOwner{action_id}`, `NotAllowedByOwner`), `notify.rs` (`Event::AgentSettings{agent, settings}`, `Event::Outgoing{to: UserId, action: OutgoingAction}`), `lib.rs`
- Test: `crates/push-testkit/src/scenarios/outgoing.rs` (new; register in `scenarios/mod.rs`)

Constants: `CHATS_PER_DAY=50`, `CALLS_PER_DAY=10`, `GROUPS_PER_DAY=5`, `INVITES_PER_DAY=20`,
`WAITING_ACTIONS_MAX=20`, `OUTGOING_EXPIRY=24h`, `RESOLVED_KEPT=24h`, `CALL_GRANT=15min`.

Core function:
```rust
pub(crate) enum Gate { Proceed, Waiting(OutgoingAction) }
pub(crate) async fn gate<T: Tx>(tx: &mut T, agent: UserId, owner: UserId, ask: Ask, now) -> DomainResult<Gate>
// Ask { kind, target, conversation, title, members, count }
```
Order per spec §3.2. Contact requests use `check_agent_limits` inside the gate (for `allowed`
and `ask`) instead of the action log. Callers on `Gate::Waiting(a)`: push
`Event::Outgoing{to: owner, a}`, commit, publish, return `Err(DomainError::WaitingForOwner{action_id})`.
Callers take a `skip_gate: bool` (internal `*_as` variants) used by approval.

Messaging methods (owner side, each `require_owned` first):
`agent_settings_for_owner`, `set_outgoing_policy(owner, agent, partial)`,
`set_inbound_policy(owner, agent, InboundPolicy)` (both publish `Event::AgentSettings` to the agent),
`waiting_outgoing(owner, agent)`, `approve_outgoing(owner, agent, action_id) -> OutgoingAction`,
`decline_outgoing(owner, agent, action_id)`.
Agent side: `own_settings(agent)` (`NotAllowed` for people), `own_outgoing(agent)` (waiting + resolved in 24 h).
Sweeper: `release_refusals()` also calls `expire_outgoing` (events to owner and agent) and prunes.

Scenario tests (each a `#[tokio::test]` in the scenario file pattern):
- `requests_ask_queues_and_owner_approval_sends_it`
- `requests_off_answers_not_allowed_by_owner_and_stores_nothing`
- `requests_allowed_sends_at_once`
- `same_waiting_action_answers_same_id`
- `request_to_owner_is_never_gated`
- `asking_back_a_waiting_requester_is_a_request_action` (mutual hole)
- `gate_answer_is_the_same_for_unknown_blocked_and_present_targets`
- `chat_ask_then_approve_opens_it_started_by_agent`
- `existing_conversation_is_not_gated` (peer opened first; agent opens → returned)
- `owner_chat_is_not_gated_and_not_started_by_agent`
- `group_off_by_default`, `group_allowed_creates_started_by_agent`, `invite_ask_then_approve_adds_member`
- `approval_failing_rules_ends_failed` (contact removed before approve → `failed`, `not_contact`)
- `decline_and_expiry_tell_agent_owner_declined` (agent event state Declined/Expired; mapping to wire is message-svc's)
- `limits_per_kind_and_waiting_cap` (chats 51st → RateLimited with retry_after; waiting 21st → 600)
- `contact_request_limits_are_ten_an_hour_twenty_a_day` (update the existing agent limit scenario)
- `settings_changes_publish_to_agent`
- `deleting_agent_drops_its_actions`
- `people_are_not_gated`

- [ ] Steps: failing scenarios → implement → `cargo test -p push-domain -p push-testkit -p push-store` green → commit `feat: outgoing policy gate, ask queue and agent action limits`.

### Task 3: Calls

**Files:** `crates/push-domain/src/calls/policy.rs`, `calls/history.rs`, `crates/push-message-svc/src/calls/{mod.rs,redis.rs,wire.rs}`, tests `crates/push-domain/src/calls/tests/policy.rs`, `crates/push-message-svc/tests/calls_service.rs`.

- `permission()` caller-agent branch: callee == owner → allowed; callee agent → unchanged
  (`NotAllowed` if contacts, else `NotContact`); callee a person and accepted contact → the
  gate (`calls`), first consuming an approved grant (`approved_call_grant(agent, callee, now-15min)` → `use_outgoing`). `call_permission` returns a new
  `CallPermission { Allowed{by_agent: bool}, Waiting(action_id), ... }` or `DomainResult` with
  `WaitingForOwner`/`NotAllowedByOwner`/`RateLimited` — the transaction must commit on `Waiting`.
- `CallRecord` gains `#[serde(default)] by_agent: bool`; `ended_rows` sets `started_by_agent` on both rows.
- message-svc `refused()` maps the new errors to `CallError{code, action_id, retry_after_s}`.
- Tests: agent → contact under off/ask/allowed; grant used once; grant older than 15 min not used; agent → owner always; agent → agent `NOT_ALLOWED`; history rows carry `started_by_agent`.
- [ ] Commit `feat: agents call their contacts under the owner's call policy`.

### Task 4: Protocol

**Files:** `crates/push-proto/proto/push/v1/frame.proto`, `crates/push-proto/proto/push/internal/v1/messaging.proto`, `crates/push-proto/src/codec.rs` tests if frames are enumerated, fuzz corpus untouched.

Add exactly spec §6.2/§6.3 (numbers: ErrorCode 33/34, Error.action_id 7, CallError.action_id 6,
Request 24/25, Response 13/14, Frame 55/56, ConversationSummary 14, CallHistoryEntry 9,
Notification 10/11, AgentConversation 11, AgentOwners RPCs). Verify each number is free
first (`Select-String` the proto). Messages: `GetAgentSettings{}`, `ListOutgoingActions{}`,
`AgentSettings`, `OutgoingActionList{repeated OutgoingAction actions = 1}`,
`AgentSettingsEvent{AgentSettings settings = 1}`, `OutgoingActionEvent{OutgoingAction action = 1}`.
Internal: `AgentSettingsReply{AgentSettings settings=1}`, `SetOutgoingRequest{AgentRef agent=1; optional OutgoingPolicy requests=2; chats=3; groups=4; calls=5}`, `SetInboundRequest{AgentRef agent=1; InboundPolicy policy=2}`, `OutgoingActions{repeated OutgoingAction actions=1}`, `ResolveOutgoingRequest{AgentRef agent=1; bytes action_id=2; bool approve=3}`, `OutgoingActionReply{OutgoingAction action=1}`.
- [ ] Build (`cargo build -p push-proto`), fix every non-exhaustive match the workspace build reports, commit with Task 5.

### Task 5: message-svc wiring

**Files:** `crates/push-message-svc/src/{requests.rs,convert.rs,notify.rs,owners.rs}` (+ tests `requests/tests.rs`, `owners/tests.rs`), `crates/push-gateway/src/hub.rs` (map Notification 10/11 → frames 55/56).

- `convert.rs::domain_error`: `WaitingForOwner` → `FailedPrecondition`, code `WAITING_FOR_OWNER`, `action_id`; `NotAllowedByOwner` → `NOT_ALLOWED_BY_OWNER`.
  The `Error` frame built for requests must carry `action_id` (find where `retry_after_s` is set and do the same).
- `requests.rs`: `GetAgentSettings`, `ListOutgoingActions`; `summary()` sets `started_by_agent`.
- `notify.rs`: `Event::AgentSettings` → agent; `Event::Outgoing{to, action}` → `to`; when `to` is the agent, `Declined`/`Expired` are sent as `OWNER_DECLINED`.
- `owners.rs`: new RPCs; `AgentConversation.started_by_agent`.
- Call history wire: `CallHistoryEntry.started_by_agent`.
- Tests: request mapping for each error (code + action_id), the two new requests (agent ok, person `NOT_ALLOWED`), notification mapping (agent never sees EXPIRED), RPC ownership re-check.
- [ ] Full CI set green → commit `feat: proactive agent protocol, requests and events in message-svc` → push PR 2, watch, merge.

### Task 6: (folded into Tasks 4–5)

### Task 7: auth-svc owner routes

**Files:** `crates/push-auth-core/src/{api.rs,owners.rs,audit.rs,calls.rs}`, tests `src/tests/owners.rs`, `src/audit.rs` tests.

Routes per spec §6.1 using the existing `owner_of` / `finish` / `OwnerAudit` pattern.
`OwnerAudit` gains `action_id: Option<Uuid>`, `kind: Option<&'static str>`, `outgoing: Option<[&'static str; 4]>` (logged as `policy="requests:ask chats:ask groups:off calls:off"`) — keep `Copy`.
`owner_event`: `GET outgoing-policy` → `agent.outgoing_policy.read`, `PUT` → `.changed`; inbound likewise; `GET outgoing` → `agent.outgoing.listed`; approve/decline → `agent.outgoing.approved|declined`.
Action JSON with profiles through the agent's visibility (reuse the requests listing's profile lookup); `expires_at = created_at + 24h`.
`GET /v1/calls` and owner-view conversations/calls JSON gain `started_by_agent`.
Tests: each route happy path; 404 for stranger/agent/unknown/deleted; `400 invalid` for unknown key/value; approve not waiting → 404; audit lines carry ids/kind/policy and never title/text; PUT partial keeps the others.
- [ ] Commit `feat: owner routes for outgoing and inbound policy and the outgoing queue` → PR 3.

### Task 9: push-sdk

**Files:** `crates/push-sdk/src/{owners.rs,client/owners.rs,client/requests.rs,model.rs,engine.rs,error.rs,calls.rs}`, tests next to them (`owners/tests.rs`, engine tests with the fake server in `testing.rs`), and bindings `push-sdk-c`/`push-sdk-ffi`/node if they match on changed enums.

- Agent: `agent_settings()`, `outgoing_actions()`, `create_group(title, members) -> Conversation`, `add_members(group, users)`; events `AgentSettingsChanged`, `OutgoingActionChanged`; `SdkError::Server{.., action_id: Option<String>}`; `CallErrorCode::WaitingForOwner`/`NotAllowedByOwner` + `action_id` on the call error.
- Owner: `outgoing_policy`, `set_outgoing_policy(agent, OutgoingPolicyUpdate)`, `inbound_policy`, `set_inbound_policy`, `outgoing_actions(agent)`, `approve_outgoing`, `decline_outgoing`.
- Model fields `started_by_agent`.
- [ ] Commit `feat: proactive agents in push-sdk` → PR 4.

### Tasks 10–14: Bridge (PR 5)

**Files:**
- Create: `crates/push-bridge/src/actions.rs` (Action, ActionRequest, ActionResult, ActionCode, parse helpers shared by echo and MCP)
- Create: `crates/push-bridge/src/core/actions.rs` (run actions, pending actions, outcomes), `crates/push-bridge/src/core/loop_guard.rs`
- Modify: `backend.rs` (`BackendEvent::Action(ActionRequest)`), `messenger.rs` (new trait methods with recording TestMessenger impls: `lookup`, `profile_of`, `request_contact`, `open_direct`, `create_group`, `add_members`, `agent_settings`, `outgoing_actions`, `send_text` already exists), `core.rs` (events, settings, inbound), `core/conversation.rs` + `core/contacts.rs` (inbound policy, agent turns as plain text, no notice to agents), `core/control.rs`/`commands.rs` (`/loop`), `core/calling.rs` + `core/voice.rs` (contact calls), `state.rs` (`pending_actions`), `config.rs` (`loopGuardTurns`, `tools`), `echo.rs` (commands), `voice/phone.rs` (call error codes).
- Tests: `core/actions_tests.rs`, `core/echo_tests.rs` (extend), `core/voice_tests.rs` (contact call), `tests/bridge.rs` (two echo bridges loop guard if feasible with TestMessenger pair; else in core tests).

Steps (each TDD, commit per task):
- [ ] Task 10 actions core: every action → messenger call → result code mapping (`SdkError::Server{code:"waiting_for_owner", action_id}` → `waiting_for_owner` etc.); pending actions persisted; on `OutgoingActionChanged{state:Approved, conversation}` send held text / dial; declined/failed dropped; owner note per outcome; boot reconciliation via `outgoing_actions()`.
- [ ] Task 11 inbound policy: settings fetched at boot and on `AgentSettingsChanged`; `owner_only` → PRIVATE to contacts; `owner_agents` → only same-owner agents; `contacts` → all; agent peers get plain text answers and no notice.
- [ ] Task 12 loop guard: counts, pause at N, notes, `/loop`, `/loop resume [@h]`, reset by a person's message; `send_message` → `paused`.
- [ ] Task 13 echo commands per spec §7.6 with `/help` update.
- [ ] Task 14 contact calls: `call @h` → `phone.call(peer)` after gate; greet; transcripts → `contact_prompt`; `ContactReply` for that contact spoken and posted.
- [ ] Full CI set → PR 5.

### Tasks 15–16: Coda tools (PR 6)

**Files:** Create `crates/push-bridge/src/tools/{mod.rs,mcp.rs,endpoint.rs}`; modify `main.rs`/`cli.rs` (`tools-mcp` subcommand), `coda/backend.rs` (`serve_command` env `CODA_USER_MCP_DIR`, `.mcp.json` writer, `accepts_contacts`, `contact_prompt`), `core.rs` (tools listener → `Internal::Action`).
- [ ] Task 15: MCP server tests over `tokio::io::duplex`: `initialize` → capabilities.tools; `tools/list` lists 7 tools with JSON schemas; `tools/call` forwards to a fake endpoint and maps codes to `isError`; bad token refused.
- [ ] Task 16: `.mcp.json` merge (user servers kept, `push` replaced, file written in state dir), env set on spawn (FakeLauncher sees it), Coda contact prompt framing, steer while busy; endpoint listener in core feeding actions; `tools: false` disables.
- [ ] Full CI set → PR 6.

### Tasks 17–18: Docs and acceptance (PR 7)

- [ ] Task 17: `docs/agent-bridge.md` (actions, inbound, loop guard, echo commands, Coda tools, calls to contacts), `docs/calls.md` (agent calls to contacts, `started_by_agent`), main design doc (protocol numbers), `docs/deployment-configuration.md` if any env added, `CHANGELOG.md` Unreleased, spec §15 "As built".
- [ ] Task 18: `crates/push-smoke/tests/proactive_agents.rs` whole-stack per spec §14 (raw sockets / SDK like `agent_connections.rs`, two agents of one owner; bridge loop-guard part via the bridge crate tests if the smoke harness cannot run two bridges).
- [ ] Full CI set → PR 7 → merge → pull main → remove worktrees.
