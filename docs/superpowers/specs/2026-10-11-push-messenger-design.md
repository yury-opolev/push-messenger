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
Cleanups of another service's data run as functions that service owns and lets the
worker's role call (auth-svc: expired refresh tokens, accounts left unverified), so the
rules stay with the data and the worker never reads it. Only the worker may call
message-svc's `Accounts` API, which records the users and devices auth-svc announces:
each call carries a shared internal token from the cluster's Secret, which the gateway
does not get, so a compromised gateway cannot register devices (a NetworkPolicy adds a
second fence with the Azure plan).

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
  registration_id uuid not null,            -- chosen by the client; retries reuse it
  created_at   timestamptz not null default now(),
  last_seen_at timestamptz,
  revoked_at   timestamptz,
  unique (user_id, registration_id)
)

refresh_token (
  token_hash bytea primary key,             -- SHA-256; raw token never stored
  device_id  uuid not null references device,
  expires_at timestamptz not null,
  revoked_at timestamptz,
  rotated_to bytea,                         -- reuse detection
  rotated_at timestamptz                    -- start of the 30 s grace window
)

outbox (                                    -- read and deleted by the worker
  event_id   uuid primary key,
  kind       text not null,                 -- email.send | user.verified |
                                            -- user.password_reset |
                                            -- device.registered | device.revoked
  payload    jsonb not null,
  created_at timestamptz not null default now()
)
```

Implementation notes (push-auth, migration `0001_auth.sql`, Plan 5A):

- auth-svc has its own database, `push_auth`, owned by its own role. The migration creates
  the `citext` extension (trusted, so the database owner may) and the `auth` schema.
- Handles are stored in lower case: 3–30 letters, digits and underscores, starting with a
  letter (a `CHECK`). Display and device names have 1–64 characters; `platform` is checked
  against the five values. A partial index on `device (user_id) where revoked_at is null`
  serves the device limit; `unique (user_id, registration_id)` makes a retried
  registration find its device.
- `refresh_token.rotated_at` is set when the holder exchanges the token and starts its
  30-second grace window (§5.3); it is null on a token cut off by a retry of the one
  before it.
- Comparisons with a bound email or handle cast the parameter to `citext`
  (`email = $1::citext`): sqlx binds `text`, and `citext = text` compares as text, which
  is case-sensitive.
- Outbox payloads are JSON: `email.send {user_id, to, template, params}` (templates
  `verify_email {link, handle, expires_in_minutes}`, `account_exists {handle}`,
  `reset_password {link, expires_in_minutes}`); `user.verified {user_id}`;
  `user.password_reset {user_id}`, on which every device of the user is kicked (§5.4);
  `device.registered` and `device.revoked {user_id, device_id}`. New ids are UUIDv7.

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
  code       text primary key,             -- 10 chars of Crockford base32, also for QR
  user_id    uuid not null references known_user,
  expires_at timestamptz not null,
  max_uses   int not null default 1,       -- 1 to 100
  uses       int not null default 0,
  revoked_at timestamptz
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

Implementation notes (push-store, migration `0001_messaging.sql`):

- Enumerations are `smallint` with `CHECK` constraints: contact state 1–4 in the order
  listed, conversation kind 1 direct / 2 group, role 1 admin / 2 member, message kind 1–4
  and content type 1–2 as above. Invite codes are checked against the Crockford alphabet,
  and `uses` never exceeds `max_uses`.
- Extra indexes: `member_device (user_id)`; history pages use a partial index on
  `message (conversation_id, seq) where kind in (1, 4)`; unread counts and receipt
  positions use a partial index on live messages
  (`where kind = 1 and deleted_at is null`, including `sender_id`).
- Group-wide receipt positions, conversation listings, sync positions and conversation
  state each take one statement, however many members or conversations there are. The
  per-member logic lives in the SQL functions `messaging.delivered_seq`,
  `messaging.effective_position` and `messaging.group_positions`.
- Transactions run at READ COMMITTED. `next_seq` is
  `UPDATE conversation SET last_seq = last_seq + 1, updated_at = greatest(updated_at, $at)
  … RETURNING last_seq`, whose row lock serialises appends. Contact pairs and a group's
  receipt positions are serialised with `pg_advisory_xact_lock` on a 64-bit FNV-1a hash of
  a per-kind tag and the ids; invites with `SELECT … FOR UPDATE`. Serialization failures
  and deadlocks are reported as a retryable conflict.
- The `outbox` table above is **not** part of the messaging migration yet: the plan that
  first needs it (worker/auth) adds it.

### 3.3 worker schema

```sql
job (                                       -- job_id = the outbox event's id
  job_id uuid primary key, kind text not null, payload jsonb not null,
  run_after timestamptz not null default now(), attempts int not null default 0,
  claimed_at timestamptz, failed_reason text,
  dead_at timestamptz,                       -- buried (poison): kept, never claimed
  created_at timestamptz not null default now()
)

