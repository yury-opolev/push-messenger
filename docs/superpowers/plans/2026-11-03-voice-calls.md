# Voice calls Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 1:1 WebRTC audio calls signalled over the push.v1 WebSocket (phase 1), then
agents in calls through push-agent-bridge (phase 2), as eight independently releasable PRs.

**Architecture:** message-svc runs the call engine (policy, a pure state machine in
push-domain, Redis call state with compare-and-set Lua, a due-set sweeper, audit, metrics);
the gateway relays call frames and attaches/detaches calls-capable connections; auth-svc
serves the HTTP APIs (history, TURN credentials). The contract is the design spec
`docs/superpowers/specs/2026-11-03-voice-calls-design.md` (cited below as "spec §n").

**Tech Stack:** Rust 2024 (MSRV 1.85: no let-chains, no std APIs newer than 1.85), tokio,
tonic/prost, axum, redis 1.x (`ConnectionManager`, EVALSHA), sqlx/PostgreSQL, Helm,
proptest (dev only).

---

## Conventions for every PR

- Branch from `origin/main` in a new worktree; one PR per section below; CHANGELOG entry
  under `## Unreleased` / `### Added` (or `### Changed`).
- Before pushing, run what CI runs (`.github/workflows/ci.yml`), on Windows with
  `$env:CARGO_INCREMENTAL=0` and `-j 4` to avoid running out of memory:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`
  - `cargo test --workspace --locked -j 4` (Docker running: Redis and Postgres containers)
  - `cargo +1.85 build --workspace --locked -j 4` (install with `rustup toolchain install 1.85` if missing)
  - `cargo deny check` when Cargo.lock changed (`cargo install cargo-deny --locked`)
  - `cargo test --locked -p push-sdk-c -p push-sdk-ffi`; in `bindings/node`: `npm ci; npm test; npm run typecheck` when the SDK or bindings change
  - `pwsh deploy/helm/test-chart.ps1` when the chart changes (helm 3.16; ignore helm-diff plugin warnings, grep for `FAIL`)
- sqlx migrations are checksummed: never edit an existing one.
- No SDP, candidate, IP address or audio in any log line, metric label or audit line.
- Merge with `gh pr checks <n> --watch`, then `gh pr merge <n> --merge --delete-branch`.

---

# PR 1 — Protocol and gateway signalling

Ships: spec + this plan, the call frames, capability advertisement, the engine in
message-svc, the gateway relay, `calls.enabled` (default false) + `ringSeconds`,
`disconnectGraceSeconds`, `attemptsPerHour`, metrics, audit, tests. Request acceptance
covered: phase-1 protocol tests except history; "no SDP or candidate in logs or audit".

## File map (PR 1)

| File | Responsibility |
|---|---|
| `crates/push-proto/proto/push/v1/frame.proto` | call frames, enums, error codes, capabilities (spec §4) |
| `crates/push-proto/proto/push/internal/v1/messaging.proto` | `Call`, `CallAttach`, `CallDetach`, `IdentifyResponse.capabilities`, `Notification.call` |
| `crates/push-proto/src/codec.rs` | round-trip tests for call frames; proptest decode |
| `crates/push-domain/src/calls/mod.rs` | public API of the call domain |
| `crates/push-domain/src/calls/sdp.rs` | SDP and ICE validators (D14, D15) + proptests |
| `crates/push-domain/src/calls/machine.rs` | `CallRecord`, `CallEvent`, `transition` (spec §3) |
| `crates/push-domain/src/calls/policy.rs` | `Messaging::call_permission` (D11, D12) |
| `crates/push-domain/src/calls/tests.rs` | table-driven machine tests, policy tests |
| `crates/push-domain/src/store.rs`, `memory.rs`; `crates/push-store/src/store.rs` | `Tx::agent_owner` |
| `crates/push-testkit/src/contract/users.rs` | contract test for `agent_owner` |
| `crates/push-message-svc/src/calls/mod.rs` | `CallService`: load → transition → apply → publish; attach/detach; sweeper |
| `crates/push-message-svc/src/calls/redis.rs` | `RedisCalls`: Lua scripts (load, apply, claim, attach, detach), liveness read |
| `crates/push-message-svc/src/calls/wire.rs` | proto ↔ domain conversion for call frames, `CallSignal` notifications |
| `crates/push-message-svc/src/metrics.rs` | hand-rolled Prometheus text (D17) |
| `crates/push-message-svc/src/config.rs`, `grpc.rs`, `server.rs`, `lib.rs` | config, RPC wiring, sweeper task, `/metrics` |
| `crates/push-message-svc/tests/calls_redis.rs` | scripts against real Redis |
| `crates/push-gateway/src/config.rs`, `session.rs`, `server.rs`, `hub.rs`, `limits.rs` | relay, capability, attach/detach, device targeting, attempts limit |
| `crates/push-gateway/tests/calls.rs` | two gateways + two message-svc + Redis integration |
| `crates/push-gateway/tests/gateway.rs` | `FlakySync` delegates the three new RPCs |
| `deploy/helm/push-messenger/values.yaml`, `templates/gateway.yaml`, `templates/message-svc.yaml`, `templates/_helpers.tpl` or `validate.yaml`, `deploy/helm/test-chart.ps1` | `calls.*` values, env, validation, checks |
| `docs/calls.md` | protocol reference for client developers (frames, codes, capability, config); PR 5 completes it |
| `docs/deployment-configuration.md`, `CHANGELOG.md`, `README.md` | config reference rows, release note, docs index |

### Task 1.1: Protocol

**Files:** `frame.proto`, `messaging.proto`, `crates/push-proto/src/codec.rs`,
`crates/push-proto/Cargo.toml`, root `Cargo.toml` (`proptest = "1"` in
`[workspace.dependencies]`).

- [ ] **Step 1: frame.proto.** In `Frame.body` add (with the section comments):

```proto
    // Calls, client to server (docs/calls.md).
    CallInvite call_invite = 11;
    CallAccept call_accept = 12;
    CallDecline call_decline = 13;
    CallCancel call_cancel = 14;
    CallIce call_ice = 15;
    CallHangup call_hangup = 16;
    CallRenegotiate call_renegotiate = 17;
    // Calls, server to client.
    CallIncoming call_incoming = 44;
    CallAccepted call_accepted = 45;
    CallTaken call_taken = 46;
    CallEnded call_ended = 47;
    CallError call_error = 48;
    CallSync call_sync = 49;
    CallIce peer_call_ice = 50;
    CallRenegotiate peer_call_renegotiate = 51;
  }
  // 18-31 are free for later client frames, 52-63 for later server frames.
```

`Identify`: `repeated Capability capabilities = 2;` `Ready`: `repeated Capability
capabilities = 4;`. Add the enums and messages of spec §4.2 verbatim, with field comments
written for a client developer (what each field means, limits: SDP ≤ 16384 bytes UTF-8,
candidate ≤ 512 bytes printable ASCII, `sdp_mid` ≤ 32, `username_fragment` ≤ 256, which
reasons are allowed in `CallDecline`/`CallHangup`, who receives each server frame — the
"Sent to" column of the request). Enum value names are prefixed (`CALL_END_REASON_MISSED`,
`CALL_MEDIA_AUDIO`, `CALL_SDP_TYPE_OFFER`, `CALL_DIRECTION_IN`, `CALL_STATUS_RINGING`).
Error codes 27–30 per spec §4.3 with comments.

- [ ] **Step 2: messaging.proto.** Spec §4.7 verbatim, with comments: `IdentifyResponse`
gains `repeated push.v1.Capability capabilities = 1;` ("the features message-svc serves;
empty from versions before calls"), the three RPCs, `CallRequest`, `CallReply`,
`CallDeviceRequest`, `CallDetachResponse`, `Notification.kind` gains
`CallSignal call = 7;`, and a comment block documenting the Redis call keys (spec §5),
like the presence block.

- [ ] **Step 3: tests in codec.rs.** Add `call_frames_survive_encoding` (one of each of
the 14 call bodies with every field set) and, in a `proptest!` block,
`decoding_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096))` and
`call_invites_round_trip(sdp in "[ -~]{0,2000}", ...)`. Add `proptest.workspace = true`
to `[dev-dependencies]` of push-proto.

- [ ] **Step 4:** `cargo test -p push-proto --locked` passes; `cargo build --workspace`
compiles (fix exhaustive matches if any). Commit `feat(proto): call frames, capabilities and error codes`.

### Task 1.2: Store — `Tx::agent_owner`

**Files:** `crates/push-domain/src/store.rs` (beside `is_agent`), `memory.rs`,
`crates/push-store/src/store.rs`, `crates/push-testkit/src/contract/users.rs`.

- [ ] **Step 1: contract test** (testkit, runs on memory and PostgreSQL):

```rust
/// An enrolled agent names its owner; an owner, a stranger and an unknown user name none.
pub async fn agent_owner_names_the_owner<S: Store>(store: &S) { /* insert owner+agent,
   record_agent_owner(agent, owner); assert agent_owner(agent)==Some(owner),
   agent_owner(owner)==None, agent_owner(UserId::new())==None */ }
