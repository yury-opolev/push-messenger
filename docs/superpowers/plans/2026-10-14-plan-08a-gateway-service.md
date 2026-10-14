# Plan 8A — Gateway: the Service Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A complete `push-gateway` (spec §7). Clients identify with the access tokens auth-svc issues, checked locally against auth-svc's JWKS (EdDSA pinned, `kid` for key rotation, `exp` required with 60 s of leeway, `aud` the gateway's URL), and tokens issued before the user's last password reset are refused. Every client frame of §7.1 travels to message-svc's existing RPCs, and every server frame of §7.2 reaches the client: receipts, read positions, contact events and `AuthExpiring`. `Identify` watches the device's kick channel before message-svc answers, `ReAuth` renews a connection's token, a newer connection of a device takes over, kicks close sockets (4003 for a revocation, 4001 for a password reset), a device not relayed yet gets a retryable close of its own (4006), and the close codes of §7.4 are aligned with the owner's decisions. Group requests are refused behind one switch, because groups are not offered to apps in the first release. Rate limits in Redis guard `RedeemInvite`, `ContactRequest` and `CreateInvite` (failing closed), and per-device `Identify` and per-connection frame budgets guard the gateway. Write and idle timeouts, Redis and HTTP/2 keepalives, a 5-minute sweep and batched draining make dead connections and dead dependencies visible. Contact requests stop revealing which user ids exist. The DEV ONLY identity is removed. Everything is tested against the in-memory store and Postgres and Redis containers. Plan 8B moves the smoke test onto the WebSocket alone, puts message-svc's `Messaging` API behind the gateway's own internal token, and verifies it all on minikube.

**Architecture:** The gateway stays transport only (spec §2). A new `tokens.rs` keeps auth-svc's public keys by `kid` and checks tokens with `jsonwebtoken` (already in the workspace for auth-svc); it fetches `GET {PUSH_AUTH_URL}/v1/auth/jwks` at start-up and every 5 minutes, and again at once (at most every 30 s) when a token names a key it does not know, so a key rotation needs no restart. message-svc owns the password-reset check, as it owns the devices: the worker already relays `user.password_reset`, so message-svc now records when it heard of the last reset (`known_user.password_reset_at`, migration 0003) and `Identify` takes the token's `iat` and refuses a token from that second or earlier. The hub (`hub.rs`) is rebuilt around channels rather than users: every connection joins `kick:{device_id}` as soon as its token passed, before message-svc is asked, and `notify:{user_id}` once admitted; notifications become rings (sync) or server frames, kicks close sockets, and a takeover is a `Kick{TAKEN_OVER, connection_id}` the newcomer publishes. The session (`session.rs`) remains pure protocol state; `server.rs` does the I/O. Rate limits (`limits.rs`) are fixed windows in Redis like auth-svc's; the frame budget is a token bucket per connection; `drain.rs` spreads the shutdown closes. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §2, §5.1, §5.3–§5.5, §7 (all), §11, §12, §13.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85). No new crates in the workspace: the gateway now also depends on `jsonwebtoken` 9.3 (ring), `reqwest` 0.12, `serde`, `serde_json`, `sha2`, `async-trait` and `tokio-tungstenite` 0.29 (the version axum 0.8 uses), and on `push-auth` for tests only. All are in the workspace already, with MSRV 1.85.

---

## Before you start

- **Plan 7B is merged** (PR #13, `main` at `83e6827`). This plan changes `push-proto`,
  `push-domain`, `push-store`, `push-testkit`, `push-message-svc` and `push-gateway`, with
  test adjustments in `push-worker` and `push-smoke`, and the gateway's chart settings.
  Plan 8B does the smoke test over the WebSocket, the Messaging token and minikube.
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
- **Edit files with the edit tool, and keep them LF** (`.gitattributes`:
  `* text=auto eol=lf`). The file-creation tool may write CRLF; git normalises it on commit.
- **Do not pipe `cargo fmt`.** Run `cargo fmt --all -- --check` on its own and read the exit
  code. The code in this plan is formatted, so a diff means a copying mistake.
- **Replace blocks.** "Replace … with …" pairs below are exact copies of the file before and
  after the change, with two lines of context (more where two would not make the first block
  unique): find the first block in the file and replace it with the second. "Create" and
  "Replace the whole of" blocks are whole files.
- Baseline (after Plan 7B): `cargo test --workspace --locked` passes 822 tests;
  `deploy/helm/test-chart.ps1` passes 29 checks.
- **Cargo.lock.** Tasks 4 and 9 add dependencies to `push-gateway`; each first runs
  `cargo test -p push-gateway` without `--locked`, which records them; the gates then run
  with `--locked`. Never run `cargo update`.
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
  again.
- **Scope rule:** no auth-svc or worker behaviour changes; the smoke test changes only as far
  as the gateway forces it (it identifies with its access token from Task 5); the Messaging
  token, the smoke test over the WebSocket and minikube are Plan 8B; no TLS, Key Vault or
  NetworkPolicy (Plan 11).

## Key rules the code must preserve

- **The gateway learns the user and device only from a verified token** (spec §5.3). No
  identity comes from anything a client sends unsigned.
- **Only 4001 signs a user out and only 4003 stops a client.** Every other refusal —
  a device not relayed yet, keys or message-svc unreachable, a protocol mistake before
  `Ready`, a rate limit — must let the client reconnect.
- **Each service owns its database.** The gateway never reads auth-svc's or message-svc's
  database; the password-reset check lives in message-svc, which hears of resets from the
  worker.
- **Subscribe before you ask.** A connection watches `kick:{device_id}` before message-svc
  checks the device, so a kick published after that check cannot be missed.
- **Redis carries only signals**: rings, receipt positions, contact states and kicks with a
  reason, never a body or a token.
- **Never log tokens, bodies or message contents.** Close reasons and errors are fixed
  texts; logs name users and devices by id at most.
- **Every Redis call and connect loop is bounded and interruptible by shutdown.**
- **Delivery positions move only on `Ack`.** Whether entries are waiting is always asked of
  `Sync` (`entries_after` with a limit plus one), never decided by comparing positions:
  edit and delete entries a member may not see can end a log and park the position before
  them.
- **No answer reveals which user ids exist.**

## Decisions made for this plan

These were settled while writing the plan and verified in a worktree. The owner's earlier
decisions apply (a revoked device closes with 4003, a password reset with 4001; groups are
not offered to apps in the first release; names of people one can no longer chat with stay
visible). Do not reopen any of this during execution.

1. **Plan 8 is split.** 8A (this plan) delivers the service, tested against containers;
   8B moves the smoke test onto the WebSocket alone, adds the Messaging token and verifies
   everything on minikube.
2. **How the gateway learns of a password reset: message-svc records it, `Identify`
   compares the token's `iat`.** The worker already relays `user.password_reset` to
   message-svc, which already kicks the user's devices. It now also records *when it heard
   of the reset* (`known_user.password_reset_at`, the greater of the stored and the new time,
   so a repeated relay moves it forward), and `Identify` gets the token's `iat` and refuses
   a token issued no later than that second (`UNAUTHENTICATED`, `PASSWORD_CHANGED`), on
   connect and on every `ReAuth`. Using the relay time matches the kick exactly: every
   session that existed before message-svc heard of the reset is ended, including one a
   user signed into after the reset but before a slow relay (the kick ends that one too).
   Within the reset's second the order is unknown, so the check refuses: a token minted in
   the same second only asks for one more sign-in. The rejected alternatives: a `pwd` claim
   compared against a stamp needs the gateway to read auth-svc's database or call auth-svc
   on every connect; a gateway-side call to auth-svc adds a dependency on every connect;
   the kick alone misses a lost Redis publish and a token that outlives the reset by up to
   15 minutes.
3. **Tokens are checked in the gateway, keys from the JWKS by `kid`.** Algorithm pinned to
   EdDSA (the header cannot choose HMAC with the public key), `exp`, `sub` and `aud`
   required (jsonwebtoken 9.3 skips a mistyped claim unless required, CVE-2026-25537),
   `device_id` and `iat` present, `email_verified` true, `exp` with 60 s of leeway. A token
   naming an unknown key fetches the set again at most every 30 s; only a key missing from
   a set fetched for that very token is invalid (4001), while a key that could not be
   looked up — fetch failed or too soon — is `KeysUnavailable` (1011), so neither an
   auth-svc outage nor a rotation signs anyone out. `/readyz` waits for the keys.
4. **Close codes (spec §7.4).** 4001 invalid token or password reset (sign in again); 4002
   expired token or no `ReAuth` by `exp + 30 s` (refresh, reconnect); 4003 takeover or
   revoked (stop); 4004 rate limited; 4005 shutting down; **4006 device not ready yet**
   (reconnect after a short backoff, never sign out); **1002** a protocol mistake before
   `Ready` (this answers spec §7.4's open "4001 or 4002 before Ready": neither — a client
   bug is no reason to sign out); 1008 silent for two and a half heartbeats; 1009 a message
   over 128 KiB; 1011 keys or message-svc unreachable.
5. **`Identify` and the oracle.** The device id comes from the verified token, so
   `UNKNOWN_DEVICE` (4003) versus `DEVICE_NOT_READY` (4006) tells a client only about its
   own device. `DEVICE_NOT_READY` is a new error code, retryable, so the gateway can tell it
   from message-svc being down; its log line is `debug`, not `warn`.
6. **The hub is keyed by channel.** A connection watches `kick:{device_id}` (waiting up to
   2 s for the subscription) before message-svc is asked, then joins `notify:{user_id}`.
   Kicks go to a channel of their own per connection and are handled first; server frames
   queue up to 64 per connection (a dropped one is recovered by listing); rings coalesce.
   A takeover is published by the newcomer as `Kick{TAKEN_OVER, connection_id}` on
   `kick:{device_id}`; every other connection of the device, on any gateway, closes with
   4003. A kick lost while Redis is down is caught at the next `ReAuth` (within 15 minutes),
   when message-svc refuses a revoked device or a token older than a reset.
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
   client IP comes from `PUSH_CLIENT_IP_HEADER` (`x-real-ip`), with auth-svc's trust rule.
   The fixed-window code mirrors auth-svc's (`push-auth` is too heavy a dependency for the
   gateway); a shared crate is deferred.
10. **Contact requests never reveal which ids exist** (spec §5.5, owner rule). A request to
    an id that is not a verified user is answered `REQUESTED_OUT`, like a request nobody
    answered yet, and stores and announces nothing; `Accept` and `Decline` answer it
    `NO_PENDING_REQUEST`, `Block` and `Unblock` succeed. `USER_NOT_FOUND` now only means the
    caller itself is unknown. The requester's `ListContacts` does not show such a request
    (owner decision needed 1).
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

## Owner decisions needed

The plan proceeds with the recommendation in each case; a different answer changes a
constant or a few lines, not the design.

1. **Should a request to an id that does not exist show in the requester's own contact
   list?** Example: Alice somehow has the id `0192…` of no real user and sends it a contact
   request. She is told "pending" (`REQUESTED_OUT`), exactly as for a real user who has not
   answered. But when her app lists her contacts (`ListContacts`), a real pending request
   shows up and this one does not, because message-svc stores nothing for an id that is not
   a user. So someone who already holds an id can learn whether it is a real user with two
   calls. **Recommended: accept this.** Ids are random (74 random bits) and only learned
   from someone who has them, handle lookup already tells whether a handle exists (20 an
   hour), and requests are now limited to 30 an hour per user. The alternative is to store a
   "ghost" pending request for ids that are not users, so the list looks the same; that
   costs a row per probe and a cleanup job.
2. **Are these limits right for real people?** `RedeemInvite` 20 an hour per user (60 per
   network address), `ContactRequest` 30 an hour per user (120 per address), `CreateInvite`
   20 an hour per user. Examples: a teacher who wants 25 students as contacts creates one
   invite with `max_uses` 30 and each student redeems it once — fine. Someone adding 40
   friends by handle in one evening is told to wait after 30 and can continue an hour
   later. A family of five behind one home router redeeming invites shares 60 an hour.
   **Recommended: keep these for the first release**, and revisit with real usage.
3. **A revoked device that is connected while Redis is down.** If Redis is down at the
   moment a device is revoked, the kick is lost and that device's open connection stays up
   until its token needs renewing — at most 15 minutes (then message-svc refuses it). The
   gateway could instead re-check every open connection with message-svc whenever its Redis
   subscription comes back, at the cost of one database read per connection at that moment
   (10,000 reads at once on a full gateway). **Recommended: accept the 15-minute bound now**
   and decide with the load test of Plan 11.

## File structure

```
crates/push-proto/proto/push/v1/frame.proto   Frame gets ReAuth, Edit, Delete, ReadUpTo,
                                                Request (5-9) and Response, Receipt,
                                                ReadPosition, ContactEvent, AuthExpiring
                                                (37-41); 4 error codes; retry_after_s;
                                                Identify's dev fields reserved
crates/push-proto/proto/push/internal/v1/messaging.proto   IdentifyRequest.issued_at;
                                                Kick.connection_id, KICK_REASON_TAKEN_OVER
crates/push-proto/src/lib.rs, codec.rs        close codes 4006, 1002, 1008, 1009, 1011
crates/push-domain/src/contacts.rs            unknown peers answered as strangers
crates/push-domain/src/service.rs, error.rs, model.rs, store.rs, memory.rs, lib.rs
                                              password resets recorded; Identify by iat
crates/push-store/migrations/0003_password_reset.sql   NEW: known_user.password_reset_at
crates/push-store/src/store.rs                device_state with the reset; record it
crates/push-testkit/src/…                     scenarios and contract for both
crates/push-message-svc/src/grpc.rs, convert.rs, notify.rs   issued_at; new codes
crates/push-gateway/src/tokens.rs             NEW: JWKS keys by kid, token checks
crates/push-gateway/src/hub.rs                rebuilt: channels, kicks, server frames, PING
crates/push-gateway/src/limits.rs             NEW: Redis rate limits, frame budget
crates/push-gateway/src/drain.rs              NEW: batched closes at shutdown
crates/push-gateway/src/session.rs            token Identify, ReAuth, the new frames, groups
crates/push-gateway/src/server.rs             Identify/ReAuth, kicks, frames, limits, liveness
crates/push-gateway/src/config.rs, lib.rs, Cargo.toml   PUSH_AUTH_URL, PUSH_TOKEN_AUDIENCE,
                                                PUSH_GROUPS, PUSH_CLIENT_IP_HEADER
crates/push-gateway/tests/gateway.rs, hub.rs  the gateway against real tokens and Redis
crates/push-smoke/src/lib.rs, kicks.rs, tests/local.rs   identifies with its token
crates/push-worker/tests/relay.rs, server.rs  issued_at
deploy/helm/…                                 the gateway's settings; test-chart.ps1
README.md, spec, roadmap                      docs
```

---

### Task 1: Every client and server frame of spec §7, and the new codes

**Files:**
- Modify: `crates/push-proto/proto/push/v1/frame.proto`, `crates/push-proto/src/codec.rs`,
  `src/lib.rs`, `crates/push-gateway/src/session.rs`,
  `crates/push-message-svc/src/convert.rs`

- [ ] **Step 1: The test, failing**

The new frames survive encoding in a `Frame`. In `crates/push-proto/src/codec.rs` (the
other change is the new `Error` field in two literals):

Replace

```rust
            client_msg_id: None,
            request_id: None,
        },
    }
```

with

```rust
            client_msg_id: None,
            request_id: None,
            retry_after_s: None,
        },
    }
```

Replace

```rust
    }

    #[test]
    fn a_frame_without_a_body_is_empty() {
```

with

```rust
    }

    #[test]
    fn the_request_and_server_frames_survive_encoding() {
        use crate::v1::{
            ContactEvent, ContactState, DirectReceipt, ListContacts, ReAuth, ReadPosition, Receipt,
            Request, Response, receipt, request, response,
        };
        let conversation = id_bytes(Uuid::now_v7());
        let frames = [
            Body::ReAuth(ReAuth {
                token: "a.b.c".to_owned(),
            }),
            Body::Request(Request {
                request_id: 7,
                body: Some(request::Body::ListContacts(ListContacts {})),
            }),
            Body::Response(Response {
                request_id: 7,
                body: Some(response::Body::Done(crate::v1::Done {})),
            }),
            Body::Receipt(Receipt {
                conversation_id: conversation.clone(),
                positions: Some(receipt::Positions::Direct(DirectReceipt {
                    user_id: id_bytes(Uuid::now_v7()),
                    delivered_seq: 4,
                    read_seq: 3,
                })),
            }),
            Body::ReadPosition(ReadPosition {
                conversation_id: conversation,
                read_seq: 3,
            }),
            Body::ContactEvent(ContactEvent {
                user_id: id_bytes(Uuid::now_v7()),
                state: ContactState::Accepted.into(),
            }),
        ];
        for frame in frames {
            assert_eq!(decode_frame(&encode_frame(frame.clone())), Ok(frame));
        }
    }

    #[test]
    fn a_frame_without_a_body_is_empty() {
```

Replace

```rust
            client_msg_id: Some(id_bytes(Uuid::now_v7())),
            request_id: Some(7),
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
```

with

```rust
            client_msg_id: Some(id_bytes(Uuid::now_v7())),
            request_id: Some(7),
            retry_after_s: None,
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
```

Replace

```rust
            client_msg_id: None,
            request_id: None,
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
```

with

```rust
            client_msg_id: None,
            request_id: None,
            retry_after_s: None,
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
```

```powershell
cargo test -p push-proto
```

Expected: compile errors — `Body` has no variant `ReAuth`, and there is no `ReadPosition`,
`Receipt` or `ContactEvent`.

- [ ] **Step 2: The frames, the codes and the field**

In `crates/push-proto/proto/push/v1/frame.proto` (client frames 5–9, server frames 37–41,
their messages, four error codes and `retry_after_s`):

Replace

```protobuf
message Frame {
  oneof body {
    // Client to server.
    Identify identify = 1;
    Send send = 2;
    Ack ack = 3;
    Ping ping = 4;

    // Server to client.
    Ready ready = 32;
    SendAck send_ack = 33;
```

with

```protobuf
message Frame {
  oneof body {
    // Client to server (spec §7.1).
    Identify identify = 1;
    Send send = 2;
    Ack ack = 3;
    Ping ping = 4;
    ReAuth re_auth = 5;
    Edit edit = 6;
    Delete delete = 7;
    ReadUpTo read_up_to = 8;
    Request request = 9;

    // Server to client (spec §7.2).
    Ready ready = 32;
    SendAck send_ack = 33;
```

Replace

```protobuf
    Error error = 35;
    Pong pong = 36;
  }
  // 5-31: the other client frames of spec §7.1 (ReAuth, Edit, Delete, ReadUpTo and the
  // request frames); 37-63: the other server frames of §7.2. Assigned as they are added:
  // Edit, Delete, ReadUpTo, Request and Response are defined below and message-svc serves
  // them over gRPC (Plan 7); the gateway carries them in a Frame from Plan 8.
}
```

with

```protobuf
    Error error = 35;
    Pong pong = 36;
    Response response = 37;
    Receipt receipt = 38;
    ReadPosition read_position = 39;
    ContactEvent contact_event = 40;
    AuthExpiring auth_expiring = 41;
  }
  // 10-31 are free for later client frames, 42-63 for later server frames.
}
```

Replace

```protobuf
}

enum ContentType {
  CONTENT_TYPE_UNSPECIFIED = 0;
```

with

```protobuf
}

// A fresh access token for an identified connection (spec §7.3), sent after
// AuthExpiring. It must name the same user and device. Nothing answers a good one; a bad
// one closes the connection (4001, or 4002 when it has expired too).
message ReAuth {
  string token = 1;
}

// The access token of the connection expires in expires_in_s seconds: refresh it over
// HTTPS and send ReAuth. Without a valid ReAuth by 30 seconds after the expiry, the
// connection is closed with 4002.
message AuthExpiring {
  uint32 expires_in_s = 1;
}

// A receipt position moved in one of the user's conversations (spec §4.5). Under
// concurrent acks two receipts can arrive in either order: keep the highest position.
message Receipt {
  bytes conversation_id = 1;
  oneof positions {
    DirectReceipt direct = 2;
    GroupReceipts group = 3;
  }
}

// The other member of a direct conversation: everything up to delivered_seq reached one
// of their devices, everything up to read_seq was read.
message DirectReceipt {
  bytes user_id = 1;
  int64 delivered_seq = 2;
  int64 read_seq = 3;
}

// The user's own read position moved, from this or another of their devices.
message ReadPosition {
  bytes conversation_id = 1;
  int64 read_seq = 2;
}

// The user's relationship with another user changed, as the user sees it.
message ContactEvent {
  bytes user_id = 1;
  ContactState state = 2;
}

enum ContentType {
  CONTENT_TYPE_UNSPECIFIED = 0;
```

Replace

```protobuf
  // A group must keep at least one admin.
  ERROR_CODE_LAST_ADMIN = 22;
}
```

with

```protobuf
  // A group must keep at least one admin.
  ERROR_CODE_LAST_ADMIN = 22;
  // Too many requests of this kind; retry after retry_after_s seconds.
  ERROR_CODE_RATE_LIMITED = 23;
  // This server does not offer the feature (groups, in the first release).
  ERROR_CODE_NOT_AVAILABLE = 24;
  // The password was reset after the access token was issued: sign in again.
  ERROR_CODE_PASSWORD_CHANGED = 25;
  // The device was registered moments ago and is not ready yet: retry shortly.
  ERROR_CODE_DEVICE_NOT_READY = 26;
}
```

Replace

```protobuf
  // Set when the error answers a Request.
  optional uint64 request_id = 5;
}
```

with

```protobuf
  // Set when the error answers a Request.
  optional uint64 request_id = 5;
  // RATE_LIMITED: when the same frame may succeed again.
  optional uint32 retry_after_s = 6;
}
```

The close codes of spec §7.4 as the owner decided them (decision 4). In
`crates/push-proto/src/lib.rs`:

Replace

```rust
};

/// WebSocket close codes (spec §7.4).
pub mod close {
    /// Invalid token or identity, unverified email, or unknown or revoked device: sign in
    /// again.
    pub const INVALID_IDENTITY: u16 = 4001;
    /// Re-authentication required: refresh, then reconnect.
    pub const REAUTH_REQUIRED: u16 = 4002;
    /// Another connection took over, or the device was revoked: stop.
    pub const TAKEN_OVER: u16 = 4003;
    /// Rate limited: back off, then reconnect.
```

with

```rust
};

/// WebSocket close codes (spec §7.4). Codes from 4000 are the protocol's own; the others
/// are RFC 6455's. A client signs out only on 4001; it never reconnects after 4003; on
/// every other code it reconnects with backoff (after refreshing its token on 4002).
pub mod close {
    /// The access token is invalid (signature, audience, an unverified email) or older
    /// than the user's last password reset: sign in again.
    pub const INVALID_IDENTITY: u16 = 4001;
    /// The access token expired: refresh it, then reconnect.
    pub const REAUTH_REQUIRED: u16 = 4002;
    /// Another connection of the device took over, or the device was revoked: stop.
    pub const TAKEN_OVER: u16 = 4003;
    /// Rate limited: back off, then reconnect.
```

Replace

```rust
    /// The server is shutting down: back off with jitter, then reconnect.
    pub const SHUTTING_DOWN: u16 = 4005;
}
```

with

```rust
    /// The server is shutting down: back off with jitter, then reconnect.
    pub const SHUTTING_DOWN: u16 = 4005;
    /// The device was registered moments ago and the server is not ready for it yet:
    /// reconnect after a short backoff. Never a reason to sign out.
    pub const NOT_READY_YET: u16 = 4006;
    /// A protocol violation before `Ready`: no `Identify` within 5 s, or another frame
    /// first. Reconnect with backoff.
    pub const PROTOCOL_ERROR: u16 = 1002;
    /// The client stopped sending frames (no `Ping` within the idle timeout).
    pub const POLICY_VIOLATION: u16 = 1008;
    /// A message larger than the WebSocket limit.
    pub const MESSAGE_TOO_BIG: u16 = 1009;
    /// The server cannot serve the connection now (message-svc or the token keys are
    /// unreachable): reconnect with backoff.
    pub const INTERNAL_ERROR: u16 = 1011;
}
```

`Error` has a new field. In `crates/push-gateway/src/session.rs`:

Replace

```rust
        client_msg_id: None,
        request_id: None,
    })
}
```

with

```rust
        client_msg_id: None,
        request_id: None,
        retry_after_s: None,
    })
}
```

In `crates/push-message-svc/src/convert.rs`:

Replace

```rust
            retryable: false,
            client_msg_id: None,
            request_id: None,
        },
    )
}
```

with

```rust
            retryable: false,
            client_msg_id: None,
            request_id: None,
            retry_after_s: None,
        },
    )
}
```

Replace

```rust
            client_msg_id: None,
            request_id: None,
        },
    )
```

with

```rust
            client_msg_id: None,
            request_id: None,
            retry_after_s: None,
        },
    )
```

```powershell
cargo test -p push-proto -p push-gateway --lib
```

Expected: everything passes, `the_request_and_server_frames_survive_encoding` included.
The gateway still answers the new client frames with `BAD_FRAME` ("not a client frame")
until Task 8.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "feat(proto): every client and server frame of spec 7, new error and close codes"
```

Expected: 823 tests pass, no warnings, no diff.

---

### Task 2: Contact requests never reveal which ids exist

**Files:**
- Modify: `crates/push-testkit/src/scenarios/contacts.rs`,
  `crates/push-message-svc/src/requests/tests.rs`, `crates/push-domain/src/contacts.rs`,
  `crates/push-proto/proto/push/v1/frame.proto`

- [ ] **Step 1: The scenarios, failing**

A request to an id that is no user looks pending and leaves nothing behind; `Accept`,
`Decline`, `Block` and `Unblock` answer an unknown id as they answer a stranger (decision
10). In `crates/push-testkit/src/scenarios/contacts.rs`:

Replace

```rust
}

async fn requesting_an_unknown_user_fails<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    assert_eq!(
        world.svc.contact_request(alice.user, UserId::new()).await,
        Err(DomainError::UnknownUser)
    );
}
```

with

```rust
}

/// Spec §5.5: no answer reveals which ids exist. A request to an id that is not a verified
/// user is answered like one nobody has answered yet, and stores and announces nothing.
async fn a_request_to_an_unknown_id_looks_pending_and_stores_nothing<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    world.notifier.take();
    assert_eq!(
        world.svc.contact_request(alice.user, UserId::new()).await,
        Ok(ContactState::RequestedOut)
    );
    assert_eq!(world.svc.list_contacts(alice.user).await, Ok(Vec::new()));
    assert_eq!(world.notifier.take(), []);
}
```

Replace

```rust
}

async fn contact_operations_reject_self_and_unknown_users<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
```

with

```rust
}

async fn contact_operations_reject_self_and_treat_unknown_ids_as_strangers<B: Backend>(
    backend: &B,
) {
    let world = World::new(backend).await;
    let alice = world.person().await;
```

Replace

```rust
        selfish
    );
    let stranger = UserId::new();
    assert_eq!(
        world.svc.contact_accept(alice.user, stranger).await,
        Err(DomainError::UnknownUser)
    );
    assert_eq!(
        world.svc.contact_decline(alice.user, stranger).await,
        Err(DomainError::UnknownUser)
    );
    assert_eq!(
        world.svc.contact_block(alice.user, stranger).await,
        Err(DomainError::UnknownUser)
    );
}
```

with

```rust
        selfish
    );
    // The same answers as for a verified user Alice has no relationship with.
    let (unknown, stranger) = (UserId::new(), world.person().await);
    for peer in [unknown, stranger.user] {
        assert_eq!(
            world.svc.contact_accept(alice.user, peer).await,
            Err(DomainError::NoPendingRequest)
        );
        assert_eq!(
            world.svc.contact_decline(alice.user, peer).await,
            Err(DomainError::NoPendingRequest)
        );
        assert_eq!(world.svc.contact_unblock(alice.user, peer).await, Ok(()));
    }
    assert_eq!(world.svc.contact_block(alice.user, unknown).await, Ok(()));
    assert_eq!(world.svc.contact_state(alice.user, unknown).await, Ok(None));
}
```

Replace

```rust
        unblocking_allows_a_fresh_request,
        a_user_cannot_request_themselves,
        requesting_an_unknown_user_fails,
        a_hidden_request_arrives_when_the_block_is_lifted,
        the_sender_cannot_decline_their_own_request,
```

with

```rust
        unblocking_allows_a_fresh_request,
        a_user_cannot_request_themselves,
        a_request_to_an_unknown_id_looks_pending_and_stores_nothing,
        a_hidden_request_arrives_when_the_block_is_lifted,
        the_sender_cannot_decline_their_own_request,
```

Replace

```rust
        a_repeated_request_changes_nothing,
        a_request_to_an_accepted_contact_stays_accepted,
        contact_operations_reject_self_and_unknown_users,
        a_request_and_its_acceptance_are_announced_to_both_users,
        crossing_requests_announce_acceptance_to_both_users,
```

with

```rust
        a_repeated_request_changes_nothing,
        a_request_to_an_accepted_contact_stays_accepted,
        contact_operations_reject_self_and_treat_unknown_ids_as_strangers,
        a_request_and_its_acceptance_are_announced_to_both_users,
        crossing_requests_announce_acceptance_to_both_users,
```

Through the request frames. In `crates/push-message-svc/src/requests/tests.rs`:

Replace

```rust
    assert!(contacts(&world, &bob).await.is_empty());

    let stranger = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(UserId::new().0),
    });
    let status = world.request(&alice, stranger).await.unwrap_err();
    assert_eq!(failure(&status).0, ErrorCode::UserNotFound);
    let own = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(alice.user.0),
```

with

```rust
    assert!(contacts(&world, &bob).await.is_empty());

    // An id that is no user looks like someone who has not answered yet.
    let unknown = UserId::new();
    let stranger = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(unknown.0),
    });
    assert_eq!(
        world.request(&alice, stranger).await.unwrap(),
        response::Body::Contact(Contact {
            user_id: id_bytes(unknown.0),
            state: ContactState::RequestedOut.into(),
        })
    );
    let own = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(alice.user.0),
