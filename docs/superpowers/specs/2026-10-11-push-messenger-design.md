# push-messenger — v1 design

Date: 2026-10-11
Status: approved in brainstorming, pending written-spec review

## 1. Purpose and scope

push-messenger is a minimalistic messenger **backend plus client SDK**. We build the
server side, its API and an SDK; third parties build the UIs.

**In scope for v1**

- Account signup with email and password; email verification gates all API use.
- Token-based authentication from our own auth service.
- One-to-one conversations, modelled as a conversation with two members.
- Text messages.
- Store-and-forward in both directions: a client-side outbox and a server-side
  delivery queue.
- Per-message status: pending → sent → delivered → read.
- Full server-side history; a new device can backfill it.
- Multi-device-ready model: one account, several devices, each with its own delivery state.
- Rust SDK core with Node (napi-rs) and C ABI (cbindgen) bindings.
- Deployment to k3s on a single Azure VM, designed to scale out to multiple replicas.

**Out of scope for v1** — the design must not preclude these:

- Groups with more than two members. No schema change is needed, only membership APIs.
- Media (images, documents, audio). `content_type` and the outbox `parent_id` reserve room.
- End-to-end encryption. The message body is stored in exactly one place, so there is one
  thing to encrypt later.
- Push notifications to offline devices. The `urgent` flag and the worker reserve room.
- Email change, magic-link login, and audit-log storage.

**Scale target:** 10,000+ concurrent connections, load-tested.

Research behind these decisions: `docs/research/2026-10-11-prior-art.md` and
`docs/research/2026-10-11-email-providers.md`.

## 2. Architecture

Four services and two stores, on one k3s node behind ingress-nginx.

| Unit | Role | Client connections | Database | Scales on |
|---|---|---|---|---|
| **gateway** | Terminates WebSockets, validates JWTs locally, decodes frames, forwards to message-svc over gRPC, streams results back, subscribes to wake-ups for the devices it holds | yes | **none** | connection count |
| **message-svc** | All messaging logic: conversations, membership, devices, message log, `seq` allocation, delivery pointers, cursors, receipts | no | message tables | throughput |
| **auth-svc** | Signup, login, email verification, token issue and refresh, JWKS | no | auth tables | stateless |
| **worker** | Deferred jobs: email outbox, retention and cleanup; later large-group fanout and push | no | job and email tables | stateless |
| **PostgreSQL** | System of record | — | — | partition by `conversation_id` later |
| **Redis** | Wake-up signals and rate limits only — never message bodies | — | — | — |

**Key properties**

- **The gateway is transport only.** It has no database credentials and no business
  rules. It is the only component exposed to untrusted input, so compromising it yields
  live sockets and nothing else.
- **The queue is a table, not a daemon.** Pending delivery is rows in
  `delivery_pointer`. Redis pub/sub is a doorbell. Each gateway also drains its connected
  devices every 30 seconds, so a lost signal makes delivery late, never lost.
- **Auth is off the hot path.** The gateway verifies JWTs locally against cached JWKS,
  so a reconnect storm is bounded by our own capacity, not by auth-svc.
- **Ordering across replicas** comes from a per-conversation row lock on `seq`
  allocation: exactly one writer per conversation, however many replicas run.

Gateway ↔ message-svc uses gRPC over persistent HTTP/2. Public endpoints are
`wss://api.<domain>/v1/socket` and `https://api.<domain>/v1/auth/*`.

## 3. Data model

Each table is owned by exactly one service. No service reads another service's tables.

### auth-svc

```sql
user_account (
  user_id           uuid primary key,
  email             citext unique not null,
  email_verified_at timestamptz,          -- null = API access denied
  password_hash     text not null,        -- Argon2id PHC string
  display_name      text not null,
  created_at        timestamptz not null default now()
)

device (
  device_id    uuid primary key,          -- minted by auth-svc at first login
  user_id      uuid not null references user_account,
  display_name text,
  created_at   timestamptz not null default now(),
  revoked_at   timestamptz
)

refresh_token (
  token_hash  bytea primary key,          -- SHA-256; the raw token is never stored
  device_id   uuid not null references device,
  expires_at  timestamptz not null,
  revoked_at  timestamptz,
  rotated_to  bytea                       -- reuse detection
)
```

Email challenges (verification and password reset) are stateless; see §5.

### message-svc

