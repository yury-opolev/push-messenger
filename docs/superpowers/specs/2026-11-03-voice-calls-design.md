# Voice calls (1:1 audio, agent-ready) — design

Change request "voice calls" (consumer yuopolev/push-messenger, against upstream 32af9dd).
The owner is away: every open question was decided autonomously and is recorded in
[Decisions](#1-decisions). Delivered as eight independently releasable PRs
([plan](../plans/2026-11-03-voice-calls.md)); this document is the contract every PR
follows. Names and numbers below are final unless a later PR records a deviation here.

## Problem

push-messenger has text, rich cards, activity and presence, but no way to talk. Phase 1:
1:1 audio calls between accepted contacts from the web app and other clients. Phase 2: the
same calls between a person and their agent (Coda through push-agent-bridge): the agent
answers its owner, and calls its owner itself (for example for an approval). The server
carries signalling only and never sees media.

## Goals and non-goals

Goals: WebRTC peer-to-peer audio (Opus, DTLS-SRTP end to end), signalling on the existing
WebSocket, ringing on every capable device with first-accept-wins, call state shared by
every replica through Redis, call history separate from chats, short-lived TURN
credentials (internal coturn, external provider, or STUN only), SDK support, a browser
recipe, and in phase 2 a native media helper and the bridge's voice pipeline. Off by
default (`calls.enabled: false`).

Non-goals (the wire leaves room for all of them, see §4.8): video, group calls, ringing
through push notifications (APNs/FCM), recording, server-side media of any kind, calls
written into conversation history ("missed call" messages).

## 1. Decisions

| # | Topic | Decision |
|---|---|---|
| D1 | Who runs the call engine | **message-svc**. The gateway stays a relay, as for every other frame: it forwards call frames over a new `Messaging.Call` RPC and tells message-svc when a calls-capable connection attaches or detaches. message-svc owns the policy (contacts, blocks, agent ownership in PostgreSQL), the state machine, Redis call state, timers, audit and metrics. Rejected: the engine in the gateway (it would need a policy RPC anyway, and phase-1 history writes rows message-svc owns). |
| D2 | State machine | Pure Rust in `push-domain` (`push_domain::calls`): `transition(record, event, now) -> Outcome`. No I/O, table-driven tests. Redis is storage with compare-and-set; Lua does atomic CAS and the busy locks, never policy. |
| D3 | Redis layout | Every call key carries the hash tag `{calls}` so a call's record, both users' busy locks and the due set share one cluster slot (atomic scripts stay cluster-safe). Phase-1 volume (≤ thousands of live calls) fits one shard; documented as a scaling limit. |
| D4 | Clock | Redis `TIME` is the only clock for deadlines (load and claim scripts return it). Clients get relative `expires_in_ms` as well as the absolute `expires_at`, so a skewed client clock never shortens or lengthens ringing. |
| D5 | Timers | A sweeper in every message-svc replica claims due calls from the `calls:{calls}:due` sorted set every 500 ms; a claim pushes the entry 5 s ahead (a lease), so a replica that dies mid-claim is covered by another. Each live call has exactly one due entry. A claim takes at most 32; claims repeat at once while they come back full (up to 50 per wake), and the claimed calls are ticked 16 at a time (a tick is about 5 Redis round trips), so one replica ticks thousands of calls per second — far beyond the ~70 ticks/s of 1,000 active calls at the 15 s liveness interval. |
| D6 | Device reachability | A connection is *calls-active* when the gateway has calls enabled, message-svc offers calls (its `IdentifyResponse.capabilities`), and the client advertised `CAPABILITY_CALLS` in `Identify`. Only calls-active connections are attached (registry `calls:{calls}:devices:<user>`: device → connection id). Liveness of an attached device = its connection still holds a presence lease (`presence:{<user>}:conns`, maintained by the gateway already). No extra refresh traffic. |
| D7 | Resume | After `Ready`, a calls-active connection is attached and the server sends `CallSync`: the calls the user is in, as this device sees them (a ringing incoming call includes its offer). Covers reconnects within the grace, gateway restarts, lost `CallIncoming`/`CallEnded`. A client drops any local call that `CallSync` does not list. |
| D8 | Pre-answer ICE | The caller trickles ICE while ringing; message-svc buffers up to 32 such candidates in the call record and relays them to the answering device right after the accept, in order. Ringing devices never receive the caller's candidates. |
| D9 | Endings fan-out | `CallEnded` goes to every involved device: the caller's device, and the callee's ringing devices (while ringing) or answering device (once active), including the device whose frame ended the call (a confirmation). |
| D10 | Strictness | Every frame's `call_id` must belong to the sender's device in its current role, otherwise `CALL_NOT_FOUND` (same answer for unknown, ended, someone else's, wrong role: no oracle). `CallCancel` after an accept is `ALREADY_ANSWERED` (the client then hangs up). `CallHangup` while ringing is accepted: from the caller it ends the call as `cancelled`, from a ringing callee device as `declined`. |
| D11 | "NOT_A_CONTACT" | The request's `NOT_A_CONTACT` is the existing `ERROR_CODE_NOT_CONTACT` (6). Unknown callee, non-contact, pending request, a block either way, an agent rule: all `NOT_CONTACT`, indistinguishable. |
| D12 | Owner ↔ agent | The pair is callable whenever neither side blocked the other, even if a contact row was removed ("always contacts"). An agent may call only its owner; only its owner may call it. Needs `Tx::agent_owner(user)`. Phase 2 needs no further server change. |
| D13 | Attempts per hour | Counted by the gateway's existing fixed-window limiter (`Action::CallInvite`, per user, `calls.attemptsPerHour`, default 30), on every `CallInvite` before it reaches message-svc, refused or not. Fails closed (Redis down = calls down anyway). |
| D14 | SDP | Parsed only to enforce size (≤ 16 KiB), line structure and audio-only media: at least one `m=audio`, any other `m=` kind (video, application, text, message, …) is `INVALID` ("video is not available" for video). Never stored beyond the call, never logged. |
| D15 | Limits | ≤ 200 `CallIce` frames per side per call (`CALL_LIMIT` after), ≤ 32 of them buffered before the answer, ≤ 20 `CallRenegotiate` per side per call; candidate ≤ 512 bytes printable ASCII, `sdp_mid` ≤ 32 bytes, `username_fragment` ≤ 256 bytes. |
| D16 | Blocks during a call | A block does not end a call already ringing or active (either side can end it); it prevents the next one. |
| D17 | Metrics | No metrics stack exists. message-svc serves Prometheus text on its health listener `GET /metrics`, hand-rolled (atomics, no new dependency). PR 3 adds the TURN counters there; coturn exposes its own exporter. |
| D18 | Audit | `tracing` lines with `target: "security"` (the repo's audit convention): `event = "call.started"` and `event = "call.ended"`. Never SDP, candidates, IPs or device addresses. |
| D19 | Record encoding | The call record is JSON (`serde`) with `"v": 1`; unknown fields are ignored and new fields default, so mixed message-svc versions read each other's records during a rollout. |
| D20 | History & TURN APIs | HTTP on auth-svc (it serves the public `/v1/…` API and verifies access tokens), backed by a new internal `Calls` gRPC service on message-svc's directory listener (the Templates pattern). The auth Ingress gains the `/v1/calls` prefix. |
| D21 | Media helper | **str0m** (sans-IO WebRTC, MIT/Apache, small dependency tree, tokio-agnostic) over webrtc-rs (large async tree, being rewritten upstream). Opus through `audiopus` (libopus, BSD-3). Behind the `calls-media` feature of push-sdk, off by default (see §10.1). |
| D22 | Transcripts in chat | The bridge cannot send as the owner, so the owner's utterances go into the chat as a new built-in card `agent.transcript@1` (`speaker: owner`, `voice: true`, text), rendered "🎤 voice"; the agent's spoken replies stay `agent.reply@2`. |

## 2. Architecture

```
client ──CallInvite/…──▶ gateway ──Messaging.Call──▶ message-svc ──policy (PostgreSQL)
                                                        │  load/CAS (Lua) on calls:{calls}:*  ──▶ Redis
                                                        └─ PUBLISH notify:{user} Notification.call ──▶ gateways ──▶ target devices
gateway (Ready, calls-active)  ──Messaging.CallAttach──▶ message-svc ──▶ CallSync to the device
gateway (connection closed)    ──Messaging.CallDetach──▶ message-svc (grace starts)
message-svc sweeper (every 500 ms) ──claim due──▶ Redis ──▶ ring timeouts, grace ends, liveness ticks
```

- Publishing completes before `Messaging.Call` answers, so two frames of one device reach
  the peer in order (Redis keeps per-channel order; the gateway handles one client frame at
  a time).
- Call frames for a device travel on the user's `notify:{user}` channel as
  `Notification.call` (`CallSignal`) naming the target devices; the hub hands each frame
  only to its connections of those devices. Old gateways decode an unknown notification
  kind as empty and drop it.

## 3. State machine

### 3.1 Record

`CallRecord` (JSON in Redis, D19):

| Field | Meaning |
|---|---|
| `v` | 1 |
| `call_id`, `caller`, `caller_device`, `callee` | fixed at the invite |
| `media` | `["audio"]` |
| `state` | `ringing` or `active` (an ended call has no record) |
| `offer` | the offer SDP (kept while ringing for `CallSync`; dropped once active) |
| `ringing_devices` | callee devices sent `CallIncoming` (only these may accept or decline) |
| `callee_device` | the answering device, once active |
| `created_ms`, `ring_deadline_ms`, `answered_ms` | Redis-clock milliseconds |
| `caller_away_ms`, `callee_away_ms` | when the party device's connection was found gone; absent while connected |
| `ice` | per side: frames relayed so far (`caller`, `callee`) |
| `renegotiations` | per side |
| `pending_ice` | caller candidates buffered before the answer (≤ 32) |
| `version` | CAS counter, kept in the hash beside the record |

### 3.2 Events

| Event | Source |
|---|---|
| `Invite` | `CallInvite` from the caller's device |
| `Accept(device, answer)` | `CallAccept` |
| `Decline(device, reason)` | `CallDecline`, or `CallHangup` from a ringing callee device |
| `Cancel` | `CallCancel`, or `CallHangup` from the caller while ringing |
| `Hangup(device, reason)` | `CallHangup` while active |
| `Ice(device, candidate)` | `CallIce` |
| `Renegotiate(device, sdp, type)` | `CallRenegotiate` |
| `Attach(device)` | `Messaging.CallAttach` for a device of a participant |
| `Detach(device)` | `Messaging.CallDetach` with the device's registered connection |
| `Tick` | the sweeper: a due entry fired (ring deadline, grace end, or liveness check) |

### 3.3 Transitions

`now` is Redis time. `ring` = `calls.ringSeconds` (45 s), `grace` =
`calls.disconnectGraceSeconds` (30 s), `LIVENESS` = 15 s. "Gone" = the party device has no
registry entry, or its connection holds no live presence lease.

| From | Event / condition | To | Effects |
|---|---|---|---|
| — | `Invite`, caller already in a call | — | `CallError{IN_CALL}` |
| — | `Invite`, callee busy (in or ringing for a call) | — | reply `CallEnded{busy}`; audit `call.ended` busy |
| — | `Invite`, callee has no attached live device | — | reply `CallEnded{unreachable}`; audit `call.ended` unreachable |
| ringing | `Invite` again with the same `call_id` from the same caller device (a retry) | ringing | ok, no reply and no frames; another sender: `CallError{INVALID}` |
| — | `Invite`, ok | ringing | `CallIncoming` to every live attached callee device; due = `now + ring`; audit `call.started` |
| ringing | `Accept` from a ringing device, `now < ring_deadline` | active | `CallAccepted` to the caller device; `CallTaken` to the other ringing devices; buffered caller ICE to the answering device; offer dropped; due = `now + LIVENESS` |
| ringing | `Accept`, `now ≥ ring_deadline` | — | `CallEnded{missed}` to all involved (the late accepter included) |
| active | `Accept` from the answering device (a retry) | active | `CallAccepted` to the caller device again; no write |
| active | `Accept` from another callee device | active | `CallError{ALREADY_ANSWERED}` to the sender only |
| ringing | `Decline(declined)` from a ringing device | — | `CallEnded{declined}` to all involved |
| ringing | `Decline(busy)` from a ringing device | — | `CallEnded{busy}` to all involved |
| ringing | `Cancel` from the caller device | — | `CallEnded{cancelled}` to all involved |
| active | `Cancel` from the caller device | active | `CallError{ALREADY_ANSWERED}` |
| active | `Hangup(completed\|failed)` from a party device | — | `CallEnded{completed\|failed, duration_s}` to both party devices |
| ringing | `Ice` from the caller device, < 32 buffered, < 200 sent | ringing | buffered |
| ringing | `Ice` from the caller device, buffer full | ringing | `CallError{CALL_LIMIT}` |
| active | `Ice` from a party device, < 200 sent | active | `peer_call_ice` to the other party device |
| any | `Ice`, 200 reached for that side | same | `CallError{CALL_LIMIT}` |
| active | `Renegotiate` from a party device, < 20 | active | `peer_call_renegotiate` to the other party device |
| active | `Renegotiate`, 20 reached | active | `CallError{CALL_LIMIT}` |
| ringing | `Renegotiate` | ringing | `CallError{CALL_NOT_FOUND}` (not in that role yet) |
| ringing | `Attach` of a callee device not ringing | ringing | added to `ringing_devices`; it gets the call in its `CallSync` |
| ringing | `Attach` of the caller device (away) | ringing | `caller_away_ms` cleared; due = ring deadline |
| active | `Attach` of a party device (away) | active | that side's away cleared; due = `now + LIVENESS` (or the other side's grace end) |
| ringing | `Detach` of a ringing device | ringing | removed from `ringing_devices` (ringing continues until the deadline) |
| ringing | `Detach` of the caller device | ringing | `caller_away_ms = now`; due = min(ring deadline, `now + grace`) |
| active | `Detach` of a party device | active | that side's away = `now`; due = min(due, `now + grace`) |
| ringing | `Tick`, `now ≥ ring_deadline` | — | `CallEnded{missed}` to all involved |
| ringing | `Tick`, caller away and `now ≥ away + grace` | — | `CallEnded{failed}` to all involved |
| active | `Tick`, liveness: a party device found live | (checked first) | that side's away cleared, before the grace check (an away time a tick wrote while the device reattached) |
| active | `Tick`, a side away and `now ≥ away + grace` | — | `CallEnded{failed, duration_s}` to both party devices |
| active | `Tick`, liveness: a party device gone, not marked away | active | that side's away = `now`; due = `now + grace` |
| active | `Tick`, otherwise | active | due = `now + LIVENESS`; TTLs refreshed |
| any | frame naming a call the sender's device has no role in | same | `CallError{CALL_NOT_FOUND}` |

Ties: the first CAS wins. Two accepts: one becomes active, the other re-reads and gets
`ALREADY_ANSWERED`. Cancel vs accept: whichever commits first; the loser gets
`CALL_NOT_FOUND` (call gone) or `ALREADY_ANSWERED`. Hangup vs reconnect: a `Hangup` ends
the call; a later `Attach` finds no call and the device's `CallSync` is empty. Grace end
vs reconnect: the `Tick` re-reads the record; an `Attach` that committed first cleared
the away time, so the tick only reschedules. An `Attach` of a party device always writes (bumping the version) so
a tick that read the registry before it loses its CAS and reads the device again; a tick
that finds a party device live clears its away time. A write whose reply was lost (a
timeout or a dropped connection) re-reads the record: if it landed, its frames are
published; retried `Invite` and `Accept` frames are idempotent (rows above).

`duration_s` = `(end − answered_ms) / 1000`, rounded down; only for calls that were
answered.

### 3.4 Diagram

```mermaid
stateDiagram-v2
    [*] --> Ringing: CallInvite ok / CallIncoming to callee devices
    [*] --> [*]: CallInvite busy / unreachable / refused
    Ringing --> Active: first CallAccept / CallAccepted, CallTaken, buffered ICE
    Ringing --> [*]: CallDecline / ended declined or busy
    Ringing --> [*]: CallCancel or caller CallHangup / ended cancelled
    Ringing --> [*]: ring deadline (45 s) / ended missed
    Ringing --> [*]: caller away > grace / ended failed
    Ringing --> Ringing: caller ICE (buffered), device attach/detach
    Active --> Active: ICE, renegotiate, attach (resume), detach (grace starts), liveness tick
    Active --> [*]: CallHangup / ended completed or failed
    Active --> [*]: a side away > grace (30 s) / ended failed
```

## 4. Protocol (`push.v1`, frame.proto)

Additive only: new field numbers, nothing reused or renumbered. Old clients never receive
call frames (they do not advertise the capability), and would drop them (an unknown oneof
field decodes as a frame without a body).

### 4.1 Frame numbers

| Direction | Field | Number | Message |
|---|---|---|---|
| client → server | `call_invite` | 11 | `CallInvite` |
| | `call_accept` | 12 | `CallAccept` |
| | `call_decline` | 13 | `CallDecline` |
| | `call_cancel` | 14 | `CallCancel` |
| | `call_ice` | 15 | `CallIce` |
| | `call_hangup` | 16 | `CallHangup` |
| | `call_renegotiate` | 17 | `CallRenegotiate` |
| server → client | `call_incoming` | 44 | `CallIncoming` |
| | `call_accepted` | 45 | `CallAccepted` |
| | `call_taken` | 46 | `CallTaken` |
| | `call_ended` | 47 | `CallEnded` |
| | `call_error` | 48 | `CallError` |
| | `call_sync` | 49 | `CallSync` |
| | `peer_call_ice` | 50 | `CallIce` (from the other party) |
| | `peer_call_renegotiate` | 51 | `CallRenegotiate` (from the other party) |

18–31 stay free for client frames, 52–63 for server frames (PR 2 takes 52 for
`CallHistoryChanged`).

### 4.2 Messages

```proto
enum Capability {
  CAPABILITY_UNSPECIFIED = 0;
  CAPABILITY_CALLS = 1;          // 1:1 audio calls (this document)
}
// Identify: repeated Capability capabilities = 2;   what the client can do
// Ready:    repeated Capability capabilities = 4;   what this server offers

enum CallMedia { CALL_MEDIA_UNSPECIFIED = 0; CALL_MEDIA_AUDIO = 1; CALL_MEDIA_VIDEO = 2; }
enum CallEndReason {
  CALL_END_REASON_UNSPECIFIED = 0; COMPLETED = 1; MISSED = 2; DECLINED = 3; CANCELLED = 4;
  BUSY = 5; FAILED = 6; UNREACHABLE = 7;               // prefixed CALL_END_REASON_ in proto
}
enum CallSdpType { CALL_SDP_TYPE_UNSPECIFIED = 0; OFFER = 1; ANSWER = 2; }
enum CallDirection { CALL_DIRECTION_UNSPECIFIED = 0; IN = 1; OUT = 2; }
enum CallStatus { CALL_STATUS_UNSPECIFIED = 0; RINGING = 1; ACTIVE = 2; }

message CallInvite { bytes call_id = 1; bytes callee_user_id = 2; string offer_sdp = 3;
                     repeated CallMedia media = 4; }
message CallAccept { bytes call_id = 1; string answer_sdp = 2; }
message CallDecline { bytes call_id = 1; optional CallEndReason reason = 2; } // DECLINED|BUSY
message CallCancel { bytes call_id = 1; }
message CallIce { bytes call_id = 1; string candidate = 2; optional string sdp_mid = 3;
                  optional uint32 sdp_mline_index = 4; bool end_of_candidates = 5;
                  optional string username_fragment = 6; }
message CallHangup { bytes call_id = 1; optional CallEndReason reason = 2; } // COMPLETED|FAILED
message CallRenegotiate { bytes call_id = 1; string sdp = 2; CallSdpType type = 3; }

message CallIncoming { bytes call_id = 1; bytes caller_user_id = 2; string offer_sdp = 3;
                       repeated CallMedia media = 4; int64 expires_at = 5;
                       uint32 expires_in_ms = 6; }
message CallAccepted { bytes call_id = 1; string answer_sdp = 2; bytes answered_device_id = 3; }
message CallTaken { bytes call_id = 1; }
message CallEnded { bytes call_id = 1; CallEndReason reason = 2; optional uint32 duration_s = 3; }
message CallError { bytes call_id = 1; ErrorCode code = 2; string message = 3;
                    bool retryable = 4; optional uint32 retry_after_s = 5; }
message CallSync { repeated CallState calls = 1; }
message CallState { bytes call_id = 1; bytes peer_user_id = 2; CallDirection direction = 3;
                    CallStatus status = 4; repeated CallMedia media = 5;
                    optional string offer_sdp = 6;      // incoming ringing only
                    optional int64 expires_at = 7; optional uint32 expires_in_ms = 8;
                    optional int64 answered_at = 9;
                    bool this_device = 10; }            // the call's device on this side
```

Times are microseconds since the Unix epoch (Redis clock); identifiers 16-byte UUIDs as
everywhere. `call_id` is a UUIDv7 chosen by the caller (version 7 enforced).

### 4.3 Error codes (added to `ErrorCode`)

| Code | Number | When |
|---|---|---|
| `ERROR_CODE_ALREADY_ANSWERED` | 27 | Accept or Cancel after another device answered |
| `ERROR_CODE_CALL_NOT_FOUND` | 28 | No such call for this device in this role (unknown, ended, another user's, wrong role) |
| `ERROR_CODE_IN_CALL` | 29 | The caller already has a call (ringing or active) |
| `ERROR_CODE_CALL_LIMIT` | 30 | ICE or renegotiation cap for the call reached |

Existing codes used: `NOT_CONTACT` (6, the request's NOT_A_CONTACT), `INVALID` (2:
malformed ids, not a UUIDv7, self-call, bad SDP, video, bad candidate), `RATE_LIMITED`
(23, attempts per hour, with `retry_after_s`), `NOT_AVAILABLE` (24, calls not offered on
this connection), `UNAVAILABLE` (9, retryable: Redis or message-svc unavailable, CAS
retries exhausted). Call errors always come as `CallError` (never `Error`); only
`UNAVAILABLE` and `RATE_LIMITED` are retryable.

### 4.4 Capability advertisement

- The client lists `CAPABILITY_CALLS` in `Identify.capabilities` only if it can take
  calls (has call UI, or the media helper). The SDK advertises it only when the app opts in.
- `Ready.capabilities` lists what the server offers on this connection: `CAPABILITY_CALLS`
  iff the gateway has `PUSH_CALLS_ENABLED=1` and message-svc answered `Identify` with
  `CAPABILITY_CALLS` (message-svc has calls enabled). A client hides call UI otherwise.
- A connection is calls-active only with both. Only calls-active devices are attached, so
  a callee with no capable online device yields `CallEnded{unreachable}` at once.
- A non-calls-active connection that sends a call frame gets `CallError{NOT_AVAILABLE}`.

### 4.5 Client rules (summary; full recipe in docs/calls.md, PR 5)

- Only the caller makes the initial offer. Renegotiation (ICE restart, later media
  changes) may come from either side; resolve glare with "perfect negotiation", the
  **callee is polite**.
- Trickle ICE: send each local candidate as `CallIce`, then `end_of_candidates: true`.
  Candidates from the peer may arrive before `CallAccepted` is processed; buffer them.
- Ring locally for `expires_in_ms` from receipt; stop on `CallTaken` or `CallEnded`.
- After a reconnect, read `CallSync`: drop calls not listed; for a listed active call with
  `this_device`, do an ICE restart through `CallRenegotiate` (offer with `iceRestart`).
- `CallCancel` answered `ALREADY_ANSWERED`: the call connected; send `CallHangup`.

### 4.6 Rolling upgrades and compatibility

| Combination | Result |
|---|---|
| old client, any server | never advertises; never attached; callers see `unreachable`; messaging unchanged |
| new client, old gateway | `Ready` without capabilities: call UI hidden; a call frame would be `BAD_FRAME` |
| new gateway, old message-svc | `IdentifyResponse` without capabilities: calls not offered; frames `NOT_AVAILABLE` |
| new gateway and message-svc, `calls.enabled: false` | not offered |
| mixed message-svc replicas | a `Call` RPC landing on an old replica is `UNIMPLEMENTED` → `CallError{UNAVAILABLE}` (retryable); only new replicas sweep |
| mixed gateway replicas | devices on old gateways are not attached (unreachable on them); `Notification.call` is ignored by old gateways |
| downgrade | records expire by TTL; nothing persistent in phase 1 PR 1 |

Operators enable `calls.enabled` after every gateway and message-svc replica runs a
version with calls; the chart sets both services from the one value.

### 4.7 Internal API (`push.internal.v1`)

```proto
// Messaging service, added:
rpc Call(CallRequest) returns (CallReply);
rpc CallAttach(CallDeviceRequest) returns (push.v1.CallSync);
rpc CallDetach(CallDeviceRequest) returns (CallDetachResponse);

message IdentifyResponse { repeated push.v1.Capability capabilities = 1; }
message CallRequest { bytes user_id = 1; bytes device_id = 2;
  oneof frame { push.v1.CallInvite invite = 3; push.v1.CallAccept accept = 4;
    push.v1.CallDecline decline = 5; push.v1.CallCancel cancel = 6; push.v1.CallIce ice = 7;
    push.v1.CallHangup hangup = 8; push.v1.CallRenegotiate renegotiate = 9; } }
message CallReply { optional push.v1.CallEnded ended = 1; }   // immediate busy/unreachable
message CallDeviceRequest { bytes user_id = 1; bytes device_id = 2; bytes connection_id = 3; }
message CallDetachResponse {}
// Notification: CallSignal call = 7;
message CallSignal { repeated bytes device_ids = 1;
  oneof frame { push.v1.CallIncoming incoming = 2; push.v1.CallAccepted accepted = 3;
    push.v1.CallTaken taken = 4; push.v1.CallEnded ended = 5; push.v1.CallIce ice = 6;
    push.v1.CallRenegotiate renegotiate = 7; } }
```

`Call` errors are statuses carrying a `push.v1.Error` (as every Messaging RPC); the
gateway turns them into `CallError` with the frame's `call_id`. message-svc with calls
disabled answers `Call`/`CallAttach` with `FAILED_PRECONDITION` (`NOT_AVAILABLE`).

### 4.8 Extensibility (no breaking change needed)

- **Video**: `CALL_MEDIA_VIDEO` exists; a later release accepts it when both sides
  advertise a new `CAPABILITY_CALLS_VIDEO`, and the SDP check allows `m=video`.
- **Group calls**: a new `repeated bytes callee_user_ids` on `CallInvite`, a
  `participants` list on `CallState`, `CAPABILITY_CALLS_GROUP`; the record grows a
  participant map (D19 allows it). 1:1 frames keep their meaning.
- **Push ringing**: `CAPABILITY_CALLS_PUSH` devices registered for push are counted as
  reachable; `CallIncoming` stays the same frame, delivered after the push wakes the app
  (offer fetched through `CallSync`).
- **Recording**: a `recording` flag on `CallState`/`CallAccepted` and a consent frame;
  media stays peer to peer (a recorder is a third participant, i.e. after group calls).

## 5. Redis data model and expiry

All keys tagged `{calls}` (D3). Times are Redis-clock milliseconds.

| Key | Type | Content | Expiry |
|---|---|---|---|
| `calls:{calls}:call:<call_id>` | hash | `ver` (integer), `rec` (JSON record) | ringing: `ring_deadline + 60 s`; active: `now + 120 s`, refreshed by every write and liveness tick (≤ 15 s apart); deleted at the end |
| `calls:{calls}:user:<user_id>` | string | the user's call id (busy lock; one call per user, all devices) | same as the record; deleted at the end if it still names the call |
| `calls:{calls}:devices:<user_id>` | hash | device id → connection id of each attached (calls-active) connection | 24 h, refreshed on attach; field removed on detach of that connection |
| `calls:{calls}:due` | sorted set | call id → next deadline | member removed at the end; a claim pushes it 5 s ahead |
| gateway limiter `rl:…` | string | fixed window counter for `CallInvite` (existing limiter, hashed subject) | the window |

Scripts (EVALSHA, reloaded on NOSCRIPT, as presence):

- **load**(`call`): `{ver, rec, TIME}`.
- **apply**(`call`, `due`, user keys…; expected `ver`, new `rec` or delete, ttl, due score,
  users to lock, users to release): checks `ver` (or absence for a create), for a create
  checks the caller's then the callee's lock (`IN_CALL` / `BUSY` / `CALL_EXISTS`), writes
  everything or nothing. Returns `OK`, `CONFLICT`, `IN_CALL`, `BUSY` or `EXISTS`.
- **claim**(`due`; max 32, lease 5000): due members ≤ `TIME`, each pushed to now + lease;
  returns them with `TIME`.
- **attach**(`devices:<user>`; device, connection): set unless a newer connection (UUIDv7
  ids sort by time) is registered; a refused attach gets an empty `CallSync`.
- **detach**(`devices:<user>`; device, connection): remove if equal.

Liveness of an attached device: `ZSCORE presence:{<user>}:conns <connection>` > now
(another slot: read outside the scripts, tolerant of races; a tick pipelines the `HGET`s of
both party devices, then their `ZSCORE`s). If Redis loses everything,
calls in progress lose their state: the next frame of either side gets `CALL_NOT_FOUND`,
media continues peer to peer until a side hangs up locally.

The record is bounded: offer ≤ 16 KiB (dropped when answered), ≤ 32 buffered candidates
(≤ 2 KiB each with metadata) — under 100 KiB per call (JSON escaping of the worst-case input), one call per user.

## 6. Failure matrix

| Failure | Effect | Recovery |
|---|---|---|
| Gateway graceful restart (rollout, 4005) | connections detach → grace starts | clients reconnect elsewhere within seconds → attach → `CallSync` → ICE restart; media is peer to peer and usually never stops |
| Gateway crash (no close) | no detach | the presence lease of its connections ends ≤ 90 s later; the next liveness tick (≤ 15 s) marks the side away, grace applies: the call ends `failed` ≤ ~135 s later unless the client reconnected (it normally does within seconds, which resumes) |
| message-svc replica crash | in-flight `Call` RPC fails → `CallError{UNAVAILABLE}`; a claimed due entry re-fires after its 5 s lease on another replica | automatic |
| All message-svc down | call frames `UNAVAILABLE`; no timers run | keys expire 120 s after the last write (ringing: 60 s after the deadline), releasing the busy locks |
| Redis failover without data | records, locks and registry lost | frames get `CALL_NOT_FOUND`; clients end locally; devices re-attach on reconnect; `CallSync` empty |
| Redis failover, pub/sub messages lost | a `CallIncoming`/`CallEnded`/relay may be lost | ringing ends by deadline on both sides (clients time out locally with `expires_in_ms`); the hub rings all connections after resubscribing and clients reconnecting get `CallSync`; ICE has redundancy (restart via renegotiate) |
| Redis unavailable | invite `UNAVAILABLE` (limiter fails closed too) | retry |
| Half-open client socket | detected by the gateway's heartbeat rule (2.5 heartbeats, 75 s by default) → detach → grace | the grace starts at detection, so a half-open socket ends a call ≤ 75 s + 30 s after it died; documented |
| Half-open message-svc channel | HTTP/2 keepalive (existing) fails calls fast | retry |
| Clock skew between replicas | none: Redis `TIME` only | — |
| Client clock skew | ringing uses `expires_in_ms` from receipt | — |
| CAS contention (races) | retried up to 5 times, then `UNAVAILABLE` (retryable) | client retries |
| Takeover (same device, new connection) | attach replaces the registry entry; the old connection's detach no longer matches and changes nothing | — |
| Device revoked / password reset mid-call | the kick closes the connection → detach → grace → `failed` | — |

## 7. Threat model

| # | Threat | Mitigation | Test (file) | PR |
|---|---|---|---|---|
| T1 | Calls from non-contacts or blocked users | policy in message-svc: both directions `ACCEPTED`, no block either way; unknown ids look the same (`NOT_CONTACT`) | `calls_policy_*` in `push-domain/src/calls/tests/policy.rs`; `a_stranger_or_blocked_user_cannot_call` in `push-gateway/tests/calls.rs` | 1 |
| T2 | Ringing used to spam a user | one ringing call per caller (`IN_CALL`), busy lock per callee, `calls.attemptsPerHour` per user (also counts refused invites), frame budget, decline/block | `attempts_per_hour_are_limited`, `a_caller_has_one_call_at_a_time` in `push-gateway/tests/calls.rs`; `call_invites_are_counted_per_user_by_the_configured_hour`, `call_invites_default_to_30_an_hour` in `push-gateway/src/limits.rs` | 1 |
| T3 | Call-state spoofing (accept/end another's call by `call_id`) | every frame checked against the record: sender user and device in their role; identical `CALL_NOT_FOUND`; call ids are unguessable UUIDv7 but never trusted | `wrong_role_is_not_found` in `push-domain/src/calls/tests/machine.rs`; `another_users_device_cannot_touch_the_call` in `push-gateway/tests/calls.rs` | 1 |
| T4 | SDP / ICE injection, oversize or malformed | size caps (16 KiB SDP, 512 B candidate), line grammar, printable ASCII candidates, audio only, protobuf decode limits; never interpreted beyond that, relayed as opaque strings | proptests `sdp_check_never_panics`, `sdp_with_a_video_line_is_refused`, `sdp_padded_beyond_the_limit_is_refused`, `ice_check_never_panics` in `push-domain/src/calls/sdp.rs`; `decoding_never_panics`, `call_invites_round_trip` in `push-proto/src/codec.rs`; `oversize_and_video_offers_are_refused` in `push-gateway/tests/calls.rs` | 1 |
| T5 | TURN used to reach internal addresses (SSRF, lateral movement) | coturn `denied-peer-ip` for RFC 1918, 100.64/10, link-local (incl. 169.254.169.254 metadata), loopback, the cluster pod and service CIDRs, IPv6 ULA/link-local/loopback, `no-multicast-peers`, `no-loopback-peers`; NetworkPolicy egress | chart tests render every range; integration test: a relay allocation to 10.x/127.x/169.254.169.254 is refused by the real coturn container | 3 |
| T6 | TURN credential theft or replay | credentials expire (`ttlSeconds`, 600 s), scoped to the user (`<expiry>:<user_id>`), minted only for an access token, never logged, HTTPS only; `user-quota` limits damage | `credentials_expire_and_name_the_user`, `credentials_are_never_logged`, coturn refuses an expired credential | 3 |
| T7 | Leaking IP addresses to non-contacts | SDP/ICE only after the contact policy; callee's IPs only after the callee accepts; pre-answer caller ICE only to the answering device; docs recommend trickle (offer without candidates) and `iceTransportPolicy: relay` for privacy; no IPs in logs/metrics/audit | `ringing_devices_never_get_caller_candidates`, `answer_and_ice_only_reach_the_party_device`, `no_sdp_or_candidate_in_logs_or_audit` in `push-gateway/tests/calls.rs` | 1 (+ docs 5) |
| T8 | DoS by exhausting call state | one call per user; bounded record (under 100 KiB per call: JSON escaping of the worst-case input); TTLs on every key; attempts/hour; ICE and renegotiation caps; due set bounded by live calls | `records_are_bounded`, `ice_cap_and_buffer_cap` in `push-domain/src/calls/tests/machine.rs`; `keys_expire_without_message_svc` in `push-message-svc/tests/calls_redis.rs`; `ice_frames_are_capped_per_side` in `push-gateway/tests/calls.rs` | 1 |
| T9 | Agents calling anyone but their owner | policy: an agent may only call its owner, only its owner may call it (`NOT_CONTACT` otherwise), checked for both directions | `agent_calls_only_its_owner`, `only_the_owner_calls_the_agent` in `push-domain/src/calls/tests/policy.rs`; `agent_calls_only_its_owner` (both directions) in `push-gateway/tests/calls.rs` | 1 (bridge use: 7, 8) |
| T10 | Media or signalling content in logs, metrics, audit | no `Debug` of call frames in logs; audit fields fixed (ids, reason, duration); metrics labels are reasons/codes only | `no_sdp_or_candidate_in_logs_or_audit` in `push-gateway/tests/calls.rs` (a capturing subscriber) | 1 |
| T11 | Call history seen by others | rows per participant, read only by `user_id = caller`; delete hides only one's own row | `history_is_private_and_hides_per_side` | 2 |
| T12 | Provider API secrets (cloudflare/twilio) leak | secrets only from the Secret (file), never values; never logged; cached credentials in memory | chart refuses inline secrets; `provider_secrets_never_logged` | 3 |

## 8. TURN (PR 3)

`GET /v1/calls/ice-servers` on auth-svc (access token): `{"ice_servers":[{"urls":[…],
"username":"…","credential":"…"}],"expires_at":"<RFC 3339>","relay":true|false}`.
Unavailable configuration or provider failure → `503 {"error":"turn_unavailable"}`,
never unusable servers. Rate limit: 60 per user per hour. Metric
`push_turn_credentials_issued_total{mode}` / `…_failures_total{mode}`.

| Mode | Servers | Credential |
|---|---|---|
| `internal` | chart-deployed coturn: `stun:<publicHost>:3478`, `turn:<publicHost>:3478?transport=udp`, `turn:…?transport=tcp`, `turns:<publicHost>:5349?transport=tcp` (only with `tlsSecret`) + `stunUrls` | TURN REST: `username = "<expiry unix s>:<user_id>"`, `credential = base64(HMAC-SHA1(turn-shared-secret, username))`, expiry = now + `ttlSeconds` |
| `external` / `generic` | `external.urls` + `stunUrls` | same TURN REST scheme with the provider's shared secret (Secret key `turn-shared-secret`) |
| `external` / `cloudflare` | from the provider | `POST https://rtc.live.cloudflare.com/v1/turn/keys/<keyId>/credentials/generate-ice-servers` (Bearer token from Secret key `turn-cloudflare-token`), `ttl` = `ttlSeconds`; cached per user until 60 s before expiry |
| `external` / `twilio` | from the provider | `POST https://api.twilio.com/2010-04-01/Accounts/<sid>/Tokens.json` (Basic auth, token from Secret key `turn-twilio-token`), `Ttl` = `ttlSeconds`; cached (shared, not per user) until 60 s before expiry |
| `stun-only` | `stunUrls` (must not be empty in this mode: a validation error names `calls.turn.stunUrls`) | none; `relay: false`. Development only |

coturn (internal): image pinned by digest (`coturn/coturn:4.6.3@sha256:…`, CI scans it with
Trivy), `use-auth-secret`, `static-auth-secret` from the app Secret key
`turn-shared-secret` (consumer-managed supported), `realm`, `listening-port 3478`,
`tls-listening-port 5349` (with `tlsSecret`), `min-port`/`max-port` from
`relayPortRange`, `no-tcp-relay` **off** (TCP relay allowed), `denied-peer-ip` for every
range in T5 plus `calls.turn.internal.deniedPeerCidrs` (cluster pod/service CIDRs,
required), `user-quota 12`, `total-quota 1200`, `max-bps 512000` (values configurable),
`no-cli`, `no-multicast-peers`, `no-loopback-peers`, `prometheus` on 9641, `fingerprint`,
`stale-nonce 600`. Its own Service (`type: LoadBalancer` by default, UDP+TCP ports and the
relay range), readiness/liveness TCP probes on 3478. NetworkPolicy: ingress on its ports
from anywhere; egress to anywhere except the denied ranges. Security trade-offs documented
in docs/calls.md: a public relay is an open UDP port range; the denied ranges and quotas
are what keep it from being an SSRF or bandwidth amplifier.

## 9. Call history (PR 2)

- Table `messaging.call_history` (migration `0008_call_history.sql`): `call_id uuid`,
  `user_id uuid`, `peer_user_id uuid`, `direction text ('in'|'out')`, `started_at`,
  `answered_at null`, `ended_at`, `duration_s int`, `outcome text`, `hidden_at null`;
  primary key `(user_id, call_id)`; index `(user_id, ended_at desc, call_id)`. Two rows per
  call, written when the call ends (also immediate busy/unreachable outcomes: the caller's
  row always; the callee's row for every outcome except `unreachable`, which the callee
  never saw), `ON CONFLICT DO NOTHING` (a re-run end is idempotent).
- Nothing about calls in conversation history.
- `GET /v1/calls?before=<cursor>&limit=50` (auth-svc → `Calls.History` gRPC): own rows not
  hidden, newest first, `limit` 1–100 (default 50), `cursor` = opaque base64url of
  `(ended_at, call_id)`; answer `{"calls":[{call_id, peer_user_id, direction, started_at,
  answered_at?, ended_at, duration_s, outcome}], "next": "<cursor>"?}`. `?since=<cursor>`
  returns rows newer than the cursor (for badges).
- `DELETE /v1/calls/{call_id}` sets `hidden_at` on the caller's own row: 204 (also when
  absent: no oracle).
- Retention `calls.historyDays` (default 90, 1–3650): message-svc deletes rows with
  `ended_at` older than that, hourly, in batches of 1000.
- Server frame `CallHistoryChanged { optional int64 latest_ended_at = 1; }` = 52, sent to
  the user's calls-active devices after their row is written (badge refresh).
- Metrics: `push_call_history_rows_deleted_total`.

## 10. SDK (PR 4), docs (PR 5) and phase 2 (PRs 6–8)

### 10.1 push-sdk Calls API

```rust
// ClientConfig::calls(true) makes the client advertise CAPABILITY_CALLS.
impl Client {
    pub fn calls(&self) -> Calls;                         // handle
}
impl Calls {
    pub fn available(&self) -> bool;                      // Ready offered CAPABILITY_CALLS
    pub async fn ice_servers(&self) -> Result<IceServers>;        // GET /v1/calls/ice-servers
    pub async fn invite(&self, peer: UserId, offer_sdp: &str) -> Result<CallId>;
    pub async fn accept(&self, call: CallId, answer_sdp: &str) -> Result<()>;
    pub async fn decline(&self, call: CallId, busy: bool) -> Result<()>;
    pub async fn cancel(&self, call: CallId) -> Result<()>;
    pub async fn hangup(&self, call: CallId, failed: bool) -> Result<()>;
    pub async fn send_ice(&self, call: CallId, candidate: IceCandidate) -> Result<()>;
    pub async fn renegotiate(&self, call: CallId, sdp: &str, kind: SdpKind) -> Result<()>;
    pub async fn history(&self, before: Option<&str>, limit: u32) -> Result<CallPage>;
    pub async fn hide(&self, call: CallId) -> Result<()>;
}
// Event::Call(CallEvent): Incoming, Accepted, Taken, Ended, PeerIce, PeerRenegotiate,
// Error, Synced(Vec<CallState>), HistoryChanged.
```

Calls are fire-and-forget frames (like activity): `Offline` while disconnected, local
validation (SDP size, candidate) mirrors the server's. C and Node bindings follow with the
events in their JSON form (`docs/sdk-bindings.md`).

The media helper (PR 6, feature `calls-media`): `MediaCall::start(calls, ice_servers,
role) -> MediaCall` with `incoming() -> impl Stream<Item = PcmFrame>` (16 kHz mono, 20 ms,
`i16`), `outgoing() -> PcmSink`, `on_ended()`. str0m drives ICE/DTLS/SRTP; Opus 48 kHz via
audiopus; resampling 48 k ↔ 16 k in-house (polyphase FIR). It may exceed the workspace
MSRV only behind the feature (the `msrv` job builds default features).

### 10.2 Docs (PR 5)

`docs/calls.md`: protocol with sequence diagrams (normal flow, every ending, reconnect,
renegotiation), glare rules, trickle and restart, TURN modes and security, configuration
reference (every `calls.*` value with its validation error), AKS deployment (public UDP
LoadBalancer, NSG ports 3478/udp+tcp, 5349/tcp, relay range/udp, DNS), local cluster,
troubleshooting (no audio, one-way audio, symmetric NAT, corporate firewalls and `turns:`
on 443), retention, upgrade guide, interop checklist (Chromium, Edge, Firefox, Safari).
`docs/examples/calls/index.html`: a minimal browser client; Playwright smoke test with
Chromium fake media devices between two contexts (stun-only and internal TURN).

### 10.3 Bridge voice pipeline (PR 7)

`push-agent-bridge` config `calls` block:
`{ "answer": "auto"|"ask", "stt": {"provider": "azure-speech"|"local"|"none", …},
"tts": {…}, "vad": {"silence_ms": 700, "min_speech_ms": 300}, "language": "en-US" }`.
Pipeline: media helper PCM → VAD (energy + zero-crossing, in-house) → utterances → STT →
prompt (idle) or steer (turn running) to Coda; replies → markdown stripped (code blocks →
"I've put the code in the chat", tables → "I've put a table in the chat") → sentences →
TTS → PCM sink; owner speech during TTS = barge-in (TTS stops, utterance steered).
Transcripts in chat: `agent.transcript@1` for the owner (D22), `agent.reply@2` for the
agent. Activity labels "on a call", "listening", "thinking", "speaking". Voice answers to
an open card: transcript matched against its options (exact / ordinal "option two" /
approve/reject synonyms); ambiguous → asks again; the card answer carries `via: "voice"`.
Providers: `azure-speech` (key or Entra; audio leaves the machine to Azure), `local`
(whisper.cpp-compatible STT binary, a local TTS command; audio stays local), `none`.
Fakes for CI with recorded fixtures.

### 10.4 Answering and calling policies (PR 8)

`calls.answer: "ask"` posts `agent.approval`-style card "Owner is calling — answer?" on the
owner's other devices, answers on approve, declines after the ring. On answer the bridge
says "Connected to Coda in <folder name>". `calls.notify_by_call`: `{ "after_minutes": 0
(off), "urgent": false, "min_interval_minutes": 10, "do_not_disturb": "22:00-07:00" }`:
calls the owner when a permission request or question is unanswered for `after_minutes`,
or when `notify_user` marks something urgent (if `urgent`); opens with a spoken summary
and the question; missed or declined → the card stays as the fallback. Tray: per-agent
call state (ringing, in call, speaking) and Mute agent / Hang up in the window.

## 11. Metrics (message-svc `/metrics`, PR 1)

| Metric | Type | Labels |
|---|---|---|
| `push_calls_started_total` | counter | — |
| `push_calls_answered_total` | counter | — |
| `push_calls_ended_total` | counter | `reason` (completed, missed, declined, cancelled, busy, failed, unreachable) |
| `push_call_frames_refused_total` | counter | `code` |
| `push_call_setup_seconds` | histogram | — (accept received → `CallAccepted` published) |
| `push_call_signal_seconds` | histogram | `frame` (time to handle and publish a call frame) |
| `push_calls_live` | gauge | — (`ZCARD calls:{calls}:due` at scrape) |

Missed, failed and busy are `push_calls_ended_total{reason=…}`. Load targets (PR 4 extends
push-load): invite → `CallAccepted` relayed < 300 ms p95 within a region; 1,000 concurrent
ringing or active calls per gateway replica without errors.

## 12. Configuration

Chart values (PR 1 adds the first four; PR 2 `historyDays`; PR 3 `turn`):

| Value | Default | Validation (chart `fail`, and the service at start) | Env |
|---|---|---|---|
| `calls.enabled` | `false` | boolean | `PUSH_CALLS_ENABLED` (`1`/`0`) on gateway and message-svc |
| `calls.ringSeconds` | 45 | integer 10–120: "calls.ringSeconds must be an integer from 10 to 120" | `PUSH_CALLS_RING_SECONDS` (message-svc) |
| `calls.disconnectGraceSeconds` | 30 | integer 5–120 | `PUSH_CALLS_DISCONNECT_GRACE_SECONDS` (message-svc) |
| `calls.attemptsPerHour` | 30 | integer 1–1000 | `PUSH_CALLS_ATTEMPTS_PER_HOUR` (gateway) |
| `calls.historyDays` | 90 | integer 1–3650 | `PUSH_CALLS_HISTORY_DAYS` (message-svc) |
| `calls.turn.*` | see the request | mode enum; `internal.publicHost` required when internal and calls enabled; `stunUrls`/`external.urls` must start with `stun:`/`turn:`/`turns:`; provider secrets only by Secret key | auth-svc `PUSH_TURN_*` (PR 3) |

Services refuse invalid values at start with the variable name (existing `ConfigError`
style).

## 13. Testing (PR 1)

- Domain: table-driven transition tests (every row of §3.3, every timeout, the races:
  two accepts, cancel vs accept, hangup vs reconnect, tick vs attach), policy tests
  (contacts, pending, blocks both ways, unknown, agent rules), proptest for the SDP and
  ICE validators (never panic, size limits exact, any non-audio `m=` refused).
- push-proto: proptest that arbitrary bytes never panic `decode_frame` and call frames
  round-trip.
- Store contract: `agent_owner` on memory and PostgreSQL.
- message-svc: Redis scripts against real Redis (CAS conflict, busy/in-call/exists, claim
  lease, TTLs, attach/detach-if-equal), config validation.
- Gateway integration (`tests/calls.rs`): two gateway replicas, two message-svc instances,
  one Redis: every acceptance case in the request's phase-1 list except history (PR 2),
  with short ring (2 s) and grace (1 s).
- Chart: values render to env, validation messages.

## 14. Open items for later PRs

- PR 3 must confirm Cloudflare/Twilio endpoints against current provider docs and pick
  the coturn digest at implementation time.
- PR 6 must confirm str0m's crypto backend passes cargo-deny (no OpenSSL licence issue;
  prefer a pure-Rust/ring backend) and record the MSRV of the feature.
- Presence-lease-based liveness ties calls to the presence key layout
  (`push_proto::presence_conns_key`); a change there must update `calls::liveness`.

## 15. Implementation notes (PR 1)

Decisions taken while implementing, beyond or refining the sections above:

- `Attach`/`Detach` events carry the user as well as the device.
- An active call's due = min(`now + LIVENESS`, away + grace); a liveness `Tick` carries the
  sides found live as well as the gone ones, and a live side's away time is cleared (the
  tick-vs-attach race of §3.3).
- An `Attach` of a party device always bumps the version, even when nothing else changes.
- `RedisCalls::apply` re-reads the record after a lost reply and treats a stored record
  equal to the one it wrote as applied.
- A repeated `CallAccept` from the answering device re-publishes `CallAccepted`.
- A repeated `CallInvite` (same caller device, call still ringing) is a no-op.
- The registry attach refuses an older connection (UUIDv7 ids sort by time); the refused
  attach gets an empty `CallSync`.
- A ringing call whose ringing list is empty when it ends notifies only the caller.
- The callee must send `CallAccept` before its `CallIce` (it has no role in the call yet).
- The gateway caps `tungstenite` logging at debug, so frame contents never reach logs.
- message-svc `/metrics` is empty when calls are disabled.
- The gateway drops call frames from the hub on connections that are not calls-active.
- The sweeper claims again while claims come back full and ticks 16 calls at a time (D5).

### PR 2 (call history)

Decisions taken while implementing §9, beyond or refining it:

- Each history item carries its own opaque `cursor`; `next` is present only when the page is
  full. `before` and `since` combine (strictly older than `before` and newer than `since`).
  Cursors are base64url without padding of `<ended_at µs>:<call_id>` with one canonical
  spelling (anything else is `400 invalid`).
- `messaging.call_history` references `messaging.known_user` for both user columns; the
  user index is `(user_id, ended_at DESC, call_id DESC)`, and a second index on `ended_at`
  serves the purge. `outcome` and `direction` are `text` with CHECK constraints.
- A hidden row stays hidden when the end is recorded again (`ON CONFLICT DO NOTHING`).
- The rows are written after the Redis apply and the `CallEnded` publish, in a task of
  their own (three attempts, 100 ms then 200 ms apart, then logged and counted in
  `push_call_history_write_failures_total`, a metric beyond §9), so the ending frame's RPC
  deadline cannot cancel the write and a slow database never stalls the sweeper. A write in
  flight when the replica stops is lost. An ending apply whose reply is lost counts as
  applied when the re-read finds the record gone (PR 1), so its rows are still written;
  only a lost reply whose re-read fails too loses them.
- `CallHistoryChanged` travels as `CallSignal.history_changed` (field 8) on the user's
  `notify:` channel, addressed to the user's attached devices with a live presence lease
  (the same "live" as ringing). It is also sent, without `latest_ended_at`, to the user's
  own devices when they hide a call.
- The retention purge runs in message-svc (push-worker has no access to the messaging
  database), in every replica, hourly and once at start, whether calls are enabled or not
  (history recorded while calls were on still expires); at most 1000 batches of 1000 per
  run. The `Calls` gRPC service is likewise served with calls disabled.
- `/metrics` now always serves the history families; the call families still only with
  calls enabled (the body is no longer empty with calls disabled).
- auth-svc rate limits both routes together (`call_history`: 120 per minute per user, 600
  per IP). `Calls.History` treats `limit` 0 as 50 and clamps above 100; auth-svc accepts
  only 1–100. `DELETE` accepts any UUID spelling for the id.
- The gateway integration test `history_lists_both_sides_and_hiding_is_per_side` drives the
  `Calls` API in process (the HTTP layer is covered by auth-svc's tests with a fake log and
  its gRPC client by a stub server).

### PR 3 (TURN): decisions and deviations

- Provider APIs confirmed against the providers' docs (2026-11): Cloudflare Realtime TURN
  `POST https://rtc.live.cloudflare.com/v1/turn/keys/<keyId>/credentials/generate-ice-servers`
  (`Bearer` API token, `{"ttl"}`, `201 {"iceServers": [...]}`; URLs on port 53 dropped,
  browsers block them); Twilio `POST https://api.twilio.com/2010-04-01/Accounts/<sid>/Tokens.json`
  (Basic auth, form `Ttl`, `201 {"ice_servers": [{urls (a string), username?,
  credential?}], "ttl": "<seconds>"}`; the shorter of `ttlSeconds` and `ttl` is used).
- Twilio credentials are cached **per user** like Cloudflare's (not shared), so every
  credential is scoped to one user (T6); per-user single flight, at most 10000 users in
  memory per replica. Refresh margin: 60 s, or a fifth of a shorter `ttlSeconds`.
- The provider base URL is a constructor parameter for tests
  (`Turn::with_provider_base_url`), not an environment variable: nothing in production can
  redirect provider calls.
- Errors use the API's format: `503 {"error": {"code": "turn_unavailable", "retryable":
  true, …}}` with `Retry-After: 30`; rate limit 60/user/hour and 600/IP/hour. Without
  `PUSH_TURN_MODE` (calls off) the route exists and answers `turn_unavailable`.
- STUN URLs are their own `ice_servers` entry without credentials; TURN URLs share one
  entry with the credential. The chart builds every URL (internal mode: from `publicHost`
  and `ports`), so auth-svc has one TURN REST path for `internal` and `generic`
  (`PUSH_TURN_URLS`, not `PUSH_TURN_PUBLIC_HOST`/`PUSH_TURN_TLS`).
- `external.sharedSecret`, `.cloudflare.apiTokenSecret` and `.twilio.authTokenSecret` name
  a key of the app Secret (default `turn-shared-secret`, `turn-cloudflare-token`,
  `turn-twilio-token`), never hold a value; `external` requires a consumer-managed Secret
  or `secrets.volume`.
- Image: `coturn/coturn:4.18.0-alpine@sha256:c29135c08565810d37053f384584392b7c24b69570fc81a721b878d6fe63e04e`
  (not 4.6.3; alpine for the smaller package set). Its fixable HIGH/CRITICAL findings
  (BIND tools, brotli, the MongoDB driver: unused by the chart's configuration) are
  accepted with reasons and expiry in `deploy/helm/coturn.trivyignore.yaml`; the
  `coturn-image` CI job uses `aquasecurity/trivy-action` v0.36.0 (pinned SHA, after the
  March 2026 compromise of older tags) with Trivy v0.76.1.
- coturn 4.18 facts found by the integration test and applied: `denied-peer-ip` takes
  `IP` or `IP-IP` only (it aborts on CIDR), so the chart converts every CIDR to a range;
  a range of the unspecified address `::` denies **every** peer, IPv4 included, so `::/128`
  is not in the list and `deniedPeerCidrs` refuses prefixes shorter than /8 (IPv4) or /16
  (IPv6) and ranges starting at `::`; `no-cli`, `no-dtls`, `no-tlsv1`, `no-tlsv1_1` and
  `no-stun-backward-compatibility` are gone (the CLI, DTLS and classic STUN are off by
  default; TLS below 1.2 is not offered), and `no-loopback-peers` is the default. The
  integration test fails on any option coturn reports as unknown.
- `user-quota` counts by the user id: coturn strips the REST timestamp, so one user's
  credentials of different expiries share the quota (tested: the 13th allocation gets 486).
- Values beyond the request: `internal.externalIp` (coturn's `external-ip`, required in
  effect: else `loadBalancerIP` or an IP `publicHost`), `internal.deniedPeerCidrs`
  (required), `internal.quotas`, `internal.image`, `internal.metricsPeers`. The Service
  uses `externalTrafficPolicy: Local`, exposes the relay range over UDP and TCP (TCP
  relay stays on), and coturn runs as one replica with `Recreate` updates. The coturn
  NetworkPolicy repeats the denied ranges as egress exceptions.
- With `calls.enabled: true` the default `internal` mode requires `publicHost`, a public
  IP and `deniedPeerCidrs`; enabling calls without TURN settings now fails the render
  (choose `stun-only` for development).
- Local clusters: denied private ranges mean the chart's coturn cannot relay between
  private addresses; docs recommend `stun-only` locally. PR 5's browser smoke test "through
  internal TURN" will need either public addressing or a test-only allow list.

### PR 4 (push-sdk calls API): decisions and deviations

- `Config::calls` is a plain field (`false` by default), not a builder method; `Calls` is
  a handle that clones the client (like every clone it must be dropped before
  `Client::close` can finish).
- `history(before, since, limit)` takes `since` too (§9 and PR 2 added it); `limit` is
  checked locally (1–100) and `None` leaves the server's default of 50.
- Errors are typed twice: `CallEvent::Error` carries a `CallErrorCode` enum and an
  `SdkError::Server` whose code is the wire name in snake case (`already_answered`, …), so
  bindings pass it on unchanged. A frame method returns `offline` while the connection is
  down or reconnecting and `not_available` when `Ready` did not offer calls (the server's
  code, decided locally).
- `CallEvent::Synced` carries `gone`: the calls this client knew live (its invites and
  accepts, `Incoming`, `Accepted`) that the `CallSync` does not list. Calls invited or
  accepted on the new connection before its first `CallSync` (the gateway sends it before
  reading client frames) are kept, as are calls whose `CallState` has an unknown direction
  or status. An invite refused with `IN_CALL`, `NOT_CONTACT` or `RATE_LIMITED` never
  existed and is dropped from the set; other refusals keep it. The set is cleared on
  sign-out and when another user signs in.
- `CallEndReason` and `CallErrorCode` are `#[non_exhaustive]`; an unknown wire reason (or
  history outcome) reads as `Failed`. `CallId` is a new push-domain id type.
