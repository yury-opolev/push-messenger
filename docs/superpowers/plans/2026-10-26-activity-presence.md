# Live activity and presence — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ephemeral per-conversation activity frames (typing/working/waiting/idle) and
contact-only, opt-out-able presence (online/offline + coarse last_seen), end to end:
protocol, message-svc, gateway, SDK, bindings, docs (REQUEST PUSH feature 12).

**Architecture:** Spec: `docs/superpowers/specs/2026-10-26-activity-presence-design.md`
(read it first; it is the source of truth for every semantic decision). The gateway
forwards `Activity` to message-svc (new `Messaging.Activity` RPC), which checks membership
and blocks and publishes `Notification.activity_changed` on each recipient's
`notify:{user}` Redis channel; gateways turn it into `ActivityEvent`. Presence leases live
in Redis (`presence:{u}:conns` sorted set + `presence:{u}:state` hash) written by every
gateway replica with Lua scripts; on a transition the gateway calls
`Messaging.AnnouncePresence`, and message-svc (which owns contacts and the opt-out) reads
the state and publishes `presence_changed` to eligible contacts. Batch reads and the
opt-out are ordinary `Request`s answered by message-svc.

**Tech Stack:** Rust 2024 workspace, prost/tonic, axum WebSockets, redis-rs
(`ConnectionManager`, `redis::Script`), sqlx PostgreSQL migrations, testcontainers
(postgres:17-alpine, redis:7-alpine; Docker must run), napi (Node), cbindgen (C).

**Global rules for every task**

- Branch `feat/activity-presence` in `C:\Users\yurio\Documents\github\push-messenger`.
  Never `git add -A`/`git add .`: the untracked `.github/copilot/` must never be committed.
  Add files by path.
- Conventional commits, each with the trailer
  `Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>`.
- Gates before each commit: `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`, and the task's
  crate tests with `--locked -j 4`.
- Never edit an existing migration. After adding one run `cargo clean -p push-store`.
- Follow the house style: doc comments on every public item, prose comments explaining
  *why*, tests named as sentences (`an_activity_reaches_the_other_members`).
- TDD: write the failing test first, see it fail, implement, see it pass.

---

## File map

| File | Change |
|---|---|
| `crates/push-proto/proto/push/v1/frame.proto` | `Activity`(10), `ActivityEvent`(42), `PresenceEvent`(43), `ActivityState`, `PresenceState`, `Presence`, requests `GetPresence`(20)/`SetPresenceSharing`(21), responses `PresenceList`(11)/`PresenceSharing`(12) |
| `crates/push-proto/proto/push/internal/v1/messaging.proto` | RPCs `Activity`, `AnnouncePresence`; `Notification.activity_changed`(5)/`presence_changed`(6) |
| `crates/push-proto/src/codec.rs`, `lib.rs` | `presence_conns_key`, `presence_state_key` |
| `crates/push-domain/src/activity.rs` (new) | `ActivityState`, `ACTIVITY_TTL`, `validate_label`, `Messaging::activity` |
| `crates/push-domain/src/presence.rs` (new) | `PresenceView`, `MAX_PRESENCE_BATCH`, viewable/audience/sharing/announce |
| `crates/push-domain/src/{notify,store,memory,contacts,lib,validation}.rs` | events, store methods, block push, exports |
| `crates/push-store/migrations/0007_presence_hidden.sql` (new), `src/store.rs` | table + PgStore methods |
| `crates/push-testkit/src/contract/*` | contract test for the presence setting |
| `crates/push-message-svc/src/presence.rs` (new) | `PresenceStates` trait, `RedisPresence`, `FixedPresence` |
| `crates/push-message-svc/src/{notify,grpc,requests,server,lib,convert}.rs` | notifications, RPCs, requests, wiring |
| `crates/push-gateway/src/activity.rs` (new) | `ActivityLimiter` |
| `crates/push-gateway/src/presence.rs` (new) | Lua scripts, per-connection presence task |
| `crates/push-gateway/src/{session,server,hub,limits,config,lib}.rs` | step, loop, frames, limits, `PUSH_PRESENCE_GRACE_MS` |
| `crates/push-gateway/tests/{gateway.rs,presence.rs}` | acceptance tests |
| `crates/push-sdk/src/{model,engine,client.rs,client/requests.rs,connection.rs,testing.rs}` | SDK API and events |
| `bindings/ffi/src/json.rs`, `bindings/c/src/*`, `bindings/c/include/push_sdk.h`, `bindings/node/src/*`, `bindings/node/index.d.ts`, tests | bindings |
| `docs/activity-presence.md` (new), `docs/sdk-bindings.md`, `docs/deployment-configuration.md`, `CHANGELOG.md`, helm chart if it lists gateway env | docs |

---

### Task 1: Protocol

**Files:** `crates/push-proto/proto/push/v1/frame.proto`,
`crates/push-proto/proto/push/internal/v1/messaging.proto`, `crates/push-proto/src/codec.rs`,
`crates/push-proto/src/lib.rs`, plus the minimal compile fixes elsewhere (below).

