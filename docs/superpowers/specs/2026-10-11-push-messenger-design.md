# push-messenger — v1 design

Date: 2026-10-11 (revised after independent review)
Status: pending written-spec review

## 1. Purpose and scope

push-messenger is a minimal, expandable messenger **backend plus client SDK**. We build
the server side, its API and an SDK; third parties build the UIs. It must stay simple
and performant.

**Requirements**

1. Kubernetes (k3s on one Azure VM to start), scaling out horizontally later.
   Target: 10,000+ concurrent connections, load-tested.
2. Rust.
3. Users register with email and password. Email must be verified before API use.
4. The service can send email to users.
5. One-to-one chats in v1; the design supports groups of up to 100 members.
6. Every device is registered explicitly.
7. Messages are stored on the server. New devices can load history.
8. Users can connect to each other easily.
9. Users can edit and delete their own messages within a time limit.

**In v1:** requirements 1–9. Groups are fully designed and modelled; whether group
endpoints ship in the first release or the second is an implementation-plan decision.

**Out of scope, but not precluded:** media (images, documents, audio), end-to-end
encryption, push notifications, typing indicators, email change, "delete for me",
per-member read lists in groups.

Research behind these decisions: `docs/research/2026-10-11-prior-art.md` and
`docs/research/2026-10-11-email-providers.md`.

## 2. Architecture

Four services and two stores on one k3s node, behind ingress-nginx.

| Unit | Role | Client connections | Database | Scales on |
|---|---|---|---|---|
| **gateway** | Terminates WebSockets, verifies JWTs locally, decodes frames, forwards them to message-svc over gRPC, streams results back, listens for wake-up signals | yes | **none** | connection count |
| **message-svc** | Conversations, members, contacts, invites, the message log, sequence numbers, delivery and read positions, edits and deletes | no | `messaging` schema | throughput |
| **auth-svc** | Signup, email verification, login, device registration, tokens, handles, profiles, JWKS | no | `auth` schema | stateless |
| **worker** | Drains service outboxes: sends email, relays events between services, runs cleanup jobs | no | `worker` schema, plus read/delete on outboxes | stateless |
| **PostgreSQL** | System of record | — | — | partition by `conversation_id` later |
| **Redis** | Wake-up signals, kick signals and rate limits. Never message bodies | — | — | — |

**Key properties**

- **The gateway is transport only.** It has no database credentials and no business
  rules. It is the only component exposed to untrusted input, so compromising it yields
  live sockets and nothing else.
- **Postgres is the truth; Redis is a doorbell.** Losing a Redis signal delays delivery
  but never loses a message: the gateway re-syncs on reconnect, whenever its Redis
  subscription is re-established, and in a slow sweep every 5 minutes.
- **Auth is off the hot path.** The gateway verifies JWTs against cached JWKS, so a
  reconnect storm is bounded by our own capacity, not by auth-svc.
- **One writer per conversation.** Every change to a conversation's log takes a row lock
  on that conversation, however many message-svc replicas run.

**Service boundaries.** Each service owns one Postgres schema and never reads another's.
The single sanctioned exception is the **outbox**: a service writes events to its own
`outbox` table in the same transaction as the change, and the worker reads and deletes
them. This is how auth-svc sends email and tells message-svc about users and devices.

**Public endpoints:** `wss://api.<domain>/v1/socket` (gateway) and
`https://api.<domain>/v1/auth/*`, `/v1/devices/*`, `/v1/users/*` (auth-svc).

## 3. Data model

### 3.1 auth schema (auth-svc)

```sql
user_account (
  user_id           uuid primary key,
  email             citext unique not null,
  email_verified_at timestamptz,            -- null = no API access
  handle            citext unique not null, -- public @handle, no email exposed
  display_name      text not null,
  password_hash     text not null,          -- Argon2id PHC string
  created_at        timestamptz not null default now()
)

device (
  device_id    uuid primary key,
  user_id      uuid not null references user_account,
  name         text not null,               -- "Yuri's iPhone"
  platform     text not null,               -- ios | android | web | desktop | other
  created_at   timestamptz not null default now(),
  last_seen_at timestamptz,
  revoked_at   timestamptz
)

refresh_token (
  token_hash bytea primary key,             -- SHA-256; raw token never stored
  device_id  uuid not null references device,
  expires_at timestamptz not null,
  revoked_at timestamptz,
  rotated_to bytea                          -- reuse detection
)

outbox (                                    -- read and deleted by the worker
  event_id   uuid primary key,
  kind       text not null,                 -- email.send | user.verified |
                                            -- device.registered | device.revoked
  payload    jsonb not null,
  created_at timestamptz not null default now()
)
```