```

```powershell
cargo test -p push-testkit -p push-message-svc --lib -- contact
```

Expected: `a_request_to_an_unknown_id_looks_pending_and_stores_nothing`,
`contact_operations_reject_self_and_treat_unknown_ids_as_strangers` and the request test
FAIL with `Err(UnknownUser)` / `USER_NOT_FOUND`.

- [ ] **Step 2: Unknown peers answered as strangers**

In `crates/push-domain/src/contacts.rs`:

Replace

```rust
    N: Notifier,
{
    /// Sends a contact request. Returns the requester's resulting state.
    pub async fn contact_request(&self, from: UserId, to: UserId) -> DomainResult<ContactState> {
        let mut tx = self.begin_pair(from, to).await?;
        let mine = tx.get_contact(from, to).await?;
        let theirs = tx.get_contact(to, from).await?;
```

with

```rust
    N: Notifier,
{
    /// Sends a contact request. Returns the requester's resulting state. A request to an
    /// id that is not a verified user looks like a request nobody answered yet: it stores
    /// nothing and answers `RequestedOut`, so requests never reveal which ids exist.
    pub async fn contact_request(&self, from: UserId, to: UserId) -> DomainResult<ContactState> {
        let Some(mut tx) = self.begin_pair(from, to).await? else {
            return Ok(ContactState::RequestedOut);
        };
        let mine = tx.get_contact(from, to).await?;
        let theirs = tx.get_contact(to, from).await?;
```

Replace

```rust
    /// Accepts a request that `peer` sent to `me`.
    pub async fn contact_accept(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
```

with

```rust
    /// Accepts a request that `peer` sent to `me`.
    pub async fn contact_accept(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let Some(mut tx) = self.begin_pair(me, peer).await? else {
            return Err(DomainError::NoPendingRequest);
        };
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
```

Replace

```rust
    /// the same to them as a block.
    pub async fn contact_decline(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
```

with

```rust
    /// the same to them as a block.
    pub async fn contact_decline(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let Some(mut tx) = self.begin_pair(me, peer).await? else {
            return Err(DomainError::NoPendingRequest);
        };
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
```

Replace

```rust
    /// Blocks `peer`. Ends any contact; `peer`'s side is removed unless they also block.
    /// `peer` is never told.
    pub async fn contact_block(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        let blocked = Some(ContactState::Blocked);
        let mut events = Vec::new();
```

with

```rust
    /// Blocks `peer`. Ends any contact; `peer`'s side is removed unless they also block.
    /// `peer` is never told. Blocking an id that is not a verified user stores nothing.
    pub async fn contact_block(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let Some(mut tx) = self.begin_pair(me, peer).await? else {
            return Ok(());
        };
        let blocked = Some(ContactState::Blocked);
        let mut events = Vec::new();
```

Replace

```rust
    /// Removes a block. A request the block had hidden becomes an ordinary pending request.
    pub async fn contact_unblock(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        let mut events = Vec::new();
        if tx.get_contact(me, peer).await? == Some(ContactState::Blocked) {
```

with

```rust
    /// Removes a block. A request the block had hidden becomes an ordinary pending request.
    pub async fn contact_unblock(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let Some(mut tx) = self.begin_pair(me, peer).await? else {
            return Ok(());
        };
        let mut events = Vec::new();
        if tx.get_contact(me, peer).await? == Some(ContactState::Blocked) {
```

Replace

```rust
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
```

with

```rust
    }

    /// Begins a transaction for an operation of `me` on `peer`, two distinct users. `None`
    /// when `peer` is not a verified user: the operation then answers as it would for a
    /// user `me` has no relationship with, so no answer reveals which ids exist (spec
    /// §5.5). An unknown `me` is `UnknownUser`: the gateway only acts for known devices.
    async fn begin_pair(&self, me: UserId, peer: UserId) -> DomainResult<Option<S::Tx>> {
        if me == peer {
            return Err(ValidationError::SelfTarget.into());
        }
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(me).await? {
            return Err(DomainError::UnknownUser);
        }
        if !tx.user_exists(peer).await? {
            tx.rollback().await?;
            return Ok(None);
        }
        tx.lock_contact_pair(me, peer).await?;
        Ok(Some(tx))
    }
}
```

The contract says so. In `crates/push-proto/proto/push/v1/frame.proto`:

Replace

```protobuf
// Asks another user to become a contact (spec §5.5). If they already asked the caller,
// both become accepted contacts at once.
message ContactRequest {
  bytes user_id = 1;
```

with

```protobuf
// Asks another user to become a contact (spec §5.5). If they already asked the caller,
// both become accepted contacts at once. An id that is not a verified user is answered
// REQUESTED_OUT and nothing is stored, so requests never reveal which ids exist; Accept
// and Decline answer it NO_PENDING_REQUEST, Block and Unblock succeed.
message ContactRequest {
  bytes user_id = 1;
```

Replace

```protobuf
  // Any other rejection; `message` says which.
  ERROR_CODE_REJECTED = 10;
  // The other user does not exist (or has not verified their email yet).
  ERROR_CODE_USER_NOT_FOUND = 11;
  // Accept or Decline without a request from that user.
```

with

```protobuf
  // Any other rejection; `message` says which.
  ERROR_CODE_REJECTED = 10;
  // The caller is not a known user. Never answers about another user's id: requests
  // name unknown ids as strangers, so no answer reveals which ids exist.
  ERROR_CODE_USER_NOT_FOUND = 11;
  // Accept or Decline without a request from that user.
```

```powershell
cargo test -p push-testkit -p push-message-svc --lib
```

Expected: everything passes.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "fix(domain): contact requests never reveal which ids exist"
```

Expected: 823 tests pass (two scenarios renamed, run against both stores), no warnings, no
diff.

---

### Task 3: message-svc records password resets and refuses older tokens

**Files:**
- Create: `crates/push-store/migrations/0003_password_reset.sql`
- Modify: `crates/push-testkit/src/scenarios/devices.rs`, `src/contract/users.rs`,
  `src/world.rs`, `src/concurrency.rs`, `crates/push-domain/src/model.rs`, `src/lib.rs`,
  `src/error.rs`, `src/store.rs`, `src/memory.rs`, `src/service.rs`,
  `crates/push-store/src/store.rs`, `crates/push-proto/proto/push/internal/v1/messaging.proto`,
  `crates/push-message-svc/src/grpc.rs`, `src/convert.rs`, `tests/service.rs`,
  `crates/push-gateway/src/server.rs`, `crates/push-smoke/src/lib.rs`,
  `crates/push-worker/tests/relay.rs`, `tests/server.rs`

- [ ] **Step 1: The scenarios and the contract, failing**

`World::identify` identifies with a token issued now (by the world's clock); every
`world.svc.identify(device, user)` in the scenarios becomes `world.identify(device, user)`.
In `crates/push-testkit/src/world.rs`:

Replace

```rust
use chrono::{TimeZone, Utc};
use push_domain::{
    ClientMsgId, Conversation, ConversationId, DeleteMessage, DeviceId, EditText, Entry, Event,
    ManualClock, Member, MessageRef, Messaging, Policy, RecordingNotifier, SendReceipt, SendText,
    Store, SystemEvent, Tx, UserId,
};
```

with

```rust
use chrono::{TimeZone, Utc};
use push_domain::{
    ClientMsgId, Clock, Conversation, ConversationId, DeleteMessage, DeviceId, DomainResult,
    EditText, Entry, Event, ManualClock, Member, MessageRef, Messaging, Policy, RecordingNotifier,
    SendReceipt, SendText, Store, SystemEvent, Tx, UserId,
};
```

Replace

```rust
    }

    /// The delivery position of a device, read straight from the store.
    pub async fn position(&self, device: DeviceId, conversation: ConversationId) -> Option<i64> {
```

with

```rust
    }

    /// Identifies `device` of `user` with an access token issued now.
    pub async fn identify(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        self.svc.identify(device, user, self.clock.now()).await
    }

    /// The delivery position of a device, read straight from the store.
    pub async fn position(&self, device: DeviceId, conversation: ConversationId) -> Option<i64> {
```

In `crates/push-testkit/src/concurrency.rs`:

Replace

```rust
        let loser = 1 - winner;
        assert_eq!(results[loser], Err(DomainError::UnknownDevice));
        assert_eq!(world.svc.identify(device, users[winner]).await, Ok(()));
        assert_eq!(
            world.svc.identify(device, users[loser]).await,
            Err(DomainError::UnknownDevice)
        );
```

with

```rust
        let loser = 1 - winner;
        assert_eq!(results[loser], Err(DomainError::UnknownDevice));
        assert_eq!(world.identify(device, users[winner]).await, Ok(()));
        assert_eq!(
            world.identify(device, users[loser]).await,
            Err(DomainError::UnknownDevice)
        );
```

Tokens issued until the reset's second are refused, a later one passes, other users are
untouched; a reset relayed before the device still counts, and a repeat moves the time
forward (decision 2). In `crates/push-testkit/src/scenarios/devices.rs`:

Replace

```rust
use crate::{Backend, Case, World, bodies, case_list};
use push_domain::{DeviceId, DomainError, Event, KickReason, UserId};

async fn registering_a_device_for_an_unknown_user_fails<B: Backend>(backend: &B) {
```

with

```rust
use chrono::TimeDelta;
use push_domain::{Clock, DeviceId, DomainError, Event, KickReason, UserId};

use crate::{Backend, Case, World, bodies, case_list};

async fn registering_a_device_for_an_unknown_user_fails<B: Backend>(backend: &B) {
```

Replace

```rust
    let world = World::new(backend).await;
    let alice = world.person().await;
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}
```

with

```rust
    let world = World::new(backend).await;
    let alice = world.person().await;
    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
}
```

Replace

```rust
    let bob = world.person().await;
    assert_eq!(
        world.svc.identify(bob.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
```

with

```rust
    let bob = world.person().await;
    assert_eq!(
        world.identify(bob.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
```

Replace

```rust
    world.svc.revoke_device(alice.device).await.unwrap();
    assert_eq!(
        world.svc.identify(alice.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
```

with

```rust
    world.svc.revoke_device(alice.device).await.unwrap();
    assert_eq!(
        world.identify(alice.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
```

Replace

```rust
    let world = World::new(backend).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    let refused = world.svc.identify(device, user).await.unwrap_err();
    assert_eq!(refused, DomainError::DeviceNotKnownYet);
    assert!(refused.is_retryable());
    world.svc.device_registered(device, user).await.unwrap();
    assert_eq!(world.svc.identify(device, user).await, Ok(()));
}
```

with

```rust
    let world = World::new(backend).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    let refused = world.identify(device, user).await.unwrap_err();
    assert_eq!(refused, DomainError::DeviceNotKnownYet);
    assert!(refused.is_retryable());
    world.svc.device_registered(device, user).await.unwrap();
    assert_eq!(world.identify(device, user).await, Ok(()));
}
```

Replace

```rust
        .await
        .unwrap();
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}
```

with

```rust
        .await
        .unwrap();
    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
}
```

Replace

```rust
    world.svc.device_registered(device, user).await.unwrap();
    world.svc.register_user(user).await.unwrap();
    assert_eq!(world.svc.identify(device, user).await, Ok(()));
}
```

with

```rust
    world.svc.device_registered(device, user).await.unwrap();
    world.svc.register_user(user).await.unwrap();
    assert_eq!(world.identify(device, user).await, Ok(()));
}
```

Replace

```rust
    world.svc.device_revoked(device, user).await.unwrap();
    assert_eq!(
        world.svc.identify(device, user).await,
        Err(DomainError::UnknownDevice)
    );
```

with

```rust
    world.svc.device_revoked(device, user).await.unwrap();
    assert_eq!(
        world.identify(device, user).await,
        Err(DomainError::UnknownDevice)
    );
```

Replace

```rust
    world.svc.device_revoked(tablet, alice.user).await.unwrap();
    assert_eq!(
        world.svc.identify(tablet, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}
```

with

```rust
    world.svc.device_revoked(tablet, alice.user).await.unwrap();
    assert_eq!(
        world.identify(tablet, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
}
```

Replace

```rust
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}
```

with

```rust
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(world.identify(alice.device, alice.user).await, Ok(()));
}
```

Replace

```rust
    );
    // The devices stay registered: a new sign-in on them works.
    assert_eq!(world.svc.identify(laptop, alice.user).await, Ok(()));
    assert_eq!(world.svc.identify(bob.device, bob.user).await, Ok(()));

    // A user message-svc has not heard of has nothing to kick.
```

with

```rust
    );
    // The devices stay registered: a new sign-in on them works.
    world.clock.advance(TimeDelta::seconds(1));
    assert_eq!(world.identify(laptop, alice.user).await, Ok(()));
    assert_eq!(world.identify(bob.device, bob.user).await, Ok(()));

    // A user message-svc has not heard of has nothing to kick.
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

/// A token that outlives the reset (up to 15 minutes), on a device whose kick was lost,
/// cannot sign in again; a token from a later second can.
async fn identify_refuses_tokens_issued_until_the_password_reset<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let before = world.clock.now() - TimeDelta::minutes(10);
    world.svc.password_reset(alice.user).await.unwrap();
    let refused = world.svc.identify(alice.device, alice.user, before).await;
    assert_eq!(refused, Err(DomainError::PasswordChanged));
    assert!(!DomainError::PasswordChanged.is_retryable());
    // Within the second of the reset the order is unknown: refused too.
    assert_eq!(
        world.identify(alice.device, alice.user).await,
        Err(DomainError::PasswordChanged)
    );
    let after = world.clock.now() + TimeDelta::seconds(1);
    assert_eq!(
        world.svc.identify(alice.device, alice.user, after).await,
        Ok(())
    );
    // Other users are not affected.
    assert_eq!(
        world.svc.identify(bob.device, bob.user, before).await,
        Ok(())
    );
}

/// The reset can be relayed before the user's device; a repeat moves the time forward.
async fn a_password_reset_counts_in_any_order_and_the_latest_wins<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    world.svc.password_reset(user).await.unwrap();
    world.svc.device_registered(device, user).await.unwrap();
    let first = world.clock.now();
    assert_eq!(
        world.svc.identify(device, user, first).await,
        Err(DomainError::PasswordChanged)
    );
    world.clock.advance(TimeDelta::minutes(1));
    world.svc.password_reset(user).await.unwrap();
    let between = first + TimeDelta::seconds(30);
    assert_eq!(
        world.svc.identify(device, user, between).await,
        Err(DomainError::PasswordChanged)
    );
    world.clock.advance(TimeDelta::seconds(1));
    assert_eq!(world.identify(device, user).await, Ok(()));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

Replace

```rust
        a_relayed_revocation_kicks_the_device,
        a_password_reset_kicks_every_active_device_of_the_user,
    ]
}
```

with

```rust
        a_relayed_revocation_kicks_the_device,
        a_password_reset_kicks_every_active_device_of_the_user,
        identify_refuses_tokens_issued_until_the_password_reset,
        a_password_reset_counts_in_any_order_and_the_latest_wins,
    ]
}
```

The store reads the reset with the device, in one statement, and keeps the latest. In
`crates/push-testkit/src/contract/users.rs`:

Replace

```rust
use push_domain::{DeviceId, DomainError, Store, Tx, UserId};

use super::add_users;
```

with

```rust
use chrono::{TimeZone, Utc};
use push_domain::{DeviceId, DeviceState, DomainError, Store, Tx, UserId};

use super::add_users;
```

Replace

```rust
}

async fn device_state_reads_owner_and_activity_together<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (user, other) = (UserId::new(), UserId::new());
    add_users(&store, &[user, other]).await;
    let (active, revoked) = (DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(active, user).await.unwrap();
    tx.insert_device(revoked, other).await.unwrap();
    tx.revoke_device(revoked).await.unwrap();

    assert_eq!(tx.device_state(active).await.unwrap(), Some((user, true)));
    assert_eq!(
        tx.device_state(revoked).await.unwrap(),
        Some((other, false))
    );
    assert_eq!(tx.device_state(DeviceId::new()).await.unwrap(), None);
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

with

```rust
}

async fn device_state_reads_owner_activity_and_reset_together<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (user, other) = (UserId::new(), UserId::new());
    add_users(&store, &[user, other]).await;
    let (active, revoked) = (DeviceId::new(), DeviceId::new());
    let reset = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(active, user).await.unwrap();
    tx.insert_device(revoked, other).await.unwrap();
    tx.revoke_device(revoked).await.unwrap();
    tx.record_password_reset(user, reset).await.unwrap();

    let state = |user, active, password_reset_at| {
        Some(DeviceState {
            user,
            active,
            password_reset_at,
        })
    };
    assert_eq!(
        tx.device_state(active).await.unwrap(),
        state(user, true, Some(reset))
    );
    assert_eq!(
        tx.device_state(revoked).await.unwrap(),
        state(other, false, None)
    );
    assert_eq!(tx.device_state(DeviceId::new()).await.unwrap(), None);
}

async fn a_password_reset_keeps_the_latest_time_and_needs_the_user<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let device = DeviceId::new();
    let (earlier, later) = (
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap(),
    );
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(device, user).await.unwrap();
    tx.record_password_reset(user, later).await.unwrap();
    tx.record_password_reset(user, earlier).await.unwrap();
    let reset = tx
        .device_state(device)
        .await
        .unwrap()
        .unwrap()
        .password_reset_at;
    assert_eq!(reset, Some(later));
    assert!(matches!(
        tx.record_password_reset(UserId::new(), later).await,
        Err(DomainError::Store(_))
    ));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

Replace

```rust
        revoking_an_unknown_device_does_nothing,
        device_user_names_the_owner_of_revoked_devices_too,
        device_state_reads_owner_and_activity_together,
    ]
}
```

with

```rust
        revoking_an_unknown_device_does_nothing,
        device_user_names_the_owner_of_revoked_devices_too,
        device_state_reads_owner_activity_and_reset_together,
        a_password_reset_keeps_the_latest_time_and_needs_the_user,
    ]
}
```

```powershell
cargo test -p push-testkit
```

Expected: compile errors — `Messaging::identify` takes two arguments, there is no
`DeviceState`, no `Tx::record_password_reset` and no `DomainError::PasswordChanged`.

- [ ] **Step 2: The reset in the domain and both stores**

In `crates/push-domain/src/model.rs`:

Replace

```rust
}

/// A stored invite (spec §5.5). Redeeming it makes the redeemer an accepted contact of
/// its creator.
```

with

```rust
}

/// What `Identify` reads about a connecting device, in one statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceState {
    /// The owner.
    pub user: UserId,
    /// Not revoked.
    pub active: bool,
    /// When message-svc last heard that the owner's password was reset, if ever.
    pub password_reset_at: Option<DateTime<Utc>>,
}

/// A stored invite (spec §5.5). Redeeming it makes the redeemer an accepted contact of
/// its creator.
```

In `crates/push-domain/src/lib.rs`:

Replace

```rust
pub use model::{
    ContactState, ContactSummary, ContentType, Conversation, ConversationKind,
    ConversationOverview, ConversationState, ConversationSummary, Entry, EntryKind, GroupPositions,
    Invite, InviteRecord, Member, MemberState, Positions, Receipts, Role, SendReceipt, direct_key,
};
pub use notify::{Event, KickReason, NoopNotifier, Notifier, ReceiptUpdate, RecordingNotifier};
```

with

```rust
pub use model::{
    ContactState, ContactSummary, ContentType, Conversation, ConversationKind,
    ConversationOverview, ConversationState, ConversationSummary, DeviceState, Entry, EntryKind,
    GroupPositions, Invite, InviteRecord, Member, MemberState, Positions, Receipts, Role,
    SendReceipt, direct_key,
};
pub use notify::{Event, KickReason, NoopNotifier, Notifier, ReceiptUpdate, RecordingNotifier};
```

In `crates/push-domain/src/error.rs`:

Replace

```rust
    #[error("the device is not known yet; retry shortly")]
    DeviceNotKnownYet,
    #[error("conversation not found")]
    ConversationNotFound,
```

with

```rust
    #[error("the device is not known yet; retry shortly")]
    DeviceNotKnownYet,
    /// The access token was issued no later than the user's last password reset that
    /// message-svc heard of: the device must sign in again.
    #[error("the password was changed after this sign-in; sign in again")]
    PasswordChanged,
    #[error("conversation not found")]
    ConversationNotFound,
```

In `crates/push-domain/src/store.rs`:

Replace

```rust
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{
    ContactState, Conversation, ConversationOverview, Entry, GroupPositions, InviteRecord, Member,
};
```

with

```rust
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{
    ContactState, Conversation, ConversationOverview, DeviceState, Entry, GroupPositions,
    InviteRecord, Member,
};
```

Replace

```rust
    async fn insert_user(&mut self, user: UserId) -> DomainResult<()>;
    async fn user_exists(&mut self, user: UserId) -> DomainResult<bool>;
    /// Records an active device of `user` unless a device with this id exists (any owner,
    /// active or revoked), and returns whether the device is now an active device of
```

with

```rust
    async fn insert_user(&mut self, user: UserId) -> DomainResult<()>;
    async fn user_exists(&mut self, user: UserId) -> DomainResult<bool>;
    /// Records that `user`'s password was reset at `at`, keeping the latest time. The user
    /// must exist.
    async fn record_password_reset(&mut self, user: UserId, at: DateTime<Utc>) -> DomainResult<()>;
    /// Records an active device of `user` unless a device with this id exists (any owner,
    /// active or revoked), and returns whether the device is now an active device of
```

Replace

```rust
    /// The owner of a device, active or revoked; `None` if the device is unknown.
    async fn device_user(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
    /// The owner of a device and whether the device is still active, read in one
    /// statement so the two cannot disagree; `None` if the device is unknown.
    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<(UserId, bool)>>;
    /// The owner of an active device; `None` if the device is unknown or revoked.
    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
```

with

```rust
    /// The owner of a device, active or revoked; `None` if the device is unknown.
    async fn device_user(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
    /// The owner of a device, whether the device is still active and the owner's last
    /// password reset, read in one statement so they cannot disagree; `None` if the device
    /// is unknown.
    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<DeviceState>>;
    /// The owner of an active device; `None` if the device is unknown or revoked.
    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
```

In `crates/push-domain/src/memory.rs`:

Replace

```rust
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{
    ContactState, Conversation, ConversationKind, ConversationOverview, Entry, EntryKind,
    GroupPositions, InviteRecord, Member, Role,
};
use crate::store::{InsertOutcome, Store, Tx};
```

with

```rust
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{
    ContactState, Conversation, ConversationKind, ConversationOverview, DeviceState, Entry,
    EntryKind, GroupPositions, InviteRecord, Member, Role,
};
use crate::store::{InsertOutcome, Store, Tx};
```

Replace

```rust
struct State {
    users: HashSet<UserId>,
    devices: HashMap<DeviceId, DeviceRow>,
    contacts: HashMap<(UserId, UserId), ContactState>,
```

with

```rust
struct State {
    users: HashSet<UserId>,
    /// The last password reset message-svc heard of, per user.
    password_resets: HashMap<UserId, DateTime<Utc>>,
    devices: HashMap<DeviceId, DeviceRow>,
    contacts: HashMap<(UserId, UserId), ContactState>,
```

Replace

```rust
    }

    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool> {
        if let Some(row) = self.work.devices.get(&device) {
```

with

```rust
    }

    async fn record_password_reset(&mut self, user: UserId, at: DateTime<Utc>) -> DomainResult<()> {
        if !self.work.users.contains(&user) {
            return Err(missing("user"));
        }
        let latest = self.work.password_resets.entry(user).or_insert(at);
        *latest = (*latest).max(at);
        Ok(())
    }

    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool> {
        if let Some(row) = self.work.devices.get(&device) {
```

Replace

```rust
    }

    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<(UserId, bool)>> {
        Ok(self
            .work
            .devices
            .get(&device)
            .map(|row| (row.user, !row.revoked)))
    }
```

with

```rust
    }

    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<DeviceState>> {
        Ok(self.work.devices.get(&device).map(|row| DeviceState {
            user: row.user,
            active: !row.revoked,
            password_reset_at: self.work.password_resets.get(&row.user).copied(),
        }))
    }
```

`password_reset` now records the user and the time, commits, then kicks; `identify` takes
the token's `iat`. In `crates/push-domain/src/service.rs`:

Replace

```rust
use chrono::TimeDelta;

use crate::clock::Clock;
```

with

```rust
use chrono::{DateTime, TimeDelta, Utc};

use crate::clock::Clock;
```

Replace

```rust
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
```

with

```rust
    /// auth-svc reset `user`'s password (`user.password_reset`, relayed by the worker):
    /// every active device of the user is kicked, to sign in again with the new password,
    /// and [`Self::identify`] refuses access tokens issued until now, so a token that
    /// outlives the reset (up to 15 minutes) or a lost kick cannot keep a device signed in.
    /// The devices stay registered. Idempotent in effect and correct in any order: the
    /// user is recorded if message-svc has not heard of them yet, and a repeated event
    /// moves the time forward and kicks again, which only asks for one more sign-in.
    pub async fn password_reset(&self, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.insert_user(user).await?;
        tx.record_password_reset(user, self.clock.now()).await?;
        let mut devices = tx.devices_of_user(user).await?;
        tx.commit().await?;
        if !devices.is_empty() {
            devices.sort();
```

Replace

```rust
    }

    /// Confirms that a connecting device is active and belongs to the user in its token.
    /// A device never heard of is `DeviceNotKnownYet` (retryable): auth-svc registered it
    /// and the worker has not relayed it yet. A revoked device, or one of another user, is
    /// `UnknownDevice`.
    pub async fn identify(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        // One statement: reading the owner and the revocation separately could straddle
```

with

```rust
    }

    /// Confirms that a connecting device is active and belongs to the user in its token,
    /// a token issued at `issued_at` (its `iat`, whole seconds). A device never heard of is
    /// `DeviceNotKnownYet` (retryable): auth-svc registered it and the worker has not
    /// relayed it yet. A revoked device, or one of another user, is `UnknownDevice`. A
    /// token issued no later than the second of the user's last password reset is
    /// `PasswordChanged`: within that second the order is unknown, and refusing a token
    /// minted just after the reset only asks for one more sign-in.
    pub async fn identify(
        &self,
        device: DeviceId,
        user: UserId,
        issued_at: DateTime<Utc>,
    ) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        // One statement: reading the owner and the revocation separately could straddle
```

Replace

```rust
        let state = tx.device_state(device).await?;
        tx.rollback().await?;
        match state {
            None => Err(DomainError::DeviceNotKnownYet),
            Some((owner, is_active)) if owner == user && is_active => Ok(()),
            Some(_) => Err(DomainError::UnknownDevice),
        }
    }
```

with

```rust
        let state = tx.device_state(device).await?;
        tx.rollback().await?;
        let Some(state) = state else {
            return Err(DomainError::DeviceNotKnownYet);
        };
        if state.user != user || !state.active {
            return Err(DomainError::UnknownDevice);
        }
        match state.password_reset_at {
            Some(reset) if issued_at.timestamp() <= reset.timestamp() => {
                Err(DomainError::PasswordChanged)
            }
            _ => Ok(()),
        }
    }
```

Create `crates/push-store/migrations/0003_password_reset.sql`:

```sql
-- When message-svc last heard (from auth-svc, through the worker) that the user's password
-- was reset. Identify refuses access tokens issued no later than that second, so a token
-- that outlives the reset, or a kick lost on Redis, cannot keep a device signed in.
ALTER TABLE messaging.known_user ADD COLUMN password_reset_at timestamptz;
```

In `crates/push-store/src/store.rs`:

Replace

```rust
use push_domain::{
    ClientMsgId, ContactState, Conversation, ConversationId, ConversationOverview, DeviceId,
    DomainError, DomainResult, Entry, EntryKind, GroupPositions, InsertOutcome, InviteRecord,
    Member, MessageId, Store, Tx, UserId,
};
use sqlx::postgres::{PgPool, PgRow};
```

with

```rust
use push_domain::{
    ClientMsgId, ContactState, Conversation, ConversationId, ConversationOverview, DeviceId,
    DeviceState, DomainError, DomainResult, Entry, EntryKind, GroupPositions, InsertOutcome,
    InviteRecord, Member, MessageId, Store, Tx, UserId,
};
use sqlx::postgres::{PgPool, PgRow};
```

Replace

```rust
    }

    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool> {
        // A concurrent insert of the same id makes this wait for its outcome, so the check
```

with

```rust
    }

    async fn record_password_reset(&mut self, user: UserId, at: DateTime<Utc>) -> DomainResult<()> {
        // greatest() skips a NULL, so the first reset is recorded as is.
        let updated = sqlx::query(
            "UPDATE messaging.known_user \
             SET password_reset_at = greatest(password_reset_at, $2) WHERE user_id = $1",
        )
        .bind(user.0)
        .bind(at)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        if updated.rows_affected() == 0 {
            return Err(DomainError::Store(
                "foreign key violation: user does not exist".to_owned(),
            ));
        }
        Ok(())
    }

    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool> {
        // A concurrent insert of the same id makes this wait for its outcome, so the check
```

Replace

```rust
    }

    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<(UserId, bool)>> {
        let row: Option<(Uuid, bool)> = sqlx::query_as(
            "SELECT user_id, revoked_at IS NULL FROM messaging.member_device \
             WHERE device_id = $1",
        )
        .bind(device.0)
```

with

```rust
    }

    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<DeviceState>> {
        let row: Option<(Uuid, bool, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT d.user_id, d.revoked_at IS NULL, u.password_reset_at \
             FROM messaging.member_device d JOIN messaging.known_user u USING (user_id) \
             WHERE d.device_id = $1",
        )
        .bind(device.0)
```

Replace

```rust
        .await
        .map_err(store_error)?;
        Ok(row.map(|(owner, is_active)| (UserId(owner), is_active)))
    }
```

with

```rust
        .await
        .map_err(store_error)?;
        Ok(row.map(|(owner, active, password_reset_at)| DeviceState {
            user: UserId(owner),
            active,
            password_reset_at,
        }))
    }
```

```powershell
cargo test -p push-testkit -p push-store
```

Expected: everything passes against the in-memory store and Postgres, the two new
scenarios and the two new contract cases included.

- [ ] **Step 3: `Identify` takes the token's `iat`**

In `crates/push-proto/proto/push/internal/v1/messaging.proto`:

Replace

```protobuf
service Messaging {
  // Confirms that the device is active and belongs to the user. A revoked device, or one
  // of another user, is PERMISSION_DENIED (UNKNOWN_DEVICE); a device message-svc has never
  // heard of is UNAVAILABLE (retryable): auth-svc registered it and the worker has not
  // relayed it yet, which takes about a second.
  rpc Identify(IdentifyRequest) returns (IdentifyResponse);
  // Appends a text message; answers after commit.
```

with

```protobuf
service Messaging {
  // Confirms that the device is active and belongs to the user, and that the access token
  // is newer than the user's last password reset. A revoked device, or one of another
  // user, is PERMISSION_DENIED (UNKNOWN_DEVICE); a token issued no later than the second
  // of the last reset message-svc heard of is UNAUTHENTICATED (PASSWORD_CHANGED); a device
  // message-svc has never heard of is UNAVAILABLE (DEVICE_NOT_READY, retryable): auth-svc
  // registered it and the worker has not relayed it yet, which takes about a second.
  rpc Identify(IdentifyRequest) returns (IdentifyResponse);
  // Appends a text message; answers after commit.
```

Replace

```protobuf
  rpc DeviceRevoked(AccountDevice) returns (AccountsResponse);
  // user.password_reset: every active device of the user is kicked
  // (`kick:{device_id}`), to sign in again. The gateway closes kicked sockets and refuses
  // tokens older than the reset in Plan 8.
  rpc PasswordReset(AccountUser) returns (AccountsResponse);
}
```

with

```protobuf
  rpc DeviceRevoked(AccountDevice) returns (AccountsResponse);
  // user.password_reset: every active device of the user is kicked
  // (`kick:{device_id}`), to sign in again, and Identify refuses the access tokens issued
  // until now. A repeated event moves that time forward.
  rpc PasswordReset(AccountUser) returns (AccountsResponse);
}
```

Replace

```protobuf
message IdentifyRequest {
  bytes user_id = 1;
  bytes device_id = 2;
}

```

with

```protobuf
message IdentifyRequest {
  bytes user_id = 1;
  bytes device_id = 2;
  // The access token's `iat`, in seconds since the Unix epoch.
  int64 issued_at = 3;
}

```

`DeviceNotKnownYet` gets a code of its own (`DEVICE_NOT_READY`, still `UNAVAILABLE` and
retryable) and `PasswordChanged` maps to `UNAUTHENTICATED` (decision 5). In
`crates/push-message-svc/src/convert.rs`:

Replace

```rust
        DomainError::UnknownUser => (Code::NotFound, ErrorCode::UserNotFound),
        DomainError::UnknownDevice => (Code::PermissionDenied, ErrorCode::UnknownDevice),
        DomainError::DeviceNotKnownYet => (Code::Unavailable, ErrorCode::Unavailable),
        DomainError::ConversationNotFound => (Code::NotFound, ErrorCode::ConversationNotFound),
        DomainError::NotMember => (Code::FailedPrecondition, ErrorCode::NotMember),
```

with

```rust
        DomainError::UnknownUser => (Code::NotFound, ErrorCode::UserNotFound),
        DomainError::UnknownDevice => (Code::PermissionDenied, ErrorCode::UnknownDevice),
        DomainError::DeviceNotKnownYet => (Code::Unavailable, ErrorCode::DeviceNotReady),
        DomainError::PasswordChanged => (Code::Unauthenticated, ErrorCode::PasswordChanged),
        DomainError::ConversationNotFound => (Code::NotFound, ErrorCode::ConversationNotFound),
        DomainError::NotMember => (Code::FailedPrecondition, ErrorCode::NotMember),
```

Replace

```rust
                DomainError::DeviceNotKnownYet,
                Code::Unavailable,
                ErrorCode::Unavailable,
            ),
            (
```

with

```rust
                DomainError::DeviceNotKnownYet,
                Code::Unavailable,
                ErrorCode::DeviceNotReady,
            ),
            (
                DomainError::PasswordChanged,
                Code::Unauthenticated,
                ErrorCode::PasswordChanged,
            ),
            (
```

Replace

```rust
        }
        // Only the retryable errors share a client code.
        assert_eq!(client_codes.len(), 20);

        let store = error_from_status(&domain_error(DomainError::Store("relation".into())));
```

with

```rust
        }
        // Only the retryable errors share a client code.
        assert_eq!(client_codes.len(), 22);

        let store = error_from_status(&domain_error(DomainError::Store("relation".into())));
```

In `crates/push-message-svc/src/grpc.rs` (the parse and its tests: a token from before a
relayed reset is refused, one from a second later passes):

Replace

```rust
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        self.messaging
            .identify(device, user)
            .await
            .map_err(domain_error)?;
```

with

```rust
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        let issued_at = chrono::DateTime::from_timestamp(request.issued_at, 0)
            .ok_or_else(|| invalid("issued_at is out of range"))?;
        self.messaging
            .identify(device, user, issued_at)
            .await
            .map_err(domain_error)?;
```

Replace

```rust
                user_id: id_bytes(alice.user.0),
                device_id: id_bytes(alice.device.0),
            }))
            .await;
```

with

```rust
                user_id: id_bytes(alice.user.0),
                device_id: id_bytes(alice.device.0),
                issued_at: now(),
            }))
            .await;
```

Replace

```rust
                user_id: id_bytes(alice.user.0),
                device_id: id_bytes(bob.device.0),
            }))
            .await
```

with

```rust
                user_id: id_bytes(alice.user.0),
                device_id: id_bytes(bob.device.0),
                issued_at: now(),
            }))
            .await
```

Replace

```rust
        let identify = |user_id: Vec<u8>, device_id: Vec<u8>| {
            Request::new(IdentifyRequest { user_id, device_id })
        };
        let status = world
```

with

```rust
        let identify = |user_id: Vec<u8>, device_id: Vec<u8>| {
            Request::new(IdentifyRequest {
                user_id,
                device_id,
                issued_at: now(),
            })
        };
        let status = world
```

Replace

```rust
    }

    async fn identify(world: &World, user: UserId, device: DeviceId) -> Result<(), Code> {
        world
            .api
```

with

```rust
    }

    /// Seconds since the Unix epoch, as an access token's `iat`.
    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    async fn identify(world: &World, user: UserId, device: DeviceId) -> Result<(), Code> {
        identify_at(world, user, device, now()).await
    }

    /// Identify with a token issued at `issued_at` (seconds); the client error code of a
    /// refusal.
    async fn identify_at(
        world: &World,
        user: UserId,
        device: DeviceId,
        issued_at: i64,
    ) -> Result<(), Code> {
        world
            .api
```

Replace

```rust
                user_id: id_bytes(user.0),
                device_id: id_bytes(device.0),
            }))
            .await
```

with

```rust
                user_id: id_bytes(user.0),
                device_id: id_bytes(device.0),
                issued_at,
            }))
            .await
```

Replace

```rust
        assert_eq!(identify(&world, user, phone).await, Ok(()));
        // A device not relayed yet is worth retrying, unlike a revoked one.
        assert_eq!(identify(&world, user, tablet).await, Err(Code::Unavailable));

        // The revocation of the tablet overtakes its registration.
```

with

```rust
        assert_eq!(identify(&world, user, phone).await, Ok(()));
        // A device not relayed yet is worth retrying, unlike a revoked one.
        let status = world
            .api
            .identify(Request::new(IdentifyRequest {
                user_id: id_bytes(user.0),
                device_id: id_bytes(tablet.0),
                issued_at: now(),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unavailable);
        let error = error_from_status(&status);
        assert_eq!(error.code(), ErrorCode::DeviceNotReady);
        assert!(error.retryable);

        // The revocation of the tablet overtakes its registration.
```

Replace

```rust
            }]
        );
        assert_eq!(identify(&world, user, phone).await, Ok(()));
    }
```

with

```rust
            }]
        );
        // Tokens issued until the reset sign in no more; a later one does. The phone stays
        // registered.
        let status = world
            .api
            .identify(Request::new(IdentifyRequest {
                user_id: id_bytes(user.0),
                device_id: id_bytes(phone.0),
                issued_at: now() - 60,
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);
        let error = error_from_status(&status);
        assert_eq!(error.code(), ErrorCode::PasswordChanged);
        assert!(!error.retryable);
        assert_eq!(identify_at(&world, user, phone, now() + 1).await, Ok(()));
    }
```

The callers send an issue time. In `crates/push-message-svc/tests/service.rs`:

Replace

```rust
        device_id: id_bytes(device),
    };
    let identify = IdentifyRequest {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
    };
    // Not relayed yet: worth retrying, unlike a revoked device.
```

with

```rust
        device_id: id_bytes(device),
    };
    // No password reset in this test: any issue time passes.
    let identify = IdentifyRequest {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
        issued_at: 0,
    };
    // Not relayed yet: worth retrying, unlike a revoked device.
```

In `crates/push-worker/tests/relay.rs` (its test relays a reset before it identifies):

Replace

```rust
}

async fn identifies(world: &World, user: Uuid, device: Uuid) -> bool {
    world
        .messaging
```

with

```rust
}

/// Whether message-svc accepts the device, with an access token issued a second from now:
/// after any password reset relayed so far.
async fn identifies(world: &World, user: Uuid, device: Uuid) -> bool {
    let issued_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 1;
    world
        .messaging
```

Replace

```rust
            user_id: id_bytes(user),
            device_id: id_bytes(device),
        })
        .await
```

with

```rust
            user_id: id_bytes(user),
            device_id: id_bytes(device),
            issued_at,
        })
        .await
```

In `crates/push-worker/tests/server.rs`:

Replace

```rust
                    user_id: id_bytes(user),
                    device_id: id_bytes(device),
                })
                .await
```

with

```rust
                    user_id: id_bytes(user),
                    device_id: id_bytes(device),
                    // No password reset in this test: any issue time passes.
                    issued_at: 0,
                })
                .await
```

In `crates/push-smoke/src/lib.rs`:

Replace

```rust
            user_id: id_bytes(carol.user),
            device_id: id_bytes(carol.device),
        })
        .await;
```

with

```rust
            user_id: id_bytes(carol.user),
            device_id: id_bytes(carol.device),
            // Carol never reset her password.
            issued_at: 0,
        })
        .await;
```

Replace

```rust
                    user_id: id_bytes(user),
                    device_id: id_bytes(device),
                })
                .await;
