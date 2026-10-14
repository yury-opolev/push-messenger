# Plan 8A — Gateway: the Service Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A complete `push-gateway` (spec §7). Clients identify with the access tokens auth-svc issues, checked locally against auth-svc's JWKS (EdDSA pinned, `kid` for key rotation, `exp` required with 60 s of leeway, `aud` the gateway's URL), and tokens issued no later than the user's last password reset are refused. Every client frame of §7.1 travels to message-svc's existing RPCs, and every server frame of §7.2 reaches the client: receipts, read positions, contact events and `AuthExpiring`. `Identify` watches the device's kick channel before message-svc answers, `ReAuth` renews a connection's token, a newer connection of a device takes over, kicks close sockets (4003 for a revocation, 4001 for a password reset), a device not relayed yet gets a retryable close of its own (4006), and the close codes of §7.4 are aligned with the owner's decisions. Group requests are refused behind one switch, because groups are not offered to apps in the first release. Rate limits in Redis guard `RedeemInvite`, `ContactRequest` and `CreateInvite` (failing closed), and per-device `Identify` and per-connection frame budgets guard the gateway. Write and idle timeouts, Redis and HTTP/2 keepalives, a 5-minute sweep and batched draining make dead connections and dead dependencies visible. Contact requests stop revealing which user ids exist. The DEV ONLY identity is removed. Everything is tested against the in-memory store and Postgres and Redis containers. Plan 8B moves the smoke test onto the WebSocket alone, puts message-svc's `Messaging` API behind the gateway's own internal token, and verifies it all on minikube.

**Architecture:** The gateway stays transport only (spec §2). A new `tokens.rs` keeps auth-svc's public keys by `kid` and checks tokens with `jsonwebtoken` (already in the workspace for auth-svc); it fetches `GET {PUSH_AUTH_URL}/v1/auth/jwks` (at most 64 KiB) at start-up and every 5 minutes, and again at once (at most every 30 s, whatever the `kid`) when a token names a key it does not hold. A key that leaves the published set is kept for 16 minutes (a token's 15 minutes plus the leeway), so a rotation signs nobody out and needs no restart. message-svc owns the password-reset check, as it owns the devices: auth-svc now writes its own time of the reset into the `user.password_reset` outbox event, the worker relays it in `AccountUser.reset_at`, message-svc keeps the latest (`known_user.password_reset_at`, migration 0003) and `Identify` takes the token's `iat` and refuses a token from that second or earlier. The hub (`hub.rs`) is rebuilt around channels rather than users: every connection joins `kick:{device_id}` as soon as its token passed, before message-svc is asked, and `notify:{user_id}` once admitted; notifications become rings (sync) or server frames, kicks close sockets, and a takeover is a `Kick{TAKEN_OVER, connection_id}` the newcomer publishes. The session (`session.rs`) remains pure protocol state; `server.rs` does the I/O. Rate limits (`limits.rs`) are fixed windows in Redis like auth-svc's; the frame budget is a token bucket per connection; `drain.rs` spreads the shutdown closes. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §2, §5.1, §5.3–§5.5, §7 (all), §11, §12, §13.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85). No new crates in the workspace: the gateway now also depends on `jsonwebtoken` 9.3 (ring), `reqwest` 0.12 (no TLS), `serde`, `serde_json`, `sha2`, `async-trait` and `tokio-tungstenite` 0.29 (the version axum 0.8 uses), and on `push-auth`, `push-domain` and `push-message-svc` for tests only. All are in the workspace already, with MSRV 1.85.

---

## Before you start

- **Plan 7B is merged** (PR #13). This plan changes `push-proto`, `push-domain`,
  `push-store`, `push-testkit`, `push-message-svc` and `push-gateway`; auth-svc and the
  worker change only to carry the time of a password reset (Task 3); the smoke test changes
  only as far as the gateway forces it; and the gateway's chart settings change. Plan 8B
  does the smoke test over the WebSocket, the Messaging token and minikube.
- **The spec is already revised.** `docs/superpowers/specs/2026-10-11-push-messenger-design.md`
  on `main` describes Plan 8 as built here (it was committed with this plan), so no task
  changes it; Task 11 updates only the README and the roadmap.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-08a-gateway-service
```

- Fix a stale PATH for the current PowerShell session (each new shell needs it again):

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
```

- **Docker must be running.** The tests start `postgres:17-alpine` and `redis:7-alpine`
  containers. A run killed half-way can leave containers behind (`docker ps -a`, then
  `docker rm -f <id>` for the `postgres` and `redis` ones only — never touch other
  containers on this machine).
- **How the code is given.** Every code block of a task was pasted from the commit that
  implemented and verified it. A new file is given whole ("Create"). A changed file is
  given as a git patch ("Change"): save the block to a file outside the tree and run
  `git apply <file>` from the repository root, in the order the task lists them; a patch
  that does not apply means an earlier step was skipped or changed. Deleted files are
  removed with `git rm`.
- **Keep files LF** (`.gitattributes`: `* text=auto eol=lf`). A tool that writes CRLF is
  normalised by git on commit, but `git apply` needs the tree as committed.
- **Do not pipe `cargo fmt`.** Run `cargo fmt --all -- --check` on its own and read the exit
  code. The code in this plan is formatted, so a diff means a copying mistake.
- Baseline (after Plan 7B): `cargo test --workspace --locked` passes 822 tests;
  `deploy/helm/test-chart.ps1` passes 29 checks.
- **Cargo.lock.** Tasks 4 and 9 add dependencies to `push-gateway`; their patches include
  the `Cargo.lock` change, so every gate runs with `--locked`. Never run `cargo update`.
- **Every task ends green:**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

  plus `pwsh deploy/helm/test-chart.ps1` when a task touches the chart. Never add
  `#[allow(...)]`: every item a task adds is used by its code, tests or public API.
- **Postgres tests connect to `127.0.0.1`, never `localhost`** (on Windows `localhost`
  tries IPv6 first and every connect costs about 2 s).
- **A test run that fails with `crate … required to be available in rlib format`** means an
  interrupted build left the shared target directory half-written: run the same command
  again. Building every task fills the target directory quickly (tens of GB with
  incremental builds); keep an eye on free disk space.
- **Scope rule:** auth-svc and the worker change only to put auth-svc's time of a password
  reset into the relayed event; the smoke test changes only as far as the gateway forces it
  (it identifies with its access token from Task 5); the Messaging token, the smoke test
  over the WebSocket and minikube are Plan 8B; no TLS, Key Vault or NetworkPolicy (Plan 11).

## Key rules the code must preserve

- **The gateway learns the user and device only from a verified token** (spec §5.3). No
  identity comes from anything a client sends unsigned.
- **Only 4001 signs a user out and only 4003 stops a client.** Every other refusal —
  a device not relayed yet, keys or message-svc unreachable, an unknown or retired key, a
  protocol mistake before `Ready`, a rate limit — must let the client reconnect.
- **Each service owns its database.** The gateway never reads auth-svc's or message-svc's
  database; the password-reset check lives in message-svc, which hears of resets from the
  worker.
- **Subscribe before you ask.** A connection watches `kick:{device_id}` before message-svc
  checks the device, so a kick published after that check cannot be missed.
- **Redis carries only signals**: rings, receipt positions, contact states and kicks with a
  reason (and, for a reset, its time), never a body or a token.
- **Never log tokens, bodies or message contents.** Close reasons and errors are fixed
  texts; logs name users and devices by id at most.
- **Every Redis call and connect loop is bounded and interruptible by shutdown.**
- **Delivery positions move only on `Ack`.** Whether entries are waiting is always asked of
  `Sync` (`entries_after` with a limit plus one), never decided by comparing positions:
  edit and delete entries a member may not see can end a log and park the position before
  them.
- **No answer reveals which user ids exist.**

## Decisions made for this plan

These were settled while writing the plan, revised after review and verified in a
worktree. The owner's earlier decisions apply (a revoked device closes with 4003, a
password reset with 4001; groups are not offered to apps in the first release; names of
people one can no longer chat with stay visible). Do not reopen any of this during
execution.

1. **Plan 8 is split.** 8A (this plan) delivers the service, tested against containers;
   8B moves the smoke test onto the WebSocket alone, adds the Messaging token and verifies
   everything on minikube.
2. **How the gateway learns of a password reset: auth-svc's time, recorded by message-svc,
   compared with the token's `iat`.** auth-svc writes its own time of the reset (the clock
   that sets the tokens' `iat`) into the `user.password_reset` outbox payload as `reset_at`;
   the worker relays it in `AccountUser.reset_at` (a new field, appended); message-svc
   stores the greater of the stored and the relayed time in `known_user.password_reset_at`
   (migration 0003), so a redelivered or older event moves nothing; an event written before
   auth-svc sent the field carries 0 and message-svc uses its own clock. `Identify` gets the
   token's `iat` and refuses a token issued no later than that second (`UNAUTHENTICATED`,
   `PASSWORD_CHANGED`), on connect and on every `ReAuth`; within the reset's second the
   order is unknown, so it refuses. A sign-in after the reset passes even when the worker
   relays the reset later. The `PASSWORD_RESET` kick carries the reset time, and the
   gateway leaves open a connection whose token (after any `ReAuth`) was issued after it.
   Rejected: the relay time (a slow relay or a redelivery would sign out sessions that
   started after the reset); a `pwd` claim (the gateway would read auth-svc's database or
   call it on every connect); the kick alone (a lost Redis publish, and a token that
   outlives the reset by up to 15 minutes).
3. **Tokens are checked in the gateway, keys from the JWKS by `kid`.** Algorithm pinned to
   EdDSA (the header cannot choose HMAC with the public key), `exp`, `sub` and `aud`
   required (jsonwebtoken 9.3 skips a mistyped claim unless required, CVE-2026-25537),
   `device_id` and `iat` present, `email_verified` true, `exp` with 60 s of leeway. The set
   is read up to 64 KiB; a larger one, or a failed fetch, changes nothing. **Rotation signs
   nobody out:** a key that leaves the fetched set is kept for 16 minutes (15 minutes plus
   the leeway) after the first fetch that missed it, and a token naming a key the gateway
   does not hold even after fetching the set for that very token is `Expired` (4002:
   refresh, which gets a token signed with the current key), never `Invalid` (4001). A
   token naming an unknown key fetches the set again at most every 30 s whatever the `kid`,
   so made-up ids cannot make the gateway hammer auth-svc; a key that could not be looked
   up — fetch failed or too soon — is `KeysUnavailable` (1011). `/readyz` waits for the
   keys. Retiring a compromised key at once takes a restart of the gateways.
4. **Close codes (spec §7.4).** 4001 invalid token or password reset (sign in again); 4002
   expired token or no `ReAuth` by `exp + 30 s` (refresh, reconnect); 4003 takeover or
   revoked (stop); 4004 rate limited; 4005 shutting down; **4006 device not ready yet**
   (reconnect after a short backoff, never sign out); **1002** a protocol mistake before
   `Ready` (this answers spec §7.4's open "4001 or 4002 before Ready": neither — a client
   bug is no reason to sign out); 1008 silent for two and a half heartbeats; 1009 a message
   over 128 KiB; 1011 keys or message-svc unreachable. A token presented to `Identify`
   already past its `ReAuth` deadline (`exp + 30 s`, but still inside the 60 s leeway) is
   refused with 4002 at once, never answered `Ready` and closed a moment later.
5. **`Identify` and the oracle.** The device id comes from the verified token, so
   `UNKNOWN_DEVICE` (4003) versus `DEVICE_NOT_READY` (4006) tells a client only about its
   own device. `DEVICE_NOT_READY` is a new error code, retryable, so the gateway can tell it
   from message-svc being down; its log line is `debug`, not `warn`.
6. **The hub is keyed by channel.** A connection watches `kick:{device_id}` (waiting up to
   2 s for the subscription) before message-svc is asked, then joins `notify:{user_id}`.
   Kicks go to a channel of their own per connection and are handled first; server frames
   queue up to 64 per connection (a dropped one is recovered by listing); rings coalesce.
   A connection's id is a UUIDv7 taken when it starts watching its kick channel, so ids
   sort by time. A takeover is published by the newcomer as
   `Kick{TAKEN_OVER, connection_id}` on `kick:{device_id}`, and a connection obeys it only
   from a strictly newer id: every older connection of the device, on any gateway, closes
   with 4003, while an abandoned slow `Identify` that announces itself after a newer
   connection closes nobody. A kick lost while Redis is down is caught at the next `ReAuth`
   (within 15 minutes), when message-svc refuses a revoked device or a token older than a
   reset.
7. **`ReAuth`** carries a fresh token for the same user and device; the gateway checks it,
   asks message-svc again, and answers nothing when it passes. `AuthExpiring` comes 60 s
   before `exp`; the 4002 deadline is `exp + 30 s`. When the keys or message-svc cannot be
   reached, a retryable `Error` asks for the `ReAuth` again; the deadline still applies.
8. **Groups: one switch, `PUSH_GROUPS` (chart `gateway.groups`, default off).** Off, the six
   group requests are answered `NOT_AVAILABLE` (a new error code) with their `request_id`.
   Nothing else needs filtering: without `CreateGroup` no group can exist, so no group
   entries, receipts or listings can reach a client. message-svc keeps full support.
9. **Rate limits live in the gateway, in Redis.** Only the gateway knows the client's IP,
   and its Redis is already there; counting in Redis makes the limits hold across gateways.
   `RedeemInvite` 60 an hour per IP and 20 per user (50-bit codes), `ContactRequest` 120 per
   IP and 30 per user, `CreateInvite` 20 per user: over a limit, `RATE_LIMITED` (new) with
   `retry_after_s` (a new optional field of `Error`); Redis silent for 500 ms, a retryable
   `UNAVAILABLE` — fail closed like auth-svc. `Identify` is limited to 30 a minute per device
   but fails open: devices must keep connecting while Redis is down (spec §12, test 4). Per
   connection, a token bucket of 100 frames refilled at 20 a second; beyond it, 4004. The
   budget assumes the SDK acks only the highest `seq` it holds while a backlog arrives
   (Plan 9): that is one `Ack` per `Sync` batch of up to 500 entries — 20 acks for 10,000
   waiting entries, far inside the burst of 100, each batch waiting for the ack's round
   trip — while a client that acked every `Deliver` would spend 500 frames a batch and be
   closed. The client IP comes from `PUSH_CLIENT_IP_HEADER` (`x-real-ip`), with auth-svc's
   trust rule. The fixed-window code mirrors auth-svc's (`push-auth` is too heavy a
   dependency for the gateway); a shared crate is deferred.
10. **Contact requests never reveal which ids exist** (spec §5.5, owner rule). A request to
    an id that is not a verified user is answered `REQUESTED_OUT`, like a request nobody
    answered yet, and stores and announces nothing; `Accept` and `Decline` answer it
    `NO_PENDING_REQUEST`, `Block` and `Unblock` succeed. `USER_NOT_FOUND` now only means the
    caller itself is unknown. The requester's `ListContacts` does not show such a request
    (owner decision 1).
11. **Message size.** A WebSocket message is at most 128 KiB (`MAX_FRAME_BYTES`); above it,
    1009. WebSocket frames may be up to 512 KiB so a message just over the limit is read
    whole and the close frame reaches the client (a frame refused from its header leaves
    unread bytes behind, and the client sees a reset instead).
12. **Liveness.** Sending a frame may take 10 s; a client silent for two and a half
    heartbeats (75 s by default) is closed with 1008; the hub pings Redis every 30 s and
    reconnects (subscribing again and ringing everyone) when no answer comes within 5 s,
    which catches a half-open connection; the gRPC channel sends HTTP/2 pings every 30 s
    (10 s timeout) and uses TCP keepalive; every connection syncs every 5 minutes, its
    first sweep spread by its id.
13. **Draining.** On shutdown the connections close with 4005 a hundred every 50 ms (at most
    5 s), inside the 10 s the gateway waits and the 30 s grace period; the preStop sleep of
    the chart stays.
14. **Receipts may arrive out of order.** Two acks committed together (READ COMMITTED)
    publish in either order, so clients keep the highest position they have seen (spec
    §7.2); the gateway forwards what Redis gives it.
15. **The DEV ONLY identity is gone.** `Identify` has only `token`; field numbers 14 and 15
    and their names are reserved; a gateway given `PUSH_DEV_AUTH=1` refuses to start, so a
    stale deployment fails loudly. The smoke test identifies with its real token from
    Task 5.

## Owner decisions

Decided by the owner on 2026-10-14, after review; recorded in the spec.

1. **The id-existence check through one's own contact list is accepted.** A request to an id
   that is not a user is answered "pending" (`REQUESTED_OUT`) like one to a real user who
   has not answered, but stores nothing, so it does not show in the requester's
   `ListContacts`: someone who already holds an id can tell with two calls whether it is a
   user. Ids carry 74 random bits and are learnt only from someone who has them, handle
   lookup already tells whether a handle exists, and requests are limited to 30 an hour per
   user. No "ghost" pending rows.
2. **The rate limits stay as proposed** for the first release, to revisit with real usage:
   `RedeemInvite` 20 an hour per user and 60 per IP, `ContactRequest` 30 an hour per user
   and 120 per IP, `CreateInvite` 20 an hour per user.
3. **Revocation while Redis is down: the 15-minute bound is accepted.** A kick lost while
   Redis is down leaves a revoked device connected until its next `ReAuth` is refused, at
   most 15 minutes; the gateway does not re-check every connection when its subscription
   returns. Revisit with the load test of Plan 11.

## Deferred items addressed

| Item (deferred to Plan 8) | Where |
|---|---|
| Local JWT checks: EdDSA via the JWKS, alg pinned, `exp` required, ~60 s leeway, `aud`, `kid` for rotation | Task 4 (decision 3) |
| Refuse tokens issued before the last password reset; a lost reset kick covered | Task 3 (decision 2), used from Task 5 |
| Every client frame to message-svc; server frames (Deliver, receipts, contact changes, read positions, kicks) | Tasks 1, 7, 8 |
| Identify/ReAuth, heartbeats, close codes aligned with the owner's decisions, draining | Tasks 5, 6, 7, 10 (decisions 4, 7, 13) |
| Remove the DEV ONLY identity; reserve fields 14/15 and their names | Task 5 (decision 15) |
| Groups refused behind one switch | Task 8 (decision 8) |
| Per-frame size limit; WebSocket limit vs `MAX_FRAME_BYTES`; the error or 1009 | Task 8 (decision 11) |
| Rate limits on `RedeemInvite`, `ContactRequest`, `CreateInvite`; frames per second; fail closed | Task 9 (decision 9) |
| `ContactRequest`'s `USER_NOT_FOUND` reveals ids | Task 2 (decision 10) |
| The Identify oracle (`UNKNOWN_DEVICE` vs `UNAVAILABLE`) | Tasks 3, 5 (decision 5) |
| Subscribe to `kick:{device}` before Identify | Task 7 (decision 6) |
| A retryable close of its own for a device not relayed yet; a quieter log line | Tasks 3, 5 (decisions 4, 5) |
| Send write timeout, idle read timeout, hub half-open detection, HTTP/2 keepalive | Task 10 (decision 12) |
| The pre-Ready 4001-vs-4002 question of spec §7.4 | Task 5 (decision 4: 1002) |
| Sync advances only on ack; pending checks probe `entries_after(…, 1)` | Tasks 10, 11 (rule above; the sweep syncs) |
| Group receipt ordering under READ COMMITTED: clients keep the highest | Task 11 (decision 14) |
| The Messaging gRPC API trusts any `user_id`: restrict it to the gateway | Plan 8B (a token of its own, like Accounts and Directory) |

## File structure

```
crates/push-proto/proto/push/v1/frame.proto   Frame gets ReAuth, Edit, Delete, ReadUpTo,
                                                Request (5-9) and Response, Receipt,
                                                ReadPosition, ContactEvent, AuthExpiring
                                                (37-41); 4 error codes; retry_after_s;
                                                Identify's dev fields reserved
crates/push-proto/proto/push/internal/v1/messaging.proto   IdentifyRequest.issued_at;
                                                AccountUser.reset_at; Kick.connection_id,
                                                Kick.reset_at, KICK_REASON_TAKEN_OVER
crates/push-proto/src/lib.rs, codec.rs        close codes 4006, 1002, 1008, 1009, 1011
crates/push-domain/src/contacts.rs            unknown peers answered as strangers
crates/push-domain/src/service.rs, error.rs, model.rs, store.rs, memory.rs, notify.rs,
  lib.rs, invites.rs                          password resets recorded; Identify by iat
crates/push-store/migrations/0003_password_reset.sql   NEW: known_user.password_reset_at
crates/push-store/src/store.rs                device_state with the reset; keep the latest
crates/push-testkit/src/…                     scenarios and contract for both
crates/push-auth/src/outbox.rs, service.rs    reset_at in user.password_reset
crates/push-worker/src/accounts.rs            relays reset_at
crates/push-message-svc/src/grpc.rs, convert.rs, notify.rs   issued_at, reset_at; new codes
crates/push-gateway/src/tokens.rs             NEW: JWKS keys by kid, retired keys, checks
crates/push-gateway/src/hub.rs                rebuilt: channels, kicks, server frames, PING
crates/push-gateway/src/limits.rs             NEW: Redis rate limits, frame budget
crates/push-gateway/src/drain.rs              NEW: batched closes at shutdown
crates/push-gateway/src/session.rs            token Identify, ReAuth, the new frames, groups
crates/push-gateway/src/server.rs             Identify/ReAuth, kicks, frames, limits, liveness
crates/push-gateway/src/config.rs, lib.rs, Cargo.toml   PUSH_AUTH_URL, PUSH_TOKEN_AUDIENCE,
                                                PUSH_GROUPS, PUSH_CLIENT_IP_HEADER
crates/push-gateway/tests/gateway.rs, hub.rs  the gateway against real tokens and Redis
crates/push-smoke/src/lib.rs, kicks.rs, tests/local.rs   identifies with its token
crates/push-worker/tests/relay.rs, server.rs, crates/push-auth/tests/accounts.rs   reset_at
deploy/helm/…                                 the gateway's settings; test-chart.ps1
README.md, roadmap                            docs
```

---

### Task 1: Every client and server frame of spec §7, and the new codes

**Files:**
- Modify: `crates/push-gateway/src/session.rs`, `crates/push-message-svc/src/convert.rs`, `crates/push-proto/proto/push/v1/frame.proto`, `crates/push-proto/src/codec.rs`, `crates/push-proto/src/lib.rs`

The frame protocol gets every client frame of §7.1 (`ReAuth`, `Edit`, `Delete`, `ReadUpTo`,
`Request`, fields 5–9) and every server frame of §7.2 (`Response`, `Receipt`,
`ReadPosition`, `ContactEvent`, `AuthExpiring`, fields 37–41), the error codes
`RATE_LIMITED`, `NOT_AVAILABLE`, `PASSWORD_CHANGED` and `DEVICE_NOT_READY`, `Error`'s
optional `retry_after_s`, and the close codes 4006, 1002, 1008, 1009 and 1011. A codec test
proves the new frames survive encoding; the gateway's session and message-svc's error
conversion follow the new enums.

- [ ] **Step 1: Make the changes**

Change `crates/push-gateway/src/session.rs`:

```diff
diff --git a/crates/push-gateway/src/session.rs b/crates/push-gateway/src/session.rs
index 8c6880e..5cd1d25 100644
--- a/crates/push-gateway/src/session.rs
+++ b/crates/push-gateway/src/session.rs
@@ -207,6 +207,7 @@ pub(crate) fn error_frame(code: ErrorCode, message: &str) -> Body {
         retryable: false,
         client_msg_id: None,
         request_id: None,
+        retry_after_s: None,
     })
 }
 
```

Change `crates/push-message-svc/src/convert.rs`:

```diff
diff --git a/crates/push-message-svc/src/convert.rs b/crates/push-message-svc/src/convert.rs
index 3732910..6fb96e8 100644
--- a/crates/push-message-svc/src/convert.rs
+++ b/crates/push-message-svc/src/convert.rs
@@ -35,6 +35,7 @@ pub(crate) fn invalid(message: impl Into<String>) -> Status {
             retryable: false,
             client_msg_id: None,
             request_id: None,
+            retry_after_s: None,
         },
     )
 }
@@ -80,6 +81,7 @@ pub(crate) fn domain_error(error: DomainError) -> Status {
             retryable: error.is_retryable() || matches!(error, DomainError::Store(_)),
             client_msg_id: None,
             request_id: None,
+            retry_after_s: None,
         },
     )
 }
```

Change `crates/push-proto/proto/push/v1/frame.proto`:

```diff
diff --git a/crates/push-proto/proto/push/v1/frame.proto b/crates/push-proto/proto/push/v1/frame.proto
index 4ea4ab9..b70a2e5 100644
--- a/crates/push-proto/proto/push/v1/frame.proto
+++ b/crates/push-proto/proto/push/v1/frame.proto
@@ -16,23 +16,30 @@ package push.v1;
 
 message Frame {
   oneof body {
-    // Client to server.
+    // Client to server (spec §7.1).
     Identify identify = 1;
     Send send = 2;
     Ack ack = 3;
     Ping ping = 4;
+    ReAuth re_auth = 5;
+    Edit edit = 6;
+    Delete delete = 7;
+    ReadUpTo read_up_to = 8;
+    Request request = 9;
 
-    // Server to client.
+    // Server to client (spec §7.2).
     Ready ready = 32;
     SendAck send_ack = 33;
     Deliver deliver = 34;
     Error error = 35;
     Pong pong = 36;
+    Response response = 37;
+    Receipt receipt = 38;
+    ReadPosition read_position = 39;
+    ContactEvent contact_event = 40;
+    AuthExpiring auth_expiring = 41;
   }
-  // 5-31: the other client frames of spec §7.1 (ReAuth, Edit, Delete, ReadUpTo and the
-  // request frames); 37-63: the other server frames of §7.2. Assigned as they are added:
-  // Edit, Delete, ReadUpTo, Request and Response are defined below and message-svc serves
-  // them over gRPC (Plan 7); the gateway carries them in a Frame from Plan 8.
+  // 10-31 are free for later client frames, 42-63 for later server frames.
 }
 
 // Must be the first frame, within 5 seconds of connecting.
@@ -56,6 +63,50 @@ message Ready {
   uint32 heartbeat_ms = 3;
 }
 
+// A fresh access token for an identified connection (spec §7.3), sent after
+// AuthExpiring. It must name the same user and device. Nothing answers a good one; a bad
+// one closes the connection (4001, or 4002 when it has expired too).
+message ReAuth {
+  string token = 1;
+}
+
+// The access token of the connection expires in expires_in_s seconds: refresh it over
+// HTTPS and send ReAuth. Without a valid ReAuth by 30 seconds after the expiry, the
+// connection is closed with 4002.
+message AuthExpiring {
+  uint32 expires_in_s = 1;
+}
+
+// A receipt position moved in one of the user's conversations (spec §4.5). Under
+// concurrent acks two receipts can arrive in either order: keep the highest position.
+message Receipt {
+  bytes conversation_id = 1;
+  oneof positions {
+    DirectReceipt direct = 2;
+    GroupReceipts group = 3;
+  }
+}
+
+// The other member of a direct conversation: everything up to delivered_seq reached one
+// of their devices, everything up to read_seq was read.
+message DirectReceipt {
+  bytes user_id = 1;
+  int64 delivered_seq = 2;
+  int64 read_seq = 3;
+}
+
+// The user's own read position moved, from this or another of their devices.
+message ReadPosition {
+  bytes conversation_id = 1;
+  int64 read_seq = 2;
+}
+
+// The user's relationship with another user changed, as the user sees it.
+message ContactEvent {
+  bytes user_id = 1;
+  ContactState state = 2;
+}
+
 enum ContentType {
   CONTENT_TYPE_UNSPECIFIED = 0;
   CONTENT_TYPE_TEXT = 1;
@@ -455,6 +506,14 @@ enum ErrorCode {
   ERROR_CODE_NOT_ADMIN = 21;
   // A group must keep at least one admin.
   ERROR_CODE_LAST_ADMIN = 22;
+  // Too many requests of this kind; retry after retry_after_s seconds.
+  ERROR_CODE_RATE_LIMITED = 23;
+  // This server does not offer the feature (groups, in the first release).
+  ERROR_CODE_NOT_AVAILABLE = 24;
+  // The password was reset after the access token was issued: sign in again.
+  ERROR_CODE_PASSWORD_CHANGED = 25;
+  // The device was registered moments ago and is not ready yet: retry shortly.
+  ERROR_CODE_DEVICE_NOT_READY = 26;
 }
 
 message Error {
@@ -466,4 +525,6 @@ message Error {
   optional bytes client_msg_id = 4;
   // Set when the error answers a Request.
   optional uint64 request_id = 5;
+  // RATE_LIMITED: when the same frame may succeed again.
+  optional uint32 retry_after_s = 6;
 }
```

Change `crates/push-proto/src/codec.rs`:

```diff
diff --git a/crates/push-proto/src/codec.rs b/crates/push-proto/src/codec.rs
index c56a60f..92cf001 100644
--- a/crates/push-proto/src/codec.rs
+++ b/crates/push-proto/src/codec.rs
@@ -103,6 +103,7 @@ pub fn error_from_status(status: &tonic::Status) -> Error {
             retryable: true,
             client_msg_id: None,
             request_id: None,
+            retry_after_s: None,
         },
     }
 }
@@ -151,6 +152,47 @@ mod tests {
         );
     }
 
+    #[test]
+    fn the_request_and_server_frames_survive_encoding() {
+        use crate::v1::{
+            ContactEvent, ContactState, DirectReceipt, ListContacts, ReAuth, ReadPosition, Receipt,
+            Request, Response, receipt, request, response,
+        };
+        let conversation = id_bytes(Uuid::now_v7());
+        let frames = [
+            Body::ReAuth(ReAuth {
+                token: "a.b.c".to_owned(),
+            }),
+            Body::Request(Request {
+                request_id: 7,
+                body: Some(request::Body::ListContacts(ListContacts {})),
+            }),
+            Body::Response(Response {
+                request_id: 7,
+                body: Some(response::Body::Done(crate::v1::Done {})),
+            }),
+            Body::Receipt(Receipt {
+                conversation_id: conversation.clone(),
+                positions: Some(receipt::Positions::Direct(DirectReceipt {
+                    user_id: id_bytes(Uuid::now_v7()),
+                    delivered_seq: 4,
+                    read_seq: 3,
+                })),
+            }),
+            Body::ReadPosition(ReadPosition {
+                conversation_id: conversation,
+                read_seq: 3,
+            }),
+            Body::ContactEvent(ContactEvent {
+                user_id: id_bytes(Uuid::now_v7()),
+                state: ContactState::Accepted.into(),
+            }),
+        ];
+        for frame in frames {
+            assert_eq!(decode_frame(&encode_frame(frame.clone())), Ok(frame));
+        }
+    }
+
     #[test]
     fn a_frame_without_a_body_is_empty() {
         assert_eq!(decode_frame(&[]), Err(FrameError::Empty));
@@ -205,6 +247,7 @@ mod tests {
             retryable: false,
             client_msg_id: Some(id_bytes(Uuid::now_v7())),
             request_id: Some(7),
+            retry_after_s: None,
         };
         let status = error_status(tonic::Code::FailedPrecondition, error.clone());
         assert_eq!(status.code(), tonic::Code::FailedPrecondition);
@@ -227,6 +270,7 @@ mod tests {
             retryable: false,
             client_msg_id: None,
             request_id: None,
+            retry_after_s: None,
         };
         let status = error_status(tonic::Code::FailedPrecondition, error.clone());
         let rich = RpcStatus::decode(status.details()).unwrap();
```

Change `crates/push-proto/src/lib.rs`:

```diff
diff --git a/crates/push-proto/src/lib.rs b/crates/push-proto/src/lib.rs
index 45be2e5..7cafd3d 100644
--- a/crates/push-proto/src/lib.rs
+++ b/crates/push-proto/src/lib.rs
@@ -26,17 +26,32 @@ pub use codec::{
     error_from_status, error_status, id, id_bytes, kick_channel, notify_channel,
 };
 
-/// WebSocket close codes (spec §7.4).
+/// WebSocket close codes (spec §7.4). Codes from 4000 are the protocol's own; the others
+/// are RFC 6455's. A client signs out only on 4001; it never reconnects after 4003; on
+/// every other code it reconnects with backoff (after refreshing its token on 4002).
 pub mod close {
-    /// Invalid token or identity, unverified email, or unknown or revoked device: sign in
-    /// again.
+    /// The access token is invalid (signature, audience, an unverified email) or older
+    /// than the user's last password reset: sign in again.
     pub const INVALID_IDENTITY: u16 = 4001;
-    /// Re-authentication required: refresh, then reconnect.
+    /// The access token expired: refresh it, then reconnect.
     pub const REAUTH_REQUIRED: u16 = 4002;
-    /// Another connection took over, or the device was revoked: stop.
+    /// Another connection of the device took over, or the device was revoked: stop.
     pub const TAKEN_OVER: u16 = 4003;
     /// Rate limited: back off, then reconnect.
     pub const RATE_LIMITED: u16 = 4004;
     /// The server is shutting down: back off with jitter, then reconnect.
     pub const SHUTTING_DOWN: u16 = 4005;
+    /// The device was registered moments ago and the server is not ready for it yet:
+    /// reconnect after a short backoff. Never a reason to sign out.
+    pub const NOT_READY_YET: u16 = 4006;
+    /// A protocol violation before `Ready`: no `Identify` within 5 s, or another frame
+    /// first. Reconnect with backoff.
+    pub const PROTOCOL_ERROR: u16 = 1002;
+    /// The client stopped sending frames (no `Ping` within the idle timeout).
+    pub const POLICY_VIOLATION: u16 = 1008;
+    /// A message larger than the WebSocket limit.
+    pub const MESSAGE_TOO_BIG: u16 = 1009;
+    /// The server cannot serve the connection now (message-svc or the token keys are
+    /// unreachable): reconnect with backoff.
+    pub const INTERNAL_ERROR: u16 = 1011;
 }
```
- [ ] **Step 2: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "feat(proto): every client and server frame of spec 7, new error and close codes"
```

Expected: 823 tests pass, no warnings, no diff.
---

### Task 2: Contact requests never reveal which ids exist

**Files:**
- Modify: `crates/push-domain/src/contacts.rs`, `crates/push-message-svc/src/requests/tests.rs`, `crates/push-proto/proto/push/v1/frame.proto`, `crates/push-testkit/src/scenarios/contacts.rs`

Decision 10 and owner decision 1. A request to an id that is not a verified user is
answered `REQUESTED_OUT` and stores and announces nothing; `Accept` and `Decline` answer it
`NO_PENDING_REQUEST`, `Block` and `Unblock` succeed and store nothing; `USER_NOT_FOUND`
only means the caller is unknown. The testkit scenario runs against the in-memory store and
Postgres.

- [ ] **Step 1: Make the changes**

Change `crates/push-domain/src/contacts.rs`:

```diff
diff --git a/crates/push-domain/src/contacts.rs b/crates/push-domain/src/contacts.rs
index d481cc1..a3773a0 100644
--- a/crates/push-domain/src/contacts.rs
+++ b/crates/push-domain/src/contacts.rs
@@ -13,9 +13,13 @@ where
     C: Clock,
     N: Notifier,
 {
-    /// Sends a contact request. Returns the requester's resulting state.
+    /// Sends a contact request. Returns the requester's resulting state. A request to an
+    /// id that is not a verified user looks like a request nobody answered yet: it stores
+    /// nothing and answers `RequestedOut`, so requests never reveal which ids exist.
     pub async fn contact_request(&self, from: UserId, to: UserId) -> DomainResult<ContactState> {
-        let mut tx = self.begin_pair(from, to).await?;
+        let Some(mut tx) = self.begin_pair(from, to).await? else {
+            return Ok(ContactState::RequestedOut);
+        };
         let mine = tx.get_contact(from, to).await?;
         let theirs = tx.get_contact(to, from).await?;
         let (result, their_new) = match (mine, theirs) {
@@ -43,7 +47,9 @@ where
 
     /// Accepts a request that `peer` sent to `me`.
     pub async fn contact_accept(&self, me: UserId, peer: UserId) -> DomainResult<()> {
-        let mut tx = self.begin_pair(me, peer).await?;
+        let Some(mut tx) = self.begin_pair(me, peer).await? else {
+            return Err(DomainError::NoPendingRequest);
+        };
         if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
             return Err(DomainError::NoPendingRequest);
         }
@@ -64,7 +70,9 @@ where
     /// The requester's pending request disappears without an event: a decline must look
     /// the same to them as a block.
     pub async fn contact_decline(&self, me: UserId, peer: UserId) -> DomainResult<()> {
-        let mut tx = self.begin_pair(me, peer).await?;
+        let Some(mut tx) = self.begin_pair(me, peer).await? else {
+            return Err(DomainError::NoPendingRequest);
+        };
         if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
             return Err(DomainError::NoPendingRequest);
         }
@@ -78,9 +86,11 @@ where
     }
 
     /// Blocks `peer`. Ends any contact; `peer`'s side is removed unless they also block.
-    /// `peer` is never told.
+    /// `peer` is never told. Blocking an id that is not a verified user stores nothing.
     pub async fn contact_block(&self, me: UserId, peer: UserId) -> DomainResult<()> {
-        let mut tx = self.begin_pair(me, peer).await?;
+        let Some(mut tx) = self.begin_pair(me, peer).await? else {
+            return Ok(());
+        };
         let blocked = Some(ContactState::Blocked);
         let mut events = Vec::new();
         // The block records whether `me` could see `peer` until now, so that blocking a
@@ -104,7 +114,9 @@ where
 
     /// Removes a block. A request the block had hidden becomes an ordinary pending request.
     pub async fn contact_unblock(&self, me: UserId, peer: UserId) -> DomainResult<()> {
-        let mut tx = self.begin_pair(me, peer).await?;
+        let Some(mut tx) = self.begin_pair(me, peer).await? else {
+            return Ok(());
+        };
         let mut events = Vec::new();
         if tx.get_contact(me, peer).await? == Some(ContactState::Blocked) {
             let restored = match tx.get_contact(peer, me).await? {
@@ -170,17 +182,24 @@ where
         Ok(visible)
     }
 
-    /// Begins a transaction for an operation between two distinct, known users.
-    async fn begin_pair(&self, a: UserId, b: UserId) -> DomainResult<S::Tx> {
-        if a == b {
+    /// Begins a transaction for an operation of `me` on `peer`, two distinct users. `None`
+    /// when `peer` is not a verified user: the operation then answers as it would for a
+    /// user `me` has no relationship with, so no answer reveals which ids exist (spec
+    /// §5.5). An unknown `me` is `UnknownUser`: the gateway only acts for known devices.
+    async fn begin_pair(&self, me: UserId, peer: UserId) -> DomainResult<Option<S::Tx>> {
+        if me == peer {
             return Err(ValidationError::SelfTarget.into());
         }
         let mut tx = self.store.begin().await?;
-        if !tx.user_exists(a).await? || !tx.user_exists(b).await? {
+        if !tx.user_exists(me).await? {
             return Err(DomainError::UnknownUser);
         }
-        tx.lock_contact_pair(a, b).await?;
-        Ok(tx)
+        if !tx.user_exists(peer).await? {
+            tx.rollback().await?;
+            return Ok(None);
+        }
+        tx.lock_contact_pair(me, peer).await?;
+        Ok(Some(tx))
     }
 }
 
```

Change `crates/push-proto/proto/push/v1/frame.proto`:

```diff
diff --git a/crates/push-proto/proto/push/v1/frame.proto b/crates/push-proto/proto/push/v1/frame.proto
index b70a2e5..b8dedb2 100644
--- a/crates/push-proto/proto/push/v1/frame.proto
+++ b/crates/push-proto/proto/push/v1/frame.proto
@@ -333,7 +333,9 @@ message DirectOpened {
 }
 
 // Asks another user to become a contact (spec §5.5). If they already asked the caller,
-// both become accepted contacts at once.
+// both become accepted contacts at once. An id that is not a verified user is answered
+// REQUESTED_OUT and nothing is stored, so requests never reveal which ids exist; Accept
+// and Decline answer it NO_PENDING_REQUEST, Block and Unblock succeed.
 message ContactRequest {
   bytes user_id = 1;
 }
@@ -485,7 +487,8 @@ enum ErrorCode {
   ERROR_CODE_UNAVAILABLE = 9;
   // Any other rejection; `message` says which.
   ERROR_CODE_REJECTED = 10;
-  // The other user does not exist (or has not verified their email yet).
+  // The caller is not a known user. Never answers about another user's id: requests
+  // name unknown ids as strangers, so no answer reveals which ids exist.
   ERROR_CODE_USER_NOT_FOUND = 11;
   // Accept or Decline without a request from that user.
   ERROR_CODE_NO_PENDING_REQUEST = 12;
```

Change `crates/push-testkit/src/scenarios/contacts.rs`:

```diff
diff --git a/crates/push-testkit/src/scenarios/contacts.rs b/crates/push-testkit/src/scenarios/contacts.rs
index df7c34d..ffddcc3 100644
--- a/crates/push-testkit/src/scenarios/contacts.rs
+++ b/crates/push-testkit/src/scenarios/contacts.rs
@@ -160,13 +160,18 @@ async fn a_user_cannot_request_themselves<B: Backend>(backend: &B) {
     );
 }
 
-async fn requesting_an_unknown_user_fails<B: Backend>(backend: &B) {
+/// Spec §5.5: no answer reveals which ids exist. A request to an id that is not a verified
+/// user is answered like one nobody has answered yet, and stores and announces nothing.
+async fn a_request_to_an_unknown_id_looks_pending_and_stores_nothing<B: Backend>(backend: &B) {
     let world = World::new(backend).await;
     let alice = world.person().await;
+    world.notifier.take();
     assert_eq!(
         world.svc.contact_request(alice.user, UserId::new()).await,
-        Err(DomainError::UnknownUser)
+        Ok(ContactState::RequestedOut)
     );
+    assert_eq!(world.svc.list_contacts(alice.user).await, Ok(Vec::new()));
+    assert_eq!(world.notifier.take(), []);
 }
 
 async fn a_hidden_request_arrives_when_the_block_is_lifted<B: Backend>(backend: &B) {
@@ -279,7 +284,9 @@ async fn a_request_to_an_accepted_contact_stays_accepted<B: Backend>(backend: &B
     );
 }
 
-async fn contact_operations_reject_self_and_unknown_users<B: Backend>(backend: &B) {
+async fn contact_operations_reject_self_and_treat_unknown_ids_as_strangers<B: Backend>(
+    backend: &B,
+) {
     let world = World::new(backend).await;
     let alice = world.person().await;
     let selfish = Err(DomainError::Invalid(ValidationError::SelfTarget));
@@ -295,19 +302,21 @@ async fn contact_operations_reject_self_and_unknown_users<B: Backend>(backend: &
         world.svc.contact_block(alice.user, alice.user).await,
         selfish
     );
-    let stranger = UserId::new();
-    assert_eq!(
-        world.svc.contact_accept(alice.user, stranger).await,
-        Err(DomainError::UnknownUser)
-    );
-    assert_eq!(
-        world.svc.contact_decline(alice.user, stranger).await,
-        Err(DomainError::UnknownUser)
-    );
-    assert_eq!(
-        world.svc.contact_block(alice.user, stranger).await,
-        Err(DomainError::UnknownUser)
-    );
+    // The same answers as for a verified user Alice has no relationship with.
+    let (unknown, stranger) = (UserId::new(), world.person().await);
+    for peer in [unknown, stranger.user] {
+        assert_eq!(
+            world.svc.contact_accept(alice.user, peer).await,
+            Err(DomainError::NoPendingRequest)
+        );
+        assert_eq!(
+            world.svc.contact_decline(alice.user, peer).await,
+            Err(DomainError::NoPendingRequest)
+        );
+        assert_eq!(world.svc.contact_unblock(alice.user, peer).await, Ok(()));
+    }
+    assert_eq!(world.svc.contact_block(alice.user, unknown).await, Ok(()));
+    assert_eq!(world.svc.contact_state(alice.user, unknown).await, Ok(None));
 }
 
 async fn a_request_and_its_acceptance_are_announced_to_both_users<B: Backend>(backend: &B) {
@@ -714,13 +723,13 @@ pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
         blocking_ends_an_accepted_contact,
         unblocking_allows_a_fresh_request,
         a_user_cannot_request_themselves,
-        requesting_an_unknown_user_fails,
+        a_request_to_an_unknown_id_looks_pending_and_stores_nothing,
         a_hidden_request_arrives_when_the_block_is_lifted,
         the_sender_cannot_decline_their_own_request,
         blocking_cancels_a_pending_request_in_either_direction,
         a_repeated_request_changes_nothing,
         a_request_to_an_accepted_contact_stays_accepted,
-        contact_operations_reject_self_and_unknown_users,
+        contact_operations_reject_self_and_treat_unknown_ids_as_strangers,
         a_request_and_its_acceptance_are_announced_to_both_users,
         crossing_requests_announce_acceptance_to_both_users,
         a_request_that_changes_nothing_publishes_nothing,
```

Change `crates/push-message-svc/src/requests/tests.rs`:

```diff
diff --git a/crates/push-message-svc/src/requests/tests.rs b/crates/push-message-svc/src/requests/tests.rs
index bf8213b..cc0514e 100644
--- a/crates/push-message-svc/src/requests/tests.rs
+++ b/crates/push-message-svc/src/requests/tests.rs
@@ -493,11 +493,18 @@ async fn contacts_are_requested_accepted_and_blocked_and_a_block_freezes_the_cha
     world.request(&bob, unblock).await.unwrap();
     assert!(contacts(&world, &bob).await.is_empty());
 
+    // An id that is no user looks like someone who has not answered yet.
+    let unknown = UserId::new();
     let stranger = request::Body::ContactRequest(ContactRequest {
-        user_id: id_bytes(UserId::new().0),
+        user_id: id_bytes(unknown.0),
     });
-    let status = world.request(&alice, stranger).await.unwrap_err();
-    assert_eq!(failure(&status).0, ErrorCode::UserNotFound);
+    assert_eq!(
+        world.request(&alice, stranger).await.unwrap(),
+        response::Body::Contact(Contact {
+            user_id: id_bytes(unknown.0),
+            state: ContactState::RequestedOut.into(),
+        })
+    );
     let own = request::Body::ContactRequest(ContactRequest {
         user_id: id_bytes(alice.user.0),
     });
```
- [ ] **Step 2: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "fix(domain): contact requests never reveal which ids exist"
```

Expected: 823 tests pass, no warnings, no diff.
---

### Task 3: Identify refuses tokens issued until the last password reset

**Files:**
- Create: `crates/push-store/migrations/0003_password_reset.sql`
- Modify: `crates/push-auth/src/outbox.rs`, `crates/push-auth/src/service.rs`, `crates/push-auth/tests/accounts.rs`, `crates/push-domain/src/error.rs`, `crates/push-domain/src/lib.rs`, `crates/push-domain/src/memory.rs`, `crates/push-domain/src/model.rs`, `crates/push-domain/src/notify.rs`, `crates/push-domain/src/service.rs`, `crates/push-domain/src/store.rs`, `crates/push-gateway/src/server.rs`, `crates/push-message-svc/src/convert.rs`, `crates/push-message-svc/src/grpc.rs`, `crates/push-message-svc/src/notify.rs`, `crates/push-message-svc/tests/service.rs`, `crates/push-proto/proto/push/internal/v1/messaging.proto`, `crates/push-smoke/src/lib.rs`, `crates/push-store/src/store.rs`, `crates/push-testkit/src/concurrency.rs`, `crates/push-testkit/src/contract/users.rs`, `crates/push-testkit/src/scenarios/devices.rs`, `crates/push-testkit/src/world.rs`, `crates/push-worker/src/accounts.rs`, `crates/push-worker/tests/relay.rs`, `crates/push-worker/tests/server.rs`

Decision 2. auth-svc writes `reset_at` (its own clock, the one that sets `iat`) into the
`user.password_reset` event; the worker relays it in `AccountUser.reset_at` (0 for an event
written before the field existed); message-svc keeps the latest reset
(`greatest(password_reset_at, $2)`, falling back to its own clock for 0), publishes the
`PASSWORD_RESET` kick with that time, and `Identify`, which now takes the token's
`issued_at`, refuses a token issued no later than the reset's second with
`UNAUTHENTICATED` (`PASSWORD_CHANGED`). A device not relayed yet is `UNAVAILABLE`
(`DEVICE_NOT_READY`). The testkit scenarios prove, against memory and Postgres: a token
issued before the reset (or within its second) is refused; a token issued after the reset
but before the relay is accepted; a redelivered or older reset signs out no newer session
and its kick carries the latest reset. The gateway passes `issued_at = 0` until Task 5.

- [ ] **Step 1: Make the changes**

Change `crates/push-auth/src/outbox.rs`:

```diff
diff --git a/crates/push-auth/src/outbox.rs b/crates/push-auth/src/outbox.rs
index 2c48369..3e2b933 100644
--- a/crates/push-auth/src/outbox.rs
+++ b/crates/push-auth/src/outbox.rs
@@ -8,7 +8,10 @@
 //! |---|---|
 //! | `email.send` | `{user_id, to, template, params}` |
 //! | `user.verified` | `{user_id}` |
-//! | `user.password_reset` | `{user_id}`: every device of the user is to be kicked |
+//! | `user.password_reset` | `{user_id, reset_at}`: every device of the user is to be kicked |
+//!
+//! `reset_at` is in seconds since the Unix epoch, from the clock that sets the access
+//! tokens' `iat`: message-svc refuses the tokens issued no later than that second.
 //! | `device.registered` | `{user_id, device_id}` |
 //! | `device.revoked` | `{user_id, device_id}` |
 
@@ -65,6 +68,9 @@ pub enum Event {
     },
     UserPasswordReset {
         user_id: Uuid,
+        /// When the password was reset, in seconds since the Unix epoch: the clock that
+        /// sets the access tokens' `iat`.
+        reset_at: i64,
     },
     DeviceRegistered {
         user_id: Uuid,
@@ -97,8 +103,9 @@ impl Event {
                 "template": email.template(),
                 "params": email.params(),
             }),
-            Self::UserVerified { user_id } | Self::UserPasswordReset { user_id } => {
-                json!({"user_id": user_id})
+            Self::UserVerified { user_id } => json!({"user_id": user_id}),
+            Self::UserPasswordReset { user_id, reset_at } => {
+                json!({"user_id": user_id, "reset_at": reset_at})
             }
             Self::DeviceRegistered { user_id, device_id }
             | Self::DeviceRevoked { user_id, device_id } => {
@@ -155,7 +162,13 @@ mod tests {
         let (user_id, device_id) = (Uuid::now_v7(), Uuid::now_v7());
         let cases = [
             (Event::UserVerified { user_id }, "user.verified"),
-            (Event::UserPasswordReset { user_id }, "user.password_reset"),
+            (
+                Event::UserPasswordReset {
+                    user_id,
+                    reset_at: 1_760_000_000,
+                },
+                "user.password_reset",
+            ),
             (
                 Event::DeviceRegistered { user_id, device_id },
                 "device.registered",
@@ -173,5 +186,13 @@ mod tests {
             Event::DeviceRevoked { user_id, device_id }.payload()["device_id"],
             json!(device_id)
         );
+        assert_eq!(
+            Event::UserPasswordReset {
+                user_id,
+                reset_at: 1_760_000_000
+            }
+            .payload()["reset_at"],
+            json!(1_760_000_000)
+        );
     }
 }
```

Change `crates/push-auth/src/service.rs`:

```diff
diff --git a/crates/push-auth/src/service.rs b/crates/push-auth/src/service.rs
index c598806..61b4d84 100644
--- a/crates/push-auth/src/service.rs
+++ b/crates/push-auth/src/service.rs
@@ -496,10 +496,13 @@ impl Auth {
             )
             .await?;
         }
+        // The clock that sets the access tokens' `iat`: message-svc refuses every token
+        // issued no later than this second, and only those.
         outbox::write(
             &mut tx,
             &Event::UserPasswordReset {
                 user_id: claims.sub,
+                reset_at: unix_now(),
             },
         )
         .await?;
```

Change `crates/push-domain/src/error.rs`:

```diff
diff --git a/crates/push-domain/src/error.rs b/crates/push-domain/src/error.rs
index 821c770..3c16e15 100644
--- a/crates/push-domain/src/error.rs
+++ b/crates/push-domain/src/error.rs
@@ -12,6 +12,10 @@ pub enum DomainError {
     /// has not relayed it (that takes about a second). Unlike `UnknownDevice`, it passes.
     #[error("the device is not known yet; retry shortly")]
     DeviceNotKnownYet,
+    /// The access token was issued no later than the user's last password reset that
+    /// message-svc heard of: the device must sign in again.
+    #[error("the password was changed after this sign-in; sign in again")]
+    PasswordChanged,
     #[error("conversation not found")]
     ConversationNotFound,
     #[error("not a member of this conversation")]
```

Change `crates/push-domain/src/lib.rs`:

```diff
diff --git a/crates/push-domain/src/lib.rs b/crates/push-domain/src/lib.rs
index d94ac1f..870f4b9 100644
--- a/crates/push-domain/src/lib.rs
+++ b/crates/push-domain/src/lib.rs
@@ -32,8 +32,9 @@ pub use ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
 pub use memory::InMemoryStore;
 pub use model::{
     ContactState, ContactSummary, ContentType, Conversation, ConversationKind,
-    ConversationOverview, ConversationState, ConversationSummary, Entry, EntryKind, GroupPositions,
-    Invite, InviteRecord, Member, MemberState, Positions, Receipts, Role, SendReceipt, direct_key,
+    ConversationOverview, ConversationState, ConversationSummary, DeviceState, Entry, EntryKind,
+    GroupPositions, Invite, InviteRecord, Member, MemberState, Positions, Receipts, Role,
+    SendReceipt, direct_key,
 };
 pub use notify::{Event, KickReason, NoopNotifier, Notifier, ReceiptUpdate, RecordingNotifier};
 pub use send::SendText;
```

Change `crates/push-domain/src/memory.rs`:

```diff
diff --git a/crates/push-domain/src/memory.rs b/crates/push-domain/src/memory.rs
index eb87ac5..e5f0ea4 100644
--- a/crates/push-domain/src/memory.rs
+++ b/crates/push-domain/src/memory.rs
@@ -14,8 +14,8 @@ use tokio::sync::{Mutex, OwnedMutexGuard};
 use crate::error::{DomainError, DomainResult};
 use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
 use crate::model::{
-    ContactState, Conversation, ConversationKind, ConversationOverview, Entry, EntryKind,
-    GroupPositions, InviteRecord, Member, Role,
+    ContactState, Conversation, ConversationKind, ConversationOverview, DeviceState, Entry,
+    EntryKind, GroupPositions, InviteRecord, Member, Role,
 };
 use crate::store::{InsertOutcome, Store, Tx};
 
@@ -26,6 +26,8 @@ fn missing(what: &str) -> DomainError {
 #[derive(Debug, Clone, Default)]
 struct State {
     users: HashSet<UserId>,
+    /// The last password reset message-svc heard of, per user.
+    password_resets: HashMap<UserId, DateTime<Utc>>,
     devices: HashMap<DeviceId, DeviceRow>,
     contacts: HashMap<(UserId, UserId), ContactState>,
     /// Blocked rows whose owner could see the peer when blocking.
@@ -236,6 +238,19 @@ impl Tx for InMemoryTx {
         Ok(self.work.users.contains(&user))
     }
 
+    async fn record_password_reset(
+        &mut self,
+        user: UserId,
+        at: DateTime<Utc>,
+    ) -> DomainResult<DateTime<Utc>> {
+        if !self.work.users.contains(&user) {
+            return Err(missing("user"));
+        }
+        let latest = self.work.password_resets.entry(user).or_insert(at);
+        *latest = (*latest).max(at);
+        Ok(*latest)
+    }
+
     async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool> {
         if let Some(row) = self.work.devices.get(&device) {
             return Ok(row.user == user && !row.revoked);
@@ -264,12 +279,12 @@ impl Tx for InMemoryTx {
         Ok(self.work.devices.get(&device).map(|row| row.user))
     }
 
-    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<(UserId, bool)>> {
-        Ok(self
-            .work
-            .devices
-            .get(&device)
-            .map(|row| (row.user, !row.revoked)))
+    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<DeviceState>> {
+        Ok(self.work.devices.get(&device).map(|row| DeviceState {
+            user: row.user,
+            active: !row.revoked,
+            password_reset_at: self.work.password_resets.get(&row.user).copied(),
+        }))
     }
 
     async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
```

Change `crates/push-domain/src/model.rs`:

```diff
diff --git a/crates/push-domain/src/model.rs b/crates/push-domain/src/model.rs
index 9d172c8..805256b 100644
--- a/crates/push-domain/src/model.rs
+++ b/crates/push-domain/src/model.rs
@@ -22,6 +22,17 @@ pub struct ContactSummary {
     pub state: ContactState,
 }
 
+/// What `Identify` reads about a connecting device, in one statement.
+#[derive(Debug, Clone, Copy, PartialEq, Eq)]
+pub struct DeviceState {
+    /// The owner.
+    pub user: UserId,
+    /// Not revoked.
+    pub active: bool,
+    /// When message-svc last heard that the owner's password was reset, if ever.
+    pub password_reset_at: Option<DateTime<Utc>>,
+}
+
 /// A stored invite (spec §5.5). Redeeming it makes the redeemer an accepted contact of
 /// its creator.
 #[derive(Debug, Clone, PartialEq, Eq)]
```

Change `crates/push-domain/src/notify.rs`:

```diff
diff --git a/crates/push-domain/src/notify.rs b/crates/push-domain/src/notify.rs
index 95b01f8..b2cd535 100644
--- a/crates/push-domain/src/notify.rs
+++ b/crates/push-domain/src/notify.rs
@@ -20,6 +20,7 @@
 use std::sync::{Arc, Mutex};
 
 use async_trait::async_trait;
+use chrono::{DateTime, Utc};
 
 use crate::ids::{ConversationId, DeviceId, UserId};
 use crate::model::ContactState;
@@ -62,9 +63,10 @@ pub enum Event {
 pub enum KickReason {
     /// The device was revoked: it must stop.
     DeviceRevoked,
-    /// The user's password was reset: every refresh token is revoked, so the device must
-    /// sign in again.
-    PasswordReset,
+    /// The user's password was reset at `reset_at`: every refresh token is revoked, so the
+    /// device must sign in again. A connection whose access token was issued after the
+    /// second of `reset_at` signed in after the reset and is not closed.
+    PasswordReset { reset_at: DateTime<Utc> },
 }
 
 /// The positions a receipt reports.
```

Change `crates/push-domain/src/service.rs`:

```diff
diff --git a/crates/push-domain/src/service.rs b/crates/push-domain/src/service.rs
index b535558..24fce87 100644
--- a/crates/push-domain/src/service.rs
+++ b/crates/push-domain/src/service.rs
@@ -1,4 +1,4 @@
-use chrono::TimeDelta;
+use chrono::{DateTime, TimeDelta, Utc};
 
 use crate::clock::Clock;
 use crate::error::{DomainError, DomainResult};
@@ -126,19 +126,33 @@ where
         Ok(())
     }
 
-    /// auth-svc reset `user`'s password (`user.password_reset`, relayed by the worker):
-    /// every active device of the user is kicked, to sign in again with the new password.
-    /// The devices stay registered. Changes nothing, so a repeated event only kicks again;
-    /// a user message-svc has not heard of has no devices to kick.
-    pub async fn password_reset(&self, user: UserId) -> DomainResult<()> {
+    /// auth-svc reset `user`'s password at `reset_at` (`user.password_reset`, relayed by
+    /// the worker): every active device of the user is kicked, to sign in again with the
+    /// new password, and [`Self::identify`] refuses access tokens issued no later than that
+    /// second, so a token that outlives the reset (up to 15 minutes) or a lost kick cannot
+    /// keep a device signed in. `reset_at` is auth-svc's time, the clock of the tokens'
+    /// `iat`; an event from before auth-svc sent it (`None`) takes message-svc's clock.
+    ///
+    /// The devices stay registered. Idempotent and correct in any order: the user is
+    /// recorded if message-svc has not heard of them yet, the latest reset is kept, so a
+    /// redelivered or older event moves nothing, and the kick carries the latest reset,
+    /// so it spares a session that signed in after it.
+    pub async fn password_reset(
+        &self,
+        user: UserId,
+        reset_at: Option<DateTime<Utc>>,
+    ) -> DomainResult<()> {
+        let reset_at = reset_at.unwrap_or_else(|| self.clock.now());
         let mut tx = self.store.begin().await?;
+        tx.insert_user(user).await?;
+        let reset_at = tx.record_password_reset(user, reset_at).await?;
         let mut devices = tx.devices_of_user(user).await?;
-        tx.rollback().await?;
+        tx.commit().await?;
         if !devices.is_empty() {
             devices.sort();
             self.publish(vec![Event::Kick {
                 devices,
-                reason: KickReason::PasswordReset,
+                reason: KickReason::PasswordReset { reset_at },
             }])
             .await;
         }
@@ -152,20 +166,35 @@ where
         }
     }
 
-    /// Confirms that a connecting device is active and belongs to the user in its token.
-    /// A device never heard of is `DeviceNotKnownYet` (retryable): auth-svc registered it
-    /// and the worker has not relayed it yet. A revoked device, or one of another user, is
-    /// `UnknownDevice`.
-    pub async fn identify(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
+    /// Confirms that a connecting device is active and belongs to the user in its token,
+    /// a token issued at `issued_at` (its `iat`, whole seconds). A device never heard of is
+    /// `DeviceNotKnownYet` (retryable): auth-svc registered it and the worker has not
+    /// relayed it yet. A revoked device, or one of another user, is `UnknownDevice`. A
+    /// token issued no later than the second of the user's last password reset is
+    /// `PasswordChanged`: within that second the order is unknown, and refusing a token
+    /// minted just after the reset only asks for one more sign-in.
+    pub async fn identify(
+        &self,
+        device: DeviceId,
+        user: UserId,
+        issued_at: DateTime<Utc>,
+    ) -> DomainResult<()> {
         let mut tx = self.store.begin().await?;
         // One statement: reading the owner and the revocation separately could straddle
         // the worker's commit and refuse a device that is merely not relayed yet.
         let state = tx.device_state(device).await?;
         tx.rollback().await?;
-        match state {
-            None => Err(DomainError::DeviceNotKnownYet),
-            Some((owner, is_active)) if owner == user && is_active => Ok(()),
-            Some(_) => Err(DomainError::UnknownDevice),
+        let Some(state) = state else {
+            return Err(DomainError::DeviceNotKnownYet);
+        };
+        if state.user != user || !state.active {
+            return Err(DomainError::UnknownDevice);
+        }
+        match state.password_reset_at {
+            Some(reset) if issued_at.timestamp() <= reset.timestamp() => {
+                Err(DomainError::PasswordChanged)
+            }
+            _ => Ok(()),
         }
     }
 }
```

Change `crates/push-domain/src/store.rs`:

```diff
diff --git a/crates/push-domain/src/store.rs b/crates/push-domain/src/store.rs
index 81cc088..0cc3fcc 100644
--- a/crates/push-domain/src/store.rs
+++ b/crates/push-domain/src/store.rs
@@ -54,7 +54,8 @@ use chrono::{DateTime, Utc};
 use crate::error::DomainResult;
 use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
 use crate::model::{
-    ContactState, Conversation, ConversationOverview, Entry, GroupPositions, InviteRecord, Member,
+    ContactState, Conversation, ConversationOverview, DeviceState, Entry, GroupPositions,
+    InviteRecord, Member,
 };
 
 /// Result of an insert guarded by a uniqueness rule.
@@ -82,6 +83,13 @@ pub trait Tx: Send {
     /// Records a verified user. Idempotent.
     async fn insert_user(&mut self, user: UserId) -> DomainResult<()>;
     async fn user_exists(&mut self, user: UserId) -> DomainResult<bool>;
+    /// Records that `user`'s password was reset at `at`, keeping the latest time, and
+    /// returns the time kept. The user must exist.
+    async fn record_password_reset(
+        &mut self,
+        user: UserId,
+        at: DateTime<Utc>,
+    ) -> DomainResult<DateTime<Utc>>;
     /// Records an active device of `user` unless a device with this id exists (any owner,
     /// active or revoked), and returns whether the device is now an active device of
     /// `user`. Postgres checks after `INSERT … ON CONFLICT DO NOTHING`, which waits for a
@@ -93,9 +101,10 @@ pub trait Tx: Send {
     async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()>;
     /// The owner of a device, active or revoked; `None` if the device is unknown.
     async fn device_user(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
-    /// The owner of a device and whether the device is still active, read in one
-    /// statement so the two cannot disagree; `None` if the device is unknown.
-    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<(UserId, bool)>>;
+    /// The owner of a device, whether the device is still active and the owner's last
+    /// password reset, read in one statement so they cannot disagree; `None` if the device
+    /// is unknown.
+    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<DeviceState>>;
     /// The owner of an active device; `None` if the device is unknown or revoked.
     async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
     /// The user's active devices.
```

Change `crates/push-gateway/src/server.rs`:

```diff
diff --git a/crates/push-gateway/src/server.rs b/crates/push-gateway/src/server.rs
index d8df800..ff92280 100644
--- a/crates/push-gateway/src/server.rs
+++ b/crates/push-gateway/src/server.rs
@@ -236,9 +236,14 @@ async fn identify(
             return None;
         }
     };
+    // DEV ONLY identities carry no token; they count as issued now.
+    let issued_at = std::time::SystemTime::now()
+        .duration_since(std::time::UNIX_EPOCH)
+        .map_or(0, |since| since.as_secs() as i64);
     let request = IdentifyRequest {
         user_id: id_bytes(user),
         device_id: id_bytes(device),
+        issued_at,
     };
     match app.messaging.clone().identify(request).await {
         Ok(_) => {
```

Change `crates/push-message-svc/src/convert.rs`:

```diff
diff --git a/crates/push-message-svc/src/convert.rs b/crates/push-message-svc/src/convert.rs
index 6fb96e8..88d2402 100644
--- a/crates/push-message-svc/src/convert.rs
+++ b/crates/push-message-svc/src/convert.rs
@@ -49,7 +49,8 @@ pub(crate) fn domain_error(error: DomainError) -> Status {
         DomainError::Invalid(_) => (Code::InvalidArgument, ErrorCode::Invalid),
         DomainError::UnknownUser => (Code::NotFound, ErrorCode::UserNotFound),
         DomainError::UnknownDevice => (Code::PermissionDenied, ErrorCode::UnknownDevice),
-        DomainError::DeviceNotKnownYet => (Code::Unavailable, ErrorCode::Unavailable),
+        DomainError::DeviceNotKnownYet => (Code::Unavailable, ErrorCode::DeviceNotReady),
+        DomainError::PasswordChanged => (Code::Unauthenticated, ErrorCode::PasswordChanged),
         DomainError::ConversationNotFound => (Code::NotFound, ErrorCode::ConversationNotFound),
         DomainError::NotMember => (Code::FailedPrecondition, ErrorCode::NotMember),
         DomainError::NotContact => (Code::FailedPrecondition, ErrorCode::NotContact),
@@ -261,7 +262,12 @@ mod tests {
             (
                 DomainError::DeviceNotKnownYet,
                 Code::Unavailable,
-                ErrorCode::Unavailable,
+                ErrorCode::DeviceNotReady,
+            ),
+            (
+                DomainError::PasswordChanged,
+                Code::Unauthenticated,
+                ErrorCode::PasswordChanged,
             ),
             (
                 DomainError::ConversationNotFound,
@@ -365,7 +371,7 @@ mod tests {
             client_codes.insert(error_code);
         }
         // Only the retryable errors share a client code.
-        assert_eq!(client_codes.len(), 20);
+        assert_eq!(client_codes.len(), 22);
 
         let store = error_from_status(&domain_error(DomainError::Store("relation".into())));
         assert!(!store.message.contains("relation"), "{}", store.message);
```

Change `crates/push-message-svc/src/grpc.rs`:

```diff
diff --git a/crates/push-message-svc/src/grpc.rs b/crates/push-message-svc/src/grpc.rs
index cfe7dbc..f2b774d 100644
--- a/crates/push-message-svc/src/grpc.rs
+++ b/crates/push-message-svc/src/grpc.rs
@@ -47,8 +47,10 @@ where
         let request = request.into_inner();
         let user = parse_id(&request.user_id, "user_id", UserId)?;
         let device = parse_id(&request.device_id, "device_id", DeviceId)?;
+        let issued_at = chrono::DateTime::from_timestamp(request.issued_at, 0)
+            .ok_or_else(|| invalid("issued_at is out of range"))?;
         self.messaging
-            .identify(device, user)
+            .identify(device, user, issued_at)
             .await
             .map_err(domain_error)?;
         Ok(Response::new(IdentifyResponse {}))
@@ -204,9 +206,19 @@ where
         &self,
         request: Request<AccountUser>,
     ) -> Result<Response<AccountsResponse>, Status> {
-        let user = parse_id(&request.into_inner().user_id, "user_id", UserId)?;
+        let request = request.into_inner();
+        let user = parse_id(&request.user_id, "user_id", UserId)?;
+        // 0: an outbox row written before auth-svc sent the time; message-svc's clock then.
+        let reset_at = match request.reset_at {
+            0 => None,
+            seconds if seconds > 0 => Some(
+                chrono::DateTime::from_timestamp(seconds, 0)
+                    .ok_or_else(|| invalid("reset_at is out of range"))?,
+            ),
+            _ => return Err(invalid("reset_at is out of range")),
+        };
         self.messaging
-            .password_reset(user)
+            .password_reset(user, reset_at)
             .await
             .map_err(domain_error)?;
         Ok(Response::new(AccountsResponse {}))
@@ -526,6 +538,7 @@ mod tests {
             .identify(Request::new(IdentifyRequest {
                 user_id: id_bytes(alice.user.0),
                 device_id: id_bytes(alice.device.0),
+                issued_at: now(),
             }))
             .await;
         assert!(own.is_ok());
@@ -534,6 +547,7 @@ mod tests {
             .identify(Request::new(IdentifyRequest {
                 user_id: id_bytes(alice.user.0),
                 device_id: id_bytes(bob.device.0),
+                issued_at: now(),
             }))
             .await
             .unwrap_err();
@@ -596,7 +610,11 @@ mod tests {
         let short = vec![1, 2, 3];
 
         let identify = |user_id: Vec<u8>, device_id: Vec<u8>| {
-            Request::new(IdentifyRequest { user_id, device_id })
+            Request::new(IdentifyRequest {
+                user_id,
+                device_id,
+                issued_at: now(),
+            })
         };
         let status = world
             .api
@@ -673,12 +691,29 @@ mod tests {
         })
     }
 
+    /// Seconds since the Unix epoch, as an access token's `iat`.
+    fn now() -> i64 {
+        chrono::Utc::now().timestamp()
+    }
+
     async fn identify(world: &World, user: UserId, device: DeviceId) -> Result<(), Code> {
+        identify_at(world, user, device, now()).await
+    }
+
+    /// Identify with a token issued at `issued_at` (seconds); the client error code of a
+    /// refusal.
+    async fn identify_at(
+        world: &World,
+        user: UserId,
+        device: DeviceId,
+        issued_at: i64,
+    ) -> Result<(), Code> {
         world
             .api
             .identify(Request::new(IdentifyRequest {
                 user_id: id_bytes(user.0),
                 device_id: id_bytes(device.0),
+                issued_at,
             }))
             .await
             .map(|_| ())
@@ -691,6 +726,7 @@ mod tests {
         let (user, phone, tablet) = (UserId::new(), DeviceId::new(), DeviceId::new());
         let verified = AccountUser {
             user_id: id_bytes(user.0),
+            reset_at: 0,
         };
         world
             .accounts
@@ -711,7 +747,19 @@ mod tests {
         }
         assert_eq!(identify(&world, user, phone).await, Ok(()));
         // A device not relayed yet is worth retrying, unlike a revoked one.
-        assert_eq!(identify(&world, user, tablet).await, Err(Code::Unavailable));
+        let status = world
+            .api
+            .identify(Request::new(IdentifyRequest {
+                user_id: id_bytes(user.0),
+                device_id: id_bytes(tablet.0),
+                issued_at: now(),
+            }))
+            .await
+            .unwrap_err();
+        assert_eq!(status.code(), Code::Unavailable);
+        let error = error_from_status(&status);
+        assert_eq!(error.code(), ErrorCode::DeviceNotReady);
+        assert!(error.retryable);
 
         // The revocation of the tablet overtakes its registration.
         world
@@ -729,22 +777,76 @@ mod tests {
             Err(Code::PermissionDenied)
         );
         world.notifier.take();
+        // auth-svc reset the password a minute before the worker relayed it.
+        let reset_at = now() - 60;
+        let reset = AccountUser {
+            user_id: id_bytes(user.0),
+            reset_at,
+        };
         world
             .accounts
-            .password_reset(Request::new(AccountUser {
-                user_id: id_bytes(user.0),
-            }))
+            .password_reset(Request::new(reset.clone()))
             .await
             .unwrap();
-        // The phone is kicked, to sign in again; the revoked tablet is not active.
+        // The phone is kicked, to sign in again; the revoked tablet is not active. The kick
+        // carries auth-svc's time of the reset.
         assert_eq!(
             world.notifier.take(),
             [Event::Kick {
                 devices: vec![phone],
-                reason: KickReason::PasswordReset,
+                reason: KickReason::PasswordReset {
+                    reset_at: chrono::DateTime::from_timestamp(reset_at, 0).unwrap(),
+                },
             }]
         );
-        assert_eq!(identify(&world, user, phone).await, Ok(()));
+        // Tokens issued until the reset's second sign in no more. The phone stays
+        // registered, and a token from after the reset passes, even one issued before the
+        // relay.
+        let status = world
+            .api
+            .identify(Request::new(IdentifyRequest {
+                user_id: id_bytes(user.0),
+                device_id: id_bytes(phone.0),
+                issued_at: reset_at,
+            }))
+            .await
+            .unwrap_err();
+        assert_eq!(status.code(), Code::Unauthenticated);
+        let error = error_from_status(&status);
+        assert_eq!(error.code(), ErrorCode::PasswordChanged);
+        assert!(!error.retryable);
+        assert_eq!(identify_at(&world, user, phone, reset_at + 1).await, Ok(()));
+
+        // A redelivered event changes nothing for the session that signed in after it.
+        world
+            .accounts
+            .password_reset(Request::new(reset))
+            .await
+            .unwrap();
+        assert_eq!(identify_at(&world, user, phone, reset_at + 1).await, Ok(()));
+
+        // An event from before auth-svc sent the time takes message-svc's clock.
+        world
+            .accounts
+            .password_reset(Request::new(AccountUser {
+                user_id: id_bytes(user.0),
+                reset_at: 0,
+            }))
+            .await
+            .unwrap();
+        assert_eq!(
+            identify_at(&world, user, phone, now() - 1).await,
+            Err(Code::Unauthenticated)
+        );
+        let status = world
+            .accounts
+            .password_reset(Request::new(AccountUser {
+                user_id: id_bytes(user.0),
+                reset_at: -1,
+            }))
+            .await
+            .unwrap_err();
+        assert_eq!(status.code(), Code::InvalidArgument);
     }
 
     #[tokio::test]
```

Change `crates/push-message-svc/src/notify.rs`:

```diff
diff --git a/crates/push-message-svc/src/notify.rs b/crates/push-message-svc/src/notify.rs
index 60e9ba9..b9547f3 100644
--- a/crates/push-message-svc/src/notify.rs
+++ b/crates/push-message-svc/src/notify.rs
@@ -119,8 +119,13 @@ pub(crate) fn notifications(events: &[Event]) -> Vec<(String, Vec<u8>)> {
                 messages.push(message(user.0, kind));
             }
             Event::Kick { devices, reason } => {
+                let reset_at = match reason {
+                    KickReason::PasswordReset { reset_at } => reset_at.timestamp(),
+                    KickReason::DeviceRevoked => 0,
+                };
                 let kick = Kick {
                     reason: kick_reason(*reason).into(),
+                    reset_at,
                 };
                 for device in devices {
                     messages.push((kick_channel(device.0), kick.encode_to_vec()));
@@ -135,7 +140,7 @@ pub(crate) fn notifications(events: &[Event]) -> Vec<(String, Vec<u8>)> {
 fn kick_reason(reason: KickReason) -> internal::KickReason {
     match reason {
         KickReason::DeviceRevoked => internal::KickReason::DeviceRevoked,
-        KickReason::PasswordReset => internal::KickReason::PasswordReset,
+        KickReason::PasswordReset { .. } => internal::KickReason::PasswordReset,
     }
 }
 
@@ -283,29 +288,35 @@ mod tests {
     #[test]
     fn a_kick_goes_to_each_device_with_its_reason() {
         let (phone, laptop) = (DeviceId::new(), DeviceId::new());
+        let reset_at = chrono::DateTime::from_timestamp(1_760_000_000, 0).unwrap();
         let messages = notifications(&[
             Event::Kick {
                 devices: vec![phone, laptop],
-                reason: KickReason::PasswordReset,
+                reason: KickReason::PasswordReset { reset_at },
             },
             Event::Kick {
                 devices: vec![phone],
                 reason: KickReason::DeviceRevoked,
             },
         ]);
-        let decoded: Vec<(String, internal::KickReason)> = messages
+        let decoded: Vec<(String, internal::KickReason, i64)> = messages
             .iter()
             .map(|(channel, payload)| {
                 let kick = Kick::decode(payload.as_slice()).unwrap();
-                (channel.clone(), kick.reason())
+                (channel.clone(), kick.reason(), kick.reset_at)
             })
             .collect();
+        let reset = internal::KickReason::PasswordReset;
         assert_eq!(
             decoded,
             vec![
-                (kick_channel(phone.0), internal::KickReason::PasswordReset),
-                (kick_channel(laptop.0), internal::KickReason::PasswordReset),
-                (kick_channel(phone.0), internal::KickReason::DeviceRevoked),
+                (kick_channel(phone.0), reset, 1_760_000_000),
+                (kick_channel(laptop.0), reset, 1_760_000_000),
+                (
+                    kick_channel(phone.0),
+                    internal::KickReason::DeviceRevoked,
+                    0
+                ),
             ]
         );
     }
```

Change `crates/push-proto/proto/push/internal/v1/messaging.proto`:

```diff
diff --git a/crates/push-proto/proto/push/internal/v1/messaging.proto b/crates/push-proto/proto/push/internal/v1/messaging.proto
index f20535d..e28f112 100644
--- a/crates/push-proto/proto/push/internal/v1/messaging.proto
+++ b/crates/push-proto/proto/push/internal/v1/messaging.proto
@@ -15,10 +15,12 @@ package push.internal.v1;
 import "push/v1/frame.proto";
 
 service Messaging {
-  // Confirms that the device is active and belongs to the user. A revoked device, or one
-  // of another user, is PERMISSION_DENIED (UNKNOWN_DEVICE); a device message-svc has never
-  // heard of is UNAVAILABLE (retryable): auth-svc registered it and the worker has not
-  // relayed it yet, which takes about a second.
+  // Confirms that the device is active and belongs to the user, and that the access token
+  // is newer than the user's last password reset. A revoked device, or one of another
+  // user, is PERMISSION_DENIED (UNKNOWN_DEVICE); a token issued no later than the second
+  // of the last reset message-svc heard of is UNAUTHENTICATED (PASSWORD_CHANGED); a device
+  // message-svc has never heard of is UNAVAILABLE (DEVICE_NOT_READY, retryable): auth-svc
+  // registered it and the worker has not relayed it yet, which takes about a second.
   rpc Identify(IdentifyRequest) returns (IdentifyResponse);
   // Appends a text message; answers after commit.
   rpc Send(SendRequest) returns (push.v1.SendAck);
@@ -58,8 +60,9 @@ service Accounts {
   // before the registration; the device is kicked (`kick:{device_id}`).
   rpc DeviceRevoked(AccountDevice) returns (AccountsResponse);
   // user.password_reset: every active device of the user is kicked
-  // (`kick:{device_id}`), to sign in again. The gateway closes kicked sockets and refuses
-  // tokens older than the reset in Plan 8.
+  // (`kick:{device_id}`), to sign in again, and Identify refuses the access tokens issued
+  // no later than the second of `reset_at` (message-svc's own clock when it is 0). The
+  // latest reset counts: a repeated or an older event moves nothing.
   rpc PasswordReset(AccountUser) returns (AccountsResponse);
 }
 
@@ -81,6 +84,8 @@ service Directory {
 message IdentifyRequest {
   bytes user_id = 1;
   bytes device_id = 2;
+  // The access token's `iat`, in seconds since the Unix epoch.
+  int64 issued_at = 3;
 }
 
 message IdentifyResponse {}
@@ -184,9 +189,13 @@ message ReadPositionChanged {
 }
 
 // Published on the Redis channel `kick:{device_id}` (spec §5.3, §5.4): the gateway holding
-// the device closes its socket at once (Plan 8). Never carries anything but the reason.
+// the device closes its socket at once (Plan 8). Never carries anything but the reason and
+// the reset time.
 message Kick {
   KickReason reason = 1;
+  // PASSWORD_RESET: the reset, in seconds since the Unix epoch. A connection whose token
+  // was issued after that second signed in after the reset and stays open.
+  int64 reset_at = 2;
 }
 
 enum KickReason {
@@ -199,6 +208,9 @@ enum KickReason {
 
 message AccountUser {
   bytes user_id = 1;
+  // PasswordReset only: auth-svc's time of the reset, in seconds since the Unix epoch (the
+  // clock of the tokens' `iat`); 0 for an event written before auth-svc sent it.
+  int64 reset_at = 2;
 }
 
 message AccountDevice {
```

Change `crates/push-smoke/src/lib.rs`:

```diff
diff --git a/crates/push-smoke/src/lib.rs b/crates/push-smoke/src/lib.rs
index 315fff2..a986d38 100644
--- a/crates/push-smoke/src/lib.rs
+++ b/crates/push-smoke/src/lib.rs
@@ -519,6 +519,8 @@ async fn kick(
         .identify(IdentifyRequest {
             user_id: id_bytes(carol.user),
             device_id: id_bytes(carol.device),
+            // Carol never reset her password.
+            issued_at: 0,
         })
         .await;
     match identified {
@@ -577,6 +579,8 @@ impl Client {
                 .identify(IdentifyRequest {
                     user_id: id_bytes(user),
                     device_id: id_bytes(device),
+                    // New users: no password reset yet.
+                    issued_at: 0,
                 })
                 .await;
             match identified {
```

Create `crates/push-store/migrations/0003_password_reset.sql`:

```sql
-- When message-svc last heard (from auth-svc, through the worker) that the user's password
-- was reset. Identify refuses access tokens issued no later than that second, so a token
-- that outlives the reset, or a kick lost on Redis, cannot keep a device signed in.
ALTER TABLE messaging.known_user ADD COLUMN password_reset_at timestamptz;
```

Change `crates/push-store/src/store.rs`:

```diff
diff --git a/crates/push-store/src/store.rs b/crates/push-store/src/store.rs
index ba81a96..082b52d 100644
--- a/crates/push-store/src/store.rs
+++ b/crates/push-store/src/store.rs
@@ -4,8 +4,8 @@ use async_trait::async_trait;
 use chrono::{DateTime, Utc};
 use push_domain::{
     ClientMsgId, ContactState, Conversation, ConversationId, ConversationOverview, DeviceId,
-    DomainError, DomainResult, Entry, EntryKind, GroupPositions, InsertOutcome, InviteRecord,
-    Member, MessageId, Store, Tx, UserId,
+    DeviceState, DomainError, DomainResult, Entry, EntryKind, GroupPositions, InsertOutcome,
+    InviteRecord, Member, MessageId, Store, Tx, UserId,
 };
 use sqlx::postgres::{PgPool, PgRow};
 use sqlx::{PgConnection, Postgres, Row, Transaction};
@@ -132,6 +132,27 @@ impl Tx for PgTx {
             .map_err(store_error)
     }
 
+    async fn record_password_reset(
+        &mut self,
+        user: UserId,
+        at: DateTime<Utc>,
+    ) -> DomainResult<DateTime<Utc>> {
+        // greatest() skips a NULL, so the first reset is recorded as is.
+        let kept: Option<DateTime<Utc>> = sqlx::query_scalar(
+            "UPDATE messaging.known_user \
+             SET password_reset_at = greatest(password_reset_at, $2) WHERE user_id = $1 \
+             RETURNING password_reset_at",
+        )
+        .bind(user.0)
+        .bind(at)
+        .fetch_optional(self.conn())
+        .await
+        .map_err(store_error)?;
+        kept.ok_or_else(|| {
+            DomainError::Store("foreign key violation: user does not exist".to_owned())
+        })
+    }
+
     async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool> {
         // A concurrent insert of the same id makes this wait for its outcome, so the check
         // below sees whichever row won.
@@ -177,16 +198,21 @@ impl Tx for PgTx {
         Ok(owner.map(UserId))
     }
 
-    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<(UserId, bool)>> {
-        let row: Option<(Uuid, bool)> = sqlx::query_as(
-            "SELECT user_id, revoked_at IS NULL FROM messaging.member_device \
-             WHERE device_id = $1",
+    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<DeviceState>> {
+        let row: Option<(Uuid, bool, Option<DateTime<Utc>>)> = sqlx::query_as(
+            "SELECT d.user_id, d.revoked_at IS NULL, u.password_reset_at \
+             FROM messaging.member_device d JOIN messaging.known_user u USING (user_id) \
+             WHERE d.device_id = $1",
         )
         .bind(device.0)
         .fetch_optional(self.conn())
         .await
         .map_err(store_error)?;
-        Ok(row.map(|(owner, is_active)| (UserId(owner), is_active)))
+        Ok(row.map(|(owner, active, password_reset_at)| DeviceState {
+            user: UserId(owner),
+            active,
+            password_reset_at,
+        }))
     }
 
     async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
```

Change `crates/push-testkit/src/concurrency.rs`:

```diff
diff --git a/crates/push-testkit/src/concurrency.rs b/crates/push-testkit/src/concurrency.rs
index 553c3a9..c52fcbd 100644
--- a/crates/push-testkit/src/concurrency.rs
+++ b/crates/push-testkit/src/concurrency.rs
@@ -246,9 +246,9 @@ async fn concurrent_registrations_of_one_device_give_it_one_owner<B: Backend>(ba
             .expect("one registration succeeds");
         let loser = 1 - winner;
         assert_eq!(results[loser], Err(DomainError::UnknownDevice));
-        assert_eq!(world.svc.identify(device, users[winner]).await, Ok(()));
+        assert_eq!(world.identify(device, users[winner]).await, Ok(()));
         assert_eq!(
-            world.svc.identify(device, users[loser]).await,
+            world.identify(device, users[loser]).await,
             Err(DomainError::UnknownDevice)
         );
         assert!(world.position(device, chats[winner]).await.is_some());
```

Change `crates/push-testkit/src/contract/users.rs`:

```diff
diff --git a/crates/push-testkit/src/contract/users.rs b/crates/push-testkit/src/contract/users.rs
index 10e2536..d6dd8bd 100644
--- a/crates/push-testkit/src/contract/users.rs
+++ b/crates/push-testkit/src/contract/users.rs
@@ -1,4 +1,5 @@
-use push_domain::{DeviceId, DomainError, Store, Tx, UserId};
+use chrono::{TimeZone, Utc};
+use push_domain::{DeviceId, DeviceState, DomainError, Store, Tx, UserId};
 
 use super::add_users;
 use crate::{Backend, Case, case_list};
@@ -79,24 +80,66 @@ async fn device_user_names_the_owner_of_revoked_devices_too<B: Backend>(backend:
     assert_eq!(tx.device_user(DeviceId::new()).await.unwrap(), None);
 }
 
-async fn device_state_reads_owner_and_activity_together<B: Backend>(backend: &B) {
+async fn device_state_reads_owner_activity_and_reset_together<B: Backend>(backend: &B) {
     let store = backend.store().await;
     let (user, other) = (UserId::new(), UserId::new());
     add_users(&store, &[user, other]).await;
     let (active, revoked) = (DeviceId::new(), DeviceId::new());
+    let reset = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
     let mut tx = store.begin().await.unwrap();
     tx.insert_device(active, user).await.unwrap();
     tx.insert_device(revoked, other).await.unwrap();
     tx.revoke_device(revoked).await.unwrap();
+    tx.record_password_reset(user, reset).await.unwrap();
 
-    assert_eq!(tx.device_state(active).await.unwrap(), Some((user, true)));
+    let state = |user, active, password_reset_at| {
+        Some(DeviceState {
+            user,
+            active,
+            password_reset_at,
+        })
+    };
+    assert_eq!(
+        tx.device_state(active).await.unwrap(),
+        state(user, true, Some(reset))
+    );
     assert_eq!(
         tx.device_state(revoked).await.unwrap(),
-        Some((other, false))
+        state(other, false, None)
     );
     assert_eq!(tx.device_state(DeviceId::new()).await.unwrap(), None);
 }
 
+async fn a_password_reset_keeps_the_latest_time_and_needs_the_user<B: Backend>(backend: &B) {
+    let store = backend.store().await;
+    let user = UserId::new();
+    add_users(&store, &[user]).await;
+    let device = DeviceId::new();
+    let (earlier, later) = (
+        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
+        Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap(),
+    );
+    let mut tx = store.begin().await.unwrap();
+    tx.insert_device(device, user).await.unwrap();
+    // The time kept comes back.
+    assert_eq!(tx.record_password_reset(user, later).await.unwrap(), later);
+    assert_eq!(
+        tx.record_password_reset(user, earlier).await.unwrap(),
+        later
+    );
+    let reset = tx
+        .device_state(device)
+        .await
+        .unwrap()
+        .unwrap()
+        .password_reset_at;
+    assert_eq!(reset, Some(later));
+    assert!(matches!(
+        tx.record_password_reset(UserId::new(), later).await,
+        Err(DomainError::Store(_))
+    ));
+}
+
 pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
     case_list![
         inserting_a_user_twice_is_harmless,
@@ -105,6 +148,7 @@ pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
         a_device_of_an_unknown_user_is_refused,
         revoking_an_unknown_device_does_nothing,
         device_user_names_the_owner_of_revoked_devices_too,
-        device_state_reads_owner_and_activity_together,
+        device_state_reads_owner_activity_and_reset_together,
+        a_password_reset_keeps_the_latest_time_and_needs_the_user,
     ]
 }
```

Change `crates/push-testkit/src/scenarios/devices.rs`:

```diff
diff --git a/crates/push-testkit/src/scenarios/devices.rs b/crates/push-testkit/src/scenarios/devices.rs
index fcb296d..26a00b6 100644
--- a/crates/push-testkit/src/scenarios/devices.rs
+++ b/crates/push-testkit/src/scenarios/devices.rs
@@ -1,5 +1,7 @@
+use chrono::TimeDelta;
+use push_domain::{Clock, DeviceId, DomainError, Event, KickReason, UserId};
+
 use crate::{Backend, Case, World, bodies, case_list};
-use push_domain::{DeviceId, DomainError, Event, KickReason, UserId};
 
 async fn registering_a_device_for_an_unknown_user_fails<B: Backend>(backend: &B) {
     let world = World::new(backend).await;
@@ -13,7 +15,7 @@ async fn registering_a_device_for_an_unknown_user_fails<B: Backend>(backend: &B)
 async fn identify_accepts_the_users_own_active_device<B: Backend>(backend: &B) {
     let world = World::new(backend).await;
     let alice = world.person().await;
-    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
+    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
 }
 
 async fn identify_rejects_a_device_of_another_user<B: Backend>(backend: &B) {
@@ -21,7 +23,7 @@ async fn identify_rejects_a_device_of_another_user<B: Backend>(backend: &B) {
     let alice = world.person().await;
     let bob = world.person().await;
     assert_eq!(
-        world.svc.identify(bob.device, alice.user).await,
+        world.identify(bob.device, alice.user).await,
         Err(DomainError::UnknownDevice)
     );
 }
@@ -31,7 +33,7 @@ async fn identify_rejects_a_revoked_device<B: Backend>(backend: &B) {
     let alice = world.person().await;
     world.svc.revoke_device(alice.device).await.unwrap();
     assert_eq!(
-        world.svc.identify(alice.device, alice.user).await,
+        world.identify(alice.device, alice.user).await,
         Err(DomainError::UnknownDevice)
     );
 }
@@ -39,11 +41,11 @@ async fn identify_rejects_a_revoked_device<B: Backend>(backend: &B) {
 async fn identify_asks_to_retry_a_device_not_relayed_yet<B: Backend>(backend: &B) {
     let world = World::new(backend).await;
     let (user, device) = (UserId::new(), DeviceId::new());
-    let refused = world.svc.identify(device, user).await.unwrap_err();
+    let refused = world.identify(device, user).await.unwrap_err();
     assert_eq!(refused, DomainError::DeviceNotKnownYet);
     assert!(refused.is_retryable());
     world.svc.device_registered(device, user).await.unwrap();
-    assert_eq!(world.svc.identify(device, user).await, Ok(()));
+    assert_eq!(world.identify(device, user).await, Ok(()));
 }
 
 async fn a_device_id_of_another_user_cannot_be_registered<B: Backend>(backend: &B) {
@@ -75,7 +77,7 @@ async fn registering_the_same_user_and_device_twice_is_harmless<B: Backend>(back
         .register_device(alice.device, alice.user)
         .await
         .unwrap();
-    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
+    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
 }
 
 async fn a_relayed_registration_records_the_user_too<B: Backend>(backend: &B) {
@@ -84,7 +86,7 @@ async fn a_relayed_registration_records_the_user_too<B: Backend>(backend: &B) {
     world.svc.device_registered(device, user).await.unwrap();
     world.svc.device_registered(device, user).await.unwrap();
     world.svc.register_user(user).await.unwrap();
-    assert_eq!(world.svc.identify(device, user).await, Ok(()));
+    assert_eq!(world.identify(device, user).await, Ok(()));
 }
 
 async fn a_relayed_registration_starts_up_to_date<B: Backend>(backend: &B) {
@@ -116,7 +118,7 @@ async fn a_revocation_relayed_before_the_registration_wins<B: Backend>(backend:
     world.svc.device_registered(device, user).await.unwrap();
     world.svc.device_revoked(device, user).await.unwrap();
     assert_eq!(
-        world.svc.identify(device, user).await,
+        world.identify(device, user).await,
         Err(DomainError::UnknownDevice)
     );
 }
@@ -127,10 +129,10 @@ async fn a_relayed_revocation_closes_an_active_device<B: Backend>(backend: &B) {
     let tablet = world.add_device(&alice).await;
     world.svc.device_revoked(tablet, alice.user).await.unwrap();
     assert_eq!(
-        world.svc.identify(tablet, alice.user).await,
+        world.identify(tablet, alice.user).await,
         Err(DomainError::UnknownDevice)
     );
-    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
+    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
 }
 
 async fn relayed_events_for_another_users_device_are_refused<B: Backend>(backend: &B) {
@@ -144,7 +146,7 @@ async fn relayed_events_for_another_users_device_are_refused<B: Backend>(backend
         world.svc.device_revoked(alice.device, bob.user).await,
         Err(DomainError::UnknownDevice)
     );
-    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
+    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
 }
 
 async fn a_relayed_revocation_kicks_the_device<B: Backend>(backend: &B) {
@@ -183,25 +185,128 @@ async fn a_password_reset_kicks_every_active_device_of_the_user<B: Backend>(back
     world.svc.device_revoked(revoked, alice.user).await.unwrap();
     world.notifier.take();
 
-    world.svc.password_reset(alice.user).await.unwrap();
+    let reset_at = world.clock.now();
+    world
+        .svc
+        .password_reset(alice.user, Some(reset_at))
+        .await
+        .unwrap();
     let mut devices = vec![alice.device, laptop];
     devices.sort();
     assert_eq!(
         world.notifier.take(),
         [Event::Kick {
             devices,
-            reason: KickReason::PasswordReset,
+            reason: KickReason::PasswordReset { reset_at },
         }]
     );
     // The devices stay registered: a new sign-in on them works.
-    assert_eq!(world.svc.identify(laptop, alice.user).await, Ok(()));
-    assert_eq!(world.svc.identify(bob.device, bob.user).await, Ok(()));
+    world.clock.advance(TimeDelta::seconds(1));
+    assert_eq!(world.identify(laptop, alice.user).await, Ok(()));
+    assert_eq!(world.identify(bob.device, bob.user).await, Ok(()));
 
     // A user message-svc has not heard of has nothing to kick.
-    world.svc.password_reset(UserId::new()).await.unwrap();
+    world
+        .svc
+        .password_reset(UserId::new(), Some(reset_at))
+        .await
+        .unwrap();
     assert_eq!(world.notifier.take(), []);
 }
 
+/// A token that outlives the reset (up to 15 minutes), on a device whose kick was lost,
+/// cannot sign in again. The reset's time is auth-svc's: a token issued after it passes
+/// even when the worker relays the reset later.
+async fn identify_refuses_tokens_issued_until_the_password_reset<B: Backend>(backend: &B) {
+    let world = World::new(backend).await;
+    let (alice, bob) = (world.person().await, world.person().await);
+    // auth-svc reset the password half a minute before the worker relayed it.
+    let reset_at = world.clock.now() - TimeDelta::seconds(30);
+    world
+        .svc
+        .password_reset(alice.user, Some(reset_at))
+        .await
+        .unwrap();
+    let before = reset_at - TimeDelta::minutes(10);
+    let refused = world.svc.identify(alice.device, alice.user, before).await;
+    assert_eq!(refused, Err(DomainError::PasswordChanged));
+    assert!(!DomainError::PasswordChanged.is_retryable());
+    // Within the second of the reset the order is unknown: refused too.
+    assert_eq!(
+        world.svc.identify(alice.device, alice.user, reset_at).await,
+        Err(DomainError::PasswordChanged)
+    );
+    // Signed in after the reset, before the relay.
+    let after = reset_at + TimeDelta::seconds(1);
+    assert_eq!(
+        world.svc.identify(alice.device, alice.user, after).await,
+        Ok(())
+    );
+    // Other users are not affected.
+    assert_eq!(
+        world.svc.identify(bob.device, bob.user, before).await,
+        Ok(())
+    );
+}
+
+/// The reset can be relayed before the user's device. A redelivered or an older reset
+/// moves nothing, so a session that signed in after the latest reset stays; a newer one
+/// ends it. An event without a time takes message-svc's clock.
+async fn a_redelivered_or_older_reset_signs_out_no_newer_session<B: Backend>(backend: &B) {
+    let world = World::new(backend).await;
+    let (user, device) = (UserId::new(), DeviceId::new());
+    let reset_at = world.clock.now();
+    world
+        .svc
+        .password_reset(user, Some(reset_at))
+        .await
+        .unwrap();
+    world.svc.device_registered(device, user).await.unwrap();
+    assert_eq!(
+        world.svc.identify(device, user, reset_at).await,
+        Err(DomainError::PasswordChanged)
+    );
+    world.clock.advance(TimeDelta::minutes(1));
+    let signed_in = world.clock.now();
+    assert_eq!(world.identify(device, user).await, Ok(()));
+
+    // At-least-once: the same event again, a minute later; then an older reset.
+    world.clock.advance(TimeDelta::minutes(1));
+    world.notifier.take();
+    world
+        .svc
+        .password_reset(user, Some(reset_at))
+        .await
+        .unwrap();
+    let older = reset_at - TimeDelta::hours(1);
+    world.svc.password_reset(user, Some(older)).await.unwrap();
+    assert_eq!(world.svc.identify(device, user, signed_in).await, Ok(()));
+    // The kicks carry the latest reset, which the newer session postdates.
+    let kick = Event::Kick {
+        devices: vec![device],
+        reason: KickReason::PasswordReset { reset_at },
+    };
+    assert_eq!(world.notifier.take(), [kick.clone(), kick]);
+
+    // A newer reset ends it.
+    let newer = signed_in + TimeDelta::seconds(30);
+    world.svc.password_reset(user, Some(newer)).await.unwrap();
+    assert_eq!(
+        world.svc.identify(device, user, signed_in).await,
+        Err(DomainError::PasswordChanged)
+    );
+
+    // Without a time, now.
+    world.clock.advance(TimeDelta::minutes(1));
+    world.svc.password_reset(user, None).await.unwrap();
+    assert_eq!(
+        world.identify(device, user).await,
+        Err(DomainError::PasswordChanged)
+    );
+    world.clock.advance(TimeDelta::seconds(1));
+    assert_eq!(world.identify(device, user).await, Ok(()));
+}
+
 pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
     case_list![
         registering_a_device_for_an_unknown_user_fails,
@@ -219,5 +324,7 @@ pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
         relayed_events_for_another_users_device_are_refused,
         a_relayed_revocation_kicks_the_device,
         a_password_reset_kicks_every_active_device_of_the_user,
+        identify_refuses_tokens_issued_until_the_password_reset,
+        a_redelivered_or_older_reset_signs_out_no_newer_session,
     ]
 }
```

Change `crates/push-testkit/src/world.rs`:

```diff
diff --git a/crates/push-testkit/src/world.rs b/crates/push-testkit/src/world.rs
index f1a665c..05ff72b 100644
--- a/crates/push-testkit/src/world.rs
+++ b/crates/push-testkit/src/world.rs
@@ -5,9 +5,9 @@ use std::sync::Arc;
 
 use chrono::{TimeZone, Utc};
 use push_domain::{
-    ClientMsgId, Conversation, ConversationId, DeleteMessage, DeviceId, EditText, Entry, Event,
-    ManualClock, Member, MessageRef, Messaging, Policy, RecordingNotifier, SendReceipt, SendText,
-    Store, SystemEvent, Tx, UserId,
+    ClientMsgId, Clock, Conversation, ConversationId, DeleteMessage, DeviceId, DomainResult,
+    EditText, Entry, Event, ManualClock, Member, MessageRef, Messaging, Policy, RecordingNotifier,
+    SendReceipt, SendText, Store, SystemEvent, Tx, UserId,
 };
 
 use crate::Backend;
@@ -54,6 +54,11 @@ impl<S: Store + Clone> World<S> {
         }
     }
 
+    /// Identifies `device` of `user` with an access token issued now.
+    pub async fn identify(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
+        self.svc.identify(device, user, self.clock.now()).await
+    }
+
     /// The delivery position of a device, read straight from the store.
     pub async fn position(&self, device: DeviceId, conversation: ConversationId) -> Option<i64> {
         let mut tx = self.store.begin().await.unwrap();
```

Change `crates/push-worker/src/accounts.rs`:

```diff
diff --git a/crates/push-worker/src/accounts.rs b/crates/push-worker/src/accounts.rs
index 7679ec6..c325c4d 100644
--- a/crates/push-worker/src/accounts.rs
+++ b/crates/push-worker/src/accounts.rs
@@ -23,10 +23,23 @@ const CALL_TIMEOUT: Duration = Duration::from_secs(10);
 /// One of auth-svc's user and device events (spec §3.1).
 #[derive(Debug, Clone, Copy, PartialEq, Eq)]
 pub enum AccountEvent {
-    UserVerified { user: Uuid },
-    DeviceRegistered { user: Uuid, device: Uuid },
-    DeviceRevoked { user: Uuid, device: Uuid },
-    PasswordReset { user: Uuid },
+    UserVerified {
+        user: Uuid,
+    },
+    DeviceRegistered {
+        user: Uuid,
+        device: Uuid,
+    },
+    DeviceRevoked {
+        user: Uuid,
+        device: Uuid,
+    },
+    /// `reset_at`: auth-svc's time of the reset, in seconds since the Unix epoch; `None` in
+    /// an event written before auth-svc sent it.
+    PasswordReset {
+        user: Uuid,
+        reset_at: Option<i64>,
+    },
 }
 
 impl AccountEvent {
@@ -42,7 +55,22 @@ impl AccountEvent {
         let device = || -> Result<(Uuid, Uuid), String> { Ok((id("user_id")?, id("device_id")?)) };
         Some(match kind {
             "user.verified" => id("user_id").map(|user| Self::UserVerified { user }),
-            "user.password_reset" => id("user_id").map(|user| Self::PasswordReset { user }),
+            "user.password_reset" => {
+                let reset_at = match &payload["reset_at"] {
+                    Value::Null => Ok(None),
+                    value => value
+                        .as_i64()
+                        .filter(|seconds| *seconds > 0)
+                        .map(Some)
+                        .ok_or_else(|| format!("{kind}: the payload has no valid reset_at")),
+                };
+                id("user_id").and_then(|user| {
+                    Ok(Self::PasswordReset {
+                        user,
+                        reset_at: reset_at?,
+                    })
+                })
+            }
             "device.registered" => {
                 device().map(|(user, device)| Self::DeviceRegistered { user, device })
             }
@@ -84,20 +112,23 @@ impl Accounts {
     /// another user) is poison; anything else is retried.
     pub async fn apply(&self, event: AccountEvent) -> Outcome {
         let mut client = self.client.clone();
-        let user_message = |user: Uuid| AccountUser {
+        let user_message = |user: Uuid, reset_at: Option<i64>| AccountUser {
             user_id: id_bytes(user),
+            reset_at: reset_at.unwrap_or(0),
         };
         let device_message = |user: Uuid, device: Uuid| AccountDevice {
             user_id: id_bytes(user),
             device_id: id_bytes(device),
         };
         let result = match event {
-            AccountEvent::UserVerified { user } => {
-                client.user_verified(user_message(user)).await.map(|_| ())
-            }
-            AccountEvent::PasswordReset { user } => {
-                client.password_reset(user_message(user)).await.map(|_| ())
-            }
+            AccountEvent::UserVerified { user } => client
+                .user_verified(user_message(user, None))
+                .await
+                .map(|_| ()),
+            AccountEvent::PasswordReset { user, reset_at } => client
+                .password_reset(user_message(user, reset_at))
+                .await
+                .map(|_| ()),
             AccountEvent::DeviceRegistered { user, device } => client
                 .device_registered(device_message(user, device))
                 .await
@@ -167,10 +198,33 @@ mod tests {
             AccountEvent::parse("device.revoked", &both),
             Some(Ok(AccountEvent::DeviceRevoked { user, device }))
         );
+        assert_eq!(
+            AccountEvent::parse(
+                "user.password_reset",
+                &json!({"user_id": user, "reset_at": 1_760_000_000})
+            ),
+            Some(Ok(AccountEvent::PasswordReset {
+                user,
+                reset_at: Some(1_760_000_000)
+            }))
+        );
+        // An event written before auth-svc sent the time.
         assert_eq!(
             AccountEvent::parse("user.password_reset", &json!({"user_id": user})),
-            Some(Ok(AccountEvent::PasswordReset { user }))
+            Some(Ok(AccountEvent::PasswordReset {
+                user,
+                reset_at: None
+            }))
         );
+        for reset_at in [json!("soon"), json!(-5), json!(0)] {
+            assert!(matches!(
+                AccountEvent::parse(
+                    "user.password_reset",
+                    &json!({"user_id": user, "reset_at": reset_at})
+                ),
+                Some(Err(_))
+            ));
+        }
         assert_eq!(AccountEvent::parse("email.send", &both), None);
         assert!(matches!(
             AccountEvent::parse("device.registered", &json!({"user_id": user})),
```

Change `crates/push-auth/tests/accounts.rs`:

```diff
diff --git a/crates/push-auth/tests/accounts.rs b/crates/push-auth/tests/accounts.rs
index eabe67a..9546d77 100644
--- a/crates/push-auth/tests/accounts.rs
+++ b/crates/push-auth/tests/accounts.rs
@@ -461,11 +461,13 @@ async fn a_reset_link_sets_the_password_once_and_announces_it() {
         world.auth.reset_password(&reset, "short").await,
         Err(AuthError::Invalid(_))
     ));
+    let before = unix_seconds();
     world
         .auth
         .reset_password(&reset, "battery staple")
         .await
         .unwrap();
+    let after = unix_seconds();
     assert_eq!(
         world.auth.reset_password(&reset, "another password").await,
         Err(AuthError::LinkInvalid),
@@ -480,8 +482,18 @@ async fn a_reset_link_sets_the_password_once_and_announces_it() {
         .login("alice@example.com", "battery staple")
         .await
         .unwrap();
-    let (kind, _) = outbox(&world.pool).await.pop().unwrap();
+    let (kind, payload) = outbox(&world.pool).await.pop().unwrap();
     assert_eq!(kind, "user.password_reset");
+    // The reset time comes from the clock of the tokens' `iat`.
+    let reset_at = payload["reset_at"].as_i64().unwrap();
+    assert!((before..=after).contains(&reset_at), "{reset_at}");
+}
+
+fn unix_seconds() -> i64 {
+    std::time::SystemTime::now()
+        .duration_since(std::time::UNIX_EPOCH)
+        .unwrap()
+        .as_secs() as i64
 }
 
 #[tokio::test]
```

Change `crates/push-message-svc/tests/service.rs`:

```diff
diff --git a/crates/push-message-svc/tests/service.rs b/crates/push-message-svc/tests/service.rs
index 108efe8..f97ee57 100644
--- a/crates/push-message-svc/tests/service.rs
+++ b/crates/push-message-svc/tests/service.rs
@@ -282,9 +282,11 @@ async fn accounts_are_served_only_with_the_workers_token() {
         user_id: id_bytes(user),
         device_id: id_bytes(device),
     };
+    // No password reset in this test: any issue time passes.
     let identify = IdentifyRequest {
         user_id: id_bytes(user),
         device_id: id_bytes(device),
+        issued_at: 0,
     };
     // Not relayed yet: worth retrying, unlike a revoked device.
     let status = messaging.identify(identify.clone()).await.unwrap_err();
```

Change `crates/push-worker/tests/relay.rs`:

```diff
diff --git a/crates/push-worker/tests/relay.rs b/crates/push-worker/tests/relay.rs
index 78d9f2c..e941ec3 100644
--- a/crates/push-worker/tests/relay.rs
+++ b/crates/push-worker/tests/relay.rs
@@ -151,13 +151,29 @@ async fn run(world: &World, jobs: &[Job]) -> Vec<Outcome> {
     outcomes
 }
 
+/// Seconds since the Unix epoch.
+fn unix_now() -> i64 {
+    std::time::SystemTime::now()
+        .duration_since(std::time::UNIX_EPOCH)
+        .unwrap()
+        .as_secs() as i64
+}
+
+/// Whether message-svc accepts the device, with an access token issued a second from now:
+/// after any password reset relayed so far.
 async fn identifies(world: &World, user: Uuid, device: Uuid) -> bool {
+    identifies_at(world, user, device, unix_now() + 1).await
+}
+
+/// Whether message-svc accepts the device with an access token issued at `issued_at`.
+async fn identifies_at(world: &World, user: Uuid, device: Uuid, issued_at: i64) -> bool {
     world
         .messaging
         .clone()
         .identify(IdentifyRequest {
             user_id: id_bytes(user),
             device_id: id_bytes(device),
+            issued_at,
         })
         .await
         .is_ok()
@@ -168,7 +184,14 @@ async fn events_move_from_the_outbox_to_the_queue_once() {
     let world = world().await;
     let user = Uuid::now_v7();
     let first = write(&world.auth, &Event::UserVerified { user_id: user }).await;
-    let second = write(&world.auth, &Event::UserPasswordReset { user_id: user }).await;
+    let second = write(
+        &world.auth,
+        &Event::UserPasswordReset {
+            user_id: user,
+            reset_at: unix_now(),
+        },
+    )
+    .await;
     // A worker that died after queueing the first event, before deleting it.
     world
         .queue
@@ -198,6 +221,8 @@ async fn events_move_from_the_outbox_to_the_queue_once() {
 async fn relayed_events_register_and_revoke_devices_in_message_svc_in_any_order() {
     let world = world().await;
     let (user, phone, tablet) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
+    // auth-svc reset the password a minute before the relay.
+    let reset_at = unix_now() - 60;
     for event in [
         Event::UserVerified { user_id: user },
         Event::DeviceRegistered {
@@ -212,7 +237,10 @@ async fn relayed_events_register_and_revoke_devices_in_message_svc_in_any_order(
             user_id: user,
             device_id: tablet,
         },
-        Event::UserPasswordReset { user_id: user },
+        Event::UserPasswordReset {
+            user_id: user,
+            reset_at,
+        },
     ] {
         write(&world.auth, &event).await;
     }
@@ -224,6 +252,9 @@ async fn relayed_events_register_and_revoke_devices_in_message_svc_in_any_order(
     assert_eq!(outcomes, vec![Outcome::Done; 5]);
     assert!(identifies(&world, user, phone).await);
     assert!(!identifies(&world, user, tablet).await);
+    // The reset counts from auth-svc's time, not the relay's.
+    assert!(!identifies_at(&world, user, phone, reset_at).await);
+    assert!(identifies_at(&world, user, phone, reset_at + 1).await);
 
     // Delivered again (at least once): nothing changes.
     for event in [
```

Change `crates/push-worker/tests/server.rs`:

```diff
diff --git a/crates/push-worker/tests/server.rs b/crates/push-worker/tests/server.rs
index 8d4420e..cbbb08a 100644
--- a/crates/push-worker/tests/server.rs
+++ b/crates/push-worker/tests/server.rs
@@ -191,6 +191,8 @@ async fn the_worker_relays_sends_takes_webhooks_and_stops_cleanly() {
                 .identify(IdentifyRequest {
                     user_id: id_bytes(user),
                     device_id: id_bytes(device),
+                    // No password reset in this test: any issue time passes.
+                    issued_at: 0,
                 })
                 .await
                 .is_ok()
```
- [ ] **Step 2: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "feat(message-svc): Identify refuses tokens issued until the last password reset"
```

Expected: 829 tests pass, no warnings, no diff.
---

### Task 4: The gateway checks access tokens against auth-svc's key set

**Files:**
- Create: `crates/push-gateway/src/tokens.rs`
- Modify: `Cargo.lock`, `crates/push-gateway/Cargo.toml`, `crates/push-gateway/src/lib.rs`

Decision 3. `Verifier` fetches the JWKS (at most 64 KiB, read in chunks so a lying
`Content-Length` cannot help; the error never quotes the body), keeps keys by `kid`, keeps a
key that left the set for `RETIRED_KEY_TTL` (16 minutes) after the first fetch that missed
it, and refetches for an unknown `kid` at most every `REFETCH_INTERVAL` (30 s). Its tests
cover: a token from auth-svc's own `AccessTokens` passes with its claims; expiry only after
the leeway; another audience or key is `Invalid`; `exp`, `kid`, EdDSA and a verified email
are required; a rotation from {k1} to {k2} keeps k1 tokens valid and, once k1 is dropped
(or for a gateway that never held it), answers them `Expired`, never `Invalid`; made-up
kids fetch at most every 30 s; an oversized set is refused and changes nothing; without
reachable keys nobody is signed out.

- [ ] **Step 1: Make the changes**

Change `Cargo.lock`:

```diff
diff --git a/Cargo.lock b/Cargo.lock
index fdb015f..4b0c7df 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -2100,11 +2100,16 @@ dependencies = [
  "anyhow",
  "axum",
  "futures-util",
+ "jsonwebtoken",
  "prost 0.14.4",
+ "push-auth",
  "push-domain",
  "push-message-svc",
  "push-proto",
  "redis",
+ "reqwest",
+ "serde",
+ "serde_json",
  "testcontainers-modules",
  "thiserror",
  "tokio",
```

Change `crates/push-gateway/Cargo.toml`:

```diff
diff --git a/crates/push-gateway/Cargo.toml b/crates/push-gateway/Cargo.toml
index 38325bc..93f82a1 100644
--- a/crates/push-gateway/Cargo.toml
+++ b/crates/push-gateway/Cargo.toml
@@ -11,9 +11,13 @@ repository.workspace = true
 [dependencies]
 anyhow.workspace = true
 axum = { workspace = true, features = ["ws"] }
+jsonwebtoken.workspace = true
 prost.workspace = true
 push-proto.workspace = true
 redis.workspace = true
+reqwest.workspace = true
+serde.workspace = true
+serde_json.workspace = true
 thiserror.workspace = true
 tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
 tokio-stream.workspace = true
@@ -25,6 +29,7 @@ uuid.workspace = true
 
 [dev-dependencies]
 futures-util.workspace = true
+push-auth.workspace = true
 push-domain.workspace = true
 push-message-svc.workspace = true
 testcontainers-modules.workspace = true
```

Change `crates/push-gateway/src/lib.rs`:

```diff
diff --git a/crates/push-gateway/src/lib.rs b/crates/push-gateway/src/lib.rs
index 508baf3..2f7818a 100644
--- a/crates/push-gateway/src/lib.rs
+++ b/crates/push-gateway/src/lib.rs
@@ -6,8 +6,10 @@ mod config;
 mod hub;
 mod server;
 mod session;
+mod tokens;
 
 pub use config::{Config, ConfigError};
 pub use hub::{Hub, Membership};
 pub use server::{init_logging, serve, terminated};
 pub use session::{RetryDelay, Session, Step};
+pub use tokens::{Claims, LEEWAY, REFETCH_INTERVAL, TokenError, Verifier};
```

Create `crates/push-gateway/src/tokens.rs`:

```rust
//! Checking access tokens without calling auth-svc (spec §5.1, §7.3). auth-svc signs them
//! with Ed25519 (EdDSA) and publishes its public keys as a JSON Web Key Set at
//! `GET /v1/auth/jwks`. The gateway keeps those keys by `kid` and fetches the set again
//! when a token names a key it does not know, so auth-svc can rotate its key without the
//! gateway restarting.
//!
//! A rotation signs nobody out. auth-svc publishes only its current key, so a key that
//! leaves the set is kept for [`RETIRED_KEY_TTL`] (an access token's 15 minutes plus the
//! leeway): tokens it signed keep passing until they expire. A token naming a key the
//! gateway does not hold even after fetching the set for it, such as a key retired before
//! this gateway started, is `Expired` (refresh, then reconnect), never `Invalid`: a
//! refresh gets a token signed with the current key. Retiring a compromised key at once
//! therefore takes a restart of the gateways.
//!
//! A token passes only if it names a known key with `kid`, is signed with EdDSA (the
//! algorithm is pinned: the header's `alg` cannot choose another), names this gateway in
//! `aud`, carries `exp` (required: jsonwebtoken 9 skips a mistyped claim unless it is
//! required, CVE-2026-25537), `sub`, `device_id` and `iat`, says the email is verified,
//! and has not expired more than [`LEEWAY`] ago.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::time::Instant;
use uuid::Uuid;

/// How far past `exp` a token is still accepted: clock skew between auth-svc and the
/// gateway.
pub const LEEWAY: Duration = Duration::from_secs(60);
/// A token naming an unknown key fetches the key set again at most this often.
pub const REFETCH_INTERVAL: Duration = Duration::from_secs(30);
/// How long a key that left the set keeps verifying: the 15 minutes an access token lives,
/// plus the leeway.
pub const RETIRED_KEY_TTL: Duration = Duration::from_secs(15 * 60 + LEEWAY.as_secs());
/// The largest key set read; auth-svc's is under 1 KiB.
pub const MAX_KEY_SET_BYTES: usize = 64 * 1024;
/// How long fetching the key set may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// What a valid access token says: user `user` on device `device`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claims {
    pub user: Uuid,
    pub device: Uuid,
    /// `iat` and `exp`, in seconds since the Unix epoch.
    pub issued_at: i64,
    pub expires_at: i64,
}

/// Why a token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// Not a token of ours for this gateway: sign in again (close code 4001).
    #[error("the access token is not valid")]
    Invalid,
    /// Ours, but expired: refresh it (close code 4002).
    #[error("the access token has expired")]
    Expired,
    /// The key set could not be fetched, so the token could not be checked: retry later
    /// (close code 1011). Never a reason to sign out.
    #[error("the token keys are unavailable")]
    KeysUnavailable,
}

/// The claims auth-svc puts in an access token; `aud` and `exp` are checked by
/// jsonwebtoken. A missing or mistyped field fails the decoding.
#[derive(Deserialize)]
struct AccessClaims {
    sub: Uuid,
    device_id: Uuid,
    email_verified: bool,
    exp: i64,
    iat: i64,
}

/// The access-token checker, shared by every connection.
#[derive(Clone)]
pub struct Verifier {
    inner: Arc<Inner>,
}

struct Inner {
    jwks_url: String,
    validation: Validation,
    http: reqwest::Client,
    keys: RwLock<HashMap<String, Key>>,
    /// Serialises fetches, and holds when a token naming an unknown key last fetched.
    refetched: tokio::sync::Mutex<Option<Instant>>,
    refetch_interval: Duration,
    retired_key_ttl: Duration,
}

/// A key held by `kid`.
#[derive(Clone)]
struct Key {
    key: DecodingKey,
    /// When the key was first missing from a fetched set; `None` while it is published.
    retired_at: Option<Instant>,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Verifier")
            .field("jwks_url", &self.inner.jwks_url)
            .field("keys", &self.key_ids())
            .finish()
    }
}

impl Verifier {
    /// A checker for tokens whose audience is `audience` (the gateway's public WebSocket
    /// URL), with keys from `jwks_url`. It has no keys until [`Self::fetch`] succeeds.
    pub fn new(jwks_url: impl Into<String>, audience: &str) -> Self {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[audience]);
        validation.set_required_spec_claims(&["exp", "sub", "aud"]);
        validation.leeway = LEEWAY.as_secs();
        validation.validate_nbf = false;
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("a client without TLS or proxies always builds");
        Self {
            inner: Arc::new(Inner {
                jwks_url: jwks_url.into(),
                validation,
                http,
                keys: RwLock::new(HashMap::new()),
                refetched: tokio::sync::Mutex::new(None),
                refetch_interval: REFETCH_INTERVAL,
                retired_key_ttl: RETIRED_KEY_TTL,
            }),
        }
    }

    /// Whether any key is loaded, so tokens can be checked.
    pub fn is_ready(&self) -> bool {
        !self.read_keys().is_empty()
    }

    /// The ids of the keys held, published or retired, sorted.
    pub fn key_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.read_keys().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Fetches the key set and takes its Ed25519 keys; keys of other kinds, and keys
    /// without a `kid`, are skipped. A key that left the set is kept for
    /// [`RETIRED_KEY_TTL`] from the first fetch that missed it. A failed fetch, or a set
    /// over [`MAX_KEY_SET_BYTES`], changes nothing. The error never quotes the response.
    pub async fn fetch(&self) -> Result<(), String> {
        let _serialised = self.inner.refetched.lock().await;
        self.load().await
    }

    async fn load(&self) -> Result<(), String> {
        let response = self
            .inner
            .http
            .get(&self.inner.jwks_url)
            .send()
            .await
            .map_err(|error| format!("cannot reach the key set: {}", kind(&error)))?;
        if !response.status().is_success() {
            return Err(format!("the key set answered {}", response.status()));
        }
        let body = read_bounded(response, MAX_KEY_SET_BYTES).await?;
        let set: KeySet = serde_json::from_slice(&body)
            .map_err(|_| "the key set is not a JSON Web Key Set".to_owned())?;
        let published = decoding_keys(set);
        if published.is_empty() {
            return Err("the key set has no Ed25519 key with a kid".to_owned());
        }
        let now = Instant::now();
        let mut keys = self.inner.keys.write().expect("keys lock poisoned");
        keys.retain(|kid, held| {
            if published.contains_key(kid) {
                return true;
            }
            let retired_at = *held.retired_at.get_or_insert(now);
            now.duration_since(retired_at) < self.inner.retired_key_ttl
        });
        for (kid, key) in published {
            keys.insert(
                kid,
                Key {
                    key,
                    retired_at: None,
                },
            );
        }
        Ok(())
    }

    /// The claims of `token`, if it passes every check.
    pub async fn verify(&self, token: &str) -> Result<Claims, TokenError> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| TokenError::Invalid)?;
        if header.alg != Algorithm::EdDSA {
            return Err(TokenError::Invalid);
        }
        let kid = header.kid.ok_or(TokenError::Invalid)?;
        let key = match self.key(&kid) {
            Some(key) => key,
            None => self.refetch_for(&kid).await?,
        };
        let data = jsonwebtoken::decode::<AccessClaims>(token, &key, &self.inner.validation)
            .map_err(|error| match error.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => TokenError::Expired,
                _ => TokenError::Invalid,
            })?;
        let claims = data.claims;
        if !claims.email_verified {
            return Err(TokenError::Invalid);
        }
        Ok(Claims {
            user: claims.sub,
            device: claims.device_id,
            issued_at: claims.iat,
            expires_at: claims.exp,
        })
    }

    fn key(&self, kid: &str) -> Option<DecodingKey> {
        self.read_keys().get(kid).map(|held| held.key.clone())
    }

    /// A token names a key the gateway does not have: auth-svc may have rotated its key.
    /// Fetches the set again, at most every [`REFETCH_INTERVAL`] whatever the key ids
    /// tokens name, so made-up ones cannot make the gateway hammer auth-svc. A key still
    /// missing from a set fetched for this very token is `Expired`: it may be a key
    /// retired before this gateway learnt of it, and a refresh gets a token signed with
    /// the current key, while a made-up key gets nobody in. A key that could not be looked
    /// up, because the fetch failed or came too soon after the last one, is
    /// `KeysUnavailable` (try again). Neither an auth-svc outage nor a rotation signs
    /// anyone out.
    async fn refetch_for(&self, kid: &str) -> Result<DecodingKey, TokenError> {
        let mut refetched = self.inner.refetched.lock().await;
        // Another connection may have fetched while this one waited.
        if let Some(key) = self.key(kid) {
            return Ok(key);
        }
        if refetched.is_some_and(|at| at.elapsed() < self.inner.refetch_interval) {
            return Err(TokenError::KeysUnavailable);
        }
        *refetched = Some(Instant::now());
        if let Err(error) = self.load().await {
            tracing::warn!(%error, "could not fetch the token keys");
            return Err(TokenError::KeysUnavailable);
        }
        self.key(kid).ok_or(TokenError::Expired)
    }

    fn read_keys(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Key>> {
        self.inner.keys.read().expect("keys lock poisoned")
    }
}

/// The body of `response`, refused once it exceeds `limit` bytes, whatever its
/// `content-length` says.
async fn read_bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let too_large = || format!("the key set exceeds {limit} bytes");
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("cannot read the key set: {}", kind(&error)))?
    {
        if body.len() + chunk.len() > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A key set read loosely: one key auth-svc adds in a form this gateway does not know
/// must not hide the others.
#[derive(Deserialize)]
struct KeySet {
    keys: Vec<serde_json::Value>,
}

/// The Ed25519 keys of a key set, by `kid`.
fn decoding_keys(set: KeySet) -> HashMap<String, DecodingKey> {
    set.keys
        .into_iter()
        .filter_map(|value| serde_json::from_value::<Jwk>(value).ok())
        .filter_map(|jwk| {
            let AlgorithmParameters::OctetKeyPair(pair) = &jwk.algorithm else {
                return None;
            };
            if pair.curve != EllipticCurve::Ed25519 {
                return None;
            }
            let kid = jwk.common.key_id.clone()?;
            let key = DecodingKey::from_jwk(&jwk).ok()?;
            Some((kid, key))
        })
        .collect()
}

/// What kind of failure a request met, without its URL.
fn kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "connection refused or unreachable"
    } else {
        "request failed"
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::get;
    use jsonwebtoken::{EncodingKey, Header};
    use push_auth::tokens::AccessTokens;
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    const AUDIENCE: &str = "wss://push.example.com/v1/socket";
    const SEED: [u8; 32] = [7; 32];

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn issuer(seed: [u8; 32]) -> AccessTokens {
        AccessTokens::from_seed(&seed, AUDIENCE).unwrap()
    }

    /// Serves `jwks` at `/v1/auth/jwks`, answering the latest value it was given, and
    /// counts the requests.
    async fn key_server(
        jwks: serde_json::Value,
    ) -> (
        String,
        Arc<std::sync::Mutex<serde_json::Value>>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let current = Arc::new(std::sync::Mutex::new(jwks));
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (served, counted) = (Arc::clone(&current), Arc::clone(&hits));
        let app = Router::new().route(
            "/v1/auth/jwks",
            get(move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = served.lock().unwrap().clone();
                async move { axum::Json(body) }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/auth/jwks", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, app).into_future());
        (url, current, hits)
    }

    async fn verifier_for(issuer: &AccessTokens) -> Verifier {
        let (url, _, _) = key_server(issuer.jwks()).await;
        let verifier = Verifier::new(url, AUDIENCE);
        verifier.fetch().await.unwrap();
        verifier
    }

    /// A token with these claims and header, signed with the seed's key.
    fn signed(seed: [u8; 32], header: Header, claims: serde_json::Value) -> String {
        // PKCS#8 v1 of an Ed25519 seed, as auth-svc builds it.
        let mut der = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend_from_slice(&seed);
        jsonwebtoken::encode(&header, &claims, &EncodingKey::from_ed_der(&der)).unwrap()
    }

    fn eddsa(kid: &str) -> Header {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(kid.to_owned());
        header
    }

    fn claims(exp: i64) -> serde_json::Value {
        json!({
            "sub": Uuid::now_v7(),
            "device_id": Uuid::now_v7(),
            "email_verified": true,
            "exp": exp,
            "iat": exp - 900,
            "jti": Uuid::now_v7(),
            "aud": AUDIENCE,
        })
    }

    #[tokio::test]
    async fn a_token_from_auth_svc_passes_with_its_claims() {
        let issuer = issuer(SEED);
        let verifier = verifier_for(&issuer).await;
        assert!(verifier.is_ready());
        assert_eq!(verifier.key_ids(), [issuer.kid()]);
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        let (token, issued) = issuer.issue(user, device, now());
        assert_eq!(
            verifier.verify(&token).await,
            Ok(Claims {
                user,
                device,
                issued_at: issued.iat,
                expires_at: issued.exp,
            })
        );
    }

    #[tokio::test]
    async fn an_expired_token_is_expired_only_after_the_leeway() {
        let issuer = issuer(SEED);
        let verifier = verifier_for(&issuer).await;
        let user = Uuid::now_v7();
        // Expired 30 s ago: within the 60 s leeway.
        let (fresh_enough, _) = issuer.issue(user, user, now() - 900 - 30);
        assert!(verifier.verify(&fresh_enough).await.is_ok());
        let (expired, _) = issuer.issue(user, user, now() - 900 - 90);
        assert_eq!(verifier.verify(&expired).await, Err(TokenError::Expired));
    }

    #[tokio::test]
    async fn tokens_for_another_audience_or_from_another_key_are_invalid() {
        let issuer = issuer(SEED);
        let verifier = verifier_for(&issuer).await;
        let user = Uuid::now_v7();
        let elsewhere = AccessTokens::from_seed(&SEED, "wss://other.example.com/v1/socket");
        let (other_audience, _) = elsewhere.unwrap().issue(user, user, now());
        assert_eq!(
            verifier.verify(&other_audience).await,
            Err(TokenError::Invalid)
        );
        // Signed by another key that claims our kid.
        let forged = signed([8; 32], eddsa(issuer.kid()), claims(now() + 600));
        assert_eq!(verifier.verify(&forged).await, Err(TokenError::Invalid));
        assert_eq!(
            verifier.verify("not.a.token").await,
            Err(TokenError::Invalid)
        );
    }

    #[tokio::test]
    async fn exp_kid_eddsa_and_a_verified_email_are_required() {
        let issuer = issuer(SEED);
        let verifier = verifier_for(&issuer).await;
        let kid = issuer.kid();
        let mut without_exp = claims(now() + 600);
        without_exp.as_object_mut().unwrap().remove("exp");
        let mut mistyped_exp = claims(now() + 600);
        mistyped_exp["exp"] = json!("tomorrow");
        let mut without_device = claims(now() + 600);
        without_device.as_object_mut().unwrap().remove("device_id");
        let mut unverified = claims(now() + 600);
        unverified["email_verified"] = json!(false);
        for claims in [without_exp, mistyped_exp, without_device, unverified] {
            let token = signed(SEED, eddsa(kid), claims);
            assert_eq!(verifier.verify(&token).await, Err(TokenError::Invalid));
        }
        let mut no_kid = eddsa(kid);
        no_kid.kid = None;
        let token = signed(SEED, no_kid, claims(now() + 600));
        assert_eq!(verifier.verify(&token).await, Err(TokenError::Invalid));
        // The header cannot choose another algorithm, such as HMAC keyed with the public
        // key.
        let mut hmac = Header::new(Algorithm::HS256);
        hmac.kid = Some(kid.to_owned());
        let token = jsonwebtoken::encode(
            &hmac,
            &claims(now() + 600),
            &EncodingKey::from_secret(b"anything"),
        )
        .unwrap();
        assert_eq!(verifier.verify(&token).await, Err(TokenError::Invalid));
    }

    #[tokio::test]
    async fn a_rotated_key_is_fetched_when_a_token_names_it() {
        let (old, new) = (issuer(SEED), issuer([9; 32]));
        let (url, jwks, hits) = key_server(old.jwks()).await;
        let mut verifier = Verifier::new(url, AUDIENCE);
        verifier.fetch().await.unwrap();
        // auth-svc rotates: the set now holds both keys.
        let both = json!({
            "keys": [old.jwks()["keys"][0].clone(), new.jwks()["keys"][0].clone()]
        });
        *jwks.lock().unwrap() = both;
        let user = Uuid::now_v7();
        let (token, _) = new.issue(user, user, now());
        assert!(verifier.verify(&token).await.is_ok());
        let (token, _) = old.issue(user, user, now());
        assert!(verifier.verify(&token).await.is_ok());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);

        // Made-up kids fetch at most every 30 s; until then they cannot be checked.
        let stranger = signed([10; 32], eddsa("unknown-kid"), claims(now() + 600));
        for _ in 0..3 {
            assert_eq!(
                verifier.verify(&stranger).await,
                Err(TokenError::KeysUnavailable)
            );
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        // A key missing from a set fetched for the token itself gets nobody in, and signs
        // nobody out: refresh.
        Arc::get_mut(&mut verifier.inner).unwrap().refetch_interval = Duration::ZERO;
        assert_eq!(verifier.verify(&stranger).await, Err(TokenError::Expired));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_rotation_signs_nobody_out() {
        let (k1, k2) = (issuer(SEED), issuer([9; 32]));
        let (url, jwks, _) = key_server(k1.jwks()).await;
        let mut verifier = Verifier::new(url.clone(), AUDIENCE);
        verifier.fetch().await.unwrap();
        let user = Uuid::now_v7();
        let (old_token, _) = k1.issue(user, user, now());
        // auth-svc rotates and publishes only the new key; the 5-minute fetch sees it.
        *jwks.lock().unwrap() = k2.jwks();
        verifier.fetch().await.unwrap();
        let mut held = vec![k1.kid().to_owned(), k2.kid().to_owned()];
        held.sort();
        assert_eq!(verifier.key_ids(), held);
        assert!(verifier.verify(&old_token).await.is_ok());
        let (new_token, _) = k2.issue(user, user, now());
        assert!(verifier.verify(&new_token).await.is_ok());

        // Once the old key's tokens have all expired it is dropped; a token naming it
        // asks for a refresh, not a sign-in.
        let inner = Arc::get_mut(&mut verifier.inner).unwrap();
        inner.retired_key_ttl = Duration::ZERO;
        inner.refetch_interval = Duration::ZERO;
        verifier.fetch().await.unwrap();
        assert_eq!(verifier.key_ids(), [k2.kid()]);
        assert_eq!(verifier.verify(&old_token).await, Err(TokenError::Expired));

        // So does a gateway started after the rotation, which never held the old key.
        let late = Verifier::new(url, AUDIENCE);
        late.fetch().await.unwrap();
        assert_eq!(late.verify(&old_token).await, Err(TokenError::Expired));
    }

    #[tokio::test]
    async fn an_oversized_key_set_is_refused_and_changes_nothing() {
        let issuer = issuer(SEED);
        let (url, jwks, _) = key_server(issuer.jwks()).await;
        let verifier = Verifier::new(url, AUDIENCE);
        verifier.fetch().await.unwrap();
        let padding = "x".repeat(MAX_KEY_SET_BYTES);
        *jwks.lock().unwrap() = json!({
            "keys": [issuer.jwks()["keys"][0].clone(), {"kty": "padding", "x": padding}]
        });
        let error = verifier.fetch().await.unwrap_err();
        assert!(error.contains("exceeds"), "{error}");
        assert_eq!(verifier.key_ids(), [issuer.kid()]);
        let (token, _) = issuer.issue(Uuid::now_v7(), Uuid::now_v7(), now());
        assert!(verifier.verify(&token).await.is_ok());
    }

    #[tokio::test]
    async fn without_reachable_keys_tokens_cannot_be_checked_and_nobody_is_signed_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/auth/jwks", listener.local_addr().unwrap());
        drop(listener);
        let verifier = Verifier::new(url, AUDIENCE);
        assert!(verifier.fetch().await.is_err());
        assert!(!verifier.is_ready());
        let issuer = issuer(SEED);
        let (token, _) = issuer.issue(Uuid::now_v7(), Uuid::now_v7(), now());
        assert_eq!(
            verifier.verify(&token).await,
            Err(TokenError::KeysUnavailable)
        );
    }

    #[test]
    fn keys_of_other_kinds_or_without_a_kid_are_skipped() {
        let issuer = issuer(SEED);
        let ours = issuer.jwks()["keys"][0].clone();
        let mut without_kid = ours.clone();
        without_kid.as_object_mut().unwrap().remove("kid");
        let set = KeySet {
            keys: vec![
                json!({"kty": "RSA", "kid": "rsa", "n": "AQAB", "e": "AQAB", "alg": "RS256"}),
                json!({"kty": "unknown"}),
                without_kid,
                ours,
            ],
        };
        let keys = decoding_keys(set);
        assert_eq!(keys.keys().collect::<Vec<_>>(), [issuer.kid()]);
    }
}
```
- [ ] **Step 2: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A Cargo.lock crates
git commit -m "feat(gateway): check access tokens against auth-svc's key set"
```

Expected: 838 tests pass, no warnings, no diff.
---

### Task 5: Identify with access tokens; the DEV ONLY identity goes

**Files:**
- Modify: `README.md`, `crates/push-gateway/src/config.rs`, `crates/push-gateway/src/server.rs`, `crates/push-gateway/src/session.rs`, `crates/push-gateway/tests/gateway.rs`, `crates/push-proto/proto/push/v1/frame.proto`, `crates/push-smoke/src/lib.rs`, `crates/push-smoke/tests/local.rs`, `deploy/helm/push-messenger/templates/NOTES.txt`, `deploy/helm/push-messenger/templates/_helpers.tpl`, `deploy/helm/push-messenger/templates/gateway.yaml`, `deploy/helm/push-messenger/values-minikube.yaml`, `deploy/helm/push-messenger/values.yaml`, `deploy/helm/test-chart.ps1`

`Identify` carries only `token` (fields 14 and 15 reserved); the gateway verifies it,
asks message-svc with the token's `iat`, and closes with 4001 (invalid, password changed),
4002 (expired),
4003 (unknown or revoked device), 4006 (not relayed yet), 1002 (protocol mistake before
`Ready`) or 1011 (keys or message-svc unreachable). The gateway reads `PUSH_AUTH_URL`,
`PUSH_TOKEN_AUDIENCE` and refuses to start with `PUSH_DEV_AUTH=1`; `/readyz` waits for the
keys. The chart passes the new settings and drops dev mode from the gateway; the smoke test
identifies with its access token.

- [ ] **Step 1: Make the changes**

Change `README.md`:

```diff
diff --git a/README.md b/README.md
index b9c86bb..10ee4ce 100644
--- a/README.md
+++ b/README.md
@@ -93,8 +93,9 @@ at start-up.
 | `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
 | `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
 | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
+| `PUSH_AUTH_URL` | base URL of auth-svc (`http://push-auth:8080`); the token keys come from its `/v1/auth/jwks` | required |
+| `PUSH_TOKEN_AUDIENCE` | the gateway's public URL (`wss://…/v1/socket`), the `aud` access tokens must name | required |
 | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready`, 1000 to 600000 | `30000` |
-| `PUSH_DEV_AUTH` | `1` accepts DEV ONLY identities without a token | off |
 
 `push-auth`:
 
```

Change `crates/push-gateway/src/config.rs`:

```diff
diff --git a/crates/push-gateway/src/config.rs b/crates/push-gateway/src/config.rs
index 2ffa7eb..d35f751 100644
--- a/crates/push-gateway/src/config.rs
+++ b/crates/push-gateway/src/config.rs
@@ -9,18 +9,23 @@ use std::time::Duration;
 /// |---|---|---|
 /// | `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc | required |
 /// | `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
+/// | `PUSH_AUTH_URL` | base URL of auth-svc, whose `/v1/auth/jwks` holds the token keys | required |
+/// | `PUSH_TOKEN_AUDIENCE` | the gateway's public URL (`wss://…/v1/socket`): the `aud` of the access tokens it accepts | required |
 /// | `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
 /// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
 /// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
-/// | `PUSH_DEV_AUTH` | `1` accepts DEV ONLY identities without a token | off |
+///
+/// `PUSH_DEV_AUTH=1` (the DEV ONLY identities without a token, removed in Plan 8) is
+/// refused, so a deployment that still expects it fails at start-up.
 #[derive(Clone, PartialEq, Eq)]
 pub struct Config {
     pub message_svc_url: String,
     pub redis_url: String,
+    pub auth_url: String,
+    pub token_audience: String,
     pub listen_addr: SocketAddr,
     pub health_addr: SocketAddr,
     pub heartbeat: Duration,
-    pub dev_auth: bool,
 }
 
 // The Redis URL carries a password, so it never appears in debug output.
@@ -30,10 +35,11 @@ impl std::fmt::Debug for Config {
             .debug_struct("Config")
             .field("message_svc_url", &self.message_svc_url)
             .field("redis_url", &"<redacted>")
+            .field("auth_url", &self.auth_url)
+            .field("token_audience", &self.token_audience)
             .field("listen_addr", &self.listen_addr)
             .field("health_addr", &self.health_addr)
             .field("heartbeat", &self.heartbeat)
-            .field("dev_auth", &self.dev_auth)
             .finish()
     }
 }
@@ -71,15 +77,41 @@ impl Config {
                 reason: "use 1000 to 600000 milliseconds".to_owned(),
             });
         }
+        match lookup("PUSH_DEV_AUTH").as_deref() {
+            None | Some("") | Some("0") => {}
+            Some(other) => {
+                return Err(ConfigError::Invalid {
+                    name: "PUSH_DEV_AUTH",
+                    value: other.to_owned(),
+                    reason: "DEV ONLY identities were removed: clients identify with access \
+                             tokens"
+                        .to_owned(),
+                });
+            }
+        }
+        let auth_url = required("PUSH_AUTH_URL")?;
+        if !(auth_url.starts_with("http://") || auth_url.starts_with("https://")) {
+            return Err(ConfigError::Invalid {
+                name: "PUSH_AUTH_URL",
+                value: auth_url,
+                reason: "use an http:// or https:// URL".to_owned(),
+            });
+        }
         Ok(Self {
             message_svc_url: required("PUSH_MESSAGE_SVC_URL")?,
             redis_url: required("PUSH_REDIS_URL")?,
+            auth_url,
+            token_audience: required("PUSH_TOKEN_AUDIENCE")?,
             listen_addr: parsed(&lookup, "PUSH_LISTEN_ADDR", ([0, 0, 0, 0], 8080).into())?,
             health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
             heartbeat: Duration::from_millis(heartbeat_ms),
-            dev_auth: dev_flag(&lookup, "PUSH_DEV_AUTH")?,
         })
     }
+
+    /// auth-svc's JSON Web Key Set: the keys that sign access tokens.
+    pub fn jwks_url(&self) -> String {
+        format!("{}/v1/auth/jwks", self.auth_url.trim_end_matches('/'))
+    }
 }
 
 /// A variable parsed as `T`, or `default` when it is unset or empty.
@@ -102,23 +134,6 @@ where
     }
 }
 
-/// A DEV ONLY switch: on only for exactly `1`; off when unset, empty or `0`. Anything
-/// else (`true`, `yes`) is refused, so a typo can never switch it on or leave it in doubt.
-fn dev_flag(
-    lookup: &impl Fn(&str) -> Option<String>,
-    name: &'static str,
-) -> Result<bool, ConfigError> {
-    match lookup(name).as_deref() {
-        None | Some("") | Some("0") => Ok(false),
-        Some("1") => Ok(true),
-        Some(other) => Err(ConfigError::Invalid {
-            name,
-            value: other.to_owned(),
-            reason: "use 1 to enable or 0 to disable".to_owned(),
-        }),
-    }
-}
-
 #[cfg(test)]
 mod tests {
     use std::collections::HashMap;
@@ -133,9 +148,11 @@ mod tests {
         Config::from_lookup(|name| variables.get(name).cloned())
     }
 
-    const REQUIRED: [(&str, &str); 2] = [
+    const REQUIRED: [(&str, &str); 4] = [
         ("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051"),
         ("PUSH_REDIS_URL", "redis://:secret@redis:6379"),
+        ("PUSH_AUTH_URL", "http://auth:8080/"),
+        ("PUSH_TOKEN_AUDIENCE", "wss://push.example.com/v1/socket"),
     ];
 
     #[test]
@@ -143,10 +160,11 @@ mod tests {
         let config = config(&REQUIRED).unwrap();
         assert_eq!(config.message_svc_url, "http://message-svc:50051");
         assert_eq!(config.redis_url, "redis://:secret@redis:6379");
+        assert_eq!(config.jwks_url(), "http://auth:8080/v1/auth/jwks");
+        assert_eq!(config.token_audience, "wss://push.example.com/v1/socket");
         assert_eq!(config.listen_addr, "0.0.0.0:8080".parse().unwrap());
         assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
         assert_eq!(config.heartbeat, Duration::from_secs(30));
-        assert!(!config.dev_auth);
     }
 
     #[test]
@@ -157,15 +175,25 @@ mod tests {
     }
 
     #[test]
-    fn the_urls_are_required() {
-        assert_eq!(
-            config(&REQUIRED[1..]),
-            Err(ConfigError::Missing("PUSH_MESSAGE_SVC_URL"))
-        );
-        assert_eq!(
-            config(&REQUIRED[..1]),
-            Err(ConfigError::Missing("PUSH_REDIS_URL"))
-        );
+    fn the_urls_and_the_audience_are_required() {
+        for (index, (name, _)) in REQUIRED.iter().enumerate() {
+            let mut variables = REQUIRED.to_vec();
+            variables.remove(index);
+            assert_eq!(
+                config(&variables),
+                Err(ConfigError::Missing(name)),
+                "{name}"
+            );
+        }
+        let mut variables = REQUIRED.to_vec();
+        variables[2] = ("PUSH_AUTH_URL", "auth:8080");
+        assert!(matches!(
+            config(&variables),
+            Err(ConfigError::Invalid {
+                name: "PUSH_AUTH_URL",
+                ..
+            })
+        ));
     }
 
     #[test]
@@ -183,13 +211,13 @@ mod tests {
     }
 
     #[test]
-    fn dev_auth_is_on_only_for_exactly_1() {
-        for (value, expected) in [("1", true), ("0", false), ("", false)] {
+    fn dev_identities_are_gone_and_asking_for_them_is_refused() {
+        for value in ["0", ""] {
             let mut variables = REQUIRED.to_vec();
             variables.push(("PUSH_DEV_AUTH", value));
-            assert_eq!(config(&variables).unwrap().dev_auth, expected, "{value:?}");
+            assert!(config(&variables).is_ok(), "{value:?}");
         }
-        for value in ["true", "yes", "on", " 1"] {
+        for value in ["1", "true", "yes"] {
             let mut variables = REQUIRED.to_vec();
             variables.push(("PUSH_DEV_AUTH", value));
             assert!(
```

Change `crates/push-gateway/src/server.rs`:

```diff
diff --git a/crates/push-gateway/src/server.rs b/crates/push-gateway/src/server.rs
index ff92280..fc3439d 100644
--- a/crates/push-gateway/src/server.rs
+++ b/crates/push-gateway/src/server.rs
@@ -1,5 +1,5 @@
-//! Running the gateway: the WebSocket endpoint, one task per connection, the health
-//! endpoints and graceful shutdown.
+//! Running the gateway: the WebSocket endpoint, one task per connection, the token keys,
+//! the health endpoints and graceful shutdown.
 
 use std::sync::Arc;
 use std::time::Duration;
@@ -25,18 +25,23 @@ use uuid::Uuid;
 use crate::config::Config;
 use crate::hub::Hub;
 use crate::session::{RetryDelay, Session, Step, error_frame};
+use crate::tokens::{Claims, TokenError, Verifier};
 
 /// The first frame must be `Identify`, within this time (spec §7.1).
 const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(5);
 /// How long shutdown waits for connections to close.
 const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
-/// Close code for "the server cannot serve this connection now" (RFC 6455).
-const INTERNAL_ERROR: u16 = 1011;
+
+/// How often the token keys are fetched again: auth-svc lets clients cache them 5 minutes.
+const KEYS_REFRESH: Duration = Duration::from_secs(300);
+/// How soon to fetch the keys again after a failed fetch.
+const KEYS_RETRY: Duration = Duration::from_secs(2);
 
 #[derive(Clone)]
 struct App {
     config: Arc<Config>,
     messaging: MessagingClient<Channel>,
+    verifier: Verifier,
     hub: Hub,
     sessions: TaskTracker,
     shutdown: CancellationToken,
@@ -53,13 +58,6 @@ pub async fn serve(
     health: TcpListener,
     shutdown: CancellationToken,
 ) -> anyhow::Result<()> {
-    if config.dev_auth {
-        tracing::warn!(
-            "DEV ONLY: PUSH_DEV_AUTH=1 accepts any user and device id in Identify without a \
-             token. Anyone who can reach this gateway can act as any user. Never enable it \
-             outside a local cluster."
-        );
-    }
     let endpoint = Endpoint::from_shared(config.message_svc_url.clone())?
         .connect_timeout(Duration::from_secs(2))
         .timeout(Duration::from_secs(10));
@@ -68,18 +66,21 @@ pub async fn serve(
     let redis = redis::Client::open(config.redis_url.as_str())
         .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
     let (hub, subscriber) = Hub::start(redis, shutdown.clone());
+    let verifier = Verifier::new(config.jwks_url(), &config.token_audience);
+    let keys = tokio::spawn(refresh_keys(verifier.clone(), shutdown.clone()));
     let sessions = TaskTracker::new();
     let listen_addr = config.listen_addr;
     let app = App {
         config: Arc::new(config),
         messaging,
+        verifier: verifier.clone(),
         hub: hub.clone(),
         sessions: sessions.clone(),
         shutdown: shutdown.clone(),
     };
 
     let health_server = tokio::spawn(
-        axum::serve(health, health_router(shutdown.clone()))
+        axum::serve(health, health_router(verifier, shutdown.clone()))
             .with_graceful_shutdown(shutdown.clone().cancelled_owned())
             .into_future(),
     );
@@ -100,16 +101,45 @@ pub async fn serve(
         tracing::warn!(open = sessions.len(), "connections still open after 10 s");
     }
     subscriber.await?;
+    keys.await?;
     health_server.await??;
     tracing::info!("stopped");
     Ok(())
 }
 
-/// `/healthz` always answers; `/readyz` answers until shutdown begins. Readiness does not
-/// depend on Redis: without it devices still get their entries, only later (after
-/// `Ready`, after acks and on retries), while an unready gateway would refuse every
-/// client. The subscriber reconnects by itself, so liveness does not depend on it either.
-fn health_router(shutdown: CancellationToken) -> Router {
+/// Keeps the token keys fresh: fetches them every 2 s until it has them, then every
+/// 5 minutes (a token naming a new key fetches them at once, see [`Verifier`]). A failed
+/// fetch keeps the keys it had.
+async fn refresh_keys(verifier: Verifier, shutdown: CancellationToken) {
+    loop {
+        let fetched = tokio::select! {
+            () = shutdown.cancelled() => return,
+            fetched = verifier.fetch() => fetched,
+        };
+        let wait = match fetched {
+            Ok(()) => KEYS_REFRESH,
+            Err(error) => {
+                tracing::warn!(%error, "could not fetch the token keys; retrying");
+                if verifier.is_ready() {
+                    KEYS_REFRESH / 10
+                } else {
+                    KEYS_RETRY
+                }
+            }
+        };
+        tokio::select! {
+            () = shutdown.cancelled() => return,
+            () = tokio::time::sleep(wait) => {}
+        }
+    }
+}
+
+/// `/healthz` always answers; `/readyz` answers once the token keys are loaded, until
+/// shutdown begins. Readiness does not depend on Redis: without it devices still get
+/// their entries, only later (after `Ready`, after acks and on retries), while an unready
+/// gateway would refuse every client. The subscriber reconnects by itself, so liveness
+/// does not depend on it either.
+fn health_router(verifier: Verifier, shutdown: CancellationToken) -> Router {
     Router::new()
         .route("/healthz", get(|| async { "ok" }))
         .route(
@@ -117,6 +147,8 @@ fn health_router(shutdown: CancellationToken) -> Router {
             get(move || async move {
                 if shutdown.is_cancelled() {
                     (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
+                } else if !verifier.is_ready() {
+                    (StatusCode::SERVICE_UNAVAILABLE, "loading the token keys")
                 } else {
                     (StatusCode::OK, "ready")
                 }
@@ -134,20 +166,20 @@ async fn upgrade(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response
 
 /// One client connection, from `Identify` to close.
 async fn connection(mut socket: WebSocket, app: App) {
-    let mut session = Session::new(app.config.dev_auth);
-    let Some((user, device)) = identify(&mut socket, &mut session, &app).await else {
+    let mut session = Session::new();
+    let Some(claims) = identify(&mut socket, &mut session, &app).await else {
         return;
     };
-    let mut membership = app.hub.join(user).await;
-    let Some(ready) = session.identified(app.config.heartbeat) else {
+    let mut membership = app.hub.join(claims.user).await;
+    let Some(ready) = session.identified(&claims, app.config.heartbeat) else {
         return;
     };
     let mut connected = Connected {
         socket,
         session,
         messaging: app.messaging.clone(),
-        user,
-        device,
+        user: claims.user,
+        device: claims.device,
         retry: RetryDelay::default(),
         retry_at: None,
     };
@@ -172,6 +204,7 @@ async fn connection(mut socket: WebSocket, app: App) {
                     return;
                 }
             }
+
             message = connected.socket.recv() => {
                 let step = match message {
                     Some(Ok(Message::Binary(bytes))) => connected.session.on_binary(&bytes),
@@ -192,19 +225,15 @@ fn far_future() -> Instant {
     Instant::now() + Duration::from_secs(24 * 60 * 60)
 }
 
-/// Waits for the first frame and checks the identity with message-svc. `None` once the
-/// connection is closed.
-async fn identify(
-    socket: &mut WebSocket,
-    session: &mut Session,
-    app: &App,
-) -> Option<(Uuid, Uuid)> {
+/// Waits for the first frame, checks its token and asks message-svc whether the device it
+/// names may connect. `None` once the connection is closed.
+async fn identify(socket: &mut WebSocket, session: &mut Session, app: &App) -> Option<Claims> {
     let deadline = tokio::time::sleep(IDENTIFY_TIMEOUT);
     tokio::pin!(deadline);
     let step = loop {
         tokio::select! {
             () = &mut deadline => {
-                close_with(socket, close::INVALID_IDENTITY, "Identify must come within 5 s").await;
+                close_with(socket, close::PROTOCOL_ERROR, "Identify must come within 5 s").await;
                 return None;
             }
             () = app.shutdown.cancelled() => {
@@ -219,50 +248,104 @@ async fn identify(
             },
         }
     };
-    let (user, device) = match step {
-        Step::Identify { user, device } => (user, device),
+    let token = match step {
+        Step::Identify(token) => token,
         Step::Close(code, reason) => {
             close_with(socket, code, reason).await;
             return None;
         }
         // Before Ready the session only asks to identify or to close.
-        Step::Reply(_) | Step::Send(_) | Step::Ack(_) => {
+        _ => {
             close_with(
                 socket,
-                close::INVALID_IDENTITY,
+                close::PROTOCOL_ERROR,
                 "the first frame must be Identify",
             )
             .await;
             return None;
         }
     };
-    // DEV ONLY identities carry no token; they count as issued now.
-    let issued_at = std::time::SystemTime::now()
-        .duration_since(std::time::UNIX_EPOCH)
-        .map_or(0, |since| since.as_secs() as i64);
-    let request = IdentifyRequest {
-        user_id: id_bytes(user),
-        device_id: id_bytes(device),
-        issued_at,
+    let claims = match app.verifier.verify(&token).await {
+        Ok(claims) => claims,
+        Err(error) => {
+            let (code, reason) = token_refusal(error);
+            close_with(socket, code, reason).await;
+            return None;
+        }
     };
-    match app.messaging.clone().identify(request).await {
-        Ok(_) => {
-            tracing::info!(%user, %device, dev_identity = true, "device connected");
-            Some((user, device))
+    match admit(&mut app.messaging.clone(), &claims).await {
+        Ok(()) => {
+            tracing::info!(user = %claims.user, device = %claims.device, "device connected");
+            Some(claims)
         }
-        Err(status)
-            if matches!(
-                status.code(),
-                tonic::Code::PermissionDenied | tonic::Code::InvalidArgument
-            ) =>
-        {
-            close_with(socket, close::INVALID_IDENTITY, "unknown or revoked device").await;
+        Err(refusal) => {
+            let (code, reason) = refusal.close();
+            close_with(socket, code, reason).await;
             None
         }
-        Err(status) => {
+    }
+}
+
+/// The close code and reason for a token that did not pass.
+fn token_refusal(error: TokenError) -> (u16, &'static str) {
+    match error {
+        TokenError::Invalid => (close::INVALID_IDENTITY, "the access token is not valid"),
+        TokenError::Expired => (close::REAUTH_REQUIRED, "the access token has expired"),
+        TokenError::KeysUnavailable => (
+            close::INTERNAL_ERROR,
+            "access tokens cannot be checked now; retry",
+        ),
+    }
+}
+
+/// Why message-svc did not admit a device.
+#[derive(Debug, Clone, Copy, PartialEq, Eq)]
+enum Refusal {
+    /// Close with this code and reason.
+    Close(u16, &'static str),
+    /// message-svc could not answer: retry later.
+    Unavailable,
+}
+
+impl Refusal {
+    fn close(self) -> (u16, &'static str) {
+        match self {
+            Self::Close(code, reason) => (code, reason),
+            Self::Unavailable => (close::INTERNAL_ERROR, "the message service is unavailable"),
+        }
+    }
+}
+
+/// Asks message-svc whether the device of a valid token may connect: it is active and the
+/// user's, and the token is newer than the user's last password reset.
+async fn admit(messaging: &mut MessagingClient<Channel>, claims: &Claims) -> Result<(), Refusal> {
+    let request = IdentifyRequest {
+        user_id: id_bytes(claims.user),
+        device_id: id_bytes(claims.device),
+        issued_at: claims.issued_at,
+    };
+    let Err(status) = messaging.identify(request).await else {
+        return Ok(());
+    };
+    match error_from_status(&status).code() {
+        ErrorCode::PasswordChanged => Err(Refusal::Close(
+            close::INVALID_IDENTITY,
+            "the password was changed; sign in again",
+        )),
+        ErrorCode::UnknownDevice => {
+            Err(Refusal::Close(close::TAKEN_OVER, "the device was revoked"))
+        }
+        ErrorCode::DeviceNotReady => {
+            // Normal for a second after a device registers: the worker has not relayed it.
+            tracing::debug!(device = %claims.device, "device not relayed yet");
+            Err(Refusal::Close(
+                close::NOT_READY_YET,
+                "the device is not ready yet; retry shortly",
+            ))
+        }
+        _ => {
             tracing::warn!(code = ?status.code(), message = status.message(), "identify failed");
-            close_with(socket, INTERNAL_ERROR, "the message service is unavailable").await;
-            None
+            Err(Refusal::Unavailable)
         }
     }
 }
@@ -323,7 +406,7 @@ impl Connected {
                 close_with(&mut self.socket, code, reason).await;
                 Err(Closed)
             }
-            Step::Identify { .. } => {
+            Step::Identify(_) => {
                 let error = error_frame(ErrorCode::BadFrame, "already identified");
                 send(&mut self.socket, error).await
             }
```

Change `crates/push-gateway/src/session.rs`:

```diff
diff --git a/crates/push-gateway/src/session.rs b/crates/push-gateway/src/session.rs
index 5cd1d25..a227af2 100644
--- a/crates/push-gateway/src/session.rs
+++ b/crates/push-gateway/src/session.rs
@@ -8,17 +8,19 @@ use std::time::Duration;
 use push_proto::close;
 use push_proto::v1::frame::Body;
 use push_proto::v1::{Ack, Deliver, Error, ErrorCode, Pong, Ready, Send};
-use push_proto::{FrameError, decode_frame, id, id_bytes};
+use push_proto::{FrameError, decode_frame, id_bytes};
 use uuid::Uuid;
 
+use crate::tokens::Claims;
+
 /// What the gateway must do for a frame the client sent.
 #[derive(Debug, Clone, PartialEq)]
 pub enum Step {
     /// Send this frame to the client.
     Reply(Body),
-    /// Ask message-svc whether `device` is an active device of `user`, then call
-    /// [`Session::identified`] or close with [`close::INVALID_IDENTITY`].
-    Identify { user: Uuid, device: Uuid },
+    /// Check this access token, ask message-svc whether the device it names may connect,
+    /// then call [`Session::identified`] or close.
+    Identify(String),
     /// Forward a send to message-svc; answer with its `SendAck` or an error.
     Send(Send),
     /// Forward an ack to message-svc; answer only if it fails. If it moved the position,
@@ -32,11 +34,8 @@ pub enum Step {
 enum State {
     /// Connected; the first frame must be `Identify`.
     New,
-    /// `Identify` is being checked.
-    Identifying {
-        user: Uuid,
-        device: Uuid,
-    },
+    /// The token of `Identify` is being checked.
+    Identifying,
     Ready {
         device: Uuid,
     },
@@ -44,7 +43,6 @@ enum State {
 
 #[derive(Debug)]
 pub struct Session {
-    dev_auth: bool,
     state: State,
     /// The highest `seq` sent to the client per conversation, so a sync that repeats
     /// entries the client has not acked yet does not send them twice.
@@ -55,10 +53,9 @@ pub struct Session {
 }
 
 impl Session {
-    /// A new connection. `dev_auth` is whether DEV ONLY identities are accepted.
-    pub fn new(dev_auth: bool) -> Self {
+    /// A new connection.
+    pub fn new() -> Self {
         Self {
-            dev_auth,
             state: State::New,
             sent: HashMap::new(),
             more_waiting: false,
@@ -69,7 +66,7 @@ impl Session {
     pub fn device(&self) -> Option<Uuid> {
         match self.state {
             State::Ready { device } => Some(device),
-            State::New | State::Identifying { .. } => None,
+            State::New | State::Identifying => None,
         }
     }
 
@@ -77,8 +74,8 @@ impl Session {
     pub fn on_binary(&mut self, bytes: &[u8]) -> Step {
         let body = match (decode_frame(bytes), self.state) {
             (Ok(body), _) => body,
-            (Err(_), State::New | State::Identifying { .. }) => {
-                return Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify");
+            (Err(_), State::New | State::Identifying) => {
+                return Step::Close(close::PROTOCOL_ERROR, "the first frame must be Identify");
             }
             (Err(FrameError::TooLarge { .. }), State::Ready { .. }) => {
                 return reply_error(ErrorCode::BadFrame, "the frame is too large");
@@ -89,26 +86,16 @@ impl Session {
         };
         match (body, self.state) {
             (Body::Identify(identify), State::New) => {
-                if !self.dev_auth {
-                    return Step::Close(
-                        close::INVALID_IDENTITY,
-                        "token sign-in is not available yet",
-                    );
-                }
-                match (id(&identify.dev_user_id), id(&identify.dev_device_id)) {
-                    (Ok(user), Ok(device)) => {
-                        self.state = State::Identifying { user, device };
-                        Step::Identify { user, device }
-                    }
-                    _ => Step::Close(close::INVALID_IDENTITY, "invalid dev identity"),
-                }
+                self.state = State::Identifying;
+                Step::Identify(identify.token)
             }
-            (_, State::New | State::Identifying { .. }) => {
-                Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify")
+            (_, State::New | State::Identifying) => {
+                Step::Close(close::PROTOCOL_ERROR, "the first frame must be Identify")
             }
             (Body::Identify(_), State::Ready { .. }) => {
                 reply_error(ErrorCode::BadFrame, "already identified")
             }
+
             (Body::Send(send), State::Ready { .. }) => Step::Send(send),
             (Body::Ack(ack), State::Ready { .. }) => Step::Ack(ack),
             (Body::Ping(_), State::Ready { .. }) => Step::Reply(Body::Pong(Pong {})),
@@ -120,22 +107,24 @@ impl Session {
     pub fn on_text(&mut self) -> Step {
         match self.state {
             State::Ready { .. } => reply_error(ErrorCode::BadFrame, "frames must be binary"),
-            State::New | State::Identifying { .. } => {
-                Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify")
+            State::New | State::Identifying => {
+                Step::Close(close::PROTOCOL_ERROR, "the first frame must be Identify")
             }
         }
     }
 
-    /// message-svc confirmed the identity that [`Step::Identify`] asked about: the
-    /// `Ready` frame to send. `None` if no identity is being checked.
-    pub fn identified(&mut self, heartbeat: Duration) -> Option<Body> {
-        let State::Identifying { user, device } = self.state else {
+    /// The token of [`Step::Identify`] passed and message-svc accepted the device it
+    /// names: the `Ready` frame to send. `None` if no identity is being checked.
+    pub fn identified(&mut self, claims: &Claims, heartbeat: Duration) -> Option<Body> {
+        if self.state != State::Identifying {
             return None;
+        }
+        self.state = State::Ready {
+            device: claims.device,
         };
-        self.state = State::Ready { device };
         Some(Body::Ready(Ready {
-            user_id: id_bytes(user),
-            device_id: id_bytes(device),
+            user_id: id_bytes(claims.user),
+            device_id: id_bytes(claims.device),
             heartbeat_ms: u32::try_from(heartbeat.as_millis()).unwrap_or(u32::MAX),
         }))
     }
@@ -169,6 +158,12 @@ impl Session {
     }
 }
 
+impl Default for Session {
+    fn default() -> Self {
+        Self::new()
+    }
+}
+
 /// When to retry a failed sync: after 1 s, doubling up to 30 s, and from 1 s again once a
 /// sync succeeds.
 #[derive(Debug, Clone, Copy, PartialEq, Eq)]
@@ -222,23 +217,32 @@ mod tests {
 
     use super::*;
 
-    fn identify(user: Uuid, device: Uuid) -> Vec<u8> {
+    fn identify(token: &str) -> Vec<u8> {
         encode_frame(Body::Identify(Identify {
-            token: String::new(),
-            dev_user_id: id_bytes(user),
-            dev_device_id: id_bytes(device),
+            token: token.to_owned(),
         }))
     }
 
+    fn claims() -> Claims {
+        Claims {
+            user: Uuid::now_v7(),
+            device: Uuid::now_v7(),
+            issued_at: 1_700_000_000,
+            expires_at: 1_700_000_900,
+        }
+    }
+
     fn ready_session() -> (Session, Uuid) {
-        let mut session = Session::new(true);
-        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
+        let mut session = Session::new();
         assert_eq!(
-            session.on_binary(&identify(user, device)),
-            Step::Identify { user, device }
+            session.on_binary(&identify("token")),
+            Step::Identify("token".to_owned())
         );
-        session.identified(Duration::from_secs(30)).unwrap();
-        (session, device)
+        let claims = claims();
+        session
+            .identified(&claims, Duration::from_secs(30))
+            .unwrap();
+        (session, claims.device)
     }
 
     fn error_code(step: Step) -> ErrorCode {
@@ -257,87 +261,64 @@ mod tests {
     }
 
     #[test]
-    fn a_dev_identity_is_checked_then_ready() {
-        let mut session = Session::new(true);
-        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
+    fn identify_hands_over_its_token_and_ready_names_the_tokens_device() {
+        let mut session = Session::new();
         assert_eq!(
-            session.on_binary(&identify(user, device)),
-            Step::Identify { user, device }
+            session.on_binary(&identify("a.b.c")),
+            Step::Identify("a.b.c".to_owned())
         );
         assert_eq!(session.device(), None);
-        let ready = session.identified(Duration::from_secs(30));
+        let claims = claims();
+        let ready = session.identified(&claims, Duration::from_secs(30));
         assert_eq!(
             ready,
             Some(Body::Ready(Ready {
-                user_id: id_bytes(user),
-                device_id: id_bytes(device),
+                user_id: id_bytes(claims.user),
+                device_id: id_bytes(claims.device),
                 heartbeat_ms: 30_000,
             }))
         );
-        assert_eq!(session.device(), Some(device));
-        assert_eq!(session.identified(Duration::from_secs(30)), None);
+        assert_eq!(session.device(), Some(claims.device));
+        assert_eq!(session.identified(&claims, Duration::from_secs(30)), None);
     }
 
     #[test]
     fn identified_without_a_pending_identity_does_nothing() {
-        let mut session = Session::new(true);
-        assert_eq!(session.identified(Duration::from_secs(30)), None);
+        let mut session = Session::new();
+        assert_eq!(session.identified(&claims(), Duration::from_secs(30)), None);
         assert_eq!(session.device(), None);
     }
 
+    /// A protocol mistake before Ready is no reason to sign out: 1002, not 4001.
     #[test]
-    fn without_dev_auth_identify_closes_with_4001() {
-        let mut session = Session::new(false);
-        assert_eq!(
-            session.on_binary(&identify(Uuid::now_v7(), Uuid::now_v7())),
-            Step::Close(4001, "token sign-in is not available yet")
-        );
-    }
-
-    #[test]
-    fn anything_but_identify_first_closes_with_4001() {
+    fn anything_but_identify_first_is_a_protocol_error() {
         let ping = encode_frame(Body::Ping(Ping {}));
         for bytes in [ping.as_slice(), &[0xff, 0xff], &[]] {
-            let mut session = Session::new(true);
+            let mut session = Session::new();
             assert_eq!(
                 session.on_binary(bytes),
-                Step::Close(4001, "the first frame must be Identify")
+                Step::Close(1002, "the first frame must be Identify")
             );
         }
         assert_eq!(
-            Session::new(true).on_text(),
-            Step::Close(4001, "the first frame must be Identify")
+            Session::new().on_text(),
+            Step::Close(1002, "the first frame must be Identify")
         );
     }
 
     #[test]
-    fn a_dev_identity_with_malformed_ids_closes_with_4001() {
-        let mut session = Session::new(true);
-        let bytes = encode_frame(Body::Identify(Identify {
-            token: String::new(),
-            dev_user_id: vec![1, 2, 3],
-            dev_device_id: id_bytes(Uuid::now_v7()),
-        }));
-        assert_eq!(
-            session.on_binary(&bytes),
-            Step::Close(4001, "invalid dev identity")
-        );
-    }
-
-    #[test]
-    fn while_identifying_any_other_frame_or_a_second_identify_closes_with_4001() {
+    fn while_identifying_any_other_frame_or_a_second_identify_is_a_protocol_error() {
         let ping = encode_frame(Body::Ping(Ping {}));
-        let second = identify(Uuid::now_v7(), Uuid::now_v7());
+        let second = identify("another");
         for bytes in [ping.as_slice(), second.as_slice()] {
-            let mut session = Session::new(true);
-            let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
+            let mut session = Session::new();
             assert_eq!(
-                session.on_binary(&identify(user, device)),
-                Step::Identify { user, device }
+                session.on_binary(&identify("token")),
+                Step::Identify("token".to_owned())
             );
             assert_eq!(
                 session.on_binary(bytes),
-                Step::Close(4001, "the first frame must be Identify")
+                Step::Close(1002, "the first frame must be Identify")
             );
             assert_eq!(session.device(), None);
         }
@@ -347,8 +328,8 @@ mod tests {
     fn an_oversized_frame_closes_before_identify_and_errors_once_ready() {
         let oversized = vec![0u8; MAX_FRAME_BYTES + 1];
         assert_eq!(
-            Session::new(true).on_binary(&oversized),
-            Step::Close(4001, "the first frame must be Identify")
+            Session::new().on_binary(&oversized),
+            Step::Close(1002, "the first frame must be Identify")
         );
         let (mut session, _) = ready_session();
         assert_eq!(
@@ -357,7 +338,6 @@ mod tests {
         );
         assert!(session.device().is_some());
     }
-
     #[test]
     fn once_ready_frames_become_steps() {
         let (mut session, _) = ready_session();
@@ -386,7 +366,7 @@ mod tests {
     #[test]
     fn once_ready_bad_frames_get_errors_but_keep_the_connection() {
         let (mut session, _) = ready_session();
-        let again = identify(Uuid::now_v7(), Uuid::now_v7());
+        let again = identify("again");
         assert_eq!(error_code(session.on_binary(&again)), ErrorCode::BadFrame);
         assert_eq!(
             error_code(session.on_binary(&[0xff, 0xff])),
```

Change `crates/push-proto/proto/push/v1/frame.proto`:

```diff
diff --git a/crates/push-proto/proto/push/v1/frame.proto b/crates/push-proto/proto/push/v1/frame.proto
index b8dedb2..9bb0e2a 100644
--- a/crates/push-proto/proto/push/v1/frame.proto
+++ b/crates/push-proto/proto/push/v1/frame.proto
@@ -42,18 +42,15 @@ message Frame {
   // 10-31 are free for later client frames, 42-63 for later server frames.
 }
 
-// Must be the first frame, within 5 seconds of connecting.
+// Must be the first frame, within 5 seconds of connecting. The gateway learns the user
+// and the device only from the token (spec §5.3).
 message Identify {
-  // The access token (a JWT, Plan 5). Not checked yet.
+  // The access token auth-svc issued for this device (a JWT, spec §5.1).
   string token = 1;
-  // DEV ONLY: accepted only by a gateway started with PUSH_DEV_AUTH=1, which trusts
-  // these ids without any proof. Removed when real tokens arrive (Plan 8); when
-  // removing them, reserve their field numbers (14, 15) and names.
-  bytes dev_user_id = 14;
-  bytes dev_device_id = 15;
-  // private_reads: the device does not reveal reads to others.
-  reserved 16;
-  reserved "private_reads";
+  // 14 and 15 held the DEV ONLY ids that let a local gateway skip the token (Plans 4 to
+  // 7); 16 is private_reads: the device does not reveal reads to others.
+  reserved 14, 15, 16;
+  reserved "dev_user_id", "dev_device_id", "private_reads";
 }
 
 message Ready {
```

Change `crates/push-smoke/src/lib.rs`:

```diff
diff --git a/crates/push-smoke/src/lib.rs b/crates/push-smoke/src/lib.rs
index a986d38..244678e 100644
--- a/crates/push-smoke/src/lib.rs
+++ b/crates/push-smoke/src/lib.rs
@@ -13,8 +13,8 @@
 //!    contact and Bob accepts, Carol redeems Alice's invite, Alice opens the direct
 //!    conversation with Bob; auth-svc shows Alice the profiles of Bob and Carol, and Bob
 //!    none of Carol's;
-//! 4. connects Bob's WebSocket and then Alice's, each with a DEV ONLY `Identify` naming
-//!    the device auth-svc registered, and waits for `Ready`;
+//! 4. connects Bob's WebSocket and then Alice's, each identifying with the access token
+//!    auth-svc issued for its device, and waits for `Ready`;
 //! 5. sends a message from Alice and waits for her `SendAck` and Bob's `Deliver`; acks it
 //!    from Bob, then pings: the `Pong` proves the ack went through, since the gateway
 //!    handles a connection's frames in order and answers a failed ack with an `Error`;
@@ -27,8 +27,7 @@
 //!    relays both, message-svc kicks Carol's device (revoked) and Bob's (password reset) on
 //!    their Redis channels, where the smoke test listens, and refuses Carol's device.
 //!
-//! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`, and
-//! the gateway with `PUSH_DEV_AUTH=1`.
+//! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`.
 
 mod auth;
 mod kicks;
@@ -650,9 +649,7 @@ impl Client {
         send(
             &mut socket,
             Body::Identify(Identify {
-                token: String::new(),
-                dev_user_id: id_bytes(self.user),
-                dev_device_id: id_bytes(self.device),
+                token: self.access_token.clone(),
             }),
         )
         .await?;
```

Change `deploy/helm/push-messenger/templates/NOTES.txt`:

```diff
diff --git a/deploy/helm/push-messenger/templates/NOTES.txt b/deploy/helm/push-messenger/templates/NOTES.txt
index a92502b..111b455 100644
--- a/deploy/helm/push-messenger/templates/NOTES.txt
+++ b/deploy/helm/push-messenger/templates/NOTES.txt
@@ -12,9 +12,8 @@ the released volumes and the provisioner's data directory, which deleting the na
 alone can leave behind.
 {{- if include "push.devMode" . }}
 
-DEV MODE IS ON: the gateway accepts identities without a token and the worker shows the
-emails its log sender delivered at /dev/mailbox (not routed by the ingress). This is for a
-local cluster only.
+DEV MODE IS ON: the worker shows the emails its log sender delivered at /dev/mailbox (not
+routed by the ingress). This is for a local cluster only.
 
 Run the end-to-end smoke test:
   helm test {{ .Release.Name }} --namespace {{ .Release.Namespace }} --logs
```

Change `deploy/helm/push-messenger/templates/_helpers.tpl`:

```diff
diff --git a/deploy/helm/push-messenger/templates/_helpers.tpl b/deploy/helm/push-messenger/templates/_helpers.tpl
index 34ff343..b9d4026 100644
--- a/deploy/helm/push-messenger/templates/_helpers.tpl
+++ b/deploy/helm/push-messenger/templates/_helpers.tpl
@@ -52,7 +52,7 @@ local, so dev mode cannot reach a real deployment by accident.
 {{- define "push.devMode" -}}
 {{- if .Values.devMode.enabled -}}
 {{- if ne .Values.devMode.acknowledge "insecure-local-only" -}}
-{{- fail "devMode.enabled switches authentication off: set devMode.acknowledge to \"insecure-local-only\", and only in a local values file" -}}
+{{- fail "devMode.enabled serves every email the worker sends, links and all, without authentication: set devMode.acknowledge to \"insecure-local-only\", and only in a local values file" -}}
 {{- end -}}
 {{- $host := .Values.ingress.host -}}
 {{- if not (or (hasSuffix ".local" $host) (hasSuffix ".localhost" $host) (hasSuffix ".test" $host)) -}}
@@ -62,7 +62,7 @@ true
 {{- end -}}
 {{- end -}}
 
-{{/* "1" or "0" for the PUSH_DEV_AUTH and PUSH_DEV_MAILBOX variables. */}}
+{{/* "1" or "0" for the PUSH_DEV_MAILBOX variable. */}}
 {{- define "push.devFlag" -}}
 {{- if include "push.devMode" . -}}1{{- else -}}0{{- end -}}
 {{- end -}}
```

Change `deploy/helm/push-messenger/templates/gateway.yaml`:

```diff
diff --git a/deploy/helm/push-messenger/templates/gateway.yaml b/deploy/helm/push-messenger/templates/gateway.yaml
index a5de69d..303e9f0 100644
--- a/deploy/helm/push-messenger/templates/gateway.yaml
+++ b/deploy/helm/push-messenger/templates/gateway.yaml
@@ -59,8 +59,12 @@ spec:
                   key: redis-url
             - name: PUSH_HEARTBEAT_MS
               value: {{ .Values.gateway.heartbeatMs | quote }}
-            - name: PUSH_DEV_AUTH
-              value: {{ include "push.devFlag" . | quote }}
+            # Access tokens are checked against auth-svc's key set (GET /v1/auth/jwks)
+            # and must name the gateway's public URL as their audience.
+            - name: PUSH_AUTH_URL
+              value: {{ printf "http://%s:8080" (include "push.name" (dict "root" . "component" "auth")) | quote }}
+            - name: PUSH_TOKEN_AUDIENCE
+              value: {{ include "push.socketUrl" . | quote }}
           ports:
             - name: http
               containerPort: 8080
```

Change `deploy/helm/push-messenger/values-minikube.yaml`:

```diff
diff --git a/deploy/helm/push-messenger/values-minikube.yaml b/deploy/helm/push-messenger/values-minikube.yaml
index a7c999e..5471694 100644
--- a/deploy/helm/push-messenger/values-minikube.yaml
+++ b/deploy/helm/push-messenger/values-minikube.yaml
@@ -1,5 +1,5 @@
-# Local minikube only (deploy/minikube/up.ps1). Turns on the DEV ONLY identity, setup API
-# and mailbox that the smoke test needs; the worker logs emails instead of sending them.
+# Local minikube only (deploy/minikube/up.ps1). Turns on the DEV ONLY mailbox the smoke
+# test reads its emails from; the worker logs emails instead of sending them.
 
 devMode:
   enabled: true
```

Change `deploy/helm/push-messenger/values.yaml`:

```diff
diff --git a/deploy/helm/push-messenger/values.yaml b/deploy/helm/push-messenger/values.yaml
index 5007368..5b13a34 100644
--- a/deploy/helm/push-messenger/values.yaml
+++ b/deploy/helm/push-messenger/values.yaml
@@ -24,10 +24,9 @@ images:
 # stay the same (the bring-up script sets it after rebuilding the images).
 buildId: ""
 
-# DEV ONLY switches: the gateway accepts identities without a token (PUSH_DEV_AUTH=1), the
-# worker shows the emails its log sender delivered at /dev/mailbox (PUSH_DEV_MAILBOX=1), a
-# webhook secret is generated when none is set, and `helm test` runs the smoke test. Never
-# enable outside a local cluster.
+# DEV ONLY switches: the worker shows the emails its log sender delivered at /dev/mailbox
+# (PUSH_DEV_MAILBOX=1), a webhook secret is generated when none is set, and `helm test`
+# runs the smoke test. Never enable outside a local cluster.
 devMode:
   enabled: false
   # Must be exactly "insecure-local-only" when enabled.
```

Change `deploy/helm/test-chart.ps1`:

```diff
diff --git a/deploy/helm/test-chart.ps1 b/deploy/helm/test-chart.ps1
index 3546af7..9d8ba3e 100644
--- a/deploy/helm/test-chart.ps1
+++ b/deploy/helm/test-chart.ps1
@@ -33,7 +33,12 @@ Check 'lint with the default values' { helm lint $chart --strict | Out-Null; $LA
 Check 'lint with values-minikube.yaml' { helm lint $chart --strict -f $minikube | Out-Null; $LASTEXITCODE -eq 0 }
 
 $default = helm template push $chart | Out-String
-Check 'default: the gateway refuses dev identities' { $default -match 'name: PUSH_DEV_AUTH\s+value: "0"' }
+Check 'default: the gateway checks tokens with auth-svc''s keys, for its own URL, and has no dev identities' {
+    $gateway = Doc $default 'Deployment' 'push-gateway'
+    $gateway -match 'name: PUSH_AUTH_URL\s+value: "http://push-auth:8080"' -and
+        $gateway -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "wss://push.example.com/v1/socket"' -and
+        $default -notmatch 'PUSH_DEV_AUTH'
+}
 Check 'default: the DevSetup switch is gone' { $default -notmatch 'PUSH_DEV_SETUP' }
 Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
 Check 'default: the worker has no dev mailbox and logs email; auth-svc has no mailbox at all' {
@@ -123,7 +128,10 @@ Check 'resend: the worker sends through Resend with the key from the Secret' {
 }
 
 $dev = helm template push $chart -f $minikube | Out-String
-Check 'minikube: dev identities on' { $dev -match 'name: PUSH_DEV_AUTH\s+value: "1"' }
+Check 'minikube: no dev identities either; tokens name ws://push.local/v1/socket' {
+    $dev -notmatch 'PUSH_DEV_AUTH' -and
+        (Doc $dev 'Deployment' 'push-gateway') -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "ws://push.local/v1/socket"'
+}
 Check 'minikube: the smoke test calls message-svc and reads the Redis URL from the Secret' {
     $smoke = Doc $dev 'Pod' 'push-smoke'
     $smoke -match '- --message-svc\s+- "http://push-message-svc:50051"' -and
```

Change `crates/push-gateway/tests/gateway.rs`:

```diff
diff --git a/crates/push-gateway/tests/gateway.rs b/crates/push-gateway/tests/gateway.rs
index a4b0e96..d6b4604 100644
--- a/crates/push-gateway/tests/gateway.rs
+++ b/crates/push-gateway/tests/gateway.rs
@@ -6,6 +6,7 @@ use std::sync::{Arc, Mutex};
 use std::time::Duration;
 
 use futures_util::{SinkExt, StreamExt};
+use push_auth::tokens::AccessTokens;
 use push_domain::{DeviceId, InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock, UserId};
 use push_gateway::{Config, serve};
 use push_message_svc::{MessagingApi, RedisNotifier};
@@ -98,18 +99,24 @@ struct Stack {
     /// message-svc's domain, to set up users, contacts and conversations directly.
     messaging: Arc<Messaging<InMemoryStore, SystemClock, RedisNotifier>>,
     message_svc: String,
+    /// auth-svc's token issuer, whose key set the gateway fetches.
+    issuer: AccessTokens,
     gateway: SocketAddr,
     shutdown: CancellationToken,
     server: JoinHandle<anyhow::Result<()>>,
 }
 
-/// Redis, message-svc's API on the in-memory store, and the gateway, on free ports.
-async fn stack(dev_auth: bool) -> Stack {
-    stack_with(dev_auth, 0).await
+/// The URL the gateway names itself by: the audience of its tokens.
+const AUDIENCE: &str = "ws://push.test/v1/socket";
+
+/// Redis, message-svc's API on the in-memory store, auth-svc's key set, and the gateway,
+/// on free ports.
+async fn stack() -> Stack {
+    stack_with(0).await
 }
 
 /// As [`stack`], with message-svc failing its first `sync_failures` syncs.
-async fn stack_with(dev_auth: bool, sync_failures: u32) -> Stack {
+async fn stack_with(sync_failures: u32) -> Stack {
     let redis = Redis::default()
         .with_tag("7-alpine")
         .start()
@@ -137,16 +144,28 @@ async fn stack_with(dev_auth: bool, sync_failures: u32) -> Stack {
             .serve_with_incoming(TcpListenerStream::new(grpc)),
     );
 
+    // auth-svc's key set, as `GET /v1/auth/jwks` serves it.
+    let issuer = AccessTokens::from_seed(&[7; 32], AUDIENCE).unwrap();
+    let jwks = issuer.jwks();
+    let keys = TcpListener::bind("127.0.0.1:0").await.unwrap();
+    let auth_url = format!("http://{}", keys.local_addr().unwrap());
+    let routes = axum::Router::new().route(
+        "/v1/auth/jwks",
+        axum::routing::get(move || async move { axum::Json(jwks) }),
+    );
+    tokio::spawn(axum::serve(keys, routes).into_future());
+
     let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
     let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
     let gateway = listener.local_addr().unwrap();
     let config = Config {
         message_svc_url: message_svc.clone(),
         redis_url,
+        auth_url,
+        token_audience: AUDIENCE.to_owned(),
         listen_addr: gateway,
         health_addr: health.local_addr().unwrap(),
         heartbeat: Duration::from_secs(30),
-        dev_auth,
     };
     let shutdown = CancellationToken::new();
     let server = tokio::spawn(serve(config, listener, health, shutdown.clone()));
@@ -154,12 +173,20 @@ async fn stack_with(dev_auth: bool, sync_failures: u32) -> Stack {
         _redis: redis,
         messaging,
         message_svc,
+        issuer,
         gateway,
         shutdown,
         server,
     }
 }
 
+fn now() -> i64 {
+    std::time::SystemTime::now()
+        .duration_since(std::time::UNIX_EPOCH)
+        .unwrap()
+        .as_secs() as i64
+}
+
 /// A user with one device, relayed to message-svc as the worker relays them.
 async fn person(stack: &Stack) -> (Uuid, Uuid) {
     let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
@@ -194,12 +221,24 @@ async fn send(socket: &mut Socket, body: Body) {
         .unwrap();
 }
 
-fn identify(user: Uuid, device: Uuid) -> Body {
-    Body::Identify(Identify {
-        token: String::new(),
-        dev_user_id: id_bytes(user),
-        dev_device_id: id_bytes(device),
-    })
+/// An `Identify` with an access token issued now.
+fn identify(stack: &Stack, user: Uuid, device: Uuid) -> Body {
+    identify_issued(stack, user, device, now())
+}
+
+/// An `Identify` with an access token issued at `issued_at` (seconds); it expires 15
+/// minutes later.
+fn identify_issued(stack: &Stack, user: Uuid, device: Uuid, issued_at: i64) -> Body {
+    let (token, _) = stack.issuer.issue(user, device, issued_at);
+    Body::Identify(Identify { token })
+}
+
+/// Connects and identifies with a token issued now; returns once `Ready` arrived.
+async fn ready(stack: &Stack, user: Uuid, device: Uuid) -> Socket {
+    let mut socket = connect(stack).await;
+    send(&mut socket, identify(stack, user, device)).await;
+    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
+    socket
 }
 
 /// The next frame, within 5 s.
@@ -234,23 +273,21 @@ async fn close_code(socket: &mut Socket) -> u16 {
 
 #[tokio::test]
 async fn a_message_reaches_the_other_device_and_the_sender_gets_its_ack() {
-    let stack = stack(true).await;
+    let stack = stack().await;
     let (alice, alice_device) = person(&stack).await;
     let (bob, bob_device) = person(&stack).await;
     let conversation = direct_chat(&stack, alice, bob).await;
 
     let mut bob_socket = connect(&stack).await;
-    send(&mut bob_socket, identify(bob, bob_device)).await;
-    let Body::Ready(ready) = next(&mut bob_socket).await else {
+    send(&mut bob_socket, identify(&stack, bob, bob_device)).await;
+    let Body::Ready(ready_frame) = next(&mut bob_socket).await else {
         panic!("expected Ready");
     };
-    assert_eq!(ready.user_id, id_bytes(bob));
-    assert_eq!(ready.device_id, id_bytes(bob_device));
-    assert_eq!(ready.heartbeat_ms, 30_000);
+    assert_eq!(ready_frame.user_id, id_bytes(bob));
+    assert_eq!(ready_frame.device_id, id_bytes(bob_device));
+    assert_eq!(ready_frame.heartbeat_ms, 30_000);
 
-    let mut alice_socket = connect(&stack).await;
-    send(&mut alice_socket, identify(alice, alice_device)).await;
-    assert!(matches!(next(&mut alice_socket).await, Body::Ready(_)));
+    let mut alice_socket = ready(&stack, alice, alice_device).await;
     let client_msg_id = id_bytes(Uuid::now_v7());
     send(
         &mut alice_socket,
@@ -296,44 +333,91 @@ async fn a_message_reaches_the_other_device_and_the_sender_gets_its_ack() {
 }
 
 #[tokio::test]
-async fn without_dev_auth_identify_is_closed_with_4001() {
-    let stack = stack(false).await;
+async fn an_invalid_token_is_closed_with_4001_and_an_expired_one_with_4002() {
+    let stack = stack().await;
     let (user, device) = person(&stack).await;
+    let elsewhere = AccessTokens::from_seed(&[7; 32], "wss://elsewhere.test/v1/socket").unwrap();
+    let stranger = AccessTokens::from_seed(&[8; 32], AUDIENCE).unwrap();
+    for token in [
+        "not a token".to_owned(),
+        String::new(),
+        elsewhere.issue(user, device, now()).0,
+    ] {
+        let mut socket = connect(&stack).await;
+        send(&mut socket, Body::Identify(Identify { token })).await;
+        assert_eq!(close_code(&mut socket).await, 4001);
+    }
+    // A key the gateway does not hold even after fetching the set again, such as one
+    // retired before it started: refresh, never a sign-out.
     let mut socket = connect(&stack).await;
-    send(&mut socket, identify(user, device)).await;
-    assert_eq!(close_code(&mut socket).await, 4001);
+    let token = stranger.issue(user, device, now()).0;
+    send(&mut socket, Body::Identify(Identify { token })).await;
+    assert_eq!(close_code(&mut socket).await, 4002);
+    // Expired 90 s ago: past the 60 s leeway. The client refreshes, then reconnects.
+    let mut socket = connect(&stack).await;
+    send(
+        &mut socket,
+        identify_issued(&stack, user, device, now() - 900 - 90),
+    )
+    .await;
+    assert_eq!(close_code(&mut socket).await, 4002);
 }
 
 #[tokio::test]
-async fn a_device_of_another_user_is_closed_with_4001() {
-    let stack = stack(true).await;
-    let (alice, _) = person(&stack).await;
-    let (_, bob_device) = person(&stack).await;
+async fn message_svc_decides_whether_the_tokens_device_may_connect() {
+    let stack = stack().await;
+    let (user, device) = person(&stack).await;
+
+    // Registered moments ago and not relayed yet: retry shortly, never a sign-out.
+    let mut socket = connect(&stack).await;
+    send(&mut socket, identify(&stack, user, Uuid::now_v7())).await;
+    assert_eq!(close_code(&mut socket).await, 4006);
+
+    // A token issued before the password reset: sign in again.
+    let before = now() - 60;
+    stack
+        .messaging
+        .password_reset(UserId(user), None)
+        .await
+        .unwrap();
     let mut socket = connect(&stack).await;
-    send(&mut socket, identify(alice, bob_device)).await;
+    send(&mut socket, identify_issued(&stack, user, device, before)).await;
     assert_eq!(close_code(&mut socket).await, 4001);
+    let mut socket = connect(&stack).await;
+    send(
+        &mut socket,
+        identify_issued(&stack, user, device, now() + 1),
+    )
+    .await;
+    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
 
-    // A device message-svc has not heard of yet (not relayed) is not refused for good:
-    // message-svc answers UNAVAILABLE, and the client retries (Plan 8 gives it its own code).
+    // A revoked device: stop.
+    stack
+        .messaging
+        .device_revoked(DeviceId(device), UserId(user))
+        .await
+        .unwrap();
     let mut socket = connect(&stack).await;
-    send(&mut socket, identify(alice, Uuid::now_v7())).await;
-    assert_eq!(close_code(&mut socket).await, 1011);
+    send(
+        &mut socket,
+        identify_issued(&stack, user, device, now() + 1),
+    )
+    .await;
+    assert_eq!(close_code(&mut socket).await, 4003);
 }
 
 #[tokio::test]
 async fn a_connection_that_never_identifies_is_closed_after_5_seconds() {
-    let stack = stack(true).await;
+    let stack = stack().await;
     let mut socket = connect(&stack).await;
-    assert_eq!(close_code(&mut socket).await, 4001);
+    assert_eq!(close_code(&mut socket).await, 1002);
 }
 
 #[tokio::test]
 async fn shutdown_closes_connections_with_4005() {
-    let stack = stack(true).await;
+    let stack = stack().await;
     let (user, device) = person(&stack).await;
-    let mut socket = connect(&stack).await;
-    send(&mut socket, identify(user, device)).await;
-    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
+    let mut socket = ready(&stack, user, device).await;
 
     stack.shutdown.cancel();
     assert_eq!(close_code(&mut socket).await, 4005);
@@ -375,12 +459,12 @@ async fn chat_with_backlog(stack: &Stack, count: u32) -> ((Uuid, Uuid), (Uuid, U
 
 #[tokio::test]
 async fn a_backlog_larger_than_one_sync_batch_is_delivered_as_the_device_acks() {
-    let stack = stack(true).await;
+    let stack = stack().await;
     let backlog = MAX_SYNC_BATCH + 1;
     let (_alice, (bob, bob_device), conversation) = chat_with_backlog(&stack, backlog).await;
 
     let mut socket = connect(&stack).await;
-    send(&mut socket, identify(bob, bob_device)).await;
+    send(&mut socket, identify(&stack, bob, bob_device)).await;
     assert!(matches!(next(&mut socket).await, Body::Ready(_)));
     for expected in 1..=i64::from(MAX_SYNC_BATCH) {
         let Body::Deliver(entry) = next(&mut socket).await else {
@@ -406,11 +490,11 @@ async fn a_backlog_larger_than_one_sync_batch_is_delivered_as_the_device_acks()
 
 #[tokio::test]
 async fn a_failed_sync_is_retried() {
-    let stack = stack_with(true, 1).await;
+    let stack = stack_with(1).await;
     let (_alice, (bob, bob_device), _conversation) = chat_with_backlog(&stack, 1).await;
 
     let mut socket = connect(&stack).await;
-    send(&mut socket, identify(bob, bob_device)).await;
+    send(&mut socket, identify(&stack, bob, bob_device)).await;
     assert!(matches!(next(&mut socket).await, Body::Ready(_)));
     // The sync after Ready fails; nothing rings Bob's channel, so only a retry delivers.
     let Body::Deliver(entry) = next(&mut socket).await else {
```

Change `crates/push-smoke/tests/local.rs`:

```diff
diff --git a/crates/push-smoke/tests/local.rs b/crates/push-smoke/tests/local.rs
index d89b797..5275b25 100644
--- a/crates/push-smoke/tests/local.rs
+++ b/crates/push-smoke/tests/local.rs
@@ -125,10 +125,11 @@ async fn the_smoke_test_passes_against_the_whole_stack() {
         push_gateway::Config {
             message_svc_url: message_svc_url.clone(),
             redis_url: redis_url.clone(),
+            auth_url: auth_url.clone(),
+            token_audience: socket_url.clone(),
             listen_addr: listener.local_addr().unwrap(),
             health_addr: gateway_health.local_addr().unwrap(),
             heartbeat: Duration::from_secs(30),
-            dev_auth: true,
         },
         listener,
         gateway_health,
```
- [ ] **Step 2: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
pwsh deploy/helm/test-chart.ps1
git add -A README.md crates deploy
git commit -m "feat(gateway): Identify with access tokens; the DEV ONLY identity is gone"
```

Expected: 836 tests pass, no warnings, no diff, `all chart checks passed`.
---

### Task 6: ReAuth, AuthExpiring and the 4002 deadline

**Files:**
- Modify: `crates/push-gateway/src/server.rs`, `crates/push-gateway/src/session.rs`, `crates/push-gateway/tests/gateway.rs`

Decision 7. `AuthExpiring` 60 s before `exp`; a good `ReAuth` (same user and device,
admitted again by message-svc) moves the deadline and is answered with nothing; without one
the connection closes with 4002 at `exp + 30 s`; a token of another device closes with
4001; unreachable keys or message-svc get a retryable `Error`. A token presented to `Identify`
already past `exp + 30 s` (30–60 s expired, still inside the leeway) is refused with 4002
at once instead of getting `Ready` and an immediate 4002 (decision 4).

- [ ] **Step 1: Make the changes**

Change `crates/push-gateway/src/server.rs`:

```diff
diff --git a/crates/push-gateway/src/server.rs b/crates/push-gateway/src/server.rs
index fc3439d..9440597 100644
--- a/crates/push-gateway/src/server.rs
+++ b/crates/push-gateway/src/server.rs
@@ -2,7 +2,7 @@
 //! the health endpoints and graceful shutdown.
 
 use std::sync::Arc;
-use std::time::Duration;
+use std::time::{Duration, SystemTime, UNIX_EPOCH};
 
 use axum::Router;
 use axum::extract::State;
@@ -12,8 +12,8 @@ use axum::response::Response;
 use axum::routing::get;
 use push_proto::internal::v1::messaging_client::MessagingClient;
 use push_proto::internal::v1::{AckRequest, IdentifyRequest, SendRequest, SyncRequest};
-use push_proto::v1::ErrorCode;
 use push_proto::v1::frame::Body;
+use push_proto::v1::{AuthExpiring, Error, ErrorCode};
 use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes};
 use tokio::net::TcpListener;
 use tokio::time::Instant;
@@ -31,7 +31,11 @@ use crate::tokens::{Claims, TokenError, Verifier};
 const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(5);
 /// How long shutdown waits for connections to close.
 const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
-
+/// `AuthExpiring` comes this long before the token expires (spec §7.3).
+const EXPIRY_WARNING: Duration = Duration::from_secs(60);
+/// A connection must send a valid `ReAuth` by this long after its token expired, or it is
+/// closed with 4002 (spec §7.3).
+const REAUTH_GRACE: Duration = Duration::from_secs(30);
 /// How often the token keys are fetched again: auth-svc lets clients cache them 5 minutes.
 const KEYS_REFRESH: Duration = Duration::from_secs(300);
 /// How soon to fetch the keys again after a failed fetch.
@@ -178,8 +182,10 @@ async fn connection(mut socket: WebSocket, app: App) {
         socket,
         session,
         messaging: app.messaging.clone(),
+        verifier: app.verifier.clone(),
         user: claims.user,
         device: claims.device,
+        expiry: Expiry::new(claims.expires_at),
         retry: RetryDelay::default(),
         retry_at: None,
     };
@@ -188,6 +194,11 @@ async fn connection(mut socket: WebSocket, app: App) {
     }
     loop {
         let retry_at = connected.retry_at;
+        let (warn_at, warned, deadline) = (
+            connected.expiry.warn_at,
+            connected.expiry.warned,
+            connected.expiry.deadline,
+        );
         tokio::select! {
             () = app.shutdown.cancelled() => {
                 close_with(&mut connected.socket, close::SHUTTING_DOWN, "server shutting down").await;
@@ -204,7 +215,15 @@ async fn connection(mut socket: WebSocket, app: App) {
                     return;
                 }
             }
-
+            () = tokio::time::sleep_until(warn_at), if !warned => {
+                if connected.warn_expiry().await.is_err() {
+                    return;
+                }
+            }
+            () = tokio::time::sleep_until(deadline) => {
+                close_with(&mut connected.socket, close::REAUTH_REQUIRED, "the access token expired; send ReAuth in time").await;
+                return;
+            }
             message = connected.socket.recv() => {
                 let step = match message {
                     Some(Ok(Message::Binary(bytes))) => connected.session.on_binary(&bytes),
@@ -265,7 +284,7 @@ async fn identify(socket: &mut WebSocket, session: &mut Session, app: &App) -> O
             return None;
         }
     };
-    let claims = match app.verifier.verify(&token).await {
+    let claims = match verify(&app.verifier, &token).await {
         Ok(claims) => claims,
         Err(error) => {
             let (code, reason) = token_refusal(error);
@@ -286,6 +305,17 @@ async fn identify(socket: &mut WebSocket, session: &mut Session, app: &App) -> O
     }
 }
 
+/// The claims of `token` if it passes and its 4002 deadline (`exp` + [`REAUTH_GRACE`]) is
+/// still ahead. A token past the deadline but inside the leeway would be admitted only to
+/// be closed with 4002 at once, so it is `Expired` here.
+async fn verify(verifier: &Verifier, token: &str) -> Result<Claims, TokenError> {
+    let claims = verifier.verify(token).await?;
+    if unix_now() >= claims.expires_at + REAUTH_GRACE.as_secs() as i64 {
+        return Err(TokenError::Expired);
+    }
+    Ok(claims)
+}
+
 /// The close code and reason for a token that did not pass.
 fn token_refusal(error: TokenError) -> (u16, &'static str) {
     match error {
@@ -350,13 +380,53 @@ async fn admit(messaging: &mut MessagingClient<Channel>, claims: &Claims) -> Res
     }
 }
 
+/// When the connection's token expires, and what to do about it.
+#[derive(Debug, Clone, Copy)]
+struct Expiry {
+    /// The token's `exp`, in seconds since the Unix epoch.
+    expires_at: i64,
+    /// When to send `AuthExpiring`.
+    warn_at: Instant,
+    warned: bool,
+    /// When to close with 4002 unless a `ReAuth` came.
+    deadline: Instant,
+}
+
+impl Expiry {
+    fn new(expires_at: i64) -> Self {
+        let left = expires_at - unix_now();
+        let at = |offset: i64| {
+            Instant::now() + Duration::from_secs(u64::try_from(left + offset).unwrap_or(0))
+        };
+        Self {
+            expires_at,
+            warn_at: at(-(EXPIRY_WARNING.as_secs() as i64)),
+            warned: false,
+            deadline: at(REAUTH_GRACE.as_secs() as i64),
+        }
+    }
+
+    /// Seconds until the token expires; 0 once it has.
+    fn expires_in_s(&self) -> u32 {
+        u32::try_from((self.expires_at - unix_now()).max(0)).unwrap_or(u32::MAX)
+    }
+}
+
+fn unix_now() -> i64 {
+    SystemTime::now()
+        .duration_since(UNIX_EPOCH)
+        .map_or(0, |since| since.as_secs() as i64)
+}
+
 /// An identified connection.
 struct Connected {
     socket: WebSocket,
     session: Session,
     messaging: MessagingClient<Channel>,
+    verifier: Verifier,
     user: Uuid,
     device: Uuid,
+    expiry: Expiry,
     retry: RetryDelay,
     /// When to retry a failed sync; `None` while the last sync succeeded.
     retry_at: Option<Instant>,
@@ -367,6 +437,7 @@ impl Connected {
     async fn run(&mut self, step: Step) -> Result<(), Closed> {
         match step {
             Step::Reply(body) => send(&mut self.socket, body).await,
+            Step::ReAuth(token) => self.reauth(&token).await,
             Step::Send(frame) => {
                 let client_msg_id = frame.client_msg_id.clone();
                 let request = SendRequest {
@@ -413,6 +484,63 @@ impl Connected {
         }
     }
 
+    /// Tells the client its token expires soon, once per token.
+    async fn warn_expiry(&mut self) -> Result<(), Closed> {
+        self.expiry.warned = true;
+        let warning = AuthExpiring {
+            expires_in_s: self.expiry.expires_in_s(),
+        };
+        send(&mut self.socket, Body::AuthExpiring(warning)).await
+    }
+
+    /// A fresh token for the connection: it must pass, name the same user and device, and
+    /// be newer than the user's last password reset. Nothing answers a good one. When the
+    /// keys or message-svc cannot be reached, a retryable error asks the client to send it
+    /// again; the 4002 deadline still applies.
+    async fn reauth(&mut self, token: &str) -> Result<(), Closed> {
+        let claims = match verify(&self.verifier, token).await {
+            Ok(claims) => claims,
+            Err(TokenError::KeysUnavailable) => return self.retry_reauth().await,
+            Err(error) => {
+                let (code, reason) = token_refusal(error);
+                close_with(&mut self.socket, code, reason).await;
+                return Err(Closed);
+            }
+        };
+        if claims.user != self.user || claims.device != self.device {
+            close_with(
+                &mut self.socket,
+                close::INVALID_IDENTITY,
+                "the token names another device",
+            )
+            .await;
+            return Err(Closed);
+        }
+        match admit(&mut self.messaging, &claims).await {
+            Ok(()) => {
+                self.expiry = Expiry::new(claims.expires_at);
+                Ok(())
+            }
+            Err(Refusal::Unavailable) => self.retry_reauth().await,
+            Err(Refusal::Close(code, reason)) => {
+                close_with(&mut self.socket, code, reason).await;
+                Err(Closed)
+            }
+        }
+    }
+
+    async fn retry_reauth(&mut self) -> Result<(), Closed> {
+        let error = Error {
+            code: ErrorCode::Unavailable.into(),
+            message: "the token could not be checked now; send ReAuth again".to_owned(),
+            retryable: true,
+            client_msg_id: None,
+            request_id: None,
+            retry_after_s: Some(1),
+        };
+        send(&mut self.socket, Body::Error(error)).await
+    }
+
     /// Pushes every entry the device has not been sent yet. A failed sync is retried
     /// after [`RetryDelay`], from the connection's loop.
     async fn deliver(&mut self) -> Result<(), Closed> {
```

Change `crates/push-gateway/src/session.rs`:

```diff
diff --git a/crates/push-gateway/src/session.rs b/crates/push-gateway/src/session.rs
index a227af2..d80acc4 100644
--- a/crates/push-gateway/src/session.rs
+++ b/crates/push-gateway/src/session.rs
@@ -21,6 +21,9 @@ pub enum Step {
     /// Check this access token, ask message-svc whether the device it names may connect,
     /// then call [`Session::identified`] or close.
     Identify(String),
+    /// Check this access token, which must name the connection's user and device, and
+    /// ask message-svc again: the connection continues on the new token.
+    ReAuth(String),
     /// Forward a send to message-svc; answer with its `SendAck` or an error.
     Send(Send),
     /// Forward an ack to message-svc; answer only if it fails. If it moved the position,
@@ -95,7 +98,7 @@ impl Session {
             (Body::Identify(_), State::Ready { .. }) => {
                 reply_error(ErrorCode::BadFrame, "already identified")
             }
-
+            (Body::ReAuth(reauth), State::Ready { .. }) => Step::ReAuth(reauth.token),
             (Body::Send(send), State::Ready { .. }) => Step::Send(send),
             (Body::Ack(ack), State::Ready { .. }) => Step::Ack(ack),
             (Body::Ping(_), State::Ready { .. }) => Step::Reply(Body::Pong(Pong {})),
@@ -212,7 +215,7 @@ fn reply_error(code: ErrorCode, message: &str) -> Step {
 
 #[cfg(test)]
 mod tests {
-    use push_proto::v1::{Identify, Ping};
+    use push_proto::v1::{Identify, Ping, ReAuth};
     use push_proto::{MAX_FRAME_BYTES, encode_frame};
 
     use super::*;
@@ -324,6 +327,19 @@ mod tests {
         }
     }
 
+    #[test]
+    fn reauth_hands_over_its_token_once_ready_only() {
+        let reauth = encode_frame(Body::ReAuth(ReAuth {
+            token: "fresh".to_owned(),
+        }));
+        assert_eq!(
+            Session::new().on_binary(&reauth),
+            Step::Close(1002, "the first frame must be Identify")
+        );
+        let (mut session, _) = ready_session();
+        assert_eq!(session.on_binary(&reauth), Step::ReAuth("fresh".to_owned()));
+    }
+
     #[test]
     fn an_oversized_frame_closes_before_identify_and_errors_once_ready() {
         let oversized = vec![0u8; MAX_FRAME_BYTES + 1];
```

Change `crates/push-gateway/tests/gateway.rs`:

```diff
diff --git a/crates/push-gateway/tests/gateway.rs b/crates/push-gateway/tests/gateway.rs
index d6b4604..d71d411 100644
--- a/crates/push-gateway/tests/gateway.rs
+++ b/crates/push-gateway/tests/gateway.rs
@@ -17,7 +17,7 @@ use push_proto::internal::v1::{
     ReadUpToRequest, ReadUpToResponse, RequestCall, SendRequest, SyncRequest, SyncResponse,
 };
 use push_proto::v1::frame::Body;
-use push_proto::v1::{Ack, ContentType, Identify, Ping, Pong, Send, SendAck};
+use push_proto::v1::{Ack, AuthExpiring, ContentType, Identify, Ping, Pong, ReAuth, Send, SendAck};
 use push_proto::{decode_frame, encode_frame, id_bytes};
 use testcontainers_modules::redis::Redis;
 use testcontainers_modules::testcontainers::runners::AsyncRunner;
@@ -354,13 +354,23 @@ async fn an_invalid_token_is_closed_with_4001_and_an_expired_one_with_4002() {
     send(&mut socket, Body::Identify(Identify { token })).await;
     assert_eq!(close_code(&mut socket).await, 4002);
     // Expired 90 s ago: past the 60 s leeway. The client refreshes, then reconnects.
-    let mut socket = connect(&stack).await;
-    send(
-        &mut socket,
-        identify_issued(&stack, user, device, now() - 900 - 90),
-    )
-    .await;
-    assert_eq!(close_code(&mut socket).await, 4002);
+    // Expired 45 s ago: inside the leeway, but past the 30 s a connection has to send
+    // ReAuth, so refused at once rather than closed right after Ready.
+    for expired in [90, 45] {
+        let mut socket = connect(&stack).await;
+        send(
+            &mut socket,
+            identify_issued(&stack, user, device, now() - 900 - expired),
+        )
+        .await;
+        let first = tokio::time::timeout(Duration::from_secs(10), socket.next())
+            .await
+            .expect("a message within 10 s");
+        assert!(
+            matches!(&first, Some(Ok(Message::Close(Some(frame)))) if u16::from(frame.code) == 4002),
+            "{first:?}"
+        );
+    }
 }
 
 #[tokio::test]
@@ -428,6 +438,35 @@ async fn shutdown_closes_connections_with_4005() {
         .expect("a clean stop");
 }
 
+/// Spec §12 test 7: ReAuth keeps a connection whose token expires open; without it, 4002.
+#[tokio::test]
+async fn an_expiring_token_is_announced_and_renewed_by_reauth() {
+    let stack = stack().await;
+    let (user, device) = person(&stack).await;
+    // Expired 25 s ago, within the leeway: 5 s left to send ReAuth.
+    let issued_at = now() - 900 - 25;
+    let mut renewed = connect(&stack).await;
+    let mut lapsed = connect(&stack).await;
+    for socket in [&mut renewed, &mut lapsed] {
+        send(socket, identify_issued(&stack, user, device, issued_at)).await;
+        assert!(matches!(next(socket).await, Body::Ready(_)));
+        assert_eq!(
+            next(socket).await,
+            Body::AuthExpiring(AuthExpiring { expires_in_s: 0 })
+        );
+    }
+    let (token, _) = stack.issuer.issue(user, device, now());
+    send(&mut renewed, Body::ReAuth(ReAuth { token })).await;
+    assert_eq!(close_code(&mut lapsed).await, 4002);
+    send(&mut renewed, Body::Ping(Ping {})).await;
+    assert_eq!(next(&mut renewed).await, Body::Pong(Pong {}));
+
+    // A token of another device cannot renew the connection.
+    let (_, other_device) = person(&stack).await;
+    let (token, _) = stack.issuer.issue(user, other_device, now());
+    send(&mut renewed, Body::ReAuth(ReAuth { token })).await;
+    assert_eq!(close_code(&mut renewed).await, 4001);
+}
 /// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
 /// messages from Alice in it, sent through message-svc before anyone connects.
 async fn chat_with_backlog(stack: &Stack, count: u32) -> ((Uuid, Uuid), (Uuid, Uuid), Vec<u8>) {
```
- [ ] **Step 2: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "feat(gateway): ReAuth, AuthExpiring and the 4002 deadline"
```

Expected: 838 tests pass, no warnings, no diff.
---

### Task 7: Kicks, takeovers and server frames; the kick channel watched before Identify

**Files:**
- Modify: `crates/push-gateway/src/hub.rs`, `crates/push-gateway/src/lib.rs`, `crates/push-gateway/src/server.rs`, `crates/push-gateway/tests/gateway.rs`, `crates/push-gateway/tests/hub.rs`, `crates/push-message-svc/src/notify.rs`, `crates/push-proto/proto/push/internal/v1/messaging.proto`, `crates/push-smoke/src/kicks.rs`

Decision 6. The hub is rebuilt around channels: a connection watches `kick:{device_id}`
(waiting up to 2 s for the subscription) before message-svc is asked and joins
`notify:{user_id}` once admitted. Rings make every connection of the user sync; receipts,
read positions and contact events become server frames (at most 64 queued). The newcomer
publishes `Kick{TAKEN_OVER, connection_id}`, obeyed only by connections with a strictly
older UUIDv7 id; `DEVICE_REVOKED` closes every connection of the device with 4003;
`PASSWORD_RESET` closes with 4001 only connections whose token (after any `ReAuth`) was not
issued after the reset the kick carries. The hub tests prove that a stalled, older
`Identify` announcing itself late closes nobody newer and that a reset kick spares a
connection with a newer `iat`.

- [ ] **Step 1: Make the changes**

Change `crates/push-gateway/src/hub.rs`:

```diff
diff --git a/crates/push-gateway/src/hub.rs b/crates/push-gateway/src/hub.rs
index cc79382..315ab94 100644
--- a/crates/push-gateway/src/hub.rs
+++ b/crates/push-gateway/src/hub.rs
@@ -1,36 +1,64 @@
-//! Which connected devices to wake when Redis rings a user's channel.
+//! What Redis tells the connections of this gateway: which to sync, what to forward, and
+//! which to close.
 //!
-//! Every identified session joins the hub under its user. The first session of a user
-//! makes the subscriber subscribe to `notify:{user_id}`; the last one to leave makes it
-//! unsubscribe. A ring wakes every session of the user; rings coalesce, so a session that
-//! is busy syncing syncs once more afterwards, not once per ring.
+//! Every connection joins the hub under its device as soon as its token passed, before
+//! message-svc is asked about the device: the first connection of a device makes the
+//! subscriber subscribe to `kick:{device_id}`, so a revocation or password reset that
+//! message-svc commits after admitting the device cannot be missed. Once admitted, the
+//! connection joins its user too (`notify:{user_id}`). The last connection to leave a
+//! channel unsubscribes it.
+//!
+//! On `notify:{user_id}`, `EntriesAvailable` rings every connection of the user (rings
+//! coalesce: a busy connection syncs once more afterwards, not once per ring), and receipts,
+//! read positions and contact changes are forwarded to each as server frames. On
+//! `kick:{device_id}`, the connections of the device are told to close, with two
+//! exceptions:
+//!
+//! - a takeover closes only connections older than the newcomer it names. Connection ids
+//!   are UUIDv7s taken when a connection starts watching its device, so they sort by time:
+//!   an abandoned slow `Identify` that publishes its takeover after a newer connection did
+//!   closes nobody newer;
+//! - a password-reset kick spares a connection whose token was issued after the reset's
+//!   second (the time auth-svc reset the password): it signed in after the reset, and a
+//!   redelivered or late kick must not end it. A kick without a time closes them all.
 //!
 //! Redis is a doorbell, not the truth (spec §2): whenever the subscription is
-//! re-established after a break, every session is rung, since notifications may have been
-//! lost meanwhile.
+//! re-established after a break, every connection is rung, since notifications may have
+//! been lost meanwhile. A kick lost that way is caught when the connection renews its
+//! token (`ReAuth`, within 15 minutes): message-svc refuses a revoked device and a token
+//! older than a password reset.
 
-use std::collections::HashMap;
+use std::collections::{HashMap, HashSet};
 use std::sync::atomic::{AtomicBool, Ordering};
 use std::sync::{Arc, Mutex, MutexGuard};
 use std::time::Duration;
 
 use prost::Message;
-use push_proto::internal::v1::{Notification, notification::Kind};
-use push_proto::notify_channel;
+use push_proto::internal::v1::{
+    ContactChanged, Kick, KickReason, Notification, ReadPositionChanged, ReceiptChanged,
+    notification::Kind, receipt_changed,
+};
+use push_proto::v1::frame::Body;
+use push_proto::v1::{self, ContactEvent, GroupReceipts, ReadPosition, Receipt, receipt};
+use push_proto::{id, kick_channel, notify_channel};
 use tokio::sync::{mpsc, oneshot};
 use tokio::task::JoinHandle;
 use tokio_stream::StreamExt;
 use tokio_util::sync::CancellationToken;
 use uuid::Uuid;
 
-/// How long a joining session waits for its user's subscription to become active.
+/// How long a joining connection waits for its channel's subscription to become active.
 const SUBSCRIBE_WAIT: Duration = Duration::from_secs(2);
 /// How long a subscribe or unsubscribe may take before the connection is given up on.
 const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
 /// How long to wait before reconnecting to Redis.
 const RECONNECT_DELAY: Duration = Duration::from_secs(1);
+/// Server frames waiting for a busy connection; more are dropped (the client recovers
+/// them with `ListConversations` and `ListContacts`).
+const EVENT_BACKLOG: usize = 64;
 
-/// The sessions of this gateway, by user, and the Redis subscriber that wakes them.
+/// The connections of this gateway, by device and user, and the Redis subscriber that
+/// wakes them.
 #[derive(Clone)]
 pub struct Hub {
     members: Arc<Mutex<Members>>,
@@ -41,12 +69,36 @@ pub struct Hub {
 #[derive(Default)]
 struct Members {
     next_key: u64,
-    users: HashMap<Uuid, HashMap<u64, mpsc::Sender<()>>>,
+    by_key: HashMap<u64, Member>,
+    /// The members of each channel, `notify:{user}` or `kick:{device}`.
+    channels: HashMap<String, HashSet<u64>>,
+}
+
+struct Member {
+    /// Connection id: a takeover names the newcomer by it.
+    connection: Uuid,
+    /// The `iat` of the connection's current token, in seconds since the Unix epoch.
+    issued_at: i64,
+    doorbell: mpsc::Sender<()>,
+    events: mpsc::Sender<Body>,
+    kicks: mpsc::Sender<KickReason>,
 }
 
 enum Command {
-    Subscribe(Uuid, oneshot::Sender<()>),
-    Unsubscribe(Uuid),
+    Subscribe(String, oneshot::Sender<()>),
+    Unsubscribe(String),
+}
+
+/// What a connection must do.
+#[derive(Debug, Clone, PartialEq)]
+pub enum Signal {
+    /// New entries may be waiting: sync.
+    Ring,
+    /// Send this server frame.
+    Frame(Box<Body>),
+    /// Close: the device was revoked, the password reset, or another connection took
+    /// over.
+    Kick(KickReason),
 }
 
 impl Hub {
@@ -67,101 +119,262 @@ impl Hub {
         self.connected.load(Ordering::SeqCst)
     }
 
-    /// Adds a session of `user`. Waits up to 2 s for the user's subscription to become
-    /// active, then returns anyway: the session syncs after joining, which covers
-    /// whatever a late subscription misses.
-    pub async fn join(&self, user: Uuid) -> Membership {
+    /// Adds connection `connection` of `device`, whose token was issued at `issued_at`, to
+    /// be kicked through `kick:{device}`. Waits up to 2 s for the subscription to become
+    /// active, then returns anyway: without Redis nothing would be heard in time, and the
+    /// connection's next `ReAuth` catches what a kick would have said.
+    pub async fn watch_device(&self, device: Uuid, connection: Uuid, issued_at: i64) -> Membership {
         let (doorbell, rung) = mpsc::channel(1);
+        let (events, frames) = mpsc::channel(EVENT_BACKLOG);
+        let (kicks, kicked) = mpsc::channel(1);
+        let member = Member {
+            connection,
+            issued_at,
+            doorbell,
+            events,
+            kicks,
+        };
         let (key, subscribed) = {
             let mut members = lock(&self.members);
             let key = members.next_key;
             members.next_key += 1;
-            let sessions = members.users.entry(user).or_default();
-            let first = sessions.is_empty();
-            sessions.insert(key, doorbell);
-            // Sent under the lock, so subscribe and unsubscribe commands for a user reach
-            // the subscriber in the order the sessions joined and left.
-            let subscribed = first.then(|| {
-                let (done, subscribed) = oneshot::channel();
-                let _ = self.commands.send(Command::Subscribe(user, done));
-                subscribed
-            });
-            (key, subscribed)
+            members.by_key.insert(key, member);
+            (key, self.enter(&mut members, kick_channel(device), key))
         };
-        if let Some(subscribed) = subscribed {
-            let _ = tokio::time::timeout(SUBSCRIBE_WAIT, subscribed).await;
-        }
+        wait(subscribed).await;
         Membership {
-            user,
             key,
+            connection,
+            issued_at,
+            channels: vec![kick_channel(device)],
             rung,
+            frames,
+            kicked,
             hub: self.clone(),
         }
     }
 
-    /// Wakes every session of `user`.
-    fn ring(&self, user: Uuid) {
-        if let Some(sessions) = lock(&self.members).users.get(&user) {
-            for doorbell in sessions.values() {
-                // A full doorbell has already been rung and not answered yet.
-                let _ = doorbell.try_send(());
+    /// Adds `key` to `channel`, asking the subscriber to subscribe if it is the first.
+    /// Sent under the lock, so the commands for a channel reach the subscriber in the
+    /// order its members joined and left.
+    fn enter(
+        &self,
+        members: &mut Members,
+        channel: String,
+        key: u64,
+    ) -> Option<oneshot::Receiver<()>> {
+        let keys = members.channels.entry(channel.clone()).or_default();
+        let first = keys.is_empty();
+        keys.insert(key);
+        first.then(|| {
+            let (done, subscribed) = oneshot::channel();
+            let _ = self.commands.send(Command::Subscribe(channel, done));
+            subscribed
+        })
+    }
+
+    /// The members of `channel`.
+    fn members_of<'a>(members: &'a Members, channel: &str) -> impl Iterator<Item = &'a Member> {
+        members
+            .channels
+            .get(channel)
+            .into_iter()
+            .flatten()
+            .filter_map(|key| members.by_key.get(key))
+    }
+
+    /// Wakes every connection on `channel`.
+    fn ring(&self, channel: &str) {
+        let members = lock(&self.members);
+        for member in Self::members_of(&members, channel) {
+            // A full doorbell has already been rung and not answered yet.
+            let _ = member.doorbell.try_send(());
+        }
+    }
+
+    /// Hands a server frame to every connection on `channel`.
+    fn forward(&self, channel: &str, frame: &Body) {
+        let members = lock(&self.members);
+        for member in Self::members_of(&members, channel) {
+            if member.events.try_send(frame.clone()).is_err() {
+                tracing::warn!("a connection is not keeping up; a server frame was dropped");
             }
         }
     }
 
-    /// Wakes every session.
-    fn ring_all(&self) {
-        for sessions in lock(&self.members).users.values() {
-            for doorbell in sessions.values() {
-                let _ = doorbell.try_send(());
+    /// Tells the connections on `channel` to close: for a takeover only those older than
+    /// the newcomer, for a password reset only those whose token is not newer than it.
+    fn kick(&self, channel: &str, kick: &Kick) {
+        let members = lock(&self.members);
+        let newcomer = id(&kick.connection_id).ok();
+        for member in Self::members_of(&members, channel) {
+            let closes = match kick.reason() {
+                KickReason::TakenOver => newcomer.is_some_and(|newer| member.connection < newer),
+                KickReason::PasswordReset => {
+                    kick.reset_at == 0 || member.issued_at <= kick.reset_at
+                }
+                _ => true,
+            };
+            if closes {
+                let _ = member.kicks.try_send(kick.reason());
             }
         }
     }
 
-    /// The users with at least one session.
-    fn users(&self) -> Vec<Uuid> {
-        lock(&self.members).users.keys().copied().collect()
+    /// Wakes every connection.
+    fn ring_all(&self) {
+        for member in lock(&self.members).by_key.values() {
+            let _ = member.doorbell.try_send(());
+        }
+    }
+
+    /// The channels with at least one member.
+    fn channels(&self) -> Vec<String> {
+        lock(&self.members).channels.keys().cloned().collect()
     }
 
-    /// Acts on a message from Redis: `EntriesAvailable` on `notify:{user_id}` rings the
-    /// user's sessions. Other notifications reach clients in a later plan (receipts,
-    /// contact changes, read positions); malformed ones are logged and dropped.
+    /// Acts on a message from Redis. Malformed ones are logged and dropped.
     fn dispatch(&self, channel: &str, payload: &[u8]) {
-        let Some(user) = channel
-            .strip_prefix("notify:")
-            .and_then(|id| Uuid::parse_str(id).ok())
-        else {
+        if let Some(rest) = channel.strip_prefix("notify:") {
+            if Uuid::parse_str(rest).is_err() {
+                tracing::warn!(channel, "message on an unexpected channel");
+                return;
+            }
+            match Notification::decode(payload) {
+                Ok(Notification {
+                    kind: Some(Kind::EntriesAvailable(_)),
+                }) => self.ring(channel),
+                Ok(Notification { kind: Some(kind) }) => {
+                    if let Some(frame) = server_frame(kind) {
+                        self.forward(channel, &frame);
+                    }
+                }
+                Ok(Notification { kind: None }) => {}
+                Err(error) => tracing::warn!(%error, channel, "malformed notification"),
+            }
+        } else if let Some(rest) = channel.strip_prefix("kick:") {
+            if Uuid::parse_str(rest).is_err() {
+                tracing::warn!(channel, "message on an unexpected channel");
+                return;
+            }
+            match Kick::decode(payload) {
+                Ok(kick) if kick.reason() != KickReason::Unspecified => self.kick(channel, &kick),
+                Ok(_) => tracing::warn!(channel, "a kick without a reason"),
+                Err(error) => tracing::warn!(%error, channel, "malformed kick"),
+            }
+        } else {
             tracing::warn!(channel, "message on an unexpected channel");
-            return;
-        };
-        match Notification::decode(payload) {
-            Ok(Notification {
-                kind: Some(Kind::EntriesAvailable(_)),
-            }) => self.ring(user),
-            Ok(_) => {}
-            Err(error) => tracing::warn!(%error, channel, "malformed notification"),
         }
     }
 }
 
+/// The server frame a notification becomes, if any.
+fn server_frame(kind: Kind) -> Option<Body> {
+    Some(match kind {
+        Kind::EntriesAvailable(_) => return None,
+        Kind::ReceiptChanged(ReceiptChanged {
+            conversation_id,
+            positions,
+        }) => {
+            let positions = match positions? {
+                receipt_changed::Positions::Direct(direct) => {
+                    receipt::Positions::Direct(v1::DirectReceipt {
+                        user_id: direct.user_id,
+                        delivered_seq: direct.delivered_seq,
+                        read_seq: direct.read_seq,
+                    })
+                }
+                receipt_changed::Positions::Group(group) => {
+                    receipt::Positions::Group(GroupReceipts {
+                        all_delivered_seq: group.all_delivered_seq,
+                        all_read_seq: group.all_read_seq,
+                    })
+                }
+            };
+            Body::Receipt(Receipt {
+                conversation_id,
+                positions: Some(positions),
+            })
+        }
+        Kind::ContactChanged(ContactChanged { peer_id, state }) => {
+            Body::ContactEvent(ContactEvent {
+                user_id: peer_id,
+                state,
+            })
+        }
+        Kind::ReadPositionChanged(ReadPositionChanged {
+            conversation_id,
+            read_seq,
+        }) => Body::ReadPosition(ReadPosition {
+            conversation_id,
+            read_seq,
+        }),
+    })
+}
+
 fn lock(members: &Mutex<Members>) -> MutexGuard<'_, Members> {
     members.lock().expect("hub lock poisoned")
 }
 
-/// One session's place in the hub. Dropping it leaves.
+/// Waits up to [`SUBSCRIBE_WAIT`] for a subscription to become active.
+async fn wait(subscribed: Option<oneshot::Receiver<()>>) {
+    if let Some(subscribed) = subscribed {
+        let _ = tokio::time::timeout(SUBSCRIBE_WAIT, subscribed).await;
+    }
+}
+
+/// One connection's place in the hub. Dropping it leaves every channel it joined.
 pub struct Membership {
-    user: Uuid,
     key: u64,
+    connection: Uuid,
+    issued_at: i64,
+    channels: Vec<String>,
     rung: mpsc::Receiver<()>,
+    frames: mpsc::Receiver<Body>,
+    kicked: mpsc::Receiver<KickReason>,
     hub: Hub,
 }
 
 impl Membership {
-    /// Waits until the session's user is rung.
-    pub async fn rung(&mut self) {
-        if self.rung.recv().await.is_none() {
-            // The hub keeps the sender while this membership exists; never reached.
-            std::future::pending::<()>().await;
+    /// The connection's id, which its takeover names.
+    pub fn connection(&self) -> Uuid {
+        self.connection
+    }
+
+    /// The connection renewed its token (`ReAuth`) with one issued at `issued_at`: a
+    /// password-reset kick compares that one.
+    pub fn renewed(&mut self, issued_at: i64) {
+        if issued_at == self.issued_at {
+            return;
+        }
+        self.issued_at = issued_at;
+        if let Some(member) = lock(&self.hub.members).by_key.get_mut(&self.key) {
+            member.issued_at = issued_at;
+        }
+    }
+
+    /// Joins the connection's user, `notify:{user}`, once message-svc admitted the device.
+    /// Waits up to 2 s for the subscription; the connection syncs after joining, which
+    /// covers whatever a late subscription misses.
+    pub async fn join_user(&mut self, user: Uuid) {
+        let channel = notify_channel(user);
+        let subscribed = {
+            let mut members = lock(&self.hub.members);
+            self.hub.enter(&mut members, channel.clone(), self.key)
+        };
+        self.channels.push(channel);
+        wait(subscribed).await;
+    }
+
+    /// The next thing to do: a kick first, then a server frame, then a ring.
+    pub async fn next(&mut self) -> Signal {
+        tokio::select! {
+            biased;
+            Some(reason) = self.kicked.recv() => Signal::Kick(reason),
+            Some(frame) = self.frames.recv() => Signal::Frame(Box::new(frame)),
+            Some(()) = self.rung.recv() => Signal::Ring,
+            // The hub keeps the senders while this membership exists; never reached.
+            else => std::future::pending().await,
         }
     }
 }
@@ -169,11 +382,17 @@ impl Membership {
 impl Drop for Membership {
     fn drop(&mut self) {
         let mut members = lock(&self.hub.members);
-        if let Some(sessions) = members.users.get_mut(&self.user) {
-            sessions.remove(&self.key);
-            if sessions.is_empty() {
-                members.users.remove(&self.user);
-                let _ = self.hub.commands.send(Command::Unsubscribe(self.user));
+        members.by_key.remove(&self.key);
+        for channel in &self.channels {
+            if let Some(keys) = members.channels.get_mut(channel) {
+                keys.remove(&self.key);
+                if keys.is_empty() {
+                    members.channels.remove(channel);
+                    let _ = self
+                        .hub
+                        .commands
+                        .send(Command::Unsubscribe(channel.clone()));
+                }
             }
         }
     }
@@ -199,9 +418,9 @@ async fn bounded(
 
 /// The subscriber task: keeps one Redis pub/sub connection, follows the hub's subscribe
 /// and unsubscribe commands, and dispatches what arrives. Reconnects after a break,
-/// subscribes again to every user with a session and rings them all. Connecting is
-/// retried while Redis is unavailable, and `shutdown` interrupts both the connection
-/// attempt and the wait between attempts at once.
+/// subscribes again to every channel with a member and rings every connection.
+/// Connecting is retried while Redis is unavailable, and `shutdown` interrupts both the
+/// connection attempt and the wait between attempts at once.
 async fn subscribe(
     client: redis::Client,
     hub: Hub,
@@ -217,8 +436,8 @@ async fn subscribe(
             Ok(pubsub) => {
                 let (mut sink, mut messages) = pubsub.split();
                 let mut resubscribed = true;
-                for user in hub.users() {
-                    match bounded(&shutdown, sink.subscribe(notify_channel(user))).await {
+                for channel in hub.channels() {
+                    match bounded(&shutdown, sink.subscribe(&channel)).await {
                         None => return,
                         Some(true) => {}
                         Some(false) => {
@@ -237,21 +456,21 @@ async fn subscribe(
                             command = commands.recv() => {
                                 let succeeded = match command {
                                     None => return,
-                                    Some(Command::Subscribe(user, done)) => {
+                                    Some(Command::Subscribe(channel, done)) => {
                                         let Some(succeeded) =
-                                            bounded(&shutdown, sink.subscribe(notify_channel(user))).await
+                                            bounded(&shutdown, sink.subscribe(&channel)).await
                                         else {
                                             return;
                                         };
                                         // Sent even when the subscribe failed: the reconnect path
-                                        // subscribes every user again and calls `ring_all`, so the
-                                        // joining session does not need to wait any longer.
+                                        // subscribes every channel again and calls `ring_all`, so
+                                        // the joining connection does not need to wait any longer.
                                         let _ = done.send(());
                                         succeeded
                                     }
-                                    Some(Command::Unsubscribe(user)) => {
+                                    Some(Command::Unsubscribe(channel)) => {
                                         let Some(succeeded) =
-                                            bounded(&shutdown, sink.unsubscribe(notify_channel(user))).await
+                                            bounded(&shutdown, sink.unsubscribe(&channel)).await
                                         else {
                                             return;
                                         };
@@ -285,7 +504,8 @@ async fn subscribe(
 
 #[cfg(test)]
 mod tests {
-    use push_proto::internal::v1::{EntriesAvailable, ReadPositionChanged};
+    use push_proto::id_bytes;
+    use push_proto::internal::v1::{DirectReceipt, EntriesAvailable};
 
     use super::*;
 
@@ -307,71 +527,258 @@ mod tests {
         .encode_to_vec()
     }
 
-    fn rang(membership: &mut Membership) -> bool {
-        membership.rung.try_recv().is_ok()
+    /// A connection of `user` on a new device.
+    async fn connection(hub: &Hub, user: Uuid) -> (Membership, Uuid, Uuid) {
+        let (device, id) = (Uuid::now_v7(), Uuid::now_v7());
+        let mut membership = hub.watch_device(device, id, 0).await;
+        membership.join_user(user).await;
+        (membership, device, id)
+    }
+
+    fn pending(membership: &mut Membership) -> Option<Signal> {
+        if let Ok(reason) = membership.kicked.try_recv() {
+            return Some(Signal::Kick(reason));
+        }
+        if let Ok(frame) = membership.frames.try_recv() {
+            return Some(Signal::Frame(Box::new(frame)));
+        }
+        membership.rung.try_recv().ok().map(|()| Signal::Ring)
+    }
+
+    fn subscribed(commands: &mut mpsc::UnboundedReceiver<Command>) -> Vec<String> {
+        let mut channels = Vec::new();
+        while let Ok(command) = commands.try_recv() {
+            match command {
+                Command::Subscribe(channel, _) => channels.push(format!("+{channel}")),
+                Command::Unsubscribe(channel) => channels.push(format!("-{channel}")),
+            }
+        }
+        channels
     }
 
     #[tokio::test(start_paused = true)]
-    async fn the_first_session_subscribes_and_the_last_unsubscribes() {
+    async fn the_device_is_watched_before_the_user_and_the_last_to_leave_unsubscribes() {
         let (hub, mut commands) = hub();
         let user = Uuid::now_v7();
-        let first = hub.join(user).await;
-        assert!(matches!(commands.try_recv(), Ok(Command::Subscribe(u, _)) if u == user));
-        let second = hub.join(user).await;
-        assert!(commands.try_recv().is_err(), "already subscribed");
+        let (first, device, _) = connection(&hub, user).await;
+        assert_eq!(
+            subscribed(&mut commands),
+            [
+                format!("+{}", kick_channel(device)),
+                format!("+{}", notify_channel(user))
+            ]
+        );
+        let (second, other_device, _) = connection(&hub, user).await;
+        assert_eq!(
+            subscribed(&mut commands),
+            [format!("+{}", kick_channel(other_device))],
+            "the user is already subscribed"
+        );
         drop(first);
-        assert!(commands.try_recv().is_err(), "a session is still there");
+        assert_eq!(
+            subscribed(&mut commands),
+            [format!("-{}", kick_channel(device))]
+        );
         drop(second);
-        assert!(matches!(commands.try_recv(), Ok(Command::Unsubscribe(u)) if u == user));
-        assert!(hub.users().is_empty());
+        let mut left = subscribed(&mut commands);
+        left.sort();
+        let mut expected = vec![
+            format!("-{}", kick_channel(other_device)),
+            format!("-{}", notify_channel(user)),
+        ];
+        expected.sort();
+        assert_eq!(left, expected);
+        assert!(hub.channels().is_empty());
     }
 
     #[tokio::test(start_paused = true)]
-    async fn entries_available_rings_only_that_users_sessions() {
+    async fn entries_available_rings_only_that_users_connections() {
         let (hub, _commands) = hub();
         let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
-        let (mut alice_phone, mut alice_laptop) = (hub.join(alice).await, hub.join(alice).await);
-        let mut bob_phone = hub.join(bob).await;
+        let (mut alice_phone, _, _) = connection(&hub, alice).await;
+        let (mut alice_laptop, _, _) = connection(&hub, alice).await;
+        let (mut bob_phone, _, _) = connection(&hub, bob).await;
         hub.dispatch(&notify_channel(alice), &entries_available());
-        assert!(rang(&mut alice_phone));
-        assert!(rang(&mut alice_laptop));
-        assert!(!rang(&mut bob_phone));
+        assert_eq!(pending(&mut alice_phone), Some(Signal::Ring));
+        assert_eq!(pending(&mut alice_laptop), Some(Signal::Ring));
+        assert_eq!(pending(&mut bob_phone), None);
     }
 
     #[tokio::test(start_paused = true)]
     async fn rings_coalesce_until_answered() {
         let (hub, _commands) = hub();
         let user = Uuid::now_v7();
-        let mut session = hub.join(user).await;
+        let (mut session, _, _) = connection(&hub, user).await;
         for _ in 0..3 {
             hub.dispatch(&notify_channel(user), &entries_available());
         }
-        assert!(rang(&mut session));
-        assert!(!rang(&mut session));
+        assert_eq!(pending(&mut session), Some(Signal::Ring));
+        assert_eq!(pending(&mut session), None);
     }
 
     #[tokio::test(start_paused = true)]
-    async fn other_notifications_and_strange_channels_ring_nobody() {
+    async fn receipts_read_positions_and_contact_changes_become_server_frames() {
+        let (hub, _commands) = hub();
+        let (user, peer) = (Uuid::now_v7(), Uuid::now_v7());
+        let conversation = id_bytes(Uuid::now_v7());
+        let (mut session, _, _) = connection(&hub, user).await;
+        let notifications = [
+            Kind::ReceiptChanged(ReceiptChanged {
+                conversation_id: conversation.clone(),
+                positions: Some(receipt_changed::Positions::Direct(DirectReceipt {
+                    user_id: id_bytes(peer),
+                    delivered_seq: 4,
+                    read_seq: 3,
+                })),
+            }),
+            Kind::ReadPositionChanged(ReadPositionChanged {
+                conversation_id: conversation.clone(),
+                read_seq: 9,
+            }),
+            Kind::ContactChanged(ContactChanged {
+                peer_id: id_bytes(peer),
+                state: v1::ContactState::Accepted.into(),
+            }),
+        ];
+        for kind in notifications {
+            let payload = Notification { kind: Some(kind) }.encode_to_vec();
+            hub.dispatch(&notify_channel(user), &payload);
+        }
+        let frames = [
+            Body::Receipt(Receipt {
+                conversation_id: conversation.clone(),
+                positions: Some(receipt::Positions::Direct(v1::DirectReceipt {
+                    user_id: id_bytes(peer),
+                    delivered_seq: 4,
+                    read_seq: 3,
+                })),
+            }),
+            Body::ReadPosition(ReadPosition {
+                conversation_id: conversation,
+                read_seq: 9,
+            }),
+            Body::ContactEvent(ContactEvent {
+                user_id: id_bytes(peer),
+                state: v1::ContactState::Accepted.into(),
+            }),
+        ];
+        for frame in frames {
+            assert_eq!(pending(&mut session), Some(Signal::Frame(Box::new(frame))));
+        }
+        assert_eq!(pending(&mut session), None);
+    }
+
+    fn takeover(newcomer: Uuid) -> Vec<u8> {
+        Kick {
+            reason: KickReason::TakenOver.into(),
+            connection_id: id_bytes(newcomer),
+            ..Kick::default()
+        }
+        .encode_to_vec()
+    }
+
+    #[tokio::test(start_paused = true)]
+    async fn a_kick_closes_the_devices_connections_and_a_takeover_only_older_ones() {
         let (hub, _commands) = hub();
         let user = Uuid::now_v7();
-        let mut session = hub.join(user).await;
-        let read = Notification {
-            kind: Some(Kind::ReadPositionChanged(ReadPositionChanged::default())),
+        let (mut old, device, _) = connection(&hub, user).await;
+        // An Identify that stalls: its id is older than the newcomer's.
+        let stalled = Uuid::now_v7();
+        let newcomer = Uuid::now_v7();
+        let mut new = hub.watch_device(device, newcomer, 0).await;
+        let (mut other, _, _) = connection(&hub, user).await;
+        hub.dispatch(&kick_channel(device), &takeover(newcomer));
+        assert_eq!(pending(&mut old), Some(Signal::Kick(KickReason::TakenOver)));
+        assert_eq!(pending(&mut new), None);
+        assert_eq!(pending(&mut other), None);
+
+        // The stalled one passes at last and announces itself: it closes nobody newer.
+        let mut late = hub.watch_device(device, stalled, 0).await;
+        hub.dispatch(&kick_channel(device), &takeover(stalled));
+        assert_eq!(pending(&mut new), None);
+        assert_eq!(pending(&mut late), None);
+        // A takeover that names no connection closes nobody.
+        let anonymous = Kick {
+            reason: KickReason::TakenOver.into(),
+            ..Kick::default()
+        };
+        hub.dispatch(&kick_channel(device), &anonymous.encode_to_vec());
+        assert_eq!(pending(&mut new), None);
+
+        let revoked = Kick {
+            reason: KickReason::DeviceRevoked.into(),
+            ..Kick::default()
+        };
+        hub.dispatch(&kick_channel(device), &revoked.encode_to_vec());
+        assert_eq!(
+            pending(&mut new),
+            Some(Signal::Kick(KickReason::DeviceRevoked))
+        );
+        assert_eq!(
+            pending(&mut late),
+            Some(Signal::Kick(KickReason::DeviceRevoked))
+        );
+        assert_eq!(pending(&mut other), None);
+    }
+
+    #[tokio::test(start_paused = true)]
+    async fn a_password_reset_kick_spares_connections_signed_in_after_it() {
+        let (hub, _commands) = hub();
+        let device = Uuid::now_v7();
+        let mut before = hub.watch_device(device, Uuid::now_v7(), 1_000).await;
+        let mut after = hub.watch_device(device, Uuid::now_v7(), 1_200).await;
+        let reset = |reset_at| {
+            Kick {
+                reason: KickReason::PasswordReset.into(),
+                reset_at,
+                ..Kick::default()
+            }
+            .encode_to_vec()
         };
-        hub.dispatch(&notify_channel(user), &read.encode_to_vec());
+        // Issued in the reset's second: the order is unknown, so it closes.
+        hub.dispatch(&kick_channel(device), &reset(1_000));
+        assert_eq!(
+            pending(&mut before),
+            Some(Signal::Kick(KickReason::PasswordReset))
+        );
+        assert_eq!(pending(&mut after), None);
+
+        // A connection that renewed its token after a reset is spared by that reset too.
+        before.renewed(1_300);
+        hub.dispatch(&kick_channel(device), &reset(1_250));
+        assert_eq!(pending(&mut before), None);
+        assert_eq!(
+            pending(&mut after),
+            Some(Signal::Kick(KickReason::PasswordReset))
+        );
+        // A kick without a time closes every connection.
+        hub.dispatch(&kick_channel(device), &reset(0));
+        assert_eq!(
+            pending(&mut before),
+            Some(Signal::Kick(KickReason::PasswordReset))
+        );
+    }
+
+    #[tokio::test(start_paused = true)]
+    async fn strange_messages_ring_and_kick_nobody() {
+        let (hub, _commands) = hub();
+        let user = Uuid::now_v7();
+        let (mut session, device, _) = connection(&hub, user).await;
         hub.dispatch("notify:not-a-uuid", &entries_available());
         hub.dispatch(&format!("other:{user}"), &entries_available());
         hub.dispatch(&notify_channel(user), &[0xff, 0xff]);
-        assert!(!rang(&mut session));
+        hub.dispatch(&kick_channel(device), &[0xff, 0xff]);
+        hub.dispatch(&kick_channel(device), &Kick::default().encode_to_vec());
+        assert_eq!(pending(&mut session), None);
     }
 
     #[tokio::test(start_paused = true)]
-    async fn ring_all_wakes_every_session() {
+    async fn ring_all_wakes_every_connection() {
         let (hub, _commands) = hub();
-        let mut a = hub.join(Uuid::now_v7()).await;
-        let mut b = hub.join(Uuid::now_v7()).await;
+        let (mut a, _, _) = connection(&hub, Uuid::now_v7()).await;
+        let (mut b, _, _) = connection(&hub, Uuid::now_v7()).await;
         hub.ring_all();
-        assert!(rang(&mut a));
-        assert!(rang(&mut b));
+        assert_eq!(pending(&mut a), Some(Signal::Ring));
+        assert_eq!(pending(&mut b), Some(Signal::Ring));
     }
 }
```

Change `crates/push-gateway/src/lib.rs`:

```diff
diff --git a/crates/push-gateway/src/lib.rs b/crates/push-gateway/src/lib.rs
index 2f7818a..f7ca353 100644
--- a/crates/push-gateway/src/lib.rs
+++ b/crates/push-gateway/src/lib.rs
@@ -9,7 +9,7 @@ mod session;
 mod tokens;
 
 pub use config::{Config, ConfigError};
-pub use hub::{Hub, Membership};
+pub use hub::{Hub, Membership, Signal};
 pub use server::{init_logging, serve, terminated};
 pub use session::{RetryDelay, Session, Step};
 pub use tokens::{Claims, LEEWAY, REFETCH_INTERVAL, TokenError, Verifier};
```

Change `crates/push-gateway/src/server.rs`:

```diff
diff --git a/crates/push-gateway/src/server.rs b/crates/push-gateway/src/server.rs
index 9440597..60784e9 100644
--- a/crates/push-gateway/src/server.rs
+++ b/crates/push-gateway/src/server.rs
@@ -10,11 +10,15 @@ use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
 use axum::http::StatusCode;
 use axum::response::Response;
 use axum::routing::get;
+use prost::Message as _;
 use push_proto::internal::v1::messaging_client::MessagingClient;
-use push_proto::internal::v1::{AckRequest, IdentifyRequest, SendRequest, SyncRequest};
+use push_proto::internal::v1::{
+    AckRequest, IdentifyRequest, Kick, KickReason, SendRequest, SyncRequest,
+};
 use push_proto::v1::frame::Body;
 use push_proto::v1::{AuthExpiring, Error, ErrorCode};
-use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes};
+use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes, kick_channel};
+use redis::aio::{ConnectionManager, ConnectionManagerConfig};
 use tokio::net::TcpListener;
 use tokio::time::Instant;
 use tokio_util::sync::CancellationToken;
@@ -23,7 +27,7 @@ use tonic::transport::{Channel, Endpoint};
 use uuid::Uuid;
 
 use crate::config::Config;
-use crate::hub::Hub;
+use crate::hub::{Hub, Membership, Signal};
 use crate::session::{RetryDelay, Session, Step, error_frame};
 use crate::tokens::{Claims, TokenError, Verifier};
 
@@ -40,6 +44,8 @@ const REAUTH_GRACE: Duration = Duration::from_secs(30);
 const KEYS_REFRESH: Duration = Duration::from_secs(300);
 /// How soon to fetch the keys again after a failed fetch.
 const KEYS_RETRY: Duration = Duration::from_secs(2);
+/// How long a Redis command (other than the subscriber's) may take.
+const REDIS_TIMEOUT: Duration = Duration::from_millis(500);
 
 #[derive(Clone)]
 struct App {
@@ -47,6 +53,8 @@ struct App {
     messaging: MessagingClient<Channel>,
     verifier: Verifier,
     hub: Hub,
+    /// Publishes takeovers on `kick:{device_id}`.
+    redis: ConnectionManager,
     sessions: TaskTracker,
     shutdown: CancellationToken,
 }
@@ -69,6 +77,13 @@ pub async fn serve(
     // The error is not passed on: the URL carries the Redis password.
     let redis = redis::Client::open(config.redis_url.as_str())
         .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
+    let commands = ConnectionManagerConfig::new()
+        .set_connection_timeout(Some(REDIS_TIMEOUT))
+        .set_response_timeout(Some(REDIS_TIMEOUT))
+        .set_number_of_retries(1);
+    let publisher = redis
+        .get_connection_manager_lazy(commands)
+        .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
     let (hub, subscriber) = Hub::start(redis, shutdown.clone());
     let verifier = Verifier::new(config.jwks_url(), &config.token_audience);
     let keys = tokio::spawn(refresh_keys(verifier.clone(), shutdown.clone()));
@@ -79,6 +94,7 @@ pub async fn serve(
         messaging,
         verifier: verifier.clone(),
         hub: hub.clone(),
+        redis: publisher,
         sessions: sessions.clone(),
         shutdown: shutdown.clone(),
     };
@@ -171,13 +187,14 @@ async fn upgrade(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response
 /// One client connection, from `Identify` to close.
 async fn connection(mut socket: WebSocket, app: App) {
     let mut session = Session::new();
-    let Some(claims) = identify(&mut socket, &mut session, &app).await else {
+    let Some((claims, mut membership)) = identify(&mut socket, &mut session, &app).await else {
         return;
     };
-    let mut membership = app.hub.join(claims.user).await;
+    membership.join_user(claims.user).await;
     let Some(ready) = session.identified(&claims, app.config.heartbeat) else {
         return;
     };
+    take_over(&app, claims.device, membership.connection()).await;
     let mut connected = Connected {
         socket,
         session,
@@ -186,6 +203,7 @@ async fn connection(mut socket: WebSocket, app: App) {
         user: claims.user,
         device: claims.device,
         expiry: Expiry::new(claims.expires_at),
+        issued_at: claims.issued_at,
         retry: RetryDelay::default(),
         retry_at: None,
     };
@@ -204,8 +222,18 @@ async fn connection(mut socket: WebSocket, app: App) {
                 close_with(&mut connected.socket, close::SHUTTING_DOWN, "server shutting down").await;
                 return;
             }
-            () = membership.rung() => {
-                if connected.deliver().await.is_err() {
+            signal = membership.next() => {
+                let done = match signal {
+                    Signal::Ring => connected.deliver().await,
+                    Signal::Frame(frame) => send(&mut connected.socket, *frame).await,
+                    Signal::Kick(reason) => {
+                        let (code, why) = kick_close(reason);
+                        tracing::info!(device = %connected.device, ?reason, "connection kicked");
+                        close_with(&mut connected.socket, code, why).await;
+                        Err(Closed)
+                    }
+                };
+                if done.is_err() {
                     return;
                 }
             }
@@ -235,6 +263,7 @@ async fn connection(mut socket: WebSocket, app: App) {
                 if connected.run(step).await.is_err() {
                     return;
                 }
+                membership.renewed(connected.issued_at);
             }
         }
     }
@@ -245,8 +274,14 @@ fn far_future() -> Instant {
 }
 
 /// Waits for the first frame, checks its token and asks message-svc whether the device it
-/// names may connect. `None` once the connection is closed.
-async fn identify(socket: &mut WebSocket, session: &mut Session, app: &App) -> Option<Claims> {
+/// names may connect. The connection watches the device's kicks before message-svc
+/// answers, so a revocation or password reset committed after the answer still reaches
+/// it. `None` once the connection is closed.
+async fn identify(
+    socket: &mut WebSocket,
+    session: &mut Session,
+    app: &App,
+) -> Option<(Claims, Membership)> {
     let deadline = tokio::time::sleep(IDENTIFY_TIMEOUT);
     tokio::pin!(deadline);
     let step = loop {
@@ -292,10 +327,17 @@ async fn identify(socket: &mut WebSocket, session: &mut Session, app: &App) -> O
             return None;
         }
     };
+    // Taken now, not when the socket opened: a connection that starts watching later is the
+    // newer one, so its takeover wins even if it opened first and its token check stalled.
+    let connection_id = Uuid::now_v7();
+    let membership = app
+        .hub
+        .watch_device(claims.device, connection_id, claims.issued_at)
+        .await;
     match admit(&mut app.messaging.clone(), &claims).await {
         Ok(()) => {
             tracing::info!(user = %claims.user, device = %claims.device, "device connected");
-            Some(claims)
+            Some((claims, membership))
         }
         Err(refusal) => {
             let (code, reason) = refusal.close();
@@ -316,6 +358,44 @@ async fn verify(verifier: &Verifier, token: &str) -> Result<Claims, TokenError>
     Ok(claims)
 }
 
+/// Tells any other connection of the device, on any gateway, to close: the newest
+/// connection wins (spec §7.3). Best effort: without Redis two connections may coexist
+/// for a while, which the client tolerates.
+async fn take_over(app: &App, device: Uuid, connection_id: Uuid) {
+    let kick = Kick {
+        reason: KickReason::TakenOver.into(),
+        connection_id: id_bytes(connection_id),
+        ..Kick::default()
+    };
+    let mut redis = app.redis.clone();
+    let mut command = redis::cmd("PUBLISH");
+    command.arg(kick_channel(device)).arg(kick.encode_to_vec());
+    let publish = command.exec_async(&mut redis);
+    tokio::select! {
+        () = app.shutdown.cancelled() => {}
+        published = tokio::time::timeout(REDIS_TIMEOUT, publish) => {
+            if !matches!(published, Ok(Ok(()))) {
+                tracing::warn!(%device, "could not announce the connection to other gateways");
+            }
+        }
+    }
+}
+
+/// The close code and reason for a kick.
+fn kick_close(reason: KickReason) -> (u16, &'static str) {
+    match reason {
+        KickReason::PasswordReset => (
+            close::INVALID_IDENTITY,
+            "the password was changed; sign in again",
+        ),
+        KickReason::DeviceRevoked => (close::TAKEN_OVER, "the device was revoked"),
+        KickReason::TakenOver | KickReason::Unspecified => (
+            close::TAKEN_OVER,
+            "another connection of this device took over",
+        ),
+    }
+}
+
 /// The close code and reason for a token that did not pass.
 fn token_refusal(error: TokenError) -> (u16, &'static str) {
     match error {
@@ -427,6 +507,8 @@ struct Connected {
     user: Uuid,
     device: Uuid,
     expiry: Expiry,
+    /// The `iat` of the current token: a password-reset kick spares a newer one.
+    issued_at: i64,
     retry: RetryDelay,
     /// When to retry a failed sync; `None` while the last sync succeeded.
     retry_at: Option<Instant>,
@@ -519,6 +601,7 @@ impl Connected {
         match admit(&mut self.messaging, &claims).await {
             Ok(()) => {
                 self.expiry = Expiry::new(claims.expires_at);
+                self.issued_at = claims.issued_at;
                 Ok(())
             }
             Err(Refusal::Unavailable) => self.retry_reauth().await,
```

Change `crates/push-message-svc/src/notify.rs`:

```diff
diff --git a/crates/push-message-svc/src/notify.rs b/crates/push-message-svc/src/notify.rs
index b9547f3..f08b82a 100644
--- a/crates/push-message-svc/src/notify.rs
+++ b/crates/push-message-svc/src/notify.rs
@@ -126,6 +126,7 @@ pub(crate) fn notifications(events: &[Event]) -> Vec<(String, Vec<u8>)> {
                 let kick = Kick {
                     reason: kick_reason(*reason).into(),
                     reset_at,
+                    connection_id: Vec::new(),
                 };
                 for device in devices {
                     messages.push((kick_channel(device.0), kick.encode_to_vec()));
```

Change `crates/push-proto/proto/push/internal/v1/messaging.proto`:

```diff
diff --git a/crates/push-proto/proto/push/internal/v1/messaging.proto b/crates/push-proto/proto/push/internal/v1/messaging.proto
index e28f112..1b4638a 100644
--- a/crates/push-proto/proto/push/internal/v1/messaging.proto
+++ b/crates/push-proto/proto/push/internal/v1/messaging.proto
@@ -188,22 +188,30 @@ message ReadPositionChanged {
   int64 read_seq = 2;
 }
 
-// Published on the Redis channel `kick:{device_id}` (spec §5.3, §5.4): the gateway holding
-// the device closes its socket at once (Plan 8). Never carries anything but the reason and
-// the reset time.
+// Published on the Redis channel `kick:{device_id}` (spec §5.3, §5.4, §7.3): every gateway
+// holding a connection of the device closes it at once, but for the exceptions below.
+// Never carries anything but the reason, the reset time and, for a takeover, the new
+// connection.
 message Kick {
   KickReason reason = 1;
   // PASSWORD_RESET: the reset, in seconds since the Unix epoch. A connection whose token
   // was issued after that second signed in after the reset and stays open.
   int64 reset_at = 2;
+  // TAKEN_OVER: the id the gateway gave the connection that takes over (a UUIDv7, so ids
+  // sort by time). Only older connections close; the newcomer and newer ones stay open.
+  bytes connection_id = 3;
 }
 
 enum KickReason {
   KICK_REASON_UNSPECIFIED = 0;
-  // The device was revoked: the client stops (close code 4003).
+  // The device was revoked: the client stops (close code 4003). From message-svc.
   KICK_REASON_DEVICE_REVOKED = 1;
-  // The user's password was reset: the client signs in again (close code 4001).
+  // The user's password was reset: the client signs in again (close code 4001). From
+  // message-svc.
   KICK_REASON_PASSWORD_RESET = 2;
+  // A newer connection of the device identified, on this or another gateway: the older
+  // one stops (close code 4003). From the gateway.
+  KICK_REASON_TAKEN_OVER = 3;
 }
 
 message AccountUser {
```

Change `crates/push-smoke/src/kicks.rs`:

```diff
diff --git a/crates/push-smoke/src/kicks.rs b/crates/push-smoke/src/kicks.rs
index 74d760c..441d07d 100644
--- a/crates/push-smoke/src/kicks.rs
+++ b/crates/push-smoke/src/kicks.rs
@@ -35,28 +35,35 @@ impl Kicks {
         Ok(Self { pubsub })
     }
 
-    /// Waits for `device` to be kicked for `reason`; how long that took.
+    /// Waits for `device` to be kicked for `reason`; how long that took. The takeovers
+    /// a gateway announces whenever a device connects are skipped.
     pub async fn kicked(&mut self, device: Uuid, reason: KickReason) -> anyhow::Result<Duration> {
         let started = Instant::now();
+        let deadline = tokio::time::Instant::now() + WORKER_TIMEOUT;
         let channel = kick_channel(device);
         let mut messages = self.pubsub.on_message();
-        let Ok(message) = tokio::time::timeout(WORKER_TIMEOUT, messages.next()).await else {
-            bail!("{device} was not kicked within {WORKER_TIMEOUT:?}");
-        };
-        let message = message.ok_or_else(|| anyhow!("the Redis subscription ended"))?;
-        if message.get_channel_name() != channel {
-            bail!(
-                "waiting for {device} to be kicked, but {} was kicked instead",
-                message.get_channel_name()
-            );
+        loop {
+            let Ok(message) = tokio::time::timeout_at(deadline, messages.next()).await else {
+                bail!("{device} was not kicked within {WORKER_TIMEOUT:?}");
+            };
+            let message = message.ok_or_else(|| anyhow!("the Redis subscription ended"))?;
+            let kick = Kick::decode(message.get_payload_bytes()).context("decode a kick")?;
+            if kick.reason() == KickReason::TakenOver {
+                continue;
+            }
+            if message.get_channel_name() != channel {
+                bail!(
+                    "waiting for {device} to be kicked, but {} was kicked instead",
+                    message.get_channel_name()
+                );
+            }
+            if kick.reason() != reason {
+                bail!(
+                    "{device} was kicked for {:?}, not {reason:?}",
+                    kick.reason()
+                );
+            }
+            return Ok(started.elapsed());
         }
-        let kick = Kick::decode(message.get_payload_bytes()).context("decode a kick")?;
-        if kick.reason() != reason {
-            bail!(
-                "{device} was kicked for {:?}, not {reason:?}",
-                kick.reason()
-            );
-        }
-        Ok(started.elapsed())
     }
 }
```

Change `crates/push-gateway/tests/gateway.rs`:

```diff
diff --git a/crates/push-gateway/tests/gateway.rs b/crates/push-gateway/tests/gateway.rs
index d71d411..d035a20 100644
--- a/crates/push-gateway/tests/gateway.rs
+++ b/crates/push-gateway/tests/gateway.rs
@@ -7,7 +7,9 @@ use std::time::Duration;
 
 use futures_util::{SinkExt, StreamExt};
 use push_auth::tokens::AccessTokens;
-use push_domain::{DeviceId, InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock, UserId};
+use push_domain::{
+    ConversationId, DeviceId, InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock, UserId,
+};
 use push_gateway::{Config, serve};
 use push_message_svc::{MessagingApi, RedisNotifier};
 use push_proto::internal::v1::messaging_client::MessagingClient;
@@ -17,8 +19,11 @@ use push_proto::internal::v1::{
     ReadUpToRequest, ReadUpToResponse, RequestCall, SendRequest, SyncRequest, SyncResponse,
 };
 use push_proto::v1::frame::Body;
-use push_proto::v1::{Ack, AuthExpiring, ContentType, Identify, Ping, Pong, ReAuth, Send, SendAck};
-use push_proto::{decode_frame, encode_frame, id_bytes};
+use push_proto::v1::{
+    Ack, AuthExpiring, ContactEvent, ContactState, ContentType, DirectReceipt, Identify, Ping,
+    Pong, ReAuth, ReadPosition, Receipt, Send, SendAck, receipt,
+};
+use push_proto::{decode_frame, encode_frame, id, id_bytes};
 use testcontainers_modules::redis::Redis;
 use testcontainers_modules::testcontainers::runners::AsyncRunner;
 use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
@@ -443,11 +448,15 @@ async fn shutdown_closes_connections_with_4005() {
 async fn an_expiring_token_is_announced_and_renewed_by_reauth() {
     let stack = stack().await;
     let (user, device) = person(&stack).await;
+    let (other, other_device) = person(&stack).await;
     // Expired 25 s ago, within the leeway: 5 s left to send ReAuth.
     let issued_at = now() - 900 - 25;
     let mut renewed = connect(&stack).await;
     let mut lapsed = connect(&stack).await;
-    for socket in [&mut renewed, &mut lapsed] {
+    for (socket, user, device) in [
+        (&mut renewed, user, device),
+        (&mut lapsed, other, other_device),
+    ] {
         send(socket, identify_issued(&stack, user, device, issued_at)).await;
         assert!(matches!(next(socket).await, Body::Ready(_)));
         assert_eq!(
@@ -462,11 +471,171 @@ async fn an_expiring_token_is_announced_and_renewed_by_reauth() {
     assert_eq!(next(&mut renewed).await, Body::Pong(Pong {}));
 
     // A token of another device cannot renew the connection.
-    let (_, other_device) = person(&stack).await;
-    let (token, _) = stack.issuer.issue(user, other_device, now());
+    let (token, _) = stack.issuer.issue(user, Uuid::now_v7(), now());
     send(&mut renewed, Body::ReAuth(ReAuth { token })).await;
     assert_eq!(close_code(&mut renewed).await, 4001);
 }
+
+/// The first frame `wanted` accepts, skipping others, within 5 s each.
+async fn wait_for(socket: &mut Socket, wanted: impl Fn(&Body) -> bool) -> Body {
+    loop {
+        let frame = next(socket).await;
+        if wanted(&frame) {
+            return frame;
+        }
+    }
+}
+
+/// Spec §12 test 6 and the owner's close codes: a revoked device is told to stop (4003),
+/// a password reset to sign in again (4001).
+#[tokio::test]
+async fn a_revocation_and_a_password_reset_close_the_socket_at_once() {
+    let stack = stack().await;
+    let (alice, alice_device) = person(&stack).await;
+    let (bob, bob_device) = person(&stack).await;
+    let mut alice_socket = ready(&stack, alice, alice_device).await;
+    let mut bob_socket = ready(&stack, bob, bob_device).await;
+    stack
+        .messaging
+        .device_revoked(DeviceId(alice_device), UserId(alice))
+        .await
+        .unwrap();
+    assert_eq!(close_code(&mut alice_socket).await, 4003);
+
+    // Bob's laptop signed in after the reset, which reaches the gateway later: its newer
+    // token keeps it open, while the phone's session from before closes.
+    let laptop = Uuid::now_v7();
+    stack
+        .messaging
+        .device_registered(DeviceId(laptop), UserId(bob))
+        .await
+        .unwrap();
+    let mut laptop_socket = connect(&stack).await;
+    send(
+        &mut laptop_socket,
+        identify_issued(&stack, bob, laptop, now() + 2),
+    )
+    .await;
+    assert!(matches!(next(&mut laptop_socket).await, Body::Ready(_)));
+    stack
+        .messaging
+        .password_reset(UserId(bob), None)
+        .await
+        .unwrap();
+    assert_eq!(close_code(&mut bob_socket).await, 4001);
+    send(&mut laptop_socket, Body::Ping(Ping {})).await;
+    assert_eq!(next(&mut laptop_socket).await, Body::Pong(Pong {}));
+}
+
+/// Spec §7.3: the newest connection of a device wins; the older one is told to stop.
+#[tokio::test]
+async fn a_newer_connection_of_the_device_takes_over() {
+    let stack = stack().await;
+    let (user, device) = person(&stack).await;
+    let mut first = ready(&stack, user, device).await;
+    let mut second = ready(&stack, user, device).await;
+    assert_eq!(close_code(&mut first).await, 4003);
+    send(&mut second, Body::Ping(Ping {})).await;
+    assert_eq!(next(&mut second).await, Body::Pong(Pong {}));
+}
+
+#[tokio::test]
+async fn contact_events_receipts_and_read_positions_reach_the_sockets() {
+    let stack = stack().await;
+    let (alice, alice_device) = person(&stack).await;
+    let (bob, bob_device) = person(&stack).await;
+    let mut alice_socket = ready(&stack, alice, alice_device).await;
+    let mut bob_socket = ready(&stack, bob, bob_device).await;
+
+    stack
+        .messaging
+        .contact_request(UserId(alice), UserId(bob))
+        .await
+        .unwrap();
+    let event = |user: Uuid, state: ContactState| {
+        Body::ContactEvent(ContactEvent {
+            user_id: id_bytes(user),
+            state: state.into(),
+        })
+    };
+    assert_eq!(
+        next(&mut bob_socket).await,
+        event(alice, ContactState::RequestedIn)
+    );
+    assert_eq!(
+        next(&mut alice_socket).await,
+        event(bob, ContactState::RequestedOut)
+    );
+    stack
+        .messaging
+        .contact_accept(UserId(bob), UserId(alice))
+        .await
+        .unwrap();
+    assert_eq!(
+        next(&mut bob_socket).await,
+        event(alice, ContactState::Accepted)
+    );
+    let conversation = stack
+        .messaging
+        .open_direct(UserId(alice), UserId(bob))
+        .await
+        .unwrap();
+    let conversation = id_bytes(conversation.0);
+
+    send(
+        &mut alice_socket,
+        Body::Send(Send {
+            client_msg_id: id_bytes(Uuid::now_v7()),
+            conversation_id: conversation.clone(),
+            content_type: ContentType::Text.into(),
+            body: b"hello".to_vec(),
+            client_ts: None,
+        }),
+    )
+    .await;
+    wait_for(&mut bob_socket, |frame| matches!(frame, Body::Deliver(_))).await;
+    send(
+        &mut bob_socket,
+        Body::Ack(Ack {
+            conversation_id: conversation.clone(),
+            seq: 1,
+        }),
+    )
+    .await;
+    let receipt = |delivered_seq, read_seq| {
+        Body::Receipt(Receipt {
+            conversation_id: conversation.clone(),
+            positions: Some(receipt::Positions::Direct(DirectReceipt {
+                user_id: id_bytes(bob),
+                delivered_seq,
+                read_seq,
+            })),
+        })
+    };
+    let delivered = receipt(1, 0);
+    let got = wait_for(&mut alice_socket, |frame| matches!(frame, Body::Receipt(_))).await;
+    assert_eq!(got, delivered);
+
+    stack
+        .messaging
+        .read_up_to(UserId(bob), ConversationId(id(&conversation).unwrap()), 1)
+        .await
+        .unwrap();
+    let position = wait_for(&mut bob_socket, |frame| {
+        matches!(frame, Body::ReadPosition(_))
+    })
+    .await;
+    assert_eq!(
+        position,
+        Body::ReadPosition(ReadPosition {
+            conversation_id: conversation.clone(),
+            read_seq: 1,
+        })
+    );
+    let got = wait_for(&mut alice_socket, |frame| matches!(frame, Body::Receipt(_))).await;
+    assert_eq!(got, receipt(1, 1));
+}
+
 /// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
 /// messages from Alice in it, sent through message-svc before anyone connects.
 async fn chat_with_backlog(stack: &Stack, count: u32) -> ((Uuid, Uuid), (Uuid, Uuid), Vec<u8>) {
```

Change `crates/push-gateway/tests/hub.rs`:

```diff
diff --git a/crates/push-gateway/tests/hub.rs b/crates/push-gateway/tests/hub.rs
index 157aa8c..2d80e0c 100644
--- a/crates/push-gateway/tests/hub.rs
+++ b/crates/push-gateway/tests/hub.rs
@@ -3,9 +3,11 @@
 use std::time::Duration;
 
 use prost::Message;
-use push_gateway::Hub;
-use push_proto::internal::v1::{EntriesAvailable, Notification, notification::Kind};
-use push_proto::notify_channel;
+use push_gateway::{Hub, Membership, Signal};
+use push_proto::internal::v1::{
+    EntriesAvailable, Kick, KickReason, Notification, notification::Kind,
+};
+use push_proto::{kick_channel, notify_channel};
 use redis::AsyncCommands;
 use testcontainers_modules::redis::Redis;
 use testcontainers_modules::testcontainers::runners::AsyncRunner;
@@ -34,6 +36,21 @@ async fn connected(hub: &Hub) {
     panic!("the hub did not connect to Redis within 5 s");
 }
 
+/// A connection of `user` on a new device, as the gateway makes one.
+async fn join(hub: &Hub, user: Uuid) -> (Membership, Uuid) {
+    let device = Uuid::now_v7();
+    let mut membership = hub.watch_device(device, Uuid::now_v7(), 0).await;
+    membership.join_user(user).await;
+    (membership, device)
+}
+
+/// The next signal, within `seconds`.
+async fn next(membership: &mut Membership, seconds: u64) -> Signal {
+    tokio::time::timeout(Duration::from_secs(seconds), membership.next())
+        .await
+        .expect("a signal in time")
+}
+
 async fn subscribers(client: &redis::Client, channel: &str) -> i64 {
     let mut connection = client.get_multiplexed_async_connection().await.unwrap();
     let counts: Vec<(String, i64)> = redis::cmd("PUBSUB")
@@ -53,7 +70,7 @@ async fn a_notification_published_on_redis_rings_the_users_session() {
     connected(&hub).await;
 
     let user = Uuid::now_v7();
-    let mut session = hub.join(user).await;
+    let (mut session, device) = join(&hub, user).await;
     let payload = Notification {
         kind: Some(Kind::EntriesAvailable(EntriesAvailable {})),
     }
@@ -63,9 +80,21 @@ async fn a_notification_published_on_redis_rings_the_users_session() {
         .publish(notify_channel(user), payload)
         .await
         .unwrap();
-    tokio::time::timeout(Duration::from_secs(5), session.rung())
+    assert_eq!(next(&mut session, 5).await, Signal::Ring);
+
+    // The device's kick channel was subscribed before anything else.
+    let kick = Kick {
+        reason: KickReason::DeviceRevoked.into(),
+        ..Kick::default()
+    };
+    let _: i64 = publisher
+        .publish(kick_channel(device), kick.encode_to_vec())
         .await
-        .expect("the session is rung within 5 s");
+        .unwrap();
+    assert_eq!(
+        next(&mut session, 5).await,
+        Signal::Kick(KickReason::DeviceRevoked)
+    );
 
     shutdown.cancel();
     subscriber.await.unwrap();
@@ -80,8 +109,9 @@ async fn a_session_is_rung_again_after_the_subscription_is_reestablished() {
 
     let user = Uuid::now_v7();
     let channel = notify_channel(user);
-    let mut session = hub.join(user).await;
+    let (mut session, device) = join(&hub, user).await;
     assert_eq!(subscribers(&client, &channel).await, 1);
+    assert_eq!(subscribers(&client, &kick_channel(device)).await, 1);
 
     let mut admin = client.get_multiplexed_async_connection().await.unwrap();
     let killed: i64 = redis::cmd("CLIENT")
@@ -94,20 +124,17 @@ async fn a_session_is_rung_again_after_the_subscription_is_reestablished() {
     assert!(killed >= 1, "the hub's pubsub connection was killed");
 
     // The reconnect rings every session, since notifications may have been lost.
-    tokio::time::timeout(Duration::from_secs(10), session.rung())
-        .await
-        .expect("the session is rung after the reconnect within 10 s");
+    assert_eq!(next(&mut session, 10).await, Signal::Ring);
     connected(&hub).await;
     assert_eq!(subscribers(&client, &channel).await, 1);
+    assert_eq!(subscribers(&client, &kick_channel(device)).await, 1);
 
     let payload = Notification {
         kind: Some(Kind::EntriesAvailable(EntriesAvailable {})),
     }
     .encode_to_vec();
     let _: i64 = admin.publish(&channel, payload).await.unwrap();
-    tokio::time::timeout(Duration::from_secs(5), session.rung())
-        .await
-        .expect("a fresh publish rings the session within 5 s");
+    assert_eq!(next(&mut session, 5).await, Signal::Ring);
 
     shutdown.cancel();
     subscriber.await.unwrap();
@@ -122,15 +149,17 @@ async fn the_channel_is_unsubscribed_when_the_last_session_leaves() {
 
     let user = Uuid::now_v7();
     let channel = notify_channel(user);
-    let session = hub.join(user).await;
+    let (session, device) = join(&hub, user).await;
     assert_eq!(subscribers(&client, &channel).await, 1);
     drop(session);
     for _ in 0..100 {
-        if subscribers(&client, &channel).await == 0 {
+        if subscribers(&client, &channel).await == 0
+            && subscribers(&client, &kick_channel(device)).await == 0
+        {
             shutdown.cancel();
             return;
         }
         tokio::time::sleep(Duration::from_millis(50)).await;
     }
-    panic!("the channel is still subscribed 5 s after the last session left");
+    panic!("the channels are still subscribed 5 s after the last session left");
 }
```
- [ ] **Step 2: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "feat(gateway): kicks, takeovers and server frames from Redis; kick channel watched before Identify"
```

Expected: 844 tests pass, no warnings, no diff.
---

### Task 8: Every request frame to message-svc; groups behind one switch; 1009

**Files:**
- Modify: `README.md`, `crates/push-gateway/Cargo.toml`, `crates/push-gateway/src/config.rs`, `crates/push-gateway/src/server.rs`, `crates/push-gateway/src/session.rs`, `crates/push-gateway/tests/gateway.rs`, `crates/push-smoke/tests/local.rs`, `deploy/helm/push-messenger/templates/gateway.yaml`, `deploy/helm/push-messenger/values.yaml`, `deploy/helm/test-chart.ps1`

Decisions 8 and 11. `Edit`, `Delete`, `ReadUpTo` and `Request` go to message-svc's RPCs
with their `request_id`s; the six group requests are answered `NOT_AVAILABLE` unless
`PUSH_GROUPS=1` (chart `gateway.groups`, off); a message over 128 KiB closes with 1009
(WebSocket frames may be up to 512 KiB so the close reaches the client).

- [ ] **Step 1: Make the changes**

Change `README.md`:

```diff
diff --git a/README.md b/README.md
index 10ee4ce..c20a888 100644
--- a/README.md
+++ b/README.md
@@ -96,6 +96,7 @@ at start-up.
 | `PUSH_AUTH_URL` | base URL of auth-svc (`http://push-auth:8080`); the token keys come from its `/v1/auth/jwks` | required |
 | `PUSH_TOKEN_AUDIENCE` | the gateway's public URL (`wss://…/v1/socket`), the `aud` access tokens must name | required |
 | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready`, 1000 to 600000 | `30000` |
+| `PUSH_GROUPS` | `1` serves group requests; `0` refuses them with `NOT_AVAILABLE` (groups are not offered to apps in the first release; message-svc keeps them) | `0` |
 
 `push-auth`:
 
```

Change `crates/push-gateway/Cargo.toml`:

```diff
diff --git a/crates/push-gateway/Cargo.toml b/crates/push-gateway/Cargo.toml
index 93f82a1..f802bd8 100644
--- a/crates/push-gateway/Cargo.toml
+++ b/crates/push-gateway/Cargo.toml
@@ -21,6 +21,8 @@ serde_json.workspace = true
 thiserror.workspace = true
 tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
 tokio-stream.workspace = true
+# Only to recognise axum's WebSocket errors: the same version axum 0.8 uses.
+tokio-tungstenite.workspace = true
 tokio-util.workspace = true
 tonic.workspace = true
 tracing.workspace = true
@@ -34,7 +36,6 @@ push-domain.workspace = true
 push-message-svc.workspace = true
 testcontainers-modules.workspace = true
 tokio = { workspace = true, features = ["test-util"] }
-tokio-tungstenite.workspace = true
 
 [lints]
 workspace = true
```

Change `crates/push-gateway/src/config.rs`:

```diff
diff --git a/crates/push-gateway/src/config.rs b/crates/push-gateway/src/config.rs
index d35f751..e39117d 100644
--- a/crates/push-gateway/src/config.rs
+++ b/crates/push-gateway/src/config.rs
@@ -14,6 +14,7 @@ use std::time::Duration;
 /// | `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
 /// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
 /// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
+/// | `PUSH_GROUPS` | `1` serves group requests; `0` refuses them with `NOT_AVAILABLE` (groups are not offered to apps in the first release) | `0` |
 ///
 /// `PUSH_DEV_AUTH=1` (the DEV ONLY identities without a token, removed in Plan 8) is
 /// refused, so a deployment that still expects it fails at start-up.
@@ -26,6 +27,8 @@ pub struct Config {
     pub listen_addr: SocketAddr,
     pub health_addr: SocketAddr,
     pub heartbeat: Duration,
+    /// Whether group requests are served.
+    pub groups: bool,
 }
 
 // The Redis URL carries a password, so it never appears in debug output.
@@ -40,6 +43,7 @@ impl std::fmt::Debug for Config {
             .field("listen_addr", &self.listen_addr)
             .field("health_addr", &self.health_addr)
             .field("heartbeat", &self.heartbeat)
+            .field("groups", &self.groups)
             .finish()
     }
 }
@@ -105,6 +109,7 @@ impl Config {
             listen_addr: parsed(&lookup, "PUSH_LISTEN_ADDR", ([0, 0, 0, 0], 8080).into())?,
             health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
             heartbeat: Duration::from_millis(heartbeat_ms),
+            groups: flag(&lookup, "PUSH_GROUPS")?,
         })
     }
 
@@ -134,6 +139,20 @@ where
     }
 }
 
+/// A switch: on only for exactly `1`; off when unset, empty or `0`. Anything else
+/// (`true`, `yes`) is refused, so a typo can never leave it in doubt.
+fn flag(lookup: &impl Fn(&str) -> Option<String>, name: &'static str) -> Result<bool, ConfigError> {
+    match lookup(name).as_deref() {
+        None | Some("") | Some("0") => Ok(false),
+        Some("1") => Ok(true),
+        Some(other) => Err(ConfigError::Invalid {
+            name,
+            value: other.to_owned(),
+            reason: "use 1 to enable or 0 to disable".to_owned(),
+        }),
+    }
+}
+
 #[cfg(test)]
 mod tests {
     use std::collections::HashMap;
@@ -165,6 +184,21 @@ mod tests {
         assert_eq!(config.listen_addr, "0.0.0.0:8080".parse().unwrap());
         assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
         assert_eq!(config.heartbeat, Duration::from_secs(30));
+        assert!(!config.groups, "groups are off unless switched on");
+    }
+
+    #[test]
+    fn groups_are_on_only_for_exactly_1() {
+        for (value, expected) in [("1", true), ("0", false), ("", false)] {
+            let mut variables = REQUIRED.to_vec();
+            variables.push(("PUSH_GROUPS", value));
+            assert_eq!(config(&variables).unwrap().groups, expected, "{value:?}");
+        }
+        for value in ["true", "yes", " 1"] {
+            let mut variables = REQUIRED.to_vec();
+            variables.push(("PUSH_GROUPS", value));
+            assert!(config(&variables).is_err(), "{value:?}");
+        }
     }
 
     #[test]
```

Change `crates/push-gateway/src/server.rs`:

```diff
diff --git a/crates/push-gateway/src/server.rs b/crates/push-gateway/src/server.rs
index 60784e9..73b77f3 100644
--- a/crates/push-gateway/src/server.rs
+++ b/crates/push-gateway/src/server.rs
@@ -13,10 +13,11 @@ use axum::routing::get;
 use prost::Message as _;
 use push_proto::internal::v1::messaging_client::MessagingClient;
 use push_proto::internal::v1::{
-    AckRequest, IdentifyRequest, Kick, KickReason, SendRequest, SyncRequest,
+    AckRequest, DeleteRequest, EditRequest, IdentifyRequest, Kick, KickReason, ReadUpToRequest,
+    RequestCall, SendRequest, SyncRequest,
 };
 use push_proto::v1::frame::Body;
-use push_proto::v1::{AuthExpiring, Error, ErrorCode};
+use push_proto::v1::{AuthExpiring, Error, ErrorCode, SendAck};
 use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes, kick_channel};
 use redis::aio::{ConnectionManager, ConnectionManagerConfig};
 use tokio::net::TcpListener;
@@ -176,17 +177,23 @@ fn health_router(verifier: Verifier, shutdown: CancellationToken) -> Router {
         )
 }
 
+/// A message is at most [`MAX_FRAME_BYTES`]. A WebSocket frame may be bigger, up to
+/// 512 KiB, so that a message over the limit is read whole and refused with a close frame
+/// (1009) the client can read; a frame above 512 KiB is refused from its header, before
+/// its payload is buffered, and the client may only see the connection drop.
+const MAX_WEBSOCKET_FRAME: usize = 4 * MAX_FRAME_BYTES;
+
 async fn upgrade(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response {
     let sessions = app.sessions.clone();
     upgrade
         .max_message_size(MAX_FRAME_BYTES)
-        .max_frame_size(MAX_FRAME_BYTES)
+        .max_frame_size(MAX_WEBSOCKET_FRAME)
         .on_upgrade(move |socket| sessions.track_future(connection(socket, app)))
 }
 
 /// One client connection, from `Identify` to close.
 async fn connection(mut socket: WebSocket, app: App) {
-    let mut session = Session::new();
+    let mut session = Session::new(app.config.groups);
     let Some((claims, mut membership)) = identify(&mut socket, &mut session, &app).await else {
         return;
     };
@@ -258,7 +265,11 @@ async fn connection(mut socket: WebSocket, app: App) {
                     Some(Ok(Message::Text(_))) => connected.session.on_text(),
                     // WebSocket pings are answered by the library.
                     Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
-                    Some(Ok(Message::Close(_)) | Err(_)) | None => return,
+                    Some(Err(error)) => {
+                        refuse_oversized(&mut connected.socket, error).await;
+                        return;
+                    }
+                    Some(Ok(Message::Close(_))) | None => return,
                 };
                 if connected.run(step).await.is_err() {
                     return;
@@ -269,6 +280,19 @@ async fn connection(mut socket: WebSocket, app: App) {
     }
 }
 
+/// A message over the WebSocket limit ([`MAX_FRAME_BYTES`]) ends the connection with 1009
+/// (RFC 6455: message too big); other read errors end it without a word, the peer being
+/// gone or broken.
+async fn refuse_oversized(socket: &mut WebSocket, error: axum::Error) {
+    let oversized = error
+        .into_inner()
+        .downcast::<tokio_tungstenite::tungstenite::Error>()
+        .is_ok_and(|error| matches!(*error, tokio_tungstenite::tungstenite::Error::Capacity(_)));
+    if oversized {
+        close_with(socket, close::MESSAGE_TOO_BIG, "a frame is at most 128 KiB").await;
+    }
+}
+
 fn far_future() -> Instant {
     Instant::now() + Duration::from_secs(24 * 60 * 60)
 }
@@ -298,7 +322,11 @@ async fn identify(
                 Some(Ok(Message::Binary(bytes))) => break session.on_binary(&bytes),
                 Some(Ok(Message::Text(_))) => break session.on_text(),
                 Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
-                Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
+                Some(Err(error)) => {
+                    refuse_oversized(socket, error).await;
+                    return None;
+                }
+                Some(Ok(Message::Close(_))) | None => return None,
             },
         }
     };
@@ -555,6 +583,55 @@ impl Connected {
                     }
                 }
             }
+            Step::Edit(edit) => {
+                let client_msg_id = edit.client_msg_id.clone();
+                let request = EditRequest {
+                    user_id: id_bytes(self.user),
+                    device_id: id_bytes(self.device),
+                    edit: Some(edit),
+                };
+                let answer = self.messaging.edit(request).await;
+                self.acknowledge(answer, client_msg_id).await
+            }
+            Step::Delete(delete) => {
+                let client_msg_id = delete.client_msg_id.clone();
+                let request = DeleteRequest {
+                    user_id: id_bytes(self.user),
+                    device_id: id_bytes(self.device),
+                    delete: Some(delete),
+                };
+                let answer = self.messaging.delete(request).await;
+                self.acknowledge(answer, client_msg_id).await
+            }
+            Step::ReadUpTo(read) => {
+                let request = ReadUpToRequest {
+                    user_id: id_bytes(self.user),
+                    read: Some(read),
+                };
+                match self.messaging.read_up_to(request).await {
+                    Ok(_) => Ok(()),
+                    Err(status) => {
+                        let error = Body::Error(error_from_status(&status));
+                        send(&mut self.socket, error).await
+                    }
+                }
+            }
+            Step::Request(request) => {
+                let request_id = request.request_id;
+                let call = RequestCall {
+                    user_id: id_bytes(self.user),
+                    request: Some(request),
+                };
+                let body = match self.messaging.request(call).await {
+                    Ok(response) => Body::Response(response.into_inner()),
+                    Err(status) => {
+                        let mut error = error_from_status(&status);
+                        error.request_id = Some(request_id);
+                        Body::Error(error)
+                    }
+                };
+                send(&mut self.socket, body).await
+            }
             Step::Close(code, reason) => {
                 close_with(&mut self.socket, code, reason).await;
                 Err(Closed)
@@ -566,6 +643,23 @@ impl Connected {
         }
     }
 
+    /// Answers an edit or a delete: its `SendAck`, or the error with its `client_msg_id`.
+    async fn acknowledge(
+        &mut self,
+        answer: Result<tonic::Response<SendAck>, tonic::Status>,
+        client_msg_id: Vec<u8>,
+    ) -> Result<(), Closed> {
+        let body = match answer {
+            Ok(ack) => Body::SendAck(ack.into_inner()),
+            Err(status) => {
+                let mut error = error_from_status(&status);
+                error.client_msg_id = Some(client_msg_id);
+                Body::Error(error)
+            }
+        };
+        send(&mut self.socket, body).await
+    }
+
     /// Tells the client its token expires soon, once per token.
     async fn warn_expiry(&mut self) -> Result<(), Closed> {
         self.expiry.warned = true;
```

Change `crates/push-gateway/src/session.rs`:

```diff
diff --git a/crates/push-gateway/src/session.rs b/crates/push-gateway/src/session.rs
index d80acc4..7331d5a 100644
--- a/crates/push-gateway/src/session.rs
+++ b/crates/push-gateway/src/session.rs
@@ -7,7 +7,9 @@ use std::time::Duration;
 
 use push_proto::close;
 use push_proto::v1::frame::Body;
-use push_proto::v1::{Ack, Deliver, Error, ErrorCode, Pong, Ready, Send};
+use push_proto::v1::{
+    Ack, Delete, Deliver, Edit, Error, ErrorCode, Pong, ReadUpTo, Ready, Request, Send, request,
+};
 use push_proto::{FrameError, decode_frame, id_bytes};
 use uuid::Uuid;
 
@@ -29,6 +31,16 @@ pub enum Step {
     /// Forward an ack to message-svc; answer only if it fails. If it moved the position,
     /// ask [`Session::sync_after_ack`] whether to sync again.
     Ack(Ack),
+    /// Forward an edit to message-svc; answer with its `SendAck` or an error.
+    Edit(Edit),
+    /// Forward a delete to message-svc; answer with its `SendAck` or an error.
+    Delete(Delete),
+    /// Forward a read position to message-svc; answer only if it fails (the user's
+    /// devices hear the new position as `ReadPosition`).
+    ReadUpTo(ReadUpTo),
+    /// Forward a request to message-svc; answer with its `Response` or an error, both
+    /// carrying the request's `request_id`.
+    Request(Request),
     /// Close the connection with this code and reason.
     Close(u16, &'static str),
 }
@@ -46,6 +58,9 @@ enum State {
 
 #[derive(Debug)]
 pub struct Session {
+    /// Whether group requests are served (`PUSH_GROUPS`); refused with `NOT_AVAILABLE`
+    /// otherwise.
+    groups: bool,
     state: State,
     /// The highest `seq` sent to the client per conversation, so a sync that repeats
     /// entries the client has not acked yet does not send them twice.
@@ -56,9 +71,10 @@ pub struct Session {
 }
 
 impl Session {
-    /// A new connection.
-    pub fn new() -> Self {
+    /// A new connection. `groups`: whether group requests are served.
+    pub fn new(groups: bool) -> Self {
         Self {
+            groups,
             state: State::New,
             sent: HashMap::new(),
             more_waiting: false,
@@ -101,6 +117,22 @@ impl Session {
             (Body::ReAuth(reauth), State::Ready { .. }) => Step::ReAuth(reauth.token),
             (Body::Send(send), State::Ready { .. }) => Step::Send(send),
             (Body::Ack(ack), State::Ready { .. }) => Step::Ack(ack),
+            (Body::Edit(edit), State::Ready { .. }) => Step::Edit(edit),
+            (Body::Delete(delete), State::Ready { .. }) => Step::Delete(delete),
+            (Body::ReadUpTo(read), State::Ready { .. }) => Step::ReadUpTo(read),
+            (Body::Request(request), State::Ready { .. }) => {
+                if !self.groups && request.body.as_ref().is_some_and(is_group_request) {
+                    return Step::Reply(Body::Error(Error {
+                        code: ErrorCode::NotAvailable.into(),
+                        message: "groups are not available".to_owned(),
+                        retryable: false,
+                        client_msg_id: None,
+                        request_id: Some(request.request_id),
+                        retry_after_s: None,
+                    }));
+                }
+                Step::Request(request)
+            }
             (Body::Ping(_), State::Ready { .. }) => Step::Reply(Body::Pong(Pong {})),
             (_, State::Ready { .. }) => reply_error(ErrorCode::BadFrame, "not a client frame"),
         }
@@ -161,10 +193,20 @@ impl Session {
     }
 }
 
-impl Default for Session {
-    fn default() -> Self {
-        Self::new()
-    }
+/// The requests about groups, which apps cannot use in the first release (owner
+/// decision): message-svc keeps serving them, the gateway refuses them unless
+/// `PUSH_GROUPS=1`. No group can be created otherwise, so no group entries, receipts or
+/// listings reach clients either.
+fn is_group_request(body: &request::Body) -> bool {
+    matches!(
+        body,
+        request::Body::CreateGroup(_)
+            | request::Body::AddMembers(_)
+            | request::Body::RemoveMember(_)
+            | request::Body::LeaveGroup(_)
+            | request::Body::RenameGroup(_)
+            | request::Body::SetRole(_)
+    )
 }
 
 /// When to retry a failed sync: after 1 s, doubling up to 30 s, and from 1 s again once a
@@ -215,7 +257,10 @@ fn reply_error(code: ErrorCode, message: &str) -> Step {
 
 #[cfg(test)]
 mod tests {
-    use push_proto::v1::{Identify, Ping, ReAuth};
+    use push_proto::v1::{
+        AddMembers, CreateGroup, Identify, LeaveGroup, ListContacts, Ping, ReAuth, RemoveMember,
+        RenameGroup, SetRole,
+    };
     use push_proto::{MAX_FRAME_BYTES, encode_frame};
 
     use super::*;
@@ -236,7 +281,7 @@ mod tests {
     }
 
     fn ready_session() -> (Session, Uuid) {
-        let mut session = Session::new();
+        let mut session = Session::new(false);
         assert_eq!(
             session.on_binary(&identify("token")),
             Step::Identify("token".to_owned())
@@ -265,7 +310,7 @@ mod tests {
 
     #[test]
     fn identify_hands_over_its_token_and_ready_names_the_tokens_device() {
-        let mut session = Session::new();
+        let mut session = Session::new(false);
         assert_eq!(
             session.on_binary(&identify("a.b.c")),
             Step::Identify("a.b.c".to_owned())
@@ -287,7 +332,7 @@ mod tests {
 
     #[test]
     fn identified_without_a_pending_identity_does_nothing() {
-        let mut session = Session::new();
+        let mut session = Session::new(false);
         assert_eq!(session.identified(&claims(), Duration::from_secs(30)), None);
         assert_eq!(session.device(), None);
     }
@@ -297,14 +342,14 @@ mod tests {
     fn anything_but_identify_first_is_a_protocol_error() {
         let ping = encode_frame(Body::Ping(Ping {}));
         for bytes in [ping.as_slice(), &[0xff, 0xff], &[]] {
-            let mut session = Session::new();
+            let mut session = Session::new(false);
             assert_eq!(
                 session.on_binary(bytes),
                 Step::Close(1002, "the first frame must be Identify")
             );
         }
         assert_eq!(
-            Session::new().on_text(),
+            Session::new(false).on_text(),
             Step::Close(1002, "the first frame must be Identify")
         );
     }
@@ -314,7 +359,7 @@ mod tests {
         let ping = encode_frame(Body::Ping(Ping {}));
         let second = identify("another");
         for bytes in [ping.as_slice(), second.as_slice()] {
-            let mut session = Session::new();
+            let mut session = Session::new(false);
             assert_eq!(
                 session.on_binary(&identify("token")),
                 Step::Identify("token".to_owned())
@@ -333,7 +378,7 @@ mod tests {
             token: "fresh".to_owned(),
         }));
         assert_eq!(
-            Session::new().on_binary(&reauth),
+            Session::new(false).on_binary(&reauth),
             Step::Close(1002, "the first frame must be Identify")
         );
         let (mut session, _) = ready_session();
@@ -344,7 +389,7 @@ mod tests {
     fn an_oversized_frame_closes_before_identify_and_errors_once_ready() {
         let oversized = vec![0u8; MAX_FRAME_BYTES + 1];
         assert_eq!(
-            Session::new().on_binary(&oversized),
+            Session::new(false).on_binary(&oversized),
             Step::Close(1002, "the first frame must be Identify")
         );
         let (mut session, _) = ready_session();
@@ -394,6 +439,88 @@ mod tests {
         assert!(session.device().is_some());
     }
 
+    #[test]
+    fn edits_deletes_reads_and_requests_become_steps() {
+        let (mut session, _) = ready_session();
+        let edit = Edit {
+            client_msg_id: id_bytes(Uuid::now_v7()),
+            ..Edit::default()
+        };
+        assert_eq!(
+            session.on_binary(&encode_frame(Body::Edit(edit.clone()))),
+            Step::Edit(edit)
+        );
+        let delete = Delete {
+            client_msg_id: id_bytes(Uuid::now_v7()),
+            ..Delete::default()
+        };
+        assert_eq!(
+            session.on_binary(&encode_frame(Body::Delete(delete.clone()))),
+            Step::Delete(delete)
+        );
+        let read = ReadUpTo {
+            conversation_id: id_bytes(Uuid::now_v7()),
+            seq: 4,
+        };
+        assert_eq!(
+            session.on_binary(&encode_frame(Body::ReadUpTo(read.clone()))),
+            Step::ReadUpTo(read)
+        );
+        let list = Request {
+            request_id: 7,
+            body: Some(request::Body::ListContacts(ListContacts {})),
+        };
+        assert_eq!(
+            session.on_binary(&encode_frame(Body::Request(list.clone()))),
+            Step::Request(list)
+        );
+    }
+
+    fn group_requests() -> Vec<request::Body> {
+        let conversation_id = id_bytes(Uuid::now_v7());
+        vec![
+            request::Body::CreateGroup(CreateGroup::default()),
+            request::Body::AddMembers(AddMembers::default()),
+            request::Body::RemoveMember(RemoveMember::default()),
+            request::Body::LeaveGroup(LeaveGroup { conversation_id }),
+            request::Body::RenameGroup(RenameGroup::default()),
+            request::Body::SetRole(SetRole::default()),
+        ]
+    }
+
+    #[test]
+    fn group_requests_are_refused_unless_groups_are_switched_on() {
+        let (mut session, _) = ready_session();
+        for (number, body) in (1..).zip(group_requests()) {
+            let request = Request {
+                request_id: number,
+                body: Some(body),
+            };
+            let Step::Reply(Body::Error(error)) =
+                session.on_binary(&encode_frame(Body::Request(request)))
+            else {
+                panic!("a group request was not refused");
+            };
+            assert_eq!(error.code(), ErrorCode::NotAvailable);
+            assert_eq!(error.request_id, Some(number));
+            assert!(!error.retryable);
+        }
+
+        let mut session = Session::new(true);
+        session.on_binary(&identify("token"));
+        session.identified(&claims(), Duration::from_secs(30));
+        for body in group_requests() {
+            let request = Request {
+                request_id: 1,
+                body: Some(body),
+            };
+            assert_eq!(
+                session.on_binary(&encode_frame(Body::Request(request.clone()))),
+                Step::Request(request)
+            );
+        }
+    }
+
     #[test]
     fn entries_already_sent_are_not_sent_again() {
         let (mut session, _) = ready_session();
```

Change `deploy/helm/push-messenger/templates/gateway.yaml`:

```diff
diff --git a/deploy/helm/push-messenger/templates/gateway.yaml b/deploy/helm/push-messenger/templates/gateway.yaml
index 303e9f0..6b78577 100644
--- a/deploy/helm/push-messenger/templates/gateway.yaml
+++ b/deploy/helm/push-messenger/templates/gateway.yaml
@@ -65,6 +65,8 @@ spec:
               value: {{ printf "http://%s:8080" (include "push.name" (dict "root" . "component" "auth")) | quote }}
             - name: PUSH_TOKEN_AUDIENCE
               value: {{ include "push.socketUrl" . | quote }}
+            - name: PUSH_GROUPS
+              value: {{ ternary "1" "0" (eq (toString .Values.gateway.groups) "true") | quote }}
           ports:
             - name: http
               containerPort: 8080
```

Change `deploy/helm/push-messenger/values.yaml`:

```diff
diff --git a/deploy/helm/push-messenger/values.yaml b/deploy/helm/push-messenger/values.yaml
index 5b13a34..b1c7178 100644
--- a/deploy/helm/push-messenger/values.yaml
+++ b/deploy/helm/push-messenger/values.yaml
@@ -36,6 +36,10 @@ gateway:
   replicas: 1
   # Client ping interval announced in Ready, in milliseconds.
   heartbeatMs: 30000
+  # Groups are not offered to apps in the first release (owner decision): the gateway
+  # refuses group requests with NOT_AVAILABLE. message-svc keeps full group support;
+  # true serves them.
+  groups: false
   # 5 s preStop sleep + the gateway's own 10 s drain + margin.
   terminationGracePeriodSeconds: 30
   resources:
```

Change `deploy/helm/test-chart.ps1`:

```diff
diff --git a/deploy/helm/test-chart.ps1 b/deploy/helm/test-chart.ps1
index 9d8ba3e..4d74f2e 100644
--- a/deploy/helm/test-chart.ps1
+++ b/deploy/helm/test-chart.ps1
@@ -39,6 +39,11 @@ Check 'default: the gateway checks tokens with auth-svc''s keys, for its own URL
         $gateway -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "wss://push.example.com/v1/socket"' -and
         $default -notmatch 'PUSH_DEV_AUTH'
 }
+Check 'default: groups are refused; one switch serves them' {
+    $on = helm template push $chart --set gateway.groups=true | Out-String
+    (Doc $default 'Deployment' 'push-gateway') -match 'name: PUSH_GROUPS\s+value: "0"' -and
+        (Doc $on 'Deployment' 'push-gateway') -match 'name: PUSH_GROUPS\s+value: "1"'
+}
 Check 'default: the DevSetup switch is gone' { $default -notmatch 'PUSH_DEV_SETUP' }
 Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
 Check 'default: the worker has no dev mailbox and logs email; auth-svc has no mailbox at all' {
```

Change `crates/push-gateway/tests/gateway.rs`:

```diff
diff --git a/crates/push-gateway/tests/gateway.rs b/crates/push-gateway/tests/gateway.rs
index d035a20..2ef5c92 100644
--- a/crates/push-gateway/tests/gateway.rs
+++ b/crates/push-gateway/tests/gateway.rs
@@ -18,12 +18,15 @@ use push_proto::internal::v1::{
     AckRequest, AckResponse, DeleteRequest, EditRequest, IdentifyRequest, IdentifyResponse,
     ReadUpToRequest, ReadUpToResponse, RequestCall, SendRequest, SyncRequest, SyncResponse,
 };
+use push_proto::v1;
 use push_proto::v1::frame::Body;
 use push_proto::v1::{
-    Ack, AuthExpiring, ContactEvent, ContactState, ContentType, DirectReceipt, Identify, Ping,
-    Pong, ReAuth, ReadPosition, Receipt, Send, SendAck, receipt,
+    Ack, AuthExpiring, Contact, ContactAccept, ContactEvent, ContactRequest, ContactState,
+    ContentType, CreateGroup, Delete, DirectReceipt, Edit, ErrorCode, Identify, ListConversations,
+    OpenDirect, Ping, Pong, ReAuth, ReadPosition, ReadUpTo, Receipt, Send, SendAck, delete, edit,
+    receipt, request, response,
 };
-use push_proto::{decode_frame, encode_frame, id, id_bytes};
+use push_proto::{MAX_FRAME_BYTES, decode_frame, encode_frame, id, id_bytes};
 use testcontainers_modules::redis::Redis;
 use testcontainers_modules::testcontainers::runners::AsyncRunner;
 use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
@@ -171,6 +174,7 @@ async fn stack_with(sync_failures: u32) -> Stack {
         listen_addr: gateway,
         health_addr: health.local_addr().unwrap(),
         heartbeat: Duration::from_secs(30),
+        groups: false,
     };
     let shutdown = CancellationToken::new();
     let server = tokio::spawn(serve(config, listener, health, shutdown.clone()));
@@ -636,6 +640,182 @@ async fn contact_events_receipts_and_read_positions_reach_the_sockets() {
     assert_eq!(got, receipt(1, 1));
 }
 
+/// Sends a request and waits for its answer, skipping other frames.
+async fn request(socket: &mut Socket, request_id: u64, body: request::Body) -> Body {
+    send(
+        socket,
+        Body::Request(v1::Request {
+            request_id,
+            body: Some(body),
+        }),
+    )
+    .await;
+    wait_for(socket, |frame| match frame {
+        Body::Response(response) => response.request_id == request_id,
+        Body::Error(error) => error.request_id == Some(request_id),
+        _ => false,
+    })
+    .await
+}
+
+/// Every client frame of spec §7.1 travels to message-svc and back; group requests are
+/// refused (groups are not offered to apps in the first release).
+#[tokio::test]
+async fn requests_edits_deletes_and_reads_go_through_and_group_requests_are_refused() {
+    let stack = stack().await;
+    let (alice, alice_device) = person(&stack).await;
+    let (bob, bob_device) = person(&stack).await;
+    let mut alice_socket = ready(&stack, alice, alice_device).await;
+    let mut bob_socket = ready(&stack, bob, bob_device).await;
+
+    let ask = request::Body::ContactRequest(ContactRequest {
+        user_id: id_bytes(bob),
+    });
+    let Body::Response(answer) = request(&mut alice_socket, 1, ask).await else {
+        panic!("the contact request failed");
+    };
+    assert_eq!(
+        answer.body,
+        Some(response::Body::Contact(Contact {
+            user_id: id_bytes(bob),
+            state: ContactState::RequestedOut.into(),
+        }))
+    );
+    let accept = request::Body::ContactAccept(ContactAccept {
+        user_id: id_bytes(alice),
+    });
+    assert!(matches!(
+        request(&mut bob_socket, 2, accept).await,
+        Body::Response(_)
+    ));
+    let open = request::Body::OpenDirect(OpenDirect {
+        peer_user_id: id_bytes(bob),
+    });
+    let Body::Response(v1::Response {
+        body: Some(response::Body::DirectOpened(opened)),
+        ..
+    }) = request(&mut alice_socket, 3, open).await
+    else {
+        panic!("opening the conversation failed");
+    };
+    let conversation = opened.conversation_id;
+
+    let client_msg_id = id_bytes(Uuid::now_v7());
+    send(
+        &mut alice_socket,
+        Body::Send(Send {
+            client_msg_id: client_msg_id.clone(),
+            conversation_id: conversation.clone(),
+            content_type: ContentType::Text.into(),
+            body: b"hello".to_vec(),
+            client_ts: None,
+        }),
+    )
+    .await;
+    let edit_id = id_bytes(Uuid::now_v7());
+    send(
+        &mut alice_socket,
+        Body::Edit(Edit {
+            client_msg_id: edit_id.clone(),
+            conversation_id: conversation.clone(),
+            target: Some(edit::Target::TargetClientMsgId(client_msg_id.clone())),
+            body: b"hello, edited".to_vec(),
+            client_ts: None,
+        }),
+    )
+    .await;
+    let Body::SendAck(edited) = wait_for(
+        &mut alice_socket,
+        |frame| matches!(frame, Body::SendAck(ack) if ack.client_msg_id == edit_id),
+    )
+    .await
+    else {
+        unreachable!();
+    };
+    assert_eq!(edited.seq, 2);
+    let delete_id = id_bytes(Uuid::now_v7());
+    send(
+        &mut alice_socket,
+        Body::Delete(Delete {
+            client_msg_id: delete_id.clone(),
+            conversation_id: conversation.clone(),
+            target: Some(delete::Target::TargetClientMsgId(client_msg_id)),
+        }),
+    )
+    .await;
+    let deleted = wait_for(
+        &mut alice_socket,
+        |frame| matches!(frame, Body::SendAck(ack) if ack.client_msg_id == delete_id),
+    )
+    .await;
+    assert!(matches!(deleted, Body::SendAck(ack) if ack.seq == 3));
+    for seq in 1..=3 {
+        let Body::Deliver(entry) =
+            wait_for(&mut bob_socket, |frame| matches!(frame, Body::Deliver(_))).await
+        else {
+            unreachable!();
+        };
+        assert_eq!(entry.seq, seq);
+    }
+    send(
+        &mut bob_socket,
+        Body::ReadUpTo(ReadUpTo {
+            conversation_id: conversation.clone(),
+            seq: 1,
+        }),
+    )
+    .await;
+    let position = wait_for(&mut bob_socket, |frame| {
+        matches!(frame, Body::ReadPosition(_))
+    })
+    .await;
+    assert!(matches!(position, Body::ReadPosition(read) if read.read_seq == 1));
+
+    // An error answers the request it belongs to.
+    let stranger = request::Body::ContactAccept(ContactAccept {
+        user_id: id_bytes(Uuid::now_v7()),
+    });
+    let Body::Error(refused) = request(&mut alice_socket, 4, stranger).await else {
+        panic!("accepting a stranger succeeded");
+    };
+    assert_eq!(refused.code(), ErrorCode::NoPendingRequest);
+
+    let group = request::Body::CreateGroup(CreateGroup {
+        title: "Friends".to_owned(),
+        member_ids: vec![id_bytes(bob)],
+    });
+    let Body::Error(refused) = request(&mut alice_socket, 5, group).await else {
+        panic!("a group was created");
+    };
+    assert_eq!(refused.code(), ErrorCode::NotAvailable);
+    let list = request::Body::ListConversations(ListConversations::default());
+    let Body::Response(v1::Response {
+        body: Some(response::Body::Conversations(listed)),
+        ..
+    }) = request(&mut alice_socket, 6, list).await
+    else {
+        panic!("listing failed");
+    };
+    assert_eq!(
+        listed.conversations.len(),
+        1,
+        "only the direct conversation"
+    );
+}
+
+/// A message over the 128 KiB limit ends the connection with 1009 (message too big).
+#[tokio::test]
+async fn an_oversized_frame_is_closed_with_1009() {
+    let stack = stack().await;
+    let (user, device) = person(&stack).await;
+    let mut socket = ready(&stack, user, device).await;
+    socket
+        .send(Message::Binary(vec![0u8; MAX_FRAME_BYTES + 1].into()))
+        .await
+        .unwrap();
+    assert_eq!(close_code(&mut socket).await, 1009);
+}
+
 /// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
 /// messages from Alice in it, sent through message-svc before anyone connects.
 async fn chat_with_backlog(stack: &Stack, count: u32) -> ((Uuid, Uuid), (Uuid, Uuid), Vec<u8>) {
```

Change `crates/push-smoke/tests/local.rs`:

```diff
diff --git a/crates/push-smoke/tests/local.rs b/crates/push-smoke/tests/local.rs
index 5275b25..9b3115d 100644
--- a/crates/push-smoke/tests/local.rs
+++ b/crates/push-smoke/tests/local.rs
@@ -130,6 +130,7 @@ async fn the_smoke_test_passes_against_the_whole_stack() {
             listen_addr: listener.local_addr().unwrap(),
             health_addr: gateway_health.local_addr().unwrap(),
             heartbeat: Duration::from_secs(30),
+            groups: false,
         },
         listener,
         gateway_health,
```
- [ ] **Step 2: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
pwsh deploy/helm/test-chart.ps1
git add -A README.md crates deploy
git commit -m "feat(gateway): Edit, Delete, ReadUpTo and requests to message-svc; group requests behind one switch; 1009 for oversized messages"
```

Expected: 849 tests pass, no warnings, no diff, `all chart checks passed`.
---

### Task 9: Rate limits

**Files:**
- Create: `crates/push-gateway/src/limits.rs`
- Modify: `Cargo.lock`, `README.md`, `crates/push-domain/src/invites.rs`, `crates/push-gateway/Cargo.toml`, `crates/push-gateway/src/config.rs`, `crates/push-gateway/src/lib.rs`, `crates/push-gateway/src/server.rs`, `crates/push-gateway/tests/gateway.rs`, `crates/push-smoke/tests/local.rs`, `deploy/helm/push-messenger/templates/gateway.yaml`, `deploy/helm/test-chart.ps1`

Decision 9 and owner decision 2. Fixed windows in Redis, keyed by a SHA-256 of the subject:
`RedeemInvite` 20 an hour per user and 60 per IP, `ContactRequest` 30 per user and 120 per
IP, `CreateInvite` 20 per user, failing closed (`UNAVAILABLE` after 500 ms) with
`RATE_LIMITED` and `retry_after_s` over a limit; `Identify` 30 a minute per device, failing
open; a frame budget of 100 at once and 20 a second per connection, 4004 beyond it. The
budget's documentation records the Plan 9 rule that the SDK acks only the highest `seq`
during a backlog (one `Ack` per `Sync` batch). The client IP comes from
`PUSH_CLIENT_IP_HEADER`, which the chart sets to `x-real-ip` (ingress-nginx overwrites it
with the client's address).

- [ ] **Step 1: Make the changes**

Change `Cargo.lock`:

```diff
diff --git a/Cargo.lock b/Cargo.lock
index 4b0c7df..93e3773 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -2098,6 +2098,7 @@ name = "push-gateway"
 version = "0.1.0"
 dependencies = [
  "anyhow",
+ "async-trait",
  "axum",
  "futures-util",
  "jsonwebtoken",
@@ -2110,6 +2111,7 @@ dependencies = [
  "reqwest",
  "serde",
  "serde_json",
+ "sha2",
  "testcontainers-modules",
  "thiserror",
  "tokio",
```

Change `README.md`:

```diff
diff --git a/README.md b/README.md
index c20a888..1ddc5df 100644
--- a/README.md
+++ b/README.md
@@ -97,6 +97,7 @@ at start-up.
 | `PUSH_TOKEN_AUDIENCE` | the gateway's public URL (`wss://…/v1/socket`), the `aud` access tokens must name | required |
 | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready`, 1000 to 600000 | `30000` |
 | `PUSH_GROUPS` | `1` serves group requests; `0` refuses them with `NOT_AVAILABLE` (groups are not offered to apps in the first release; message-svc keeps them) | `0` |
+| `PUSH_CLIENT_IP_HEADER` | header with the client's IP, set by the ingress (`x-real-ip`), for the per-IP limits on redeeming invites and contact requests; same trust assumption as auth-svc's | the peer address |
 
 `push-auth`:
 
```

Change `crates/push-domain/src/invites.rs`:

```diff
diff --git a/crates/push-domain/src/invites.rs b/crates/push-domain/src/invites.rs
index 942dfab..178febc 100644
--- a/crates/push-domain/src/invites.rs
+++ b/crates/push-domain/src/invites.rs
@@ -1,8 +1,9 @@
 //! Invite links and QR codes (spec §5.5).
 //!
 //! A code is ten characters of Crockford base32: 50 random bits. That is plenty because
-//! redeeming is rate limited per account and per IP at the transport layer (spec §5.6):
-//! guessing one live code among a million would take about a billion attempts.
+//! the gateway limits `RedeemInvite` to 20 an hour per user and 60 per IP, in Redis
+//! across every gateway, and refuses it while Redis cannot count (spec §5.5): guessing one
+//! live code among a million would take about a billion attempts.
 
 use chrono::TimeDelta;
 use uuid::Uuid;
```

Change `crates/push-gateway/Cargo.toml`:

```diff
diff --git a/crates/push-gateway/Cargo.toml b/crates/push-gateway/Cargo.toml
index f802bd8..97d7559 100644
--- a/crates/push-gateway/Cargo.toml
+++ b/crates/push-gateway/Cargo.toml
@@ -10,6 +10,7 @@ repository.workspace = true
 
 [dependencies]
 anyhow.workspace = true
+async-trait.workspace = true
 axum = { workspace = true, features = ["ws"] }
 jsonwebtoken.workspace = true
 prost.workspace = true
@@ -18,6 +19,7 @@ redis.workspace = true
 reqwest.workspace = true
 serde.workspace = true
 serde_json.workspace = true
+sha2.workspace = true
 thiserror.workspace = true
 tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
 tokio-stream.workspace = true
```

Change `crates/push-gateway/src/config.rs`:

```diff
diff --git a/crates/push-gateway/src/config.rs b/crates/push-gateway/src/config.rs
index e39117d..8bd0fdd 100644
--- a/crates/push-gateway/src/config.rs
+++ b/crates/push-gateway/src/config.rs
@@ -3,6 +3,8 @@
 use std::net::SocketAddr;
 use std::time::Duration;
 
+use axum::http::HeaderName;
+
 /// How the gateway runs. Read from the environment by [`Config::from_env`]:
 ///
 /// | Variable | Meaning | Default |
@@ -15,6 +17,7 @@ use std::time::Duration;
 /// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
 /// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
 /// | `PUSH_GROUPS` | `1` serves group requests; `0` refuses them with `NOT_AVAILABLE` (groups are not offered to apps in the first release) | `0` |
+/// | `PUSH_CLIENT_IP_HEADER` | header with the client's IP, set by the ingress (`x-real-ip`); its last line, then last entry is used | the peer address |
 ///
 /// `PUSH_DEV_AUTH=1` (the DEV ONLY identities without a token, removed in Plan 8) is
 /// refused, so a deployment that still expects it fails at start-up.
@@ -29,6 +32,9 @@ pub struct Config {
     pub heartbeat: Duration,
     /// Whether group requests are served.
     pub groups: bool,
+    /// Where the ingress puts the client's IP, for the per-IP rate limits; without it the
+    /// peer address counts. Only trust a header the proxy in front overwrites.
+    pub client_ip_header: Option<HeaderName>,
 }
 
 // The Redis URL carries a password, so it never appears in debug output.
@@ -44,6 +50,7 @@ impl std::fmt::Debug for Config {
             .field("health_addr", &self.health_addr)
             .field("heartbeat", &self.heartbeat)
             .field("groups", &self.groups)
+            .field("client_ip_header", &self.client_ip_header)
             .finish()
     }
 }
@@ -93,6 +100,17 @@ impl Config {
                 });
             }
         }
+        let client_ip_header =
+            match lookup("PUSH_CLIENT_IP_HEADER").filter(|v| !v.is_empty()) {
+                None => None,
+                Some(value) => Some(HeaderName::try_from(value.as_str()).map_err(|_| {
+                    ConfigError::Invalid {
+                        name: "PUSH_CLIENT_IP_HEADER",
+                        value: value.clone(),
+                        reason: "not a header name".to_owned(),
+                    }
+                })?),
+            };
         let auth_url = required("PUSH_AUTH_URL")?;
         if !(auth_url.starts_with("http://") || auth_url.starts_with("https://")) {
             return Err(ConfigError::Invalid {
@@ -110,6 +128,7 @@ impl Config {
             health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
             heartbeat: Duration::from_millis(heartbeat_ms),
             groups: flag(&lookup, "PUSH_GROUPS")?,
+            client_ip_header,
         })
     }
 
@@ -185,6 +204,20 @@ mod tests {
         assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
         assert_eq!(config.heartbeat, Duration::from_secs(30));
         assert!(!config.groups, "groups are off unless switched on");
+        assert_eq!(config.client_ip_header, None);
+    }
+
+    #[test]
+    fn the_client_ip_header_is_a_header_name() {
+        let mut variables = REQUIRED.to_vec();
+        variables.push(("PUSH_CLIENT_IP_HEADER", "X-Real-IP"));
+        assert_eq!(
+            config(&variables).unwrap().client_ip_header,
+            Some(HeaderName::from_static("x-real-ip"))
+        );
+        variables.pop();
+        variables.push(("PUSH_CLIENT_IP_HEADER", "not a header"));
+        assert!(config(&variables).is_err());
     }
 
     #[test]
```

Change `crates/push-gateway/src/lib.rs`:

```diff
diff --git a/crates/push-gateway/src/lib.rs b/crates/push-gateway/src/lib.rs
index f7ca353..cff971e 100644
--- a/crates/push-gateway/src/lib.rs
+++ b/crates/push-gateway/src/lib.rs
@@ -4,12 +4,16 @@
 
 mod config;
 mod hub;
+mod limits;
 mod server;
 mod session;
 mod tokens;
 
 pub use config::{Config, ConfigError};
 pub use hub::{Hub, Membership, Signal};
+pub use limits::{
+    Action, Counter, FrameBudget, Limit, Limits, Outcome, RedisCounter, Refused, Scope, Subject,
+};
 pub use server::{init_logging, serve, terminated};
 pub use session::{RetryDelay, Session, Step};
 pub use tokens::{Claims, LEEWAY, REFETCH_INTERVAL, TokenError, Verifier};
```

Create `crates/push-gateway/src/limits.rs`:

```rust
//! Rate limits of the gateway: the requests that let someone guess or probe (redeeming
//! invites, contact requests, creating invites), `Identify` per device (spec §7.3), and
//! frames per connection.
//!
//! Request limits are fixed windows in Redis (`INCR`, then `EXPIRE … NX`), keyed by a
//! hash of the subject, so no user id or IP is stored in Redis; they count across every
//! gateway. When Redis does not answer within 500 ms the request is refused with a
//! retryable `UNAVAILABLE` and a warning (fail closed, like auth-svc): without counters
//! nothing would stop someone guessing invite codes. `Identify` fails open instead:
//! signed-in devices must keep connecting while Redis is down (spec §12, test 4), and a
//! connection storm is bounded by the frame budget and the token checks anyway.
//!
//! The frame budget is per connection, in memory: a token bucket of 100 frames refilled
//! at 20 a second. A connection that runs dry is closed with 4004.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use push_proto::v1::request;
use redis::aio::ConnectionManager;
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// What is being limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// `RedeemInvite`: guessing codes.
    RedeemInvite,
    /// `ContactRequest`: probing user ids, or flooding users with requests.
    ContactRequest,
    /// `CreateInvite`: filling the invite table.
    CreateInvite,
    /// `Identify`, per device: reconnect loops.
    Identify,
}

/// Whom a limit counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The client's IP (an IPv6 /64).
    Ip,
    User,
    Device,
}

/// At most `max` per `window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max: u32,
    pub window: Duration,
}

const fn per(max: u32, seconds: u64) -> Limit {
    Limit {
        max,
        window: Duration::from_secs(seconds),
    }
}

const HOUR: u64 = 3600;
const MINUTE: u64 = 60;

impl Action {
    /// The action a request is limited as, if any.
    pub fn of(body: &request::Body) -> Option<Self> {
        match body {
            request::Body::RedeemInvite(_) => Some(Self::RedeemInvite),
            request::Body::ContactRequest(_) => Some(Self::ContactRequest),
            request::Body::CreateInvite(_) => Some(Self::CreateInvite),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::RedeemInvite => "redeem",
            Self::ContactRequest => "contact",
            Self::CreateInvite => "invite",
            Self::Identify => "identify",
        }
    }

    /// The limits of the action, in the order they are counted. A request refused by one
    /// is not counted by the later ones.
    pub fn limits(self) -> &'static [(Scope, Limit)] {
        // 50-bit codes: 20 guesses an hour per user and 60 per address.
        const REDEEM: &[(Scope, Limit)] =
            &[(Scope::Ip, per(60, HOUR)), (Scope::User, per(20, HOUR))];
        const CONTACT: &[(Scope, Limit)] =
            &[(Scope::Ip, per(120, HOUR)), (Scope::User, per(30, HOUR))];
        const INVITE: &[(Scope, Limit)] = &[(Scope::User, per(20, HOUR))];
        const IDENTIFY: &[(Scope, Limit)] = &[(Scope::Device, per(30, MINUTE))];
        match self {
            Self::RedeemInvite => REDEEM,
            Self::ContactRequest => CONTACT,
            Self::CreateInvite => INVITE,
            Self::Identify => IDENTIFY,
        }
    }
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::Ip => "ip",
            Self::User => "user",
            Self::Device => "device",
        }
    }
}

/// The answer of a counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allowed,
    Limited { retry_after_s: u32 },
}

/// A store of fixed-window counters.
#[async_trait]
pub trait Counter: Send + Sync {
    /// Counts one hit against `key`; whether it is within `limit`.
    async fn hit(&self, key: &str, limit: Limit) -> Result<Outcome, String>;
}

/// Why a limit refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Over the limit: retry after this many seconds.
    Limited { retry_after_s: u32 },
    /// The counters could not be reached.
    Unavailable,
}

/// The gateway's rate limits over one counter store.
#[derive(Clone)]
pub struct Limits {
    counter: Arc<dyn Counter>,
}

impl std::fmt::Debug for Limits {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Limits")
    }
}

/// Who did something: the subjects an action may be counted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Subject {
    pub ip: Option<IpAddr>,
    pub user: Option<Uuid>,
    pub device: Option<Uuid>,
}

impl Limits {
    pub fn new(counter: Arc<dyn Counter>) -> Self {
        Self { counter }
    }

    /// Counts `action` by `subject`. A failing counter is `Unavailable`; the caller
    /// decides whether that refuses.
    pub async fn check(&self, action: Action, subject: Subject) -> Result<(), Refused> {
        for &(scope, limit) in action.limits() {
            let id = match scope {
                Scope::Ip => subject.ip.map(ip_subject),
                Scope::User => subject.user.map(|user| user.to_string()),
                Scope::Device => subject.device.map(|device| device.to_string()),
            };
            let Some(id) = id else {
                continue;
            };
            match self.counter.hit(&key(action, scope, &id), limit).await {
                Ok(Outcome::Allowed) => {}
                Ok(Outcome::Limited { retry_after_s }) => {
                    return Err(Refused::Limited { retry_after_s });
                }
                Err(error) => {
                    tracing::warn!(%error, action = action.name(), "rate limit not checked");
                    return Err(Refused::Unavailable);
                }
            }
        }
        Ok(())
    }
}

/// What an IP is counted as: an IPv4 address whole, also when mapped into IPv6; an IPv6
/// address by its /64 prefix, since one subscriber usually gets a whole /64.
fn ip_subject(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => {
            let segments = address.segments();
            format!(
                "{:x}:{:x}:{:x}:{:x}::/64",
                segments[0], segments[1], segments[2], segments[3]
            )
        }
    }
}

/// `gw:{action}:{scope}:{hash}`: the subject is hashed, never stored.
fn key(action: Action, scope: Scope, subject: &str) -> String {
    let digest = Sha256::digest(subject.as_bytes());
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("gw:{}:{}:{hex}", action.name(), scope.name())
}

/// How long a counter may take before the request is decided without it.
pub const REDIS_TIMEOUT: Duration = Duration::from_millis(500);

/// Counters in Redis, shared by every gateway.
#[derive(Clone)]
pub struct RedisCounter {
    connection: ConnectionManager,
    shutdown: CancellationToken,
}

impl RedisCounter {
    /// Counters on `connection`; calls give up when `shutdown` is cancelled.
    pub fn new(connection: ConnectionManager, shutdown: CancellationToken) -> Self {
        Self {
            connection,
            shutdown,
        }
    }
}

#[async_trait]
impl Counter for RedisCounter {
    async fn hit(&self, key: &str, limit: Limit) -> Result<Outcome, String> {
        let window = limit.window.as_secs().max(1);
        let mut pipeline = redis::pipe();
        pipeline
            .atomic()
            .incr(key, 1)
            .cmd("EXPIRE")
            .arg(key)
            .arg(window)
            .arg("NX")
            .ignore()
            .ttl(key);
        let mut connection = self.connection.clone();
        let call = pipeline.query_async::<(u64, i64)>(&mut connection);
        let (count, ttl) = tokio::select! {
            () = self.shutdown.cancelled() => return Err("shutting down".to_owned()),
            result = tokio::time::timeout(REDIS_TIMEOUT, call) => result
                .map_err(|_| "redis did not answer in time".to_owned())?
                .map_err(|error| format!("{:?}", error.kind()))?,
        };
        if count <= u64::from(limit.max) {
            Ok(Outcome::Allowed)
        } else {
            Ok(Outcome::Limited {
                retry_after_s: u32::try_from(ttl.max(1)).unwrap_or(u32::MAX),
            })
        }
    }
}

/// A connection's frame budget: a token bucket.
///
/// A backlog costs one `Ack` per `Sync` batch (up to 500 entries) when the client acks only
/// the highest seq it holds, as the SDK must (Plan 9): 10,000 waiting entries are 20 acks,
/// far inside the burst, and the next batch comes only after the ack's round trip. A client
/// that acked every `Deliver` of a backlog would spend 500 frames a batch and be closed
/// with 4004.
#[derive(Debug, Clone)]
pub struct FrameBudget {
    tokens: f64,
    updated: Instant,
}

impl FrameBudget {
    /// Frames a connection may send at once.
    pub const BURST: f64 = 100.0;
    /// Frames a second a connection may keep sending.
    pub const PER_SECOND: f64 = 20.0;

    pub fn new() -> Self {
        Self {
            tokens: Self::BURST,
            updated: Instant::now(),
        }
    }

    /// Spends one frame; false when the budget is exhausted.
    pub fn spend(&mut self) -> bool {
        let now = Instant::now();
        let refill = now.duration_since(self.updated).as_secs_f64() * Self::PER_SECOND;
        self.tokens = (self.tokens + refill).min(Self::BURST);
        self.updated = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

impl Default for FrameBudget {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use push_proto::v1::{ContactRequest, CreateInvite, ListContacts, RedeemInvite};

    use super::*;

    /// Counters in this process.
    #[derive(Debug, Default)]
    struct MemoryCounter {
        counts: std::sync::Mutex<HashMap<String, (u32, Instant)>>,
    }

    #[async_trait]
    impl Counter for MemoryCounter {
        async fn hit(&self, key: &str, limit: Limit) -> Result<Outcome, String> {
            let now = Instant::now();
            let mut counts = self.counts.lock().expect("not poisoned");
            let (count, ends) = counts
                .entry(key.to_owned())
                .or_insert((0, now + limit.window));
            if now >= *ends {
                *count = 0;
                *ends = now + limit.window;
            }
            *count += 1;
            if *count <= limit.max {
                Ok(Outcome::Allowed)
            } else {
                let left = ends.saturating_duration_since(now).as_secs().max(1);
                Ok(Outcome::Limited {
                    retry_after_s: u32::try_from(left).unwrap_or(u32::MAX),
                })
            }
        }
    }

    struct Broken;

    #[async_trait]
    impl Counter for Broken {
        async fn hit(&self, _key: &str, _limit: Limit) -> Result<Outcome, String> {
            Err("connection refused".to_owned())
        }
    }

    fn memory() -> Limits {
        Limits::new(Arc::new(MemoryCounter::default()))
    }

    fn from(ip: u8, user: Uuid) -> Subject {
        Subject {
            ip: Some(IpAddr::from([10, 0, 0, ip])),
            user: Some(user),
            device: None,
        }
    }

    #[test]
    fn only_guessing_and_probing_requests_are_limited() {
        let redeem = request::Body::RedeemInvite(RedeemInvite::default());
        let ask = request::Body::ContactRequest(ContactRequest::default());
        let invite = request::Body::CreateInvite(CreateInvite::default());
        let list = request::Body::ListContacts(ListContacts {});
        assert_eq!(Action::of(&redeem), Some(Action::RedeemInvite));
        assert_eq!(Action::of(&ask), Some(Action::ContactRequest));
        assert_eq!(Action::of(&invite), Some(Action::CreateInvite));
        assert_eq!(Action::of(&list), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_user_is_limited_across_addresses_and_an_address_across_users() {
        let limits = memory();
        let alice = Uuid::now_v7();
        for attempt in 0..20 {
            limits
                .check(Action::RedeemInvite, from(attempt, alice))
                .await
                .unwrap();
        }
        let refused = limits.check(Action::RedeemInvite, from(99, alice)).await;
        assert!(
            matches!(refused, Err(Refused::Limited { retry_after_s }) if retry_after_s > 3000),
            "{refused:?}"
        );
        // One address, many users: 60 an hour.
        for _ in 0..60 {
            let user = Uuid::now_v7();
            limits
                .check(Action::RedeemInvite, from(200, user))
                .await
                .unwrap();
        }
        assert!(
            limits
                .check(Action::RedeemInvite, from(200, Uuid::now_v7()))
                .await
                .is_err()
        );
        // The window ends.
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(
            limits
                .check(Action::RedeemInvite, from(99, alice))
                .await
                .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn identify_is_counted_per_device() {
        let limits = memory();
        let device = Subject {
            device: Some(Uuid::now_v7()),
            ..Subject::default()
        };
        for _ in 0..30 {
            limits.check(Action::Identify, device).await.unwrap();
        }
        assert!(matches!(
            limits.check(Action::Identify, device).await,
            Err(Refused::Limited { retry_after_s }) if retry_after_s <= 60
        ));
    }

    #[tokio::test]
    async fn a_failing_counter_is_unavailable() {
        let limits = Limits::new(Arc::new(Broken));
        assert_eq!(
            limits
                .check(Action::ContactRequest, from(1, Uuid::now_v7()))
                .await,
            Err(Refused::Unavailable)
        );
    }

    #[test]
    fn keys_hash_the_subject() {
        let user = Uuid::now_v7().to_string();
        let key = key(Action::ContactRequest, Scope::User, &user);
        assert!(key.starts_with("gw:contact:user:"), "{key}");
        assert!(!key.contains(&user), "{key}");
        assert_eq!(
            ip_subject("::ffff:10.0.0.1".parse().unwrap()),
            ip_subject("10.0.0.1".parse().unwrap())
        );
        assert_eq!(
            ip_subject("2001:db8:1:2:3:4:5:6".parse().unwrap()),
            "2001:db8:1:2::/64"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_frame_budget_allows_bursts_and_a_steady_rate() {
        let mut budget = FrameBudget::new();
        for _ in 0..100 {
            assert!(budget.spend());
        }
        assert!(!budget.spend(), "the burst is used up");
        tokio::time::advance(Duration::from_millis(500)).await;
        for _ in 0..10 {
            assert!(budget.spend(), "20 a second come back");
        }
        assert!(!budget.spend());
    }
}
```

Change `crates/push-gateway/src/server.rs`:

```diff
diff --git a/crates/push-gateway/src/server.rs b/crates/push-gateway/src/server.rs
index 73b77f3..b3ea2b0 100644
--- a/crates/push-gateway/src/server.rs
+++ b/crates/push-gateway/src/server.rs
@@ -1,13 +1,14 @@
 //! Running the gateway: the WebSocket endpoint, one task per connection, the token keys,
 //! the health endpoints and graceful shutdown.
 
+use std::net::{IpAddr, SocketAddr};
 use std::sync::Arc;
 use std::time::{Duration, SystemTime, UNIX_EPOCH};
 
 use axum::Router;
-use axum::extract::State;
 use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
-use axum::http::StatusCode;
+use axum::extract::{ConnectInfo, State};
+use axum::http::{HeaderMap, StatusCode};
 use axum::response::Response;
 use axum::routing::get;
 use prost::Message as _;
@@ -29,6 +30,7 @@ use uuid::Uuid;
 
 use crate::config::Config;
 use crate::hub::{Hub, Membership, Signal};
+use crate::limits::{Action, FrameBudget, Limits, RedisCounter, Refused, Subject};
 use crate::session::{RetryDelay, Session, Step, error_frame};
 use crate::tokens::{Claims, TokenError, Verifier};
 
@@ -56,6 +58,7 @@ struct App {
     hub: Hub,
     /// Publishes takeovers on `kick:{device_id}`.
     redis: ConnectionManager,
+    limits: Limits,
     sessions: TaskTracker,
     shutdown: CancellationToken,
 }
@@ -95,6 +98,10 @@ pub async fn serve(
         messaging,
         verifier: verifier.clone(),
         hub: hub.clone(),
+        limits: Limits::new(Arc::new(RedisCounter::new(
+            publisher.clone(),
+            shutdown.clone(),
+        ))),
         redis: publisher,
         sessions: sessions.clone(),
         shutdown: shutdown.clone(),
@@ -109,9 +116,12 @@ pub async fn serve(
         .route("/v1/socket", get(upgrade))
         .with_state(app);
     tracing::info!(addr = %listen_addr, "serving WebSockets");
-    axum::serve(listener, router)
-        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
-        .await?;
+    axum::serve(
+        listener,
+        router.into_make_service_with_connect_info::<SocketAddr>(),
+    )
+    .with_graceful_shutdown(shutdown.clone().cancelled_owned())
+    .await?;
 
     // Upgraded connections outlive the HTTP server; each closes itself with 4005.
     sessions.close();
@@ -183,16 +193,37 @@ fn health_router(verifier: Verifier, shutdown: CancellationToken) -> Router {
 /// its payload is buffered, and the client may only see the connection drop.
 const MAX_WEBSOCKET_FRAME: usize = 4 * MAX_FRAME_BYTES;
 
-async fn upgrade(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response {
+async fn upgrade(
+    upgrade: WebSocketUpgrade,
+    State(app): State<App>,
+    ConnectInfo(peer): ConnectInfo<SocketAddr>,
+    headers: HeaderMap,
+) -> Response {
+    let ip = client_ip(&headers, app.config.client_ip_header.as_ref(), peer);
     let sessions = app.sessions.clone();
     upgrade
         .max_message_size(MAX_FRAME_BYTES)
         .max_frame_size(MAX_WEBSOCKET_FRAME)
-        .on_upgrade(move |socket| sessions.track_future(connection(socket, app)))
+        .on_upgrade(move |socket| sessions.track_future(connection(socket, app, ip)))
+}
+
+/// The client's IP: the last entry of the last line of the configured header, which the
+/// proxy in front overwrites or appends to (`X-Real-IP` on ingress-nginx); else the peer.
+fn client_ip(
+    headers: &HeaderMap,
+    header: Option<&axum::http::HeaderName>,
+    peer: SocketAddr,
+) -> IpAddr {
+    header
+        .and_then(|name| headers.get_all(name).iter().next_back())
+        .and_then(|value| value.to_str().ok())
+        .and_then(|value| value.rsplit(',').next())
+        .and_then(|last| last.trim().parse().ok())
+        .unwrap_or(peer.ip())
 }
 
 /// One client connection, from `Identify` to close.
-async fn connection(mut socket: WebSocket, app: App) {
+async fn connection(mut socket: WebSocket, app: App, ip: IpAddr) {
     let mut session = Session::new(app.config.groups);
     let Some((claims, mut membership)) = identify(&mut socket, &mut session, &app).await else {
         return;
@@ -207,6 +238,9 @@ async fn connection(mut socket: WebSocket, app: App) {
         session,
         messaging: app.messaging.clone(),
         verifier: app.verifier.clone(),
+        limits: app.limits.clone(),
+        ip,
+        budget: FrameBudget::new(),
         user: claims.user,
         device: claims.device,
         expiry: Expiry::new(claims.expires_at),
@@ -260,6 +294,13 @@ async fn connection(mut socket: WebSocket, app: App) {
                 return;
             }
             message = connected.socket.recv() => {
+                if matches!(message, Some(Ok(Message::Binary(_) | Message::Text(_))))
+                    && !connected.budget.spend()
+                {
+                    tracing::info!(device = %connected.device, "too many frames");
+                    close_with(&mut connected.socket, close::RATE_LIMITED, "too many frames; slow down").await;
+                    return;
+                }
                 let step = match message {
                     Some(Ok(Message::Binary(bytes))) => connected.session.on_binary(&bytes),
                     Some(Ok(Message::Text(_))) => connected.session.on_text(),
@@ -355,6 +396,20 @@ async fn identify(
             return None;
         }
     };
+    let device = Subject {
+        device: Some(claims.device),
+        ..Subject::default()
+    };
+    // Fails open: devices keep connecting while Redis is down.
+    if let Err(Refused::Limited { .. }) = app.limits.check(Action::Identify, device).await {
+        close_with(
+            socket,
+            close::RATE_LIMITED,
+            "too many connections; back off",
+        )
+        .await;
+        return None;
+    }
     // Taken now, not when the socket opened: a connection that starts watching later is the
     // newer one, so its takeover wins even if it opened first and its token check stalled.
     let connection_id = Uuid::now_v7();
@@ -386,6 +441,31 @@ async fn verify(verifier: &Verifier, token: &str) -> Result<Claims, TokenError>
     Ok(claims)
 }
 
+/// The error for a request a rate limit refused: retryable either way, after the window
+/// ends or once the counters answer again (fail closed).
+fn limited(refused: Refused, request_id: u64) -> Error {
+    let (code, message, retry_after_s) = match refused {
+        Refused::Limited { retry_after_s } => (
+            ErrorCode::RateLimited,
+            "too many requests of this kind; retry later",
+            retry_after_s,
+        ),
+        Refused::Unavailable => (
+            ErrorCode::Unavailable,
+            "the rate limits cannot be checked now; retry",
+            1,
+        ),
+    };
+    Error {
+        code: code.into(),
+        message: message.to_owned(),
+        retryable: true,
+        client_msg_id: None,
+        request_id: Some(request_id),
+        retry_after_s: Some(retry_after_s),
+    }
+}
+
 /// Tells any other connection of the device, on any gateway, to close: the newest
 /// connection wins (spec §7.3). Best effort: without Redis two connections may coexist
 /// for a while, which the client tolerates.
@@ -532,6 +612,9 @@ struct Connected {
     session: Session,
     messaging: MessagingClient<Channel>,
     verifier: Verifier,
+    limits: Limits,
+    ip: IpAddr,
+    budget: FrameBudget,
     user: Uuid,
     device: Uuid,
     expiry: Expiry,
@@ -618,6 +701,17 @@ impl Connected {
             }
             Step::Request(request) => {
                 let request_id = request.request_id;
+                if let Some(action) = request.body.as_ref().and_then(Action::of) {
+                    let who = Subject {
+                        ip: Some(self.ip),
+                        user: Some(self.user),
+                        device: None,
+                    };
+                    if let Err(refused) = self.limits.check(action, who).await {
+                        let error = limited(refused, request_id);
+                        return send(&mut self.socket, Body::Error(error)).await;
+                    }
+                }
                 let call = RequestCall {
                     user_id: id_bytes(self.user),
                     request: Some(request),
```

Change `deploy/helm/push-messenger/templates/gateway.yaml`:

```diff
diff --git a/deploy/helm/push-messenger/templates/gateway.yaml b/deploy/helm/push-messenger/templates/gateway.yaml
index 6b78577..f32e59b 100644
--- a/deploy/helm/push-messenger/templates/gateway.yaml
+++ b/deploy/helm/push-messenger/templates/gateway.yaml
@@ -65,6 +65,10 @@ spec:
               value: {{ printf "http://%s:8080" (include "push.name" (dict "root" . "component" "auth")) | quote }}
             - name: PUSH_TOKEN_AUDIENCE
               value: {{ include "push.socketUrl" . | quote }}
+            # ingress-nginx overwrites X-Real-IP with the client's address: the per-IP
+            # limits on redeeming invites and contact requests count by it.
+            - name: PUSH_CLIENT_IP_HEADER
+              value: x-real-ip
             - name: PUSH_GROUPS
               value: {{ ternary "1" "0" (eq (toString .Values.gateway.groups) "true") | quote }}
           ports:
```

Change `deploy/helm/test-chart.ps1`:

```diff
diff --git a/deploy/helm/test-chart.ps1 b/deploy/helm/test-chart.ps1
index 4d74f2e..6750019 100644
--- a/deploy/helm/test-chart.ps1
+++ b/deploy/helm/test-chart.ps1
@@ -37,6 +37,7 @@ Check 'default: the gateway checks tokens with auth-svc''s keys, for its own URL
     $gateway = Doc $default 'Deployment' 'push-gateway'
     $gateway -match 'name: PUSH_AUTH_URL\s+value: "http://push-auth:8080"' -and
         $gateway -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "wss://push.example.com/v1/socket"' -and
+        $gateway -match 'name: PUSH_CLIENT_IP_HEADER\s+value: x-real-ip' -and
         $default -notmatch 'PUSH_DEV_AUTH'
 }
 Check 'default: groups are refused; one switch serves them' {
```

Change `crates/push-gateway/tests/gateway.rs`:

```diff
diff --git a/crates/push-gateway/tests/gateway.rs b/crates/push-gateway/tests/gateway.rs
index 2ef5c92..ecc647a 100644
--- a/crates/push-gateway/tests/gateway.rs
+++ b/crates/push-gateway/tests/gateway.rs
@@ -22,9 +22,9 @@ use push_proto::v1;
 use push_proto::v1::frame::Body;
 use push_proto::v1::{
     Ack, AuthExpiring, Contact, ContactAccept, ContactEvent, ContactRequest, ContactState,
-    ContentType, CreateGroup, Delete, DirectReceipt, Edit, ErrorCode, Identify, ListConversations,
-    OpenDirect, Ping, Pong, ReAuth, ReadPosition, ReadUpTo, Receipt, Send, SendAck, delete, edit,
-    receipt, request, response,
+    ContentType, CreateGroup, Delete, DirectReceipt, Edit, ErrorCode, Identify, ListContacts,
+    ListConversations, OpenDirect, Ping, Pong, ReAuth, ReadPosition, ReadUpTo, Receipt,
+    RedeemInvite, Send, SendAck, delete, edit, receipt, request, response,
 };
 use push_proto::{MAX_FRAME_BYTES, decode_frame, encode_frame, id, id_bytes};
 use testcontainers_modules::redis::Redis;
@@ -175,6 +175,7 @@ async fn stack_with(sync_failures: u32) -> Stack {
         health_addr: health.local_addr().unwrap(),
         heartbeat: Duration::from_secs(30),
         groups: false,
+        client_ip_header: None,
     };
     let shutdown = CancellationToken::new();
     let server = tokio::spawn(serve(config, listener, health, shutdown.clone()));
@@ -803,6 +804,51 @@ async fn requests_edits_deletes_and_reads_go_through_and_group_requests_are_refu
     );
 }
 
+/// Invite codes cannot be guessed at speed: after 20 redeems in an hour a user is told to
+/// wait, whatever the codes (spec §5.5).
+#[tokio::test]
+async fn redeeming_invites_is_rate_limited() {
+    let stack = stack().await;
+    let (user, device) = person(&stack).await;
+    let mut socket = ready(&stack, user, device).await;
+    for attempt in 1..=20 {
+        let guess = request::Body::RedeemInvite(RedeemInvite {
+            code: "0000000000".to_owned(),
+        });
+        let Body::Error(error) = request(&mut socket, attempt, guess).await else {
+            panic!("a made-up code was redeemed");
+        };
+        assert_eq!(error.code(), ErrorCode::InviteNotFound);
+    }
+    let guess = request::Body::RedeemInvite(RedeemInvite {
+        code: "0000000000".to_owned(),
+    });
+    let Body::Error(error) = request(&mut socket, 21, guess).await else {
+        panic!("a made-up code was redeemed");
+    };
+    assert_eq!(error.code(), ErrorCode::RateLimited);
+    assert!(error.retryable);
+    assert!(error.retry_after_s.is_some_and(|seconds| seconds > 3000));
+    // Other requests still work.
+    let list = request::Body::ListContacts(ListContacts {});
+    assert!(matches!(
+        request(&mut socket, 22, list).await,
+        Body::Response(_)
+    ));
+}
+
+/// A connection that floods the gateway with frames is closed with 4004.
+#[tokio::test]
+async fn too_many_frames_close_the_connection_with_4004() {
+    let stack = stack().await;
+    let (user, device) = person(&stack).await;
+    let mut socket = ready(&stack, user, device).await;
+    for _ in 0..150 {
+        send(&mut socket, Body::Ping(Ping {})).await;
+    }
+    assert_eq!(close_code(&mut socket).await, 4004);
+}
+
 /// A message over the 128 KiB limit ends the connection with 1009 (message too big).
 #[tokio::test]
 async fn an_oversized_frame_is_closed_with_1009() {
```

Change `crates/push-smoke/tests/local.rs`:

```diff
diff --git a/crates/push-smoke/tests/local.rs b/crates/push-smoke/tests/local.rs
index 9b3115d..c3c0a62 100644
--- a/crates/push-smoke/tests/local.rs
+++ b/crates/push-smoke/tests/local.rs
@@ -131,6 +131,7 @@ async fn the_smoke_test_passes_against_the_whole_stack() {
             health_addr: gateway_health.local_addr().unwrap(),
             heartbeat: Duration::from_secs(30),
             groups: false,
+            client_ip_header: None,
         },
         listener,
         gateway_health,
```
- [ ] **Step 2: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
pwsh deploy/helm/test-chart.ps1
git add -A Cargo.lock README.md crates deploy
git commit -m "feat(gateway): rate limits on redeeming invites, contact requests, invites, Identify and frames"
```

Expected: 858 tests pass, no warnings, no diff, `all chart checks passed`.
---

### Task 10: Timeouts, keepalives, a sweep and batched draining

**Files:**
- Create: `crates/push-gateway/src/drain.rs`
- Modify: `crates/push-gateway/src/hub.rs`, `crates/push-gateway/src/lib.rs`, `crates/push-gateway/src/server.rs`, `crates/push-gateway/tests/gateway.rs`

Decisions 12 and 13. Sending a frame may take 10 s; a client silent for two and a half
heartbeats is closed with 1008; the hub pings Redis every 30 s and reconnects without an
answer in 5 s; the gRPC channel keeps HTTP/2 and TCP alive; every connection syncs every 5
minutes (its first sweep spread by its id), a backlog larger than one batch is delivered as
the device acks and a failed sync is retried; shutdown closes connections with 4005 a
hundred every 50 ms.

- [ ] **Step 1: Make the changes**

Create `crates/push-gateway/src/drain.rs`:

```rust
//! Closing connections in batches when the gateway shuts down (spec §11), so the clients
//! it held do not all reconnect to the other gateways at the same instant.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Connections closed together.
const BATCH: usize = 100;
/// The pause between two batches.
const BATCH_INTERVAL: Duration = Duration::from_millis(50);
/// The last batch closes at most this long after shutdown began, well inside the 10 s
/// the gateway waits for its connections.
const SPREAD: Duration = Duration::from_secs(5);

/// Hands each closing connection its turn.
#[derive(Debug, Default)]
pub struct Drain {
    closing: AtomicUsize,
}

impl Drain {
    /// How long the next connection to close waits: 100 connections every 50 ms, at
    /// most 5 s.
    pub fn next_delay(&self) -> Duration {
        delay_for(self.closing.fetch_add(1, Ordering::Relaxed))
    }
}

fn delay_for(index: usize) -> Duration {
    let batch = u32::try_from(index / BATCH).unwrap_or(u32::MAX);
    BATCH_INTERVAL.saturating_mul(batch).min(SPREAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_close_a_hundred_at_a_time_within_five_seconds() {
        let drain = Drain::default();
        let delays: Vec<Duration> = (0..250).map(|_| drain.next_delay()).collect();
        assert!(delays[..100].iter().all(|delay| delay.is_zero()));
        assert!(
            delays[100..200]
                .iter()
                .all(|delay| *delay == Duration::from_millis(50))
        );
        assert_eq!(delays[249], Duration::from_millis(100));
        assert_eq!(delay_for(1_000_000), Duration::from_secs(5));
        assert_eq!(delay_for(usize::MAX), Duration::from_secs(5));
    }
}
```

Change `crates/push-gateway/src/hub.rs`:

```diff
diff --git a/crates/push-gateway/src/hub.rs b/crates/push-gateway/src/hub.rs
index 315ab94..624ff89 100644
--- a/crates/push-gateway/src/hub.rs
+++ b/crates/push-gateway/src/hub.rs
@@ -53,6 +53,9 @@ const SUBSCRIBE_WAIT: Duration = Duration::from_secs(2);
 const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
 /// How long to wait before reconnecting to Redis.
 const RECONNECT_DELAY: Duration = Duration::from_secs(1);
+/// How often the subscriber pings Redis: a connection that died without a word (a
+/// half-open TCP connection) would otherwise look idle forever.
+const PING_INTERVAL: Duration = Duration::from_secs(30);
 /// Server frames waiting for a busy connection; more are dropped (the client recovers
 /// them with `ListConversations` and `ListContacts`).
 const EVENT_BACKLOG: usize = 64;
@@ -104,13 +107,27 @@ pub enum Signal {
 impl Hub {
     /// A hub whose subscriber task listens on Redis through `client` until `shutdown`.
     pub fn start(client: redis::Client, shutdown: CancellationToken) -> (Self, JoinHandle<()>) {
+        Self::start_with(client, shutdown, PING_INTERVAL)
+    }
+
+    fn start_with(
+        client: redis::Client,
+        shutdown: CancellationToken,
+        ping_interval: Duration,
+    ) -> (Self, JoinHandle<()>) {
         let (commands, receiver) = mpsc::unbounded_channel();
         let hub = Self {
             members: Arc::default(),
             commands,
             connected: Arc::default(),
         };
-        let subscriber = tokio::spawn(subscribe(client, hub.clone(), receiver, shutdown));
+        let subscriber = tokio::spawn(subscribe(
+            client,
+            hub.clone(),
+            receiver,
+            shutdown,
+            ping_interval,
+        ));
         (hub, subscriber)
     }
 
@@ -409,7 +426,7 @@ async fn bounded(
         result = tokio::time::timeout(COMMAND_TIMEOUT, command) => match result {
             Ok(result) => Some(result.is_ok()),
             Err(_) => {
-                tracing::warn!("Redis did not answer a subscription command in time");
+                tracing::warn!("Redis did not answer the subscriber in time");
                 Some(false)
             }
         },
@@ -417,8 +434,9 @@ async fn bounded(
 }
 
 /// The subscriber task: keeps one Redis pub/sub connection, follows the hub's subscribe
-/// and unsubscribe commands, and dispatches what arrives. Reconnects after a break,
-/// subscribes again to every channel with a member and rings every connection.
+/// and unsubscribe commands, and dispatches what arrives. Pings Redis every
+/// `ping_interval` and reconnects when it does not answer within 5 s, as after any break:
+/// it subscribes again to every channel with a member and rings every connection.
 /// Connecting is retried while Redis is unavailable, and `shutdown` interrupts both the
 /// connection attempt and the wait between attempts at once.
 async fn subscribe(
@@ -426,6 +444,7 @@ async fn subscribe(
     hub: Hub,
     mut commands: mpsc::UnboundedReceiver<Command>,
     shutdown: CancellationToken,
+    ping_interval: Duration,
 ) {
     loop {
         let connection = tokio::select! {
@@ -450,9 +469,20 @@ async fn subscribe(
                     hub.connected.store(true, Ordering::SeqCst);
                     tracing::info!("subscribed to Redis");
                     hub.ring_all();
+                    let mut ping_at = tokio::time::Instant::now() + ping_interval;
                     loop {
                         tokio::select! {
                             () = shutdown.cancelled() => return,
+                            () = tokio::time::sleep_until(ping_at) => {
+                                let ping = async { sink.ping::<redis::Value>().await.map(|_| ()) };
+                                let Some(alive) = bounded(&shutdown, ping).await else {
+                                    return;
+                                };
+                                if !alive {
+                                    break;
+                                }
+                                ping_at = tokio::time::Instant::now() + ping_interval;
+                            }
                             command = commands.recv() => {
                                 let succeeded = match command {
                                     None => return,
@@ -772,6 +802,50 @@ mod tests {
         assert_eq!(pending(&mut session), None);
     }
 
+    /// A Redis that keeps the connection but stops answering, as behind a half-open TCP
+    /// connection, is given up after one unanswered PING; the hub connects again and
+    /// rings everyone, since notifications may have been lost meanwhile.
+    #[tokio::test]
+    async fn a_redis_that_stops_answering_is_left_and_everyone_rung() {
+        use testcontainers_modules::redis::Redis;
+        use testcontainers_modules::testcontainers::ImageExt;
+        use testcontainers_modules::testcontainers::runners::AsyncRunner;
+
+        let container = Redis::default()
+            .with_tag("7-alpine")
+            .start()
+            .await
+            .expect("start Redis (is Docker running?)");
+        let port = container.get_host_port_ipv4(6379).await.unwrap();
+        let client = redis::Client::open(format!("redis://127.0.0.1:{port}")).unwrap();
+        let shutdown = CancellationToken::new();
+        let (hub, subscriber) =
+            Hub::start_with(client.clone(), shutdown.clone(), Duration::from_millis(500));
+        for _ in 0..100 {
+            if hub.is_connected() {
+                break;
+            }
+            tokio::time::sleep(Duration::from_millis(50)).await;
+        }
+        assert!(hub.is_connected());
+        let (mut session, _, _) = connection(&hub, Uuid::now_v7()).await;
+
+        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
+        let () = redis::cmd("CLIENT")
+            .arg("PAUSE")
+            .arg(8000)
+            .arg("ALL")
+            .query_async(&mut admin)
+            .await
+            .unwrap();
+        let signal = tokio::time::timeout(Duration::from_secs(20), session.next())
+            .await
+            .expect("rung after the reconnect");
+        assert_eq!(signal, Signal::Ring);
+        shutdown.cancel();
+        subscriber.await.unwrap();
+    }
+
     #[tokio::test(start_paused = true)]
     async fn ring_all_wakes_every_connection() {
         let (hub, _commands) = hub();
```

Change `crates/push-gateway/src/lib.rs`:

```diff
diff --git a/crates/push-gateway/src/lib.rs b/crates/push-gateway/src/lib.rs
index cff971e..32e3515 100644
--- a/crates/push-gateway/src/lib.rs
+++ b/crates/push-gateway/src/lib.rs
@@ -3,6 +3,7 @@
 //! their user's channel. It holds no database credentials and no business rules.
 
 mod config;
+mod drain;
 mod hub;
 mod limits;
 mod server;
```

Change `crates/push-gateway/src/server.rs`:

```diff
diff --git a/crates/push-gateway/src/server.rs b/crates/push-gateway/src/server.rs
index b3ea2b0..0204243 100644
--- a/crates/push-gateway/src/server.rs
+++ b/crates/push-gateway/src/server.rs
@@ -29,6 +29,7 @@ use tonic::transport::{Channel, Endpoint};
 use uuid::Uuid;
 
 use crate::config::Config;
+use crate::drain::Drain;
 use crate::hub::{Hub, Membership, Signal};
 use crate::limits::{Action, FrameBudget, Limits, RedisCounter, Refused, Subject};
 use crate::session::{RetryDelay, Session, Step, error_frame};
@@ -49,6 +50,13 @@ const KEYS_REFRESH: Duration = Duration::from_secs(300);
 const KEYS_RETRY: Duration = Duration::from_secs(2);
 /// How long a Redis command (other than the subscriber's) may take.
 const REDIS_TIMEOUT: Duration = Duration::from_millis(500);
+/// How long sending one frame may take; a client that does not read is cut off.
+const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
+/// How long sending a close frame may take.
+const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
+/// Every connection syncs this often even without a ring, so entries arrive while Redis
+/// is lost or a notification was (spec §12, test 4).
+const SWEEP_INTERVAL: Duration = Duration::from_secs(300);
 
 #[derive(Clone)]
 struct App {
@@ -60,6 +68,7 @@ struct App {
     redis: ConnectionManager,
     limits: Limits,
     sessions: TaskTracker,
+    drain: Arc<Drain>,
     shutdown: CancellationToken,
 }
 
@@ -74,9 +83,15 @@ pub async fn serve(
     health: TcpListener,
     shutdown: CancellationToken,
 ) -> anyhow::Result<()> {
+    // HTTP/2 pings find a message-svc connection that died without a word (a half-open
+    // TCP connection), so calls do not wait for a peer that is gone.
     let endpoint = Endpoint::from_shared(config.message_svc_url.clone())?
         .connect_timeout(Duration::from_secs(2))
-        .timeout(Duration::from_secs(10));
+        .timeout(Duration::from_secs(10))
+        .http2_keep_alive_interval(Duration::from_secs(30))
+        .keep_alive_timeout(Duration::from_secs(10))
+        .keep_alive_while_idle(true)
+        .tcp_keepalive(Some(Duration::from_secs(60)));
     let messaging = MessagingClient::new(endpoint.connect_lazy());
     // The error is not passed on: the URL carries the Redis password.
     let redis = redis::Client::open(config.redis_url.as_str())
@@ -104,6 +119,7 @@ pub async fn serve(
         ))),
         redis: publisher,
         sessions: sessions.clone(),
+        drain: Arc::default(),
         shutdown: shutdown.clone(),
     };
 
@@ -123,7 +139,8 @@ pub async fn serve(
     .with_graceful_shutdown(shutdown.clone().cancelled_owned())
     .await?;
 
-    // Upgraded connections outlive the HTTP server; each closes itself with 4005.
+    // Upgraded connections outlive the HTTP server; each closes itself with 4005, a
+    // hundred every 50 ms.
     sessions.close();
     if tokio::time::timeout(DRAIN_TIMEOUT, sessions.wait())
         .await
@@ -251,6 +268,10 @@ async fn connection(mut socket: WebSocket, app: App, ip: IpAddr) {
     if send(&mut connected.socket, ready).await.is_err() || connected.deliver().await.is_err() {
         return;
     }
+    // Clients ping every heartbeat; one silent for two and a half is gone.
+    let idle = app.config.heartbeat * 5 / 2;
+    let mut idle_at = Instant::now() + idle;
+    let mut sweep_at = Instant::now() + first_sweep(membership.connection());
     loop {
         let retry_at = connected.retry_at;
         let (warn_at, warned, deadline) = (
@@ -260,9 +281,20 @@ async fn connection(mut socket: WebSocket, app: App, ip: IpAddr) {
         );
         tokio::select! {
             () = app.shutdown.cancelled() => {
+                tokio::time::sleep(app.drain.next_delay()).await;
                 close_with(&mut connected.socket, close::SHUTTING_DOWN, "server shutting down").await;
                 return;
             }
+            () = tokio::time::sleep_until(idle_at) => {
+                close_with(&mut connected.socket, close::POLICY_VIOLATION, "no frame within two and a half heartbeats").await;
+                return;
+            }
+            () = tokio::time::sleep_until(sweep_at) => {
+                sweep_at = Instant::now() + SWEEP_INTERVAL;
+                if connected.deliver().await.is_err() {
+                    return;
+                }
+            }
             signal = membership.next() => {
                 let done = match signal {
                     Signal::Ring => connected.deliver().await,
@@ -294,6 +326,7 @@ async fn connection(mut socket: WebSocket, app: App, ip: IpAddr) {
                 return;
             }
             message = connected.socket.recv() => {
+                idle_at = Instant::now() + idle;
                 if matches!(message, Some(Ok(Message::Binary(_) | Message::Text(_))))
                     && !connected.budget.spend()
                 {
@@ -334,6 +367,13 @@ async fn refuse_oversized(socket: &mut WebSocket, error: axum::Error) {
     }
 }
 
+/// When a connection first sweeps: somewhere in the first five minutes, from its id, so
+/// connections that came together do not sweep together.
+fn first_sweep(connection_id: Uuid) -> Duration {
+    let spread = u64::from(connection_id.as_bytes()[15]) * SWEEP_INTERVAL.as_secs() / 256;
+    Duration::from_secs(spread.max(1))
+}
+
 fn far_future() -> Instant {
     Instant::now() + Duration::from_secs(24 * 60 * 60)
 }
@@ -851,11 +891,13 @@ impl Connected {
     }
 }
 
+/// Sends one frame. A client that does not take it within 10 s is cut off.
 async fn send(socket: &mut WebSocket, body: Body) -> Result<(), Closed> {
-    socket
-        .send(Message::Binary(encode_frame(body).into()))
-        .await
-        .map_err(|_| Closed)
+    let message = Message::Binary(encode_frame(body).into());
+    match tokio::time::timeout(WRITE_TIMEOUT, socket.send(message)).await {
+        Ok(Ok(())) => Ok(()),
+        Ok(Err(_)) | Err(_) => Err(Closed),
+    }
 }
 
 async fn close_with(socket: &mut WebSocket, code: u16, reason: &'static str) {
@@ -863,7 +905,7 @@ async fn close_with(socket: &mut WebSocket, code: u16, reason: &'static str) {
         code,
         reason: reason.into(),
     };
-    let _ = socket.send(Message::Close(Some(frame))).await;
+    let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.send(Message::Close(Some(frame)))).await;
 }
 
 /// Structured JSON logs on stdout. The level comes from `RUST_LOG` (default `info`).
```

Change `crates/push-gateway/tests/gateway.rs`:

```diff
diff --git a/crates/push-gateway/tests/gateway.rs b/crates/push-gateway/tests/gateway.rs
index ecc647a..7a2862e 100644
--- a/crates/push-gateway/tests/gateway.rs
+++ b/crates/push-gateway/tests/gateway.rs
@@ -120,11 +120,12 @@ const AUDIENCE: &str = "ws://push.test/v1/socket";
 /// Redis, message-svc's API on the in-memory store, auth-svc's key set, and the gateway,
 /// on free ports.
 async fn stack() -> Stack {
-    stack_with(0).await
+    stack_with(0, Duration::from_secs(30)).await
 }
 
-/// As [`stack`], with message-svc failing its first `sync_failures` syncs.
-async fn stack_with(sync_failures: u32) -> Stack {
+/// As [`stack`], with message-svc failing its first `sync_failures` syncs, and the
+/// gateway announcing `heartbeat`.
+async fn stack_with(sync_failures: u32, heartbeat: Duration) -> Stack {
     let redis = Redis::default()
         .with_tag("7-alpine")
         .start()
@@ -173,7 +174,7 @@ async fn stack_with(sync_failures: u32) -> Stack {
         token_audience: AUDIENCE.to_owned(),
         listen_addr: gateway,
         health_addr: health.local_addr().unwrap(),
-        heartbeat: Duration::from_secs(30),
+        heartbeat,
         groups: false,
         client_ip_header: None,
     };
@@ -849,6 +850,20 @@ async fn too_many_frames_close_the_connection_with_4004() {
     assert_eq!(close_code(&mut socket).await, 4004);
 }
 
+/// A client that stops sending frames is gone (or behind a half-open connection): it is
+/// closed after two and a half heartbeats without a frame.
+#[tokio::test]
+async fn a_silent_client_is_closed_after_two_and_a_half_heartbeats() {
+    let stack = stack_with(0, Duration::from_secs(1)).await;
+    let (user, device) = person(&stack).await;
+    let mut socket = ready(&stack, user, device).await;
+    let started = std::time::Instant::now();
+    send(&mut socket, Body::Ping(Ping {})).await;
+    assert_eq!(next(&mut socket).await, Body::Pong(Pong {}));
+    assert_eq!(close_code(&mut socket).await, 1008);
+    assert!(started.elapsed() >= Duration::from_millis(2500));
+}
+
 /// A message over the 128 KiB limit ends the connection with 1009 (message too big).
 #[tokio::test]
 async fn an_oversized_frame_is_closed_with_1009() {
@@ -924,7 +939,7 @@ async fn a_backlog_larger_than_one_sync_batch_is_delivered_as_the_device_acks()
 
 #[tokio::test]
 async fn a_failed_sync_is_retried() {
-    let stack = stack_with(1).await;
+    let stack = stack_with(1, Duration::from_secs(30)).await;
     let (_alice, (bob, bob_device), _conversation) = chat_with_backlog(&stack, 1).await;
 
     let mut socket = connect(&stack).await;
```
- [ ] **Step 2: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "feat(gateway): write and idle timeouts, Redis and HTTP/2 keepalives, a 5-minute sweep and batched draining"
```

Expected: 861 tests pass, no warnings, no diff.
---

### Task 11: README, roadmap and pull request

**Files:**
- Modify: `README.md`, `docs/superpowers/plans/2026-10-11-roadmap.md`

The spec already describes Plan 8 (§2, §5.3–§5.5, §7.1–§7.6, §11, §12, §13), with the
review revisions and the owner decisions: it was committed to `main` together with this
plan. The README lists the gateway's settings and the roadmap marks 8A in review.

- [ ] **Step 1: Make the changes**

Change `README.md`:

```diff
diff --git a/README.md b/README.md
index 1ddc5df..fd10711 100644
--- a/README.md
+++ b/README.md
@@ -43,11 +43,10 @@ Secret, the PostgreSQL volume and its data directory (`-Delete` deletes the prof
 so a later install reuses both and the passwords keep matching. Every pull request is
 verified this way before it is merged.
 
-The minikube values switch on DEV ONLY shortcuts: the gateway accepts any user and device
-id without a token (until it checks access tokens, Plan 8), and the worker, which logs
-emails instead of sending them, shows them at `/dev/mailbox`; neither is routed by the
-ingress, which
-reaches the worker only at `/v1/webhooks/resend`. The chart refuses that mode unless the
+The minikube values switch on a DEV ONLY shortcut: the worker, which logs emails instead
+of sending them, shows them at `/dev/mailbox`, which the ingress does not route (it
+reaches the worker only at `/v1/webhooks/resend`). Clients, the smoke test included,
+always identify with real access tokens. The chart refuses that mode unless the
 values also say `devMode.acknowledge: insecure-local-only` and the ingress host is local
 (`*.local`, `*.localhost` or `*.test`), and refuses it with the Resend sender.
 `pwsh deploy/helm/test-chart.ps1` checks the chart without a cluster.
```

Change `docs/superpowers/plans/2026-10-11-roadmap.md`:

```diff
diff --git a/docs/superpowers/plans/2026-10-11-roadmap.md b/docs/superpowers/plans/2026-10-11-roadmap.md
index d933f30..c92e427 100644
--- a/docs/superpowers/plans/2026-10-11-roadmap.md
+++ b/docs/superpowers/plans/2026-10-11-roadmap.md
@@ -33,7 +33,7 @@ Locally, email uses the log-only sender, so no cloud account is needed to run th
 | 6B | Worker and email: minikube | The smoke test on the real flow (the worker relays and its log sender delivers; no DEV ONLY mirror), the DEV ONLY mailbox moved to the worker, the worker's image and chart (own database and role, the Accounts token and Resend credentials in the Secret, a webhook-only Ingress, one worker in dev mode) — verified on minikube | 6A | Done (PR #11) |
 | 7A | Message service: the core | Complete `push-message-svc` over gRPC: the client frames `Edit`, `Delete`, `ReadUpTo` and `Request`/`Response` (contacts, invites, direct conversations, groups of up to 100 with admins, read positions, 48-hour edits and deletes, history, listings with receipts); kicks on `kick:{device_id}` for relayed revocations and password resets; a `Directory` API behind auth-svc's own token, and auth-svc's `GET /v1/users?ids=…` on it. Review fixes: a per-user lock serialising device registrations with joins (proven by a Postgres concurrency case), exhaustive error codes, `google.rpc.Status` error details, sync documented as a read; tested against Postgres and Redis containers | 6B | Done (PR #12) |
 | 7B | Message service: minikube | The smoke test on the new API (contacts by request and invite, profiles, edit, read, a group and a delete delivered to an open socket, kicks heard on Redis after a revocation and a password reset), the DEV ONLY `DevSetup` API removed, the chart's Directory token for auth-svc and message-svc, auth-svc's message-svc URL and the smoke test's Redis URL — verified on minikube | 7A | Done (PR #13) |
-| 8A | Gateway: the service | Complete `push-gateway`: access tokens checked locally against auth-svc's JWKS (EdDSA pinned, `kid` for key rotation with retired keys kept 16 minutes so a rotation signs nobody out, the key set bounded and refetched at most every 30 s, `exp` required with 60 s leeway, `aud`); tokens issued no later than the last password reset refused (auth-svc's reset time relayed by the worker and kept by message-svc, `Identify` compares the token's `iat`; the reset kick spares newer sessions); every client frame of §7.1 carried to message-svc and the server frames of §7.2 (receipts, read positions, contact events, `AuthExpiring`); Identify/ReAuth with the kick channel watched before message-svc answers, takeovers obeyed only from a newer connection, the close codes of §7.4 (4001 password reset, 4003 revoked, 4006 not relayed yet, 1002 before Ready, 1009 too big); group requests refused behind one switch (groups are not offered to apps in the first release); Redis rate limits on redeeming invites, contact requests and invites (fail closed), per-device `Identify` and per-connection frame budgets; write and idle timeouts, Redis and HTTP/2 keepalives, a 5-minute sweep, batched draining; contact requests never reveal which ids exist; the DEV ONLY identity removed; owner decisions recorded (the two-call id check accepted, the rate limits kept, the 15-minute revocation bound while Redis is down accepted) — tested against Postgres and Redis containers | 7B | |
+| 8A | Gateway: the service | Complete `push-gateway`: access tokens checked locally against auth-svc's JWKS (EdDSA pinned, `kid` for key rotation with retired keys kept 16 minutes so a rotation signs nobody out, the key set bounded and refetched at most every 30 s, `exp` required with 60 s leeway, `aud`); tokens issued no later than the last password reset refused (auth-svc's reset time relayed by the worker and kept by message-svc, `Identify` compares the token's `iat`; the reset kick spares newer sessions); every client frame of §7.1 carried to message-svc and the server frames of §7.2 (receipts, read positions, contact events, `AuthExpiring`); Identify/ReAuth with the kick channel watched before message-svc answers, takeovers obeyed only from a newer connection, the close codes of §7.4 (4001 password reset, 4003 revoked, 4006 not relayed yet, 1002 before Ready, 1009 too big); group requests refused behind one switch (groups are not offered to apps in the first release); Redis rate limits on redeeming invites, contact requests and invites (fail closed), per-device `Identify` and per-connection frame budgets; write and idle timeouts, Redis and HTTP/2 keepalives, a 5-minute sweep, batched draining; contact requests never reveal which ids exist; the DEV ONLY identity removed; owner decisions recorded (the two-call id check accepted, the rate limits kept, the 15-minute revocation bound while Redis is down accepted) — tested against Postgres and Redis containers | 7B | In review |
 | 8B | Gateway: minikube | The smoke test over the WebSocket only, with real access tokens (contacts by request and invite, chat, edit, read, delete, a refused group request, a revocation and a password reset closing sockets with 4003 and 4001, expired and wrong-audience tokens refused), message-svc's `Messaging` API behind the gateway's own internal token (generated by the chart), the chart's gateway settings — verified on minikube | 8A | |
 | 9 | SDK core | `push-sdk`: connection state machine (reconnects, without signing out, while a new device is not relayed yet), SQLite outbox, local store, retries, events — end-to-end tests against minikube | 8B | |
 | 10 | SDK bindings | napi-rs Node binding, cbindgen C binding | 9 | |
```
- [ ] **Step 2: Final gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
pwsh deploy/helm/test-chart.ps1
git add -A README.md docs
git commit -m "docs: record plan 8A in the README and roadmap"
```

Expected: 861 tests pass, no warnings, no diff, `all chart checks passed`.
`cargo +1.85 test --workspace --locked` passes the same tests.

- [ ] **Step 3: Push, open the pull request and wait for CI**

```powershell
git push -u origin feature/plan-08a-gateway-service
$body = @'
Implements docs/superpowers/plans/2026-10-14-plan-08a-gateway-service.md: the complete gateway. Access tokens are checked locally against auth-svc's JWKS (EdDSA pinned, kid for key rotation, exp required with 60 s leeway, aud); a key that leaves the set is kept 16 minutes and an unknown kid missing from a fresh set is 4002, never 4001, so a rotation signs nobody out; the set is read up to 64 KiB and refetched at most every 30 s. Tokens issued no later than the last password reset are refused: auth-svc puts its own reset time in user.password_reset, the worker relays it, message-svc keeps the latest and Identify compares the token's iat; the reset kick carries the time and spares connections with a newer token. Every client frame reaches message-svc and receipts, read positions, contact events and AuthExpiring reach clients. Identify watches kick:{device} before message-svc answers; ReAuth renews tokens; takeovers are obeyed only from a strictly newer connection; kicks (4003 revoked, 4001 password reset), 4006 for a device not relayed yet, 1002 for protocol mistakes before Ready, 4002 at once for a token past its ReAuth deadline. Group requests are refused behind PUSH_GROUPS. Redis rate limits on RedeemInvite, ContactRequest and CreateInvite (fail closed), Identify per device and a frame budget per connection; 1009 over 128 KiB; write and idle timeouts, Redis PING and HTTP/2 keepalive, a 5-minute sweep, batched draining. Contact requests no longer reveal which ids exist. The DEV ONLY identity is removed. Owner decisions recorded: the two-call id check accepted, the rate limits kept, the 15-minute revocation bound while Redis is down accepted. 861 tests.

Plan 8B moves the smoke test onto the WebSocket, puts the Messaging API behind the gateway's own token and verifies it on minikube.
'@
gh pr create --base main --title "Plan 8A: the gateway" --body $body
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust`, `msrv`, `images`, `chart`). Do not merge; the
controller merges after the final review.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §5.1 access-token claims checked: `sub`, `device_id`, `email_verified`, `exp`, `iat`, `aud`; key rotation without sign-outs | 4 |
| §5.3 the device only from the verified token; not relayed yet is retryable, never a sign-out | 3, 5 |
| §5.3, §5.4 revoked devices closed at once (4003); a password reset kicks (4001) connections older than it, and tokens issued until it are refused | 3, 7 |
| §5.5 contact requests; invites rate limited; no answer reveals which ids exist (owner decision 1) | 2, 9 |
| §7.1 every client frame; `Identify` within 5 s; request frames with `request_id` | 1, 5, 8 |
| §7.2 every server frame: `Ready`, `Deliver`, `SendAck`, `Receipt`, `ReadPosition`, `ContactEvent`, `Response`, `AuthExpiring`, `Error`, `Pong` | 1, 5, 6, 7, 8 |
| §7.3 Identify rate limited per device, subscriptions, takeover with 4003 from a newer connection only, `Ready` then sync; `AuthExpiring`, `ReAuth`, 4002 | 5, 6, 7, 9 |
| §7.4 close codes, aligned with the owner's decisions | 1, 5, 6, 7, 9, 10 |
| §7.5 heartbeat and timeouts | 10 |
| §7.6 the DEV ONLY identity removed, fields reserved | 5 |
| §11 the gateway closes connections in batches with 4005 | 10 |
| §12 tests 4 (Redis down: sweep, reconnect), 6 (revoked while connected), 7 (`ReAuth` or 4002) | 10, 7, 6 |
| §13 groups not exposed to apps in the first release | 8 |

## Deferred to later plans

- **The smoke test over the WebSocket only, the Messaging token, minikube** (Plan 8B):
  until then message-svc's `Messaging` API trusts any `user_id` from anyone who can reach
  it, and the smoke test still calls message-svc and hears kicks on Redis.
- **auth-svc's own API and password resets**: `GET /v1/devices` and the other
  authenticated endpoints still accept an access token from before a reset until it
  expires (15 minutes); only the gateway checks `password_reset_at`.
- **A shared rate-limit crate** for auth-svc and the gateway, whose fixed windows are the
  same code twice.
- **A per-IP limit before `Identify`** (connections per address); for now the token check,
  the per-device `Identify` limit and the frame budget bound what a connection can cost.
- **The SDK** (Plan 9): keep the highest receipt positions; reconnect without signing out
  on everything but 4001 and stop only on 4003; refresh on `AuthExpiring` and send
  `ReAuth`; validate frame sizes before queuing; **while a backlog arrives, ack only the
  highest `seq` it holds** (one `Ack` per `Sync` batch), which the frame budget requires.
- **Plan 11:** `x-real-ip` behind an SNAT load balancer gives every client the balancer's
  address and collapses the per-IP limits into one bucket (needs
  `externalTrafficPolicy: Local` or the PROXY protocol); a JWKS URL over https needs TLS
  turned on in the gateway's reqwest (built without TLS; inside the cluster it is plain
  http); TLS for the internal APIs, a NetworkPolicy, the 10,000-connection load test,
  tuning the drain batches, and revisiting the 15-minute revocation bound while Redis is
  down (owner decision 3).
- **jsonwebtoken 10** once it offers the ring backend again; 9.3 is safe here because
  `exp`, `sub` and `aud` are required.
- **gRPC load balancing across message-svc replicas** (from Plan 4): the gateway holds one
  HTTP/2 connection.