```

Register it where the other `users` contract tests are listed (follow `is_agent`'s test).

- [ ] **Step 2:** trait method `async fn agent_owner(&mut self, user: UserId) ->
DomainResult<Option<UserId>>;` (doc: "The owner recorded for an owned agent; `None` for
anyone else"). Memory: `self.work.agent_owners.get(&user).copied()`. Postgres:
`SELECT owner_user_id FROM messaging.agent_owner WHERE agent_user_id = $1`
(`fetch_optional`, `store_error`).

- [ ] **Step 3:** `cargo test -p push-domain -p push-store -p push-testkit --locked`;
commit `feat(store): read an agent's owner`.

### Task 1.3: Domain — SDP and ICE validation

**Files:** create `crates/push-domain/src/calls/mod.rs`, `calls/sdp.rs`; `lib.rs`
(`pub mod calls;`); push-domain `[dev-dependencies] proptest.workspace = true`.

API:

```rust
pub const MAX_SDP_BYTES: usize = 16 * 1024;
pub const MAX_CANDIDATE_BYTES: usize = 512;
pub const MAX_SDP_MID_BYTES: usize = 32;
pub const MAX_UFRAG_BYTES: usize = 256;
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallInputError { /* Empty, TooLarge{limit}, NotSdp(&'static str), Video, NotAudio(String kind), BadCandidate(&'static str) */ }
/// Size, line grammar (`<letter>=<value>` lines separated by CRLF or LF, first line `v=0`,
/// no control characters but tab), at least one `m=audio`, no other `m=` kind.
pub fn check_sdp(sdp: &str) -> Result<(), CallInputError>;
/// `candidate`: empty only with end_of_candidates; else starts with `candidate:`,
/// printable ASCII (0x20..=0x7e), ≤ 512 bytes; sdp_mid ≤ 32 and ufrag ≤ 256 printable.
pub fn check_candidate(c: &CandidateInput<'_>) -> Result<(), CallInputError>;
```

Error messages never echo the input (no SDP in errors). The `NotAudio` kind string is the
`m=` media token only, truncated to 16 chars.

- [ ] **Step 1: failing tests** in `sdp.rs` `#[cfg(test)]`: a real Chrome audio offer
(fixture string with `a=candidate` lines) passes; 16384 bytes ok, 16385 `TooLarge`;
`m=video` → `Video`; `m=application` → `NotAudio`; no `m=` → `NotSdp`; NUL byte → `NotSdp`;
first line not `v=0` → `NotSdp`; candidate cases. proptest: `check_sdp` never panics on
any string ≤ 20000 chars; any SDP with an inserted `m=video` line is refused; any valid
fixture padded beyond 16384 is refused.
- [ ] **Step 2:** implement; **Step 3:** `cargo test -p push-domain calls::sdp`; commit
`feat(domain): call SDP and ICE validation`.

### Task 1.4: Domain — the state machine

**Files:** `calls/machine.rs`, `calls/tests.rs`.

Types (all `Serialize`/`Deserialize` with `#[serde(default)]` where optional, D19):

```rust
pub enum CallState { Ringing, Active }
pub enum EndReason { Completed, Missed, Declined, Cancelled, Busy, Failed, Unreachable }
pub enum Side { Caller, Callee }
pub struct CallRecord {
    pub v: u32, pub call_id: Uuid, pub caller: UserId, pub caller_device: DeviceId,
    pub callee: UserId, pub state: CallState, pub offer: Option<String>,
    pub ringing_devices: Vec<DeviceId>, pub callee_device: Option<DeviceId>,
    pub created_ms: i64, pub ring_deadline_ms: i64, pub answered_ms: Option<i64>,
    pub caller_away_ms: Option<i64>, pub callee_away_ms: Option<i64>,
    pub ice_caller: u32, pub ice_callee: u32, pub reneg_caller: u32, pub reneg_callee: u32,
    pub pending_ice: Vec<Candidate>, pub due_ms: i64,
}
pub struct Candidate { pub candidate: String, pub sdp_mid: Option<String>,
    pub sdp_mline_index: Option<u32>, pub end_of_candidates: bool,
    pub username_fragment: Option<String> }
pub struct Timing { pub ring_ms: i64, pub grace_ms: i64, pub liveness_ms: i64 } // 45000, 30000, 15000
pub enum CallEvent {
    Accept { device: DeviceId, answer: String },
    Decline { device: DeviceId, busy: bool },
    Cancel { device: DeviceId },
    Hangup { device: DeviceId, failed: bool },
    Ice { device: DeviceId, candidate: Candidate },
    Renegotiate { device: DeviceId, sdp: String, offer: bool },
    Attach { device: DeviceId },
    Detach { device: DeviceId },
    Tick { gone: Vec<Side> },          // sides whose device the liveness read found gone
}
pub enum Refusal { NotFound, AlreadyAnswered, Limit }   // → CALL_NOT_FOUND / ALREADY_ANSWERED / CALL_LIMIT
pub enum Delivery {   // what to publish, to whom (devices of one user)
    Incoming { to: Vec<DeviceId> }, Accepted { to: DeviceId, answer: String, device: DeviceId },
    Taken { to: Vec<DeviceId> }, Ended { user: UserId, to: Vec<DeviceId>, reason: EndReason, duration_s: Option<u32> },
    Ice { user: UserId, to: DeviceId, candidate: Candidate },
    Renegotiate { user: UserId, to: DeviceId, sdp: String, offer: bool },
}
pub struct Outcome { pub record: Option<CallRecord>, pub deliveries: Vec<Delivery>,
                     pub ended: Option<EndReason>, pub answered: bool }
pub fn start(call_id, caller, caller_device, callee, offer, ringing: Vec<DeviceId>, now_ms, &Timing) -> (CallRecord, Vec<Delivery>);
pub fn transition(record: &CallRecord, user: Option<UserId>, event: CallEvent, now_ms: i64, t: &Timing) -> Result<Outcome, Refusal>;
pub fn sync_view(record: &CallRecord, user: UserId, device: DeviceId, now_ms: i64) -> Option<CallView>; // CallState of spec §4.2
```

`user` is the sender for frame events (`None` for Attach/Detach/Tick, which carry the
device whose user the service already matched). Every row of spec §3.3 is one test case.
Constants `MAX_ICE_PER_SIDE = 200`, `MAX_PENDING_ICE = 32`, `MAX_RENEGOTIATIONS = 20`.