```

with

```rust
                    user_id: id_bytes(user),
                    device_id: id_bytes(device),
                    // New users: no password reset yet.
                    issued_at: 0,
                })
                .await;
```

In `crates/push-gateway/src/server.rs` (until Task 5 the DEV ONLY identity has no token):

Replace

```rust
        }
    };
    let request = IdentifyRequest {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
    };
    match app.messaging.clone().identify(request).await {
```

with

```rust
        }
    };
    // DEV ONLY identities carry no token; they count as issued now.
    let issued_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let request = IdentifyRequest {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
        issued_at,
    };
    match app.messaging.clone().identify(request).await {
```

```powershell
cargo test -p push-message-svc -p push-worker
```

Expected: everything passes, among them `every_domain_error_has_its_own_status_and_client_error`
(22 client codes now) and
`relayed_account_events_register_and_revoke_devices_in_any_order`.

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "feat(message-svc): Identify refuses tokens issued until the last password reset"
```

Expected: 829 tests pass, no warnings, no diff.

---

### Task 4: The gateway checks access tokens against auth-svc's key set

**Files:**
- Create: `crates/push-gateway/src/tokens.rs`
- Modify: `crates/push-gateway/Cargo.toml`, `src/lib.rs`

- [ ] **Step 1: The dependencies**

In `crates/push-gateway/Cargo.toml` (`push-auth` only for tests: its real token issuer):

Replace

```toml
anyhow.workspace = true
axum = { workspace = true, features = ["ws"] }
prost.workspace = true
push-proto.workspace = true
redis.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
```

with

```toml
anyhow.workspace = true
axum = { workspace = true, features = ["ws"] }
jsonwebtoken.workspace = true
prost.workspace = true
push-proto.workspace = true
redis.workspace = true
reqwest.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
```

Replace

```toml
[dev-dependencies]
futures-util.workspace = true
push-domain.workspace = true
push-message-svc.workspace = true
```

with

```toml
[dev-dependencies]
futures-util.workspace = true
push-auth.workspace = true
push-domain.workspace = true
push-message-svc.workspace = true
```

In `crates/push-gateway/src/lib.rs`:

Replace

```rust
mod server;
mod session;

pub use config::{Config, ConfigError};
```

with

```rust
mod server;
mod session;
mod tokens;

pub use config::{Config, ConfigError};
```

Replace

```rust
pub use server::{init_logging, serve, terminated};
pub use session::{RetryDelay, Session, Step};
```

with

```rust
pub use server::{init_logging, serve, terminated};
pub use session::{RetryDelay, Session, Step};
pub use tokens::{Claims, LEEWAY, REFETCH_INTERVAL, TokenError, Verifier};
```

- [ ] **Step 2: The checker and its tests**

Create `crates/push-gateway/src/tokens.rs` (decision 3; the tests issue tokens with
auth-svc's own `AccessTokens`, serve its JWKS from a local axum route, and sign the broken
ones with the same seed):

```rust
//! Checking access tokens without calling auth-svc (spec §5.1, §7.3). auth-svc signs them
//! with Ed25519 (EdDSA) and publishes its public keys as a JSON Web Key Set at
//! `GET /v1/auth/jwks`. The gateway keeps those keys by `kid` and fetches the set again
//! when a token names a key it does not know, so auth-svc can rotate its key without the
//! gateway restarting.
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
    keys: RwLock<HashMap<String, DecodingKey>>,
    /// Serialises fetches, and holds when a token naming an unknown key last fetched.
    refetched: tokio::sync::Mutex<Option<Instant>>,
    refetch_interval: Duration,
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
            }),
        }
    }

    /// Whether any key is loaded, so tokens can be checked.
    pub fn is_ready(&self) -> bool {
        !self.read_keys().is_empty()
    }

    /// The ids of the loaded keys, sorted.
    pub fn key_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.read_keys().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Fetches the key set and replaces the keys with its Ed25519 keys; keys of other
    /// kinds, and keys without a `kid`, are skipped. A failed fetch keeps the keys. The
    /// error never quotes the response.
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
        let set: KeySet = response
            .json()
            .await
            .map_err(|_| "the key set is not a JSON Web Key Set".to_owned())?;
        let keys = decoding_keys(set);
        if keys.is_empty() {
            return Err("the key set has no Ed25519 key with a kid".to_owned());
        }
        *self.inner.keys.write().expect("keys lock poisoned") = keys;
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
        self.read_keys().get(kid).cloned()
    }

    /// A token names a key the gateway does not have: auth-svc may have rotated its key.
    /// Fetches the set again, at most every [`REFETCH_INTERVAL`], so made-up key ids
    /// cannot make the gateway hammer auth-svc. Only a key still missing from a set
    /// fetched for this very token is `Invalid` (a sign-out); a key that could not be
    /// looked up, because the fetch failed or came too soon after the last one, is
    /// `KeysUnavailable` (try again), so neither an auth-svc outage nor a rotation signs
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
        self.key(kid).ok_or(TokenError::Invalid)
    }

    fn read_keys(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, DecodingKey>> {
        self.inner.keys.read().expect("keys lock poisoned")
    }
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
        // A key missing from a set fetched for the token itself is invalid.
        Arc::get_mut(&mut verifier.inner).unwrap().refetch_interval = Duration::ZERO;
        assert_eq!(verifier.verify(&stranger).await, Err(TokenError::Invalid));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
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

```powershell
cargo test -p push-gateway --lib tokens
```

Expected (this run records the new dependencies in `Cargo.lock`): 7 tests pass —
`a_token_from_auth_svc_passes_with_its_claims`,
`an_expired_token_is_expired_only_after_the_leeway`,
`tokens_for_another_audience_or_from_another_key_are_invalid`,
`exp_kid_eddsa_and_a_verified_email_are_required`,
`a_rotated_key_is_fetched_when_a_token_names_it`,
`without_reachable_keys_tokens_cannot_be_checked_and_nobody_is_signed_out` and
`keys_of_other_kinds_or_without_a_kid_are_skipped`. To see them fail first, create the file
with `verify` returning `Err(TokenError::Invalid)`: all but the last two fail.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock crates
git commit -m "feat(gateway): check access tokens against auth-svc's key set"
```

Expected: 836 tests pass, no warnings, no diff.

---

### Task 5: Identify with access tokens; the DEV ONLY identity goes

**Files:**
- Modify: `crates/push-gateway/tests/gateway.rs`, `src/session.rs`, `src/config.rs`,
  `src/server.rs`, `crates/push-proto/proto/push/v1/frame.proto`,
  `crates/push-smoke/src/lib.rs`, `tests/local.rs`, `README.md`,
  `deploy/helm/push-messenger/templates/gateway.yaml`, `templates/_helpers.tpl`,
  `templates/NOTES.txt`, `values.yaml`, `values-minikube.yaml`, `deploy/helm/test-chart.ps1`

- [ ] **Step 1: The gateway's tests on real tokens, failing**

The test stack serves auth-svc's JWKS and issues tokens with its `AccessTokens`; the
gateway is configured with `auth_url` and `token_audience`. New cases: invalid tokens close
with 4001 and an expired one with 4002; message-svc decides the rest (4006 not relayed yet,
4001 after a password reset, 4003 revoked); a connection that never identifies gets 1002.
In `crates/push-gateway/tests/gateway.rs`:

Replace

```rust
use futures_util::{SinkExt, StreamExt};
use push_domain::{DeviceId, InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock, UserId};
use push_gateway::{Config, serve};
```

with

```rust
use futures_util::{SinkExt, StreamExt};
use push_auth::tokens::AccessTokens;
use push_domain::{DeviceId, InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock, UserId};
use push_gateway::{Config, serve};
```

Replace

```rust
    messaging: Arc<Messaging<InMemoryStore, SystemClock, RedisNotifier>>,
    message_svc: String,
    gateway: SocketAddr,
    shutdown: CancellationToken,
```

with

```rust
    messaging: Arc<Messaging<InMemoryStore, SystemClock, RedisNotifier>>,
    message_svc: String,
    /// auth-svc's token issuer, whose key set the gateway fetches.
    issuer: AccessTokens,
    gateway: SocketAddr,
    shutdown: CancellationToken,
```

Replace

```rust
}

/// Redis, message-svc's API on the in-memory store, and the gateway, on free ports.
async fn stack(dev_auth: bool) -> Stack {
    stack_with(dev_auth, 0).await
}

/// As [`stack`], with message-svc failing its first `sync_failures` syncs.
async fn stack_with(dev_auth: bool, sync_failures: u32) -> Stack {
    let redis = Redis::default()
        .with_tag("7-alpine")
```

with

```rust
}

/// The URL the gateway names itself by: the audience of its tokens.
const AUDIENCE: &str = "ws://push.test/v1/socket";

/// Redis, message-svc's API on the in-memory store, auth-svc's key set, and the gateway,
/// on free ports.
async fn stack() -> Stack {
    stack_with(0).await
}

/// As [`stack`], with message-svc failing its first `sync_failures` syncs.
async fn stack_with(sync_failures: u32) -> Stack {
    let redis = Redis::default()
        .with_tag("7-alpine")
```

Replace

```rust
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
```

with

```rust
    );

    // auth-svc's key set, as `GET /v1/auth/jwks` serves it.
    let issuer = AccessTokens::from_seed(&[7; 32], AUDIENCE).unwrap();
    let jwks = issuer.jwks();
    let keys = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth_url = format!("http://{}", keys.local_addr().unwrap());
    let routes = axum::Router::new().route(
        "/v1/auth/jwks",
        axum::routing::get(move || async move { axum::Json(jwks) }),
    );
    tokio::spawn(axum::serve(keys, routes).into_future());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
```

Replace

```rust
        message_svc_url: message_svc.clone(),
        redis_url,
        listen_addr: gateway,
        health_addr: health.local_addr().unwrap(),
        heartbeat: Duration::from_secs(30),
        dev_auth,
    };
    let shutdown = CancellationToken::new();
```

with

```rust
        message_svc_url: message_svc.clone(),
        redis_url,
        auth_url,
        token_audience: AUDIENCE.to_owned(),
        listen_addr: gateway,
        health_addr: health.local_addr().unwrap(),
        heartbeat: Duration::from_secs(30),
    };
    let shutdown = CancellationToken::new();
```

Replace

```rust
        messaging,
        message_svc,
        gateway,
        shutdown,
```

with

```rust
        messaging,
        message_svc,
        issuer,
        gateway,
        shutdown,
```

Replace

```rust
}

/// A user with one device, relayed to message-svc as the worker relays them.
async fn person(stack: &Stack) -> (Uuid, Uuid) {
```

with

```rust
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A user with one device, relayed to message-svc as the worker relays them.
async fn person(stack: &Stack) -> (Uuid, Uuid) {
```

Replace

```rust
}

fn identify(user: Uuid, device: Uuid) -> Body {
    Body::Identify(Identify {
        token: String::new(),
        dev_user_id: id_bytes(user),
        dev_device_id: id_bytes(device),
    })
}
```

with

```rust
}

/// An `Identify` with an access token issued now.
fn identify(stack: &Stack, user: Uuid, device: Uuid) -> Body {
    identify_issued(stack, user, device, now())
}

/// An `Identify` with an access token issued at `issued_at` (seconds); it expires 15
/// minutes later.
fn identify_issued(stack: &Stack, user: Uuid, device: Uuid, issued_at: i64) -> Body {
    let (token, _) = stack.issuer.issue(user, device, issued_at);
    Body::Identify(Identify { token })
}

/// Connects and identifies with a token issued now; returns once `Ready` arrived.
async fn ready(stack: &Stack, user: Uuid, device: Uuid) -> Socket {
    let mut socket = connect(stack).await;
    send(&mut socket, identify(stack, user, device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
    socket
}
```

Replace

```rust
#[tokio::test]
async fn a_message_reaches_the_other_device_and_the_sender_gets_its_ack() {
    let stack = stack(true).await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
```

with

```rust
#[tokio::test]
async fn a_message_reaches_the_other_device_and_the_sender_gets_its_ack() {
    let stack = stack().await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
```

Replace

```rust
    let mut bob_socket = connect(&stack).await;
    send(&mut bob_socket, identify(bob, bob_device)).await;
    let Body::Ready(ready) = next(&mut bob_socket).await else {
        panic!("expected Ready");
    };
    assert_eq!(ready.user_id, id_bytes(bob));
    assert_eq!(ready.device_id, id_bytes(bob_device));
    assert_eq!(ready.heartbeat_ms, 30_000);

    let mut alice_socket = connect(&stack).await;
    send(&mut alice_socket, identify(alice, alice_device)).await;
    assert!(matches!(next(&mut alice_socket).await, Body::Ready(_)));
    let client_msg_id = id_bytes(Uuid::now_v7());
    send(
```

with

```rust
    let mut bob_socket = connect(&stack).await;
    send(&mut bob_socket, identify(&stack, bob, bob_device)).await;
    let Body::Ready(ready_frame) = next(&mut bob_socket).await else {
        panic!("expected Ready");
    };
    assert_eq!(ready_frame.user_id, id_bytes(bob));
    assert_eq!(ready_frame.device_id, id_bytes(bob_device));
    assert_eq!(ready_frame.heartbeat_ms, 30_000);

    let mut alice_socket = ready(&stack, alice, alice_device).await;
    let client_msg_id = id_bytes(Uuid::now_v7());
    send(
```

Replace

```rust
#[tokio::test]
async fn without_dev_auth_identify_is_closed_with_4001() {
    let stack = stack(false).await;
    let (user, device) = person(&stack).await;
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(user, device)).await;
    assert_eq!(close_code(&mut socket).await, 4001);
}

#[tokio::test]
async fn a_device_of_another_user_is_closed_with_4001() {
    let stack = stack(true).await;
    let (alice, _) = person(&stack).await;
    let (_, bob_device) = person(&stack).await;
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(alice, bob_device)).await;
    assert_eq!(close_code(&mut socket).await, 4001);

    // A device message-svc has not heard of yet (not relayed) is not refused for good:
    // message-svc answers UNAVAILABLE, and the client retries (Plan 8 gives it its own code).
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(alice, Uuid::now_v7())).await;
    assert_eq!(close_code(&mut socket).await, 1011);
}

#[tokio::test]
async fn a_connection_that_never_identifies_is_closed_after_5_seconds() {
    let stack = stack(true).await;
    let mut socket = connect(&stack).await;
    assert_eq!(close_code(&mut socket).await, 4001);
}

#[tokio::test]
async fn shutdown_closes_connections_with_4005() {
    let stack = stack(true).await;
    let (user, device) = person(&stack).await;
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(user, device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));

    stack.shutdown.cancel();
```

with

```rust
#[tokio::test]
async fn an_invalid_token_is_closed_with_4001_and_an_expired_one_with_4002() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    let elsewhere = AccessTokens::from_seed(&[7; 32], "wss://elsewhere.test/v1/socket").unwrap();
    let stranger = AccessTokens::from_seed(&[8; 32], AUDIENCE).unwrap();
    for token in [
        "not a token".to_owned(),
        String::new(),
        elsewhere.issue(user, device, now()).0,
        stranger.issue(user, device, now()).0,
    ] {
        let mut socket = connect(&stack).await;
        send(&mut socket, Body::Identify(Identify { token })).await;
        assert_eq!(close_code(&mut socket).await, 4001);
    }
    // Expired 90 s ago: past the 60 s leeway. The client refreshes, then reconnects.
    let mut socket = connect(&stack).await;
    send(
        &mut socket,
        identify_issued(&stack, user, device, now() - 900 - 90),
    )
    .await;
    assert_eq!(close_code(&mut socket).await, 4002);
}

#[tokio::test]
async fn message_svc_decides_whether_the_tokens_device_may_connect() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;

    // Registered moments ago and not relayed yet: retry shortly, never a sign-out.
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(&stack, user, Uuid::now_v7())).await;
    assert_eq!(close_code(&mut socket).await, 4006);

    // A token issued before the password reset: sign in again.
    let before = now() - 60;
    stack.messaging.password_reset(UserId(user)).await.unwrap();
    let mut socket = connect(&stack).await;
    send(&mut socket, identify_issued(&stack, user, device, before)).await;
    assert_eq!(close_code(&mut socket).await, 4001);
    let mut socket = connect(&stack).await;
    send(
        &mut socket,
        identify_issued(&stack, user, device, now() + 1),
    )
    .await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));

    // A revoked device: stop.
    stack
        .messaging
        .device_revoked(DeviceId(device), UserId(user))
        .await
        .unwrap();
    let mut socket = connect(&stack).await;
    send(
        &mut socket,
        identify_issued(&stack, user, device, now() + 1),
    )
    .await;
    assert_eq!(close_code(&mut socket).await, 4003);
}

#[tokio::test]
async fn a_connection_that_never_identifies_is_closed_after_5_seconds() {
    let stack = stack().await;
    let mut socket = connect(&stack).await;
    assert_eq!(close_code(&mut socket).await, 1002);
}

#[tokio::test]
async fn shutdown_closes_connections_with_4005() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    let mut socket = ready(&stack, user, device).await;

    stack.shutdown.cancel();
```

Replace

```rust
#[tokio::test]
async fn a_backlog_larger_than_one_sync_batch_is_delivered_as_the_device_acks() {
    let stack = stack(true).await;
    let backlog = MAX_SYNC_BATCH + 1;
    let (_alice, (bob, bob_device), conversation) = chat_with_backlog(&stack, backlog).await;

    let mut socket = connect(&stack).await;
    send(&mut socket, identify(bob, bob_device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
    for expected in 1..=i64::from(MAX_SYNC_BATCH) {
```

with

```rust
#[tokio::test]
async fn a_backlog_larger_than_one_sync_batch_is_delivered_as_the_device_acks() {
    let stack = stack().await;
    let backlog = MAX_SYNC_BATCH + 1;
    let (_alice, (bob, bob_device), conversation) = chat_with_backlog(&stack, backlog).await;

    let mut socket = connect(&stack).await;
    send(&mut socket, identify(&stack, bob, bob_device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
    for expected in 1..=i64::from(MAX_SYNC_BATCH) {
```

Replace

```rust
#[tokio::test]
async fn a_failed_sync_is_retried() {
    let stack = stack_with(true, 1).await;
    let (_alice, (bob, bob_device), _conversation) = chat_with_backlog(&stack, 1).await;

    let mut socket = connect(&stack).await;
    send(&mut socket, identify(bob, bob_device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
    // The sync after Ready fails; nothing rings Bob's channel, so only a retry delivers.
```

with

```rust
#[tokio::test]
async fn a_failed_sync_is_retried() {
    let stack = stack_with(1).await;
    let (_alice, (bob, bob_device), _conversation) = chat_with_backlog(&stack, 1).await;

    let mut socket = connect(&stack).await;
    send(&mut socket, identify(&stack, bob, bob_device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
    // The sync after Ready fails; nothing rings Bob's channel, so only a retry delivers.
```

```powershell
cargo test -p push-gateway --test gateway
```

Expected: compile errors — `Config` has no field `auth_url`, and `Identify` has fields the
test does not set.

- [ ] **Step 2: The protocol state**

`Identify` hands over its token and `Ready` names the token's user and device; before
`Ready` every mistake is 1002 (decision 4). Replace the whole of
`crates/push-gateway/src/session.rs`, with its tests, with:

```rust
//! The protocol state of one WebSocket connection, without any I/O: which frames are
//! allowed when, what each one asks the gateway to do, which entries the client has
//! already been sent, and when to sync again.

use std::collections::HashMap;
use std::time::Duration;

use push_proto::close;
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, Deliver, Error, ErrorCode, Pong, Ready, Send};
use push_proto::{FrameError, decode_frame, id_bytes};
use uuid::Uuid;

use crate::tokens::Claims;

/// What the gateway must do for a frame the client sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Send this frame to the client.
    Reply(Body),
    /// Check this access token, ask message-svc whether the device it names may connect,
    /// then call [`Session::identified`] or close.
    Identify(String),
    /// Forward a send to message-svc; answer with its `SendAck` or an error.
    Send(Send),
    /// Forward an ack to message-svc; answer only if it fails. If it moved the position,
    /// ask [`Session::sync_after_ack`] whether to sync again.
    Ack(Ack),
    /// Close the connection with this code and reason.
    Close(u16, &'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Connected; the first frame must be `Identify`.
    New,
    /// The token of `Identify` is being checked.
    Identifying,
    Ready {
        device: Uuid,
    },
}

#[derive(Debug)]
pub struct Session {
    state: State,
    /// The highest `seq` sent to the client per conversation, so a sync that repeats
    /// entries the client has not acked yet does not send them twice.
    sent: HashMap<Vec<u8>, i64>,
    /// The last sync left entries behind (`more`). They come only after an ack, which
    /// rings nobody, so the session must sync again itself.
    more_waiting: bool,
}

impl Session {
    /// A new connection.
    pub fn new() -> Self {
        Self {
            state: State::New,
            sent: HashMap::new(),
            more_waiting: false,
        }
    }

    /// The connected device, once identified.
    pub fn device(&self) -> Option<Uuid> {
        match self.state {
            State::Ready { device } => Some(device),
            State::New | State::Identifying => None,
        }
    }

    /// What to do with a binary message from the client.
    pub fn on_binary(&mut self, bytes: &[u8]) -> Step {
        let body = match (decode_frame(bytes), self.state) {
            (Ok(body), _) => body,
            (Err(_), State::New | State::Identifying) => {
                return Step::Close(close::PROTOCOL_ERROR, "the first frame must be Identify");
            }
            (Err(FrameError::TooLarge { .. }), State::Ready { .. }) => {
                return reply_error(ErrorCode::BadFrame, "the frame is too large");
            }
            (Err(_), State::Ready { .. }) => {
                return reply_error(ErrorCode::BadFrame, "the frame could not be decoded");
            }
        };
        match (body, self.state) {
            (Body::Identify(identify), State::New) => {
                self.state = State::Identifying;
                Step::Identify(identify.token)
            }
            (_, State::New | State::Identifying) => {
                Step::Close(close::PROTOCOL_ERROR, "the first frame must be Identify")
            }
            (Body::Identify(_), State::Ready { .. }) => {
                reply_error(ErrorCode::BadFrame, "already identified")
            }

            (Body::Send(send), State::Ready { .. }) => Step::Send(send),
            (Body::Ack(ack), State::Ready { .. }) => Step::Ack(ack),
            (Body::Ping(_), State::Ready { .. }) => Step::Reply(Body::Pong(Pong {})),
            (_, State::Ready { .. }) => reply_error(ErrorCode::BadFrame, "not a client frame"),
        }
    }

    /// What to do with a text message: the protocol is binary only.
    pub fn on_text(&mut self) -> Step {
        match self.state {
            State::Ready { .. } => reply_error(ErrorCode::BadFrame, "frames must be binary"),
            State::New | State::Identifying => {
                Step::Close(close::PROTOCOL_ERROR, "the first frame must be Identify")
            }
        }
    }

    /// The token of [`Step::Identify`] passed and message-svc accepted the device it
    /// names: the `Ready` frame to send. `None` if no identity is being checked.
    pub fn identified(&mut self, claims: &Claims, heartbeat: Duration) -> Option<Body> {
        if self.state != State::Identifying {
            return None;
        }
        self.state = State::Ready {
            device: claims.device,
        };
        Some(Body::Ready(Ready {
            user_id: id_bytes(claims.user),
            device_id: id_bytes(claims.device),
            heartbeat_ms: u32::try_from(heartbeat.as_millis()).unwrap_or(u32::MAX),
        }))
    }

    /// The entries of a sync the client has not been sent on this connection yet,
    /// remembering them as sent.
    pub fn fresh(&mut self, entries: Vec<Deliver>) -> Vec<Deliver> {
        entries
            .into_iter()
            .filter(|entry| {
                let sent = self.sent.entry(entry.conversation_id.clone()).or_insert(0);
                if entry.seq > *sent {
                    *sent = entry.seq;
                    true
                } else {
                    false
                }
            })
            .collect()
    }

    /// Records whether the last sync left entries behind.
    pub fn synced(&mut self, more: bool) {
        self.more_waiting = more;
    }

    /// Whether to sync again after an ack: only if the ack `moved` the device's position
    /// and the last sync left entries behind. Then the next sync returns them.
    pub fn sync_after_ack(&self, moved: bool) -> bool {
        moved && self.more_waiting
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// When to retry a failed sync: after 1 s, doubling up to 30 s, and from 1 s again once a
/// sync succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryDelay {
    next: Duration,
}

impl RetryDelay {
    pub const FIRST: Duration = Duration::from_secs(1);
    pub const MAX: Duration = Duration::from_secs(30);

    /// A sync failed: how long to wait before the next attempt.
    pub fn failed(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        delay
    }

    /// A sync succeeded.
    pub fn succeeded(&mut self) {
        self.next = Self::FIRST;
    }
}

impl Default for RetryDelay {
    fn default() -> Self {
        Self { next: Self::FIRST }
    }
}

/// An `Error` frame for the client.
pub(crate) fn error_frame(code: ErrorCode, message: &str) -> Body {
    Body::Error(Error {
        code: code.into(),
        message: message.to_owned(),
        retryable: false,
        client_msg_id: None,
        request_id: None,
        retry_after_s: None,
    })
}

fn reply_error(code: ErrorCode, message: &str) -> Step {
    Step::Reply(error_frame(code, message))
}

#[cfg(test)]
mod tests {
    use push_proto::v1::{Identify, Ping};
    use push_proto::{MAX_FRAME_BYTES, encode_frame};

    use super::*;

    fn identify(token: &str) -> Vec<u8> {
        encode_frame(Body::Identify(Identify {
            token: token.to_owned(),
        }))
    }

    fn claims() -> Claims {
        Claims {
            user: Uuid::now_v7(),
            device: Uuid::now_v7(),
            issued_at: 1_700_000_000,
            expires_at: 1_700_000_900,
        }
    }

    fn ready_session() -> (Session, Uuid) {
        let mut session = Session::new();
        assert_eq!(
            session.on_binary(&identify("token")),
            Step::Identify("token".to_owned())
        );
        let claims = claims();
        session
            .identified(&claims, Duration::from_secs(30))
            .unwrap();
        (session, claims.device)
    }

    fn error_code(step: Step) -> ErrorCode {
        match step {
            Step::Reply(Body::Error(error)) => error.code(),
            other => panic!("expected an error frame, got {other:?}"),
        }
    }

    fn entry(conversation: &[u8], seq: i64) -> Deliver {
        Deliver {
            conversation_id: conversation.to_vec(),
            seq,
            ..Deliver::default()
        }
    }

    #[test]
    fn identify_hands_over_its_token_and_ready_names_the_tokens_device() {
        let mut session = Session::new();
        assert_eq!(
            session.on_binary(&identify("a.b.c")),
            Step::Identify("a.b.c".to_owned())
        );
        assert_eq!(session.device(), None);
        let claims = claims();
        let ready = session.identified(&claims, Duration::from_secs(30));
        assert_eq!(
            ready,
            Some(Body::Ready(Ready {
                user_id: id_bytes(claims.user),
                device_id: id_bytes(claims.device),
                heartbeat_ms: 30_000,
            }))
        );
        assert_eq!(session.device(), Some(claims.device));
        assert_eq!(session.identified(&claims, Duration::from_secs(30)), None);
    }

    #[test]
    fn identified_without_a_pending_identity_does_nothing() {
        let mut session = Session::new();
        assert_eq!(session.identified(&claims(), Duration::from_secs(30)), None);
        assert_eq!(session.device(), None);
    }

    /// A protocol mistake before Ready is no reason to sign out: 1002, not 4001.
    #[test]
    fn anything_but_identify_first_is_a_protocol_error() {
        let ping = encode_frame(Body::Ping(Ping {}));
        for bytes in [ping.as_slice(), &[0xff, 0xff], &[]] {
            let mut session = Session::new();
            assert_eq!(
                session.on_binary(bytes),
                Step::Close(1002, "the first frame must be Identify")
            );
        }
        assert_eq!(
            Session::new().on_text(),
            Step::Close(1002, "the first frame must be Identify")
        );
    }

    #[test]
    fn while_identifying_any_other_frame_or_a_second_identify_is_a_protocol_error() {
        let ping = encode_frame(Body::Ping(Ping {}));
        let second = identify("another");
        for bytes in [ping.as_slice(), second.as_slice()] {
            let mut session = Session::new();
            assert_eq!(
                session.on_binary(&identify("token")),
                Step::Identify("token".to_owned())
            );
            assert_eq!(
                session.on_binary(bytes),
                Step::Close(1002, "the first frame must be Identify")
            );
            assert_eq!(session.device(), None);
        }
    }

    #[test]
    fn an_oversized_frame_closes_before_identify_and_errors_once_ready() {
        let oversized = vec![0u8; MAX_FRAME_BYTES + 1];
        assert_eq!(
            Session::new().on_binary(&oversized),
            Step::Close(1002, "the first frame must be Identify")
        );
        let (mut session, _) = ready_session();
        assert_eq!(
            session.on_binary(&oversized),
            Step::Reply(error_frame(ErrorCode::BadFrame, "the frame is too large"))
        );
        assert!(session.device().is_some());
    }
    #[test]
    fn once_ready_frames_become_steps() {
        let (mut session, _) = ready_session();
        assert_eq!(
            session.on_binary(&encode_frame(Body::Ping(Ping {}))),
            Step::Reply(Body::Pong(Pong {}))
        );
        let send = Send {
            client_msg_id: id_bytes(Uuid::now_v7()),
            ..Send::default()
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::Send(send.clone()))),
            Step::Send(send)
        );
        let ack = Ack {
            conversation_id: id_bytes(Uuid::now_v7()),
            seq: 3,
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::Ack(ack.clone()))),
            Step::Ack(ack)
        );
    }

    #[test]
    fn once_ready_bad_frames_get_errors_but_keep_the_connection() {
        let (mut session, _) = ready_session();
        let again = identify("again");
        assert_eq!(error_code(session.on_binary(&again)), ErrorCode::BadFrame);
        assert_eq!(
            error_code(session.on_binary(&[0xff, 0xff])),
            ErrorCode::BadFrame
        );
        let pong = encode_frame(Body::Pong(Pong {}));
        assert_eq!(error_code(session.on_binary(&pong)), ErrorCode::BadFrame);
        assert_eq!(error_code(session.on_text()), ErrorCode::BadFrame);
        assert!(session.device().is_some());
    }

    #[test]
    fn entries_already_sent_are_not_sent_again() {
        let (mut session, _) = ready_session();
        let (a, b) = (id_bytes(Uuid::now_v7()), id_bytes(Uuid::now_v7()));
        assert_eq!(
            session.fresh(vec![entry(&a, 1), entry(&a, 2), entry(&b, 1)]),
            vec![entry(&a, 1), entry(&a, 2), entry(&b, 1)]
        );
        assert_eq!(
            session.fresh(vec![entry(&a, 1), entry(&a, 2), entry(&a, 3), entry(&b, 1)]),
            vec![entry(&a, 3)]
        );
    }

    #[test]
    fn an_ack_syncs_again_only_if_it_moved_and_entries_were_left() {
        let (mut session, _) = ready_session();
        assert!(!session.sync_after_ack(true), "nothing was left behind");
        session.synced(true);
        assert!(session.sync_after_ack(true));
        assert!(!session.sync_after_ack(false), "the position did not move");
        session.synced(false);
        assert!(!session.sync_after_ack(true));
    }

    #[test]
    fn failed_syncs_back_off_from_1_s_to_30_s_and_reset_on_success() {
        let mut delay = RetryDelay::default();
        let delays: Vec<u64> = (0..7).map(|_| delay.failed().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
        delay.succeeded();
        assert_eq!(delay.failed(), Duration::from_secs(1));
    }
}
```

The dev fields go (decision 15). In `crates/push-proto/proto/push/v1/frame.proto`:

Replace

```protobuf
}

// Must be the first frame, within 5 seconds of connecting.
message Identify {
  // The access token (a JWT, Plan 5). Not checked yet.
  string token = 1;
  // DEV ONLY: accepted only by a gateway started with PUSH_DEV_AUTH=1, which trusts
  // these ids without any proof. Removed when real tokens arrive (Plan 8); when
  // removing them, reserve their field numbers (14, 15) and names.
  bytes dev_user_id = 14;
  bytes dev_device_id = 15;
  // private_reads: the device does not reveal reads to others.
  reserved 16;
  reserved "private_reads";
}
```

with

```protobuf
}

// Must be the first frame, within 5 seconds of connecting. The gateway learns the user
// and the device only from the token (spec §5.3).
message Identify {
  // The access token auth-svc issued for this device (a JWT, spec §5.1).
  string token = 1;
  // 14 and 15 held the DEV ONLY ids that let a local gateway skip the token (Plans 4 to
  // 7); 16 is private_reads: the device does not reveal reads to others.
  reserved 14, 15, 16;
  reserved "dev_user_id", "dev_device_id", "private_reads";
}
```

- [ ] **Step 3: The configuration**

`PUSH_AUTH_URL` and `PUSH_TOKEN_AUDIENCE` are required; `PUSH_DEV_AUTH=1` is refused. In
`crates/push-gateway/src/config.rs`:

Replace

```rust
/// | `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc | required |
/// | `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
/// | `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
/// | `PUSH_DEV_AUTH` | `1` accepts DEV ONLY identities without a token | off |
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub message_svc_url: String,
    pub redis_url: String,
    pub listen_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub heartbeat: Duration,
    pub dev_auth: bool,
}
```

with

```rust
/// | `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc | required |
/// | `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
/// | `PUSH_AUTH_URL` | base URL of auth-svc, whose `/v1/auth/jwks` holds the token keys | required |
/// | `PUSH_TOKEN_AUDIENCE` | the gateway's public URL (`wss://…/v1/socket`): the `aud` of the access tokens it accepts | required |
/// | `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
///
/// `PUSH_DEV_AUTH=1` (the DEV ONLY identities without a token, removed in Plan 8) is
/// refused, so a deployment that still expects it fails at start-up.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub message_svc_url: String,
    pub redis_url: String,
    pub auth_url: String,
    pub token_audience: String,
    pub listen_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub heartbeat: Duration,
}
```

Replace

```rust
            .field("message_svc_url", &self.message_svc_url)
            .field("redis_url", &"<redacted>")
            .field("listen_addr", &self.listen_addr)
            .field("health_addr", &self.health_addr)
            .field("heartbeat", &self.heartbeat)
            .field("dev_auth", &self.dev_auth)
            .finish()
    }
```

with

```rust
            .field("message_svc_url", &self.message_svc_url)
            .field("redis_url", &"<redacted>")
            .field("auth_url", &self.auth_url)
            .field("token_audience", &self.token_audience)
            .field("listen_addr", &self.listen_addr)
            .field("health_addr", &self.health_addr)
            .field("heartbeat", &self.heartbeat)
            .finish()
    }
