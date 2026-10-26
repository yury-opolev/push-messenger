# Live activity and presence

REQUEST PUSH feature 12. **Activity** tells the other members of a conversation what a user
(or an agent) is doing there right now: typing, working, waiting, or idle again, with an
optional short label. **Presence** tells a user's accepted contacts whether the user is
online, or when they were last seen. Neither is stored as a message: activity is never
stored at all, and presence follows live connections. Design:
[spec 2026-10-26](superpowers/specs/2026-10-26-activity-presence-design.md). Where this
guide and the spec differ, this guide describes what the code does.

- [1. Overview](#1-overview)
- [2. Wire frames](#2-wire-frames)
- [3. Activity](#3-activity)
- [4. Presence](#4-presence)
- [5. Multi-replica design](#5-multi-replica-design)
- [6. SDK and bindings](#6-sdk-and-bindings)
- [7. Agents](#7-agents)
- [8. Compatibility and upgrade](#8-compatibility-and-upgrade)

## 1. Overview

| | Activity | Presence |
|---|---|---|
| Scope | one conversation | one user (all devices, every gateway replica) |
| Who hears it | the conversation's other current members | accepted contacts (both ways), neither side opted out |
| Lifetime | 30 s unless sent again; `idle` ends it | online while a connection holds a lease; offline after a 10 s grace |
| How it arrives | `ActivityEvent` (server frame 42) | `PresenceEvent` (server frame 43), and `GetPresence` reads |
| Stored | never | the opt-out only (`messaging.presence_hidden`) |

Both are signals, like receipts: a lost one only delays the client. Activity is sent again
by the sender anyway, and presence can be read again with `GetPresence`.

```
client ──Activity──▶ gateway ──Messaging.Activity──▶ message-svc ──PUBLISH notify:{member}──▶ gateways ──ActivityEvent──▶ members
gateway (connect / refresh / close) ──Lua on presence:{user}:*──▶ Redis
gateway (on a transition) ──Messaging.AnnouncePresence──▶ message-svc ──reads Redis, PUBLISH notify:{contact}──▶ PresenceEvent
client ──Request GetPresence / SetPresenceSharing──▶ gateway ──Messaging.Request──▶ message-svc (visibility in the database, state in Redis)
```

## 2. Wire frames

All in `crates/push-proto/proto/push/v1/frame.proto`. Ids are 16-byte UUIDs, as everywhere.
Activity and presence are frames of their own; the reserved `ephemeral` field (16) of
`Send`/`Deliver` stays reserved for ephemeral (disappearing) messages.

| Frame | Direction | `Frame.body` / `Request.body` / `Response.body` field |
|---|---|---|
| `Activity` | client → server | `Frame` 10 |
| `ActivityEvent` | server → client | `Frame` 42 |
| `PresenceEvent` | server → client | `Frame` 43 |
| `GetPresence` | request | `Request` 20, answered by `PresenceList` (`Response` 11) |
| `SetPresenceSharing` | request | `Request` 21, answered by `PresenceSharing` (`Response` 12) |

### `Activity` (client → server, 10)

| # | Field | Type | Meaning |
|---|---|---|---|
| 1 | `conversation_id` | `bytes` | a conversation the sender is a current member of |
| 2 | `state` | `ActivityState` | not `UNSPECIFIED` |
| 3 | `label` | `optional string` | short plain text ([§3](#label)); empty or absent = none; dropped for idle |

Nothing answers a good one. A refused one is answered with an `Error` without
`client_msg_id` or `request_id` ([§3](#errors)).

### `ActivityEvent` (server → client, 42)

| # | Field | Type | Meaning |
|---|---|---|---|
| 1 | `conversation_id` | `bytes` | where |
| 2 | `user_id` | `bytes` | who (never the receiving user: a user does not hear their own activity) |
| 3 | `state` | `ActivityState` | |
| 4 | `label` | `optional string` | absent for idle and when the sender gave none |
| 5 | `ttl_ms` | `uint32` | 30000; 0 for idle |

### `PresenceEvent` (server → client, 43) and `Presence`

`PresenceEvent { Presence presence = 1; }` — a contact's presence changed, or became
visible or invisible.

| # | `Presence` field | Type | Meaning |
|---|---|---|---|
| 1 | `user_id` | `bytes` | whose |
| 2 | `state` | `PresenceState` | |
| 3 | `last_seen` | `optional int64` | `OFFLINE` only, when known: microseconds since the Unix epoch, a whole minute |

### `GetPresence` (request 20) → `PresenceList` (response 11)

| Message | # | Field | Type |
|---|---|---|---|
| `GetPresence` | 1 | `user_ids` | `repeated bytes` — 1 to 100 ids (duplicates count towards the 100) |
| `PresenceList` | 1 | `presences` | `repeated Presence` — one per distinct id, in request order |

### `SetPresenceSharing` (request 21) → `PresenceSharing` (response 12)

| Message | # | Field | Type |
|---|---|---|---|
| `SetPresenceSharing` | 1 | `share` | `optional bool` — absent: only read the setting |
| `PresenceSharing` | 1 | `share` | `bool` — the setting now |

### Enums

| `ActivityState` | | `PresenceState` | |
|---|---|---|---|
| `ACTIVITY_STATE_UNSPECIFIED` | 0 (refused) | `PRESENCE_STATE_UNKNOWN` | 0 — not visible |
| `ACTIVITY_STATE_TYPING` | 1 | `PRESENCE_STATE_ONLINE` | 1 — at least one device connected |
| `ACTIVITY_STATE_WORKING` | 2 | `PRESENCE_STATE_OFFLINE` | 2 — optional `last_seen` |
| `ACTIVITY_STATE_WAITING` | 3 | | |
| `ACTIVITY_STATE_IDLE` | 4 | | |

## 3. Activity

### Lifetime

- Every non-idle `ActivityEvent` carries `ttl_ms` 30000. A client expires it locally at
  **receive time + `ttl_ms`**, on its own clock, so clock skew between devices and servers
  never matters. A newer event from the same user and conversation replaces it.
- A sender refreshes its activity every **10 to 20 s** while the state lasts.
- `idle` clears at once: it carries `ttl_ms` 0 and never a label.
- **Idle on disconnect.** When a connection ends, the gateway sends `idle` to every
  conversation where that connection's last accepted activity is unexpired and not idle
  (so a crashed agent process does not show "working" for the rest of the 30 s). Not when
  the connection was closed by a **takeover** (the same device connected again): the newer
  connection keeps its own activity fresh, and an idle from the old one would overwrite it.

### Who hears it

The conversation's **current members other than the sender**, on all their connected
devices. The sender's own other devices do not hear it. Members with a block in either
direction with the sender are skipped silently, in groups too: no block is ever revealed.
When nobody may hear it, nothing is published (and the sender is not told).

### Rate limit

Per device (a device has at most one live connection: a takeover closes the older one), a
token bucket: **burst 2, refill 1 per second**. Extra frames are **not errors**: the latest
waiting activity per conversation is kept (a newer one replaces it in place) and sent when
a token frees up (trailing edge), so the final state always arrives and the fan-out never
exceeds the rate. While anything waits, new activities queue behind it in order of first
arrival, so a newer one never overtakes an older one. At most **16** conversations wait; an
activity for a 17th is dropped silently. The general per-connection frame budget (close
4004) still applies on top.

### Label

- Optional. Absent or **empty means no label**. Otherwise 1 to **60 characters** (Unicode
  scalar values), not only whitespace (whitespace-only is `INVALID`). The label is sent and
  shown as given: apps should trim it before sending.
- The rules of template values ([rich-messages.md §5](rich-messages.md#5-message-bodies-and-card-state)):
  no control characters (no line breaks either), no U+2028/U+2029, no invisible,
  formatting or bidi characters.
- URLs and markup are **allowed** and must be **rendered inert**, as plain text (never
  linkified or interpreted), as for template values filled by a sender.
- `idle` drops the label. The SDK drops it before checking it; the server still refuses a
  malformed one, whatever the state.

### Errors

An `Error` frame without ids that is **never retryable** (`retryable` false, no
`retry_after_s`): SDKs read an anonymous retryable error as "send `ReAuth` again", and
activity is refreshed anyway. The SDK only logs them.

| Code | When |
|---|---|
| `ERROR_CODE_INVALID` | a bad label, `UNSPECIFIED` (or unknown) state, a malformed conversation id |
| `ERROR_CODE_CONVERSATION_NOT_FOUND` | no such conversation |
| `ERROR_CODE_NOT_MEMBER` | the sender is not a current member (never joined, left, removed) |
| `ERROR_CODE_UNAVAILABLE` | message-svc failed or is unreachable (also an old message-svc without the `Activity` call) |

A block never produces an error.

## 4. Presence

### States

- **Online** while at least one connection of the user (any device, any gateway replica)
  holds a lease. A live connection's lease lasts **90 s** and is renewed every **30 s**.
- A closed connection keeps its lease for a **grace** of 10 s
  (`PUSH_PRESENCE_GRACE_MS`, gateway, 0 to 120000, default 10000), so a reconnect within it
  (a network blip, a gateway rollout) is never shown offline. Offline is announced once the
  last lease has ended.
- **`last_seen`** is the minute the user was last known online (written on connect, on
  every lease renewal and on disconnect, truncated to a whole minute), and is shown only
  with `OFFLINE`.

### Visibility

A viewer sees a user's presence when both contact rows are **`ACCEPTED`** and **neither**
side switched sharing off. A user always sees their own presence.

Everything else is `PRESENCE_STATE_UNKNOWN` without `last_seen`: a stranger, a pending
request, a block in either direction, an opt-out on either side, an id that is no user.
These cases are **indistinguishable** on purpose.

- **Opt-out** (`SetPresenceSharing{share: false}`, persisted in message-svc) is
  **reciprocal**: a user who hides cannot see anyone else's presence either (no lurking).
  Contacts are pushed `UNKNOWN` at once when it is switched off and the current state when
  it is switched back on. Nothing is pushed to the user who switches: an app clears the
  presence it holds when switching off, and reads it again (`GetPresence`) when switching
  on. Sharing is on by default. Activity is not affected by the opt-out (it is explicit
  and per conversation).
- **Block.** Blocking an accepted contact ends the contact, so presence becomes invisible
  both ways: each side is pushed `UNKNOWN` for the other, the same event an opt-out gives,
  so the blocked user learns nothing new.
- A contact that becomes accepted (or an unblock) pushes no presence: read it with
  `GetPresence` after the `ContactEvent`.

### Push on change

On a transition (offline → online, online → offline, sharing switched, a block)
message-svc publishes a `PresenceEvent` on `notify:{contact}` for every contact allowed to
see it. Redis drops it for contacts without a connected gateway, so only online contacts
receive it; the others read presence when they connect. Events are not replayed: read the
contacts' presence with `GetPresence` after connecting and after a reconnect.

### Batch read

`GetPresence{user_ids}`: **1 to 100 ids** (none, or more than 100 counting duplicates, is
`INVALID`; so is a malformed id). The answer has **one `Presence` per distinct id, in
request order**. Rate limited per user: **120 per minute** across the user's devices
(`ERROR_CODE_RATE_LIMITED` with `retry_after_s`), like the other probing requests.

### Failure modes

- **Redis down or slow** (a read times out after 500 ms): visible users read `OFFLINE`
  without `last_seen` (the others still `UNKNOWN`), and **nothing is pushed**: a guess
  would tell contacts a wrong state. Switching sharing on still changes the setting but
  pushes nothing. Messaging is unaffected; gateways log lease failures and keep the
  connection. Once Redis answers again, a gateway announces the user it was in doubt about.
- **Redis flushed**: `last_seen` is lost; leases are written again within 30 s.
- **A gateway crashes**: its leases are not ended, they expire within 90 s. The first read
  of such a user after that (a `GetPresence`, or another announcement) finds no lease
  while the user is still marked online, clears the mark and announces offline, once. A
  sweeper that does this without a read is a follow-up.
- **A gateway shuts down**: closed connections are not settled (reconnects normally land
  on another replica within the grace); reads stay correct.
- **`AnnouncePresence` fails**: logged; the next transition or read corrects the state.

## 5. Multi-replica design

Every gateway replica writes leases to the shared Redis; message-svc reads them and fans
out through the existing `notify:{user}` pub/sub, so any replica's connections hear any
other's changes. message-svc and the gateways must use the same Redis (as for
notifications already).

### Redis keys

Both keys of a user share a hash tag, so every script touches one cluster slot:

| Key | Type | Content |
|---|---|---|
| `presence:{<user uuid>}:conns` | sorted set | member = connection id, score = lease end (ms, Redis `TIME`); expires with its longest lease, so a dead gateway's set disappears |
| `presence:{<user uuid>}:state` | hash | `on` (`1` while announced online), `seen` (last-seen minute, unix seconds); 30-day TTL, refreshed on every write |

### Scripts

All run with `EVALSHA` (loaded once, reloaded on `NOSCRIPT`) and use Redis `TIME`, never a
replica's clock.

| Script | Who, when | What |
|---|---|---|
| **up** | gateway: connect, then every 30 s | prune ended leases, remember whether any was left, add or renew this connection's 90 s lease, set `seen` and `on=1`; returns 1 when no lease was left before (announce online) |
| **down** | gateway: connection closed | if the connection has a lease, end it after the grace; set `seen` |
| **settle** | gateway: grace + 100 ms after down | prune; when no lease is left and `on=1`, set `on=0` and return 1 (announce offline) |
| **read** | message-svc: per user, in one pipeline | prune; when no lease is left and `on=1`, set `on=0` and report the flip (a crashed gateway's user); return online and `seen` |

Each connection has one presence task in the gateway that runs up, renewals, down and
settle in order, so they cannot reorder. A script whose outcome is unknown (Redis failed or
timed out, or the connection dropped and the script ran again) is announced anyway on
connect and settle (announcing is idempotent); a failed renewal is only remembered and
announced with the next answer, so a Redis outage causes no announcement storm.

### `AnnouncePresence`

Internal gRPC `Messaging.AnnouncePresence{user_id}` (`push/internal/v1/messaging.proto`):
the gateway calls it on a transition (2 s timeout); message-svc takes the user's lock,
computes the audience (accepted contacts that share), reads the state with the read script
inside the lock and publishes `Notification.presence_changed` (6). Holding the lock
serializes every publication of a user's presence (announcements, opt-out, block), so a
hide or block is never followed by a stale `ONLINE`, and two announcements cannot overtake
each other. Activity goes through `Messaging.Activity` and
`Notification.activity_changed` (5). The gateway hub turns both into `ActivityEvent` and
`PresenceEvent`.

## 6. SDK and bindings

The SDK never refreshes activity by itself: the app or agent does. Activity is fire and
forget and needs a live connection (it is stale by the time a connection comes up).

### Rust (`push-sdk`)

```rust
use push_sdk::{ActivityState, Event, PresenceState};

// Tell the other members; Err(SdkError::Offline) unless connected, Invalid for a bad label.
client
    .send_activity(conversation, ActivityState::Working, Some("running tests"))
    .await?;
client.send_activity(conversation, ActivityState::Idle, None).await?;

// 1 to 100 users, one per distinct user in the order asked.
for presence in client.get_presence(&contacts).await? {
    match presence.state {
        PresenceState::Online => { /* online */ }
        PresenceState::Offline => { /* presence.last_seen_ms: Option<i64>, a whole minute */ }
        PresenceState::Unknown => { /* not visible: show nothing */ }
    }
}

let sharing: bool = client.presence_sharing().await?;
client.set_presence_sharing(false).await?; // reciprocal: everyone reads Unknown now

while let Some(event) = events.next().await {
    match event {
        // Show until activity.expires_at_ms (this device's clock); Idle: clear now.
        Event::ActivityChanged(activity) => { /* conversation_id, user_id, state, label */ }
        Event::PresenceChanged(presence) => { /* user_id, state, last_seen_ms */ }
        _ => {}
    }
}
```

### C

```c
PushError *error = NULL;
push_client_send_activity(client, conversation_id, "working", "running tests", &error);
push_client_send_activity(client, conversation_id, "idle", NULL, &error);

const char *ids[] = {peer_id};
char *presences_json = NULL;  /* a JSON array of Presence; free with push_string_free */
push_client_get_presence(client, ids, 1, &presences_json, &error);

bool sharing = true;
push_client_presence_sharing(client, &sharing, &error);
push_client_set_presence_sharing(client, false, &sharing, &error);
```

### Node

```js
await client.sendActivity(conversationId, 'working', 'running tests');
await client.sendActivity(conversationId, 'idle');
const presences = await client.getPresence([peerId]); // [{ userId, state, lastSeenMs }]
await client.setPresenceSharing(false);              // resolves to the setting now
const sharing = await client.presenceSharing();
```

### Events (JSON, C and Node)

Fields ending in `Ms` are **milliseconds** since the Unix epoch (other times in the
bindings are microseconds).

```json
{"type": "activityChanged", "activity": {
  "conversationId": "0199e1c2-0000-7000-8000-0000000000a1",
  "userId": "0199e1c2-0000-7000-8000-000000000001",
  "state": "working", "label": "running tests", "expiresAtMs": 1793045250553}}
{"type": "presenceChanged", "presence": {
  "userId": "0199e1c2-0000-7000-8000-000000000001",
  "state": "offline", "lastSeenMs": 1793045220000}}
```

`state` is `typing`, `working`, `waiting` or `idle` for activity, and `online`, `offline`
or `unknown` for presence; `label` and `lastSeenMs` are `null` when absent. The full
binding reference is [sdk-bindings.md](sdk-bindings.md#activity-and-presence).

## 7. Agents

### Recipe

- While busy, send `working` with a label of what it is doing ("running tests",
  "reading the diff") about **every 15 s**, and again at once when the label changes. The
  rate limit keeps only the latest per conversation, so a burst of label changes is
  harmless.
- While a card waits for the user's answer, send `waiting` (for example "approval
  needed") on the same cadence.
- When done (the reply is sent, the card answered), send `idle`.
- If the agent crashes or loses its connection, the gateway sends `idle` for it, and its
  presence turns offline after the grace. If it hangs with its connection open, its
  activity simply expires 30 s after the last refresh.

```rust
let mut refresh = tokio::time::interval(std::time::Duration::from_secs(15));
loop {
    tokio::select! {
        _ = refresh.tick() => {
            // Ignore Offline: the next tick after a reconnect sends it again.
            let _ = client
                .send_activity(conversation, ActivityState::Working, Some("running tests"))
                .await;
        }
        result = &mut job => {
            let _ = client.send_activity(conversation, ActivityState::Idle, None).await;
            break result;
        }
    }
}
```

### Migrating from the heartbeat edit

Consumers that edited the agent's latest message every 15 s to signal "still working"
can drop that: it wrote history only to signal activity, could not show online or
offline, and noticed a crashed agent only after 45 s. Instead:

| Before | Now |
|---|---|
| edit the latest message every 15 s with a heartbeat | `send_activity(working, label)` every ~15 s |
| "the agent is waiting" derived from an open card | `send_activity(waiting, label)` while the card is open |
| the heartbeat stops → stale after 45 s | `idle` when done; idle on disconnect; local expiry 30 s after the last refresh |
| no online/offline | `get_presence` on start and reconnect, then `presenceChanged` (the agent must be an accepted contact) |

Keep the edits for real content changes only; history then holds only what the agent said.

## 8. Compatibility and upgrade

- **Additive protocol**: new frame and field numbers only. Old clients ignore the new
  server frames: an unknown `oneof` field decodes as a frame without a body, which the SDK
  logs ("an unreadable frame") and drops. Old gateways ignore the new notifications.
- **Deploy order**: message-svc first (it runs messaging migration
  `0007_presence_hidden.sql`, a new table `messaging.presence_hidden`; no existing data
  changes), then the gateways. A new gateway in front of an old message-svc answers
  `Activity` with `UNAVAILABLE`, gets `INVALID` for the presence requests (the old service
  sees a request without a body), and its announcements fail (logged).
- Rolling message-svc back after 0007 is not supported (sqlx refuses a database with
  migrations the binary does not know), as for earlier migrations.
- New gateway setting `PUSH_PRESENCE_GRACE_MS` (default 10000, 0 to 120000; chart
  `gateway.presenceGraceMs`). No new secret; the gateway uses the Redis it already
  publishes to.