- [ ] **Step 1: failing table test** `every_transition_of_the_spec` iterating a
`Vec<Case { name, record: fn() -> CallRecord, user, event, now, expect }>` covering all
rows of §3.3, plus named tests: `two_devices_accepting_at_once` (apply accept to the same
record twice in sequence: second gets `AlreadyAnswered`), `cancel_racing_accept` (both
orders), `hangup_racing_reconnect` (hangup ends; attach on `None` record = no call),
`tick_after_attach_only_reschedules`, `wrong_role_is_not_found` (caller accepts, other
user's device, non-ringing callee device, callee ICE while ringing, renegotiate while
ringing), `late_accept_is_missed`, `duration_is_whole_seconds_from_answer`,
`buffered_ice_reaches_the_answerer_in_order`, `ice_cap_and_buffer_cap`,
`records_are_bounded` (serialized record of a ringing call with max offer and 32 max
candidates < 50 KiB).
- [ ] **Step 2:** implement; **Step 3:** `cargo test -p push-domain calls`; commit
`feat(domain): the call state machine`.

### Task 1.5: Domain — policy

**Files:** `calls/policy.rs`, tests in `calls/tests.rs`.

```rust
impl<S: Store, C: Clock, N: Notifier> Messaging<S, C, N> {
    /// Whether `caller` may call `callee` (spec D11, D12). `NotContact` otherwise;
    /// `Invalid(SelfTarget)` for oneself.
    pub async fn call_permission(&self, caller: UserId, callee: UserId) -> DomainResult<()>;
}
```

Rules in order: self → `Invalid`; either unknown → `NotContact`; a `Blocked` row either
direction → `NotContact`; `agent_owner(caller)` is `Some(o)` → allowed iff `o == callee`;
`agent_owner(callee)` is `Some(o)` → allowed iff `o == caller`; owner↔agent with no block
→ allowed; otherwise both directions `Accepted` required (`are_contacts`).

- [ ] Tests: `calls_policy_contacts_may_call`, `..._pending_or_strangers_may_not`,
`..._blocks_either_way_refuse`, `..._unknown_callee_looks_like_a_stranger`,
`agent_calls_only_its_owner`, `only_the_owner_calls_the_agent`,
`owner_and_agent_without_contact_rows_may_call`. Use `InMemoryStore` +
`enroll_agent` as existing domain tests do. Commit `feat(domain): who may call whom`.

### Task 1.6: message-svc — Redis call store

**Files:** `crates/push-message-svc/src/calls/redis.rs`, `calls/mod.rs` (module skeleton),
`lib.rs` exports, `tests/calls_redis.rs`.

Keys (spec §5): `calls:{calls}:call:<id>`, `calls:{calls}:user:<id>`,
`calls:{calls}:devices:<user>`, `calls:{calls}:due`. Script runner: copy the `Script`
pattern of `push-gateway/src/presence.rs` (EVALSHA, reload on NOSCRIPT, retry once on a
dropped connection) into `calls/redis.rs` (keys/args generic).

```lua
-- LOAD: KEYS[1]=call → {ver or false, rec or false, now_ms}
local t = redis.call('TIME'); local now = t[1]*1000 + math.floor(t[2]/1000)
local v = redis.call('HMGET', KEYS[1], 'ver', 'rec')
return {v[1], v[2], now}
```

```lua
-- APPLY: KEYS = call, due, then user keys. ARGV: 1 expected ver ('' = create),
-- 2 new rec ('' = end), 3 ttl ms, 4 due ms, 5 call id, 6 n_lock (user keys 3.. that must be
-- free or already this call; the first is the caller), 7 n_release (next user keys,
-- deleted when they name this call).
local ver = redis.call('HGET', KEYS[1], 'ver')
if ARGV[1] == '' then
  if ver then return 'EXISTS' end
else
  if ver ~= ARGV[1] then return 'CONFLICT' end
end
local nlock, nrel = tonumber(ARGV[6]), tonumber(ARGV[7])
if ARGV[1] == '' then
  for i = 1, nlock do
    local cur = redis.call('GET', KEYS[2 + i])
    if cur and cur ~= ARGV[5] then
      if i == 1 then return 'IN_CALL' else return 'BUSY' end
    end
  end
end
if ARGV[2] == '' then
  redis.call('DEL', KEYS[1]); redis.call('ZREM', KEYS[2], ARGV[5])
  for i = 1, nlock + nrel do
    if redis.call('GET', KEYS[2 + i]) == ARGV[5] then redis.call('DEL', KEYS[2 + i]) end
  end
  return 'OK'
end
local nv = (tonumber(ver) or 0) + 1
redis.call('HSET', KEYS[1], 'ver', nv, 'rec', ARGV[2]); redis.call('PEXPIRE', KEYS[1], ARGV[3])
for i = 1, nlock do
  local cur = redis.call('GET', KEYS[2 + i])
  -- An update refreshes the locks it still holds; it never takes another call's.
  if (not cur) or cur == ARGV[5] then redis.call('SET', KEYS[2 + i], ARGV[5], 'PX', ARGV[3]) end
end
redis.call('ZADD', KEYS[2], ARGV[4], ARGV[5])
return 'OK'
```

```lua
-- CLAIM: KEYS[1]=due; ARGV 1 max, 2 lease ms → {now_ms, id1, id2, …}
local t = redis.call('TIME'); local now = t[1]*1000 + math.floor(t[2]/1000)
local ids = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', now, 'LIMIT', 0, tonumber(ARGV[1]))
for _, id in ipairs(ids) do redis.call('ZADD', KEYS[1], 'XX', now + tonumber(ARGV[2]), id) end
table.insert(ids, 1, now); return ids
```

ATTACH: `HSET devices device conn; PEXPIRE 86400000`. DETACH: `if HGET == conn then HDEL; return 1 else return 0`.
Liveness (outside scripts): `HGETALL devices:<user>`, then pipelined
`ZSCORE presence:{user}:conns <conn>` (`push_proto::presence_conns_key`) compared with
the `now_ms` the caller passes.

```rust
pub struct RedisCalls { connection: ConnectionManager }
pub enum Applied { Ok, Conflict, InCall, Busy, Exists }
impl RedisCalls {
    pub async fn load(&self, call: Uuid) -> RedisResult<(Option<(u64, String)>, i64)>;
    pub async fn now(&self) -> RedisResult<i64>;                 // TIME
    pub async fn user_call(&self, user: UserId) -> RedisResult<Option<Uuid>>;
    pub async fn apply(&self, w: &Write<'_>) -> RedisResult<Applied>;
    pub async fn claim(&self, max: u32, lease_ms: i64) -> RedisResult<(i64, Vec<Uuid>)>;
    pub async fn attach(&self, user: UserId, device: DeviceId, conn: Uuid) -> RedisResult<()>;
    pub async fn detach(&self, user: UserId, device: DeviceId, conn: Uuid) -> RedisResult<bool>;
    pub async fn connection_of(&self, user: UserId, device: DeviceId) -> RedisResult<Option<Uuid>>;
    pub async fn live_devices(&self, user: UserId, now_ms: i64) -> RedisResult<Vec<DeviceId>>;
    pub async fn live_count(&self) -> RedisResult<u64>;          // ZCARD due
    pub async fn zrem_due(&self, call: Uuid) -> RedisResult<()>; // a due entry without a record
}
pub struct Write<'a> { pub call: Uuid, pub expected: Option<u64>, pub record: Option<&'a str>,
    pub ttl_ms: i64, pub due_ms: i64, pub lock: Vec<UserId>, pub release: Vec<UserId> }
```

- [ ] Tests (`tests/calls_redis.rs`, real Redis like `tests/redis.rs`):
`create_locks_both_users_and_a_second_create_is_busy_or_in_call`,
`a_stale_version_is_a_conflict`, `ending_releases_the_locks_and_the_due_entry`,
`claim_leases_due_calls_for_five_seconds`, `keys_expire_without_message_svc` (ttl 200 ms,
sleep, all gone), `detach_only_removes_the_matching_connection`,
`live_devices_need_a_presence_lease`. Commit `feat(message-svc): Redis call store`.