### 3.2 messaging schema (message-svc)

```sql
known_user (                               -- mirrored from auth via user.verified
  user_id    uuid primary key,
  created_at timestamptz not null default now()
)

member_device (                            -- mirrored from auth via device events
  device_id  uuid primary key,
  user_id    uuid not null references known_user,
  revoked_at timestamptz
)

contact (                                  -- one row per direction
  owner_id   uuid not null references known_user,
  peer_id    uuid not null references known_user,
  state      smallint not null,            -- requested_out | requested_in |
                                           -- accepted | blocked
  updated_at timestamptz not null default now(),
  primary key (owner_id, peer_id)
)

invite (
  code       text primary key,             -- short random code, also used for QR
  user_id    uuid not null references known_user,
  expires_at timestamptz not null,
  max_uses   int not null default 1,
  uses       int not null default 0
)

conversation (
  conversation_id uuid primary key,
  kind            smallint not null,       -- 1 = direct, 2 = group
  direct_key      bytea unique,            -- direct only: least(a,b) || greatest(a,b)
  title           text,                    -- group only
  created_by      uuid not null,
  last_seq        bigint not null default 0,   -- the single-writer counter
  created_at      timestamptz not null default now(),
  updated_at      timestamptz not null default now()   -- creation or last append
)

conversation_member (
  conversation_id  uuid not null references conversation,
  user_id          uuid not null references known_user,
  role             smallint not null,      -- admin | member
  visible_from_seq bigint not null,        -- first seq this member may see
  left_at_seq      bigint,                 -- last seq visible after leaving
  read_seq         bigint not null default 0,
  primary key (conversation_id, user_id)
)
-- index on (user_id) for "my conversations"

message (
  conversation_id  uuid not null references conversation,
  seq              bigint not null,        -- gapless per conversation
  message_id       uuid not null unique,
  client_msg_id    uuid not null,          -- UUIDv7 from the client
  kind             smallint not null,      -- 1 message | 2 edit | 3 delete | 4 system
  target_seq       bigint,                 -- edit/delete: the message being changed
  sender_id        uuid not null,
  sender_device_id uuid,                   -- null for system rows
  content_type     smallint not null,      -- 1 text | 2 system event
  body             bytea,                  -- null once deleted or superseded
  client_ts        timestamptz,
  server_ts        timestamptz not null default now(),
  edited_at        timestamptz,            -- kind=1 only: current state
  edit_seq         bigint,                 -- kind=1 only: seq of the applied edit
  deleted_at       timestamptz,            -- kind=1 only: tombstone
  primary key (conversation_id, seq),
  unique (conversation_id, client_msg_id)
)
-- index on (conversation_id, target_seq) where target_seq is not null

device_cursor (
  device_id       uuid not null references member_device,
  conversation_id uuid not null references conversation,
  delivered_seq   bigint not null,         -- device has everything up to here
  primary key (device_id, conversation_id)
)

outbox ( event_id uuid primary key, kind text not null,
         payload jsonb not null, created_at timestamptz not null default now() )
```

### 3.3 worker schema

```sql
job (
  job_id uuid primary key, kind text not null, payload jsonb not null,
  run_after timestamptz not null default now(), attempts int not null default 0,
  claimed_at timestamptz, failed_reason text
)

email_message (
  email_id            uuid primary key,    -- also Resend's idempotency key
  to_address          citext not null,
  recipient_domain    text not null,       -- for deliverability metrics
  template            text not null,
  params              jsonb not null,
  provider_message_id text,
  status              smallint not null,   -- queued | sent | suppressed | failed
  created_at          timestamptz not null default now(),
  sent_at             timestamptz
)

email_event (
  event_id uuid primary key, provider_message_id text not null,
  kind smallint not null,                  -- delivered | bounced | complained
  occurred_at timestamptz not null, raw jsonb not null
)

email_suppression (
  address citext primary key, reason smallint not null,
  created_at timestamptz not null default now()
)
```