```

Replace

```rust
            });
        }
        Ok(Self {
            message_svc_url: required("PUSH_MESSAGE_SVC_URL")?,
            redis_url: required("PUSH_REDIS_URL")?,
            listen_addr: parsed(&lookup, "PUSH_LISTEN_ADDR", ([0, 0, 0, 0], 8080).into())?,
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            heartbeat: Duration::from_millis(heartbeat_ms),
            dev_auth: dev_flag(&lookup, "PUSH_DEV_AUTH")?,
        })
    }
}
```

with

```rust
            });
        }
        match lookup("PUSH_DEV_AUTH").as_deref() {
            None | Some("") | Some("0") => {}
            Some(other) => {
                return Err(ConfigError::Invalid {
                    name: "PUSH_DEV_AUTH",
                    value: other.to_owned(),
                    reason: "DEV ONLY identities were removed: clients identify with access \
                             tokens"
                        .to_owned(),
                });
            }
        }
        let auth_url = required("PUSH_AUTH_URL")?;
        if !(auth_url.starts_with("http://") || auth_url.starts_with("https://")) {
            return Err(ConfigError::Invalid {
                name: "PUSH_AUTH_URL",
                value: auth_url,
                reason: "use an http:// or https:// URL".to_owned(),
            });
        }
        Ok(Self {
            message_svc_url: required("PUSH_MESSAGE_SVC_URL")?,
            redis_url: required("PUSH_REDIS_URL")?,
            auth_url,
            token_audience: required("PUSH_TOKEN_AUDIENCE")?,
            listen_addr: parsed(&lookup, "PUSH_LISTEN_ADDR", ([0, 0, 0, 0], 8080).into())?,
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            heartbeat: Duration::from_millis(heartbeat_ms),
        })
    }

    /// auth-svc's JSON Web Key Set: the keys that sign access tokens.
    pub fn jwks_url(&self) -> String {
        format!("{}/v1/auth/jwks", self.auth_url.trim_end_matches('/'))
    }
}
```

Replace

```rust
}

/// A DEV ONLY switch: on only for exactly `1`; off when unset, empty or `0`. Anything
/// else (`true`, `yes`) is refused, so a typo can never switch it on or leave it in doubt.
fn dev_flag(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<bool, ConfigError> {
    match lookup(name).as_deref() {
        None | Some("") | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(ConfigError::Invalid {
            name,
            value: other.to_owned(),
            reason: "use 1 to enable or 0 to disable".to_owned(),
        }),
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
    }

    const REQUIRED: [(&str, &str); 2] = [
        ("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051"),
        ("PUSH_REDIS_URL", "redis://:secret@redis:6379"),
    ];
```

with

```rust
    }

    const REQUIRED: [(&str, &str); 4] = [
        ("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051"),
        ("PUSH_REDIS_URL", "redis://:secret@redis:6379"),
        ("PUSH_AUTH_URL", "http://auth:8080/"),
        ("PUSH_TOKEN_AUDIENCE", "wss://push.example.com/v1/socket"),
    ];
```

Replace

```rust
        assert_eq!(config.message_svc_url, "http://message-svc:50051");
        assert_eq!(config.redis_url, "redis://:secret@redis:6379");
        assert_eq!(config.listen_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.heartbeat, Duration::from_secs(30));
        assert!(!config.dev_auth);
    }
```

with

```rust
        assert_eq!(config.message_svc_url, "http://message-svc:50051");
        assert_eq!(config.redis_url, "redis://:secret@redis:6379");
        assert_eq!(config.jwks_url(), "http://auth:8080/v1/auth/jwks");
        assert_eq!(config.token_audience, "wss://push.example.com/v1/socket");
        assert_eq!(config.listen_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.heartbeat, Duration::from_secs(30));
    }
```

Replace

```rust
    #[test]
    fn the_urls_are_required() {
        assert_eq!(
            config(&REQUIRED[1..]),
            Err(ConfigError::Missing("PUSH_MESSAGE_SVC_URL"))
        );
        assert_eq!(
            config(&REQUIRED[..1]),
            Err(ConfigError::Missing("PUSH_REDIS_URL"))
        );
    }
```

with

```rust
    #[test]
    fn the_urls_and_the_audience_are_required() {
        for (index, (name, _)) in REQUIRED.iter().enumerate() {
            let mut variables = REQUIRED.to_vec();
            variables.remove(index);
            assert_eq!(
                config(&variables),
                Err(ConfigError::Missing(name)),
                "{name}"
            );
        }
        let mut variables = REQUIRED.to_vec();
        variables[2] = ("PUSH_AUTH_URL", "auth:8080");
        assert!(matches!(
            config(&variables),
            Err(ConfigError::Invalid {
                name: "PUSH_AUTH_URL",
                ..
            })
        ));
    }
```

Replace

```rust
    #[test]
    fn dev_auth_is_on_only_for_exactly_1() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_AUTH", value));
            assert_eq!(config(&variables).unwrap().dev_auth, expected, "{value:?}");
        }
        for value in ["true", "yes", "on", " 1"] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_AUTH", value));
```

with

```rust
    #[test]
    fn dev_identities_are_gone_and_asking_for_them_is_refused() {
        for value in ["0", ""] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_AUTH", value));
            assert!(config(&variables).is_ok(), "{value:?}");
        }
        for value in ["1", "true", "yes"] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_AUTH", value));
```

- [ ] **Step 4: Identify and the keys**

The keys are fetched until loaded, then every 5 minutes, and `/readyz` waits for them;
`Identify` checks the token, then asks message-svc (`admit`) with the token's `iat` and
turns its answer into a close code (decisions 3, 4, 5). In
`crates/push-gateway/src/server.rs`:

Replace

```rust
//! Running the gateway: the WebSocket endpoint, one task per connection, the health
//! endpoints and graceful shutdown.

use std::sync::Arc;
```

with

```rust
//! Running the gateway: the WebSocket endpoint, one task per connection, the token keys,
//! the health endpoints and graceful shutdown.

use std::sync::Arc;
```

Replace

```rust
use crate::hub::Hub;
use crate::session::{RetryDelay, Session, Step, error_frame};

/// The first frame must be `Identify`, within this time (spec §7.1).
```

with

```rust
use crate::hub::Hub;
use crate::session::{RetryDelay, Session, Step, error_frame};
use crate::tokens::{Claims, TokenError, Verifier};

/// The first frame must be `Identify`, within this time (spec §7.1).
```

Replace

```rust
/// How long shutdown waits for connections to close.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Close code for "the server cannot serve this connection now" (RFC 6455).
const INTERNAL_ERROR: u16 = 1011;

#[derive(Clone)]
```

with

```rust
/// How long shutdown waits for connections to close.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the token keys are fetched again: auth-svc lets clients cache them 5 minutes.
const KEYS_REFRESH: Duration = Duration::from_secs(300);
/// How soon to fetch the keys again after a failed fetch.
const KEYS_RETRY: Duration = Duration::from_secs(2);

#[derive(Clone)]
```

Replace

```rust
    config: Arc<Config>,
    messaging: MessagingClient<Channel>,
    hub: Hub,
    sessions: TaskTracker,
```

with

```rust
    config: Arc<Config>,
    messaging: MessagingClient<Channel>,
    verifier: Verifier,
    hub: Hub,
    sessions: TaskTracker,
```

Replace

```rust
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    if config.dev_auth {
        tracing::warn!(
            "DEV ONLY: PUSH_DEV_AUTH=1 accepts any user and device id in Identify without a \
             token. Anyone who can reach this gateway can act as any user. Never enable it \
             outside a local cluster."
        );
    }
    let endpoint = Endpoint::from_shared(config.message_svc_url.clone())?
        .connect_timeout(Duration::from_secs(2))
```

with

```rust
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let endpoint = Endpoint::from_shared(config.message_svc_url.clone())?
        .connect_timeout(Duration::from_secs(2))
```

Replace

```rust
        .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
    let (hub, subscriber) = Hub::start(redis, shutdown.clone());
    let sessions = TaskTracker::new();
    let listen_addr = config.listen_addr;
```

with

```rust
        .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
    let (hub, subscriber) = Hub::start(redis, shutdown.clone());
    let verifier = Verifier::new(config.jwks_url(), &config.token_audience);
    let keys = tokio::spawn(refresh_keys(verifier.clone(), shutdown.clone()));
    let sessions = TaskTracker::new();
    let listen_addr = config.listen_addr;
```

Replace

```rust
        config: Arc::new(config),
        messaging,
        hub: hub.clone(),
        sessions: sessions.clone(),
```

with

```rust
        config: Arc::new(config),
        messaging,
        verifier: verifier.clone(),
        hub: hub.clone(),
        sessions: sessions.clone(),
```

Replace

```rust
    let health_server = tokio::spawn(
        axum::serve(health, health_router(shutdown.clone()))
            .with_graceful_shutdown(shutdown.clone().cancelled_owned())
            .into_future(),
```

with

```rust
    let health_server = tokio::spawn(
        axum::serve(health, health_router(verifier, shutdown.clone()))
            .with_graceful_shutdown(shutdown.clone().cancelled_owned())
            .into_future(),
```

Replace

```rust
    }
    subscriber.await?;
    health_server.await??;
    tracing::info!("stopped");
```

with

```rust
    }
    subscriber.await?;
    keys.await?;
    health_server.await??;
    tracing::info!("stopped");
```

Replace

```rust
}

/// `/healthz` always answers; `/readyz` answers until shutdown begins. Readiness does not
/// depend on Redis: without it devices still get their entries, only later (after
/// `Ready`, after acks and on retries), while an unready gateway would refuse every
/// client. The subscriber reconnects by itself, so liveness does not depend on it either.
fn health_router(shutdown: CancellationToken) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
```

with

```rust
}