```sql
conversation (
  conversation_id uuid primary key,
  kind            smallint not null,      -- 1 = direct, 2 = group (reserved)
  created_at      timestamptz not null default now()
)

conversation_member (
  conversation_id  uuid references conversation,
  user_id          uuid,
  joined_at        timestamptz not null default now(),
  read_seq         bigint not null default 0,  -- public read cursor
  private_read_seq bigint not null default 0,  -- never shared with others
  primary key (conversation_id, user_id)
)

member_device (                                -- message-svc's own device registry
  device_id     uuid primary key,
  user_id       uuid not null,
  registered_at timestamptz not null default now(),
  last_seen_at  timestamptz not null default now(),
  revoked_at    timestamptz
)

conversation_seq (
  conversation_id uuid primary key references conversation,
  next_seq        bigint not null default 1    -- the single-writer counter
)

message (
  conversation_id  uuid not null references conversation,
  seq              bigint not null,
  message_id       uuid not null unique,
  client_msg_id    uuid not null,              -- UUIDv7 from the client
  sender_id        uuid not null,
  sender_device_id uuid not null,
  content_type     smallint not null,          -- 1 = text
  body             bytea not null,
  client_ts        timestamptz not null,
  server_ts        timestamptz not null default now(),
  primary key (conversation_id, seq),
  unique (conversation_id, client_msg_id)
)

delivery_pointer (                             -- the hot table; rows deleted on ack
  device_id       uuid not null,
  conversation_id uuid not null,
  seq             bigint not null,
  enqueued_at     timestamptz not null default now(),
  primary key (device_id, conversation_id, seq)
)

device_cursor (
  device_id       uuid not null,
  conversation_id uuid not null,
  delivered_seq   bigint not null default 0,   -- highest contiguous acked seq
  primary key (device_id, conversation_id)
)
```

**Device registry.** message-svc never reads auth-svc's tables. The gateway passes the
`device_id` and `user_id` from the validated JWT on every call, and message-svc upserts
`member_device` on connect. A device that has never connected has no pointers; on first
connect it backfills through history.

**Stale devices.** The worker deletes pointers for devices not seen for 30 days and
marks them inactive. If such a device returns, it backfills through history like a new
device. Device revocation in auth-svc takes effect at the gateway within one access-token
lifetime; message-svc marks the device revoked on its next failed connect.

### worker

```sql
job (
  job_id        uuid primary key,
  kind          text not null,
  payload       jsonb not null,
  run_after     timestamptz not null default now(),
  attempts      int not null default 0,
  claimed_at    timestamptz,
  failed_reason text
)

email_outbox (
  email_id            uuid primary key,
  to_address          citext not null,
  recipient_domain    text not null,          -- for per-domain deliverability metrics
  template            text not null,
  params              jsonb not null,
  provider_message_id text,
  status              smallint not null,      -- queued | sent | failed
  created_at          timestamptz not null default now(),
  sent_at             timestamptz
)

email_event (
  event_id            uuid primary key,
  provider_message_id text not null,
  kind                smallint not null,      -- delivered | bounced | complained
  occurred_at         timestamptz not null,
  raw                 jsonb not null
)
```

Jobs are claimed with `SELECT … FOR UPDATE SKIP LOCKED`, so the worker scales to several
replicas without coordination, and a crashed worker's jobs are claimed again.

auth-svc writes to `email_outbox` in the same transaction as the account change
(transactional outbox). `email_outbox` belongs to the worker as a queue; auth-svc only
inserts into it.

## 4. Message flow and delivery states

### Send (Alice → Bob)

1. The SDK validates the message (§7.3). If it is invalid, `send()` returns an error and
   nothing is stored.
2. The SDK writes it to the local SQLite outbox under a new UUIDv7 `client_msg_id` and
   emits `LocalEcho`, so the UI shows it immediately.
3. The SDK sends a `Send` frame. The gateway forwards it to message-svc.
4. message-svc validates again and, in **one transaction**:
   - checks that the sender is a member of the conversation;
   - inserts into `message` with `ON CONFLICT (conversation_id, client_msg_id)`. On a
     conflict it returns the **original** `seq` and `message_id` and stops here;
   - otherwise allocates `seq` with `UPDATE conversation_seq … RETURNING`, which takes a
     row lock;
   - inserts a `delivery_pointer` for every active device of every member, except the
     sending device. This includes the sender's other devices;
   - advances the sending device's cursor.
5. **After commit**, message-svc returns `{client_msg_id, message_id, seq, server_ts}`.
   The gateway relays a `SendAck`; the SDK deletes the outbox row and emits `Sent`.
6. message-svc publishes `notify:{device_id}` on Redis for each target device. No body.
7. The gateway holding a target device calls `FetchPending(device_id)` and pushes
   `Deliver` frames in `seq` order per conversation.