email_message (
  email_id            uuid primary key,    -- also Resend's idempotency key
  to_address          citext not null,
  recipient_domain    text not null,       -- for deliverability metrics
  template            text not null,
  params              jsonb not null,      -- never the link: it signs its holder in
  provider_message_id text,
  status              smallint not null,   -- queued | sent | suppressed | failed
  created_at          timestamptz not null default now(),
  sent_at             timestamptz
)

email_event (                               -- event_id = the webhook's id (svix-id)
  event_id text primary key, provider_message_id text not null,
  kind smallint not null,                  -- delivered | bounced | complained
  occurred_at timestamptz not null, raw jsonb not null
)

email_suppression (
  address citext primary key, reason smallint not null,
  created_at timestamptz not null default now()
)
```

Implementation notes (push-worker, migration `0001_worker.sql`, Plan 6A):

- The worker has its own database, `push_worker`, owned by its role `push_worker`, which may
  also connect to `push_auth`: auth-svc grants it (`PUSH_AUTH_WORKER_ROLE`, at start-up)
  `SELECT`, `DELETE` and `UPDATE (created_at)` on `auth.outbox` — `FOR UPDATE` needs an
  `UPDATE` right — and `EXECUTE` on `auth.delete_expired_refresh_tokens` and
  `auth.delete_unverified_accounts`, `SECURITY DEFINER` functions of auth-svc's migration
  `0002_worker.sql`. It never reads accounts or tokens.
- `job` also has `dead_at` (a buried job: poison, kept for inspection without an email's
  `params.link`, never claimed again) and `created_at`. A job's id is its outbox event's
  id. A claim is `UPDATE … SET attempts = attempts + 1, claimed_at = now(), run_after =
  now() + 2 min WHERE job_id IN (SELECT … LIMIT 4 FOR UPDATE SKIP LOCKED) RETURNING`: a
  lease, so a crashed worker's job comes back. A worker claims at most 4 jobs, which it
  runs one after another within the lease (an attempt, a cleanup run included, takes at
  most about 15 s). Every outcome is recorded only while the job is still under its claim
  (`attempts` and `claimed_at` match), so a late one never overwrites a newer claim; at
  shutdown the jobs not started are given back (`run_after = now()`, `attempts - 1`).
  Retries wait 1 s, doubling, at most 10 minutes; account events retry without limit, an
  email gives up after 12 attempts (about 30 minutes, the life of its link).
- The relay copies a locked batch of outbox rows into `job` (`ON CONFLICT DO NOTHING`),
  commits, and only then deletes them from the outbox: at least once, never lost.
- `email_message.params` never holds the link. `email_event.event_id` is `text`: the
  webhook's `svix-id`, so a redelivered webhook is recorded once.
- The cleanups (`cleanup.refresh_tokens`, `cleanup.unverified_accounts`,
  `cleanup.worker_records`) are jobs with fixed ids that run hourly; they delete expired
  refresh tokens, accounts unverified for 7 days (a new signup over an unverified account
  resets its `created_at`), and buried jobs, email records and delivery events older than 30
  days. The suppression list is kept. A buried `email.send` job keeps the recipient address
  in its payload until `delete_dead` removes it with the other buried jobs after 30 days.

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
  that has no position gets one at `visible_from_seq - 1`, so it receives everything from
  the start of its visibility. A rejoining member's devices keep their older position,
  and every reader treats positions below `visible_from_seq - 1` as the floor.

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
- **Groups:** the server pushes only the group-wide minimums `all_delivered_seq` and
  `all_read_seq`, taken over the current members: every message up to N has been
  delivered to (read by) every member other than its sender. A member's own messages never
  hold these back: each member counts as having reached the point just before the first
  live message from someone else that they have not received (read). message-svc
  publishes a change whenever a minimum moves over a live message; the gateway sends at
  most one per second per conversation, keeping the latest.
- Position changes that pass only over edits, deletes or system entries send no receipt.
- Sending advances the sender's read position to what the sending device has received, so
  a reply can also produce a read receipt for the other member of a direct chat.
- A read implies delivery: a receipt never reports `delivered_seq` below `read_seq`.
- When a user's read position moves, by reading or by sending, their own devices are told
  with `ReadPosition{conversation_id, read_seq}`, so all of them show the same unread
  counts.
- After connecting, the client calls `ListConversations{changed_since}` to get the current
  positions and unread counts for conversations whose content changed. `changed_since`
  reflects content changes only: a conversation created or an entry appended. Receipt
  positions are not tracked by `updated_at`, so the busy conversation row is not written
  on every ack and read. A conversation's summary carries its receipt positions — the
  other member's `peer_delivered_seq` and `peer_read_seq` in a direct chat, the group-wide
  `all_delivered_seq` and `all_read_seq` in a group — so a reconnecting client refreshes
  its ticks with one `ListConversations` without `changed_since`.
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
  - in a direct conversation, both contact rows are `accepted`, as for sending (see
    §5.5);
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
   - If the email has an **unverified** account, the new signup replaces its password,
     handle and display name and sends a new link; links sent before stop working.
     Otherwise whoever signs up first with someone else's address would own the account
     that person later verifies. A verified account changes nothing and gets an "account
     exists" email.
2. auth-svc writes `email.send` (verification) to its outbox in the same transaction.
3. The link carries an HMAC-signed `{user_id, email, purpose: verify, exp, pw}`, `pw`
   being a fingerprint of the current `password_hash`. Binding the email means a future
   email-change feature cannot make old links dangerous; binding `pw` means a link sent
   before the password changed verifies nothing.
4. Opening the link sets `email_verified_at` and writes `user.verified` to the outbox.
   It never signs the user in. Verification is the gate that opens API usage.
5. Unverified accounts cannot register devices or open sockets. They can only ask for
   the email to be resent. An account still unverified 7 days after its last signup is
   deleted (owner decision, 2026-10-13).

### 5.3 Explicit device registration

1. `POST /v1/auth/login {email, password}` returns a **registration token**. It cannot
   open a socket or call any other API. It binds a fingerprint of the password hash, so
   a password reset makes it worthless.
   - Or `POST /v1/auth/login {email, password, device_id}` for an existing, active
     device, which returns access and refresh tokens directly.
2. `POST /v1/devices {name, platform, registration_id}` with the registration token.
   `registration_id` is a UUID the client chooses for this registration and reuses for
   its retries, like `client_msg_id`:
   - a retry with the same `registration_id` for the same user returns the same device
     (with new tokens; the earlier ones are revoked) and never creates a second one,
     even when the account is at its limit; a revoked device's id is not reused;
   - otherwise refuses if the account already has 10 active devices;
   - creates the device and writes `device.registered` to the outbox;
   - returns `{device_id, access_token, refresh_token}`.
3. `POST /v1/auth/refresh` is the only way to get new access tokens afterwards. Each
   refresh rotates the refresh token. Presenting the token rotated last again within
   30 s of its rotation (the answer was lost) returns a new pair and cuts off the pair
   issued before, so only one branch lives. Presenting it later, or any older or cut-off
   token, is reuse: every refresh token of the device is revoked.
4. `GET /v1/devices` lists devices; `DELETE /v1/devices/{id}` revokes one, revokes its
   refresh tokens, and writes `device.revoked`.
5. The worker relays device events to message-svc. On `device.revoked`, message-svc
   marks the device revoked and publishes `kick:{device_id}`; the gateway holding it
   closes the socket at once with 4003.
6. The gateway learns the device only from the verified token. message-svc rejects
   `Identify` for a device that is not active in `member_device`.
   - A device message-svc has **not heard of yet** is not rejected for good: the worker
     relays `device.registered` about a second after the registration, and until then
     `Identify` answers `UNAVAILABLE` (retryable), unlike a revoked device or one of
     another user (`PERMISSION_DENIED`, `UNKNOWN_DEVICE`). The gateway (Plan 8) must
     pass this on as "try again shortly", and the SDK must reconnect after a short
     backoff — never treat it as a revocation or sign the user out.

Implementation note (Plan 6A): message-svc's internal `Accounts` gRPC API takes the relayed
`user.verified`, `device.registered`, `device.revoked` and `user.password_reset`. Every call
is idempotent and any order ends in the same state: a registration also records the user,
and a revocation that arrives before the registration records the device as revoked, so the
registration cannot revive it. Every call needs the shared internal token
(`authorization: Bearer <PUSH_ACCOUNTS_TOKEN>`, compared in constant time); without it, or
when message-svc has none, the call is `UNAUTHENTICATED`. A revoked device is refused by
`Identify` at once (`PERMISSION_DENIED`); a device not relayed yet is `UNAVAILABLE`
(`retryable`), which the gateway closes with 1011 until Plan 8 gives it a code of its own.
Publishing `kick:{device_id}` (and kicks on a password reset) comes with Plan 7, closing
the socket and refusing older tokens with Plan 8.

### 5.4 Password reset

- The link carries HMAC-signed `{user_id, purpose: reset, exp, pw}` where `pw` is a hash
  of the current `password_hash`. Using it changes the password, which invalidates the
  link — single use with no stored state.
- On reset, every refresh token of the account is revoked and every device is kicked.
- A completed reset also verifies the email, since the link reached the address; if it
  was not verified yet, `user.verified` is written too.

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
  immediately: sharing the link is the consent. Codes expire (1 minute to 30 days), have a
  use limit (1 to 100) and can be revoked. A code is 10 characters of Crockford base32
  (50 random bits, enough because redeeming is rate limited), read case-insensitively. An
  unknown, revoked, expired or used-up code, and a code whose creator has blocked the
  redeemer, all give the same "invite not found", so redeeming reveals nothing. Users who
  are already contacts use nothing up; a pending request between the two becomes
  accepted.
- **Only accepted contacts can chat.** `OpenDirect{peer_user_id}` and `Send` in a direct
  conversation require `accepted` on both sides. `blocked` rejects sends both ways and
  hides the conversation from the blocker. After either user blocks the other, neither
  can send, edit or delete messages in their direct conversation. Earlier messages stay
  exactly as they were, so the conversation is preserved as evidence (for example of
  harassment). Unblocking restores the ability to edit and delete within the normal time
  windows.
- **Profiles.** `GET /v1/users?ids=…` returns handle and display name for user ids the
  caller shares a contact or conversation with.

### 5.6 Passwords and rate limits

- Argon2id `m=19456 KiB, t=2, p=1` (OWASP minimum), stored as a PHC string so the
  parameters can be raised on next login. Calibrated to about 250 ms on the production VM.
- Passwords have 8–128 characters and no other rule for now; a check against leaked
  passwords comes later.
- Rate limits in Redis, per account and per IP, on signup, login, resend, reset, handle
  lookup and invite redeem. Accounts are never locked; excess requests are refused.
  Login counts per account **and** IP, so a single address cannot lock a user out; the
  per-account ceiling (100 per hour) still lets an attacker with about 10 addresses
  block the owner's logins for up to an hour, an accepted trade-off. When Redis is unreachable, the limited endpoints fail closed with a
  retryable `unavailable` (503).
- Links are built from a configured base URL, never from the `Host` header. Pages they
  open send `Referrer-Policy: no-referrer`.

Implementation notes (push-auth, Plan 5A):

- **Endpoints.** `POST /v1/auth/signup` (202), `GET|POST /v1/auth/verify`,
  `POST /v1/auth/verify/resend` (202), `POST /v1/auth/login`, `POST /v1/auth/refresh`,
  `POST /v1/auth/password/forgot` (202), `GET|POST /v1/auth/password/reset`,
  `GET /v1/auth/jwks`, `POST|GET /v1/devices`, `DELETE /v1/devices/{id}`,
  `GET|PATCH /v1/users/me` and `GET /v1/users/lookup?handle=…`. Errors are
  `{"error": {"code", "message", "retryable"}}`; 429 carries `Retry-After`; 503
  `unavailable` is retryable and carries `Retry-After: 1`. No answer may be cached except
  the JWKS.
- **Link pages.** A link opens a small page whose button posts the token: a `GET` changes
  nothing, so mail scanners that open links do nothing. Pages send
  `Referrer-Policy: no-referrer`, `Cache-Control: no-store` and a CSP of
  `default-src 'none'`. API clients post the token as JSON instead.
- **Signed tokens.** Email links and the registration token are
  `base64url(JSON claims).base64url(HMAC-SHA256)` with `{sub, purpose, exp, pw}` plus
  `email` (verify). `pw` is 16 bytes of SHA-256 of the account's password hash when the
  token was made, checked against the stored hash under the row lock, so a reset — or a
  new signup over an unverified account — makes every earlier link and registration
  token worthless. The purposes `verify`, `reset` and `register` are checked, so no token
  stands in for another; the gateway does not have the HMAC key, so a registration token
  can never open a socket.
- **Signup over an unverified account** is one `INSERT … ON CONFLICT (email) DO UPDATE …
  WHERE email_verified_at IS NULL`: it replaces the password hash, handle and display
  name, or does nothing for a verified account. A handle held by any account, this
  email's own included, is `handle_taken`, so no answer depends on the email.
- **Login rehash.** A hash with outdated parameters is replaced on login only after the
  email-verified check, so an unverified login never changes the hash and never voids
  pending links.
- **Access tokens** are EdDSA (Ed25519) JWTs with a `kid` (the RFC 7638 thumbprint) and
  the claims of §5.1; `GET /v1/auth/jwks` publishes the key. The signing seed and the HMAC
  key come from the Secret. `jsonwebtoken` 9.3.1 (version 10 drops the `ring` backend) has
  CVE-2026-25537, fixed in 10.3.0: a claim with the wrong JSON type is
  treated as absent unless it is required. Accepted, because `exp`, `sub` and `aud` are
  required and only auth-svc signs; the gateway's check (Plan 8) must require `exp` too.
- **Refresh tokens** are 32 random bytes, base64url, stored as SHA-256. Refreshing marks
  the old row `rotated_to` the new hash and `rotated_at` now. The row rotated last, if its
  successor is still live and `rotated_at` is under 30 s old, may be presented again: it
  gets a new pair, its successor is marked `rotated_to` the new hash with a null
  `rotated_at` (cut off, no window of its own), and its own `rotated_at` stays, so retries
  never extend the window. Anything else presented after rotation is reuse and revokes
  every refresh token of the device. Signing in again with `device_id` revokes the
  device's earlier refresh tokens. A cut-off pair's access token stays valid until it
  expires (at most 15 minutes).
- **Lock order.** Every path that changes a device's refresh tokens locks the device row
  first, in the order user row → device rows → token rows. Concurrent refreshes are
  thereby serialised and leave one live branch, a stolen token cannot survive a
  revocation or reuse detection, and the paths cannot deadlock. The grace window and the
  expiry use the statement clock (`statement_timestamp()`), so lock waits do not stretch
  them.
- **Device registration** takes `registration_id`, unique per user among all of its
  devices. Under the account row lock, an active device with that id gets new tokens
  (its refresh tokens revoked, no outbox event, no limit check); a revoked one answers
  `device_not_found`; otherwise the 10-device limit is checked and the device created.
- **Device limit**: 10 active devices, checked under a row lock on the account, so
  concurrent registrations cannot pass it together.
- **Timing and load.** Signup hashes the password before looking at the email; login
  checks a dummy hash when the email is unknown. Password hashing runs on blocking threads,
  at most `PUSH_AUTH_HASH_CONCURRENCY` (default 2) at once, 19 MiB each, the gate's permit
  held inside the blocking task until the hash is done; at most `PUSH_AUTH_HASH_QUEUE`
  (default 32) requests wait, each at most 2 s, and the rest get a retryable
  `unavailable` at once.
- **Inputs.** An email is normalised first and its length (≤ 254 bytes) checked after
  that. Passwords have 8–128 characters, used as typed.
- **Rate limits** (fixed windows in Redis, keyed by a SHA-256 of the subject):

  | Action | Per IP | Per account and IP | Per account |
  |---|---|---|---|
  | signup | 20 / hour | — | 5 / hour (email) |
  | login | 60 / 15 min | 10 / 15 min | 100 / hour (email) |
  | verification resend | 20 / hour | — | 3 / hour (email) |
  | password forgot | 20 / hour | — | 3 / hour (email) |
  | opening a link (verify, reset) | 60 / hour | — | — |
  | handle lookup | 120 / hour | — | 20 / hour (user) |

  Limits are counted in the order of the table; a refused request is not counted by the
  later ones, so a guesser stopped per account and IP does not use up the account's
  ceiling. An IPv6 client counts by its /64 prefix. The client IP comes from the last line
  of the configured header (`X-Real-IP`), then its last comma-separated entry; without the
  header, the peer address. Trust assumption: exactly one proxy in front that overwrites
  or appends to the header. Without such a proxy clients can spoof the header, which only
  weakens rate limiting. The per-account ceiling (100 / hour) lets an attacker with about
  10 addresses lock an account owner out of login for up to an hour; this is an accepted
  trade-off. When Redis does not answer within 500 ms the request is refused with a
  retryable `unavailable` and a warning is logged (fail closed); refresh is not limited,
  so signed-in devices keep working.
- **Profiles.** `GET /v1/users/lookup` finds verified users only. `GET /v1/users?ids=…`
  needs to know who shares a contact or conversation with the caller, which only
  message-svc knows; it comes with Plan 7.
- **DEV ONLY mailbox.** Until Plan 6B auth-svc served the outbox's emails at
  `/dev/mailbox`; the worker's log sender keeps them now (§9), and auth-svc has no mailbox.
- **Deferred / open (Plan 5A review).** Refresh sessions slide (30 d) with no absolute
  cap — an owner decision for later. Display names allow bidi and zero-width characters.
  Rate-limit key subjects use plain SHA-256 and should use HMAC. Form POSTs of the link
  pages return JSON errors, not HTML. Path, Query, 404 and 405 rejections are plain text.
  `Bearer` is matched case-sensitively. `?token=` is kept out of ingress access
  logs (Plan 5B; the nginx error log is Plan 11). A password reset revokes refresh tokens but leaves devices active, so an
  access token issued before the reset keeps working for up to 15 minutes; later, a
  `password_changed_at` stamp should make `authenticate` and the gateway reject older
  tokens.

## 6. Groups (up to 100 members)

- `CreateGroup{title, member_ids[]}` — every member must be an accepted contact of the
  creator. The creator becomes an admin.
- Two roles. **Admins** (one or more) rename the group, add and remove members, and
  promote members to admin or demote admins to member. **Members** chat and can leave.
- A group always has at least one admin: the last admin cannot be demoted. If the last
  admin leaves, the current member who joined first (lowest
  `visible_from_seq`, then lowest user id) becomes admin, recorded as a system entry.
  This happens only on leaving: an admin can only be removed by another admin, so removal
  never leaves a group without an admin. Admins remove others; an admin who wants to go
  leaves. A group with no members left is
  closed.
- Every membership change is a **system entry** in the log, written under the same
  conversation lock as messages. Sends and membership changes are therefore strictly
  ordered: a removed member never receives anything after the removal entry. A system
  entry (`kind = 4`, `content_type = 2`, no device) carries the event as compact JSON with
  a format version, for example `{"v":1,"type":"members_added","actor":"…","users":["…"]}`.
  The types are `created`, `members_added`, `member_removed`, `member_left`, `renamed`,
  `role_changed` and `admin_promoted`; clients show an unknown type as a generic line.
- A new member sees history from `visible_from_seq` (the seq of the "added" entry), and
  nothing before it counts as unread: their read position starts at
  `visible_from_seq - 1`. A member who leaves keeps history up to `left_at_seq` and still
  lists the group as it was when they left, without its member list. Rejoining sets a new
  `visible_from_seq` and clears `left_at_seq`.
- The 100-member limit is checked under the conversation lock.
- Adding a member requires them to be an accepted contact of the admin adding them. Inside
  a group, members need not be contacts of each other, and blocks between them do not
  affect the group.
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
| Deliver | `conversation_id, seq, message_id, client_msg_id, kind, target_seq, sender_id, content_type, body, client_ts, server_ts, edited_at, edit_seq, deleted` — `target_seq` is set on edit and delete entries: the `seq` of the message they change, as the log stores it |
| SendAck | `client_msg_id, message_id, seq, server_ts` (also for Edit and Delete) |
| Receipt | direct: `conversation_id, user_id, delivered_seq, read_seq`; group: `conversation_id, all_delivered_seq, all_read_seq` |
| ReadPosition | `conversation_id, read_seq` — the user's own read position moved |
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

### 7.6 Encoding and the internal API

Implementation notes (push-proto, Plan 4A):

- `crates/push-proto/proto/push/v1/frame.proto` holds the client frames,
  `push/internal/v1/messaging.proto` the gateway's gRPC calls to message-svc. Rust code is
  generated at build time with a vendored `protoc`, so building needs no system protoc.
- Identifiers are 16-byte UUIDs; times are microseconds since the Unix epoch, UTC.
- Frame field numbers: client frames 1–31, server frames 32–63. Reserved field numbers:
  `ephemeral` 16, `urgent` 17 and media `attachments` 18 on `Send` and `Deliver`;
  `private_reads` 16 on `Identify`.
- A failed gRPC call carries the client's `Error` frame, encoded, in the status details;
  the gateway forwards it, adding the `client_msg_id` of the `Send` it answers. Storage
  failures and conflicts are `retryable`.
- message-svc publishes a small `Notification` (entries available, receipt, contact
  change, read position; never a body) on `notify:{user_id}` after every commit. The
  gateway subscribes to the channels of the users it holds and syncs their devices; it
  does not send an entry twice on one connection, so repeated syncs without acks are
  harmless.
- **DEV ONLY identity, until real tokens (Plans 5 and 8).** A gateway started with
  `PUSH_DEV_AUTH=1` accepts `Identify{dev_user_id, dev_device_id}` without a token;
  otherwise `Identify` closes with 4001. message-svc started with `PUSH_DEV_SETUP=1`
  serves the `DevSetup` gRPC service (register users and devices, make contacts, open a
  direct conversation). Both variables accept only `1` or `0`, and both services log a
  warning at start-up when enabled. When the dev fields are removed (Plan 8), their field
  numbers (14, 15) and names are reserved.
- **Start-up and runtime behaviour (message-svc).** It fails at start-up, without
  retrying, on an invalid database or Redis URL and on `PUSH_DB_MAX_CONNECTIONS=0`; error
  messages never echo URLs. Store timeouts must be between 1 ms and `i32::MAX` ms.
  Start-up connect retries (every 2 s) are interruptible by shutdown. Services migrate
  with `push_store::migrate_database` on a dedicated connection, not the request pool.
- **Start-up and runtime behaviour (gateway).** The Redis hub reconnects every 1 s and
  rings every local session after re-subscribing; its subscription commands are bounded
  at 5 s and interruptible by shutdown.

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

Implementation notes (push-email, push-worker, Plan 6A):

- Templates `verify_email`, `account_exists` and `reset_password`, as plain text and minimal
  HTML with escaped parameters; the product name is configurable.
- `ResendSender` posts to `/emails` with the email's id as `Idempotency-Key` and a
  `template` tag; 408, 429, 409, 5xx, 401/403 and transport errors are retried, other refusals
  fail the email. It uses rustls on `ring`.
- `ResendWebhooks` checks the Svix signature (`v1,` HMAC-SHA256 of
  `{svix-id}.{svix-timestamp}.{body}`) and refuses timestamps more than 5 minutes off;
  `email.bounced` with `bounce.type` `Permanent` and `email.complained` add the recipients to
  `email_suppression`, `email.delivered` is recorded, other types are ignored.
  `POST /v1/webhooks/resend` is served only with a webhook secret; it answers 401 to a
  missing, wrong or stale signature and 204 to a signed body it cannot read (logged and
  dropped: Resend retries anything but a 2xx for days).
- With `PUSH_DEV_MAILBOX=1` (log sender only) the worker keeps the last 1000 emails it
  delivered in memory and serves them at `GET /dev/mailbox?to=…` (DEV ONLY).

Implementation notes (Plan 6B): the chart's default sender is `log`; `resend` needs
`worker.email.resendApiKey` and `worker.email.from` and is refused in dev mode. The webhook
Ingress routes only `/v1/webhooks/resend` (Exact), and only when a webhook secret exists
(generated in dev mode). Verified on minikube: a signed webhook through the ingress is
recorded once and suppresses a hard bounce; an unsigned one answers 401.

Azure Communication Services Email is not used: it is being retired (onboarding closes
2026-10-23, service ends 2028-09-30).

Owner decisions (2026-10-13, Plan 6):

- Emails come from `no-reply@push.kubegames.dk` and name the product "Push Messenger"
  (`Push Messenger <no-reply@push.kubegames.dk>`).
- Real sending through Resend comes with the Azure plan (Plan 11), with the API key and
  the webhook secret in Key Vault and the domain's SPF, DKIM and DMARC. Until then the
  chart's default sender is the log sender: a deployment sends no email.
- An email gives up after 12 attempts, about 30 minutes, when its link has expired.
- Emails are plain English text plus minimal HTML for now.
- The sent-email log, delivery events and buried jobs are kept 30 days; the suppression
  list is kept forever. Accounts left unverified are deleted after 7 days.

## 10. Repository layout

```
push-messenger/
├── crates/
│   ├── push-proto        protobuf types, validation rules, shared limits
│   ├── push-domain       messaging logic over repository traits; no I/O
│   ├── push-testkit      the domain's behaviour suite and storage contract, run
│   │                     against every store (not published)
│   ├── push-store        Postgres implementations (sqlx) and migrations
│   ├── push-email        email traits; Resend and log implementations
│   ├── push-gateway      binary
│   ├── push-message-svc  binary
│   ├── push-auth         binary
│   ├── push-worker       binary
│   ├── push-sdk          client core
│   └── push-smoke        end-to-end smoke test of a running deployment (not published)
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