- [ ] **Step 1: frame.proto.** In `Frame.body` add `Activity activity = 10;` (client
  section) and `ActivityEvent activity_event = 42; PresenceEvent presence_event = 43;`
  (server section); update the trailing comment to "11-31 … 44-63". In the header comment
  add one sentence: activity and presence are frames of their own, not the reserved
  `ephemeral` field, which stays reserved for ephemeral (disappearing) messages. Add:

```proto
// What the sender is doing in a conversation (docs/activity-presence.md). Not stored.
// The other current members' connected devices hear it as ActivityEvent; it lasts 30 s
// unless sent again, and idle ends it at once. At most one per second per device is fanned
// out: extra frames are not refused, the latest one per conversation waits for its turn.
// Nothing answers a good one; a refused one is answered with an Error without ids that is
// never retryable.
message Activity {
  bytes conversation_id = 1;
  ActivityState state = 2;
  // Plain text, 1 to 60 characters, no control, invisible or bidi characters; shown
  // inert (never linkified). Ignored for idle. Empty means none.
  optional string label = 3;
}

enum ActivityState {
  ACTIVITY_STATE_UNSPECIFIED = 0;
  ACTIVITY_STATE_TYPING = 1;
  ACTIVITY_STATE_WORKING = 2;
  ACTIVITY_STATE_WAITING = 3;
  ACTIVITY_STATE_IDLE = 4;
}

// Another member's activity in a conversation. Expire it ttl_ms after receiving it
// (measured on the client's own clock); a newer event from the same user and conversation
// replaces it. Idle has ttl_ms 0: clear at once.
message ActivityEvent {
  bytes conversation_id = 1;
  bytes user_id = 2;
  ActivityState state = 3;
  optional string label = 4;
  uint32 ttl_ms = 5;
}

enum PresenceState {
  // Not visible: not an accepted contact, blocked, either side switched presence off, or
  // no such user. These cases are indistinguishable on purpose.
  PRESENCE_STATE_UNKNOWN = 0;
  // At least one device of the user is connected.
  PRESENCE_STATE_ONLINE = 1;
  PRESENCE_STATE_OFFLINE = 2;
}

message Presence {
  bytes user_id = 1;
  PresenceState state = 2;
  // OFFLINE only, when known: the minute the user was last online (microseconds since
  // the Unix epoch, a whole minute).
  optional int64 last_seen = 3;
}

// A contact's presence changed (or became visible or invisible).
message PresenceEvent {
  Presence presence = 1;
}

// The presence of up to 100 users; one Presence per distinct id, in request order.
message GetPresence {
  repeated bytes user_ids = 1;
}

message PresenceList {
  repeated Presence presences = 1;
}

// Switches the caller's presence sharing. Off: contacts see UNKNOWN, and the caller sees
// UNKNOWN for everyone else too. Without share it only reads the setting.
message SetPresenceSharing {
  optional bool share = 1;
}

message PresenceSharing {
  bool share = 1;
}
```

  Add `GetPresence get_presence = 20; SetPresenceSharing set_presence_sharing = 21;` to
  `Request.body` and `PresenceList presences = 11; PresenceSharing presence_sharing = 12;`
  to `Response.body`.

- [ ] **Step 2: messaging.proto.** In `service Messaging` add:

```proto
  // An Activity frame of the user's device: checks the conversation and the membership,
  // and publishes it to the other current members (no block either way). Answers once
  // published. Errors: INVALID, CONVERSATION_NOT_FOUND, NOT_MEMBER.
  rpc Activity(ActivityRequest) returns (ActivityResponse);
  // The user's presence may have changed (the gateway saw a first connection or the end
  // of the last one): reads the shared state and publishes it to the contacts allowed to
  // see it. Idempotent.
  rpc AnnouncePresence(AnnouncePresenceRequest) returns (AnnouncePresenceResponse);
```

  Messages: `ActivityRequest { bytes user_id = 1; bytes device_id = 2; push.v1.Activity activity = 3; }`,
  `ActivityResponse {}`, `AnnouncePresenceRequest { bytes user_id = 1; }`,
  `AnnouncePresenceResponse {}`. In `Notification.kind` add
  `push.v1.ActivityEvent activity_changed = 5; push.v1.Presence presence_changed = 6;`
  (comment: like everything on `notify:`, never a message body; lost ones only delay).
  Add a comment block describing the presence keys (spec "Redis presence state").

- [ ] **Step 3: key helpers.** In `codec.rs` next to `notify_channel`:

```rust
/// The Redis sorted set of a user's presence leases: connection id → lease end (ms). The
/// braces are a hash tag, so a user's presence keys share a cluster slot.
pub fn presence_conns_key(user: Uuid) -> String {
    format!("presence:{{{user}}}:conns")
}

/// The Redis hash of a user's announced presence: `on` (1 while announced online) and
/// `seen` (the last minute the user was online, in seconds since the Unix epoch).
pub fn presence_state_key(user: Uuid) -> String {
    format!("presence:{{{user}}}:state")
}
```

  Unit test: both keys for one uuid share the `{uuid}` tag. Re-export from `lib.rs`.