/// Keeps the token keys fresh: fetches them every 2 s until it has them, then every
/// 5 minutes (a token naming a new key fetches them at once, see [`Verifier`]). A failed
/// fetch keeps the keys it had.
async fn refresh_keys(verifier: Verifier, shutdown: CancellationToken) {
    loop {
        let fetched = tokio::select! {
            () = shutdown.cancelled() => return,
            fetched = verifier.fetch() => fetched,
        };
        let wait = match fetched {
            Ok(()) => KEYS_REFRESH,
            Err(error) => {
                tracing::warn!(%error, "could not fetch the token keys; retrying");
                if verifier.is_ready() {
                    KEYS_REFRESH / 10
                } else {
                    KEYS_RETRY
                }
            }
        };
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// `/healthz` always answers; `/readyz` answers once the token keys are loaded, until
/// shutdown begins. Readiness does not depend on Redis: without it devices still get
/// their entries, only later (after `Ready`, after acks and on retries), while an unready
/// gateway would refuse every client. The subscriber reconnects by itself, so liveness
/// does not depend on it either.
fn health_router(verifier: Verifier, shutdown: CancellationToken) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
```

Replace

```rust
                if shutdown.is_cancelled() {
                    (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
                } else {
                    (StatusCode::OK, "ready")
```

with

```rust
                if shutdown.is_cancelled() {
                    (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
                } else if !verifier.is_ready() {
                    (StatusCode::SERVICE_UNAVAILABLE, "loading the token keys")
                } else {
                    (StatusCode::OK, "ready")
```

Replace

```rust
/// One client connection, from `Identify` to close.
async fn connection(mut socket: WebSocket, app: App) {
    let mut session = Session::new(app.config.dev_auth);
    let Some((user, device)) = identify(&mut socket, &mut session, &app).await else {
        return;
    };
    let mut membership = app.hub.join(user).await;
    let Some(ready) = session.identified(app.config.heartbeat) else {
        return;
    };
```

with

```rust
/// One client connection, from `Identify` to close.
async fn connection(mut socket: WebSocket, app: App) {
    let mut session = Session::new();
    let Some(claims) = identify(&mut socket, &mut session, &app).await else {
        return;
    };
    let mut membership = app.hub.join(claims.user).await;
    let Some(ready) = session.identified(&claims, app.config.heartbeat) else {
        return;
    };
```

Replace

```rust
        session,
        messaging: app.messaging.clone(),
        user,
        device,
        retry: RetryDelay::default(),
        retry_at: None,
```

with

```rust
        session,
        messaging: app.messaging.clone(),
        user: claims.user,
        device: claims.device,
        retry: RetryDelay::default(),
        retry_at: None,
```

Replace

```rust
                }
            }
            message = connected.socket.recv() => {
                let step = match message {
```

with

```rust
                }
            }

            message = connected.socket.recv() => {
                let step = match message {
```

Replace

```rust
}

/// Waits for the first frame and checks the identity with message-svc. `None` once the
/// connection is closed.
async fn identify(
    socket: &mut WebSocket,
    session: &mut Session,
    app: &App,
) -> Option<(Uuid, Uuid)> {
    let deadline = tokio::time::sleep(IDENTIFY_TIMEOUT);
    tokio::pin!(deadline);
```

with

```rust
}

/// Waits for the first frame, checks its token and asks message-svc whether the device it
/// names may connect. `None` once the connection is closed.
async fn identify(socket: &mut WebSocket, session: &mut Session, app: &App) -> Option<Claims> {
    let deadline = tokio::time::sleep(IDENTIFY_TIMEOUT);
    tokio::pin!(deadline);
```

Replace

```rust
        tokio::select! {
            () = &mut deadline => {
                close_with(socket, close::INVALID_IDENTITY, "Identify must come within 5 s").await;
                return None;
            }
```

with

```rust
        tokio::select! {
            () = &mut deadline => {
                close_with(socket, close::PROTOCOL_ERROR, "Identify must come within 5 s").await;
                return None;
            }
```

Replace

```rust
        }
    };
    let (user, device) = match step {
        Step::Identify { user, device } => (user, device),
        Step::Close(code, reason) => {
            close_with(socket, code, reason).await;
```

with

```rust
        }
    };
    let token = match step {
        Step::Identify(token) => token,
        Step::Close(code, reason) => {
            close_with(socket, code, reason).await;
```

Replace

```rust
        }
        // Before Ready the session only asks to identify or to close.
        Step::Reply(_) | Step::Send(_) | Step::Ack(_) => {
            close_with(
                socket,
                close::INVALID_IDENTITY,
                "the first frame must be Identify",
            )
```

with

```rust
        }
        // Before Ready the session only asks to identify or to close.
        _ => {
            close_with(
                socket,
                close::PROTOCOL_ERROR,
                "the first frame must be Identify",
            )
```

Replace

```rust
        }
    };
    // DEV ONLY identities carry no token; they count as issued now.
    let issued_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let request = IdentifyRequest {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
        issued_at,
    };
    match app.messaging.clone().identify(request).await {
        Ok(_) => {
            tracing::info!(%user, %device, dev_identity = true, "device connected");
            Some((user, device))
        }
        Err(status)
            if matches!(
                status.code(),
                tonic::Code::PermissionDenied | tonic::Code::InvalidArgument
            ) =>
        {
            close_with(socket, close::INVALID_IDENTITY, "unknown or revoked device").await;
            None
        }
        Err(status) => {
            tracing::warn!(code = ?status.code(), message = status.message(), "identify failed");
            close_with(socket, INTERNAL_ERROR, "the message service is unavailable").await;
            None
        }
    }
```

with

```rust
        }
    };
    let claims = match app.verifier.verify(&token).await {
        Ok(claims) => claims,
        Err(error) => {
            let (code, reason) = token_refusal(error);
            close_with(socket, code, reason).await;
            return None;
        }
    };
    match admit(&mut app.messaging.clone(), &claims).await {
        Ok(()) => {
            tracing::info!(user = %claims.user, device = %claims.device, "device connected");
            Some(claims)
        }
        Err(refusal) => {
            let (code, reason) = refusal.close();
            close_with(socket, code, reason).await;
            None
        }
    }
}

/// The close code and reason for a token that did not pass.
fn token_refusal(error: TokenError) -> (u16, &'static str) {
    match error {
        TokenError::Invalid => (close::INVALID_IDENTITY, "the access token is not valid"),
        TokenError::Expired => (close::REAUTH_REQUIRED, "the access token has expired"),
        TokenError::KeysUnavailable => (
            close::INTERNAL_ERROR,
            "access tokens cannot be checked now; retry",
        ),
    }
}

/// Why message-svc did not admit a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// Close with this code and reason.
    Close(u16, &'static str),
    /// message-svc could not answer: retry later.
    Unavailable,
}

impl Refusal {
    fn close(self) -> (u16, &'static str) {
        match self {
            Self::Close(code, reason) => (code, reason),
            Self::Unavailable => (close::INTERNAL_ERROR, "the message service is unavailable"),
        }
    }
}

/// Asks message-svc whether the device of a valid token may connect: it is active and the
/// user's, and the token is newer than the user's last password reset.
async fn admit(messaging: &mut MessagingClient<Channel>, claims: &Claims) -> Result<(), Refusal> {
    let request = IdentifyRequest {
        user_id: id_bytes(claims.user),
        device_id: id_bytes(claims.device),
        issued_at: claims.issued_at,
    };
    let Err(status) = messaging.identify(request).await else {
        return Ok(());
    };
    match error_from_status(&status).code() {
        ErrorCode::PasswordChanged => Err(Refusal::Close(
            close::INVALID_IDENTITY,
            "the password was changed; sign in again",
        )),
        ErrorCode::UnknownDevice => {
            Err(Refusal::Close(close::TAKEN_OVER, "the device was revoked"))
        }
        ErrorCode::DeviceNotReady => {
            // Normal for a second after a device registers: the worker has not relayed it.
            tracing::debug!(device = %claims.device, "device not relayed yet");
            Err(Refusal::Close(
                close::NOT_READY_YET,
                "the device is not ready yet; retry shortly",
            ))
        }
        _ => {
            tracing::warn!(code = ?status.code(), message = status.message(), "identify failed");
            Err(Refusal::Unavailable)
        }
    }
```

Replace

```rust
                Err(Closed)
            }
            Step::Identify { .. } => {
                let error = error_frame(ErrorCode::BadFrame, "already identified");
                send(&mut self.socket, error).await
```

with

```rust
                Err(Closed)
            }
            Step::Identify(_) => {
                let error = error_frame(ErrorCode::BadFrame, "already identified");
                send(&mut self.socket, error).await
```

The smoke test identifies with its access token. In `crates/push-smoke/src/lib.rs`:

Replace

```rust
//!    conversation with Bob; auth-svc shows Alice the profiles of Bob and Carol, and Bob
//!    none of Carol's;
//! 4. connects Bob's WebSocket and then Alice's, each with a DEV ONLY `Identify` naming
//!    the device auth-svc registered, and waits for `Ready`;
//! 5. sends a message from Alice and waits for her `SendAck` and Bob's `Deliver`; acks it
//!    from Bob, then pings: the `Pong` proves the ack went through, since the gateway
```

with

```rust
//!    conversation with Bob; auth-svc shows Alice the profiles of Bob and Carol, and Bob
//!    none of Carol's;
//! 4. connects Bob's WebSocket and then Alice's, each identifying with the access token
//!    auth-svc issued for its device, and waits for `Ready`;
//! 5. sends a message from Alice and waits for her `SendAck` and Bob's `Deliver`; acks it
//!    from Bob, then pings: the `Pong` proves the ack went through, since the gateway
```

Replace

```rust
//!    their Redis channels, where the smoke test listens, and refuses Carol's device.
//!
//! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`, and
//! the gateway with `PUSH_DEV_AUTH=1`.

mod auth;
```

with

```rust
//!    their Redis channels, where the smoke test listens, and refuses Carol's device.
//!
//! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`.

mod auth;
```

Replace

```rust
            &mut socket,
            Body::Identify(Identify {
                token: String::new(),
                dev_user_id: id_bytes(self.user),
                dev_device_id: id_bytes(self.device),
            }),
        )
```

with

```rust
            &mut socket,
            Body::Identify(Identify {
                token: self.access_token.clone(),
            }),
        )
```

In `crates/push-smoke/tests/local.rs`:

Replace

```rust
            message_svc_url: message_svc_url.clone(),
            redis_url: redis_url.clone(),
            listen_addr: listener.local_addr().unwrap(),
            health_addr: gateway_health.local_addr().unwrap(),
            heartbeat: Duration::from_secs(30),
            dev_auth: true,
        },
        listener,
```

with

```rust
            message_svc_url: message_svc_url.clone(),
            redis_url: redis_url.clone(),
            auth_url: auth_url.clone(),
            token_audience: socket_url.clone(),
            listen_addr: listener.local_addr().unwrap(),
            health_addr: gateway_health.local_addr().unwrap(),
            heartbeat: Duration::from_secs(30),
        },
        listener,
```

```powershell
cargo test -p push-gateway -p push-smoke
```

Expected: everything passes, the 10 gateway cases and the local smoke run included.

- [ ] **Step 5: The chart and the README**

The gateway gets auth-svc's URL and the token audience; dev mode no longer touches the
gateway. In `deploy/helm/push-messenger/templates/gateway.yaml`:

Replace

```yaml
            - name: PUSH_HEARTBEAT_MS
              value: {{ .Values.gateway.heartbeatMs | quote }}
            - name: PUSH_DEV_AUTH
              value: {{ include "push.devFlag" . | quote }}
          ports:
            - name: http
```

with

```yaml
            - name: PUSH_HEARTBEAT_MS
              value: {{ .Values.gateway.heartbeatMs | quote }}
            # Access tokens are checked against auth-svc's key set (GET /v1/auth/jwks)
            # and must name the gateway's public URL as their audience.
            - name: PUSH_AUTH_URL
              value: {{ printf "http://%s:8080" (include "push.name" (dict "root" . "component" "auth")) | quote }}
            - name: PUSH_TOKEN_AUDIENCE
              value: {{ include "push.socketUrl" . | quote }}
          ports:
            - name: http
```

In `deploy/helm/push-messenger/values.yaml`:

Replace

```yaml
buildId: ""

# DEV ONLY switches: the gateway accepts identities without a token (PUSH_DEV_AUTH=1), the
# worker shows the emails its log sender delivered at /dev/mailbox (PUSH_DEV_MAILBOX=1), a
# webhook secret is generated when none is set, and `helm test` runs the smoke test. Never
# enable outside a local cluster.
devMode:
  enabled: false
```

with

```yaml
buildId: ""

# DEV ONLY switches: the worker shows the emails its log sender delivered at /dev/mailbox
# (PUSH_DEV_MAILBOX=1), a webhook secret is generated when none is set, and `helm test`
# runs the smoke test. Never enable outside a local cluster.
devMode:
  enabled: false
```

In `deploy/helm/push-messenger/values-minikube.yaml`:

Replace

```yaml
# Local minikube only (deploy/minikube/up.ps1). Turns on the DEV ONLY identity, setup API
# and mailbox that the smoke test needs; the worker logs emails instead of sending them.

devMode:
```

with

```yaml
# Local minikube only (deploy/minikube/up.ps1). Turns on the DEV ONLY mailbox the smoke
# test reads its emails from; the worker logs emails instead of sending them.

devMode:
```

In `deploy/helm/push-messenger/templates/_helpers.tpl`:

Replace

```text
{{- if .Values.devMode.enabled -}}
{{- if ne .Values.devMode.acknowledge "insecure-local-only" -}}
{{- fail "devMode.enabled switches authentication off: set devMode.acknowledge to \"insecure-local-only\", and only in a local values file" -}}
{{- end -}}
{{- $host := .Values.ingress.host -}}
```

with

```text
{{- if .Values.devMode.enabled -}}
{{- if ne .Values.devMode.acknowledge "insecure-local-only" -}}
{{- fail "devMode.enabled serves every email the worker sends, links and all, without authentication: set devMode.acknowledge to \"insecure-local-only\", and only in a local values file" -}}
{{- end -}}
{{- $host := .Values.ingress.host -}}
```

Replace

```text
{{- end -}}

{{/* "1" or "0" for the PUSH_DEV_AUTH and PUSH_DEV_MAILBOX variables. */}}
{{- define "push.devFlag" -}}
{{- if include "push.devMode" . -}}1{{- else -}}0{{- end -}}
```

with

```text
{{- end -}}

{{/* "1" or "0" for the PUSH_DEV_MAILBOX variable. */}}
{{- define "push.devFlag" -}}
{{- if include "push.devMode" . -}}1{{- else -}}0{{- end -}}
```

In `deploy/helm/push-messenger/templates/NOTES.txt`:

Replace

```text
{{- if include "push.devMode" . }}

DEV MODE IS ON: the gateway accepts identities without a token and the worker shows the
emails its log sender delivered at /dev/mailbox (not routed by the ingress). This is for a
local cluster only.

Run the end-to-end smoke test:
```

with

```text
{{- if include "push.devMode" . }}

DEV MODE IS ON: the worker shows the emails its log sender delivered at /dev/mailbox (not
routed by the ingress). This is for a local cluster only.

Run the end-to-end smoke test:
```

In `deploy/helm/test-chart.ps1`:

Replace

```powershell
$default = helm template push $chart | Out-String
Check 'default: the gateway refuses dev identities' { $default -match 'name: PUSH_DEV_AUTH\s+value: "0"' }
Check 'default: the DevSetup switch is gone' { $default -notmatch 'PUSH_DEV_SETUP' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
```

with

```powershell
$default = helm template push $chart | Out-String
Check 'default: the gateway checks tokens with auth-svc''s keys, for its own URL, and has no dev identities' {
    $gateway = Doc $default 'Deployment' 'push-gateway'
    $gateway -match 'name: PUSH_AUTH_URL\s+value: "http://push-auth:8080"' -and
        $gateway -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "wss://push.example.com/v1/socket"' -and
        $default -notmatch 'PUSH_DEV_AUTH'
}
Check 'default: the DevSetup switch is gone' { $default -notmatch 'PUSH_DEV_SETUP' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
```

Replace

```powershell
$dev = helm template push $chart -f $minikube | Out-String
Check 'minikube: dev identities on' { $dev -match 'name: PUSH_DEV_AUTH\s+value: "1"' }
Check 'minikube: the smoke test calls message-svc and reads the Redis URL from the Secret' {
    $smoke = Doc $dev 'Pod' 'push-smoke'
```

with

```powershell
$dev = helm template push $chart -f $minikube | Out-String
Check 'minikube: no dev identities either; tokens name ws://push.local/v1/socket' {
    $dev -notmatch 'PUSH_DEV_AUTH' -and
        (Doc $dev 'Deployment' 'push-gateway') -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "ws://push.local/v1/socket"'
}
Check 'minikube: the smoke test calls message-svc and reads the Redis URL from the Secret' {
    $smoke = Doc $dev 'Pod' 'push-smoke'
```

In `README.md`, the gateway's table:

Replace

```markdown
| `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
| `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
| `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready`, 1000 to 600000 | `30000` |
| `PUSH_DEV_AUTH` | `1` accepts DEV ONLY identities without a token | off |

`push-auth`:
```

with

```markdown
| `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
| `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
| `PUSH_AUTH_URL` | base URL of auth-svc (`http://push-auth:8080`); the token keys come from its `/v1/auth/jwks` | required |
| `PUSH_TOKEN_AUDIENCE` | the gateway's public URL (`wss://…/v1/socket`), the `aud` access tokens must name | required |
| `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready`, 1000 to 600000 | `30000` |

`push-auth`:
```

```powershell
pwsh deploy/helm/test-chart.ps1
```

Expected: `all chart checks passed` (29 checks).

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add README.md crates deploy
git commit -m "feat(gateway): Identify with access tokens; the DEV ONLY identity is gone"
```

Expected: 834 tests pass, no warnings, no diff.

---

### Task 6: ReAuth, AuthExpiring and the 4002 deadline

**Files:**
- Modify: `crates/push-gateway/tests/gateway.rs`, `src/session.rs`, `src/server.rs`

- [ ] **Step 1: The case, failing**

Spec §12 test 7: a token expired 25 s ago (inside the leeway) is accepted; both connections
hear `AuthExpiring` at once; the one that sends a fresh token in `ReAuth` stays open, the
other is closed with 4002 at `exp + 30 s`; a token of another device cannot renew a
connection. In `crates/push-gateway/tests/gateway.rs`:

Replace

```rust
};
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, ContentType, Identify, Ping, Pong, Send, SendAck};
use push_proto::{decode_frame, encode_frame, id_bytes};
use testcontainers_modules::redis::Redis;
```

with

```rust
};
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, AuthExpiring, ContentType, Identify, Ping, Pong, ReAuth, Send, SendAck};
use push_proto::{decode_frame, encode_frame, id_bytes};
use testcontainers_modules::redis::Redis;
```

Replace

```rust
}

/// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
/// messages from Alice in it, sent through message-svc before anyone connects.
```

with

```rust
}

/// Spec §12 test 7: ReAuth keeps a connection whose token expires open; without it, 4002.
#[tokio::test]
async fn an_expiring_token_is_announced_and_renewed_by_reauth() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    // Expired 25 s ago, within the leeway: 5 s left to send ReAuth.
    let issued_at = now() - 900 - 25;
    let mut renewed = connect(&stack).await;
    let mut lapsed = connect(&stack).await;
    for socket in [&mut renewed, &mut lapsed] {
        send(socket, identify_issued(&stack, user, device, issued_at)).await;
        assert!(matches!(next(socket).await, Body::Ready(_)));
        assert_eq!(
            next(socket).await,
            Body::AuthExpiring(AuthExpiring { expires_in_s: 0 })
        );
    }
    let (token, _) = stack.issuer.issue(user, device, now());
    send(&mut renewed, Body::ReAuth(ReAuth { token })).await;
    assert_eq!(close_code(&mut lapsed).await, 4002);
    send(&mut renewed, Body::Ping(Ping {})).await;
    assert_eq!(next(&mut renewed).await, Body::Pong(Pong {}));

    // A token of another device cannot renew the connection.
    let (_, other_device) = person(&stack).await;
    let (token, _) = stack.issuer.issue(user, other_device, now());
    send(&mut renewed, Body::ReAuth(ReAuth { token })).await;
    assert_eq!(close_code(&mut renewed).await, 4001);
}
/// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
/// messages from Alice in it, sent through message-svc before anyone connects.
```

```powershell
cargo test -p push-gateway --test gateway -- expiring
```

Expected: `an_expiring_token_is_announced_and_renewed_by_reauth` fails ("a frame within
5 s"): no `AuthExpiring` comes yet.

- [ ] **Step 2: ReAuth as a step**

In `crates/push-gateway/src/session.rs`, with its test:

Replace

```rust
    /// then call [`Session::identified`] or close.
    Identify(String),
    /// Forward a send to message-svc; answer with its `SendAck` or an error.
    Send(Send),
```

with

```rust
    /// then call [`Session::identified`] or close.
    Identify(String),
    /// Check this access token, which must name the connection's user and device, and
    /// ask message-svc again: the connection continues on the new token.
    ReAuth(String),
    /// Forward a send to message-svc; answer with its `SendAck` or an error.
    Send(Send),
```

Replace

```rust
                reply_error(ErrorCode::BadFrame, "already identified")
            }

            (Body::Send(send), State::Ready { .. }) => Step::Send(send),
            (Body::Ack(ack), State::Ready { .. }) => Step::Ack(ack),
```

with

```rust
                reply_error(ErrorCode::BadFrame, "already identified")
            }
            (Body::ReAuth(reauth), State::Ready { .. }) => Step::ReAuth(reauth.token),
            (Body::Send(send), State::Ready { .. }) => Step::Send(send),
            (Body::Ack(ack), State::Ready { .. }) => Step::Ack(ack),
```

Replace

```rust
#[cfg(test)]
mod tests {
    use push_proto::v1::{Identify, Ping};
    use push_proto::{MAX_FRAME_BYTES, encode_frame};
```

with

```rust
#[cfg(test)]
mod tests {
    use push_proto::v1::{Identify, Ping, ReAuth};
    use push_proto::{MAX_FRAME_BYTES, encode_frame};
```

Replace

```rust
    }

    #[test]
    fn an_oversized_frame_closes_before_identify_and_errors_once_ready() {
```

with

```rust
    }

    #[test]
    fn reauth_hands_over_its_token_once_ready_only() {
        let reauth = encode_frame(Body::ReAuth(ReAuth {
            token: "fresh".to_owned(),
        }));
        assert_eq!(
            Session::new().on_binary(&reauth),
            Step::Close(1002, "the first frame must be Identify")
        );
        let (mut session, _) = ready_session();
        assert_eq!(session.on_binary(&reauth), Step::ReAuth("fresh".to_owned()));
    }

    #[test]
    fn an_oversized_frame_closes_before_identify_and_errors_once_ready() {
```

- [ ] **Step 3: The expiry and the renewal**

The connection's `Expiry` sends `AuthExpiring` 60 s before `exp` and closes with 4002 at
`exp + 30 s`; `reauth` checks the token, its user and device, and asks message-svc again
(decision 7). In `crates/push-gateway/src/server.rs`:

Replace

```rust
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
```

with

```rust
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
```

Replace

```rust
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{AckRequest, IdentifyRequest, SendRequest, SyncRequest};
use push_proto::v1::ErrorCode;
use push_proto::v1::frame::Body;
use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes};
use tokio::net::TcpListener;
```

with

```rust
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{AckRequest, IdentifyRequest, SendRequest, SyncRequest};
use push_proto::v1::frame::Body;
use push_proto::v1::{AuthExpiring, Error, ErrorCode};
use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes};
use tokio::net::TcpListener;
```

Replace

```rust
/// How long shutdown waits for connections to close.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the token keys are fetched again: auth-svc lets clients cache them 5 minutes.
const KEYS_REFRESH: Duration = Duration::from_secs(300);
```

with

```rust
/// How long shutdown waits for connections to close.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// `AuthExpiring` comes this long before the token expires (spec §7.3).
const EXPIRY_WARNING: Duration = Duration::from_secs(60);
/// A connection must send a valid `ReAuth` by this long after its token expired, or it is
/// closed with 4002 (spec §7.3).
const REAUTH_GRACE: Duration = Duration::from_secs(30);
/// How often the token keys are fetched again: auth-svc lets clients cache them 5 minutes.
const KEYS_REFRESH: Duration = Duration::from_secs(300);
```

Replace

```rust
        session,
        messaging: app.messaging.clone(),
        user: claims.user,
        device: claims.device,
        retry: RetryDelay::default(),
        retry_at: None,
```

with

```rust
        session,
        messaging: app.messaging.clone(),
        verifier: app.verifier.clone(),
        user: claims.user,
        device: claims.device,
        expiry: Expiry::new(claims.expires_at),
        retry: RetryDelay::default(),
        retry_at: None,
```

Replace

```rust
    loop {
        let retry_at = connected.retry_at;
        tokio::select! {
            () = app.shutdown.cancelled() => {
```

with

```rust
    loop {
        let retry_at = connected.retry_at;
        let (warn_at, warned, deadline) = (
            connected.expiry.warn_at,
            connected.expiry.warned,
            connected.expiry.deadline,
        );
        tokio::select! {
            () = app.shutdown.cancelled() => {
```

Replace

```rust
                }
            }

            message = connected.socket.recv() => {
                let step = match message {
```

with

```rust
                }
            }
            () = tokio::time::sleep_until(warn_at), if !warned => {
                if connected.warn_expiry().await.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                close_with(&mut connected.socket, close::REAUTH_REQUIRED, "the access token expired; send ReAuth in time").await;
                return;
            }
            message = connected.socket.recv() => {
                let step = match message {
```

Replace

```rust
}

/// An identified connection.
struct Connected {
```

with

```rust
}

/// When the connection's token expires, and what to do about it.
#[derive(Debug, Clone, Copy)]
struct Expiry {
    /// The token's `exp`, in seconds since the Unix epoch.
    expires_at: i64,
    /// When to send `AuthExpiring`.
    warn_at: Instant,
    warned: bool,
    /// When to close with 4002 unless a `ReAuth` came.
    deadline: Instant,
}

impl Expiry {
    fn new(expires_at: i64) -> Self {
        let left = expires_at - unix_now();
        let at = |offset: i64| {
            Instant::now() + Duration::from_secs(u64::try_from(left + offset).unwrap_or(0))
        };
        Self {
            expires_at,
            warn_at: at(-(EXPIRY_WARNING.as_secs() as i64)),
            warned: false,
            deadline: at(REAUTH_GRACE.as_secs() as i64),
        }
    }

    /// Seconds until the token expires; 0 once it has.
    fn expires_in_s(&self) -> u32 {
        u32::try_from((self.expires_at - unix_now()).max(0)).unwrap_or(u32::MAX)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

/// An identified connection.
struct Connected {
```

Replace

```rust
    session: Session,
    messaging: MessagingClient<Channel>,
    user: Uuid,
    device: Uuid,
    retry: RetryDelay,
    /// When to retry a failed sync; `None` while the last sync succeeded.
```

with

```rust
    session: Session,
    messaging: MessagingClient<Channel>,
    verifier: Verifier,
    user: Uuid,
    device: Uuid,
    expiry: Expiry,
    retry: RetryDelay,
    /// When to retry a failed sync; `None` while the last sync succeeded.
```

Replace

```rust
        match step {
            Step::Reply(body) => send(&mut self.socket, body).await,
            Step::Send(frame) => {
                let client_msg_id = frame.client_msg_id.clone();
```

with

```rust
        match step {
            Step::Reply(body) => send(&mut self.socket, body).await,
            Step::ReAuth(token) => self.reauth(&token).await,
            Step::Send(frame) => {
                let client_msg_id = frame.client_msg_id.clone();
```

Replace

```rust
    }

    /// Pushes every entry the device has not been sent yet. A failed sync is retried
    /// after [`RetryDelay`], from the connection's loop.
```

with

```rust
    }

    /// Tells the client its token expires soon, once per token.
    async fn warn_expiry(&mut self) -> Result<(), Closed> {
        self.expiry.warned = true;
        let warning = AuthExpiring {
            expires_in_s: self.expiry.expires_in_s(),
        };
        send(&mut self.socket, Body::AuthExpiring(warning)).await
    }

    /// A fresh token for the connection: it must pass, name the same user and device, and
    /// be newer than the user's last password reset. Nothing answers a good one. When the
    /// keys or message-svc cannot be reached, a retryable error asks the client to send it
    /// again; the 4002 deadline still applies.
    async fn reauth(&mut self, token: &str) -> Result<(), Closed> {
        let claims = match self.verifier.verify(token).await {
            Ok(claims) => claims,
            Err(TokenError::KeysUnavailable) => return self.retry_reauth().await,
            Err(error) => {
                let (code, reason) = token_refusal(error);
                close_with(&mut self.socket, code, reason).await;
                return Err(Closed);
            }
        };
        if claims.user != self.user || claims.device != self.device {
            close_with(
                &mut self.socket,
                close::INVALID_IDENTITY,
                "the token names another device",
            )
            .await;
            return Err(Closed);
        }
        match admit(&mut self.messaging, &claims).await {
            Ok(()) => {
                self.expiry = Expiry::new(claims.expires_at);
                Ok(())
            }
            Err(Refusal::Unavailable) => self.retry_reauth().await,
            Err(Refusal::Close(code, reason)) => {
                close_with(&mut self.socket, code, reason).await;
                Err(Closed)
            }
        }
    }

    async fn retry_reauth(&mut self) -> Result<(), Closed> {
        let error = Error {
            code: ErrorCode::Unavailable.into(),
            message: "the token could not be checked now; send ReAuth again".to_owned(),
            retryable: true,
            client_msg_id: None,
            request_id: None,
            retry_after_s: Some(1),
        };
        send(&mut self.socket, Body::Error(error)).await
    }

    /// Pushes every entry the device has not been sent yet. A failed sync is retried
    /// after [`RetryDelay`], from the connection's loop.
```

```powershell
cargo test -p push-gateway
```

Expected: everything passes, `reauth_hands_over_its_token_once_ready_only` and the expiring
case included (it takes about 5 s).

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "feat(gateway): ReAuth, AuthExpiring and the 4002 deadline"
```

Expected: 836 tests pass, no warnings, no diff.

---
### Task 7: Kicks, takeovers and server frames; the kick channel watched before Identify

**Files:**
- Modify: `crates/push-gateway/src/hub.rs`, `src/lib.rs`, `src/server.rs`,
  `tests/hub.rs`, `tests/gateway.rs`,
  `crates/push-proto/proto/push/internal/v1/messaging.proto`,
  `crates/push-message-svc/src/notify.rs`, `crates/push-smoke/src/kicks.rs`

- [ ] **Step 1: The gateway's cases, failing**

A revocation closes the socket with 4003 and a password reset with 4001; a newer
connection of the device takes over; contact events, receipts and read positions reach the
sockets. The expiring case of Task 6 now uses two devices: a second connection of one
device would take the first over. In `crates/push-gateway/tests/gateway.rs`:

Replace

```rust
use futures_util::{SinkExt, StreamExt};
use push_auth::tokens::AccessTokens;
use push_domain::{DeviceId, InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock, UserId};
use push_gateway::{Config, serve};
use push_message_svc::{MessagingApi, RedisNotifier};
```

with

```rust
use futures_util::{SinkExt, StreamExt};
use push_auth::tokens::AccessTokens;
use push_domain::{
    ConversationId, DeviceId, InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock, UserId,
};
use push_gateway::{Config, serve};
use push_message_svc::{MessagingApi, RedisNotifier};
```

Replace

```rust
};
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, AuthExpiring, ContentType, Identify, Ping, Pong, ReAuth, Send, SendAck};
use push_proto::{decode_frame, encode_frame, id_bytes};
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
```

with

```rust
};
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, AuthExpiring, ContactEvent, ContactState, ContentType, DirectReceipt, Identify, Ping,
    Pong, ReAuth, ReadPosition, Receipt, Send, SendAck, receipt,
};
use push_proto::{decode_frame, encode_frame, id, id_bytes};
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
```

Replace

```rust
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    // Expired 25 s ago, within the leeway: 5 s left to send ReAuth.
    let issued_at = now() - 900 - 25;
    let mut renewed = connect(&stack).await;
    let mut lapsed = connect(&stack).await;
    for socket in [&mut renewed, &mut lapsed] {
        send(socket, identify_issued(&stack, user, device, issued_at)).await;
        assert!(matches!(next(socket).await, Body::Ready(_)));
```

with

```rust
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    let (other, other_device) = person(&stack).await;
    // Expired 25 s ago, within the leeway: 5 s left to send ReAuth.
    let issued_at = now() - 900 - 25;
    let mut renewed = connect(&stack).await;
    let mut lapsed = connect(&stack).await;
    for (socket, user, device) in [
        (&mut renewed, user, device),
        (&mut lapsed, other, other_device),
    ] {
        send(socket, identify_issued(&stack, user, device, issued_at)).await;
        assert!(matches!(next(socket).await, Body::Ready(_)));
```

Replace

```rust
    // A token of another device cannot renew the connection.
    let (_, other_device) = person(&stack).await;
    let (token, _) = stack.issuer.issue(user, other_device, now());
    send(&mut renewed, Body::ReAuth(ReAuth { token })).await;
    assert_eq!(close_code(&mut renewed).await, 4001);
}
/// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
/// messages from Alice in it, sent through message-svc before anyone connects.
```

with

```rust
    // A token of another device cannot renew the connection.
    let (token, _) = stack.issuer.issue(user, Uuid::now_v7(), now());
    send(&mut renewed, Body::ReAuth(ReAuth { token })).await;
    assert_eq!(close_code(&mut renewed).await, 4001);
}

/// The first frame `wanted` accepts, skipping others, within 5 s each.
async fn wait_for(socket: &mut Socket, wanted: impl Fn(&Body) -> bool) -> Body {
    loop {
        let frame = next(socket).await;
        if wanted(&frame) {
            return frame;
        }
    }
}

/// Spec §12 test 6 and the owner's close codes: a revoked device is told to stop (4003),
/// a password reset to sign in again (4001).
#[tokio::test]
async fn a_revocation_and_a_password_reset_close_the_socket_at_once() {
    let stack = stack().await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    let mut alice_socket = ready(&stack, alice, alice_device).await;
    let mut bob_socket = ready(&stack, bob, bob_device).await;
    stack
        .messaging
        .device_revoked(DeviceId(alice_device), UserId(alice))
        .await
        .unwrap();
    assert_eq!(close_code(&mut alice_socket).await, 4003);
    stack.messaging.password_reset(UserId(bob)).await.unwrap();
    assert_eq!(close_code(&mut bob_socket).await, 4001);
}

/// Spec §7.3: the newest connection of a device wins; the older one is told to stop.
#[tokio::test]
async fn a_newer_connection_of_the_device_takes_over() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    let mut first = ready(&stack, user, device).await;
    let mut second = ready(&stack, user, device).await;
    assert_eq!(close_code(&mut first).await, 4003);
    send(&mut second, Body::Ping(Ping {})).await;
    assert_eq!(next(&mut second).await, Body::Pong(Pong {}));
}

#[tokio::test]
async fn contact_events_receipts_and_read_positions_reach_the_sockets() {
    let stack = stack().await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    let mut alice_socket = ready(&stack, alice, alice_device).await;
    let mut bob_socket = ready(&stack, bob, bob_device).await;

    stack
        .messaging
        .contact_request(UserId(alice), UserId(bob))
        .await
        .unwrap();
    let event = |user: Uuid, state: ContactState| {
        Body::ContactEvent(ContactEvent {
            user_id: id_bytes(user),
            state: state.into(),
        })
    };
    assert_eq!(
        next(&mut bob_socket).await,
        event(alice, ContactState::RequestedIn)
    );
    assert_eq!(
        next(&mut alice_socket).await,
        event(bob, ContactState::RequestedOut)
    );
    stack
        .messaging
        .contact_accept(UserId(bob), UserId(alice))
        .await
        .unwrap();
    assert_eq!(
        next(&mut bob_socket).await,
        event(alice, ContactState::Accepted)
    );
    let conversation = stack
        .messaging
        .open_direct(UserId(alice), UserId(bob))
        .await
        .unwrap();
    let conversation = id_bytes(conversation.0);

    send(
        &mut alice_socket,
        Body::Send(Send {
            client_msg_id: id_bytes(Uuid::now_v7()),
            conversation_id: conversation.clone(),
            content_type: ContentType::Text.into(),
            body: b"hello".to_vec(),
            client_ts: None,
        }),
    )
    .await;
    wait_for(&mut bob_socket, |frame| matches!(frame, Body::Deliver(_))).await;
    send(
        &mut bob_socket,
        Body::Ack(Ack {
            conversation_id: conversation.clone(),
            seq: 1,
        }),
    )
    .await;
    let receipt = |delivered_seq, read_seq| {
        Body::Receipt(Receipt {
            conversation_id: conversation.clone(),
            positions: Some(receipt::Positions::Direct(DirectReceipt {
                user_id: id_bytes(bob),
                delivered_seq,
                read_seq,
            })),
        })
    };
    let delivered = receipt(1, 0);
    let got = wait_for(&mut alice_socket, |frame| matches!(frame, Body::Receipt(_))).await;
    assert_eq!(got, delivered);

    stack
        .messaging
        .read_up_to(UserId(bob), ConversationId(id(&conversation).unwrap()), 1)
        .await
        .unwrap();
    let position = wait_for(&mut bob_socket, |frame| {
        matches!(frame, Body::ReadPosition(_))
    })
    .await;
    assert_eq!(
        position,
        Body::ReadPosition(ReadPosition {
            conversation_id: conversation.clone(),
            read_seq: 1,
        })
    );
    let got = wait_for(&mut alice_socket, |frame| matches!(frame, Body::Receipt(_))).await;
    assert_eq!(got, receipt(1, 1));
}

/// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
/// messages from Alice in it, sent through message-svc before anyone connects.
```

The hub's Redis cases watch the device too, and hear its kick. In
`crates/push-gateway/tests/hub.rs`:

Replace

```rust
use prost::Message;
use push_gateway::Hub;
use push_proto::internal::v1::{EntriesAvailable, Notification, notification::Kind};
use push_proto::notify_channel;
use redis::AsyncCommands;
use testcontainers_modules::redis::Redis;
```

with

```rust
use prost::Message;
use push_gateway::{Hub, Membership, Signal};
use push_proto::internal::v1::{
    EntriesAvailable, Kick, KickReason, Notification, notification::Kind,
};
use push_proto::{kick_channel, notify_channel};
use redis::AsyncCommands;
use testcontainers_modules::redis::Redis;
```

Replace

```rust
}

async fn subscribers(client: &redis::Client, channel: &str) -> i64 {
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
```

with

```rust
}

/// A connection of `user` on a new device, as the gateway makes one.
async fn join(hub: &Hub, user: Uuid) -> (Membership, Uuid) {
    let device = Uuid::now_v7();
    let mut membership = hub.watch_device(device, Uuid::now_v7()).await;
    membership.join_user(user).await;
    (membership, device)
}

/// The next signal, within `seconds`.
async fn next(membership: &mut Membership, seconds: u64) -> Signal {
    tokio::time::timeout(Duration::from_secs(seconds), membership.next())
        .await
        .expect("a signal in time")
}

async fn subscribers(client: &redis::Client, channel: &str) -> i64 {
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
```

Replace

```rust
    let user = Uuid::now_v7();
    let mut session = hub.join(user).await;
    let payload = Notification {
        kind: Some(Kind::EntriesAvailable(EntriesAvailable {})),
```

with

```rust
    let user = Uuid::now_v7();
    let (mut session, device) = join(&hub, user).await;
    let payload = Notification {
        kind: Some(Kind::EntriesAvailable(EntriesAvailable {})),
```

Replace

```rust
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), session.rung())
        .await
        .expect("the session is rung within 5 s");

    shutdown.cancel();
```

with

```rust
        .await
        .unwrap();
    assert_eq!(next(&mut session, 5).await, Signal::Ring);

    // The device's kick channel was subscribed before anything else.
    let kick = Kick {
        reason: KickReason::DeviceRevoked.into(),
        connection_id: Vec::new(),
    };
    let _: i64 = publisher
        .publish(kick_channel(device), kick.encode_to_vec())
        .await
        .unwrap();
    assert_eq!(
        next(&mut session, 5).await,
        Signal::Kick(KickReason::DeviceRevoked)
    );

    shutdown.cancel();
```

Replace

```rust
    let user = Uuid::now_v7();
    let channel = notify_channel(user);
    let mut session = hub.join(user).await;
    assert_eq!(subscribers(&client, &channel).await, 1);

    let mut admin = client.get_multiplexed_async_connection().await.unwrap();
```

with

```rust
    let user = Uuid::now_v7();
    let channel = notify_channel(user);
    let (mut session, device) = join(&hub, user).await;
    assert_eq!(subscribers(&client, &channel).await, 1);
    assert_eq!(subscribers(&client, &kick_channel(device)).await, 1);

    let mut admin = client.get_multiplexed_async_connection().await.unwrap();
```

Replace

```rust
    // The reconnect rings every session, since notifications may have been lost.
    tokio::time::timeout(Duration::from_secs(10), session.rung())
        .await
        .expect("the session is rung after the reconnect within 10 s");
    connected(&hub).await;
    assert_eq!(subscribers(&client, &channel).await, 1);

    let payload = Notification {
```

with

```rust
    // The reconnect rings every session, since notifications may have been lost.
    assert_eq!(next(&mut session, 10).await, Signal::Ring);
    connected(&hub).await;
    assert_eq!(subscribers(&client, &channel).await, 1);
    assert_eq!(subscribers(&client, &kick_channel(device)).await, 1);

    let payload = Notification {
```

Replace

```rust
    .encode_to_vec();
    let _: i64 = admin.publish(&channel, payload).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), session.rung())
        .await
        .expect("a fresh publish rings the session within 5 s");

    shutdown.cancel();
```

with

```rust
    .encode_to_vec();
    let _: i64 = admin.publish(&channel, payload).await.unwrap();
    assert_eq!(next(&mut session, 5).await, Signal::Ring);

    shutdown.cancel();
```

Replace

```rust
    let user = Uuid::now_v7();
    let channel = notify_channel(user);
    let session = hub.join(user).await;
    assert_eq!(subscribers(&client, &channel).await, 1);
    drop(session);
    for _ in 0..100 {
        if subscribers(&client, &channel).await == 0 {
            shutdown.cancel();
            return;
```

with

```rust
    let user = Uuid::now_v7();
    let channel = notify_channel(user);
    let (session, device) = join(&hub, user).await;
    assert_eq!(subscribers(&client, &channel).await, 1);
    drop(session);
    for _ in 0..100 {
        if subscribers(&client, &channel).await == 0
            && subscribers(&client, &kick_channel(device)).await == 0
        {
            shutdown.cancel();
            return;
```

Replace

```rust
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the channel is still subscribed 5 s after the last session left");
}
```

with

```rust
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the channels are still subscribed 5 s after the last session left");
}
```

```powershell
cargo test -p push-gateway --tests
```

Expected: compile errors — no `Signal`, no `Hub::watch_device`, no `Kick::connection_id`.

- [ ] **Step 2: Takeovers in the contract**

In `crates/push-proto/proto/push/internal/v1/messaging.proto`:

Replace

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
  // The user's password was reset: the client signs in again (close code 4001).
  KICK_REASON_PASSWORD_RESET = 2;
}
```

with

```protobuf
}

// Published on the Redis channel `kick:{device_id}` (spec §5.3, §5.4, §7.3): every gateway
// holding a connection of the device closes it at once. Never carries anything but the
// reason and, for a takeover, the new connection.
message Kick {
  KickReason reason = 1;
  // TAKEN_OVER: the id the gateway gave the connection that takes over, which stays open.
  bytes connection_id = 2;
}

enum KickReason {
  KICK_REASON_UNSPECIFIED = 0;
  // The device was revoked: the client stops (close code 4003). From message-svc.
  KICK_REASON_DEVICE_REVOKED = 1;
  // The user's password was reset: the client signs in again (close code 4001). From
  // message-svc.
  KICK_REASON_PASSWORD_RESET = 2;
  // A newer connection of the device identified, on this or another gateway: the older
  // one stops (close code 4003). From the gateway.
  KICK_REASON_TAKEN_OVER = 3;
}
```

In `crates/push-message-svc/src/notify.rs`:

Replace

```rust
                let kick = Kick {
                    reason: kick_reason(*reason).into(),
                };
                for device in devices {
```

with

```rust
                let kick = Kick {
                    reason: kick_reason(*reason).into(),
                    connection_id: Vec::new(),
                };
                for device in devices {
```

- [ ] **Step 3: The hub by channel**

Replace the whole of `crates/push-gateway/src/hub.rs` with (decision 6; its unit tests
cover the subscriptions, rings, server frames, kicks and takeovers):

```rust
//! What Redis tells the connections of this gateway: which to sync, what to forward, and
//! which to close.
//!
//! Every connection joins the hub under its device as soon as its token passed, before
//! message-svc is asked about the device: the first connection of a device makes the
//! subscriber subscribe to `kick:{device_id}`, so a revocation or password reset that
//! message-svc commits after admitting the device cannot be missed. Once admitted, the
//! connection joins its user too (`notify:{user_id}`). The last connection to leave a
//! channel unsubscribes it.
//!
//! On `notify:{user_id}`, `EntriesAvailable` rings every connection of the user (rings
//! coalesce: a busy connection syncs once more afterwards, not once per ring), and receipts,
//! read positions and contact changes are forwarded to each as server frames. On
//! `kick:{device_id}`, every connection of the device is told to close, except the one a
//! takeover names as the newcomer.
//!
//! Redis is a doorbell, not the truth (spec §2): whenever the subscription is
//! re-established after a break, every connection is rung, since notifications may have
//! been lost meanwhile. A kick lost that way is caught when the connection renews its
//! token (`ReAuth`, within 15 minutes): message-svc refuses a revoked device and a token
//! older than a password reset.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use prost::Message;
use push_proto::internal::v1::{
    ContactChanged, Kick, KickReason, Notification, ReadPositionChanged, ReceiptChanged,
    notification::Kind, receipt_changed,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, ContactEvent, GroupReceipts, ReadPosition, Receipt, receipt};
use push_proto::{id, kick_channel, notify_channel};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How long a joining connection waits for its channel's subscription to become active.
const SUBSCRIBE_WAIT: Duration = Duration::from_secs(2);
/// How long a subscribe or unsubscribe may take before the connection is given up on.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait before reconnecting to Redis.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Server frames waiting for a busy connection; more are dropped (the client recovers
/// them with `ListConversations` and `ListContacts`).
const EVENT_BACKLOG: usize = 64;

/// The connections of this gateway, by device and user, and the Redis subscriber that
/// wakes them.
#[derive(Clone)]
pub struct Hub {
    members: Arc<Mutex<Members>>,
    commands: mpsc::UnboundedSender<Command>,
    connected: Arc<AtomicBool>,
}

#[derive(Default)]
struct Members {
    next_key: u64,
    by_key: HashMap<u64, Member>,
    /// The members of each channel, `notify:{user}` or `kick:{device}`.
    channels: HashMap<String, HashSet<u64>>,
}

struct Member {
    /// Connection id: a takeover names the newcomer by it.
    connection: Uuid,
    doorbell: mpsc::Sender<()>,
    events: mpsc::Sender<Body>,
    kicks: mpsc::Sender<KickReason>,
}

enum Command {
    Subscribe(String, oneshot::Sender<()>),
    Unsubscribe(String),
}

/// What a connection must do.
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    /// New entries may be waiting: sync.
    Ring,
    /// Send this server frame.
    Frame(Box<Body>),
    /// Close: the device was revoked, the password reset, or another connection took
    /// over.
    Kick(KickReason),
}

impl Hub {
    /// A hub whose subscriber task listens on Redis through `client` until `shutdown`.
    pub fn start(client: redis::Client, shutdown: CancellationToken) -> (Self, JoinHandle<()>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let hub = Self {
            members: Arc::default(),
            commands,
            connected: Arc::default(),
        };
        let subscriber = tokio::spawn(subscribe(client, hub.clone(), receiver, shutdown));
        (hub, subscriber)
    }

    /// Whether the subscriber is connected to Redis.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Adds connection `connection` of `device`, to be kicked through `kick:{device}`.
    /// Waits up to 2 s for the subscription to become active, then returns anyway:
    /// without Redis nothing would be heard in time, and the connection's next `ReAuth`
    /// catches what a kick would have said.
    pub async fn watch_device(&self, device: Uuid, connection: Uuid) -> Membership {
        let (doorbell, rung) = mpsc::channel(1);
        let (events, frames) = mpsc::channel(EVENT_BACKLOG);
        let (kicks, kicked) = mpsc::channel(1);
        let member = Member {
            connection,
            doorbell,
            events,
            kicks,
        };
        let (key, subscribed) = {
            let mut members = lock(&self.members);
            let key = members.next_key;
            members.next_key += 1;
            members.by_key.insert(key, member);
            (key, self.enter(&mut members, kick_channel(device), key))
        };
        wait(subscribed).await;
        Membership {
            key,
            channels: vec![kick_channel(device)],
            rung,
            frames,
            kicked,
            hub: self.clone(),
        }
    }

    /// Adds `key` to `channel`, asking the subscriber to subscribe if it is the first.
    /// Sent under the lock, so the commands for a channel reach the subscriber in the
    /// order its members joined and left.
    fn enter(
        &self,
        members: &mut Members,
        channel: String,
        key: u64,
    ) -> Option<oneshot::Receiver<()>> {
        let keys = members.channels.entry(channel.clone()).or_default();
        let first = keys.is_empty();
        keys.insert(key);
        first.then(|| {
            let (done, subscribed) = oneshot::channel();
            let _ = self.commands.send(Command::Subscribe(channel, done));
            subscribed
        })
    }

    /// The members of `channel`.
    fn members_of<'a>(members: &'a Members, channel: &str) -> impl Iterator<Item = &'a Member> {
        members
            .channels
            .get(channel)
            .into_iter()
            .flatten()
            .filter_map(|key| members.by_key.get(key))
    }

    /// Wakes every connection on `channel`.
    fn ring(&self, channel: &str) {
        let members = lock(&self.members);
        for member in Self::members_of(&members, channel) {
            // A full doorbell has already been rung and not answered yet.
            let _ = member.doorbell.try_send(());
        }
    }

    /// Hands a server frame to every connection on `channel`.
    fn forward(&self, channel: &str, frame: &Body) {
        let members = lock(&self.members);
        for member in Self::members_of(&members, channel) {
            if member.events.try_send(frame.clone()).is_err() {
                tracing::warn!("a connection is not keeping up; a server frame was dropped");
            }
        }
    }

    /// Tells every connection on `channel` to close, but the newcomer a takeover names.
    fn kick(&self, channel: &str, kick: &Kick) {
        let members = lock(&self.members);
        let newcomer = id(&kick.connection_id).ok();
        for member in Self::members_of(&members, channel) {
            if Some(member.connection) != newcomer {
                let _ = member.kicks.try_send(kick.reason());
            }
        }
    }

    /// Wakes every connection.
    fn ring_all(&self) {
        for member in lock(&self.members).by_key.values() {
            let _ = member.doorbell.try_send(());
        }
    }

    /// The channels with at least one member.
    fn channels(&self) -> Vec<String> {
        lock(&self.members).channels.keys().cloned().collect()
    }

    /// Acts on a message from Redis. Malformed ones are logged and dropped.
    fn dispatch(&self, channel: &str, payload: &[u8]) {
        if let Some(rest) = channel.strip_prefix("notify:") {
            if Uuid::parse_str(rest).is_err() {
                tracing::warn!(channel, "message on an unexpected channel");
                return;
            }
            match Notification::decode(payload) {
                Ok(Notification {
                    kind: Some(Kind::EntriesAvailable(_)),
                }) => self.ring(channel),
                Ok(Notification { kind: Some(kind) }) => {
                    if let Some(frame) = server_frame(kind) {
                        self.forward(channel, &frame);
                    }
                }
                Ok(Notification { kind: None }) => {}
                Err(error) => tracing::warn!(%error, channel, "malformed notification"),
            }
        } else if let Some(rest) = channel.strip_prefix("kick:") {
            if Uuid::parse_str(rest).is_err() {
                tracing::warn!(channel, "message on an unexpected channel");
                return;
            }
            match Kick::decode(payload) {
                Ok(kick) if kick.reason() != KickReason::Unspecified => self.kick(channel, &kick),
                Ok(_) => tracing::warn!(channel, "a kick without a reason"),
                Err(error) => tracing::warn!(%error, channel, "malformed kick"),
            }
        } else {
            tracing::warn!(channel, "message on an unexpected channel");
        }
    }
}

/// The server frame a notification becomes, if any.
fn server_frame(kind: Kind) -> Option<Body> {
    Some(match kind {
        Kind::EntriesAvailable(_) => return None,
        Kind::ReceiptChanged(ReceiptChanged {
            conversation_id,
            positions,
        }) => {
            let positions = match positions? {
                receipt_changed::Positions::Direct(direct) => {
                    receipt::Positions::Direct(v1::DirectReceipt {
                        user_id: direct.user_id,
                        delivered_seq: direct.delivered_seq,
                        read_seq: direct.read_seq,
                    })
                }
                receipt_changed::Positions::Group(group) => {
                    receipt::Positions::Group(GroupReceipts {
                        all_delivered_seq: group.all_delivered_seq,
                        all_read_seq: group.all_read_seq,
                    })
                }
            };
            Body::Receipt(Receipt {
                conversation_id,
                positions: Some(positions),
            })
        }
        Kind::ContactChanged(ContactChanged { peer_id, state }) => {
            Body::ContactEvent(ContactEvent {
                user_id: peer_id,
                state,
            })
        }
        Kind::ReadPositionChanged(ReadPositionChanged {
            conversation_id,
            read_seq,
        }) => Body::ReadPosition(ReadPosition {
            conversation_id,
            read_seq,
        }),
    })
}

fn lock(members: &Mutex<Members>) -> MutexGuard<'_, Members> {
    members.lock().expect("hub lock poisoned")
}

/// Waits up to [`SUBSCRIBE_WAIT`] for a subscription to become active.
async fn wait(subscribed: Option<oneshot::Receiver<()>>) {
    if let Some(subscribed) = subscribed {
        let _ = tokio::time::timeout(SUBSCRIBE_WAIT, subscribed).await;
    }
}

/// One connection's place in the hub. Dropping it leaves every channel it joined.
pub struct Membership {
    key: u64,
    channels: Vec<String>,
    rung: mpsc::Receiver<()>,
    frames: mpsc::Receiver<Body>,
    kicked: mpsc::Receiver<KickReason>,
    hub: Hub,
}

impl Membership {
    /// Joins the connection's user, `notify:{user}`, once message-svc admitted the device.
    /// Waits up to 2 s for the subscription; the connection syncs after joining, which
    /// covers whatever a late subscription misses.
    pub async fn join_user(&mut self, user: Uuid) {
        let channel = notify_channel(user);
        let subscribed = {
            let mut members = lock(&self.hub.members);
            self.hub.enter(&mut members, channel.clone(), self.key)
        };
        self.channels.push(channel);
        wait(subscribed).await;
    }

    /// The next thing to do: a kick first, then a server frame, then a ring.
    pub async fn next(&mut self) -> Signal {
        tokio::select! {
            biased;
            Some(reason) = self.kicked.recv() => Signal::Kick(reason),
            Some(frame) = self.frames.recv() => Signal::Frame(Box::new(frame)),
            Some(()) = self.rung.recv() => Signal::Ring,
            // The hub keeps the senders while this membership exists; never reached.
            else => std::future::pending().await,
        }
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        let mut members = lock(&self.hub.members);
        members.by_key.remove(&self.key);
        for channel in &self.channels {
            if let Some(keys) = members.channels.get_mut(channel) {
                keys.remove(&self.key);
                if keys.is_empty() {
                    members.channels.remove(channel);
                    let _ = self
                        .hub
                        .commands
                        .send(Command::Unsubscribe(channel.clone()));
                }
            }
        }
    }
}

/// Runs a Redis command with a time limit. `None` when `shutdown` came first; otherwise
/// whether the command succeeded, a timeout counting as a failure.
async fn bounded(
    shutdown: &CancellationToken,
    command: impl Future<Output = redis::RedisResult<()>>,
) -> Option<bool> {
    tokio::select! {
        () = shutdown.cancelled() => None,
        result = tokio::time::timeout(COMMAND_TIMEOUT, command) => match result {
            Ok(result) => Some(result.is_ok()),
            Err(_) => {
                tracing::warn!("Redis did not answer a subscription command in time");
                Some(false)
            }
        },
    }
}

/// The subscriber task: keeps one Redis pub/sub connection, follows the hub's subscribe
/// and unsubscribe commands, and dispatches what arrives. Reconnects after a break,
/// subscribes again to every channel with a member and rings every connection.
/// Connecting is retried while Redis is unavailable, and `shutdown` interrupts both the
/// connection attempt and the wait between attempts at once.
async fn subscribe(
    client: redis::Client,
    hub: Hub,
    mut commands: mpsc::UnboundedReceiver<Command>,
    shutdown: CancellationToken,
) {
    loop {
        let connection = tokio::select! {
            () = shutdown.cancelled() => return,
            connection = client.get_async_pubsub() => connection,
        };
        match connection {
            Ok(pubsub) => {
                let (mut sink, mut messages) = pubsub.split();
                let mut resubscribed = true;
                for channel in hub.channels() {
                    match bounded(&shutdown, sink.subscribe(&channel)).await {
                        None => return,
                        Some(true) => {}
                        Some(false) => {
                            resubscribed = false;
                            break;
                        }
                    }
                }
                if resubscribed {
                    hub.connected.store(true, Ordering::SeqCst);
                    tracing::info!("subscribed to Redis");
                    hub.ring_all();
                    loop {
                        tokio::select! {
                            () = shutdown.cancelled() => return,
                            command = commands.recv() => {
                                let succeeded = match command {
                                    None => return,
                                    Some(Command::Subscribe(channel, done)) => {
                                        let Some(succeeded) =
                                            bounded(&shutdown, sink.subscribe(&channel)).await
                                        else {
                                            return;
                                        };
                                        // Sent even when the subscribe failed: the reconnect path
                                        // subscribes every channel again and calls `ring_all`, so
                                        // the joining connection does not need to wait any longer.
                                        let _ = done.send(());
                                        succeeded
                                    }
                                    Some(Command::Unsubscribe(channel)) => {
                                        let Some(succeeded) =
                                            bounded(&shutdown, sink.unsubscribe(&channel)).await
                                        else {
                                            return;
                                        };
                                        succeeded
                                    }
                                };
                                if !succeeded {
                                    break;
                                }
                            }
                            message = messages.next() => match message {
                                None => break,
                                Some(message) => {
                                    hub.dispatch(message.get_channel_name(), message.get_payload_bytes());
                                }
                            },
                        }
                    }
                }
                hub.connected.store(false, Ordering::SeqCst);
                tracing::warn!("lost the Redis subscription; reconnecting");
            }
            Err(error) => tracing::warn!(%error, "cannot reach Redis; retrying"),
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(RECONNECT_DELAY) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use push_proto::id_bytes;
    use push_proto::internal::v1::{DirectReceipt, EntriesAvailable};

    use super::*;

    /// A hub without a subscriber task, for the bookkeeping alone.
    fn hub() -> (Hub, mpsc::UnboundedReceiver<Command>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let hub = Hub {
            members: Arc::default(),
            commands,
            connected: Arc::default(),
        };
        (hub, receiver)
    }

    fn entries_available() -> Vec<u8> {
        Notification {
            kind: Some(Kind::EntriesAvailable(EntriesAvailable {})),
        }
        .encode_to_vec()
    }

    /// A connection of `user` on a new device.
    async fn connection(hub: &Hub, user: Uuid) -> (Membership, Uuid, Uuid) {
        let (device, id) = (Uuid::now_v7(), Uuid::now_v7());
        let mut membership = hub.watch_device(device, id).await;
        membership.join_user(user).await;
        (membership, device, id)
    }

    fn pending(membership: &mut Membership) -> Option<Signal> {
        if let Ok(reason) = membership.kicked.try_recv() {
            return Some(Signal::Kick(reason));
        }
        if let Ok(frame) = membership.frames.try_recv() {
            return Some(Signal::Frame(Box::new(frame)));
        }
        membership.rung.try_recv().ok().map(|()| Signal::Ring)
    }

    fn subscribed(commands: &mut mpsc::UnboundedReceiver<Command>) -> Vec<String> {
        let mut channels = Vec::new();
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Subscribe(channel, _) => channels.push(format!("+{channel}")),
                Command::Unsubscribe(channel) => channels.push(format!("-{channel}")),
            }
        }
        channels
    }

    #[tokio::test(start_paused = true)]
    async fn the_device_is_watched_before_the_user_and_the_last_to_leave_unsubscribes() {
        let (hub, mut commands) = hub();
        let user = Uuid::now_v7();
        let (first, device, _) = connection(&hub, user).await;
        assert_eq!(
            subscribed(&mut commands),
            [
                format!("+{}", kick_channel(device)),
                format!("+{}", notify_channel(user))
            ]
        );
        let (second, other_device, _) = connection(&hub, user).await;
        assert_eq!(
            subscribed(&mut commands),
            [format!("+{}", kick_channel(other_device))],
            "the user is already subscribed"
        );
        drop(first);
        assert_eq!(
            subscribed(&mut commands),
            [format!("-{}", kick_channel(device))]
        );
        drop(second);
        let mut left = subscribed(&mut commands);
        left.sort();
        let mut expected = vec![
            format!("-{}", kick_channel(other_device)),
            format!("-{}", notify_channel(user)),
        ];
        expected.sort();
        assert_eq!(left, expected);
        assert!(hub.channels().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn entries_available_rings_only_that_users_connections() {
        let (hub, _commands) = hub();
        let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
        let (mut alice_phone, _, _) = connection(&hub, alice).await;
        let (mut alice_laptop, _, _) = connection(&hub, alice).await;
        let (mut bob_phone, _, _) = connection(&hub, bob).await;
        hub.dispatch(&notify_channel(alice), &entries_available());
        assert_eq!(pending(&mut alice_phone), Some(Signal::Ring));
        assert_eq!(pending(&mut alice_laptop), Some(Signal::Ring));
        assert_eq!(pending(&mut bob_phone), None);
    }

    #[tokio::test(start_paused = true)]
    async fn rings_coalesce_until_answered() {
        let (hub, _commands) = hub();
        let user = Uuid::now_v7();
        let (mut session, _, _) = connection(&hub, user).await;
        for _ in 0..3 {
            hub.dispatch(&notify_channel(user), &entries_available());
        }
        assert_eq!(pending(&mut session), Some(Signal::Ring));
        assert_eq!(pending(&mut session), None);
    }

    #[tokio::test(start_paused = true)]
    async fn receipts_read_positions_and_contact_changes_become_server_frames() {
        let (hub, _commands) = hub();
        let (user, peer) = (Uuid::now_v7(), Uuid::now_v7());
        let conversation = id_bytes(Uuid::now_v7());
        let (mut session, _, _) = connection(&hub, user).await;
        let notifications = [
            Kind::ReceiptChanged(ReceiptChanged {
                conversation_id: conversation.clone(),
                positions: Some(receipt_changed::Positions::Direct(DirectReceipt {
                    user_id: id_bytes(peer),
                    delivered_seq: 4,
                    read_seq: 3,
                })),
            }),
            Kind::ReadPositionChanged(ReadPositionChanged {
                conversation_id: conversation.clone(),
                read_seq: 9,
            }),
            Kind::ContactChanged(ContactChanged {
                peer_id: id_bytes(peer),
                state: v1::ContactState::Accepted.into(),
            }),
        ];
        for kind in notifications {
            let payload = Notification { kind: Some(kind) }.encode_to_vec();
            hub.dispatch(&notify_channel(user), &payload);
        }
        let frames = [
            Body::Receipt(Receipt {
                conversation_id: conversation.clone(),
                positions: Some(receipt::Positions::Direct(v1::DirectReceipt {
                    user_id: id_bytes(peer),
                    delivered_seq: 4,
                    read_seq: 3,
                })),
            }),
            Body::ReadPosition(ReadPosition {
                conversation_id: conversation,
                read_seq: 9,
            }),
            Body::ContactEvent(ContactEvent {
                user_id: id_bytes(peer),
                state: v1::ContactState::Accepted.into(),
            }),
        ];
        for frame in frames {
            assert_eq!(pending(&mut session), Some(Signal::Frame(Box::new(frame))));
        }
        assert_eq!(pending(&mut session), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_kick_closes_the_devices_connections_but_the_newcomer_of_a_takeover() {
        let (hub, _commands) = hub();
        let user = Uuid::now_v7();
        let (mut old, device, _) = connection(&hub, user).await;
        let newcomer = Uuid::now_v7();
        let mut new = hub.watch_device(device, newcomer).await;
        let (mut other, _, _) = connection(&hub, user).await;
        let takeover = Kick {
            reason: KickReason::TakenOver.into(),
            connection_id: id_bytes(newcomer),
        };
        hub.dispatch(&kick_channel(device), &takeover.encode_to_vec());
        assert_eq!(pending(&mut old), Some(Signal::Kick(KickReason::TakenOver)));
        assert_eq!(pending(&mut new), None);
        assert_eq!(pending(&mut other), None);

        let revoked = Kick {
            reason: KickReason::DeviceRevoked.into(),
            connection_id: Vec::new(),
        };
        hub.dispatch(&kick_channel(device), &revoked.encode_to_vec());
        assert_eq!(
            pending(&mut new),
            Some(Signal::Kick(KickReason::DeviceRevoked))
        );
        assert_eq!(pending(&mut other), None);
    }

    #[tokio::test(start_paused = true)]
    async fn strange_messages_ring_and_kick_nobody() {
        let (hub, _commands) = hub();
        let user = Uuid::now_v7();
        let (mut session, device, _) = connection(&hub, user).await;
        hub.dispatch("notify:not-a-uuid", &entries_available());
        hub.dispatch(&format!("other:{user}"), &entries_available());
        hub.dispatch(&notify_channel(user), &[0xff, 0xff]);
        hub.dispatch(&kick_channel(device), &[0xff, 0xff]);
        hub.dispatch(&kick_channel(device), &Kick::default().encode_to_vec());
        assert_eq!(pending(&mut session), None);
    }

    #[tokio::test(start_paused = true)]
    async fn ring_all_wakes_every_connection() {
        let (hub, _commands) = hub();
        let (mut a, _, _) = connection(&hub, Uuid::now_v7()).await;
        let (mut b, _, _) = connection(&hub, Uuid::now_v7()).await;
        hub.ring_all();
        assert_eq!(pending(&mut a), Some(Signal::Ring));
        assert_eq!(pending(&mut b), Some(Signal::Ring));
    }
}
```

In `crates/push-gateway/src/lib.rs`:

Replace

```rust
pub use config::{Config, ConfigError};
pub use hub::{Hub, Membership};
pub use server::{init_logging, serve, terminated};
pub use session::{RetryDelay, Session, Step};
```

with

```rust
pub use config::{Config, ConfigError};
pub use hub::{Hub, Membership, Signal};
pub use server::{init_logging, serve, terminated};
pub use session::{RetryDelay, Session, Step};
```

The connection watches its device between the token check and message-svc's answer,
publishes its takeover before `Ready`, forwards server frames and closes on kicks. In
`crates/push-gateway/src/server.rs`:

Replace

```rust
use axum::response::Response;
use axum::routing::get;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{AckRequest, IdentifyRequest, SendRequest, SyncRequest};
use push_proto::v1::frame::Body;
use push_proto::v1::{AuthExpiring, Error, ErrorCode};
use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes};
use tokio::net::TcpListener;
use tokio::time::Instant;
```

with

```rust
use axum::response::Response;
use axum::routing::get;
use prost::Message as _;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AckRequest, IdentifyRequest, Kick, KickReason, SendRequest, SyncRequest,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{AuthExpiring, Error, ErrorCode};
use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes, kick_channel};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use tokio::net::TcpListener;
use tokio::time::Instant;
```

Replace

```rust
use crate::config::Config;
use crate::hub::Hub;
use crate::session::{RetryDelay, Session, Step, error_frame};
use crate::tokens::{Claims, TokenError, Verifier};
```

with

```rust
use crate::config::Config;
use crate::hub::{Hub, Membership, Signal};
use crate::session::{RetryDelay, Session, Step, error_frame};
use crate::tokens::{Claims, TokenError, Verifier};
```

Replace

```rust
/// How soon to fetch the keys again after a failed fetch.
const KEYS_RETRY: Duration = Duration::from_secs(2);

#[derive(Clone)]
```

with

```rust
/// How soon to fetch the keys again after a failed fetch.
const KEYS_RETRY: Duration = Duration::from_secs(2);
/// How long a Redis command (other than the subscriber's) may take.
const REDIS_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone)]
```

Replace

```rust
    verifier: Verifier,
    hub: Hub,
    sessions: TaskTracker,
    shutdown: CancellationToken,
```

with

```rust
    verifier: Verifier,
    hub: Hub,
    /// Publishes takeovers on `kick:{device_id}`.
    redis: ConnectionManager,
    sessions: TaskTracker,
    shutdown: CancellationToken,
```

Replace

```rust
    let redis = redis::Client::open(config.redis_url.as_str())
        .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
    let (hub, subscriber) = Hub::start(redis, shutdown.clone());
    let verifier = Verifier::new(config.jwks_url(), &config.token_audience);
```

with

```rust
    let redis = redis::Client::open(config.redis_url.as_str())
        .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
    let commands = ConnectionManagerConfig::new()
        .set_connection_timeout(Some(REDIS_TIMEOUT))
        .set_response_timeout(Some(REDIS_TIMEOUT))
        .set_number_of_retries(1);
    let publisher = redis
        .get_connection_manager_lazy(commands)
        .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
    let (hub, subscriber) = Hub::start(redis, shutdown.clone());
    let verifier = Verifier::new(config.jwks_url(), &config.token_audience);
```

Replace

```rust
        verifier: verifier.clone(),
        hub: hub.clone(),
        sessions: sessions.clone(),
        shutdown: shutdown.clone(),
```

with

```rust
        verifier: verifier.clone(),
        hub: hub.clone(),
        redis: publisher,
        sessions: sessions.clone(),
        shutdown: shutdown.clone(),
```

Replace

```rust
async fn connection(mut socket: WebSocket, app: App) {
    let mut session = Session::new();
    let Some(claims) = identify(&mut socket, &mut session, &app).await else {
        return;
    };
    let mut membership = app.hub.join(claims.user).await;
    let Some(ready) = session.identified(&claims, app.config.heartbeat) else {
        return;
    };
    let mut connected = Connected {
        socket,
```

with

```rust
async fn connection(mut socket: WebSocket, app: App) {
    let mut session = Session::new();
    let connection_id = Uuid::now_v7();
    let Some((claims, mut membership)) =
        identify(&mut socket, &mut session, &app, connection_id).await
    else {
        return;
    };
    membership.join_user(claims.user).await;
    let Some(ready) = session.identified(&claims, app.config.heartbeat) else {
        return;
    };
    take_over(&app, claims.device, connection_id).await;
    let mut connected = Connected {
        socket,
```

Replace

```rust
                return;
            }
            () = membership.rung() => {
                if connected.deliver().await.is_err() {
                    return;
                }
```

with

```rust
                return;
            }
            signal = membership.next() => {
                let done = match signal {
                    Signal::Ring => connected.deliver().await,
                    Signal::Frame(frame) => send(&mut connected.socket, *frame).await,
                    Signal::Kick(reason) => {
                        let (code, why) = kick_close(reason);
                        tracing::info!(device = %connected.device, ?reason, "connection kicked");
                        close_with(&mut connected.socket, code, why).await;
                        Err(Closed)
                    }
                };
                if done.is_err() {
                    return;
                }
```

Replace

```rust
/// Waits for the first frame, checks its token and asks message-svc whether the device it
/// names may connect. `None` once the connection is closed.
async fn identify(socket: &mut WebSocket, session: &mut Session, app: &App) -> Option<Claims> {
    let deadline = tokio::time::sleep(IDENTIFY_TIMEOUT);
    tokio::pin!(deadline);
```

with

```rust
/// Waits for the first frame, checks its token and asks message-svc whether the device it
/// names may connect. The connection watches the device's kicks before message-svc
/// answers, so a revocation or password reset committed after the answer still reaches
/// it. `None` once the connection is closed.
async fn identify(
    socket: &mut WebSocket,
    session: &mut Session,
    app: &App,
    connection_id: Uuid,
) -> Option<(Claims, Membership)> {
    let deadline = tokio::time::sleep(IDENTIFY_TIMEOUT);
    tokio::pin!(deadline);
```

Replace

```rust
        }
    };
    match admit(&mut app.messaging.clone(), &claims).await {
        Ok(()) => {
            tracing::info!(user = %claims.user, device = %claims.device, "device connected");
            Some(claims)
        }
        Err(refusal) => {
```

with

```rust
        }
    };
    let membership = app.hub.watch_device(claims.device, connection_id).await;
    match admit(&mut app.messaging.clone(), &claims).await {
        Ok(()) => {
            tracing::info!(user = %claims.user, device = %claims.device, "device connected");
            Some((claims, membership))
        }
        Err(refusal) => {
```

Replace

```rust
}

/// The close code and reason for a token that did not pass.
fn token_refusal(error: TokenError) -> (u16, &'static str) {
```

with

```rust
}

/// Tells any other connection of the device, on any gateway, to close: the newest
/// connection wins (spec §7.3). Best effort: without Redis two connections may coexist
/// for a while, which the client tolerates.
async fn take_over(app: &App, device: Uuid, connection_id: Uuid) {
    let kick = Kick {
        reason: KickReason::TakenOver.into(),
        connection_id: id_bytes(connection_id),
    };
    let mut redis = app.redis.clone();
    let mut command = redis::cmd("PUBLISH");
    command.arg(kick_channel(device)).arg(kick.encode_to_vec());
    let publish = command.exec_async(&mut redis);
    tokio::select! {
        () = app.shutdown.cancelled() => {}
        published = tokio::time::timeout(REDIS_TIMEOUT, publish) => {
            if !matches!(published, Ok(Ok(()))) {
                tracing::warn!(%device, "could not announce the connection to other gateways");
            }
        }
    }
}

/// The close code and reason for a kick.
fn kick_close(reason: KickReason) -> (u16, &'static str) {
    match reason {
        KickReason::PasswordReset => (
            close::INVALID_IDENTITY,
            "the password was changed; sign in again",
        ),
        KickReason::DeviceRevoked => (close::TAKEN_OVER, "the device was revoked"),
        KickReason::TakenOver | KickReason::Unspecified => (
            close::TAKEN_OVER,
            "another connection of this device took over",
        ),
    }
}

/// The close code and reason for a token that did not pass.
fn token_refusal(error: TokenError) -> (u16, &'static str) {
```

- [ ] **Step 4: The smoke test skips takeovers on Redis**

Every gateway now publishes a takeover when a device connects; the smoke test still listens
for message-svc's kicks on Redis until Plan 8B. In `crates/push-smoke/src/kicks.rs`:

Replace

```rust
    }

    /// Waits for `device` to be kicked for `reason`; how long that took.
    pub async fn kicked(&mut self, device: Uuid, reason: KickReason) -> anyhow::Result<Duration> {
        let started = Instant::now();
        let channel = kick_channel(device);
        let mut messages = self.pubsub.on_message();
        let Ok(message) = tokio::time::timeout(WORKER_TIMEOUT, messages.next()).await else {
            bail!("{device} was not kicked within {WORKER_TIMEOUT:?}");
        };
        let message = message.ok_or_else(|| anyhow!("the Redis subscription ended"))?;
        if message.get_channel_name() != channel {
            bail!(
                "waiting for {device} to be kicked, but {} was kicked instead",
                message.get_channel_name()
            );
        }
        let kick = Kick::decode(message.get_payload_bytes()).context("decode a kick")?;
        if kick.reason() != reason {
            bail!(
                "{device} was kicked for {:?}, not {reason:?}",
                kick.reason()
            );
        }
        Ok(started.elapsed())
    }
}
```

with

```rust
    }

    /// Waits for `device` to be kicked for `reason`; how long that took. The takeovers
    /// a gateway announces whenever a device connects are skipped.
    pub async fn kicked(&mut self, device: Uuid, reason: KickReason) -> anyhow::Result<Duration> {
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + WORKER_TIMEOUT;
        let channel = kick_channel(device);
        let mut messages = self.pubsub.on_message();
        loop {
            let Ok(message) = tokio::time::timeout_at(deadline, messages.next()).await else {
                bail!("{device} was not kicked within {WORKER_TIMEOUT:?}");
            };
            let message = message.ok_or_else(|| anyhow!("the Redis subscription ended"))?;
            let kick = Kick::decode(message.get_payload_bytes()).context("decode a kick")?;
            if kick.reason() == KickReason::TakenOver {
                continue;
            }
            if message.get_channel_name() != channel {
                bail!(
                    "waiting for {device} to be kicked, but {} was kicked instead",
                    message.get_channel_name()
                );
            }
            if kick.reason() != reason {
                bail!(
                    "{device} was kicked for {:?}, not {reason:?}",
                    kick.reason()
                );
            }
            return Ok(started.elapsed());
        }
    }
}
```

```powershell
cargo test -p push-gateway -p push-smoke
```

Expected: everything passes, among them
`a_revocation_and_a_password_reset_close_the_socket_at_once`,
`a_newer_connection_of_the_device_takes_over`,
`contact_events_receipts_and_read_positions_reach_the_sockets` and
`a_kick_closes_the_devices_connections_but_the_newcomer_of_a_takeover`.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "feat(gateway): kicks, takeovers and server frames from Redis; kick channel watched before Identify"
```

Expected: 841 tests pass, no warnings, no diff.

---

### Task 8: Every request frame to message-svc; groups behind one switch; 1009

**Files:**
- Modify: `crates/push-gateway/tests/gateway.rs`, `src/session.rs`, `src/config.rs`,
  `src/server.rs`, `Cargo.toml`, `crates/push-smoke/tests/local.rs`, `README.md`,
  `deploy/helm/push-messenger/values.yaml`, `templates/gateway.yaml`,
  `deploy/helm/test-chart.ps1`

- [ ] **Step 1: The cases, failing**

Requests, edits, deletes and reads go through, an error answers its own request, a group
request is refused, and a message over 128 KiB ends the connection with 1009. In
`crates/push-gateway/tests/gateway.rs`:

Replace

```rust
    ReadUpToRequest, ReadUpToResponse, RequestCall, SendRequest, SyncRequest, SyncResponse,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, AuthExpiring, ContactEvent, ContactState, ContentType, DirectReceipt, Identify, Ping,
    Pong, ReAuth, ReadPosition, Receipt, Send, SendAck, receipt,
};
use push_proto::{decode_frame, encode_frame, id, id_bytes};
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
```

with

```rust
    ReadUpToRequest, ReadUpToResponse, RequestCall, SendRequest, SyncRequest, SyncResponse,
};
use push_proto::v1;
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, AuthExpiring, Contact, ContactAccept, ContactEvent, ContactRequest, ContactState,
    ContentType, CreateGroup, Delete, DirectReceipt, Edit, ErrorCode, Identify, ListConversations,
    OpenDirect, Ping, Pong, ReAuth, ReadPosition, ReadUpTo, Receipt, Send, SendAck, delete, edit,
    receipt, request, response,
};
use push_proto::{MAX_FRAME_BYTES, decode_frame, encode_frame, id, id_bytes};
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
```

Replace

```rust
        health_addr: health.local_addr().unwrap(),
        heartbeat: Duration::from_secs(30),
    };
    let shutdown = CancellationToken::new();