Implementation notes (local minikube, Plan 4B):

- One Helm chart, `deploy/helm/push-messenger`: the gateway and message-svc
  (Deployments), PostgreSQL 17 (StatefulSet with a volume, database `push_messaging`
  owned by the user message-svc migrates as) and Redis 7 (AUTH on, no persistence), and an
  Ingress for `/v1/socket` with ingress-nginx's read and send timeouts at 120 s. Database
  and Redis passwords are generated on install and kept in one Secret; the services read
  their connection URLs from it. Probes: `/healthz` and `/readyz` on port 8081 of each
  service.
- Images (`deploy/docker/Dockerfile`, one target per binary from one shared build stage)
  run as a non-root user on distroless, with a read-only root filesystem.
- auth-svc (Plan 5B) is a third Deployment with its own database `push_auth` and role
  `push_auth`, created by a PostgreSQL init script (a ConfigMap without credentials; the
  password comes from the Secret). `REVOKE ALL … FROM PUBLIC` on both databases keeps
  auth-svc's plain role out of `push_messaging`; message-svc still connects as the
  superuser `push`, which can reach every database, until Plan 11. The Secret also holds
  the generated Ed25519 seed and HMAC key (`randBytes 32`). A second Ingress routes
  `/v1/auth`, `/v1/devices` and `/v1/users` to it; the public URL and the token audience
  come from `ingress.scheme` and `ingress.host` (plain `http` only for a local host).
  The token-carrying link pages (`/v1/auth/verify`, `/v1/auth/password/reset`) have their
  own Ingress, `push-auth-links`, with access logging off; verified on minikube: no
  `?token=` reached the controller access log. The nginx error log may still show request
  lines on upstream errors (Plan 11). Like the gateway, auth-svc has a 5 s preStop sleep
  and a 30 s termination grace period.