- Times are milliseconds: `Incoming.expires_at_ms` and `CallState.expires_at_ms` are on
  this device's clock (receipt + `expires_in_ms`), server times (`answered_at`, history,
  `expires_at` of ICE servers) are converted from microseconds or RFC 3339 (chrono).
- The bindings (C, Node) carry the call events in JSON (`{"type": "call", "call": {…}}`)
  but get neither the methods nor the `calls` option in this PR (the request's "and,
  later, the bindings"); without the option they never advertise calls, so no call event
  reaches them yet.
- push-load: the `calls` scenario landed with PR 5 (#58); PR 4 adds the Redis
  `INFO commandstats` deltas (`--redis-url-file`, URLs only from files as push-load's
  other credentials).
- Tests: `crates/push-smoke/tests/calls_sdk.rs` runs two (and three) SDK clients against
  the whole stack in process: a complete call with two callee devices, buffered and
  trickled ICE both ways, renegotiation, history, `HistoryChanged` and hide; decline, busy
  decline, cancel, busy, unreachable, `NOT_CONTACT`, `IN_CALL`, `not_available`;
  `CallSync` after a reconnect; `ice_servers` in stun-only and TURN REST modes (the HMAC
  checked). Missed calls, the disconnect grace and the frame caps stay covered by the
  gateway tests (PR 1). The media smoke through coturn is PR 6.

### PR 6 (media helper): decisions and deviations

- **Crates.** str0m 0.25 as D21 chose, with default features off and the `aws-lc-rs`
  backend: its `rust-crypto` backend still builds AWS-LC (dimpl's `rcgen` feature), so a
  pure-Rust backend saves nothing. Opus through `opus` 0.4 / `opusic-sys` (libopus 1.5,
  bundled, CMake) instead of `audiopus`, unreleased since 2021. str0m has no TURN client:
  `turn-client-proto` 0.7 (sans-IO, UDP and TCP). All pass cargo-deny; `ctutils` moved to
  0.4.4 for RUSTSEC-2026-0385 (a dependency of dimpl).
- **MSRV.** str0m needs Rust 1.89. The feature is off by default and only push-smoke's
  tests enable it (a dev-dependency), so the `msrv` job (`cargo +1.85 build --workspace`,
  no test targets) never compiles it; the crate docs and docs/calls.md §19.1 state 1.89
  for the feature.
- **API** (§10.1 refined): `MediaCall::start(&Calls, &IceServers, Role, MediaOptions)`
  with `Role::Caller { peer }` / `Role::Callee { call_id, offer_sdp }`; the app forwards
  call events to `MediaCall::handle` (the client has one event stream, owned by the app);
  `incoming()` takes a `PcmStream` (a `Stream`), `outgoing()` a `PcmSink` that queues up
  to 5 s and is paced by the helper (one frame per 20 ms) with `clear()` for barge-in;
  `connected()`, `state()`, `on_ended() -> MediaEnded { reason, duration_s, cause }`
  with `EndCause::{Server, Local, MediaFailed, Gone, Refused}`; `hangup()`, and drop
  hangs up. A local hang-up resolves at once (`Local`, `Completed`) without waiting for
  the server's confirmation.
- **Gathering before signalling.** The helper gathers (host, server-reflexive, one TURN
  allocation) before making the offer or answer, which carries every candidate, then
  sends only an end-of-candidates marker: simpler than trickling and at most
  `gather_timeout` (5 s) slower. IPv4 only; `turns:` URLs are skipped (no TLS to TURN).
- **Media failure.** No connection within 30 s of both descriptions, or disconnected for
  15 s, hangs up with `FAILED` (`EndCause::MediaFailed`).
- **str0m quirk.** For an offer it answers, str0m 0.25 does not report `MediaAdded` in
  the output drained right after `accept_offer`, so the helper reads the audio `mid`
  from its answer.
- **Test placement.** The E2E smoke is `crates/push-smoke/tests/calls_media.rs` (the
  plan said push-sdk): push-smoke owns the in-process stack. Three variants (stun-only
  host candidates; relay-only through coturn over UDP and over TCP), opt-in with
  `PUSH_CALLS_MEDIA=1`, run by the new `calls-media` CI job. The coturn container helper
  moved from `browser_calls.rs` to `tests/common/coturn.rs` and publishes UDP too.
- **Robustness.** `MediaCall::start` runs detached on the client's runtime, so a caller
  that gives up after the invite or accept drops a `MediaCall` that hangs up. A caller
  whose answer never reaches the helper (the app did not forward it) hangs up as failed
  after 90 s plus `connect_timeout`. A server refusal (`NOT_CONTACT`, `IN_CALL`, …) ends
  the helper's call only while it is being set up; later refusals of ICE or
  renegotiation frames leave it running.

### PR 7 (bridge voice pipeline): decisions and deviations

- **Build.** push-bridge's optional feature `voice = ["push-sdk/calls-media"]` (Rust 1.89,
  CMake) gates only the media adapter (`voice/media.rs`, `MediaPhone`) and its wiring in
  `main.rs`. The pipeline, providers, detector, spoken text, voice answers and the core
  integration always compile with Rust 1.85 and are tested through a `FakePhone`; the
  `msrv` job never builds the feature, push-smoke enables it as a dev-dependency. A build
  without the feature does not advertise calls and warns once about a `calls` section;
  `status` prints a `Voice:` line.
- **Answering.** `calls.answer` accepts only `auto` in PR 7 (`ask` is refused by
  validation until PR 8). The owner's call is answered; anyone else's is declined (the
  server refuses non-contacts first with `NOT_CONTACT`), a second call while one runs (or
  is being answered, or the bridge stops) is declined busy. With no `calls` section or
  `calls.stt` `none` the client does not advertise calls at all, so the server answers
  the owner `UNREACHABLE` instead of the bridge declining.
- **D22 refined.** `agent.transcript@1` has no `voice` param: the template always means a
  voice transcript, and tpl/1 requires every param to be shown. Params `speaker`
  (`owner`|`agent`) and `text` (4,000 characters), a literal heading "🎤 voice", fallback
  "🎤 voice: <text>". Masked like everything the bridge sends; the peek shows it as the
  owner's text "🎤 …". The bridge posts only `speaker: owner`; its spoken replies stay the
  turn's `agent.reply@2`.
- **Voice answers edit the card** instead of an ANSWER with `via: "voice"`: the bridge
  sent the card and the server refuses a sender's own answer, and tpl/1's strict ANSWER
  format has no room for `via`. The request is settled locally (as a typed answer) and the
  card is edited into a closed `agent.reply@1` "<card title>" / "🎤 Answered by voice:
  <label>."; the new body is not interactive, so the server refuses later answers to it.
  Matching (§10.3's "exact / ordinal / synonyms") became: approve and reject word lists
  with exactly one side heard, a negation never approving and a question asked again;
  options by cardinal ("option two", a bare "two"), ordinal ("the second one") or label;
  several for a multi-select; free text when allowed. Unclear → a fixed "Sorry, …"
  sentence and the card stays open. Only the oldest open card is spoken and answered, and
  only by an utterance whose onset (the VAD's `SpeechStarted`, carried in
  `SessionEvent::Transcript { text, started }`) came after the card was posted
  (`OpenRequest::opened`); an utterance begun earlier is a prompt or steer and the card
  stays open.
- **Providers.** REST only, no new crates (reqwest with rustls): Azure AI Speech
  short-audio STT and TTS (`raw-16khz-16bit-mono-pcm`, SSML with `voice`, default
  `en-US-JennyNeural`); a key from an environment variable (`key_env`, default
  `AZURE_SPEECH_KEY`) with `region`, or an Entra token from a command (default `az account
  get-access-token …`, reused 30 minutes, refreshed once on a 401) with the custom-domain
  `endpoint` and optional `resource_id` (`aad#…`). No websocket or streaming recognition:
  each utterance is one request. `local`: a whisper.cpp server URL (multipart built by
  hand) or a command with `{wav}`/`{language}` for STT; a command writing raw s16le at
  `sample_rate` (piper `--output_raw`) or a URL answering a WAV for TTS. Commands run
  without a shell with a 30 s limit; the temporary WAV is readable only by the user and
  removed after each utterance. Errors carry content-free codes only.
- **VAD.** Energy only (§10.3 said energy plus zero-crossing): RMS dBFS per 20 ms frame against a noise floor that is the
  quietest frame of a sliding 1.5 s window (clamped to -70..-30 dBFS), speech at floor
  +10 dB and above -55 dBFS, so a steady noise cannot hold the detector in speech. Onset
  after 3 frames with 200 ms pre-roll, end after `silence_ms` with at most 200 ms kept,
  `min_speech_ms` of speech or the utterance is dropped, a cut at 30 s.
- **Speech text.** Line-based and incremental: code fences → "I've put the code in the
  chat.", tables (a `|` line or a delimiter row; a header line with `|` waits for the next
  line) → "I've put a table in the chat." (not repeated back to back), headings and list
  items as sentences of their own, links → their text, bare URLs → "a link", code spans
  unwrapped, emphasis/quotes/rules/HTML removed; sentences split after `.`/`!`/`?`/`…`
  except common abbreviations, at most 300 characters. Background messages are spoken as
  "<label>." and their text. Each complete line (and a background message's label) is
  masked (`mask::mask`) before it is read, so a secret never reaches the TTS provider nor
  splits across sentences; the mask is said as "hidden".
- **Barge-in and the session mute.** An onset while the agent speaks bumps a generation:
  the queued sentences and the sink's queued audio are dropped (`clear()`), and the session
  mutes the reply itself (`say_reply` drops sentences until the core unmutes it on the
  next transcript, voice answer or turn start), so reply text the core handles before the
  `BargedIn` event is not spoken either. The bridge's own sentences (greeting, card
  questions, confirmations) are never muted. A barge-in (and the unmute after one)
  interrupts the speech text (`SpeechText::interrupt`): the pending paragraph, a held line
  and the rest of the arriving line are dropped, but an open fence or table stays open. A
  transcript without a barge-in changes nothing in the speech text.
- **Activity.** A card open or a pause keeps its label; otherwise during a call, working
  with "speaking" > "listening" > "thinking" (a turn runs) > "on a call". The greeting is
  "Connected to <name> in <folder>." (with a period), followed by the oldest open card.
- **Tests.** Tone-based fakes: `FakeStt` hears a word per frequency (Goertzel), `FakeTts`
  speaks 600 Hz for a length that follows the text. Recorded fixtures
  (`tests/fixtures/voice/*.json`: prompt, barge-in, approve, option two, ambiguous) with
  frequency rules (350–2400 Hz, 200 Hz apart, never within 150 Hz of 600 Hz). Core tests
  in paused time through `FakePhone`; a public-API test in `tests/bridge.rs`; the E2E
  `crates/push-smoke/tests/bridge_voice.rs` (CI job `bridge-voice`, `PUSH_BRIDGE_VOICE=1`)
  runs the bridge with `SdkMessenger`, `MediaPhone` and a scripted backend against the
  owner's `MediaCall` over real Opus media; `calls-media-windows` also runs the bridge's
  `voice::` tests with the feature. Live providers: `PUSH_BRIDGE_LIVE_SPEECH=1`, not in CI.
- **SDK pacing fix found by the E2E.** The media helper's 20 ms clock used
  `MissedTickBehavior::Delay`; on Windows' ~15.6 ms timer a late tick came every ~31 ms
  (about 32 frames a second), so the agent's voice arrived with gaps. It now uses `Burst`
  and resets after a stall over 100 ms (PR 6's helper, fixed here).
- **Other details.** Utterances are transcribed one at a time, in order; an STT failure is
  said as "Sorry, I couldn't make that out.", a TTS failure skips the sentence. Up to 5 s
  of the owner's audio waits for the pipeline, newer frames are dropped beyond. Without
  TTS the call is silent (no greeting) and replies stay in the chat. Call events that
  arrive while the answer is in progress are buffered and handed to the line after it.

### PR 8 (answering and calling policies): decisions and deviations

- **Ask mode.** `calls.answer: "ask"` posts a built-in `agent.question@1` card ("📞 Owner
  is calling <agent> in <folder> — answer?", options Answer / Decline) instead of an
  `agent.approval`-style card: the approval template's buttons say Approve / Reject. The
  card goes to the owner's conversation, so it shows on **all** the owner's devices, the
  calling one too (push-messenger cannot address one device; §10.4 said "the owner's
  other devices"). Answer → the bridge answers (call events that came meanwhile are
  buffered and handed to the line); Decline → `CallDecline`; the server's end of the ring
  (missed, cancelled) closes the card by editing it into a closed `agent.reply@1`
  ("📞 Missed call.", "📞 The call was cancelled."); a card the owner answered is not
  edited (the server refuses edits of answered cards; it shows the owner's choice). A
  local deadline (the ring's `expires_at` + 5 s, at most 2 minutes) declines if the
  server's end never comes. The ask card is not one of the agent's requests: no activity,
  no `openRequests`, no typed or voice answers. While asking, other calls are busy.
- **notify_by_call.** Config `{after_minutes (0 = off, at most 8), urgent, min_interval_minutes
  (10–1440, default 10), do_not_disturb (a string or a list)}`; §10.4's single
  `"22:00-07:00"` became ranges with optional days (`"mon-fri 22:00-07:00"`, `"sat,sun
  00:00-24:00"`) in the computer's local time zone (chrono `Local`, so daylight saving
  follows the OS); a range past midnight belongs to the day it starts on. `after_minutes`
  is capped at 8 because a card still expires after 10 minutes (`CARD_TIMEOUT`, unchanged;
  extending it during a call was left out). The floor of 10 minutes is the request's "at
  most 1 call per 10 minutes"; the library's `NotifyPolicy` takes any `Duration` (the E2E
  uses 3 s and 1 s). The section needs `calls.stt` and `calls.tts` when it can call.
  Checked on the core's 500 ms tick: a request (permission, question, plan; not `/mode
  bypass`) unanswered for `after` calls when no call runs, rings, is asked about or
  answered, the messenger is connected and Coda is not paused; held by the interval or
  the quiet hours (logged once per change, `call_held`) and placed later if the request
  is still open. Each request calls at most once (`OpenRequest::notified`); the interval
  counts from each placed call.
- **Urgent.** Coda's `notify_user` (`AgentMessageDto`) has no urgency field: the bridge
  reads `urgent: true`, `priority` `urgent`/`high` (forward compatible) or a label starting
  with "urgent". `BackendEvent::AgentMessage` gained `urgent`. Urgent messages fetched
  after a reconnect (`session/pendingMessages`) never call.
- **Placing the call.** `Phone::call(peer)` (new) runs push-sdk's `MediaCall` as
  `Role::Caller` through `MediaPhone`; the same `Session` pipeline as an answered call.
  Until the phone returns the call's id is unknown, so every call event but `Incoming`
  is buffered and replayed to the line. On connect the bridge says "Hi, it's <agent> in
  <folder>." plus "A request has waited N minutes for your answer." (or "I have something
  urgent.", the label and the text, masked), then the oldest open card's question;
  everything after is an ordinary call. An outgoing call that ends before connecting
  (missed, declined, busy, unreachable, failed) leaves the card open and posts one
  `agent.reply@1` note ("📞 I called about the open request (no answer). It is still open
  above."); a call still ringing is cancelled (`CallHangup` while ringing, no note) once
  nothing is waiting or from the tray. A phone error (offline, not available) is logged
  only. The activity label while it rings is `calling you` (an open card's `waiting`
  label wins, as always).
- **Control channel** (protocol 1, additive): `Status.call: CallInfo {phase: ringing |
  connecting | in_call, direction: incoming | outgoing, speaking, listening, muted,
  since}`, event `call_changed {call?}` (after the snapshot changes, as `state_changed`),
  requests `call_state` (from the snapshot), `mute {muted?: bool = true}` → `{muted}`,
  `hangup` → `{hungUp: true}`, error `no_call`; all classified content-free in the
  protocol tests. Mute silences the session (`Session::set_silenced`: queued speech
  dropped, nothing said until unmuted, the owner still heard); a mute during the answer
  applies once the line exists. Hang up: a running call hangs up, a ringing outgoing call
  is cancelled, an ask is declined, an answer in progress is dropped.
- **Tray.** push-tray-core keeps each agent's `call` from `status` and `call_changed`;
  the Overview shows it ("ringing (you are calling; confirm on your other device)",
  "calling you", "connecting", "in a call — speaking/listening", "(muted)"), the menu's
  agent line shows "calling you" / "your call is ringing" / "on a call" while a call
  rings or runs (icon and badge unchanged). New actions Mute agent / Unmute agent / Hang
  up (enabled when the control channel is up, the bridge is not stopping, and for mute a
  call is connecting or in a call; hang up for any call).
- **Tests.** Policy units (`voice/policy.rs`: midnight, days, wrapping day ranges, time
  zones with fixed offsets, the verdict order quiet hours before interval), config
  validation by field, Coda urgency parsing, the fake phone's placed calls, and 12 core
  tests in paused time (`core/voice_tests.rs`). E2E `the_bridge_asks_before_answering_and_calls_its_owner`
  in `crates/push-smoke/tests/bridge_voice.rs` (job `bridge-voice`, ring 10 s): ask mode,
  the agent calling after 3 s with the owner answering as `Role::Callee`, hearing the
  summary and approving by voice, then a missed call whose card still takes the answer.