- [ ] **Step 4: compile fixes only.** `cargo build --workspace --locked -j 4`. Fix
  non-exhaustive matches with the *real* mapping where trivial:
  `crates/push-gateway/src/hub.rs::server_frame` maps `Kind::ActivityChanged(e)` →
  `Body::ActivityEvent(e)` and `Kind::PresenceChanged(p)` →
  `Body::PresenceEvent(PresenceEvent { presence: Some(p) })` (add a hub unit test that a
  notification of each kind is forwarded as that frame, next to the existing ones). Any
  gRPC `Messaging` implementors (message-svc `MessagingApi`, gateway test `FlakySync` in
  `crates/push-gateway/tests/gateway.rs`, others found by the compiler) get the two new
  methods: `MessagingApi` returns `Status::unimplemented` for now; `FlakySync` delegates
  to `self.api`. message-svc `requests::answer` returns `invalid("not supported yet")`
  for the two new request bodies until Task 4.
- [ ] **Step 5:** gates; `cargo test -p push-proto -p push-gateway --lib --locked -j 4`.
- [ ] **Step 6: Commit** `feat(proto): activity and presence frames`.

---

### Task 2: Domain — activity, presence rules, opt-out storage (memory)

**Files:** create `crates/push-domain/src/activity.rs`, `crates/push-domain/src/presence.rs`;
modify `notify.rs`, `store.rs`, `memory.rs`, `contacts.rs`, `validation.rs`, `lib.rs`.

Types and functions (exact names; later tasks rely on them):

```rust
// activity.rs
/// How long an activity lasts unless sent again.
pub const ACTIVITY_TTL: TimeDelta = TimeDelta::seconds(30);
/// Longest activity label, in characters.
pub const MAX_ACTIVITY_LABEL_CHARS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivityState { Typing, Working, Waiting, Idle }

/// Checks a label: `None`/empty → `Ok(None)`; else 1..=60 chars, not only whitespace,
/// `templates::check_value(label, 60, false)` rules. Errors are `ValidationError::ActivityLabel(String)`.
pub fn validate_label(label: Option<&str>) -> Result<Option<String>, ValidationError>;

impl<S: Store, C: Clock, N: Notifier> Messaging<S, C, N> {
    /// Publishes `sender`'s activity in `conversation` to the other current members that
    /// have no block with the sender in either direction. Returns the recipients (sorted).
    /// Idle drops the label and has a zero ttl. Unknown conversation →
    /// `ConversationNotFound`; sender not a current member → `NotMember`.
    pub async fn activity(&self, sender: UserId, conversation: ConversationId,
        state: ActivityState, label: Option<&str>) -> DomainResult<Vec<UserId>>;
}

// notify.rs — new Event variants
    /// `user`'s activity in `conversation`; push to `to`. `ttl` is zero for idle.
    Activity { to: Vec<UserId>, conversation: ConversationId, user: UserId,
               state: ActivityState, label: Option<String>, ttl: TimeDelta },
    /// `user`'s presence as `to` may see it.
    Presence { to: Vec<UserId>, user: UserId, view: PresenceView },

// presence.rs
/// Most users one presence read may ask about.
pub const MAX_PRESENCE_BATCH: usize = 100;

/// What a viewer is shown. `Unknown` for everything not visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceView { Unknown, Online, Offline { last_seen: Option<DateTime<Utc>> } }

impl Messaging {
    /// Which of `candidates` `viewer` may see the presence of: `viewer` itself always;
    /// others only if both contact rows are `Accepted` and neither the viewer nor the
    /// candidate hides presence. Sorted, deduplicated.
    pub async fn presence_viewable(&self, viewer: UserId, candidates: &[UserId]) -> DomainResult<Vec<UserId>>;
    /// Who hears `user`'s presence changes: accepted contacts (both rows Accepted) that do
    /// not hide presence; empty when `user` hides it. Sorted.
    pub async fn presence_audience(&self, user: UserId) -> DomainResult<Vec<UserId>>;
    /// Whether `user` shares presence (no row in the hidden set).
    pub async fn presence_sharing(&self, user: UserId) -> DomainResult<bool>;
    /// Switches sharing. When it changes: hiding publishes `Unknown` to the former
    /// audience (computed before the change); showing publishes `current` to the new
    /// audience. Returns the new setting. Unknown user → `UnknownUser`.
    pub async fn set_presence_sharing(&self, user: UserId, share: bool, current: PresenceView) -> DomainResult<bool>;
    /// Publishes `view` of `user` to `presence_audience(user)` (nothing when empty).
    pub async fn announce_presence(&self, user: UserId, view: PresenceView) -> DomainResult<()>;
}

// store.rs — Tx
    /// The users among `users` who hide their presence.
    async fn presence_hidden(&mut self, users: &[UserId]) -> DomainResult<Vec<UserId>>;
    /// Hides (`true`) or shows `user`'s presence. Idempotent.
    async fn set_presence_hidden(&mut self, user: UserId, hidden: bool) -> DomainResult<()>;
```