The worker claims work with `SELECT … FOR UPDATE SKIP LOCKED`, so it scales to several
replicas without coordination, and work held by a crashed worker is claimed again.

## 4. Messages, delivery and read state

### 4.1 Core idea

Every conversation is an ordered log with a gapless sequence number `seq`. Messages,
edits, deletes and membership changes are all entries in that log.

Each **device** records how far it has received in each conversation
(`device_cursor.delivered_seq`). Anything after that position is undelivered for that
device. There are no per-recipient queue rows, so sending a message costs the same for a
2-person chat as for a 100-person group.

Each **member** records how far they have read (`conversation_member.read_seq`).

### 4.2 Sending

1. The SDK validates the message (§7.3). If invalid, `send()` returns an error and
   nothing is stored.
2. The SDK stores it in its local outbox under a new UUIDv7 `client_msg_id` and emits
   `LocalEcho`, so the UI shows it immediately.
3. The SDK sends a `Send` frame. It keeps at most one send in flight per conversation, so
   messages from one device keep their order.
4. The gateway forwards it to message-svc, attaching `user_id` and `device_id` from the
   verified token.
5. message-svc, in **one transaction**:
   1. `UPDATE conversation SET last_seq = last_seq + 1 … RETURNING last_seq` — locks
      the conversation.
   2. Looks up `client_msg_id` in the conversation. If it is already stored, this is a
      retry: **roll back** (which restores `last_seq`), check the stored row has the same
      `sender_id`, and return its original receipt. This comes before the permission
      checks, so a retry whose first attempt succeeded is never reported as failed —
      even if the sender has since been blocked or removed.
   3. Checks that the sender is a current member and, for direct conversations, that
      both contact rows are `accepted`.
   4. `INSERT INTO message … ON CONFLICT (conversation_id, client_msg_id) DO NOTHING
      RETURNING …`. If nothing was inserted (a backstop for step 2), roll back and return
      the original as in step 2.
   5. Advances the sender's read position to what the sending device has already received
      (its delivery position): replying marks as read everything the user could have seen,
      never messages their device has not received yet. Delivery positions are **not**
      touched.
   6. Commits.
6. Only **after commit** does message-svc reply with
   `{client_msg_id, message_id, seq, server_ts}`. The gateway sends `SendAck`, and the
   SDK deletes the outbox row and emits `Sent`.
7. message-svc publishes `notify:{user_id}` on Redis for every member.

Rolling back on retry keeps `seq` gapless. A concurrent duplicate waits on the
conversation lock and then hits the conflict, so retries are race-free.

### 4.3 Delivering

1. A gateway that receives `notify:{user_id}` calls `Sync(device_id)` on message-svc for
   each of that user's connected devices. Calls for one device are serialised.
2. message-svc returns entries with `seq > delivered_seq` from every conversation where
   `last_seq > delivered_seq`, within the member's visible range, in `seq` order per
   conversation, in batches.
3. The gateway pushes them as `Deliver` frames.
4. The SDK stores them locally and sends `Ack{conversation_id, seq}`, meaning
   "I have everything up to `seq`".
5. message-svc sets `delivered_seq = GREATEST(delivered_seq, seq)`.

**Where a device starts.** Delivery positions are created eagerly, never on first sync:
- when a device is registered, it gets a position at each of its user's conversations'
  current `last_seq`, so it starts up to date and loads older messages through
  `FetchHistory`;
- when a conversation is created or a member is added, every device of each new member
  gets a position at `visible_from_seq - 1`, so it receives everything from the start of
  its visibility.

### 4.4 Delivery states