- The worker (Plan 6B) is a fourth Deployment with its own database `push_worker` and role
  `push_worker`, created by a second init script, which also grants the role `CONNECT` on
  `push_auth`; auth-svc grants it `auth.outbox` and two cleanup functions at start-up
  (`PUSH_AUTH_WORKER_ROLE`). The Secret holds its password and two URLs, the Accounts token
  (generated and kept; message-svc and the worker read it, the gateway does not), and
  Resend's API key and webhook secret when set. Its only Ingress routes `Exact`
  `/v1/webhooks/resend`; its DEV ONLY mailbox is not routed, and dev mode allows one worker
  only. It has a 5 s preStop sleep and a 30 s grace period.
- The smoke client verifies the access token's audience (`socket_url` with its authority
  replaced by the Host override, `ws://push.local/v1/socket` on minikube) and the
  refreshed token's `sub` and `device_id`. It never prints response bodies, tokens or
  token-bearing URLs, retries only on 502, 503, 504 and transport errors, and ignores
  `HTTP_PROXY`. The per-IP signup limit (20/h) caps smoke runs to about 10 per hour on a
  long-lived cluster.
- Deferred to Plan 11: validate hand-set auth keys at render time; `x-real-ip` on Azure
  needs `externalTrafficPolicy: Local` or the PROXY protocol; nginx error-log token exposure.