`contacts.rs::contact_block`: read `previous` (already read) and the peer's row before
changing anything; if both were `Accepted`, after commit also publish
`Event::Presence { to: vec![peer], user: me, view: Unknown }` and
`Event::Presence { to: vec![me], user: peer, view: Unknown }`.

Membership: use `tx.get_conversation` (None → `ConversationNotFound`),
`tx.members(conversation)` and `Member::is_current()`; the sender must be current
(`NotMember`). Blocks: `tx.get_contact(a, b) == Some(ContactState::Blocked)` either way.
Roll back read-only transactions (`tx.rollback()`), like `list_contacts`.

- [ ] **Step 1: failing tests** in `activity.rs`/`presence.rs` `#[cfg(test)]` using
  `InMemoryStore`, `ManualClock`, `RecordingNotifier` (see existing tests in
  `groups.rs`/`contacts.rs` for set-up helpers). Cases:
  `an_activity_goes_to_the_other_current_members_only` (group of 3 + a former member:
  sender excluded, left member excluded),
  `an_activity_in_a_conversation_the_sender_is_not_in_is_refused` (`NotMember`),
  `an_unknown_conversation_is_not_found`,
  `members_with_a_block_either_way_are_skipped_silently`,
  `idle_drops_the_label_and_has_no_ttl`, `labels_follow_the_value_rules` (61 chars,
  `"\u{202E}x"`, `"a\nb"`, `"   "` refused; 60 chars, `"https://x.io"` accepted; `""` →
  None), `presence_is_viewable_by_accepted_contacts_only` (stranger, requested,
  blocked → not viewable; self viewable),
  `hiding_presence_hides_it_from_contacts_and_hides_contacts_from_the_hider`,
  `the_audience_is_accepted_contacts_that_share`,
  `hiding_publishes_unknown_to_the_former_audience_and_showing_publishes_the_current_state`,
  `blocking_an_accepted_contact_publishes_unknown_both_ways`.
- [ ] **Step 2:** run `cargo test -p push-domain --locked -j 4` → fails to compile/fails.
- [ ] **Step 3:** implement (memory store: a `HashSet<UserId>` of hidden users in the
  in-memory state, transactional like the other fields). Add `ValidationError::ActivityLabel(String)`
  (`#[error("the activity label {0}")]`) and `ValidationError::ActivityState`
  (`"the activity state is missing"`), exports in `lib.rs`.
- [ ] **Step 4:** tests pass; gates (`cargo clippy` will flag every `match` on `Event` in
  other crates without a wildcard — add arms; `push-message-svc/src/notify.rs` already has
  `_ => {}` until Task 4).
- [ ] **Step 5: Commit** `feat(domain): activity fan-out rules and presence visibility`.

---

### Task 3: PostgreSQL store — presence setting

**Files:** create `crates/push-store/migrations/0007_presence_hidden.sql`; modify
`crates/push-store/src/store.rs`; contract test in `crates/push-testkit/src/contract/`
(follow how existing contract tests are registered and run for both stores — look at
`contract/mod.rs` and `crates/push-store/tests/postgres.rs`).

```sql
-- Users who switched presence sharing off (REQUEST PUSH feature 12). A row hides the
-- user's online state and last-seen time from every contact, and hides everyone else's
-- from them. No row: presence is shared.
CREATE TABLE messaging.presence_hidden (
    user_id   uuid PRIMARY KEY REFERENCES messaging.known_user,
    hidden_at timestamptz NOT NULL DEFAULT now()
);
```

PgStore: `presence_hidden` = `SELECT user_id FROM messaging.presence_hidden WHERE user_id = ANY($1)`;
`set_presence_hidden(true)` = `INSERT … ON CONFLICT DO NOTHING`, `false` = `DELETE`.

- [ ] Step 1: contract test `the_presence_setting_round_trips` (default shared; hide two,
  read a batch of three → the two; show one → one; idempotent repeats) run on memory and
  PostgreSQL. Step 2: see it fail. Step 3: migration + impl; `cargo clean -p push-store`.
  Step 4: `cargo test -p push-store -p push-testkit -p push-domain --locked -j 4` (Docker).
  Check `crates/push-store/tests/deployment_role.rs` still passes (it lists tables? adapt
  if it enumerates them). Step 5: Commit `feat(store): persist the presence opt-out`.

---

### Task 4: message-svc — RPCs, requests, notifications, Redis presence reads

**Files:** create `crates/push-message-svc/src/presence.rs`; modify `notify.rs`, `grpc.rs`,
`requests.rs`, `requests/tests.rs`, `server.rs`, `lib.rs`, `convert.rs`;
tests `crates/push-message-svc/tests/redis.rs`, `tests/service.rs`.