| State | Meaning |
|---|---|
| pending | in the sender's local outbox |
| sent | stored on the server (`SendAck` received) |
| delivered | for a direct chat: the other member's highest `delivered_seq` across their devices ≥ `seq`. For a group: every other member has it |
| read | for a direct chat: the other member's `read_seq` ≥ `seq`. For a group: every other member has read it |

"Read" is an up-to position, not a per-message flag, and it only moves forward. The
honest statement is "Bob has read up to message N".

### 4.5 Pushing receipts

- **Direct chats:** when the other member's delivered or read position changes, their
  devices are sent `Receipt{conversation_id, user_id, delivered_seq, read_seq}`.
- **Groups:** the server pushes only the group-wide minimum
  (`all_delivered_seq`, `all_read_seq`, taken over all current members), only when it
  changes, at most once per second per conversation.
- Position changes that pass only over edits, deletes or system entries send no receipt.
- Sending advances the sender's read position to what the sending device has received, so
  a reply can also produce a read receipt for the other member of a direct chat.
- A read implies delivery: a receipt never reports `delivered_seq` below `read_seq`.
- After connecting, the client calls `ListConversations{changed_since}` to get the current
  positions and unread counts for conversations whose content changed. `changed_since`
  reflects content changes only: a conversation created or an entry appended. Receipt
  positions are not tracked by `updated_at`, so the busy conversation row is not written
  on every ack and read. A direct conversation's summary carries the other member's
  `peer_delivered_seq` and `peer_read_seq`, so a reconnecting client refreshes its ticks
  with one `ListConversations` without `changed_since`.
- Block and unblock do not move `updated_at` either. After a `ContactChanged` event, the
  client also lists without `changed_since` to refresh hidden and unhidden chats. Clients
  pass `changed_since` as the previous listing time minus a safety window.

### 4.6 Edits and deletes

- `Edit` and `Delete` are entries in the log with their own `seq`, delivered like
  messages. They get their own `client_msg_id`, so retries are safe.
- In the same transaction as the entry, message-svc updates the original message row:
  - **Edit:** current `body`, `edited_at`, `edit_seq`. The previous edit entry's body is
    set to null, so only the current version keeps any text.
  - **Delete:** `deleted_at`, and `body` set to null on the original **and every edit
    entry** targeting it. Deletion is real erasure, not hiding.
- Checks, made under the conversation lock:
  - the target is a normal message (`kind = 1`) and not already deleted (a retry of the
    same delete, with the same `client_msg_id`, returns its original result; a new
    delete of an already-deleted message fails with `AlreadyDeleted`, and so does an
    edit of one);
  - the caller is the original sender (any of their devices);
  - `now() - target.server_ts` is within the window. Edits and deletes are allowed for
    48 hours by default; both limits are server settings.
- The target is identified by `target_message_id`, or by `target_client_msg_id` when the
  client does not have the server id yet.
- **Delivering:** a `Deliver` frame for a message always carries its current state
  (body, `edit_seq`, `deleted`). The SDK applies an edit or delete only if
  `entry.seq > local.edit_seq`, and drops it if the target is not stored locally. This
  makes replays and race conditions harmless.
- **History pages** return only messages and system entries, with their current state.
  They never include edit or delete entries.
- Visibility applies to the target as well: a member never receives an edit or delete
  whose `target_seq` is outside their visible range.
- Unread counts include only messages (`kind = 1`) that are not deleted and were sent by
  someone else, capped at 100 ("99+").

### 4.7 Rules that must never be broken

- Never acknowledge before commit.
- `seq` is gapless per conversation.
- Never put a message body on Redis.
- Positions only move forward: `GREATEST(current, new)`.
- Delivery positions move only on `Ack` from that device — never on send.

### 4.8 Sequence numbers and ordering

- `seq` is per conversation and starts at 1. It comes from `conversation.last_seq`,
  incremented inside the write transaction — not from a Postgres sequence, which is
  shared and may skip values.
- It is a signed 64-bit integer and never wraps. Protobuf encodes small values in 1–3
  bytes. The Node binding exposes it as `BigInt`.
- There is one ordered log per conversation and no priorities. Edits and deletes take
  the next `seq` like anything else. They need no priority: a message is always
  delivered in its current state, so a recipient who has not received the original yet
  gets the edited or deleted version directly, and a deleted body is never sent after
  the delete.