8. The receiving SDK stores the message locally, then sends `Ack{conversation_id, seq}`.
9. message-svc deletes the pointer, advances `delivered_seq`, and sends a delivered
   receipt to the sender's connected devices.
10. When Bob views the message, his SDK sends `ReadCursor{conversation_id, seq}`.
    message-svc advances `read_seq` (forward only) and notifies the other members.

### Delivery states

| State | Meaning | Source |
|---|---|---|
| pending | in the local outbox | SDK, on `send()` |
| sent | durably stored on the server | message-svc, after commit |
| delivered | at least one of the recipient's devices stored it | recipient device ack |
| read | the recipient's read cursor has passed it | recipient SDK |

**Receipts are derived from cursors, not queued.** Per member of a conversation:

- `delivered_seq(user)` = the highest `device_cursor.delivered_seq` among that user's
  devices.
- `read_seq(user)` = `conversation_member.read_seq`.

A message is delivered when the recipient's `delivered_seq` ≥ its `seq`, and read when
`read_seq` ≥ its `seq`. Live changes are pushed as `Receipt` frames to connected devices.
After connecting, a client receives one `ConversationState` frame per conversation with
these cursors, so an offline sender catches up without anything having been queued.

Other rules:

- **Read is an up-to cursor**, not per message. Backward moves are ignored. The honest
  claim is "Bob has read up to seq N".
- **Private reads.** A client may send `ReadCursor{private: true}`, which moves
  `private_read_seq` only: it clears the reader's own unread count without telling anyone.
- **Future groups** will aggregate read state as `min(read_seq)` across members rather
  than per-member ticks.

### Rules that must never be broken

- **Never acknowledge before commit.** Acknowledging first can lose messages silently.
- **Never put a message body on Redis.** Losing Redis must mean delay, not loss.
- **Cursors only move forward:** `GREATEST(current, new)`.

## 5. Authentication

### Credentials

| Credential | Proves | Lifetime | Stored |
|---|---|---|---|
| Access token (JWT, EdDSA) | "user X on device D, email verified" | 15 min | no; verified by signature |
| Refresh token | "may obtain new access tokens" | 30 days | yes; hashed and rotated |
| Email challenge | one specific action on one account | 30 min | no; stateless |

Access-token claims: `sub`, `device_id`, `email_verified`, `exp`, `iat`, `jti`, and
`aud` set to the gateway's actual URL.

Only access and refresh tokens are sign-in credentials. Email challenges grant exactly
one state change and cannot be exchanged for a session.

### Email verification

- Verification is a **gate**, not a credential. Following the link sets
  `email_verified_at` and nothing else; it never creates a session.
- Login succeeds for an unverified account, but the access token carries
  `email_verified: false`. The gateway closes such connections with 4001 and reason
  `email_unverified`; the only auth endpoint that accepts such a token is
  "resend verification".
- An email challenge is an HMAC-signed `{user_id, purpose, exp}` in a link.
  - **Verify email:** replay is harmless, so nothing else is bound.
  - **Password reset:** the signed payload also binds a hash of the current
    `password_hash`. Using the link changes the password, which invalidates the link —
    single use with no storage.
- Links are built from a configured base URL, never from the `Host` header. Pages served
  from those links send `Referrer-Policy: no-referrer`.
- Signup, resend and reset return the same response, in uniform time, whether or not the
  account exists, so they cannot be used to discover accounts.
- Rate limits in Redis, per account and per IP, on signup, login, resend and reset.
  Accounts are never locked; excess requests are refused.

### Passwords

Argon2id with `m=19456 KiB, t=2, p=1` (the OWASP minimum), stored as a PHC string so the
parameters can be raised on the next successful login. Calibrated to about 250 ms on the
production VM.

### Devices

auth-svc creates a `device_id` at first login. The client stores it with its refresh
token and presents it on later logins to reclaim the same device. The gateway learns the
device **only** from the validated JWT, never from a client-supplied field. Logout or
revocation revokes the refresh token and closes that device's socket.

Limit: 10 devices per account in v1.

### Finding other users

auth-svc exposes `GET /v1/users/lookup?email=…`, requiring a verified access token. An
exact email match returns `{user_id, display_name}`; anything else returns 404. It is
rate limited per user (20 per hour) to limit using it to test which emails are
registered. Directory search and contact invitations are out of scope for v1.

## 6. Wire protocol

WebSocket binary frames. Each carries one protobuf `Frame` with a `oneof` body. The
protobuf definitions live in `push-proto` and are the single contract between the SDK,
the gateway and message-svc.

### Client → server