- Shutdown: the gateway has a `lifecycle.preStop.sleep.seconds: 5` (ingress-nginx stops
  routing to it before SIGTERM) and all three Deployments (gateway,
  message-svc, auth-svc) set `terminationGracePeriodSeconds: 30`.
- Hardening: Redis runs non-root (uid 999) with a read-only root filesystem; the data pods
  use `automountServiceAccountToken: false` and the `RuntimeDefault` seccomp profile.
- The chart Secret carries `helm.sh/resource-policy: keep`, so `helm uninstall` keeps the
  Secret and the PVC and the passwords keep matching the database. `down` wipes the
  Secret, PVC, PVs and the hostpath directory.
- `deploy/minikube/up.ps1` (and `up.sh`) wait for ingress-nginx, stop at once with
  "run down.ps1 first" when an existing PostgreSQL volume lacks the `push_auth` or
  `push_worker` database (or the kept Secret their passwords), retry
  `helm upgrade --install` up to 5 times, and bring it up on minikube; `helm test` runs
  `push-smoke` inside the cluster through the ingress controller: two users sign up, verify
  with the emails the worker's log sender delivered to the DEV ONLY mailbox, register
  devices, wait for the worker to relay them to message-svc, and chat. The Azure plan
  replaces the in-chart secrets with Key Vault and adds TLS, and may replace the in-chart
  PostgreSQL and Redis.