## 5. Accounts, devices and contacts

### 5.1 Credentials

| Credential | Proves | Lifetime | Stored |
|---|---|---|---|
| Registration token | "this account may register a device" | 5 min | no |
| Access token (JWT, EdDSA) | "user X on device D, email verified" | 15 min | no |
| Refresh token | "device D may get new access tokens" | 30 days | yes, hashed and rotated |
| Email link | one action on one account | 30 min | no |

Access-token claims: `sub`, `device_id`, `email_verified`, `exp`, `iat`, `jti`, and
`aud` set to the gateway URL. Only access and refresh tokens are sign-in credentials.

### 5.2 Signup and email verification

1. `POST /v1/auth/signup {email, password, handle, display_name}`. The response is the
   same, in uniform time, whether or not the email exists.
2. auth-svc writes `email.send` (verification) to its outbox in the same transaction.
3. The link carries an HMAC-signed `{user_id, email, purpose: verify, exp}`. Binding the
   email means a future email-change feature cannot make old links dangerous.
4. Opening the link sets `email_verified_at` and writes `user.verified` to the outbox.
   It never signs the user in. Verification is the gate that opens API usage.
5. Unverified accounts cannot register devices or open sockets. They can only ask for
   the email to be resent.

### 5.3 Explicit device registration

1. `POST /v1/auth/login {email, password}` returns a **registration token**. It cannot
   open a socket or call any other API.
   - Or `POST /v1/auth/login {email, password, device_id}` for an existing, active
     device, which returns access and refresh tokens directly.
2. `POST /v1/devices {name, platform}` with the registration token:
   - refuses if the account already has 10 active devices;
   - creates the device and writes `device.registered` to the outbox;
   - returns `{device_id, access_token, refresh_token}`.
3. `POST /v1/auth/refresh` is the only way to get new access tokens afterwards.
4. `GET /v1/devices` lists devices; `DELETE /v1/devices/{id}` revokes one, revokes its
   refresh tokens, and writes `device.revoked`.
5. The worker relays device events to message-svc. On `device.revoked`, message-svc
   marks the device revoked and publishes `kick:{device_id}`; the gateway holding it
   closes the socket at once with 4003.
6. The gateway learns the device only from the verified token. message-svc rejects
   `Identify` for a device that is not active in `member_device`.

### 5.4 Password reset

- The link carries HMAC-signed `{user_id, purpose: reset, exp, pw}` where `pw` is a hash
  of the current `password_hash`. Using it changes the password, which invalidates the
  link — single use with no stored state.
- On reset, every refresh token of the account is revoked and every device is kicked.

### 5.5 Connecting users

- **@handle.** Every user picks a unique handle at signup. Handles reveal no email.
  `GET /v1/users/lookup?handle=…` returns `{user_id, handle, display_name}`; rate limited
  to 20 per hour per user. Email lookup is not offered.
- **Contact request.** `ContactRequest{user_id}` creates `requested_out` /
  `requested_in` rows. The other user can `Accept`, `Decline` or `Block`. If both send a
  request to each other, it becomes accepted. A decline is announced only to the user
  who declined; to the requester it looks the same as a block. There is no "remove
  contact" operation in v1, so an accepted contact can end only through a block; a
  remove/unfriend operation is a candidate for a later version.
- **Invite link / QR code.** `CreateInvite{expires_in, max_uses}` returns a short code,
  shown as a link or QR code. `RedeemInvite{code}` makes both users accepted contacts
  immediately: sharing the link is the consent. Codes expire, have a use limit and can be
  revoked.
- **Only accepted contacts can chat.** `OpenDirect{peer_user_id}` and `Send` in a direct
  conversation require `accepted` on both sides. `blocked` rejects sends both ways and
  hides the conversation from the blocker.
- **Profiles.** `GET /v1/users?ids=…` returns handle and display name for user ids the
  caller shares a contact or conversation with.

### 5.6 Passwords and rate limits