```rust
// presence.rs
/// The live presence of users, as the gateways recorded it in Redis.
#[async_trait]
pub trait PresenceStates: Send + Sync {
    /// The state of each of `users` (same order): `Online` or `Offline{last_seen}`;
    /// never `Unknown`. On a Redis failure, `Offline { last_seen: None }` for all.
    /// `flipped` lists users this read found newly offline (their gateway died): the
    /// caller announces them.
    async fn states(&self, users: &[UserId]) -> (Vec<PresenceView>, Vec<UserId>);
}

/// Reads presence with the `read` script, one EVALSHA per user in one pipeline.
#[derive(Clone)] pub struct RedisPresence { connection: ConnectionManager }

/// For tests: fixed states, everyone else offline without last_seen.
#[derive(Default)] pub struct FixedPresence { states: Mutex<HashMap<UserId, PresenceView>> }
impl FixedPresence { pub fn set(&self, user: UserId, view: PresenceView); }
```

Read script (KEYS[1] conns, KEYS[2] state; returns `{online, seen, flipped}`):

```lua
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now)
local live = redis.call('ZCARD', KEYS[1])
local seen = redis.call('HGET', KEYS[2], 'seen') or ''
local flipped = 0
if live == 0 and redis.call('HGET', KEYS[2], 'on') == '1' then
  redis.call('HSET', KEYS[2], 'on', '0')
  flipped = 1
end
return {live > 0 and 1 or 0, seen, flipped}
```

`seen` is unix seconds of a whole minute; convert to `DateTime<Utc>`. `RedisNotifier`
gains `pub fn presence(&self) -> RedisPresence` (shares its `ConnectionManager`).

`MessagingApi` gets a `presence: Arc<dyn PresenceStates>` field:
production must use Redis: add `MessagingApi::with_presence(messaging, presence)`
and make `server.rs` use `with_presence(…, Arc::new(notifier.presence()))`; `new` uses an
`OfflinePresence` (everyone offline, no last_seen) and is documented as for tests/tools.
Update the gateway test stack (`crates/push-gateway/tests/gateway.rs`) to use
`with_presence(…, Arc::new(notifier.presence()))` so Task 5/6 tests see real presence.

RPCs:
- `activity`: parse ids (`user_id`, `device_id`, `activity.conversation_id`), map
  `push.v1.ActivityState` (Unspecified → `invalid`), call `messaging.activity(...)`,
  domain errors via `domain_error`. Response `ActivityResponse {}`.
- `announce_presence`: parse `user_id`; `states(&[user])`; `messaging.announce_presence(user, view)`;
  also announce any `flipped` users (each with its read state).

Requests (`requests::request` gets `presence: &dyn PresenceStates` and passes it to
`answer`):
- `GetPresence`: 1..=100 ids (0 or >100 → `invalid("user_ids: 1 to 100 ids")`), parse
  all, dedupe keeping first-occurrence order; `viewable = presence_viewable(user, ids)`;
  `states(&viewable)`; announce flipped (best effort, spawn-free: just call
  `announce_presence` for each flipped user before answering); answer
  `PresenceList` with one `Presence` per distinct id: viewable → its state; else
  `UNKNOWN` without `last_seen`.
- `SetPresenceSharing { share: Some(s) }` → `states(&[user])` for `current`,
  `set_presence_sharing(user, s, current)`; `None` → `presence_sharing(user)`. Answer
  `PresenceSharing { share }`.

Notifications (`notify.rs::notifications`): `Event::Activity` → one
`Kind::ActivityChanged(ActivityEvent{conversation_id, user_id, state, label, ttl_ms})`
per recipient; `Event::Presence` → `Kind::PresenceChanged(Presence{…})` per recipient
(`Online` → ONLINE; `Offline{last_seen}` → OFFLINE + micros; `Unknown` → UNKNOWN). Put
the domain↔proto conversions in `convert.rs` (`activity_state`, `presence`).

- [ ] Step 1: failing tests — `notify.rs` unit tests for the two new events;
  `requests/tests.rs`: `get_presence_answers_unknown_for_strangers_and_the_state_for_contacts`,
  `get_presence_refuses_more_than_100_ids`, `get_presence_answers_once_per_distinct_id_in_order`,
  `opting_out_answers_unknown_to_contacts_and_hides_them_from_the_hider`,
  `set_presence_sharing_without_a_value_reads_it`; `tests/redis.rs` (real Redis):
  `the_read_script_reports_online_offline_and_flips_once` (seed the keys by hand with
  `ZADD`/`HSET` using a lease in the past and in the future); `tests/service.rs`:
  `activity_rpc_publishes_to_members` and `activity_rpc_refuses_a_non_member`.
- [ ] Step 2: see them fail. Step 3: implement. Step 4: `cargo test -p push-message-svc --locked -j 4`.
- [ ] Step 5: Commit `feat(message-svc): activity fan-out and presence reads`.

---

### Task 5: Gateway — activity frames, limiter, idle on disconnect

**Files:** create `crates/push-gateway/src/activity.rs`; modify `session.rs`, `server.rs`,
`lib.rs`, `limits.rs`; tests in `session.rs`, `activity.rs`, `crates/push-gateway/tests/gateway.rs`.