### Task 1.7: message-svc — config, metrics, CallService, gRPC

**Files:** `config.rs`, `metrics.rs`, `calls/mod.rs`, `calls/wire.rs`, `grpc.rs`,
`server.rs`, `lib.rs`, `notify.rs` (reuse `RedisNotifier::send` for `CallSignal`
messages: make a `pub(crate) async fn publish_raw(&self, &[(String, Vec<u8>)])`).

Config (doc table rows added): `PUSH_CALLS_ENABLED` (`1`/`0`, default 0; anything else
refused), `PUSH_CALLS_RING_SECONDS` (10–120, default 45), `PUSH_CALLS_DISCONNECT_GRACE_SECONDS`
(5–120, default 30). `Config.calls: CallsConfig { enabled, ring, grace }`. Unit tests for
defaults and each range error (message names the variable).

`CallService<S,C,N>` (in `calls/mod.rs`): holds `Arc<Messaging<..>>`, `RedisCalls`,
`RedisNotifier`, `Timing`, `Arc<Metrics>`.

- `invite(user, device, CallInvite) -> Result<Option<CallEnded>, Status>`: validate ids
  (UUIDv7 via `Uuid::get_version_num() == 7`), not self, media exactly `[AUDIO]` (VIDEO →
  `INVALID` "video is not available"), `check_sdp`; `call_permission` (map
  `NotContact`/`UnknownUser` → `NOT_CONTACT`); `now`; `live_devices(callee)` → empty →
  audit+metric `unreachable`, reply `CallEnded{unreachable}`; `machine::start`; `apply`
  (create, lock `[caller, callee]`): `InCall` → `IN_CALL`, `Busy` → audit+metric busy,
  reply `CallEnded{busy}`, `Exists` → `INVALID` ("call_id is already used"); `Ok` →
  publish deliveries, audit `call.started`, metric.
- `frame(user, device, event, call_id)` for accept/decline/cancel/ice/hangup/renegotiate:
  validate inputs (`check_sdp` for answer/renegotiate sdp, `check_candidate`); loop ≤ 5:
  `load`; no record → `CALL_NOT_FOUND`; record's caller/callee must be `user`, else
  `CALL_NOT_FOUND`; `transition`; `Refusal` → `CALL_NOT_FOUND`/`ALREADY_ANSWERED`/
  `CALL_LIMIT`; `apply` with `expected = ver` (on end: `record: None`, `release:
  [caller, callee]`; else `lock: [caller, callee]` to refresh TTLs); `Conflict` → retry;
  publish; on end audit `call.ended` + metrics; on answer setup histogram. Exhausted →
  `UNAVAILABLE` retryable.
- `attach(user, device, conn) -> CallSync`: `attach`; `user_call(user)`; if a record:
  `transition(Attach)` + apply (retry loop) ; return `sync_view` list (0 or 1 entries).
- `detach(user, device, conn)`: `detach` returns whether it matched; if so and the user
  has a call: `transition(Detach)` + apply.