- Argon2id `m=19456 KiB, t=2, p=1` (OWASP minimum), stored as a PHC string so the
  parameters can be raised on next login. Calibrated to about 250 ms on the production VM.
- Rate limits in Redis, per account and per IP, on signup, login, resend, reset, handle
  lookup and invite redeem. Accounts are never locked; excess requests are refused.
- Links are built from a configured base URL, never from the `Host` header. Pages they
  open send `Referrer-Policy: no-referrer`.

## 6. Groups (up to 100 members)

- `CreateGroup{title, member_ids[]}` — every member must be an accepted contact of the
  creator. The creator becomes an admin.
- Two roles. **Admins** (one or more) rename the group, add and remove members, and
  promote members to admin or demote admins to member. **Members** chat and can leave.
- A group always has at least one admin. If the last admin leaves or is demoted, the
  member with the earliest `joined` entry becomes admin, recorded as a system entry. A
  group with no members left is closed.
- Every membership change is a **system entry** in the log, written under the same
  conversation lock as messages. Sends and membership changes are therefore strictly
  ordered: a removed member never receives anything after the removal entry.
- A new member sees history from `visible_from_seq` (the seq of the "added" entry). A
  member who leaves keeps history up to `left_at_seq`. Rejoining sets a new
  `visible_from_seq` and clears `left_at_seq`.
- The 100-member limit is checked under the conversation lock.
- Adding a member requires them to be an accepted contact of the admin adding them.
- Group receipts are aggregated (§4.5). Per-member read lists are out of scope for v1.

## 7. Wire protocol

WebSocket binary frames. Each frame is one protobuf `Frame` with a `oneof` body. The
definitions live in `push-proto` and are the single contract between the SDK, the
gateway and message-svc. Field numbers for `ephemeral`, `urgent`, private reads and media
attachments are reserved.

### 7.1 Client → server

| Frame | Fields |
|---|---|
| Identify | `token` — must be the first frame, within 5 s |
| ReAuth | `token` |
| Send | `client_msg_id, conversation_id, content_type, body, client_ts` |
| Edit | `client_msg_id, conversation_id, target_message_id or target_client_msg_id, body, client_ts` |
| Delete | `client_msg_id, conversation_id, target_message_id or target_client_msg_id` |
| Ack | `conversation_id, seq` |
| ReadUpTo | `conversation_id, seq` |
| Ping | — |

Request frames carry a client-chosen `request_id`, echoed in the response:

| Request | Fields |
|---|---|
| ListConversations | `changed_since` (optional) |
| FetchHistory | `conversation_id, before_seq, limit` |
| OpenDirect | `peer_user_id` |
| ContactRequest / Accept / Decline / Block / Unblock | `user_id` |
| ListContacts | — |
| CreateInvite | `expires_in_s, max_uses` |
| RedeemInvite / RevokeInvite | `code` |
| CreateGroup | `title, member_ids[]` |
| AddMembers / RemoveMember / LeaveGroup / RenameGroup | `conversation_id`, plus `user_ids[]` / `user_id` / — / `title` |
| SetRole | `conversation_id, user_id, role` |

### 7.2 Server → client

| Frame | Fields |
|---|---|
| Ready | `user_id, device_id, heartbeat_ms` |
| Deliver | `conversation_id, seq, message_id, client_msg_id, kind, target_message_id, sender_id, content_type, body, client_ts, server_ts, edited_at, edit_seq, deleted` |
| SendAck | `client_msg_id, message_id, seq, server_ts` (also for Edit and Delete) |
| Receipt | direct: `conversation_id, user_id, delivered_seq, read_seq`; group: `conversation_id, all_delivered_seq, all_read_seq` |
| ContactEvent | `user_id, state` |
| Response | `request_id` and the request-specific result |
| AuthExpiring | `expires_in_s` |
| Error | `code, message, retryable`, plus `client_msg_id` or `request_id` |
| Pong | — |

### 7.3 Sessions

- **Identify** is rate limited per device after the token is verified. On success the
  gateway subscribes to `notify:{user_id}` and `kick:{device_id}`, publishes a
  "device connected" signal so any other gateway holding the same device closes it with
  4003, sends `Ready`, and runs a `Sync`. There is no separate resume step: delivery
  state lives in Postgres, so reconnecting is just Identify.