| Frame | Fields |
|---|---|
| Identify | `token`. First frame of a new session; rate limited |
| Resume | `session_id`, `token`. Cheap; not rate limited |
| Send | `client_msg_id, conversation_id, content_type, body, client_ts, ephemeral, urgent` |
| Ack | `conversation_id, seq` |
| ReadCursor | `conversation_id, seq, private` |
| FetchHistory | `request_id, conversation_id, before_seq, limit` |
| OpenDirect | `request_id, peer_user_id` — returns the existing direct conversation or creates it |
| ListConversations | `request_id` |
| ReAuth | `token` |
| Ping | — |

Request frames carry a client-chosen `request_id`, echoed in the matching response.

### Server → client

| Frame | Fields |
|---|---|
| Ready | `session_id, user_id, device_id, heartbeat_ms, resume_window_s` |
| Resumed | `session_present` |
| ConversationState | `conversation_id`, and per member: `user_id, delivered_seq, read_seq` |
| Deliver | the full message, including `message_id, seq, server_ts` |
| SendAck | `client_msg_id, message_id, seq, server_ts` |
| Receipt | `conversation_id, user_id, kind (delivered or read), seq` |
| HistoryPage | `request_id, messages[], has_more` |
| Conversation | `request_id, conversation_id, kind, members[]` |
| ConversationList | `request_id, conversations[]` |
| AuthExpiring | `expires_in_s` |
| Error | `code, message, retryable`, plus `client_msg_id` or `request_id` when relevant |
| Pong | — |

### Flags

- `ephemeral` — not stored if the recipient is offline (for later typing indicators).
- `urgent` — eligible for push notification later.

### Sessions

- **Identify** starts a fresh session. On success the gateway subscribes to
  `notify:{device_id}`, publishes a "device connected" event so any other gateway holding
  the same device closes its socket with 4003, sends `Ready` and `ConversationState`, and
  then drains pending pointers.
- **Resume** reattaches within `resume_window_s` (default 120 s). Delivery state lives in
  Postgres, so resuming is always correct; `session_present` only tells the client
  whether it may skip re-sending in-memory state.
- **Token expiry.** About 60 s before `exp` the gateway sends `AuthExpiring`. The client
  refreshes over HTTPS and sends `ReAuth`, and the gateway swaps the security context
  without dropping the connection. Without a valid `ReAuth` by `exp + 30 s`, it closes
  with 4002.
- **Heartbeat.** The client pings every 30 s. The first ping is delayed by a random
  0–30 s so that clients reconnecting together do not stay synchronised.

### Close codes

| Code | Meaning | Client action |
|---|---|---|
| 4001 | Invalid token, or email not verified | stop; sign in again or verify |
| 4002 | Re-authentication required | refresh, then Resume |
| 4003 | Session taken over by another connection | stop |
| 4004 | Rate limited | back off, then Resume |
| 4005 | Server draining | back off with jitter, then Resume |
| 4006 | Session expired | Identify |

### Timeouts

| Layer | Default | Setting |
|---|---|---|
| Azure Load Balancer idle timeout | 4 min | 30 min, with TCP reset |
| nginx `proxy_read_timeout` and `proxy_send_timeout` | 60 s | 120 s |
| Client ping interval | — | 30 s |

## 7. SDK

### 7.1 Layering

- **`push-sdk`** (Rust core) owns everything: connection state machine, outbox, local
  message cache, retry and backoff, cursors and acks, reconnect, and the event stream.
- **`bindings/node`** (napi-rs) and **`bindings/c`** (cbindgen) only convert types and
  adapt async calls and callbacks. They contain no business logic.

### 7.2 Public surface

- `Client` — `connect`, `disconnect`, auth lifecycle, `send`, `mark_read`,
  `fetch_history`, `open_direct_conversation`, `list_conversations`. On construction it
  rescans the outbox and resumes sending.
- `SendHandle` — returned by `send`. Exposes `client_msg_id` and `cancel()`, which only
  works before the server has acknowledged the message.
- `SdkEvent` — a single stream: `LocalEcho`, `Sent`, `Delivered`, `Read`, `Received`,
  `SendFailed{client_msg_id, reason}`, `ConnectionState`.

### 7.3 Validation before queuing

`send()` validates synchronously with rules from `push-proto`, which the server shares:
non-empty body, at most 64 KiB of text, a supported `content_type`, and a known
conversation. An invalid message returns an error and is never stored. The server
validates again regardless.

### 7.4 Outbox

Local SQLite:

```sql
outbox (
  client_msg_id   blob primary key,   -- UUIDv7; stable across retries and restarts
  conversation_id blob not null,
  content_type    integer not null,
  body            blob not null,
  enqueued_at     integer not null,
  priority        integer not null default 0,
  attempts        integer not null default 0,
  parent_id       blob                -- reserved: upload → message dependency
)
```

- Messages are sent in `(priority desc, enqueued_at asc)` order per conversation.
- **Transient failures** (network errors, server errors, rate limits) are retried with
  exponential backoff and jitter, from 1 s up to 60 s, for as long as the message is
  queued.
- **State rejections** (not a member, account suspended) remove the message from the
  outbox, emit `SendFailed`, and let the next message proceed. Because the rejected
  message never exists on the server, later messages cannot overtake it.
- A row is deleted only when a `SendAck` with its `client_msg_id` arrives, so the outbox
  is exactly the set of pending messages.

### 7.5 FFI rules

- Every `extern "C"` function is wrapped in `catch_unwind`.
- The core owns exactly one tokio runtime, created by `Client::new`.
- C callers receive events through a registered callback and context pointer, called on
  an SDK thread. This is documented.
- Every returned allocation has a matching `*_free`.
- Node receives events through a `ThreadsafeFunction`.

## 8. Email

- Provider: **Resend**, behind provider-neutral traits:
  - `EmailSender::send(message, idempotency_key)` returns a `provider_message_id`;
  - `WebhookVerifier::verify` and `parse` turn provider webhooks into `DeliveryEvent`s.
- Mail is sent from our own domain with SPF, DKIM and DMARC.
- Open and click tracking are **off**; tracking pixels hurt placement of transactional
  mail.
- The worker drains `email_outbox`, using `email_id` as Resend's idempotency key.
- Bounce and complaint webhooks are stored in `email_event`.
- **Deliverability is measured, not assumed.** Resend has no independent placement data,
  so we track verification-completion rate grouped by `recipient_domain`. That metric is
  the signal for switching provider, and the traits make switching a configuration change.
- Local development uses a log-only `EmailSender`.

Azure Communication Services Email is not used: it is being retired, with onboarding
closing on 2026-10-23 and service ending on 2028-09-30.

## 9. Repository layout

```
push-messenger/
├── crates/
│   ├── push-proto        protobuf types, validation rules, shared constants
│   ├── push-domain       messaging logic over repository traits; no I/O
│   ├── push-store        Postgres implementations (sqlx) and migrations
│   ├── push-email        EmailSender traits; Resend and log implementations
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

## 10. Deployment

- k3s on one Azure B-series VM, with ingress-nginx and cert-manager (Let's Encrypt).
- PostgreSQL and Redis run in the cluster for v1, with persistent volumes and Redis AUTH.
- Secrets (database password, JWT signing key, Resend API key, HMAC key) come from Azure
  Key Vault through the CSI driver — never from git or ConfigMaps.
- Gateway pods have a `preStop` hook that closes connections in batches with 4005, and a
  `terminationGracePeriodSeconds` longer than that drain.
- Images are published to GHCR and deployed with Helm.
- Region, VM size and domain are confirmed with the owner before any cloud spend.

## 11. Testing

| Layer | Scope |
|---|---|
| Unit | `push-domain` with in-memory repositories: seq allocation, idempotent replay, cursor monotonicity, receipt derivation, validation |
| Integration | Real Postgres and Redis via testcontainers: the transaction boundary, `ON CONFLICT` replay, `SKIP LOCKED` claiming |
| Protocol | Raw-frame harness: malformed frames, out-of-order acks, replayed `client_msg_id`, resume after the window, unverified email |
| End-to-end | Two SDK clients through the full stack: send, deliver, receipts, history backfill on a new device |
| Load | 10,000 concurrent sockets: memory per connection, message latency, recovery from a reconnect storm |

Required adverse-condition tests:

1. Connection dropped mid-send: the message is delivered exactly once, and the resend
   returns the original `seq`.
2. message-svc restarted with messages in flight: nothing acknowledged is lost.
3. Redis unavailable: delivery continues through reconnects and the periodic drain, only
   slower.
4. Two connections for one device: the older one is closed with 4003 and nothing is
   delivered twice.
5. Token expires on a live connection: `ReAuth` keeps it open; without it, the gateway
   closes with 4002.
6. Sender offline when the recipient reads: on reconnect, `ConversationState` shows the
   message as read.

## 12. Open items for implementation

- Confirm the Azure Load Balancer idle-timeout annotation name against the
  cloud-provider-azure documentation before writing manifests.
- Choose the production domain and the Resend sending subdomain.
- Calibrate Argon2id timing on the chosen VM size.
