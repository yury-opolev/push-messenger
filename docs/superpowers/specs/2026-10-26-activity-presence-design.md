# Live activity and presence signals — design

REQUEST PUSH feature 12 (consumer yuopolev/push-messenger, against upstream d36e398).
Owner away: every open question below was decided autonomously and is recorded here.

## Problem

Users want a chat partner's live state, especially an agent's: typing, working, waiting
for you, idle, online or offline. The consumer derives it from the agent's latest message,
edited every 15 s as a heartbeat: that writes history just to signal activity, cannot show
online or offline, and notices a crashed agent only after 45 s.

## Goals

1. **Activity**: a client says `typing | working | waiting | idle` (with an optional short
   label) in one conversation; the other members' connected devices hear it at once. Not
   stored, expires after a TTL unless refreshed, rate limited per device.
2. **Presence**: a user is online while at least one of their devices has a live socket,
   otherwise offline with a coarse `last_seen`. Visible only to accepted contacts, pushed
   on change, readable for a batch of users, and switchable off.
3. **Agents** use the same frames (`working` + "running tests", `waiting` while a card is
   open).

Non-goals: per-device presence, custom status messages, read-only "invisible but can see
others" mode, durable activity history, push notifications (APNs/FCM) for presence.

## Decisions

| Topic | Decision |
|---|---|
| Wire shape | New frames, not the reserved `ephemeral` field. `ephemeral` (16) in `Send`/`Deliver` stays reserved for ephemeral *messages* (disappearing content, a different feature); activity is not a message and has no seq. Client frame `Activity` = 10, server frames `ActivityEvent` = 42 and `PresenceEvent` = 43, requests `GetPresence` = 20 and `SetPresenceSharing` = 21 (answered by `PresenceList` = 11 and `PresenceSharing` = 12; `SetPresenceSharing` without a value reads the setting). |
| Activity TTL | 30 s (`ttl_ms` = 30000 in every `ActivityEvent`). Clients expire it locally at *receive time + ttl_ms* (no clock-skew dependence). Senders refresh every 10–20 s while the state lasts. `idle` clears at once and carries `ttl_ms` 0. |
| Idle on disconnect | When a connection ends, the gateway publishes `idle` for every conversation where that connection's last activity is still unexpired and not idle. |
| Fan-out targets | The conversation's **current members other than the sender** (all their connected devices). The sender's own other devices do not hear it (the request says "other members"; a user knows what they are doing). Members with a block in either direction with the sender are skipped silently, in groups too. |
| Membership | The sender must be a current member; otherwise the usual `CONVERSATION_NOT_FOUND`/`NOT_MEMBER` error frame comes back. Blocks never produce an error (no block is revealed). |
| Label | Optional, trimmed by the client; 1–60 characters (Unicode scalar values) when present; same value rules as template param values (`check_value`, `docs/rich-messages.md` §5): no control characters (no `\n` either), no U+2028/U+2029, no invisible/formatting/bidi characters. URLs and markup are *allowed*, rendered as inert plain text — consistent with the templates' policy for sender-filled values. An empty string means no label; whitespace-only is `INVALID`. `idle` never carries a label (dropped). |
| Rate limit | Per device (= per connection: a device has at most one live connection, takeover closes older ones) token bucket: burst 2, refill 1 per second. Excess frames are **not** errors: the latest excess activity per conversation is kept (newest wins) and sent when a token frees up (trailing edge), so the final state always arrives and fan-out never exceeds the rate. At most 16 conversations wait; beyond that the frame is dropped silently. The general frame budget (close 4004) still applies on top. |
| Activity errors | `Error` frame without ids, **never retryable** (an anonymous retryable error means "send ReAuth again" to SDKs; activity is refreshed anyway): `INVALID` (bad label, unspecified state, bad id), `CONVERSATION_NOT_FOUND`, `NOT_MEMBER`, `UNAVAILABLE`. |
| Presence states | `PRESENCE_STATE_UNKNOWN` (0) / `ONLINE` (1) / `OFFLINE` (2, optional `last_seen`). "Not visible" — stranger, pending, blocked either way, opted out, the viewer opted out, unknown id — is always `UNKNOWN` with no `last_seen`, so none of these is distinguishable from the others. |
| Aggregation | Online while at least one connection (any device, any gateway replica) holds a lease. |
| Debounce | A closed connection keeps its lease for a 10 s grace period (`PUSH_PRESENCE_GRACE_MS`, default 10000). A reconnect within the grace (network blip, gateway rollout) never shows offline. Offline is announced once the last lease ends. |
| last_seen | The minute (unix micros truncated to a whole minute) the user was last known online: written on connect, every lease refresh and disconnect. Only shown with `OFFLINE`. |
| Visibility | Both directions `ACCEPTED`, neither side opted out. The user always sees their own presence. |
| Opt-out | `SetPresenceSharing{share:false}`, persisted in message-svc (`messaging.presence_hidden`, one row per opted-out user). Contacts see `UNKNOWN` (pushed at once when switching off; the current state pushed when switching on). **Reciprocal**: a user who hides cannot see anyone else's presence either (no lurking). Activity is not affected by the opt-out (it is explicit and per-conversation). |
| Blocks | A block ends the accepted contact, so presence becomes invisible both ways. On a block of an accepted contact, each side is pushed `UNKNOWN` for the other — the same event an opt-out gives, so the blocked user learns nothing new. |
| Push on change | On a transition (offline→online, online→offline, sharing switched) message-svc publishes a `PresenceEvent` on `notify:{contact}` for every eligible contact. Redis drops it for contacts with no connected gateway, so only online contacts receive it. |
| Batch read | `GetPresence{user_ids}`: 1–100 ids (more is `INVALID`), duplicates collapsed, one `Presence` per distinct id in request order. Rate limited per user (120 per minute) like other probing requests. |
| Multi-replica | Shared state in Redis (below); every gateway replica writes leases, message-svc reads them and fans out through the existing `notify:{user}` pub/sub, so any replica's connections hear any other's changes. |