- **Token expiry.** About 60 s before `exp` the gateway sends `AuthExpiring`. The client
  refreshes over HTTPS and sends `ReAuth`; the connection is not dropped. Without a valid
  `ReAuth` by `exp + 30 s`, the gateway closes with 4002.
- **Heartbeat.** The client pings every 30 s; the first ping is delayed by a random
  0–30 s so clients that reconnected together do not stay synchronised.
- **Reconnect backoff.** Exponential with jitter, 1 s up to 60 s.
- Because takeover depends on Redis, two sockets for one device can briefly coexist. The
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

| Layer | Default | Setting |
|---|---|---|
| Azure Load Balancer idle timeout | 4 min | 30 min, with TCP reset |
| nginx `proxy_read_timeout` / `proxy_send_timeout` | 60 s | 120 s |
| Client ping interval | — | 30 s |

## 8. SDK

### 8.1 Layering

- **`push-sdk`** (Rust core) owns everything: connection, outbox, local message store,
  retries, positions and acks, reconnect, events.
- **`bindings/node`** (napi-rs) and **`bindings/c`** (cbindgen) only convert types and
  adapt async calls and callbacks. They contain no business logic.

### 8.2 Public surface

- `Client` — sign-up, verification resend, login, device registration, connect,
  disconnect; `send`, `edit`, `delete`, `mark_read`, `fetch_history`,
  `list_conversations`; contacts, invites and groups. On creation it rescans the outbox
  and resumes sending.
- `SendHandle` — returned by `send`, `edit` and `delete`; exposes `client_msg_id` and
  `cancel()` (works only before the server has acknowledged).
- `SdkEvent` — one stream: `LocalEcho`, `Sent`, `Received`, `Edited`, `Deleted`,
  `Delivered`, `Read`, `SendFailed{client_msg_id, reason}`, `ContactChanged`,
  `ConversationChanged`, `ConnectionState`.

### 8.3 Validation before queuing

`send`, `edit` and `delete` validate synchronously, using rules from `push-proto` that
the server shares: non-empty text up to 64 KiB, a supported content type, a known
conversation, and — for edit and delete — that the target was sent by this user and is
inside the time window. Invalid requests return an error and are never stored. The
server checks everything again.

### 8.4 Outbox

```sql
outbox (
  client_msg_id   blob primary key,   -- UUIDv7; stable across retries and restarts
  conversation_id blob not null,
  kind            integer not null,   -- message | edit | delete
  target          blob,               -- edit/delete: message_id or client_msg_id
  content_type    integer,
  body            blob,
  state           integer not null,   -- queued | in_flight
  enqueued_at     integer not null,
  attempts        integer not null default 0,
  parent_id       blob                -- send only after this row is acknowledged
)
```

- Rows are sent in `enqueued_at` order per conversation, one in flight at a time.
- **Transient failures** (network, server errors, rate limits) are retried with backoff
  and jitter, 1 s up to 60 s.
- **Rejections** (not a contact, not a member, window expired) remove the row, emit
  `SendFailed`, and let the next row proceed. A rejected entry never exists on the
  server, so later entries cannot overtake it.
- **Editing or deleting an unsent message:** if the original is still `queued`, an edit
  changes it in place and a delete removes it. If it is `in_flight`, the edit or delete
  is queued as its own row with `parent_id` set to the original, and waits for its
  `SendAck`.
- A row is deleted only when its `SendAck` arrives.
- Applying a delete erases the body from the local store.

### 8.5 FFI rules

- Every `extern "C"` function is wrapped in `catch_unwind`.
- The core owns exactly one tokio runtime, created with the `Client`.
- C callers receive events through a registered callback and context pointer, called on
  an SDK thread. This is documented.
- Every returned allocation has a matching `*_free`.
- Node receives events through a `ThreadsafeFunction`.

## 9. Email

- Provider: **Resend**, behind provider-neutral traits:
  - `EmailSender::send(message, idempotency_key)` returns `provider_message_id`;
  - `WebhookVerifier::verify` and `parse` turn webhooks into delivery events.