`Session::on_binary`: `(Body::Activity(a), Ready)` → `Step::Activity(a)`.

```rust
// activity.rs
/// Fans out at most one activity per second per connection (a device has one live
/// connection), burst 2. Extra activities are not refused: the latest per conversation
/// waits for the next token, so the final state always goes out. At most 16 conversations
/// wait; a 17th is dropped.
pub struct ActivityLimiter { tokens: f64, updated: Instant, pending: IndexMap-or-VecDeque<(conversation bytes, Activity)>,
                             /// Unexpired non-idle activity sent, by conversation: idle on disconnect.
                             live: HashMap<Vec<u8>, Instant> }
impl ActivityLimiter {
    pub const BURST: f64 = 2.0;
    pub const PER_SECOND: f64 = 1.0;
    pub const MAX_PENDING: usize = 16;
    pub fn new(now: Instant) -> Self;
    /// An activity from the client: `Some` to send now, or `None` (queued/replaced/dropped).
    pub fn offer(&mut self, activity: Activity, now: Instant) -> Option<Activity>;
    /// When the next queued activity may go, if any.
    pub fn next_due(&self) -> Option<Instant>;
    /// The queued activity to send now, if a token is free.
    pub fn take_due(&mut self, now: Instant) -> Option<Activity>;
    /// Records an activity that message-svc accepted (for idle on disconnect).
    pub fn sent(&mut self, activity: &Activity, now: Instant);
    /// Conversations whose last activity is unexpired and not idle.
    pub fn still_live(&self, now: Instant) -> Vec<Vec<u8>>;
}
```

Use a `Vec<(Vec<u8>, Activity)>` for pending (replace in place keeps the original
queue position; order is FIFO by first arrival); no new dependency.
Bucket math like `FrameBudget` in `limits.rs`. TTL constant: 30 s (match domain
`ACTIVITY_TTL`; reuse `push_domain::ACTIVITY_TTL` only if the gateway already depends on
push-domain — it does not; define `ACTIVITY_TTL: Duration = 30 s` in `activity.rs` with a
comment pointing at the domain constant).

`server.rs`:
- `Connected` gains `activity: ActivityLimiter`.
- `Step::Activity(a)` → `if let Some(a) = limiter.offer(a, now) { self.publish_activity(a) }`.
- New select arm: `sleep_until(next_due)` when `next_due` is `Some` → `take_due` → publish.
- `publish_activity`: `messaging.activity(ActivityRequest{user_id, device_id, activity})`;
  Ok → `limiter.sent(..)`; Err → `end_if_over` then send `error_from_status` with
  `retryable = false` (SDKs treat an anonymous retryable error as a ReAuth retry — see
  `crates/push-sdk/src/connection.rs::on_own_frame`).
- After the connection loop ends, for every `still_live` conversation spawn (on
  `app.sessions` tracker, 2 s timeout each) `messaging.activity(... IDLE ...)`. Refactor:
  move the loop into `async fn serve_connection(...) -> ()` and do the clean-up in
  `connection()` after it returns, so every exit path runs it.
- `limits.rs`: `Action::Presence` for `request::Body::GetPresence(_)`, per user 120 per
  60 s; name `"presence"`. Update `only_guessing_and_probing_requests_are_limited`.