```

with

```rust
        health_addr: health.local_addr().unwrap(),
        heartbeat: Duration::from_secs(30),
        groups: false,
    };
    let shutdown = CancellationToken::new();
```

Replace

```rust
}

/// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
/// messages from Alice in it, sent through message-svc before anyone connects.
```

with

```rust
}

/// Sends a request and waits for its answer, skipping other frames.
async fn request(socket: &mut Socket, request_id: u64, body: request::Body) -> Body {
    send(
        socket,
        Body::Request(v1::Request {
            request_id,
            body: Some(body),
        }),
    )
    .await;
    wait_for(socket, |frame| match frame {
        Body::Response(response) => response.request_id == request_id,
        Body::Error(error) => error.request_id == Some(request_id),
        _ => false,
    })
    .await
}

/// Every client frame of spec §7.1 travels to message-svc and back; group requests are
/// refused (groups are not offered to apps in the first release).
#[tokio::test]
async fn requests_edits_deletes_and_reads_go_through_and_group_requests_are_refused() {
    let stack = stack().await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    let mut alice_socket = ready(&stack, alice, alice_device).await;
    let mut bob_socket = ready(&stack, bob, bob_device).await;

    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(bob),
    });
    let Body::Response(answer) = request(&mut alice_socket, 1, ask).await else {
        panic!("the contact request failed");
    };
    assert_eq!(
        answer.body,
        Some(response::Body::Contact(Contact {
            user_id: id_bytes(bob),
            state: ContactState::RequestedOut.into(),
        }))
    );
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(alice),
    });
    assert!(matches!(
        request(&mut bob_socket, 2, accept).await,
        Body::Response(_)
    ));
    let open = request::Body::OpenDirect(OpenDirect {
        peer_user_id: id_bytes(bob),
    });
    let Body::Response(v1::Response {
        body: Some(response::Body::DirectOpened(opened)),
        ..
    }) = request(&mut alice_socket, 3, open).await
    else {
        panic!("opening the conversation failed");
    };
    let conversation = opened.conversation_id;

    let client_msg_id = id_bytes(Uuid::now_v7());
    send(
        &mut alice_socket,
        Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.clone(),
            content_type: ContentType::Text.into(),
            body: b"hello".to_vec(),
            client_ts: None,
        }),
    )
    .await;
    let edit_id = id_bytes(Uuid::now_v7());
    send(
        &mut alice_socket,
        Body::Edit(Edit {
            client_msg_id: edit_id.clone(),
            conversation_id: conversation.clone(),
            target: Some(edit::Target::TargetClientMsgId(client_msg_id.clone())),
            body: b"hello, edited".to_vec(),
            client_ts: None,
        }),
    )
    .await;
    let Body::SendAck(edited) = wait_for(
        &mut alice_socket,
        |frame| matches!(frame, Body::SendAck(ack) if ack.client_msg_id == edit_id),
    )
    .await
    else {
        unreachable!();
    };
    assert_eq!(edited.seq, 2);
    let delete_id = id_bytes(Uuid::now_v7());
    send(
        &mut alice_socket,
        Body::Delete(Delete {
            client_msg_id: delete_id.clone(),
            conversation_id: conversation.clone(),
            target: Some(delete::Target::TargetClientMsgId(client_msg_id)),
        }),
    )
    .await;
    let deleted = wait_for(
        &mut alice_socket,
        |frame| matches!(frame, Body::SendAck(ack) if ack.client_msg_id == delete_id),
    )
    .await;
    assert!(matches!(deleted, Body::SendAck(ack) if ack.seq == 3));
    for seq in 1..=3 {
        let Body::Deliver(entry) =
            wait_for(&mut bob_socket, |frame| matches!(frame, Body::Deliver(_))).await
        else {
            unreachable!();
        };
        assert_eq!(entry.seq, seq);
    }
    send(
        &mut bob_socket,
        Body::ReadUpTo(ReadUpTo {
            conversation_id: conversation.clone(),
            seq: 1,
        }),
    )
    .await;
    let position = wait_for(&mut bob_socket, |frame| {
        matches!(frame, Body::ReadPosition(_))
    })
    .await;
    assert!(matches!(position, Body::ReadPosition(read) if read.read_seq == 1));

    // An error answers the request it belongs to.
    let stranger = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(Uuid::now_v7()),
    });
    let Body::Error(refused) = request(&mut alice_socket, 4, stranger).await else {
        panic!("accepting a stranger succeeded");
    };
    assert_eq!(refused.code(), ErrorCode::NoPendingRequest);

    let group = request::Body::CreateGroup(CreateGroup {
        title: "Friends".to_owned(),
        member_ids: vec![id_bytes(bob)],
    });
    let Body::Error(refused) = request(&mut alice_socket, 5, group).await else {
        panic!("a group was created");
    };
    assert_eq!(refused.code(), ErrorCode::NotAvailable);
    let list = request::Body::ListConversations(ListConversations::default());
    let Body::Response(v1::Response {
        body: Some(response::Body::Conversations(listed)),
        ..
    }) = request(&mut alice_socket, 6, list).await
    else {
        panic!("listing failed");
    };
    assert_eq!(
        listed.conversations.len(),
        1,
        "only the direct conversation"
    );
}

/// A message over the 128 KiB limit ends the connection with 1009 (message too big).
#[tokio::test]
async fn an_oversized_frame_is_closed_with_1009() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    let mut socket = ready(&stack, user, device).await;
    socket
        .send(Message::Binary(vec![0u8; MAX_FRAME_BYTES + 1].into()))
        .await
        .unwrap();
    assert_eq!(close_code(&mut socket).await, 1009);
}

/// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
/// messages from Alice in it, sent through message-svc before anyone connects.
```

```powershell
cargo test -p push-gateway --test gateway
```

Expected: a compile error — `Config` has no field `groups`.

- [ ] **Step 2: The steps and the switch**

`Edit`, `Delete`, `ReadUpTo` and `Request` become steps; group requests are refused with
`NOT_AVAILABLE` unless groups are on (decision 8). In `crates/push-gateway/src/session.rs`,
with its tests:

Replace

```rust
use push_proto::close;
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, Deliver, Error, ErrorCode, Pong, Ready, Send};
use push_proto::{FrameError, decode_frame, id_bytes};
use uuid::Uuid;
```

with

```rust
use push_proto::close;
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, Delete, Deliver, Edit, Error, ErrorCode, Pong, ReadUpTo, Ready, Request, Send, request,
};
use push_proto::{FrameError, decode_frame, id_bytes};
use uuid::Uuid;
```

Replace

```rust
    /// ask [`Session::sync_after_ack`] whether to sync again.
    Ack(Ack),
    /// Close the connection with this code and reason.
    Close(u16, &'static str),
```

with

```rust
    /// ask [`Session::sync_after_ack`] whether to sync again.
    Ack(Ack),
    /// Forward an edit to message-svc; answer with its `SendAck` or an error.
    Edit(Edit),
    /// Forward a delete to message-svc; answer with its `SendAck` or an error.
    Delete(Delete),
    /// Forward a read position to message-svc; answer only if it fails (the user's
    /// devices hear the new position as `ReadPosition`).
    ReadUpTo(ReadUpTo),
    /// Forward a request to message-svc; answer with its `Response` or an error, both
    /// carrying the request's `request_id`.
    Request(Request),
    /// Close the connection with this code and reason.
    Close(u16, &'static str),
```

Replace

```rust
#[derive(Debug)]
pub struct Session {
    state: State,
    /// The highest `seq` sent to the client per conversation, so a sync that repeats
```

with

```rust
#[derive(Debug)]
pub struct Session {
    /// Whether group requests are served (`PUSH_GROUPS`); refused with `NOT_AVAILABLE`
    /// otherwise.
    groups: bool,
    state: State,
    /// The highest `seq` sent to the client per conversation, so a sync that repeats
```

Replace

```rust
impl Session {
    /// A new connection.
    pub fn new() -> Self {
        Self {
            state: State::New,
            sent: HashMap::new(),
```

with

```rust
impl Session {
    /// A new connection. `groups`: whether group requests are served.
    pub fn new(groups: bool) -> Self {
        Self {
            groups,
            state: State::New,
            sent: HashMap::new(),
```

Replace

```rust
            (Body::Send(send), State::Ready { .. }) => Step::Send(send),
            (Body::Ack(ack), State::Ready { .. }) => Step::Ack(ack),
            (Body::Ping(_), State::Ready { .. }) => Step::Reply(Body::Pong(Pong {})),
            (_, State::Ready { .. }) => reply_error(ErrorCode::BadFrame, "not a client frame"),
```

with

```rust
            (Body::Send(send), State::Ready { .. }) => Step::Send(send),
            (Body::Ack(ack), State::Ready { .. }) => Step::Ack(ack),
            (Body::Edit(edit), State::Ready { .. }) => Step::Edit(edit),
            (Body::Delete(delete), State::Ready { .. }) => Step::Delete(delete),
            (Body::ReadUpTo(read), State::Ready { .. }) => Step::ReadUpTo(read),
            (Body::Request(request), State::Ready { .. }) => {
                if !self.groups && request.body.as_ref().is_some_and(is_group_request) {
                    return Step::Reply(Body::Error(Error {
                        code: ErrorCode::NotAvailable.into(),
                        message: "groups are not available".to_owned(),
                        retryable: false,
                        client_msg_id: None,
                        request_id: Some(request.request_id),
                        retry_after_s: None,
                    }));
                }
                Step::Request(request)
            }
            (Body::Ping(_), State::Ready { .. }) => Step::Reply(Body::Pong(Pong {})),
            (_, State::Ready { .. }) => reply_error(ErrorCode::BadFrame, "not a client frame"),
```

Replace

```rust
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
```

with

```rust
}

/// The requests about groups, which apps cannot use in the first release (owner
/// decision): message-svc keeps serving them, the gateway refuses them unless
/// `PUSH_GROUPS=1`. No group can be created otherwise, so no group entries, receipts or
/// listings reach clients either.
fn is_group_request(body: &request::Body) -> bool {
    matches!(
        body,
        request::Body::CreateGroup(_)
            | request::Body::AddMembers(_)
            | request::Body::RemoveMember(_)
            | request::Body::LeaveGroup(_)
            | request::Body::RenameGroup(_)
            | request::Body::SetRole(_)
    )
}
```

Replace

```rust
#[cfg(test)]
mod tests {
    use push_proto::v1::{Identify, Ping, ReAuth};
    use push_proto::{MAX_FRAME_BYTES, encode_frame};
```

with

```rust
#[cfg(test)]
mod tests {
    use push_proto::v1::{
        AddMembers, CreateGroup, Identify, LeaveGroup, ListContacts, Ping, ReAuth, RemoveMember,
        RenameGroup, SetRole,
    };
    use push_proto::{MAX_FRAME_BYTES, encode_frame};
```

Replace

```rust
    fn ready_session() -> (Session, Uuid) {
        let mut session = Session::new();
        assert_eq!(
            session.on_binary(&identify("token")),
```

with

```rust
    fn ready_session() -> (Session, Uuid) {
        let mut session = Session::new(false);
        assert_eq!(
            session.on_binary(&identify("token")),
```

Replace

```rust
    #[test]
    fn identify_hands_over_its_token_and_ready_names_the_tokens_device() {
        let mut session = Session::new();
        assert_eq!(
            session.on_binary(&identify("a.b.c")),
```

with

```rust
    #[test]
    fn identify_hands_over_its_token_and_ready_names_the_tokens_device() {
        let mut session = Session::new(false);
        assert_eq!(
            session.on_binary(&identify("a.b.c")),
```

Replace

```rust
    #[test]
    fn identified_without_a_pending_identity_does_nothing() {
        let mut session = Session::new();
        assert_eq!(session.identified(&claims(), Duration::from_secs(30)), None);
        assert_eq!(session.device(), None);
```

with

```rust
    #[test]
    fn identified_without_a_pending_identity_does_nothing() {
        let mut session = Session::new(false);
        assert_eq!(session.identified(&claims(), Duration::from_secs(30)), None);
        assert_eq!(session.device(), None);
```

Replace

```rust
        let ping = encode_frame(Body::Ping(Ping {}));
        for bytes in [ping.as_slice(), &[0xff, 0xff], &[]] {
            let mut session = Session::new();
            assert_eq!(
                session.on_binary(bytes),
```

with

```rust
        let ping = encode_frame(Body::Ping(Ping {}));
        for bytes in [ping.as_slice(), &[0xff, 0xff], &[]] {
            let mut session = Session::new(false);
            assert_eq!(
                session.on_binary(bytes),
```

Replace

```rust
        }
        assert_eq!(
            Session::new().on_text(),
            Step::Close(1002, "the first frame must be Identify")
        );
```

with

```rust
        }
        assert_eq!(
            Session::new(false).on_text(),
            Step::Close(1002, "the first frame must be Identify")
        );
```

Replace

```rust
        let second = identify("another");
        for bytes in [ping.as_slice(), second.as_slice()] {
            let mut session = Session::new();
            assert_eq!(
                session.on_binary(&identify("token")),
```

with

```rust
        let second = identify("another");
        for bytes in [ping.as_slice(), second.as_slice()] {
            let mut session = Session::new(false);
            assert_eq!(
                session.on_binary(&identify("token")),
```

Replace

```rust
        }));
        assert_eq!(
            Session::new().on_binary(&reauth),
            Step::Close(1002, "the first frame must be Identify")
        );
```

with

```rust
        }));
        assert_eq!(
            Session::new(false).on_binary(&reauth),
            Step::Close(1002, "the first frame must be Identify")
        );
```

Replace

```rust
        let oversized = vec![0u8; MAX_FRAME_BYTES + 1];
        assert_eq!(
            Session::new().on_binary(&oversized),
            Step::Close(1002, "the first frame must be Identify")
        );
```

with

```rust
        let oversized = vec![0u8; MAX_FRAME_BYTES + 1];
        assert_eq!(
            Session::new(false).on_binary(&oversized),
            Step::Close(1002, "the first frame must be Identify")
        );
```

Replace

```rust
    }

    #[test]
    fn entries_already_sent_are_not_sent_again() {
```

with

```rust
    }

    #[test]
    fn edits_deletes_reads_and_requests_become_steps() {
        let (mut session, _) = ready_session();
        let edit = Edit {
            client_msg_id: id_bytes(Uuid::now_v7()),
            ..Edit::default()
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::Edit(edit.clone()))),
            Step::Edit(edit)
        );
        let delete = Delete {
            client_msg_id: id_bytes(Uuid::now_v7()),
            ..Delete::default()
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::Delete(delete.clone()))),
            Step::Delete(delete)
        );
        let read = ReadUpTo {
            conversation_id: id_bytes(Uuid::now_v7()),
            seq: 4,
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::ReadUpTo(read.clone()))),
            Step::ReadUpTo(read)
        );
        let list = Request {
            request_id: 7,
            body: Some(request::Body::ListContacts(ListContacts {})),
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::Request(list.clone()))),
            Step::Request(list)
        );
    }

    fn group_requests() -> Vec<request::Body> {
        let conversation_id = id_bytes(Uuid::now_v7());
        vec![
            request::Body::CreateGroup(CreateGroup::default()),
            request::Body::AddMembers(AddMembers::default()),
            request::Body::RemoveMember(RemoveMember::default()),
            request::Body::LeaveGroup(LeaveGroup { conversation_id }),
            request::Body::RenameGroup(RenameGroup::default()),
            request::Body::SetRole(SetRole::default()),
        ]
    }

    #[test]
    fn group_requests_are_refused_unless_groups_are_switched_on() {
        let (mut session, _) = ready_session();
        for (number, body) in (1..).zip(group_requests()) {
            let request = Request {
                request_id: number,
                body: Some(body),
            };
            let Step::Reply(Body::Error(error)) =
                session.on_binary(&encode_frame(Body::Request(request)))
            else {
                panic!("a group request was not refused");
            };
            assert_eq!(error.code(), ErrorCode::NotAvailable);
            assert_eq!(error.request_id, Some(number));
            assert!(!error.retryable);
        }

        let mut session = Session::new(true);
        session.on_binary(&identify("token"));
        session.identified(&claims(), Duration::from_secs(30));
        for body in group_requests() {
            let request = Request {
                request_id: 1,
                body: Some(body),
            };
            assert_eq!(
                session.on_binary(&encode_frame(Body::Request(request.clone()))),
                Step::Request(request)
            );
        }
    }

    #[test]
    fn entries_already_sent_are_not_sent_again() {
```

In `crates/push-gateway/src/config.rs`:

Replace

```rust
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
///
/// `PUSH_DEV_AUTH=1` (the DEV ONLY identities without a token, removed in Plan 8) is
```

with

```rust
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
/// | `PUSH_GROUPS` | `1` serves group requests; `0` refuses them with `NOT_AVAILABLE` (groups are not offered to apps in the first release) | `0` |
///
/// `PUSH_DEV_AUTH=1` (the DEV ONLY identities without a token, removed in Plan 8) is
```

Replace

```rust
    pub health_addr: SocketAddr,
    pub heartbeat: Duration,
}
```

with

```rust
    pub health_addr: SocketAddr,
    pub heartbeat: Duration,
    /// Whether group requests are served.
    pub groups: bool,
}
```

Replace

```rust
            .field("health_addr", &self.health_addr)
            .field("heartbeat", &self.heartbeat)
            .finish()
    }
```

with

```rust
            .field("health_addr", &self.health_addr)
            .field("heartbeat", &self.heartbeat)
            .field("groups", &self.groups)
            .finish()
    }
```

Replace

```rust
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            heartbeat: Duration::from_millis(heartbeat_ms),
        })
    }
```

with

```rust
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            heartbeat: Duration::from_millis(heartbeat_ms),
            groups: flag(&lookup, "PUSH_GROUPS")?,
        })
    }
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

/// A switch: on only for exactly `1`; off when unset, empty or `0`. Anything else
/// (`true`, `yes`) is refused, so a typo can never leave it in doubt.
fn flag(lookup: &impl Fn(&str) -> Option<String>, name: &'static str) -> Result<bool, ConfigError> {
    match lookup(name).as_deref() {
        None | Some("") | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(ConfigError::Invalid {
            name,
            value: other.to_owned(),
            reason: "use 1 to enable or 0 to disable".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
```

Replace

```rust
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.heartbeat, Duration::from_secs(30));
    }
```

with

```rust
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.heartbeat, Duration::from_secs(30));
        assert!(!config.groups, "groups are off unless switched on");
    }

    #[test]
    fn groups_are_on_only_for_exactly_1() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_GROUPS", value));
            assert_eq!(config(&variables).unwrap().groups, expected, "{value:?}");
        }
        for value in ["true", "yes", " 1"] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_GROUPS", value));
            assert!(config(&variables).is_err(), "{value:?}");
        }
    }