- Any service can send email by writing `email.send {user_id or address, template,
  params}` to its outbox. The worker renders the template, checks `email_suppression`,
  and sends.
- Hard bounces and complaints add the address to `email_suppression`.
- Sent from our own domain with SPF, DKIM and DMARC. Open and click tracking are off.
- **Deliverability is measured, not assumed.** Resend has no independent placement data,
  so we track verification-completion rate by `recipient_domain`. That is the signal for
  switching provider; the traits make switching a configuration change.
- Local development uses a log-only `EmailSender`.

Azure Communication Services Email is not used: it is being retired (onboarding closes
2026-10-23, service ends 2028-09-30).

## 10. Repository layout

```
push-messenger/
├── crates/
│   ├── push-proto        protobuf types, validation rules, shared limits
│   ├── push-domain       messaging logic over repository traits; no I/O
│   ├── push-store        Postgres implementations (sqlx) and migrations
│   ├── push-email        email traits; Resend and log implementations
│   ├── push-gateway      binary
│   ├── push-message-svc  binary
│   ├── push-auth         binary
│   ├── push-worker       binary
│   └── push-sdk          client core
├── bindings/
│   ├── node/             napi-rs
│   └── c/                cbindgen
├── deploy/
│   ├── helm/
│   └── k3s/              VM provisioning scripts
├── docs/
│   ├── research/
│   └── superpowers/specs/
└── .github/workflows/
```

## 11. Deployment

- k3s on one Azure VM, with ingress-nginx and cert-manager (Let's Encrypt).
- Use a **non-burstable** VM size for load testing and production. Burstable B-series CPU
  credits run out under a 10,000-connection test and Argon2 load.
- PostgreSQL and Redis run in the cluster for v1, with persistent volumes and Redis AUTH.
- Secrets (database passwords, JWT signing key, HMAC key, Resend API key) come from
  Azure Key Vault through the CSI driver — never from git or ConfigMaps.
- Gateway pods have a `preStop` hook that closes connections in batches with 4005, and
  `terminationGracePeriodSeconds` longer than that.
- Images go to GHCR and are deployed with Helm.
- Region, VM size and domain are confirmed with the owner before any cloud spend.

## 12. Testing

| Layer | Scope |
|---|---|
| Unit | `push-domain` with in-memory repositories: seq allocation, retry replay, positions, receipts, edit/delete rules, contacts, group membership, validation |
| Integration | Real Postgres and Redis via testcontainers: transaction order, gapless `seq` under concurrent retries, outbox relay, `SKIP LOCKED` |
| Protocol | Raw-frame harness: malformed frames, out-of-order acks, replayed `client_msg_id`, unverified email, revoked device, non-contact send |
| End-to-end | SDK clients through the full stack: register, verify, connect via invite, chat, receipts, edit, delete, group of 100, new device loading history |
| Load | 10,000 concurrent sockets: memory per connection, latency, recovery from a reconnect storm, 100-member group throughput |

Required adverse-condition tests:

1. Connection dropped mid-send: the message is stored once and the retry returns the
   original `seq`.
2. Two identical sends at the same moment: one row, no gap in `seq`.
3. message-svc restarted mid-send: nothing acknowledged is lost.
4. Redis unavailable: delivery continues through reconnect and the 5-minute sweep, only
   slower.
5. Two connections for one device: the SDK applies each entry once.
6. Device revoked while connected: socket closed within seconds.
7. Token expires on a live connection: `ReAuth` keeps it open; without it, 4002.
8. Sender offline while the recipient reads: on reconnect the message shows as read.
9. Edit and delete while the original is still undelivered: the recipient ends with the
   latest state, and a deleted body exists nowhere on the server.
10. Member removed from a group: receives nothing after the removal entry.

## 13. Open items for implementation

- Confirm the Azure Load Balancer idle-timeout annotation against the
  cloud-provider-azure documentation before writing manifests.
- Choose the production domain and the Resend sending subdomain.
- Calibrate Argon2id on the chosen VM size.
- Decide whether group endpoints ship in the first release or the second.