- `sweep_once()`: `claim(32, 5000)`; per id: load; none → `ZREM` nothing to do (apply
  can't: call `redis.zrem_due(id)`); record: compute `gone` sides for active calls
  (party device's `connection_of` + presence lease read) ; `transition(Tick)`; apply;
  publish; audit/metrics on end.
- `run_sweeper(shutdown)`: every 500 ms `sweep_once`, errors logged at warn without
  record contents.
- Audit (D18):

```rust
tracing::info!(target: "security", event = "call.started", call_id = %id, caller = %caller.0, callee = %callee.0, "a call started ringing");
tracing::info!(target: "security", event = "call.ended", call_id = %id, caller = %caller.0, callee = %callee.0, reason = reason.as_str(), duration_s, answered, "a call ended");
```

- Publishing (`calls/wire.rs`): each `Delivery` → `Notification { kind: Call(CallSignal
  { device_ids, frame }) }` on `notify_channel(user)`; one pipeline per outcome, awaited
  before the RPC answers. `expires_at` = `ring_deadline_ms * 1000`, `expires_in_ms` =
  deadline − now.
- Metrics (`metrics.rs`): `Metrics` with `AtomicU64` counters, a fixed-bucket histogram
  (buckets 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5 s), `render(live: u64) ->
  String` in Prometheus text format 0.0.4 with `# HELP`/`# TYPE` lines (names spec §11).
  Unit test renders every family once.
- gRPC (`grpc.rs`): `MessagingApi::with_calls(self, Arc<dyn CallsApi>)` (a small object-safe
  trait implemented by `CallService` so the generic `MessagingApi` stays generic);
  `identify` returns `capabilities: [CAPABILITY_CALLS]` iff calls enabled; `call`,
  `call_attach`, `call_detach` delegate, or answer `FAILED_PRECONDITION` with
  `NOT_AVAILABLE` when calls are disabled (no `CallService`).
- server.rs: build `CallService` when enabled; spawn `run_sweeper` in the `servers`
  JoinSet supervision (it exits on shutdown with `Ok`); health router gains `GET /metrics`
  (always served; call families only when enabled).

- [ ] Tests (unit, in-module with a real Redis where needed via testcontainers, else
  through Task 1.8's gateway tests): config, metrics render, wire conversion round trips
  (`a_delivery_becomes_a_signal_for_its_devices`). Commit
  `feat(message-svc): the call engine`.

### Task 1.8: Gateway

**Files:** `config.rs`, `limits.rs`, `session.rs`, `server.rs`, `hub.rs`,
`tests/gateway.rs` (FlakySync delegates `call`, `call_attach`, `call_detach`).

- Config: `PUSH_CALLS_ENABLED` (`flag`), `PUSH_CALLS_ATTEMPTS_PER_HOUR` (1–1000, default
  30) → `Config.calls: bool`, `Config.call_attempts_per_hour: u32`; doc table rows; tests.
  Update every `Config { … }` literal (tests/gateway.rs, hub tests, smoke/load if any).
- Limits: `Action::CallInvite` (name `"call"`), scope user, limit from config: add
  `Limits::with_call_attempts(n)` storing a `Limit`; `Action::limits()` stays static for
  the others, `Limits::check` uses the stored limit for `CallInvite`. Unit test with the
  existing fake counter.
- Session: `Step::Identify(String)` → `Step::Identify { token, capabilities: Vec<i32> }`;
  `Session::identified(claims, heartbeat, offered: &[Capability], calls_active: bool)` puts
  `offered` in `Ready.capabilities` and remembers `calls_active`; call frames map to
  `Step::Call(CallFrame)` (`enum CallFrame { Invite(CallInvite), Accept(..), … }` with
  `fn call_id(&self) -> &[u8]`) when calls-active, else `Step::Reply(CallError{call_id,
  NOT_AVAILABLE, "calls are not available on this connection"})`. Server frames sent by a
  client (44–51) stay `BAD_FRAME`. Unit tests for both paths and the Ready capabilities.
- Server:
  - `admit` returns the `IdentifyResponse`; `offered = config.calls && response has
    CAPABILITY_CALLS` → `[Capability::Calls]` else `[]`; `calls_active = offered &&
    client advertised`. On `ReAuth`, ignore capabilities (unchanged).
  - After `Ready` (and the first `deliver`), if calls-active: `CallAttach{user, device,
    connection}` → send `Body::CallSync`. On failure: retry with `RetryDelay` from the
    select loop (`attach_retry_at`, like `retry_at`), log warn.
  - `Step::Call(frame)`: for `Invite`, `limits.check(Action::CallInvite, user)`:
    `Limited` → `CallError{RATE_LIMITED, retryable, retry_after_s}`, `Unavailable` →
    `CallError{UNAVAILABLE, retryable, retry_after_s: 1}` (fail closed). Then
    `messaging.call(CallRequest)`; `Ok(reply)` with `ended` → send `Body::CallEnded`;
    `Err(status)` → `end_if_over`, then `CallError{call_id, code, message, retryable}` from
    `error_from_status` (`UNIMPLEMENTED` → `UNAVAILABLE`).
  - On connection end (any reason, including takeover), if calls-active and attached:
    spawn `CallDetach` in `app.sessions` (2 s timeout), like `idle_on_close`.
  - Never log call frames or their fields.
- Hub: `Member` gains `device: Uuid` (from `watch_device`); `server_frame` maps
  `Kind::Call(signal)` → `None`, and `dispatch` handles `Kind::Call` separately: decode
  target `device_ids`, forward the frame body (`CallIncoming`→`Body::CallIncoming`, …,
  `ice`→`Body::PeerCallIce`, `renegotiate`→`Body::PeerCallRenegotiate`) only to members of
  `notify:{user}` whose `device` is listed. Unit test
  `a_call_signal_reaches_only_the_named_devices`.
- [ ] Commit `feat(gateway): relay calls`.

### Task 1.9: Gateway integration tests (two replicas)

**File:** `crates/push-gateway/tests/calls.rs` (copy the stack helpers of
`tests/gateway.rs`; do not share a module unless trivial).

Stack: one Redis container; one `Arc<Messaging<InMemoryStore, SystemClock,
RedisNotifier>>`; two message-svc gRPC servers (`MessagingApi::with_presence(..)
.with_calls(..)`, each with its own `CallService` and sweeper, ring 2 s, grace 1 s); two
gateways (`calls: true`, attempts 5), gateway A → message-svc 1, gateway B → message-svc 2.
Clients identify with `capabilities: [CAPABILITY_CALLS]`. Helpers: `ready_calls(stack, gw,
user, device) -> Socket` (asserts `Ready.capabilities == [CALLS]` then reads `CallSync`),
`next_call(socket)` skipping `PresenceEvent`/other non-call frames, a fixture offer/answer
SDP containing the marker `SDP-MARKER-7f3a` and a candidate with `203.0.113.77`.

Tests (names are referenced by the spec's threat model):
- [ ] `invite_rings_every_device_first_accept_wins_others_get_taken` (callee phone on A,
  laptop on B; caller on A; laptop accepts; caller gets `CallAccepted{answered_device_id}`;
  phone gets `CallTaken`; late accept from phone → `CallError{ALREADY_ANSWERED}`)
- [ ] `ringing_devices_never_get_caller_candidates` (caller ICE before accept; only the
  answerer receives them, in order, after accepting)
- [ ] `answer_and_ice_only_reach_the_party_device`
- [ ] `decline_ends_for_everyone`, `decline_busy_reports_busy`, `cancel_ends_ringing`,
  `cancel_after_accept_is_already_answered`
- [ ] `no_answer_is_missed_after_the_ring` (ring 2 s; both sides `CallEnded{missed}`)
- [ ] `no_capable_device_is_unreachable_at_once` (callee online without capability; and
  offline)
- [ ] `a_callee_in_a_call_or_ringing_is_busy`, `a_caller_has_one_call_at_a_time`
- [ ] `disconnect_beyond_the_grace_fails_the_call` (drop the answerer's socket; both get
  `failed` within grace + sweep)
- [ ] `reconnect_within_the_grace_keeps_the_call` (reconnect on the other gateway; its
  `CallSync` lists the call active with `this_device`; renegotiate relays both ways)
- [ ] `a_stranger_or_blocked_user_cannot_call` (`NOT_CONTACT` for stranger, pending,
  blocked either way, unknown id)
- [ ] `agent_calls_only_its_owner` (enroll agent; agent→owner ok; agent→other contact and
  other→agent `NOT_CONTACT`)
- [ ] `another_users_device_cannot_touch_the_call` (third user sends accept/hangup/ice with
  the call id → `CALL_NOT_FOUND`)
- [ ] `oversize_and_video_offers_are_refused` (16385-byte SDP, `m=video`, VIDEO media)
- [ ] `ice_frames_are_capped_per_side` (201st → `CALL_LIMIT`)
- [ ] `attempts_per_hour_are_limited` (6th invite → `RATE_LIMITED` with `retry_after_s`)
- [ ] `calls_are_not_offered_when_message_svc_has_them_off` (a third message-svc without
  calls behind a third gateway: `Ready` without capabilities, frames `NOT_AVAILABLE`)
- [ ] `no_sdp_or_candidate_in_logs_or_audit`: install (once, `OnceLock`) a global
  `tracing_subscriber` at TRACE writing JSON to a shared `Vec<u8>`; run a full call; assert
  the buffer contains `call.started` and `call.ended` but neither `SDP-MARKER-7f3a` nor
  `203.0.113.77` nor `candidate:`.

Run: `cargo test -p push-gateway --test calls --locked -j 4` (Docker). Commit
`test(gateway): calls across two replicas`.

### Task 1.10: Chart, docs, changelog

- [ ] values.yaml: top-level block

```yaml
# Voice calls (docs/calls.md). Off by default; enable once every gateway and message-svc
# replica runs a version with calls.
calls:
  enabled: false
  # How long a call rings before it ends as missed: 10 to 120 seconds.
  ringSeconds: 45
  # How long a call survives a dropped connection of either side: 5 to 120 seconds.
  disconnectGraceSeconds: 30
  # Call attempts per user per hour, refused or not: 1 to 1000.
  attemptsPerHour: 30
```

- [ ] gateway.yaml env `PUSH_CALLS_ENABLED` (`ternary "1" "0"`), `PUSH_CALLS_ATTEMPTS_PER_HOUR`;
  message-svc.yaml env `PUSH_CALLS_ENABLED`, `PUSH_CALLS_RING_SECONDS`,
  `PUSH_CALLS_DISCONNECT_GRACE_SECONDS`.
- [ ] validation (`templates/validate.yaml` via a `push.checkCalls` helper in
  `_helpers.tpl`): `enabled` must be a boolean; each integer in range, messages exactly
  "calls.ringSeconds must be an integer from 10 to 120" etc.
- [ ] test-chart.ps1 checks: defaults render `PUSH_CALLS_ENABLED "0"` on both;
  `--set calls.enabled=true` renders `"1"` on both; each out-of-range value fails with its
  message.
- [ ] `docs/calls.md`: status note (protocol available from this release, TURN in a later
  release), frame table with numbers, capability rules, error codes, the state machine
  summary, the client rules of spec §4.5, configuration rows. Link the spec.
- [ ] `docs/deployment-configuration.md`: `calls.*` rows; `README.md` docs list; CHANGELOG
  `### Added` entry "**Voice calls (signalling)**: …" naming frames 11–17/44–51, error
  codes 27–30, `CAPABILITY_CALLS`, config keys, default off.
- [ ] Commit `docs: voice call signalling`.

### Task 1.11: Verification and PR

- [ ] Run every CI command (Conventions). `cargo deny check` (proptest added).
- [ ] Whole-branch review (requesting-code-review), fix findings.
- [ ] Push, `gh pr create`, `gh pr checks --watch`, merge.

---

# PR 2 — Call history

Spec §9. Acceptance: "history and retention" (phase-1 list), T11.

- **Task 2.1 Store:** migration `crates/push-store/migrations/0008_call_history.sql`
  (table + index of spec §9, grants follow `0007`); `Tx` methods
  `insert_call_history(rows: &[CallHistoryRow])` (`ON CONFLICT DO NOTHING`),
  `call_history(user, before: Option<(DateTime, Uuid)>, since: Option<..>, limit)`,
  `hide_call(user, call_id) -> bool`, `delete_call_history_before(cutoff, batch) -> u64`;
  memory + Postgres + testkit contract tests (`history_is_private_and_hides_per_side`,
  `pages_are_newest_first_and_stable`, `retention_deletes_only_old_rows`).
- **Task 2.2 Domain/engine:** `CallService` writes the rows when a call ends (end outcomes
  and immediate busy/unreachable; callee row except unreachable), after the Redis apply
  (idempotent). A failed insert is retried 3 times then logged (call already ended).
  Publishes `CallHistoryChanged` (frame 52) to both users' calls-active devices.
- **Task 2.3 gRPC:** new service `Calls` in messaging.proto on the directory listener with
  the directory token: `History(CallHistoryRequest) returns (CallHistoryPage)`,
  `Hide(HideCallRequest) returns (HideCallResponse)`.
- **Task 2.4 auth-svc:** routes `GET /v1/calls`, `DELETE /v1/calls/{call_id}` (access
  token; `limit` 1–100; cursor base64url of `ended_at_micros:call_id`), audit events
  `calls.history.read` not logged (reads), `calls.history.hidden` at info. Ingress auth
  route paths gain `/v1/calls`.
- **Task 2.5 Retention:** `calls.historyDays` (`PUSH_CALLS_HISTORY_DAYS`, 1–3650, default
  90), hourly sweep in message-svc; chart value + validation + test.
- **Task 2.6 Docs:** docs/calls.md history section; CHANGELOG.
- Tests: Postgres contract tests; auth-svc HTTP tests with a fake `Calls` client; gateway
  integration `history_lists_both_sides_and_hiding_is_per_side`.

# PR 3 — TURN credential API and coturn

Spec §8, T5, T6, T12. Acceptance: every "TURN" bullet.

- **Task 3.1 Config** in auth-svc: `PUSH_TURN_MODE` (`internal|external|stun-only`),
  `PUSH_TURN_TTL_SECONDS` (60–86400, default 600), `PUSH_TURN_STUN_URLS`,
  `PUSH_TURN_URLS`, `PUSH_TURN_PROVIDER` (`generic|cloudflare|twilio`),
  `PUSH_TURN_SHARED_SECRET_FILE`, `PUSH_TURN_CLOUDFLARE_KEY_ID`,
  `PUSH_TURN_CLOUDFLARE_TOKEN_FILE`, `PUSH_TURN_TWILIO_ACCOUNT_SID`,
  `PUSH_TURN_TWILIO_TOKEN_FILE`, `PUSH_TURN_PUBLIC_HOST`, `PUSH_TURN_TLS` (1/0).
- **Task 3.2 Minting:** `turn.rs` in push-auth-core: REST HMAC-SHA1 (`hmac` + `sha1`
  crates; check deny.toml licences), providers with a `reqwest` client and an in-memory
  cache (refresh 60 s before expiry; single-flight), `GET /v1/calls/ice-servers`; 503
  `turn_unavailable` when unconfigured or the provider fails; rate limit 60/h/user; never
  log credentials; metrics counters in auth-svc `/metrics` (add the same hand-rolled
  renderer).
- **Task 3.3 Chart:** `templates/coturn.yaml` (Deployment, Service with UDP+TCP and relay
  range, ConfigMap `turnserver.conf` rendered from values incl. every denied range of T5
  and `deniedPeerCidrs`, probes, NetworkPolicy), secret key `turn-shared-secret`
  (generated like the other keys; consumer-managed supported), values `calls.turn.*` of the
  request plus `internal.deniedPeerCidrs` (required), `internal.quotas: {user: 12, total:
  1200, maxBps: 512000}`, `internal.image` pinned by digest; validation messages naming
  the field; test-chart checks.
- **Task 3.4 Supply chain:** CI job `coturn-image`: `docker pull` the pinned digest, Trivy
  scan (`aquasecurity/trivy-action`, pinned SHA, fail on HIGH/CRITICAL with an ignore file
  for accepted CVEs), documented upgrade path in docs/calls.md.
- **Task 3.5 Tests:** HMAC known-answer test (coturn REST example); real coturn container
  (testcontainers `GenericImage` with the pinned image and a generated config): a TURN
  Allocate with minted credentials succeeds (use a minimal STUN/TURN client in the test:
  `turn-client` logic hand-written over UDP, ~150 lines, or the `stun_codec` crate if
  deny allows), an expired credential is refused, a CreatePermission to 10.0.0.1,
  127.0.0.1 and 169.254.169.254 is refused (403); cloudflare/twilio mocked with an axum
  stub (URL overridable by `PUSH_TURN_PROVIDER_BASE_URL`, test only) and cached (one
  upstream call for two requests); stun-only answers `relay:false`.
- **Task 3.6 Docs:** TURN modes, security trade-offs, AKS (public UDP LoadBalancer, NSG
  3478/udp+tcp, 5349/tcp, relay range udp, DNS) and local cluster notes; CHANGELOG.

# PR 4 — push-sdk Calls API (+ push-load)

Spec §10.1. Acceptance: "SDK: the Calls API and events"; load targets.

- **Task 4.1** `ClientConfig::calls(bool)`; `Identify.capabilities`; `Calls` handle and
  methods (frames sent like activity; local validation mirrors push-domain's validators —
  depend on `push_domain::calls::sdp` if push-sdk already depends on push-domain, else copy
  with a shared test vector file).
- **Task 4.2** `Event::Call(CallEvent)` from the engine (`engine.rs` `on_frame` arms for
  44–52); `Calls::available()` from `Ready.capabilities`.
- **Task 4.3** `ice_servers()`, `history()`, `hide()` over HTTPS (auth-svc base URL the SDK
  already uses).
- **Task 4.4** C and Node bindings: functions + JSON events; regenerate the C header
  (`$env:PUSH_SDK_BLESS_HEADER='1'; cargo test -p push-sdk-c --test header`);
  `docs/sdk-bindings.md` lists every new event (the ffi json test checks it).
- **Task 4.5** push-load `calls` scenario: N caller/callee pairs, invite → accept with
  fixture SDP, measures invite→`CallAccepted` p50/p95/p99, holds calls, reports Redis
  `INFO commandstats` deltas; README of push-load.
- Tests: engine unit tests per frame, fake-gateway round trips, binding JSON tests.

# PR 5 — Docs and browser recipe

Spec §10.2. Acceptance: "Docs" bullet (protocol part), browser smoke test.

- **Task 5.1** `docs/calls.md` complete: sequence diagrams (mermaid) for normal flow,
  decline, cancel, missed, busy, unreachable, failed (grace), reconnect + ICE restart,
  renegotiation glare; client rules; configuration reference for every `calls.*` value
  with its validation error; troubleshooting; retention; upgrade guide; interop manual
  checklist (Chromium, Edge, Firefox, Safari).
- **Task 5.2** `docs/examples/calls/index.html` + `calls.js`: minimal client over the
  WebSocket with protobuf (use `protobufjs` from a pinned CDN URL with SRI, or the
  generated TypeScript bindings if the repo ships them), `RTCPeerConnection` with
  `ice_servers()`, trickle ICE, perfect negotiation (callee polite).
- **Task 5.3** Playwright smoke (`docs/examples/calls/tests/`), Chromium with
  `--use-fake-device-for-media-stream --use-fake-ui-for-media-stream`, two contexts,
  stun-only and internal TURN (`iceTransportPolicy: relay`), against the minikube stack;
  CI job `browser-calls` (optional, `workflow_dispatch` + PRs touching calls docs).

# PR 6 — Phase 2: media helper

Spec D21, §10.1. Acceptance: "two push-sdk clients completing a call through coturn in CI
and exchanging audio frames".

- **Task 6.1** Feature `calls-media` on push-sdk: deps `str0m`, `audiopus` (vendored
  libopus), verify `cargo deny check` and record the feature's MSRV in the crate docs.
- **Task 6.2** `MediaCall`: str0m `Rtc` driven by a tokio UDP socket task; ICE servers from
  `ice_servers()`; Opus 48 kHz mono 20 ms; resampler 48 k ↔ 16 k; `incoming()`,
  `outgoing()`, `on_ended()`; hooks into `Calls` events (offer/answer/ICE/renegotiate).
- **Task 6.3** E2E test (`crates/push-sdk/tests/calls_media.rs`, `#[ignore]` unless
  `PUSH_CALLS_E2E=1`, run in a CI job with Docker): stack + coturn container, two SDK
  clients, relay-only ICE, 2 s of a 440 Hz tone each way, assert energy at 440 Hz in the
  received PCM, hang up, both `on_ended`.

## PR 6 refined (at implementation time)

Crate facts found on 2026-11-03: str0m 0.25 needs Rust 1.89 and always builds aws-lc-rs
(its `rust-crypto` backend still enables `dimpl/rcgen`, which pulls aws-lc-rs), so use
`default-features = false, features = ["aws-lc-rs"]` (no `examples`/rouille). `audiopus`
is unmaintained since 2021: use `opus` 0.4 (libopus 1.5 via `opusic-sys`, bundled with
cmake, BSD-3). str0m has no TURN client: use `turn-client-proto` 0.7 (sans-IO, UDP and
TCP, MIT/Apache). The E2E test lives in push-smoke (it owns the in-process stack), which
enables the feature as a **dev-dependency**, so the `msrv` job (`cargo build`, no dev
targets) never builds it.

File map (all under `crates/push-sdk/src/media/`, `#[cfg(feature = "calls-media")]`):

| File | Responsibility |
|---|---|
| `mod.rs` | public API: `MediaCall`, `Role`, `MediaOptions`, `PcmFrame`, `PcmStream`, `PcmSink`, `MediaEnded`, `MediaState`; command channel to the driver |
| `resample.rs` | polyphase FIR 48 k → 16 k and 16 k → 48 k, stateful across frames |
| `codec.rs` | Opus encoder/decoder (48 kHz mono, 20 ms) + resamplers: `Codec::encode(&PcmFrame)`, `decode(&[u8]) -> PcmFrame` |
| `ice.rs` | parse `IceServers` URLs (`stun:`, `turn:` udp/tcp; `turns:` skipped), local address, STUN binding (srflx) |
| `turn.rs` | TURN allocation over UDP or TCP with `turn-client-proto`: relay address, permissions, send/receive wrapping |
| `driver.rs` | the task: str0m `Rtc` + sockets + TURN + codec, 20 ms send tick, events → state |

- [x] **6.1** feature + deps; `cargo deny check` (dev-dep in push-smoke puts them in the
  graph); `cargo +1.85 build --workspace --locked` still passes.
- [x] **6.2** `resample.rs` with unit tests (length, 1 kHz tone kept, 12 kHz tone removed
  on the way down, round trip correlation).
- [x] **6.3** `codec.rs` with a round-trip test (440 Hz tone survives Opus).
- [x] **6.4** `ice.rs` URL parsing tests; STUN binding request/response codec tests.
- [x] **6.5** `turn.rs` + `driver.rs` + `mod.rs`; an in-process unit test: two
  `MediaCall` drivers wired with direct host candidates (no server) exchange a tone.
- [x] **6.6** `crates/push-smoke/tests/calls_media.rs` (`PUSH_CALLS_MEDIA=1`): the stack with
  calls on, two SDK clients with `MediaCall`, stun-only (host candidates) and
  `internal` TURN relay-only through the chart's coturn (shared helper moved to
  `tests/common`), 440 Hz / 880 Hz tones, Goertzel energy check, hang up, both ended
  `Completed`; CI job `calls-media`.
- [x] **6.7** docs/calls.md §19 media helper, CHANGELOG, spec §15 PR 6 notes, README of
  the feature in the crate docs (MSRV 1.89, cmake).

# PR 7 — Phase 2: bridge voice pipeline

Spec §10.3, D22. Acceptance: "the bridge answers its owner's call, transcribes an
utterance into a prompt, speaks a reply, barge-in works, and a voice 'approve' answers a
card"; "a stranger's call is refused"; fake STT/TTS.

- **Task 7.1** built-in template `agent.transcript@1` (push-domain builtins + docs).
- **Task 7.2** `crates/push-bridge/src/voice/`: `Stt`/`Tts` traits; providers `azure`
  (REST, key or Entra token), `local` (command-line STT/TTS), `none`, `fake` (fixtures);
  VAD; markdown-to-speech; sentence splitter; barge-in controller.
- **Task 7.3** wiring in core: incoming call from owner → answer (auto) → greeting →
  utterance → prompt/steer → reply → TTS; transcripts and activity labels; voice card
  answers.
- **Task 7.4** tests with `TestMessenger`/`TestBackend` and fake providers (recorded
  fixtures for barge-in and "approve"/"option two"/ambiguous); opt-in live smoke
  (`PUSH_BRIDGE_LIVE_SPEECH=1`).
- **Task 7.5** docs/agent-bridge.md voice section incl. what each provider receives.

## PR 7 refined (at implementation time)

Decisions taken on 2026-11-03 before coding:

- **Build.** push-bridge gets an optional feature `voice = ["push-sdk/calls-media"]`
  (Rust 1.89, CMake). Only the media adapter (`voice/media.rs`, `MediaPhone`) and its
  wiring in `main.rs` need it: the pipeline, providers, VAD, confirmations and the core
  integration always compile (Rust 1.85) and are tested by `cargo test -p push-bridge`
  through a `FakePhone`. The `msrv` job builds default features, so it never compiles
  the feature; push-smoke enables it as a **dev-dependency** for the E2E test (as PR 6
  did for `calls-media`). The release build of the bridge is `cargo build --release -p
  push-bridge --features voice`; without the feature the client does not advertise calls
  (the owner's app sees the agent as unreachable).
- **Answering in PR 7.** `calls.answer` accepts only `"auto"` (a call from the owner is
  answered; `"ask"` is refused by validation until PR 8). Anyone else's call is declined
  (the server refuses them first, `NOT_CONTACT`; the bridge declines as defence in depth),
  as is a second call while one runs (busy) and every call when `calls` is absent or
  `calls.stt.provider` is `none`.
- **Transcripts** are the new built-in `agent.transcript@1` (`speaker`: `owner`|`agent`,
  `text`; literal heading "🎤 voice"): the template always means a voice transcript, so
  D22's `voice: true` is implied rather than a param (tpl/1 requires every param to be
  shown). Masked like everything the bridge sends; in the peek as an owner text "🎤 …".
- **Voice answers to cards.** The bridge cannot answer its own card (the server refuses
  it), so a voice answer settles the request locally (as a typed answer does) and
  **edits the card** into a closed `agent.reply@1` "<card title>" / "🎤 Answered by
  voice: <label>." (an unanswered card may be edited by its sender; the new template has
  no interactive block, so the server refuses later answers to it). This replaces the
  spec's `via: "voice"` on an ANSWER body, which tpl/1's strict ANSWER format does not
  allow.
- **Providers** (no new crates: reqwest with rustls is already in the graph):
  `azure-speech` REST (STT short-audio `…/speech/recognition/conversation/cognitiveservices/v1`,
  TTS `…/cognitiveservices/v1` with `raw-16khz-16bit-mono-pcm`), a key from an
  environment variable (`key_env`, default `AZURE_SPEECH_KEY`, never in config.json) with
  `region`, or an Entra token from a command (default `az account get-access-token
  --resource https://cognitiveservices.azure.com --query accessToken -o tsv`, cached 30
  min) with the resource's custom-domain `endpoint` (and `resource_id` → `Bearer
  aad#<id>#<token>`); `local` STT: a whisper.cpp server `url` (`/inference`, multipart
  built by hand) or a `command` with `{wav}` (stdout is the transcript); `local` TTS: a
  `command` (text on stdin, raw s16le mono at `sample_rate` on stdout, as `piper
  --output_raw`) or a `url` (POST text, WAV back); `none`.
- **VAD** in-house: 20 ms frames, energy (dBFS) over an adaptive noise floor (+10 dB,
  above -55 dBFS), onset after 3 speech frames (60 ms) with 200 ms pre-roll; an utterance
  ends after `silence_ms` (700) of non-speech, is kept when it has `min_speech_ms` (300)
  of speech, and is cut at 30 s. Onset during TTS is the barge-in (sink `clear()`).
- **Speech text**: incremental, line-based Markdown stripping (fences → "I've put the
  code in the chat.", tables → "I've put a table in the chat.", headings/lists/quotes
  unmarked, emphasis and code spans unwrapped, links → their text, bare URLs → "a link"),
  split into sentences (max 300 characters) and spoken as they complete during a turn.
  After a barge-in the rest of the reply is not spoken; text after the owner's steer is.
- **Activity during a call** (no card open, not paused): working "speaking" > "listening"
  > "thinking" (a turn runs) > "on a call".
- **Tests**: fake STT recognises tone bursts by frequency (Goertzel) from a fixture
  table; fake TTS speaks a tone whose length follows the text and logs the texts. Fixtures
  in `crates/push-bridge/tests/fixtures/voice/*.json` (barge-in, approve, option two,
  ambiguous). Core tests through `FakePhone`; E2E `crates/push-smoke/tests/bridge_voice.rs`
  (`PUSH_BRIDGE_VOICE=1`, CI job `bridge-voice`): stack with calls and `agent-key`, the
  owner creates an agent, the bridge enrolls with a software key and runs with
  `SdkMessenger`, `TestBackend` and `MediaPhone`; the owner calls with `MediaCall`.
  Live smoke `crates/push-bridge/tests/live_speech.rs` (`PUSH_BRIDGE_LIVE_SPEECH=1` +
  provider variables), not in CI.

File map (`crates/push-bridge/src/voice/`):

| File | Responsibility |
|---|---|
| `mod.rs` | `SAMPLE_RATE`, `FRAME`, `Frame`, `VoiceSetup`, re-exports |
| `wav.rs` | WAV encode/decode, resampling, framing |
| `vad.rs` | `Vad`, `VadConfig`, `VadEvent` |
| `speech.rs` | `SpeechText` (Markdown → spoken sentences, incremental) |
| `confirm.rs` | `match_answer` (voice → card answer), `again` prompts |
| `provider.rs` | `Stt`, `Tts`, `SpeechError`, `build` from config |
| `azure.rs`, `local.rs`, `fake.rs` | providers |
| `phone.rs` | `Phone`, `Line`, `Speaker`, `LineControl`, `LineEnded`, `FakePhone` |
| `session.rs` | per-call pipeline task: VAD → STT, TTS queue → sink, barge-in |
| `media.rs` (feature `voice`) | `MediaPhone` over `push_sdk::media::MediaCall` |

Core: `core/voice.rs` (call events, session events, transcripts, voice answers, speech of
replies and cards, activity labels); config `calls` section in `config.rs`.

- [x] **7.1** `agent.transcript@1` + seeding counts + docs/rich-messages.md.
- [x] **7.2** `wav.rs`, `vad.rs`, `speech.rs`, `confirm.rs` (+ `CardPlan::title`/`spoken`).
- [x] **7.3** config `calls` + providers (`provider.rs`, `azure.rs`, `local.rs`, `fake.rs`).
- [x] **7.4** `phone.rs`, `session.rs`, `media.rs`, feature `voice`.
- [x] **7.5** `core/voice.rs` + core tests with fixtures.
- [x] **7.6** `main.rs` wiring, push-smoke E2E, CI job, live smoke.
- [x] **7.7** docs (agent-bridge.md voice, calls.md cross-ref, CHANGELOG, spec §15).

PR 8 then adds: `calls.answer: "ask"`, `notify_by_call` (outgoing calls through
`MediaPhone` as `Role::Caller`, spoken summary, missed → card stays, rate limit, DND), and
the tray's call state with Mute agent / Hang up (control channel).

# PR 8 — Phase 2: answering and calling policies

Spec §10.4. Acceptance: "the agent calls the owner after the timeout, and a missed call
falls back to the card".

- **Task 8.1** `calls.answer: ask` card flow.
- **Task 8.2** `calls.notify_by_call` (after_minutes, urgent, min_interval_minutes,
  do_not_disturb) with a clock-injected scheduler; spoken summary on connect; missed →
  card remains.
- **Task 8.3** tray: per-agent call state and Mute agent / Hang up (control channel
  methods `call_state`, `mute`, `hangup`; push-tray-core view model; window page).
- **Task 8.4** tests (paused tokio time), docs, CHANGELOG.

## PR 8 refined (at implementation time)

Decisions taken on 2026-11-04 (details and deviations in spec §15, "PR 8"):

- **Ask** is an `agent.question@1` card (Answer / Decline) in the owner's conversation,
  so on every owner device; not one of the agent's requests. The server's end of the
  ring or the bridge's own deadline closes it by an edit; an answered card keeps the
  owner's choice.
- **notify_by_call** in `config.rs` (`NotifyByCall`, field-named validation) →
  `voice::policy::{CallPolicy, NotifyPolicy, QuietHours, Verdict}` (pure; local time
  injected as a closure, `chrono::Local` by default). `VoiceSetup` gained `policy`.
  `after_minutes` ≤ 8 (cards expire at 10 minutes), `min_interval_minutes` ≥ 10.
- **Outgoing calls**: `Phone::call(peer)` (`MediaPhone`: `Role::Caller`; `FakePhone`:
  rings until `FakeCall::pick_up`); `core/calling.rs` (ask, dialing with buffered call
  events, greeting, unanswered fallback, withdraw, tray actions, call report).
- **Urgent**: `BackendEvent::AgentMessage::urgent` from Coda's `urgent` / `priority` /
  a label starting with "urgent" (Coda has no field yet).
- **Control channel**: `Status.call`, `call_changed`, `call_state`, `mute`, `hangup`,
  `no_call`; tray view models, menu line and window buttons.

- [x] **8.1** ask card flow + core tests.
- [x] **8.2** `notify_by_call`: policy module, config, `Phone::call`, dialing, greeting,
  fallback, rate limit, do-not-disturb, urgent; core tests.
- [x] **8.3** control channel (`Status.call`, `call_changed`, `call_state`, `mute`,
  `hangup`) and the tray (push-tray-core view models, supervisor, window, menu; Tauri
  shell wiring).
- [x] **8.4** E2E in `bridge_voice.rs`, docs (agent-bridge.md §13, §14, §18; calls.md),
  CHANGELOG, spec §15.

All eight PRs of this plan are complete.

---

## Request acceptance → PR map

| Acceptance item | PR / test |
|---|---|
| invite → ring all devices → first accept wins → CallTaken | 1 / `invite_rings_every_device_first_accept_wins_others_get_taken` |
| decline, cancel, missed 45 s, unreachable, busy | 1 / `decline_*`, `cancel_*`, `no_answer_is_missed_after_the_ring`, `no_capable_device_is_unreachable_at_once`, `a_callee_in_a_call_or_ringing_is_busy` |
| disconnect > 30 s failed; reconnect < 30 s continues | 1 / `disconnect_beyond_the_grace_fails_the_call`, `reconnect_within_the_grace_keeps_the_call` |
| not-a-contact and block refusals | 1 / `a_stranger_or_blocked_user_cannot_call` |
| limits: SDP size, ICE frames, attempts | 1 / `oversize_and_video_offers_are_refused`, `ice_frames_are_capped_per_side`, `attempts_per_hour_are_limited` |
| Redis-shared state across 2 gateway replicas | 1 / every `tests/calls.rs` test runs across two gateways and two message-svc |
| history and retention | 2 |
| no SDP or candidate in logs or audit | 1 / `no_sdp_or_candidate_in_logs_or_audit` |
| TURN internal/external/cloudflare/twilio/stun-only/private ranges | 3 |
| SDK Calls API, events; two SDK clients through coturn | 4, 6 |
| phase 2 bridge behaviours | 7, 8 |
| docs | 1 (reference), 5 (complete), 7 (bridge voice) |