```

- [ ] **Step 3: Forwarding, and the size limit**

`tokio-tungstenite` is the version axum uses, only to recognise its capacity error
(decision 11). In `crates/push-gateway/Cargo.toml`:

Replace

```toml
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
tokio-stream.workspace = true
tokio-util.workspace = true
tonic.workspace = true
```

with

```toml
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
tokio-stream.workspace = true
# Only to recognise axum's WebSocket errors: the same version axum 0.8 uses.
tokio-tungstenite.workspace = true
tokio-util.workspace = true
tonic.workspace = true
```

Replace

```toml
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["test-util"] }
tokio-tungstenite.workspace = true

[lints]
```

with

```toml
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["test-util"] }

[lints]
```

In `crates/push-gateway/src/server.rs`:

Replace

```rust
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AckRequest, IdentifyRequest, Kick, KickReason, SendRequest, SyncRequest,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{AuthExpiring, Error, ErrorCode};
use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes, kick_channel};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
```

with

```rust
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AckRequest, DeleteRequest, EditRequest, IdentifyRequest, Kick, KickReason, ReadUpToRequest,
    RequestCall, SendRequest, SyncRequest,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{AuthExpiring, Error, ErrorCode, SendAck};
use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes, kick_channel};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
```

Replace

```rust
}

async fn upgrade(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response {
    let sessions = app.sessions.clone();
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| sessions.track_future(connection(socket, app)))
}
```

with

```rust
}

/// A message is at most [`MAX_FRAME_BYTES`]. A WebSocket frame may be bigger, up to
/// 512 KiB, so that a message over the limit is read whole and refused with a close frame
/// (1009) the client can read; a frame above 512 KiB is refused from its header, before
/// its payload is buffered, and the client may only see the connection drop.
const MAX_WEBSOCKET_FRAME: usize = 4 * MAX_FRAME_BYTES;

async fn upgrade(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response {
    let sessions = app.sessions.clone();
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_WEBSOCKET_FRAME)
        .on_upgrade(move |socket| sessions.track_future(connection(socket, app)))
}
```

Replace

```rust
/// One client connection, from `Identify` to close.
async fn connection(mut socket: WebSocket, app: App) {
    let mut session = Session::new();
    let connection_id = Uuid::now_v7();
    let Some((claims, mut membership)) =
```

with

```rust
/// One client connection, from `Identify` to close.
async fn connection(mut socket: WebSocket, app: App) {
    let mut session = Session::new(app.config.groups);
    let connection_id = Uuid::now_v7();
    let Some((claims, mut membership)) =
```

Replace

```rust
                    // WebSocket pings are answered by the library.
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                };
                if connected.run(step).await.is_err() {
```

with

```rust
                    // WebSocket pings are answered by the library.
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Err(error)) => {
                        refuse_oversized(&mut connected.socket, error).await;
                        return;
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                };
                if connected.run(step).await.is_err() {
```

Replace

```rust
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(24 * 60 * 60)
```

with

```rust
}

/// A message over the WebSocket limit ([`MAX_FRAME_BYTES`]) ends the connection with 1009
/// (RFC 6455: message too big); other read errors end it without a word, the peer being
/// gone or broken.
async fn refuse_oversized(socket: &mut WebSocket, error: axum::Error) {
    let oversized = error
        .into_inner()
        .downcast::<tokio_tungstenite::tungstenite::Error>()
        .is_ok_and(|error| matches!(*error, tokio_tungstenite::tungstenite::Error::Capacity(_)));
    if oversized {
        close_with(socket, close::MESSAGE_TOO_BIG, "a frame is at most 128 KiB").await;
    }
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(24 * 60 * 60)
```

Replace

```rust
                Some(Ok(Message::Text(_))) => break session.on_text(),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
            },
        }
```

with

```rust
                Some(Ok(Message::Text(_))) => break session.on_text(),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Err(error)) => {
                    refuse_oversized(socket, error).await;
                    return None;
                }
                Some(Ok(Message::Close(_))) | None => return None,
            },
        }
```

Replace

```rust
                }
            }
            Step::Close(code, reason) => {
                close_with(&mut self.socket, code, reason).await;
```

with

```rust
                }
            }
            Step::Edit(edit) => {
                let client_msg_id = edit.client_msg_id.clone();
                let request = EditRequest {
                    user_id: id_bytes(self.user),
                    device_id: id_bytes(self.device),
                    edit: Some(edit),
                };
                let answer = self.messaging.edit(request).await;
                self.acknowledge(answer, client_msg_id).await
            }
            Step::Delete(delete) => {
                let client_msg_id = delete.client_msg_id.clone();
                let request = DeleteRequest {
                    user_id: id_bytes(self.user),
                    device_id: id_bytes(self.device),
                    delete: Some(delete),
                };
                let answer = self.messaging.delete(request).await;
                self.acknowledge(answer, client_msg_id).await
            }
            Step::ReadUpTo(read) => {
                let request = ReadUpToRequest {
                    user_id: id_bytes(self.user),
                    read: Some(read),
                };
                match self.messaging.read_up_to(request).await {
                    Ok(_) => Ok(()),
                    Err(status) => {
                        let error = Body::Error(error_from_status(&status));
                        send(&mut self.socket, error).await
                    }
                }
            }
            Step::Request(request) => {
                let request_id = request.request_id;
                let call = RequestCall {
                    user_id: id_bytes(self.user),
                    request: Some(request),
                };
                let body = match self.messaging.request(call).await {
                    Ok(response) => Body::Response(response.into_inner()),
                    Err(status) => {
                        let mut error = error_from_status(&status);
                        error.request_id = Some(request_id);
                        Body::Error(error)
                    }
                };
                send(&mut self.socket, body).await
            }
            Step::Close(code, reason) => {
                close_with(&mut self.socket, code, reason).await;
```

Replace

```rust
    }

    /// Tells the client its token expires soon, once per token.
    async fn warn_expiry(&mut self) -> Result<(), Closed> {
```

with

```rust
    }

    /// Answers an edit or a delete: its `SendAck`, or the error with its `client_msg_id`.
    async fn acknowledge(
        &mut self,
        answer: Result<tonic::Response<SendAck>, tonic::Status>,
        client_msg_id: Vec<u8>,
    ) -> Result<(), Closed> {
        let body = match answer {
            Ok(ack) => Body::SendAck(ack.into_inner()),
            Err(status) => {
                let mut error = error_from_status(&status);
                error.client_msg_id = Some(client_msg_id);
                Body::Error(error)
            }
        };
        send(&mut self.socket, body).await
    }

    /// Tells the client its token expires soon, once per token.
    async fn warn_expiry(&mut self) -> Result<(), Closed> {
```

In `crates/push-smoke/tests/local.rs`:

Replace

```rust
            health_addr: gateway_health.local_addr().unwrap(),
            heartbeat: Duration::from_secs(30),
        },
        listener,
```

with

```rust
            health_addr: gateway_health.local_addr().unwrap(),
            heartbeat: Duration::from_secs(30),
            groups: false,
        },
        listener,
```

```powershell
cargo test -p push-gateway -p push-smoke
```

Expected: everything passes, among them
`requests_edits_deletes_and_reads_go_through_and_group_requests_are_refused`,
`an_oversized_frame_is_closed_with_1009` and
`group_requests_are_refused_unless_groups_are_switched_on`.

- [ ] **Step 4: The switch in the chart**

In `deploy/helm/push-messenger/values.yaml`:

Replace

```yaml
  # Client ping interval announced in Ready, in milliseconds.
  heartbeatMs: 30000
  # 5 s preStop sleep + the gateway's own 10 s drain + margin.
  terminationGracePeriodSeconds: 30
```

with

```yaml
  # Client ping interval announced in Ready, in milliseconds.
  heartbeatMs: 30000
  # Groups are not offered to apps in the first release (owner decision): the gateway
  # refuses group requests with NOT_AVAILABLE. message-svc keeps full group support;
  # true serves them.
  groups: false
  # 5 s preStop sleep + the gateway's own 10 s drain + margin.
  terminationGracePeriodSeconds: 30
```

In `deploy/helm/push-messenger/templates/gateway.yaml`:

Replace

```yaml
            - name: PUSH_TOKEN_AUDIENCE
              value: {{ include "push.socketUrl" . | quote }}
          ports:
            - name: http
```

with

```yaml
            - name: PUSH_TOKEN_AUDIENCE
              value: {{ include "push.socketUrl" . | quote }}
            - name: PUSH_GROUPS
              value: {{ ternary "1" "0" (eq (toString .Values.gateway.groups) "true") | quote }}
          ports:
            - name: http