- [ ] Step 1: failing tests — `session.rs`: `activity_becomes_a_step_once_ready`;
  `activity.rs` unit tests (tokio `start_paused` or explicit Instants):
  `two_go_at_once_then_one_per_second`, `the_latest_waiting_activity_per_conversation_wins`,
  `at_most_16_conversations_wait`, `still_live_forgets_idle_and_expired`;
  integration (`tests/gateway.rs`, helpers exist for users/contacts/devices/sockets):
  `an_activity_reaches_the_other_members_and_not_other_conversations` (alice, bob, carol;
  alice–bob direct and alice–carol direct; alice sends WORKING "running tests" in A–B:
  bob gets `ActivityEvent{state WORKING, label, ttl_ms 30000, user alice}`, carol gets
  nothing within 500 ms, alice's second device gets nothing),
  `an_activity_expires_its_ttl_and_ends_with_idle_on_disconnect` (bob sees ttl_ms 30000;
  alice's socket closes → bob receives IDLE with ttl_ms 0),
  `the_activity_rate_limit_holds` (alice sends 10 TYPING/WORKING alternating in < 100 ms;
  within 1.5 s bob gets at most 3, and the last one bob gets equals alice's last one),
  `an_activity_in_a_foreign_conversation_is_refused` (`NOT_MEMBER` or
  `CONVERSATION_NOT_FOUND`, `retryable == false`).
- [ ] Step 2: fail. Step 3: implement. Step 4: `cargo test -p push-gateway --locked -j 4`.
- [ ] Step 5: Commit `feat(gateway): fan out activity frames with a per-device rate limit`.

---

### Task 6: Gateway — presence leases and announcements

**Files:** create `crates/push-gateway/src/presence.rs`; modify `server.rs`, `config.rs`
(+ its tests), `lib.rs`; create `crates/push-gateway/tests/presence.rs` (or extend
`gateway.rs`; share the stack helper — if it's private to `gateway.rs`, put the presence
tests in `gateway.rs`).

Constants: `LEASE = 90 s`, `REFRESH = 30 s`, grace from config
`PUSH_PRESENCE_GRACE_MS` (default 10_000, allowed 0..=120_000; `Config.presence_grace: Duration`).

Scripts (all KEYS = [conns, state]; time from Redis `TIME`):

```lua
-- up: ARGV[1] connection id, ARGV[2] lease ms, ARGV[3] state ttl s. Returns 1 when the
-- user had no live lease before this call, counting this connection's own (announce
-- online). A refresh of a live lease returns 0; a reconnect inside another connection's
-- grace returns 0; a user whose set expired with a dead gateway returns 1.
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now)
local before = redis.call('ZCARD', KEYS[1])
redis.call('ZADD', KEYS[1], now + tonumber(ARGV[2]), ARGV[1])
-- The set lives as long as its longest lease, never shorter.
local top = redis.call('ZRANGE', KEYS[1], -1, -1, 'WITHSCORES')
redis.call('PEXPIREAT', KEYS[1], tonumber(top[2]))
redis.call('HSET', KEYS[2], 'on', '1', 'seen', t[1] - (t[1] % 60))
redis.call('EXPIRE', KEYS[2], tonumber(ARGV[3]))
if before == 0 then return 1 end
return 0
```
```lua
-- down: ARGV[1] connection id, ARGV[2] grace ms. The lease ends after the grace.
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
if redis.call('ZSCORE', KEYS[1], ARGV[1]) then
  redis.call('ZADD', KEYS[1], 'XX', now + tonumber(ARGV[2]), ARGV[1])
end
redis.call('HSET', KEYS[2], 'seen', t[1] - (t[1] % 60))
return 0
```

```lua
-- settle: returns 1 when the user just went offline (announce).
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now)
if redis.call('ZCARD', KEYS[1]) == 0 and redis.call('HGET', KEYS[2], 'on') == '1' then
  redis.call('HSET', KEYS[2], 'on', '0')
  return 1
end
return 0
```

Per-connection task:

```rust
/// Starts tracking a connection's presence: `up` now and every 30 s, and, once the
/// returned guard is dropped, `down`, then after the grace `settle` (skipped on shutdown).
/// Transitions call `Messaging.AnnouncePresence`. Redis or message-svc failures are
/// logged and ignored.
pub fn track(app: PresenceContext, user: Uuid, connection: Uuid) -> PresenceGuard;
```

The settle step sleeps grace + 100 ms (so the lease has surely ended on Redis's clock).
The state hash TTL is 30 days.

`PresenceContext` holds the Redis `ConnectionManager` (the gateway's `publisher`), the
`MessagingApi` client, the grace, the shutdown token and the `TaskTracker`. The task runs
on `app.sessions` (`TaskTracker::spawn`) so shutdown waits for `down`. Start it in
`connection()` right after `take_over`, before `Ready` is sent; drop the guard when the
connection ends (after the idle clean-up of Task 5).

- [ ] Step 1: failing tests (real Redis, grace 300 ms via the test config):
  `presence_flips_with_connect_and_disconnect_across_all_devices` (alice & bob accepted
  contacts; bob connected; alice phone connects → bob gets `PresenceEvent ONLINE`; alice
  laptop connects → nothing new; phone closes → nothing (within 1 s); laptop closes → bob
  gets `OFFLINE` with `last_seen` a whole minute, within grace + 1 s); `GetPresence` from
  bob reflects each step),
  `a_reconnect_within_the_grace_is_not_announced`,
  `a_non_contact_sees_unknown` (carol, no contact, `GetPresence([alice])` → UNKNOWN
  without last_seen, and carol gets no PresenceEvent when alice connects),
  `opting_out_hides_presence` (alice `SetPresenceSharing{false}` → bob gets UNKNOWN; bob's
  `GetPresence([alice])` → UNKNOWN; alice's `GetPresence([bob])` → UNKNOWN; switching back
  pushes ONLINE to bob),
  `a_blocked_contact_sees_unknown`.
  Config tests: default 10 s, parse, out-of-range refused.
- [ ] Step 2: fail. Step 3: implement. Step 4: `cargo test -p push-gateway --locked -j 4`.
- [ ] Step 5: Commit `feat(gateway): presence leases shared across replicas`.

---

### Task 7: SDK

**Files:** `crates/push-sdk/src/model.rs`, `engine.rs`, `client.rs`,
`client/requests.rs`, `connection.rs`, `testing.rs`, `lib.rs`.

API (exact names):

```rust
// model.rs
pub enum ActivityState { Typing, Working, Waiting, Idle }
pub struct Activity { pub conversation_id: ConversationId, pub user_id: UserId,
    pub state: ActivityState, pub label: Option<String>,
    /// When to stop showing it, unix ms on this device's clock (receive time + ttl).
    pub expires_at_ms: i64 }
pub enum PresenceState { Unknown, Online, Offline }
pub struct Presence { pub user_id: UserId, pub state: PresenceState,
    /// Offline only: unix ms, a whole minute.
    pub last_seen_ms: Option<i64> }
// Event: ActivityChanged(Activity), PresenceChanged(Presence)

// client
pub async fn send_activity(&self, conversation: ConversationId, state: ActivityState,
    label: Option<&str>) -> Result<(), SdkError>;   // validates label (push_domain::validate_label) → SdkError::Invalid;
                                                    // not connected → SdkError::Offline; sends a frame, no answer awaited
pub async fn get_presence(&self, users: &[UserId]) -> Result<Vec<Presence>, SdkError>; // 1..=100 else Invalid
pub async fn set_presence_sharing(&self, share: bool) -> Result<bool, SdkError>;
pub async fn presence_sharing(&self) -> Result<bool, SdkError>;
```

Add `Command::Frame(Body)` to `connection.rs` (pushed to `out` while live; dropped
otherwise) for `send_activity`. Engine: `Body::ActivityEvent` → `Event::ActivityChanged`
(`expires_at_ms = now_ms + ttl_ms`), `Body::PresenceEvent` → `Event::PresenceChanged`.
Anonymous non-retryable errors keep being logged only. Use existing `SdkError` variants
(check `error.rs`; add `Invalid(String)` only if no equivalent exists).
`testing.rs` fake gateway: answer `GetPresence`/`SetPresenceSharing` if the fake answers
requests generically; otherwise add arms.

- [ ] Step 1: failing tests — engine: `an_activity_event_becomes_activity_changed_with_a_local_expiry`,
  `a_presence_event_becomes_presence_changed`; connection/client (fake socket like the
  existing tests): `send_activity_sends_a_frame`, `send_activity_refuses_a_bad_label`,
  `get_presence_round_trips`.
- [ ] Step 2: fail. Step 3: implement. Step 4: `cargo test -p push-sdk --locked -j 4`.
- [ ] Step 5: Commit `feat(sdk): activity and presence`.

---

### Task 8: Bindings (C and Node)

Read `docs/sdk-bindings.md` and the existing patterns for a fire-and-forget call and a
request call (e.g. `list_contacts`, `open_direct`) in `bindings/ffi/src/json.rs`,
`bindings/c/src/client.rs`, `bindings/node/src/client.rs`, `bindings/node/index.d.ts`,
`bindings/node/index.js`. Add: `send_activity(conversation_id, state, label?)`,
`get_presence(user_ids[])`, `set_presence_sharing(bool)`, `presence_sharing()`, and the
JSON forms of the two events:
`{"type":"activity_changed","conversation_id":…,"user_id":…,"state":"typing|working|waiting|idle","label":…|null,"expires_at_ms":…}`,
`{"type":"presence_changed","user_id":…,"state":"unknown|online|offline","last_seen_ms":…|null}`
(match the existing event JSON conventions exactly — field naming, tag key). Regenerate
`bindings/c/include/push_sdk.h` the way the repo does (cbindgen in `build.rs`; the
`header.rs` test checks it is current). Node: rebuild per `bindings/node/scripts/build.js`
and update `index.d.ts` + `test/types.ts`. Add JSON round-trip tests in `json.rs`, a C
header test expectation if needed, and a Node test if the suite has a fake-gateway one.
Check the CI workflow (`.github/workflows/*bindings*`) to run the same verification
locally.

- [ ] Steps: failing tests → implement → `cargo test -p push-sdk-ffi -p push-sdk-c --locked -j 4`
  (actual package names from the Cargo.tomls) and `npm test` in `bindings/node` if the
  toolchain is present → Commit `feat(bindings): activity and presence in C and Node`.

---

### Task 9: Docs and release notes

- Create `docs/activity-presence.md`: the SDK/protocol guide — frames with field tables,
  semantics (TTL & local expiry, refresh cadence 10–20 s, idle on disconnect, rate limit &
  coalescing, fan-out targets, errors never retryable), presence (aggregation, 10 s grace,
  last_seen minute, visibility, reciprocal opt-out, UNKNOWN indistinguishability, blocks),
  batch read limits, multi-replica Redis keys, SDK + C + Node API snippets, the agent
  recipe (`working` + label every 15 s, `waiting` while a card is open, `idle` when done),
  and migration notes for the heartbeat-edit workaround.
- `docs/sdk-bindings.md`: new calls and events.
- `docs/deployment-configuration.md`: `PUSH_PRESENCE_GRACE_MS`; migration 0007; deploy
  message-svc before gateways. Helm chart: add the env var only if the chart enumerates
  gateway env vars with defaults (run `deploy/helm/test-chart.ps1` if touched).
- `CHANGELOG.md` "Unreleased": Added (activity, presence, opt-out, SDK/bindings), Upgrade
  notes.
- Main spec `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §7: one line each
  in the client/server frame lists pointing to `docs/activity-presence.md`.
- Commit `docs: activity and presence protocol guide`.