## 12. Testing

| Layer | Scope |
|---|---|
| Unit | `push-domain` with in-memory repositories: seq allocation, retry replay, positions, receipts, edit/delete rules, contacts, group membership, validation. The scenarios and the storage contract live in `push-testkit` and are written once: every store runs all of them |
| Integration | Real Postgres and Redis via testcontainers: the whole `push-testkit` suite against Postgres, transaction order, gapless `seq` under concurrent retries, outbox relay, `SKIP LOCKED` |
| Protocol | Raw-frame harness: malformed frames, out-of-order acks, replayed `client_msg_id`, unverified email, revoked device, non-contact send |
| End-to-end | SDK clients through the full stack: register, verify, connect via invite, chat, receipts, edit, delete, group of 100, new device loading history |
| Load | 10,000 concurrent sockets: memory per connection, latency, recovery from a reconnect storm, 100-member group throughput |

Required adverse-condition tests:

1. Connection dropped mid-send: the message is stored once and the retry returns the
   original `seq`.
2. Two identical sends at the same moment: one row, no gap in `seq`.
3. message-svc restarted mid-send: nothing acknowledged is lost.
4. Redis unavailable: delivery continues through reconnect and the 5-minute sweep, only
   slower. Signup, login and the other rate-limited auth endpoints answer a retryable
   `unavailable` until Redis is back; signed-in devices keep refreshing.
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
- The production sender is `no-reply@push.kubegames.dk` (decided 2026-10-13); its DNS
  (SPF, DKIM, DMARC) and Resend's domain setup come with Plan 11.
- Calibrate Argon2id on the chosen VM size.
- Decide whether group endpoints ship in the first release or the second.