```

In `deploy/helm/test-chart.ps1`:

Replace

```powershell
        $default -notmatch 'PUSH_DEV_AUTH'
}
Check 'default: the DevSetup switch is gone' { $default -notmatch 'PUSH_DEV_SETUP' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
```

with

```powershell
        $default -notmatch 'PUSH_DEV_AUTH'
}
Check 'default: groups are refused; one switch serves them' {
    $on = helm template push $chart --set gateway.groups=true | Out-String
    (Doc $default 'Deployment' 'push-gateway') -match 'name: PUSH_GROUPS\s+value: "0"' -and
        (Doc $on 'Deployment' 'push-gateway') -match 'name: PUSH_GROUPS\s+value: "1"'
}
Check 'default: the DevSetup switch is gone' { $default -notmatch 'PUSH_DEV_SETUP' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
```

In `README.md`:

Replace

```markdown
| `PUSH_TOKEN_AUDIENCE` | the gateway's public URL (`wss://…/v1/socket`), the `aud` access tokens must name | required |
| `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready`, 1000 to 600000 | `30000` |

`push-auth`:
```

with

```markdown
| `PUSH_TOKEN_AUDIENCE` | the gateway's public URL (`wss://…/v1/socket`), the `aud` access tokens must name | required |
| `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready`, 1000 to 600000 | `30000` |
| `PUSH_GROUPS` | `1` serves group requests; `0` refuses them with `NOT_AVAILABLE` (groups are not offered to apps in the first release; message-svc keeps them) | `0` |

`push-auth`:
```

```powershell
pwsh deploy/helm/test-chart.ps1
```

Expected: `all chart checks passed` (30 checks).

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock README.md crates deploy
git commit -m "feat(gateway): Edit, Delete, ReadUpTo and requests to message-svc; group requests behind one switch; 1009 for oversized messages"
```

Expected: 846 tests pass, no warnings, no diff.

---

### Task 9: Rate limits

**Files:**
- Create: `crates/push-gateway/src/limits.rs`
- Modify: `crates/push-gateway/Cargo.toml`, `src/lib.rs`, `src/config.rs`, `src/server.rs`,
  `tests/gateway.rs`, `crates/push-smoke/tests/local.rs`, `crates/push-domain/src/invites.rs`,
  `README.md`, `deploy/helm/push-messenger/templates/gateway.yaml`,
  `deploy/helm/test-chart.ps1`

- [ ] **Step 1: The cases, failing**

The 21st redeem in an hour is `RATE_LIMITED` with a retry delay while other requests still
work; a flood of frames is closed with 4004. In `crates/push-gateway/tests/gateway.rs`:

Replace

```rust
use push_proto::v1::{
    Ack, AuthExpiring, Contact, ContactAccept, ContactEvent, ContactRequest, ContactState,
    ContentType, CreateGroup, Delete, DirectReceipt, Edit, ErrorCode, Identify, ListConversations,
    OpenDirect, Ping, Pong, ReAuth, ReadPosition, ReadUpTo, Receipt, Send, SendAck, delete, edit,
    receipt, request, response,
};
use push_proto::{MAX_FRAME_BYTES, decode_frame, encode_frame, id, id_bytes};
```

with

```rust
use push_proto::v1::{
    Ack, AuthExpiring, Contact, ContactAccept, ContactEvent, ContactRequest, ContactState,
    ContentType, CreateGroup, Delete, DirectReceipt, Edit, ErrorCode, Identify, ListContacts,
    ListConversations, OpenDirect, Ping, Pong, ReAuth, ReadPosition, ReadUpTo, Receipt,
    RedeemInvite, Send, SendAck, delete, edit, receipt, request, response,
};
use push_proto::{MAX_FRAME_BYTES, decode_frame, encode_frame, id, id_bytes};
```

Replace

```rust
        heartbeat: Duration::from_secs(30),
        groups: false,
    };
    let shutdown = CancellationToken::new();
```

with

```rust
        heartbeat: Duration::from_secs(30),
        groups: false,
        client_ip_header: None,
    };
    let shutdown = CancellationToken::new();
```

Replace

```rust
}

/// A message over the 128 KiB limit ends the connection with 1009 (message too big).
#[tokio::test]
```

with

```rust
}

/// Invite codes cannot be guessed at speed: after 20 redeems in an hour a user is told to
/// wait, whatever the codes (spec §5.5).
#[tokio::test]
async fn redeeming_invites_is_rate_limited() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    let mut socket = ready(&stack, user, device).await;
    for attempt in 1..=20 {
        let guess = request::Body::RedeemInvite(RedeemInvite {
            code: "0000000000".to_owned(),
        });
        let Body::Error(error) = request(&mut socket, attempt, guess).await else {
            panic!("a made-up code was redeemed");
        };
        assert_eq!(error.code(), ErrorCode::InviteNotFound);
    }
    let guess = request::Body::RedeemInvite(RedeemInvite {
        code: "0000000000".to_owned(),
    });
    let Body::Error(error) = request(&mut socket, 21, guess).await else {
        panic!("a made-up code was redeemed");
    };
    assert_eq!(error.code(), ErrorCode::RateLimited);
    assert!(error.retryable);
    assert!(error.retry_after_s.is_some_and(|seconds| seconds > 3000));
    // Other requests still work.
    let list = request::Body::ListContacts(ListContacts {});
    assert!(matches!(
        request(&mut socket, 22, list).await,
        Body::Response(_)
    ));
}

/// A connection that floods the gateway with frames is closed with 4004.
#[tokio::test]
async fn too_many_frames_close_the_connection_with_4004() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    let mut socket = ready(&stack, user, device).await;
    for _ in 0..150 {
        send(&mut socket, Body::Ping(Ping {})).await;
    }
    assert_eq!(close_code(&mut socket).await, 4004);
}

/// A message over the 128 KiB limit ends the connection with 1009 (message too big).
#[tokio::test]
```

```powershell
cargo test -p push-gateway --test gateway
```

Expected: a compile error — `Config` has no field `client_ip_header`.

- [ ] **Step 2: The limits**

In `crates/push-gateway/Cargo.toml`:

Replace

```toml
[dependencies]
anyhow.workspace = true
axum = { workspace = true, features = ["ws"] }
jsonwebtoken.workspace = true
```

with

```toml
[dependencies]
anyhow.workspace = true
async-trait.workspace = true
axum = { workspace = true, features = ["ws"] }
jsonwebtoken.workspace = true
```

Replace

```toml
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
```

with

```toml
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
```

Create `crates/push-gateway/src/limits.rs` (decision 9):

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

In `crates/push-gateway/src/lib.rs`:

Replace

```rust
mod config;
mod hub;
mod server;
mod session;
```

with

```rust
mod config;
mod hub;
mod limits;
mod server;
mod session;
```

Replace

```rust
pub use config::{Config, ConfigError};
pub use hub::{Hub, Membership, Signal};
pub use server::{init_logging, serve, terminated};
pub use session::{RetryDelay, Session, Step};
```

with

```rust
pub use config::{Config, ConfigError};
pub use hub::{Hub, Membership, Signal};
pub use limits::{
    Action, Counter, FrameBudget, Limit, Limits, Outcome, RedisCounter, Refused, Scope, Subject,
};
pub use server::{init_logging, serve, terminated};
pub use session::{RetryDelay, Session, Step};
```

The client's IP, as auth-svc reads it. In `crates/push-gateway/src/config.rs`:

Replace

```rust
use std::time::Duration;

/// How the gateway runs. Read from the environment by [`Config::from_env`]:
///
```

with

```rust
use std::time::Duration;

use axum::http::HeaderName;

/// How the gateway runs. Read from the environment by [`Config::from_env`]:
///
```

Replace

```rust
/// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
/// | `PUSH_GROUPS` | `1` serves group requests; `0` refuses them with `NOT_AVAILABLE` (groups are not offered to apps in the first release) | `0` |
///
/// `PUSH_DEV_AUTH=1` (the DEV ONLY identities without a token, removed in Plan 8) is
```

with

```rust
/// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
/// | `PUSH_GROUPS` | `1` serves group requests; `0` refuses them with `NOT_AVAILABLE` (groups are not offered to apps in the first release) | `0` |
/// | `PUSH_CLIENT_IP_HEADER` | header with the client's IP, set by the ingress (`x-real-ip`); its last line, then last entry is used | the peer address |
///
/// `PUSH_DEV_AUTH=1` (the DEV ONLY identities without a token, removed in Plan 8) is
```

Replace

```rust
    /// Whether group requests are served.
    pub groups: bool,
}
```

with

```rust
    /// Whether group requests are served.
    pub groups: bool,
    /// Where the ingress puts the client's IP, for the per-IP rate limits; without it the
    /// peer address counts. Only trust a header the proxy in front overwrites.
    pub client_ip_header: Option<HeaderName>,
}
```

Replace

```rust
            .field("heartbeat", &self.heartbeat)
            .field("groups", &self.groups)
            .finish()
    }
```

with

```rust
            .field("heartbeat", &self.heartbeat)
            .field("groups", &self.groups)
            .field("client_ip_header", &self.client_ip_header)
            .finish()
    }
```

Replace

```rust
            }
        }
        let auth_url = required("PUSH_AUTH_URL")?;
        if !(auth_url.starts_with("http://") || auth_url.starts_with("https://")) {
```

with

```rust
            }
        }
        let client_ip_header =
            match lookup("PUSH_CLIENT_IP_HEADER").filter(|v| !v.is_empty()) {
                None => None,
                Some(value) => Some(HeaderName::try_from(value.as_str()).map_err(|_| {
                    ConfigError::Invalid {
                        name: "PUSH_CLIENT_IP_HEADER",
                        value: value.clone(),
                        reason: "not a header name".to_owned(),
                    }
                })?),
            };
        let auth_url = required("PUSH_AUTH_URL")?;
        if !(auth_url.starts_with("http://") || auth_url.starts_with("https://")) {
```

Replace

```rust
            heartbeat: Duration::from_millis(heartbeat_ms),
            groups: flag(&lookup, "PUSH_GROUPS")?,
        })
    }
```

with

```rust
            heartbeat: Duration::from_millis(heartbeat_ms),
            groups: flag(&lookup, "PUSH_GROUPS")?,
            client_ip_header,
        })
    }
```

Replace

```rust
        assert_eq!(config.heartbeat, Duration::from_secs(30));
        assert!(!config.groups, "groups are off unless switched on");
    }
```

with

```rust
        assert_eq!(config.heartbeat, Duration::from_secs(30));
        assert!(!config.groups, "groups are off unless switched on");
        assert_eq!(config.client_ip_header, None);
    }

    #[test]
    fn the_client_ip_header_is_a_header_name() {
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_CLIENT_IP_HEADER", "X-Real-IP"));
        assert_eq!(
            config(&variables).unwrap().client_ip_header,
            Some(HeaderName::from_static("x-real-ip"))
        );
        variables.pop();
        variables.push(("PUSH_CLIENT_IP_HEADER", "not a header"));
        assert!(config(&variables).is_err());
    }
```

`Identify` counts per device and fails open; the requests count per user and IP and fail
closed; every frame spends the budget. In `crates/push-gateway/src/server.rs`:

Replace

```rust
//! the health endpoints and graceful shutdown.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
```

with

```rust
//! the health endpoints and graceful shutdown.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
```

Replace

```rust
use crate::config::Config;
use crate::hub::{Hub, Membership, Signal};
use crate::session::{RetryDelay, Session, Step, error_frame};
use crate::tokens::{Claims, TokenError, Verifier};
```

with

```rust
use crate::config::Config;
use crate::hub::{Hub, Membership, Signal};
use crate::limits::{Action, FrameBudget, Limits, RedisCounter, Refused, Subject};
use crate::session::{RetryDelay, Session, Step, error_frame};
use crate::tokens::{Claims, TokenError, Verifier};
```

Replace

```rust
    /// Publishes takeovers on `kick:{device_id}`.
    redis: ConnectionManager,
    sessions: TaskTracker,
    shutdown: CancellationToken,
```

with

```rust
    /// Publishes takeovers on `kick:{device_id}`.
    redis: ConnectionManager,
    limits: Limits,
    sessions: TaskTracker,
    shutdown: CancellationToken,
```

Replace

```rust
        verifier: verifier.clone(),
        hub: hub.clone(),
        redis: publisher,
        sessions: sessions.clone(),
```

with

```rust
        verifier: verifier.clone(),
        hub: hub.clone(),
        limits: Limits::new(Arc::new(RedisCounter::new(
            publisher.clone(),
            shutdown.clone(),
        ))),
        redis: publisher,
        sessions: sessions.clone(),
```

Replace

```rust
        .with_state(app);
    tracing::info!(addr = %listen_addr, "serving WebSockets");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await?;

    // Upgraded connections outlive the HTTP server; each closes itself with 4005.
```

with

```rust
        .with_state(app);
    tracing::info!(addr = %listen_addr, "serving WebSockets");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned())
    .await?;

    // Upgraded connections outlive the HTTP server; each closes itself with 4005.
```

Replace

```rust
const MAX_WEBSOCKET_FRAME: usize = 4 * MAX_FRAME_BYTES;

async fn upgrade(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response {
    let sessions = app.sessions.clone();
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_WEBSOCKET_FRAME)
        .on_upgrade(move |socket| sessions.track_future(connection(socket, app)))
}

/// One client connection, from `Identify` to close.
async fn connection(mut socket: WebSocket, app: App) {
    let mut session = Session::new(app.config.groups);
    let connection_id = Uuid::now_v7();
```

with

```rust
const MAX_WEBSOCKET_FRAME: usize = 4 * MAX_FRAME_BYTES;

async fn upgrade(
    upgrade: WebSocketUpgrade,
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = client_ip(&headers, app.config.client_ip_header.as_ref(), peer);
    let sessions = app.sessions.clone();
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_WEBSOCKET_FRAME)
        .on_upgrade(move |socket| sessions.track_future(connection(socket, app, ip)))
}

/// The client's IP: the last entry of the last line of the configured header, which the
/// proxy in front overwrites or appends to (`X-Real-IP` on ingress-nginx); else the peer.
fn client_ip(
    headers: &HeaderMap,
    header: Option<&axum::http::HeaderName>,
    peer: SocketAddr,
) -> IpAddr {
    header
        .and_then(|name| headers.get_all(name).iter().next_back())
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .and_then(|last| last.trim().parse().ok())
        .unwrap_or(peer.ip())
}

/// One client connection, from `Identify` to close.
async fn connection(mut socket: WebSocket, app: App, ip: IpAddr) {
    let mut session = Session::new(app.config.groups);
    let connection_id = Uuid::now_v7();
```

Replace

```rust
        messaging: app.messaging.clone(),
        verifier: app.verifier.clone(),
        user: claims.user,
        device: claims.device,
```

with

```rust
        messaging: app.messaging.clone(),
        verifier: app.verifier.clone(),
        limits: app.limits.clone(),
        ip,
        budget: FrameBudget::new(),
        user: claims.user,
        device: claims.device,
```

Replace

```rust
            }
            message = connected.socket.recv() => {
                let step = match message {
                    Some(Ok(Message::Binary(bytes))) => connected.session.on_binary(&bytes),
```

with

```rust
            }
            message = connected.socket.recv() => {
                if matches!(message, Some(Ok(Message::Binary(_) | Message::Text(_))))
                    && !connected.budget.spend()
                {
                    tracing::info!(device = %connected.device, "too many frames");
                    close_with(&mut connected.socket, close::RATE_LIMITED, "too many frames; slow down").await;
                    return;
                }
                let step = match message {
                    Some(Ok(Message::Binary(bytes))) => connected.session.on_binary(&bytes),
```

Replace

```rust
        }
    };
    let membership = app.hub.watch_device(claims.device, connection_id).await;
    match admit(&mut app.messaging.clone(), &claims).await {
```

with

```rust
        }
    };
    let device = Subject {
        device: Some(claims.device),
        ..Subject::default()
    };
    // Fails open: devices keep connecting while Redis is down.
    if let Err(Refused::Limited { .. }) = app.limits.check(Action::Identify, device).await {
        close_with(
            socket,
            close::RATE_LIMITED,
            "too many connections; back off",
        )
        .await;
        return None;
    }
    let membership = app.hub.watch_device(claims.device, connection_id).await;
    match admit(&mut app.messaging.clone(), &claims).await {
```

Replace

```rust
}

/// Tells any other connection of the device, on any gateway, to close: the newest
/// connection wins (spec §7.3). Best effort: without Redis two connections may coexist
```

with

```rust
}

/// The error for a request a rate limit refused: retryable either way, after the window
/// ends or once the counters answer again (fail closed).
fn limited(refused: Refused, request_id: u64) -> Error {
    let (code, message, retry_after_s) = match refused {
        Refused::Limited { retry_after_s } => (
            ErrorCode::RateLimited,
            "too many requests of this kind; retry later",
            retry_after_s,
        ),
        Refused::Unavailable => (
            ErrorCode::Unavailable,
            "the rate limits cannot be checked now; retry",
            1,
        ),
    };
    Error {
        code: code.into(),
        message: message.to_owned(),
        retryable: true,
        client_msg_id: None,
        request_id: Some(request_id),
        retry_after_s: Some(retry_after_s),
    }
}

/// Tells any other connection of the device, on any gateway, to close: the newest
/// connection wins (spec §7.3). Best effort: without Redis two connections may coexist
```

Replace

```rust
    messaging: MessagingClient<Channel>,
    verifier: Verifier,
    user: Uuid,
    device: Uuid,
```

with

```rust
    messaging: MessagingClient<Channel>,
    verifier: Verifier,
    limits: Limits,
    ip: IpAddr,
    budget: FrameBudget,
    user: Uuid,
    device: Uuid,
```

Replace

```rust
            Step::Request(request) => {
                let request_id = request.request_id;
                let call = RequestCall {
                    user_id: id_bytes(self.user),
```

with

```rust
            Step::Request(request) => {
                let request_id = request.request_id;
                if let Some(action) = request.body.as_ref().and_then(Action::of) {
                    let who = Subject {
                        ip: Some(self.ip),
                        user: Some(self.user),
                        device: None,
                    };
                    if let Err(refused) = self.limits.check(action, who).await {
                        let error = limited(refused, request_id);
                        return send(&mut self.socket, Body::Error(error)).await;
                    }
                }
                let call = RequestCall {
                    user_id: id_bytes(self.user),
```

In `crates/push-smoke/tests/local.rs`:

Replace

```rust
            heartbeat: Duration::from_secs(30),
            groups: false,
        },
        listener,
```

with

```rust
            heartbeat: Duration::from_secs(30),
            groups: false,
            client_ip_header: None,
        },
        listener,
```

The claim in the invite code's comment is now true. In `crates/push-domain/src/invites.rs`:

Replace

```rust
//!
//! A code is ten characters of Crockford base32: 50 random bits. That is plenty because
//! redeeming is rate limited per account and per IP at the transport layer (spec §5.6):
//! guessing one live code among a million would take about a billion attempts.

use chrono::TimeDelta;
```

with

```rust
//!
//! A code is ten characters of Crockford base32: 50 random bits. That is plenty because
//! the gateway limits `RedeemInvite` to 20 an hour per user and 60 per IP, in Redis
//! across every gateway, and refuses it while Redis cannot count (spec §5.5): guessing one
//! live code among a million would take about a billion attempts.

use chrono::TimeDelta;
```

```powershell
cargo test -p push-gateway
```

Expected (this run records `async-trait` and `sha2` in `Cargo.lock`): everything passes,
among them `redeeming_invites_is_rate_limited`,
`too_many_frames_close_the_connection_with_4004`,
`a_user_is_limited_across_addresses_and_an_address_across_users`,
`a_failing_counter_is_unavailable` and
`the_frame_budget_allows_bursts_and_a_steady_rate`.

- [ ] **Step 3: The IP header in the chart**

In `deploy/helm/push-messenger/templates/gateway.yaml`:

Replace

```yaml
            - name: PUSH_TOKEN_AUDIENCE
              value: {{ include "push.socketUrl" . | quote }}
            - name: PUSH_GROUPS
              value: {{ ternary "1" "0" (eq (toString .Values.gateway.groups) "true") | quote }}
```

with

```yaml
            - name: PUSH_TOKEN_AUDIENCE
              value: {{ include "push.socketUrl" . | quote }}
            # ingress-nginx overwrites X-Real-IP with the client's address: the per-IP
            # limits on redeeming invites and contact requests count by it.
            - name: PUSH_CLIENT_IP_HEADER
              value: x-real-ip
            - name: PUSH_GROUPS
              value: {{ ternary "1" "0" (eq (toString .Values.gateway.groups) "true") | quote }}
```

In `deploy/helm/test-chart.ps1`:

Replace

```powershell
    $gateway -match 'name: PUSH_AUTH_URL\s+value: "http://push-auth:8080"' -and
        $gateway -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "wss://push.example.com/v1/socket"' -and
        $default -notmatch 'PUSH_DEV_AUTH'
}
```

with

```powershell
    $gateway -match 'name: PUSH_AUTH_URL\s+value: "http://push-auth:8080"' -and
        $gateway -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "wss://push.example.com/v1/socket"' -and
        $gateway -match 'name: PUSH_CLIENT_IP_HEADER\s+value: x-real-ip' -and
        $default -notmatch 'PUSH_DEV_AUTH'
}
```

In `README.md`:

Replace

```markdown
| `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready`, 1000 to 600000 | `30000` |
| `PUSH_GROUPS` | `1` serves group requests; `0` refuses them with `NOT_AVAILABLE` (groups are not offered to apps in the first release; message-svc keeps them) | `0` |

`push-auth`:
```

with

```markdown
| `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready`, 1000 to 600000 | `30000` |
| `PUSH_GROUPS` | `1` serves group requests; `0` refuses them with `NOT_AVAILABLE` (groups are not offered to apps in the first release; message-svc keeps them) | `0` |
| `PUSH_CLIENT_IP_HEADER` | header with the client's IP, set by the ingress (`x-real-ip`), for the per-IP limits on redeeming invites and contact requests; same trust assumption as auth-svc's | the peer address |

`push-auth`:
```

```powershell
pwsh deploy/helm/test-chart.ps1
```

Expected: `all chart checks passed` (30 checks).

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock README.md crates deploy
git commit -m "feat(gateway): rate limits on redeeming invites, contact requests, invites, Identify and frames"
```

Expected: 855 tests pass, no warnings, no diff.

---

### Task 10: Timeouts, keepalives, a sweep and batched draining

**Files:**
- Create: `crates/push-gateway/src/drain.rs`
- Modify: `crates/push-gateway/src/hub.rs`, `src/lib.rs`, `src/server.rs`,
  `tests/gateway.rs`

- [ ] **Step 1: The cases, failing**

A client silent for two and a half heartbeats is closed with 1008. In
`crates/push-gateway/tests/gateway.rs` (the stack takes the heartbeat):

Replace

```rust
/// on free ports.
async fn stack() -> Stack {
    stack_with(0).await
}

/// As [`stack`], with message-svc failing its first `sync_failures` syncs.
async fn stack_with(sync_failures: u32) -> Stack {
    let redis = Redis::default()
        .with_tag("7-alpine")
```

with

```rust
/// on free ports.
async fn stack() -> Stack {
    stack_with(0, Duration::from_secs(30)).await
}

/// As [`stack`], with message-svc failing its first `sync_failures` syncs, and the
/// gateway announcing `heartbeat`.
async fn stack_with(sync_failures: u32, heartbeat: Duration) -> Stack {
    let redis = Redis::default()
        .with_tag("7-alpine")
```

Replace

```rust
        listen_addr: gateway,
        health_addr: health.local_addr().unwrap(),
        heartbeat: Duration::from_secs(30),
        groups: false,
        client_ip_header: None,
```

with

```rust
        listen_addr: gateway,
        health_addr: health.local_addr().unwrap(),
        heartbeat,
        groups: false,
        client_ip_header: None,
```

Replace

```rust
}

/// A message over the 128 KiB limit ends the connection with 1009 (message too big).
#[tokio::test]
```

with

```rust
}

/// A client that stops sending frames is gone (or behind a half-open connection): it is
/// closed after two and a half heartbeats without a frame.
#[tokio::test]
async fn a_silent_client_is_closed_after_two_and_a_half_heartbeats() {
    let stack = stack_with(0, Duration::from_secs(1)).await;
    let (user, device) = person(&stack).await;
    let mut socket = ready(&stack, user, device).await;
    let started = std::time::Instant::now();
    send(&mut socket, Body::Ping(Ping {})).await;
    assert_eq!(next(&mut socket).await, Body::Pong(Pong {}));
    assert_eq!(close_code(&mut socket).await, 1008);
    assert!(started.elapsed() >= Duration::from_millis(2500));
}

/// A message over the 128 KiB limit ends the connection with 1009 (message too big).
#[tokio::test]
```

Replace

```rust
#[tokio::test]
async fn a_failed_sync_is_retried() {
    let stack = stack_with(1).await;
    let (_alice, (bob, bob_device), _conversation) = chat_with_backlog(&stack, 1).await;
```

with

```rust
#[tokio::test]
async fn a_failed_sync_is_retried() {
    let stack = stack_with(1, Duration::from_secs(30)).await;
    let (_alice, (bob, bob_device), _conversation) = chat_with_backlog(&stack, 1).await;
```

A Redis that stops answering — `CLIENT PAUSE` keeps the connection and answers nothing, as
behind a half-open TCP connection — is left after one unanswered PING, and everyone is
rung after the reconnect. In `crates/push-gateway/src/hub.rs`, the PING and its test
(decision 12):

Replace

```rust
/// How long to wait before reconnecting to Redis.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Server frames waiting for a busy connection; more are dropped (the client recovers
/// them with `ListConversations` and `ListContacts`).
```

with

```rust
/// How long to wait before reconnecting to Redis.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// How often the subscriber pings Redis: a connection that died without a word (a
/// half-open TCP connection) would otherwise look idle forever.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Server frames waiting for a busy connection; more are dropped (the client recovers
/// them with `ListConversations` and `ListContacts`).
```

Replace

```rust
    /// A hub whose subscriber task listens on Redis through `client` until `shutdown`.
    pub fn start(client: redis::Client, shutdown: CancellationToken) -> (Self, JoinHandle<()>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let hub = Self {
```

with

```rust
    /// A hub whose subscriber task listens on Redis through `client` until `shutdown`.
    pub fn start(client: redis::Client, shutdown: CancellationToken) -> (Self, JoinHandle<()>) {
        Self::start_with(client, shutdown, PING_INTERVAL)
    }

    fn start_with(
        client: redis::Client,
        shutdown: CancellationToken,
        ping_interval: Duration,
    ) -> (Self, JoinHandle<()>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let hub = Self {
```

Replace

```rust
            connected: Arc::default(),
        };
        let subscriber = tokio::spawn(subscribe(client, hub.clone(), receiver, shutdown));
        (hub, subscriber)
    }
```

with

```rust
            connected: Arc::default(),
        };
        let subscriber = tokio::spawn(subscribe(
            client,
            hub.clone(),
            receiver,
            shutdown,
            ping_interval,
        ));
        (hub, subscriber)
    }
```

Replace

```rust
            Ok(result) => Some(result.is_ok()),
            Err(_) => {
                tracing::warn!("Redis did not answer a subscription command in time");
                Some(false)
            }
```

with

```rust
            Ok(result) => Some(result.is_ok()),
            Err(_) => {
                tracing::warn!("Redis did not answer the subscriber in time");
                Some(false)
            }
```

Replace

```rust
/// The subscriber task: keeps one Redis pub/sub connection, follows the hub's subscribe
/// and unsubscribe commands, and dispatches what arrives. Reconnects after a break,
/// subscribes again to every channel with a member and rings every connection.
/// Connecting is retried while Redis is unavailable, and `shutdown` interrupts both the
/// connection attempt and the wait between attempts at once.
```

with

```rust
/// The subscriber task: keeps one Redis pub/sub connection, follows the hub's subscribe
/// and unsubscribe commands, and dispatches what arrives. Pings Redis every
/// `ping_interval` and reconnects when it does not answer within 5 s, as after any break:
/// it subscribes again to every channel with a member and rings every connection.
/// Connecting is retried while Redis is unavailable, and `shutdown` interrupts both the
/// connection attempt and the wait between attempts at once.
```

Replace

```rust
    mut commands: mpsc::UnboundedReceiver<Command>,
    shutdown: CancellationToken,
) {
    loop {
```

with

```rust
    mut commands: mpsc::UnboundedReceiver<Command>,
    shutdown: CancellationToken,
    ping_interval: Duration,
) {
    loop {
```

Replace

```rust
                    tracing::info!("subscribed to Redis");
                    hub.ring_all();
                    loop {
                        tokio::select! {
                            () = shutdown.cancelled() => return,
                            command = commands.recv() => {
                                let succeeded = match command {
```

with

```rust
                    tracing::info!("subscribed to Redis");
                    hub.ring_all();
                    let mut ping_at = tokio::time::Instant::now() + ping_interval;
                    loop {
                        tokio::select! {
                            () = shutdown.cancelled() => return,
                            () = tokio::time::sleep_until(ping_at) => {
                                let ping = async { sink.ping::<redis::Value>().await.map(|_| ()) };
                                let Some(alive) = bounded(&shutdown, ping).await else {
                                    return;
                                };
                                if !alive {
                                    break;
                                }
                                ping_at = tokio::time::Instant::now() + ping_interval;
                            }
                            command = commands.recv() => {
                                let succeeded = match command {
```

Replace

```rust
    }

    #[tokio::test(start_paused = true)]
    async fn ring_all_wakes_every_connection() {
```

with

```rust
    }

    /// A Redis that keeps the connection but stops answering, as behind a half-open TCP
    /// connection, is given up after one unanswered PING; the hub connects again and
    /// rings everyone, since notifications may have been lost meanwhile.
    #[tokio::test]
    async fn a_redis_that_stops_answering_is_left_and_everyone_rung() {
        use testcontainers_modules::redis::Redis;
        use testcontainers_modules::testcontainers::ImageExt;
        use testcontainers_modules::testcontainers::runners::AsyncRunner;

        let container = Redis::default()
            .with_tag("7-alpine")
            .start()
            .await
            .expect("start Redis (is Docker running?)");
        let port = container.get_host_port_ipv4(6379).await.unwrap();
        let client = redis::Client::open(format!("redis://127.0.0.1:{port}")).unwrap();
        let shutdown = CancellationToken::new();
        let (hub, subscriber) =
            Hub::start_with(client.clone(), shutdown.clone(), Duration::from_millis(500));
        for _ in 0..100 {
            if hub.is_connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(hub.is_connected());
        let (mut session, _, _) = connection(&hub, Uuid::now_v7()).await;

        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let () = redis::cmd("CLIENT")
            .arg("PAUSE")
            .arg(8000)
            .arg("ALL")
            .query_async(&mut admin)
            .await
            .unwrap();
        let signal = tokio::time::timeout(Duration::from_secs(20), session.next())
            .await
            .expect("rung after the reconnect");
        assert_eq!(signal, Signal::Ring);
        shutdown.cancel();
        subscriber.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn ring_all_wakes_every_connection() {
```

```powershell
cargo test -p push-gateway --test gateway -- silent
```

Expected: `a_silent_client_is_closed_after_two_and_a_half_heartbeats` fails ("a close
within 10 s"): nothing closes a silent client yet. With the PING interval of the hub test
set to an hour, `a_redis_that_stops_answering_is_left_and_everyone_rung` fails with
`rung after the reconnect: Elapsed(())`.

- [ ] **Step 2: Draining in batches**

Create `crates/push-gateway/src/drain.rs` (decision 13):

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

In `crates/push-gateway/src/lib.rs`:

Replace

```rust
mod config;
mod hub;
mod limits;
```

with

```rust
mod config;
mod drain;
mod hub;
mod limits;
```

- [ ] **Step 3: Timeouts, keepalive and the sweep**

In `crates/push-gateway/src/server.rs`:

Replace

```rust
use crate::config::Config;
use crate::hub::{Hub, Membership, Signal};
use crate::limits::{Action, FrameBudget, Limits, RedisCounter, Refused, Subject};
```

with

```rust
use crate::config::Config;
use crate::drain::Drain;
use crate::hub::{Hub, Membership, Signal};
use crate::limits::{Action, FrameBudget, Limits, RedisCounter, Refused, Subject};
```

Replace

```rust
/// How long a Redis command (other than the subscriber's) may take.
const REDIS_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone)]
```

with

```rust
/// How long a Redis command (other than the subscriber's) may take.
const REDIS_TIMEOUT: Duration = Duration::from_millis(500);
/// How long sending one frame may take; a client that does not read is cut off.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long sending a close frame may take.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Every connection syncs this often even without a ring, so entries arrive while Redis
/// is lost or a notification was (spec §12, test 4).
const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Clone)]
```

Replace

```rust
    limits: Limits,
    sessions: TaskTracker,
    shutdown: CancellationToken,
}
```

with

```rust
    limits: Limits,
    sessions: TaskTracker,
    drain: Arc<Drain>,
    shutdown: CancellationToken,
}
```

Replace

```rust
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let endpoint = Endpoint::from_shared(config.message_svc_url.clone())?
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10));
    let messaging = MessagingClient::new(endpoint.connect_lazy());
    // The error is not passed on: the URL carries the Redis password.
```

with

```rust
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    // HTTP/2 pings find a message-svc connection that died without a word (a half-open
    // TCP connection), so calls do not wait for a peer that is gone.
    let endpoint = Endpoint::from_shared(config.message_svc_url.clone())?
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true)
        .tcp_keepalive(Some(Duration::from_secs(60)));
    let messaging = MessagingClient::new(endpoint.connect_lazy());
    // The error is not passed on: the URL carries the Redis password.
```

Replace

```rust
        redis: publisher,
        sessions: sessions.clone(),
        shutdown: shutdown.clone(),
    };
```

with

```rust
        redis: publisher,
        sessions: sessions.clone(),
        drain: Arc::default(),
        shutdown: shutdown.clone(),
    };
```

Replace

```rust
    .await?;

    // Upgraded connections outlive the HTTP server; each closes itself with 4005.
    sessions.close();
    if tokio::time::timeout(DRAIN_TIMEOUT, sessions.wait())
```

with

```rust
    .await?;

    // Upgraded connections outlive the HTTP server; each closes itself with 4005, a
    // hundred every 50 ms.
    sessions.close();
    if tokio::time::timeout(DRAIN_TIMEOUT, sessions.wait())
```

Replace

```rust
        return;
    }
    loop {
        let retry_at = connected.retry_at;
```

with

```rust
        return;
    }
    // Clients ping every heartbeat; one silent for two and a half is gone.
    let idle = app.config.heartbeat * 5 / 2;
    let mut idle_at = Instant::now() + idle;
    let mut sweep_at = Instant::now() + first_sweep(connection_id);
    loop {
        let retry_at = connected.retry_at;
```

Replace

```rust
        tokio::select! {
            () = app.shutdown.cancelled() => {
                close_with(&mut connected.socket, close::SHUTTING_DOWN, "server shutting down").await;
                return;
            }
            signal = membership.next() => {
                let done = match signal {
```

with

```rust
        tokio::select! {
            () = app.shutdown.cancelled() => {
                tokio::time::sleep(app.drain.next_delay()).await;
                close_with(&mut connected.socket, close::SHUTTING_DOWN, "server shutting down").await;
                return;
            }
            () = tokio::time::sleep_until(idle_at) => {
                close_with(&mut connected.socket, close::POLICY_VIOLATION, "no frame within two and a half heartbeats").await;
                return;
            }
            () = tokio::time::sleep_until(sweep_at) => {
                sweep_at = Instant::now() + SWEEP_INTERVAL;
                if connected.deliver().await.is_err() {
                    return;
                }
            }
            signal = membership.next() => {
                let done = match signal {
```

Replace

```rust
            }
            message = connected.socket.recv() => {
                if matches!(message, Some(Ok(Message::Binary(_) | Message::Text(_))))
                    && !connected.budget.spend()
```

with

```rust
            }
            message = connected.socket.recv() => {
                idle_at = Instant::now() + idle;
                if matches!(message, Some(Ok(Message::Binary(_) | Message::Text(_))))
                    && !connected.budget.spend()
```

Replace

```rust
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(24 * 60 * 60)
```

with

```rust
}

/// When a connection first sweeps: somewhere in the first five minutes, from its id, so
/// connections that came together do not sweep together.
fn first_sweep(connection_id: Uuid) -> Duration {
    let spread = u64::from(connection_id.as_bytes()[15]) * SWEEP_INTERVAL.as_secs() / 256;
    Duration::from_secs(spread.max(1))
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(24 * 60 * 60)
```

Replace

```rust
}

async fn send(socket: &mut WebSocket, body: Body) -> Result<(), Closed> {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .map_err(|_| Closed)
}
```

with

```rust
}

/// Sends one frame. A client that does not take it within 10 s is cut off.
async fn send(socket: &mut WebSocket, body: Body) -> Result<(), Closed> {
    let message = Message::Binary(encode_frame(body).into());
    match tokio::time::timeout(WRITE_TIMEOUT, socket.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(Closed),
    }
}
```

Replace

```rust
        reason: reason.into(),
    };
    let _ = socket.send(Message::Close(Some(frame))).await;
}
```

with

```rust
        reason: reason.into(),
    };
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.send(Message::Close(Some(frame)))).await;
}
```

```powershell
cargo test -p push-gateway
```

Expected: everything passes (the hub's PING case takes about 9 s).

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "feat(gateway): write and idle timeouts, Redis and HTTP/2 keepalives, a 5-minute sweep and batched draining"
```

Expected: 858 tests pass, no warnings, no diff.

---

### Task 11: Spec, README, roadmap and pull request

**Files:**
- Modify: `docs/superpowers/specs/2026-10-11-push-messenger-design.md`, `README.md`,
  `docs/superpowers/plans/2026-10-11-roadmap.md`

CI needs no change: the `rust` and `msrv` jobs run the whole workspace and already pull
`postgres:17-alpine` and `redis:7-alpine`; the `chart` job runs `test-chart.ps1`, now with
30 checks. minikube comes with Plan 8B.

- [ ] **Step 1: Record the implementation in the spec**

In `docs/superpowers/specs/2026-10-11-push-messenger-design.md` (§5.3, §5.5, §5.6 — the
reset stamp, the 4006 close and contact requests that reveal nothing; §7.2 — receipts out of
order; §7.3 — the gateway's implementation notes; §7.4 — the close codes; §7.5 — the
gateway's timeouts; §7.6 — the frames in `Frame`, the size limit, the DEV ONLY identity
gone; §13 — the group switch):

Replace

```markdown
   - A device message-svc has **not heard of yet** is not rejected for good: the worker
     relays `device.registered` about a second after the registration, and until then
     `Identify` answers `UNAVAILABLE` (retryable), unlike a revoked device or one of
     another user (`PERMISSION_DENIED`, `UNKNOWN_DEVICE`). The gateway (Plan 8) must
     pass this on as "try again shortly", and the SDK must reconnect after a short
     backoff — never treat it as a revocation or sign the user out.

Implementation notes (Plans 6A and 7A): message-svc's internal `Accounts` gRPC API takes the
```

with

```markdown
   - A device message-svc has **not heard of yet** is not rejected for good: the worker
     relays `device.registered` about a second after the registration, and until then
     `Identify` answers `UNAVAILABLE` (`DEVICE_NOT_READY`, retryable), unlike a revoked
     device or one of another user (`PERMISSION_DENIED`, `UNKNOWN_DEVICE`). The gateway
     closes the socket with 4006 ("try again shortly"), and the SDK must reconnect after
     a short backoff — never treat it as a revocation or sign the user out. Since the
     device id comes from the verified token, the difference tells a client only about
     its own device: no oracle for other ids.

Implementation notes (Plans 6A and 7A): message-svc's internal `Accounts` gRPC API takes the
```

Replace

```markdown
message-svc has none, the call is `UNAUTHENTICATED`. A revoked device is refused by
`Identify` at once (`PERMISSION_DENIED`); a device not relayed yet is `UNAVAILABLE`
(`retryable`), which the gateway closes with 1011 until Plan 8 gives it a code of its own.
After committing a revocation message-svc publishes `Kick{reason: DEVICE_REVOKED}` on
`kick:{device_id}`, every time it is relayed; after a relayed password reset it publishes
`Kick{reason: PASSWORD_RESET}` for every active device of the user, which stay registered.
The gateway closes kicked sockets with Plan 8: 4003 for a revocation, 4001 ("sign in
again") for a password reset (owner decision), and refuses tokens older than a reset.

### 5.4 Password reset
```

with

```markdown
message-svc has none, the call is `UNAUTHENTICATED`. A revoked device is refused by
`Identify` at once (`PERMISSION_DENIED`); a device not relayed yet is `UNAVAILABLE`
(`DEVICE_NOT_READY`, retryable), which the gateway closes with 4006 (Plan 8).
After committing a revocation message-svc publishes `Kick{reason: DEVICE_REVOKED}` on
`kick:{device_id}`, every time it is relayed; after a relayed password reset it publishes
`Kick{reason: PASSWORD_RESET}` for every active device of the user, which stay registered.
The gateway closes kicked sockets: 4003 for a revocation, 4001 ("sign in again") for a
password reset (owner decision). A relayed reset also records `password_reset_at` (the
time message-svc heard of it, on `known_user`; a repeat moves it forward), and
`Identify` refuses (`UNAUTHENTICATED`, `PASSWORD_CHANGED`) any access token whose `iat` is
no later than that second (Plan 8): a token that outlives the reset (up to 15 minutes) or
a kick lost on Redis cannot keep or bring back a session. The gateway passes the token's
`iat` with every `Identify`, also on `ReAuth`.

### 5.4 Password reset
```

Replace

```markdown
  contact" operation in v1, so an accepted contact can end only through a block; a
  remove/unfriend operation is a candidate for a later version.
- **Invite link / QR code.** `CreateInvite{expires_in, max_uses}` returns a short code,
  shown as a link or QR code. `RedeemInvite{code}` makes both users accepted contacts
  immediately: sharing the link is the consent. Codes expire (1 minute to 30 days), have a
  use limit (1 to 100) and can be revoked. A code is 10 characters of Crockford base32
  (50 random bits, enough because redeeming is rate limited), read case-insensitively. An
  unknown, revoked, expired or used-up code, and a code whose creator has blocked the
  redeemer, all give the same "invite not found", so redeeming reveals nothing. Users who
```

with

```markdown
  contact" operation in v1, so an accepted contact can end only through a block; a
  remove/unfriend operation is a candidate for a later version.
  No answer reveals which ids exist (Plan 8): a request to an id that is not a verified
  user is answered `REQUESTED_OUT` like one nobody has answered yet, and stores and
  announces nothing; `Accept` and `Decline` answer it `NO_PENDING_REQUEST`, `Block` and
  `Unblock` succeed and store nothing. The requester's `ListContacts` shows no such
  request (there is no row to show); user ids carry 74 random bits (UUIDv7) and requests
  are limited to 30 an hour per user, so that cannot be used to search for ids.
- **Invite link / QR code.** `CreateInvite{expires_in, max_uses}` returns a short code,
  shown as a link or QR code. `RedeemInvite{code}` makes both users accepted contacts
  immediately: sharing the link is the consent. Codes expire (1 minute to 30 days), have a
  use limit (1 to 100) and can be revoked. A code is 10 characters of Crockford base32
  (50 random bits, enough because redeeming is rate limited: 20 an hour per user and 60
  per IP, §7.3), read case-insensitively. An
  unknown, revoked, expired or used-up code, and a code whose creator has blocked the
  redeemer, all give the same "invite not found", so redeeming reveals nothing. Users who
```

Replace

```markdown
  access token issued before the reset keeps working for up to 15 minutes; later, a
  `password_changed_at` stamp should make `authenticate` and the gateway reject older
  tokens.

```

with

```markdown
  access token issued before the reset keeps working for up to 15 minutes; later, a
  `password_changed_at` stamp should make `authenticate` and the gateway reject older
  tokens. Plan 8 does it for the gateway (message-svc's `password_reset_at`, §5.3);
  auth-svc's own API (`/v1/devices`, `/v1/users`) still accepts such a token until it
  expires.

```

Replace

```markdown
| Response | `request_id` and the request-specific result |
| AuthExpiring | `expires_in_s` |
| Error | `code, message, retryable`, plus `client_msg_id` or `request_id` |
| Pong | — |

### 7.3 Sessions
```

with

```markdown
| Response | `request_id` and the request-specific result |
| AuthExpiring | `expires_in_s` |
| Error | `code, message, retryable`, plus `client_msg_id` or `request_id`, and `retry_after_s` when rate limited |
| Pong | — |

Receipts and read positions are signals published after each commit. Two acks committed
at the same moment (transactions run at READ COMMITTED) can publish their receipts in
either order, so a client keeps the highest `delivered_seq`, `read_seq`,
`all_delivered_seq` and `all_read_seq` it has seen per conversation and ignores a lower
one; `ListConversations` gives the current values.

### 7.3 Sessions
```

Replace

```markdown
  SDK ignores a `Deliver` it already has, keyed by `(conversation_id, seq)`.

### 7.4 Close codes

| Code | Meaning | Client action |
|---|---|---|
| 4001 | Invalid token, unverified email, or unknown/revoked device | stop; sign in again |
| 4002 | Re-authentication required | refresh, then reconnect |
| 4003 | Another connection took over, or device revoked | stop |
| 4004 | Rate limited | back off, then reconnect |
| 4005 | Server shutting down | back off with jitter, then reconnect |

### 7.5 Timeouts
```

with

```markdown
  SDK ignores a `Deliver` it already has, keyed by `(conversation_id, seq)`.

Implementation notes (gateway, Plan 8):

- **Checking the token** happens in the gateway, without calling auth-svc: an EdDSA JWT
  whose header names a key (`kid`) from auth-svc's JWKS (`GET /v1/auth/jwks`, fetched at
  start-up, every 5 minutes, and at once when a token names an unknown `kid`, at most every
  30 s, so a key rotation needs no restart); the algorithm is pinned to EdDSA; `exp`,
  `sub` and `aud` are required, `aud` must be the gateway's public URL, `device_id` and
  `iat` must be present and `email_verified` true; `exp` has 60 s of leeway. `/readyz`
  waits for the keys. An invalid token closes with 4001, an expired one with 4002; when
  the keys cannot be fetched the close is 1011, never a sign-out.
- **Identify**, in order: the token is checked; `Identify` is limited to 30 a minute per
  device (in Redis, failing open: devices keep connecting while Redis is down); the
  connection subscribes to `kick:{device_id}` and waits up to 2 s for the subscription,
  **before** message-svc is asked, so a revocation or reset committed after message-svc's
  answer still reaches it; message-svc's `Identify` checks the device and the token's
  `iat` against the last password reset (§5.3); then `notify:{user_id}`, the takeover
  (`Kick{TAKEN_OVER, connection_id}` on `kick:{device_id}`, which closes every older
  connection of the device on any gateway with 4003), `Ready` and a sync.
- **ReAuth** carries a fresh token for the same user and device; the gateway checks it and
  asks message-svc again. Nothing answers a good one. `AuthExpiring` comes 60 s before
  `exp`; without a good `ReAuth` the connection closes with 4002 at `exp + 30 s`. A token
  of another device closes with 4001; when the keys or message-svc cannot be reached a
  retryable `Error` asks for the `ReAuth` again. A kick lost on Redis is thus caught
  within one token lifetime.
- **Kicks** close the socket: `DEVICE_REVOKED` and `TAKEN_OVER` with 4003,
  `PASSWORD_RESET` with 4001.
- **Server frames.** `EntriesAvailable` makes every connection of the user sync; receipt,
  read-position and contact notifications become `Receipt`, `ReadPosition` and
  `ContactEvent` frames for every connection of the user (at most 64 waiting per
  connection; the client recovers dropped ones by listing). Every connection also syncs
  every 5 minutes (its first sweep spread over 5 minutes by its id), so entries arrive
  while Redis is lost. Delivery positions move only on `Ack`; whether entries are waiting
  is always asked of `Sync` (`entries_after`, limit + 1), never decided by comparing
  positions, since edit and delete entries a member may not see can end a log and park
  the position before them.
- **Groups are not offered to apps in the first release** (owner decision): unless
  `PUSH_GROUPS=1` (chart `gateway.groups`), `CreateGroup`, `AddMembers`, `RemoveMember`,
  `LeaveGroup`, `RenameGroup` and `SetRole` are answered `NOT_AVAILABLE` with their
  `request_id`; message-svc keeps full group support. No group can be created through the
  gateway, so no group entries, receipts or listings reach clients either.
- **Rate limits** (fixed windows in Redis across every gateway, keyed by a SHA-256 of the
  subject; the client IP from `X-Real-IP` as in auth-svc): `RedeemInvite` 60 an hour per
  IP and 20 per user, `ContactRequest` 120 per IP and 30 per user, `CreateInvite` 20 per
  user. Over a limit a request gets `RATE_LIMITED` with `retry_after_s`; when Redis does
  not answer within 500 ms it gets a retryable `UNAVAILABLE` (fail closed). Each
  connection may send 100 frames at once and 20 a second after that; beyond, it is
  closed with 4004.
- **Liveness.** Sending a frame may take 10 s; a client silent for two and a half
  heartbeats (75 s) is closed with 1008. The hub pings Redis every 30 s and reconnects
  when it gets no answer within 5 s, as after any break (subscribing again and ringing
  every connection); the gRPC channel to message-svc sends HTTP/2 pings every 30 s
  (10 s timeout) and uses TCP keepalive.
- **Shutdown**: `/readyz` turns unready, the listener stops, and open connections close
  with 4005, a hundred every 50 ms (at most 5 s), so clients do not all reconnect at once;
  the gateway waits up to 10 s for them.

### 7.4 Close codes

| Code | Meaning | Client action |
|---|---|---|
| 4001 | Invalid token (signature, audience, unverified email), or the password was reset after it was issued | stop; sign in again |
| 4002 | The token expired, or no `ReAuth` came by `exp + 30 s` | refresh, then reconnect |
| 4003 | Another connection of the device took over, or the device was revoked | stop |
| 4004 | Rate limited (too many frames or `Identify`s) | back off, then reconnect |
| 4005 | Server shutting down | back off with jitter, then reconnect |
| 4006 | The device was registered moments ago and is not ready yet | reconnect after a short backoff; never sign out |
| 1002 | A protocol mistake before `Ready`: no `Identify` within 5 s, or another frame first | back off, then reconnect |
| 1008 | No frame for two and a half heartbeats | reconnect |
| 1009 | A message over 128 KiB | fix the client; reconnect |
| 1011 | The server cannot serve the connection now (message-svc or the token keys unreachable) | back off, then reconnect |

Only 4001 signs the user out, and only 4003 stops reconnecting (owner decisions: a
revoked device stops, a password reset signs in again). A protocol mistake before `Ready`
is 1002, not 4001: a client bug is no reason to sign out.

### 7.5 Timeouts
```

Replace

```markdown
| nginx `proxy_read_timeout` / `proxy_send_timeout` | 60 s | 120 s |
| Client ping interval | — | 30 s |

### 7.6 Encoding and the internal API
```

with

```markdown
| nginx `proxy_read_timeout` / `proxy_send_timeout` | 60 s | 120 s |
| Client ping interval | — | 30 s |
| Gateway: first frame (`Identify`) | — | 5 s |
| Gateway: silent client closed (two and a half heartbeats) | — | 75 s |
| Gateway: sending one frame | — | 10 s |
| Gateway to Redis: subscriber PING, answer within | — | 30 s, 5 s |
| Gateway to message-svc: HTTP/2 PING, answer within | — | 30 s, 10 s |

### 7.6 Encoding and the internal API
```

Replace

```markdown
  of the 18 request bodies of §7.1) and `Response` are defined in `frame.proto` (Plan 7A);
  message-svc's `Messaging` service serves them as `Edit`, `Delete`, `ReadUpTo` and
  `Request`, whose `Response` and `Error` carry the request's `request_id`. They join
  `Frame` with Plan 8.
- message-svc publishes a small `Notification` (entries available, receipt, contact
  change, read position; never a body) on `notify:{user_id}` after every commit. The
```

with

```markdown
  of the 18 request bodies of §7.1) and `Response` are defined in `frame.proto` (Plan 7A);
  message-svc's `Messaging` service serves them as `Edit`, `Delete`, `ReadUpTo` and
  `Request`, whose `Response` and `Error` carry the request's `request_id`. Plan 8 puts
  them in `Frame` (client frames 5–9: `ReAuth`, `Edit`, `Delete`, `ReadUpTo`, `Request`;
  server frames 37–41: `Response`, `Receipt`, `ReadPosition`, `ContactEvent`,
  `AuthExpiring`); the gateway forwards them as they are and adds the `client_msg_id` of
  an `Edit` or `Delete` to its error, as for `Send`.
- A WebSocket message is at most 128 KiB (`MAX_FRAME_BYTES`); a bigger one closes the
  connection with 1009. WebSocket frames may be up to 512 KiB, so a message just over the
  limit is read whole and the close frame reaches the client; a bigger frame is refused
  from its header and the client may only see the connection drop.
- message-svc publishes a small `Notification` (entries available, receipt, contact
  change, read position; never a body) on `notify:{user_id}` after every commit. The
```

Replace

```markdown
  does not send an entry twice on one connection, so repeated syncs without acks are
  harmless.
- **DEV ONLY identity, until real tokens (Plan 8).** A gateway started with
  `PUSH_DEV_AUTH=1` accepts `Identify{dev_user_id, dev_device_id}` without a token;
  otherwise `Identify` closes with 4001. The variable accepts only `1` or `0`, and the
  gateway logs a warning at start-up when it is enabled. When the dev fields are removed
  (Plan 8), their field numbers (14, 15) and names are reserved. message-svc's DEV ONLY
  `DevSetup` service and `PUSH_DEV_SETUP` were removed in Plan 7B: users and devices come
  from the worker's `Accounts` calls, contacts and conversations from the request frames.
- **Start-up and runtime behaviour (message-svc).** It fails at start-up, without
  retrying, on an invalid database or Redis URL and on `PUSH_DB_MAX_CONNECTIONS=0`; error
```

with

```markdown
  does not send an entry twice on one connection, so repeated syncs without acks are
  harmless.
- **No DEV ONLY identity.** Until Plan 8 a gateway started with `PUSH_DEV_AUTH=1`
  accepted `Identify{dev_user_id, dev_device_id}` without a token; Plan 8 removed it, and
  `frame.proto` reserves the field numbers 14 and 15 and their names. A gateway given
  `PUSH_DEV_AUTH=1` refuses to start. message-svc's DEV ONLY `DevSetup` service and
  `PUSH_DEV_SETUP` were removed in Plan 7B: users and devices come from the worker's
  `Accounts` calls, contacts and conversations from the request frames.
- **Start-up and runtime behaviour (message-svc).** It fails at start-up, without
  retrying, on an invalid database or Redis URL and on `PUSH_DB_MAX_CONNECTIONS=0`; error
```

Replace

```markdown
- Calibrate Argon2id on the chosen VM size.
- Groups: message-svc serves them in full from Plan 7A, but they are not exposed to apps in
  the first release (owner decision): the Plan 8 gateway hides group frames.
```

with

```markdown
- Calibrate Argon2id on the chosen VM size.
- Groups: message-svc serves them in full from Plan 7A, but they are not exposed to apps in
  the first release (owner decision): the gateway refuses the group requests with
  `NOT_AVAILABLE` unless `PUSH_GROUPS=1` (chart `gateway.groups`), one switch to turn
  them on later (Plan 8).
```

- [ ] **Step 2: README**

In `README.md`, the paragraph on dev mode:

Replace

```markdown
verified this way before it is merged.

The minikube values switch on DEV ONLY shortcuts: the gateway accepts any user and device
id without a token (until it checks access tokens, Plan 8), and the worker, which logs
emails instead of sending them, shows them at `/dev/mailbox`; neither is routed by the
ingress, which
reaches the worker only at `/v1/webhooks/resend`. The chart refuses that mode unless the
values also say `devMode.acknowledge: insecure-local-only` and the ingress host is local
(`*.local`, `*.localhost` or `*.test`), and refuses it with the Resend sender.
```

with

```markdown
verified this way before it is merged.

The minikube values switch on a DEV ONLY shortcut: the worker, which logs emails instead
of sending them, shows them at `/dev/mailbox`, which the ingress does not route (it
reaches the worker only at `/v1/webhooks/resend`). Clients, the smoke test included,
always identify with real access tokens. The chart refuses that mode unless the
values also say `devMode.acknowledge: insecure-local-only` and the ingress host is local
(`*.local`, `*.localhost` or `*.test`), and refuses it with the Resend sender.
```

- [ ] **Step 3: Mark plan 8A in review**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the empty status cell at the end
of the row for plan 8A (`| 8A | Gateway: the service | … | 7B | |`) with `In review`.

- [ ] **Step 4: Final gates**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 test --workspace --locked
pwsh deploy/helm/test-chart.ps1
```

Expected: no diff, no warnings, 858 tests on both toolchains, `all chart checks passed`.

- [ ] **Step 5: Commit and push**

```powershell
git add README.md docs
git commit -m "docs: record plan 8A in the spec and roadmap"
git push -u origin feature/plan-08a-gateway-service
```

- [ ] **Step 6: Open the pull request and wait for CI**

```powershell
$body = @'
Implements docs/superpowers/plans/2026-10-14-plan-08a-gateway-service.md: the complete gateway. Access tokens are checked locally against auth-svc's JWKS (EdDSA pinned, kid for key rotation, exp required with 60 s leeway, aud) and tokens issued before the last password reset are refused (message-svc records relayed resets; Identify compares the token's iat). Every client frame reaches message-svc and receipts, read positions, contact events and AuthExpiring reach clients. Identify watches kick:{device} before message-svc answers; ReAuth renews tokens; takeovers, kicks (4003 revoked, 4001 password reset), 4006 for a device not relayed yet and 1002 for protocol mistakes before Ready. Group requests are refused behind PUSH_GROUPS (groups are not offered to apps in the first release). Redis rate limits on RedeemInvite, ContactRequest and CreateInvite (fail closed), Identify per device and a frame budget per connection; 1009 over 128 KiB; write and idle timeouts, Redis PING and HTTP/2 keepalive, a 5-minute sweep, batched draining. Contact requests no longer reveal which ids exist. The DEV ONLY identity is removed. 858 tests.

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
| §5.1 access-token claims checked: `sub`, `device_id`, `email_verified`, `exp`, `iat`, `aud` | 4 |
| §5.3 the device only from the verified token; not relayed yet is retryable, never a sign-out | 3, 5 |
| §5.3, §5.4 revoked devices closed at once (4003); a password reset kicks (4001) and older tokens are refused | 3, 7 |
| §5.5 contact requests; invites rate limited; no answer reveals which ids exist | 2, 9 |
| §7.1 every client frame; `Identify` within 5 s; request frames with `request_id` | 1, 5, 8 |
| §7.2 every server frame: `Ready`, `Deliver`, `SendAck`, `Receipt`, `ReadPosition`, `ContactEvent`, `Response`, `AuthExpiring`, `Error`, `Pong` | 1, 5, 6, 7, 8 |
| §7.3 Identify rate limited per device, subscriptions, takeover with 4003, `Ready` then sync; `AuthExpiring`, `ReAuth`, 4002 | 5, 6, 7, 9 |
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
- **The SDK** (Plan 9): keep the highest receipt positions, reconnect without signing out
  on everything but 4001 and stop only on 4003, refresh on `AuthExpiring` and send
  `ReAuth`, validate frame sizes before queuing.
- **TLS for the JWKS fetch and the internal APIs, a NetworkPolicy, the 10,000-connection
  load test and tuning the drain batches** (Plan 11); also owner question 3.
- **jsonwebtoken 10** once it offers the ring backend again; 9.3 is safe here because
  `exp`, `sub` and `aud` are required.
- **gRPC load balancing across message-svc replicas** (from Plan 4): the gateway holds one
  HTTP/2 connection.