## Architecture

```
client ──Activity──▶ gateway ──Messaging.Activity──▶ message-svc ──PUBLISH notify:{member}──▶ gateways ──ActivityEvent──▶ members
gateway (connect/refresh/close) ──Lua on presence:{u}:*──▶ Redis
gateway (on transition) ──Messaging.AnnouncePresence──▶ message-svc ──reads Redis, PUBLISH notify:{contact}──▶ PresenceEvent
client ──Request GetPresence / SetPresenceSharing──▶ gateway ──Messaging.Request──▶ message-svc (DB visibility + Redis state)
```

### Redis presence state (shared by every replica)

Keys use a hash tag so both keys of a user share a slot (cluster-safe scripts):

- `presence:{<user uuid>}:conns` — sorted set: member = connection id, score = lease end
  (Redis `TIME`, ms). PEXPIRE'd to the lease so a dead gateway's set disappears.
- `presence:{<user uuid>}:state` — hash: `on` (`1` while announced online), `seen`
  (last-seen minute, unix seconds). 30-day TTL, refreshed on every write.

Scripts (all use Redis `TIME`, never a replica's clock):

- **up** (connect, and every 30 s while connected): prune expired members, remember
  whether any live member was left, add/refresh this connection with a 90 s lease, set
  `seen`, set `on=1`. Returns 1 when the user was offline before (announce online).
- **down** (connection closed): if the member exists, set its lease to now + grace; set
  `seen`.
- **settle** (grace after down, by the same gateway): prune; if no member is left and
  `on=1`, set `on=0` and return 1 (announce offline).
- **read** (message-svc, per user): prune; if no member is left and `on=1`, set `on=0`
  and report the flip, so a crashed gateway's users are announced offline by the first
  read after their leases ran out; return online?, `seen`. A read whose outcome is in
  doubt (sent again after a dropped connection, or failed or timed out after it was
  sent) may have flipped someone without reporting it, so `GetPresence` announces every
  user it covered from a fresh read under the user's lock (nothing when that fails too).

Each connection has one presence task in the gateway that runs up, refresh, down and
settle in order, so they cannot reorder. On gateway shutdown the settle step is skipped
(reads stay correct; reconnects normally land within the grace).

Redis is "a doorbell, not the truth" (spec §2): if Redis is lost, presence reads `OFFLINE`
without `last_seen` (or `UNKNOWN` when not visible) and nothing is pushed; messaging is
unaffected. If Redis is flushed, `last_seen` is lost.

### message-svc

- Domain (`push-domain`, `presence.rs`, `activity.rs`):
  - `Messaging::activity(sender, conversation, state, label)`: validates, checks current
    membership, picks recipients (other current members without a block either way),
    publishes `Event::Activity { to, conversation, user, state, label, ttl }`.
  - `presence_viewable(viewer, candidates)`, `presence_audience(user)`,
    `presence_sharing(user)`, `set_presence_sharing(user, share, current)`,
    `announce_presence(user, current)`; `Event::Presence { to, user, view }`. `current`
    is an async source of the transport's view (message-svc reads Redis).
  - `contact_block` additionally publishes `Event::Presence` UNKNOWN both ways when the
    pair were accepted contacts.
  - Publications of a user's presence are serialized by the user's lock
    (`Tx::lock_users`, taken last, after the pair lock in `contact_block`): each takes
    it, computes the audience, reads `current` inside the lock and publishes before its
    transaction ends, so a hide or block is never followed by a stale ONLINE and two
    announcements cannot overtake each other.
  - Store: `presence_hidden(users) -> hidden subset`, `set_presence_hidden(user, hidden)`;
    in memory and PostgreSQL (migration `0007_presence_hidden.sql`).
- gRPC: `Messaging.Activity(ActivityRequest) -> ActivityResponse`,
  `Messaging.AnnouncePresence(AnnouncePresenceRequest) -> AnnouncePresenceResponse`;
  `GetPresence` / `SetPresenceSharing` through `Messaging.Request`.
- `RedisPresence` (read script) behind a `PresenceStates` trait (fake in tests).
- Notifications: `Notification.activity_changed` (5) and `presence_changed` (6), turned by
  the gateway hub into `ActivityEvent` / `PresenceEvent`.

### gateway

- `Session` maps `Activity` to `Step::Activity`.
- `ActivityLimiter` (per connection): token bucket + per-conversation pending latest;
  a select-loop timer flushes it; the connection remembers unexpired non-idle activity
  per conversation for idle-on-disconnect.
- `presence.rs`: the Lua scripts and the per-connection presence task.
- `Action::Presence` limit for `GetPresence`.

### SDK (`push-sdk`) and bindings

- `Client::send_activity(conversation, ActivityState, Option<&str>)` — fire and forget
  while connected (returns `Offline` otherwise); validates the label locally.
- `Client::get_presence(&[UserId]) -> Vec<Presence>`,
  `Client::set_presence_sharing(bool)`, `Client::presence_sharing() -> bool`.
- Events `Event::ActivityChanged(Activity)` (with `expires_at_ms`, local clock) and
  `Event::PresenceChanged(Presence)`.
- C and Node bindings: the same calls and the events in their JSON form.

## Error handling

- Activity RPC failure (message-svc down): `UNAVAILABLE` error frame, not retryable.
- Presence Redis failures in the gateway are logged and ignored (fail open: the
  connection continues; presence may be stale until the next refresh).
- `AnnouncePresence` failures are logged; the next transition or read corrects state.

## Testing

- Domain unit tests (memory store, recording notifier): recipients exclude the sender,
  non-members, blocked pairs; labels; viewable/audience/opt-out/reciprocity; block push.
- Store contract test for `presence_hidden` on memory and PostgreSQL.
- message-svc: request handling for GetPresence/SetPresenceSharing with a fake states
  source; Redis read script against real Redis.
- Gateway integration (real Redis, in-memory store), the acceptance list:
  activity reaches the other members and not the sender's other conversations; expires
  (ttl in the event, idle on disconnect); rate limit holds (burst then trailing latest);
  presence flips with connect and disconnect across all devices (with a short grace);
  a non-contact sees `UNKNOWN`; opt-out hides presence.
- SDK: engine handling of the new frames; request round trips with the fake gateway;
  binding JSON for the new events.

## Rollout

Additive protocol (new frame numbers). Old clients ignore unknown frames (prost skips an
unknown oneof field: the frame decodes with no body and the SDK drops it). Deploy
message-svc (migration 0007) before gateways; a gateway calling an old message-svc gets
`UNIMPLEMENTED` for Activity/AnnouncePresence, answered as `UNAVAILABLE`.

## Follow-ups

- Sweeper that announces offline for crashed gateways without waiting for a read.
- Per-device presence and "last active" for agents; typing indicators in push payloads.
