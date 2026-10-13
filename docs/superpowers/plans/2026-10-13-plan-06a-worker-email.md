# Plan 6A — Worker and Email: the Services Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The worker of spec §2, §3.3 and §9 as tested code: `push-email` (the email templates, a provider-neutral `EmailSender` with a log-only sender for local work and a Resend sender, and a `WebhookVerifier` that checks Resend's Svix signatures and turns bounces and complaints into delivery events) and `push-worker` (its own database `push_worker` with the `worker` schema; a job queue claimed with `FOR UPDATE SKIP LOCKED`, leased, retried with backoff and burying poison; the outbox relay that moves auth-svc's events into that queue at least once; the relay of `user.verified`, `device.registered`, `device.revoked` and `user.password_reset` to a new internal message-svc API; `email.send` rendered, checked against the suppression list, recorded and sent once; Resend webhooks feeding the suppression list; and the cleanup jobs — expired refresh tokens, accounts left unverified, the worker's own old records). message-svc gains the `Accounts` gRPC API, idempotent, correct in any order and callable only with the worker's shared internal token, and its `Identify` now tells a device not relayed yet (retryable) from a revoked one; auth-svc gains what the worker's role may do in its database: drain `auth.outbox` and call two cleanup functions, nothing else. Everything is tested against Postgres containers, an in-process message-svc and a stand-in for Resend. Plan 6B moves the smoke test to the real flow, adds the worker to the image and the chart and verifies it on minikube.

**Architecture:** auth-svc keeps writing events to `auth.outbox` in the same transaction as each change (Plan 5A). The worker connects to two databases: its own, as the owner `push_worker`, and auth-svc's `push_auth` as the same role, which auth-svc grants (at start-up, `PUSH_AUTH_WORKER_ROLE`) `SELECT`/`DELETE` on `auth.outbox` (plus `UPDATE` of `created_at` alone, which `FOR UPDATE` requires) and `EXECUTE` on two `SECURITY DEFINER` functions. The **relay** locks a batch of outbox rows (`FOR UPDATE SKIP LOCKED`), inserts them into `worker.job` with the event id as the job id (`ON CONFLICT DO NOTHING`, committed in the worker's database), then deletes them from the outbox and commits there: at least once, never lost, and a second copy is ignored. The **job loop** claims at most 4 due jobs at a time with `UPDATE … WHERE job_id IN (SELECT … FOR UPDATE SKIP LOCKED) RETURNING`, which leases them for 2 minutes — 4 attempts of at most about 15 s each fit in it with room to spare; each job ends `Done` (deleted), `RunAgain` (periodic), `Failed` (retried after 1 s doubling to 10 minutes) or `Poison` (buried with `dead_at`, kept for inspection without an email's link). An outcome is recorded only under the claim it ran under (`attempts` and `claimed_at`), so a late one never overwrites a newer claim, and at shutdown the jobs not started are given back. Account events go to message-svc's new `Accounts` gRPC service, with the shared internal token in their metadata (the gateway has none), whose calls are idempotent and commute: a registration also records the user, and a revocation that arrives first records the device as revoked, so a late registration cannot revive it. `email.send` jobs are rendered by `push-email`, refused for suppressed addresses, recorded in `worker.email_message` (without the link) and sent with the job id as the idempotency key. `POST /v1/webhooks/resend` verifies the Svix signature within a 5-minute window, records the event once by its `svix-id`, and suppresses hard bounces and complaints; a signed body it cannot read is logged and acknowledged, since Resend retries anything but a 2xx for days. The cleanups are periodic jobs with fixed ids in the same queue, so one replica runs each. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §2, §3.1, §3.3, §5.3, §5.4, §9, §11, §12.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85). New: `reqwest` 0.12's `rustls-tls` feature for `push-email` only (rustls 0.23 on `ring`, webpki roots: no OpenSSL, no cmake — it builds on 1.85 and on Windows; verified against `api.resend.com` while writing this plan). Already in the workspace: `axum` 0.8, `sqlx` 0.8, `tonic` 0.14, `hmac` 0.12, `sha2` 0.10, `base64` 0.22, `chrono`, `testcontainers-modules` 0.13; for tests `tower` 0.5 and `http-body-util` 0.1.

---

## Before you start

- **Plan 5B is merged** (PR #9). This plan adds the crates `push-email` and `push-worker`,
  the `Accounts` API to message-svc (with the domain and store calls behind it), and a
  migration, a start-up grant and a signup tweak to auth-svc. Plan 6B does the smoke test,
  the image, the chart and minikube.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-06a-worker-email
```

- Fix a stale PATH for the current PowerShell session (each new shell needs it again):

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
```

- **Docker must be running.** The tests start `postgres:17-alpine` and `redis:7-alpine`
  containers. A run killed half-way can leave containers behind (`docker ps -a`, then
  `docker rm -f <id>` for the `postgres` and `redis` ones only — never touch other
  containers on this machine).
- **No Resend account is needed.** The Resend sender is tested against a small axum stand-in
  on `127.0.0.1`, the webhook verifier against Svix's published example, and nothing in this
  plan calls `api.resend.com`.
- **Edit files with the edit tool, and keep them LF** (`.gitattributes`:
  `* text=auto eol=lf`). The file-creation tool may write CRLF; git normalises it on commit.
- **Do not pipe `cargo fmt`.** Run `cargo fmt --all -- --check` on its own and read the exit
  code. The code in this plan is formatted, so a diff means a copying mistake.
- **Replace blocks.** "Replace … with …" pairs below are exact copies of the file before and
  after the change, with two lines of context (more where two would not make the first block
  unique): find the first block in the file and replace it with the second.
- Baseline (after Plan 5B): `cargo test --workspace --locked` passes 706 tests.
- **Cargo.lock.** Tasks that add a crate or a dependency first run `cargo test -p <crate>`
  without `--locked`, which records it; the gates then run with `--locked`. Resolver 3 with
  `rust-version = "1.85"` picks versions that build on 1.85. Never run `cargo update`.
- **Every task ends green:**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

  Never add `#[allow(...)]`: every item a task adds is used by its code, tests or public
  API.
- **Postgres tests connect to `127.0.0.1`, never `localhost`** (on Windows `localhost`
  tries IPv6 first and every connect costs about 2 s). Testcontainers' Postgres is
  `postgres`/`postgres`.
- **Scope rule:** no changes to `push-gateway` beyond one test (Task 1: a device not
  relayed yet is no longer refused like a revoked one); no Dockerfile, chart or scripts, and
  in the smoke test only message-svc's new configuration field (Plan 6B); the DEV ONLY mailbox of auth-svc stays until Plan 6B moves it to the worker; no
  kick signals (Plan 7) and no token checks in the gateway (Plan 8).

## Key rules the code must preserve

- **Each service owns its database** (spec §2). The worker reads auth-svc's database only
  through what auth-svc grants its role: `auth.outbox` and two cleanup functions. It never
  sees accounts, passwords or refresh tokens.
- **At least once, never lost.** An outbox row is deleted only after its job is committed in
  the worker's database; a job is deleted only after it succeeded. Everything downstream is
  idempotent: job ids are event ids, an email's id is its job id and Resend's idempotency
  key, a webhook is recorded once by its `svix-id`, and message-svc's `Accounts` calls are
  idempotent.
- **Any order converges.** Account events may arrive late, twice or reversed (several
  workers, retries); every order ends in the same state. A device revoked once stays
  revoked.
- **Poison is buried, not retried forever, and never lost:** a payload that does not fit, a
  device id of another user, an unknown template or a refusal by the provider ends the job
  with `dead_at` and a reason; it stays in `worker.job` for inspection (and is deleted after
  the retention period).
- **Several workers can run.** Claims and drains use `FOR UPDATE SKIP LOCKED`; a claim is a
  lease, so a crashed worker's job comes back; a claimed batch fits in its lease; an
  outcome is recorded only under the claim it belongs to, never over a newer one; jobs
  claimed but not started are given back at shutdown; periodic jobs have fixed ids, so one
  replica runs each.
- **Only the worker calls `Accounts`.** message-svc refuses every `Accounts` call that
  lacks the shared internal token (`authorization: Bearer …`, compared in constant time),
  and every call when it has no token configured. The gateway never gets the token.
- **Nothing secret is logged or kept longer than needed:** logs name a job by id, kind and
  attempt count, an email by id, template and recipient domain — never an address, a body,
  a link, a token or a key. `worker.email_message` keeps no link, and a buried job keeps
  its payload without `params.link`. Both services' `Config`, `AccountsToken`,
  `ResendSender`, `ResendWebhooks`, `OutgoingEmail`, `Rendered` and `DeliveryEvent` have
  redacting `Debug`.
  Errors never quote URLs, keys or Resend's messages (which can quote addresses).
- **Webhooks are verified before anything is read:** a Svix HMAC-SHA256 over
  `{id}.{timestamp}.{body}`, compared in constant time, with the timestamp within 5 minutes
  of our clock. Only what a retry could fix gets anything but a 2xx: a signed body that is
  not a readable delivery event is logged and answered 204.
- **DEV ONLY switches are strict:** `PUSH_DEV_MAILBOX` accepts exactly `1` or `0`/empty, is
  refused with any sender but `log`, and logs a warning at start-up.

## Decisions made for this plan

These were settled while writing the plan, revised after its review, and verified in a
worktree. The owner's decisions are listed under "Owner decisions" below; all are final.
Do not reopen any of them during execution.

1. **Plan 6 is split.** 6A (this plan) delivers the services, tested against containers;
   6B moves the smoke test to the real flow (the DEV ONLY mirror into message-svc goes, the
   mailbox moves to the worker), adds the worker to the image and the chart, and verifies it
   on minikube.
2. **Delivery semantics: at least once through a job queue.** The relay copies outbox rows
   into `worker.job` (job id = event id, `ON CONFLICT DO NOTHING`), commits there, and only
   then deletes them from the outbox; a crash in between re-copies rows the queue ignores.
   Workers claim with `FOR UPDATE SKIP LOCKED` and lease by moving `run_after` 2 minutes
   ahead (`attempts + 1`, `claimed_at = now()`), so several replicas share the work without
   coordination and a crashed worker's jobs come back. Within a claim, jobs run oldest first
   (UUIDv7 ids), one after another, so **a claim takes at most 4 jobs**: an attempt takes at
   most about 15 s (message-svc: a 5 s connect and a 10 s call timeout; Resend: a 10 s
   request timeout; statements of a few milliseconds; a cleanup run starts no batch after
   5 s, decision 13), so a batch ends within about 60 s of its 120 s lease. (The first draft
   claimed 50: later jobs of a batch outlived their lease, a second replica claimed and ran
   them as well, and emails used up their 12 attempts early.) **A claim is fenced:** every
   `UPDATE` and `DELETE` of `Queue::finish` also matches the claim's `attempts` and
   `claimed_at`, so an outcome that comes after the lease ended — when another worker may
   hold a newer claim — is dropped (`finish` returns `false`, the worker logs it).
   `claimed_at` is needed besides `attempts` because a periodic job's attempts start again
   at 0 after each run. **At shutdown** the jobs of a batch not started yet are given back
   (`Queue::release`: `run_after = now()`, `attempts - 1`, `claimed_at = NULL`, fenced the
   same way), so another replica takes them at once instead of after the lease.
3. **Retries and poison.** A failure worth retrying waits 1 s, doubling, at most 10 minutes
   (`backoff`). Account events retry for as long as it takes (message-svc must learn every
   one); an email gives up after 12 attempts — about half an hour, when its 30-minute link
   has expired anyway (owner decision 4) — and is then marked `failed`. Poison (a malformed payload, an unknown
   kind or template, `INVALID_ARGUMENT`/`PERMISSION_DENIED`/… from message-svc, a refusal by
   Resend) is buried at once: `dead_at`, `failed_reason`, never claimed again. A buried
   job keeps its payload for inspection without `params.link` (`payload #- '{params,link}'`),
   so a buried email cannot sign anyone in during the retention period. The schema of spec
   §3.3 gains `job.dead_at` and `job.created_at` for that.
4. **message-svc's `Accounts` API** (gRPC, `push.internal.v1.Accounts`) has one call per
   event: `UserVerified`, `DeviceRegistered`, `DeviceRevoked`, `PasswordReset`. It is always
   served (not DEV ONLY), to the worker alone (decision 17). Idempotency and ordering are the domain's: `device_registered`
   records the user too (auth-svc registers devices only for verified users), creates the
   device with its delivery positions, and is a no-op for a device already known for that
   user, revoked or not; `device_revoked` records user and device if they are new and marks
   the device revoked, so a revocation that overtakes the registration still wins; a device
   id that belongs to another user is `PERMISSION_DENIED` (poison). A new store call,
   `Tx::device_user`, names a device's owner whether or not it is revoked; `Identify` uses
   it to tell a device not relayed yet from a revoked one (decision 16).
5. **Kicks.** On `device.revoked`, message-svc marks the device revoked, so `Identify`
   refuses it from then on (a reconnect fails). `PasswordReset` is accepted and otherwise
   ignored for now: publishing `kick:{device_id}` is Plan 7 (message-svc's Redis kick) and
   closing the socket and refusing tokens older than the reset is Plan 8 (gateway). The call
   exists now so the contract and the relay do not change then.
6. **The worker's access to auth-svc's database.** auth-svc's migration `0002_worker.sql`
   adds `auth.delete_expired_refresh_tokens(batch)` and
   `auth.delete_unverified_accounts(older_than, batch)`, `SECURITY DEFINER` with a pinned
   `search_path` and `EXECUTE` revoked from `PUBLIC`, so the cleanup rules stay in auth-svc's
   schema. At start-up, after migrating, auth-svc grants the role named by
   `PUSH_AUTH_WORKER_ROLE` `USAGE` on the schema, `SELECT, DELETE` and `UPDATE (created_at)` on
   `auth.outbox` (PostgreSQL refuses `FOR UPDATE` without some `UPDATE` right), and `EXECUTE`
   on the two functions. Grants in a migration would need the role to exist in every
   database a migration runs in (tests included) and would fix its name; the role name is
   checked (`[a-z_][a-z0-9_]{0,62}`) before it is quoted into SQL.
7. **Unverified accounts.** The cleanup deletes accounts whose email was never verified and
   that were created more than `PUSH_UNVERIFIED_ACCOUNT_DAYS` ago (7 days, owner decision 1);
   such an account has no devices, and one that has is kept. A new signup over an
   unverified account now also resets its `created_at`, so the cleanup never deletes an
   account whose fresh verification link is still valid.
8. **Expired refresh tokens** — rotated and revoked ones included — are deleted once past
   `expires_at`: nothing can use them, and reuse detection of a token older than that is
   moot. Both auth cleanups delete in batches of 1000, at most 50 batches a run, hourly.
9. **Email.** `push-email` renders three templates (`verify_email`, `account_exists`,
   `reset_password`) as plain text and minimal HTML with every parameter escaped and links
   restricted to `http(s)://`, in English (owner decision 5); the product name is
   configurable (`PUSH_EMAIL_PRODUCT_NAME`, default `Push Messenger`, owner decision 2). The worker checks
   `worker.email_suppression` first (a suppressed address is recorded as `suppressed` and
   skipped), records the email in `worker.email_message` (status `queued`, the params
   **without the link**), sends with the job id as the idempotency key, and records `sent`
   with the provider's id. An email already `sent`, `suppressed` or `failed` is never sent
   again.
10. **Senders.** `EmailSender::send(email, idempotency_key) -> provider_message_id`
    (spec §9). `LogSender` logs the email id, template and recipient domain and sends
    nothing; it is the default (`PUSH_EMAIL_SENDER=log`). `ResendSender` posts to
    `{PUSH_RESEND_API_URL}/emails` (default `https://api.resend.com`) with
    `Authorization: Bearer`, `Idempotency-Key` and a `template` tag; 429, 409 (a concurrent
    request with the same key), 5xx, transport errors **and 401/403** (a missing or wrong key:
    the emails wait while it is fixed) are retryable, other refusals permanent. Only Resend's
    error `name` is quoted, never its message. `http://` API URLs are accepted only for
    `127.0.0.1`/`localhost`, for tests. The From address is `PUSH_EMAIL_FROM`, which is
    `Push Messenger <no-reply@push.kubegames.dk>` in production (owner decision 2); real
    sending through Resend comes with Plan 11 (owner decision 3).
11. **Webhooks.** `WebhookVerifier::verify` checks Resend's Svix signature (`svix-id`,
    `svix-timestamp`, `svix-signature`: `v1,` + base64 HMAC-SHA256 of
    `{id}.{timestamp}.{body}` with the base64 key after `whsec_`; any of several signatures,
    for key rotation) and refuses timestamps more than **5 minutes** from our clock either
    way; `parse` maps `email.delivered`, `email.bounced` (permanent when `bounce.type` is
    `Permanent`) and `email.complained`, and ignores other types. The worker records each
    event once in `worker.email_event`, whose `event_id` is now `text` — the `svix-id`, the
    same on every redelivery, so a replay inside the window is a no-op (spec §3.3 had
    `uuid`). Hard bounces and complaints add to `worker.email_suppression` both the addresses
    the event names and the address the email was sent to. `POST /v1/webhooks/resend` answers
    204 (recorded, a replay, an event nothing acts on, or a signed body that is not a
    delivery event it can read — logged with its `svix-id` and dropped, since Resend sends
    anything but a 2xx again for days), 401 (unsigned, forged or stale) or 503 (database),
    and is served only when `PUSH_RESEND_WEBHOOK_SECRET` is set; bodies are at most 256 KiB.
12. **The DEV ONLY mailbox moves to the worker** (Plan 6B switches the smoke test and the
    chart and removes auth-svc's). With `PUSH_DEV_MAILBOX=1` and the log sender, a
    `MailboxSender` wraps `LogSender` and keeps the last 1000 emails it delivered in memory;
    `GET /dev/mailbox?to=…` shows the newest 20 for one address (template, subject, text).
    In memory, because it exists only beside the log sender on a local cluster with one
    worker (Plan 6B's chart refuses dev mode with more); refused with any other sender,
    since it shows sign-in links.
13. **Cleanups are jobs.** `cleanup.refresh_tokens`, `cleanup.unverified_accounts` and
    `cleanup.worker_records` have fixed ids, are added on every start (`ON CONFLICT DO
    NOTHING`), run hourly (`RunAgain`), and come back after a minute when a run left more
    than 50 batches or ran 5 s (it starts no batch after that, so with a 10 s statement
    timeout a run takes at most about 15 s, like any other job: inside its lease, and
    inside the pod's 30 s grace period with the 5 s preStop sleep). `cleanup.worker_records`
    deletes buried jobs, `email_message` and `email_event` rows older than
    `PUSH_WORKER_RETENTION_DAYS` (30 days, owner decision 6). The suppression list is never
    cleaned up.
14. **Latency.** An idle worker looks for work every `PUSH_WORKER_POLL_MS` (**1000**); the
    relay wakes the job loop at once when it moved events. Delivery of an email or a device
    to message-svc therefore takes about a second. `LISTEN/NOTIFY` is deferred.
15. **Readiness and shutdown.** `/readyz` turns ready once the worker's database is migrated
    and auth-svc's answers with the outbox readable — that is, once auth-svc has granted the
    role its access; message-svc is not waited for (its events wait in the queue). Start-up
    retries every 2 s and every loop select!s against shutdown; on SIGTERM the worker
    finishes the job in progress, gives back the jobs it claimed but did not start (due at
    once, for any replica; decision 2) and stops.
16. **A device not relayed yet is not a revoked device.** Between auth-svc's registration
    and the worker's relay (about a second) `Identify` refused a new device with the same
    `PERMISSION_DENIED`/`UNKNOWN_DEVICE` as a revoked one, which a client would take as
    "signed out". `Identify` now answers a device message-svc has never heard of with
    `UNAVAILABLE`, `ErrorCode::Unavailable` and `retryable: true`
    (`DomainError::DeviceNotKnownYet`, which `is_retryable`), and keeps `PERMISSION_DENIED`
    for a revoked device or one of another user. The gateway passes `UNAVAILABLE` on as
    close code 1011 for now, which clients retry; **Plan 8** gives it a close code of its
    own ("try again shortly"), and its SDK reconnects after a short backoff and never signs
    the user out for it (spec §5.3).
17. **Only the worker may call `Accounts`.** Until Plan 11's NetworkPolicy, any pod that
    reaches message-svc — a compromised gateway — could otherwise register devices. The
    `Accounts` service is wrapped in an interceptor, `AccountsToken`, that requires
    `authorization: Bearer <PUSH_ACCOUNTS_TOKEN>` metadata and compares it in constant time;
    message-svc without a token (a cluster from Plan 5B until 6B) refuses every call with
    `UNAUTHENTICATED` and warns at start-up. The worker sends the token
    (`PUSH_ACCOUNTS_TOKEN`, required, 32 to 256 visible ASCII characters, never shown); a
    refusal is retried, since the fix is configuration. Plan 6B generates the token into
    the kept Secret and gives it to message-svc and the worker only. The NetworkPolicy
    remains a Plan 11 must-do.
18. **`PUSH_MESSAGE_SVC_URL` is plain `http://`.** tonic is built without TLS, so an
    `https://` URL could never connect and every account event would wait forever; the
    worker's configuration refuses anything but `http://` at start-up.

## Owner decisions

The owner decided these on 2026-10-13; all are final and recorded in the spec (§5.2, §9,
§13). Each is a configuration value or a chart default, so a later change changes no code.

1. **Unverified accounts** are deleted after 7 days (`PUSH_UNVERIFIED_ACCOUNT_DAYS`, default
   7).
2. **Sender and product name:** `Push Messenger <no-reply@push.kubegames.dk>`
   (`PUSH_EMAIL_FROM`; the chart's default in Plan 6B) and "Push Messenger"
   (`PUSH_EMAIL_PRODUCT_NAME`, default), in the templates.
3. **Real sending through Resend comes with the Azure plan (Plan 11)**, with the API key
   and the webhook secret in Key Vault. The chart defaults to the log sender.
4. **An email gives up after about 30 minutes** (12 attempts).
5. **Plain English text plus minimal HTML** for now.
6. **Retention:** the sent-email log, delivery events and buried jobs are kept 30 days
   (`PUSH_WORKER_RETENTION_DAYS`, default 30); the suppression list is kept forever.

## File structure

```
Cargo.toml                                    + push-email, push-worker
crates/push-domain/src/store.rs, memory.rs    + Tx::device_user
crates/push-domain/src/error.rs               + DeviceNotKnownYet (retryable)
crates/push-domain/src/service.rs             + device_registered, device_revoked; identify
crates/push-store/src/store.rs                + device_user
crates/push-testkit/src/contract/users.rs     + 1 contract case
crates/push-testkit/src/scenarios/devices.rs  + 5 scenarios (relayed events in any order)
crates/push-proto/proto/push/internal/v1/messaging.proto   + service Accounts
crates/push-message-svc/src/grpc.rs, server.rs, lib.rs     + AccountsApi, AccountsToken
crates/push-message-svc/src/config.rs, convert.rs           PUSH_ACCOUNTS_TOKEN; UNAVAILABLE
crates/push-message-svc/tests/service.rs      Accounts over real Postgres, with the token
crates/push-gateway/tests/gateway.rs          a device not relayed yet is not 4001
crates/push-smoke/tests/local.rs              message-svc's new configuration field
crates/push-auth/migrations/0002_worker.sql   NEW: cleanup functions, indexes
crates/push-auth/src/db.rs                    + grant_worker_access, is_role_name
crates/push-auth/src/config.rs, server.rs     + PUSH_AUTH_WORKER_ROLE, granted at start-up
crates/push-auth/src/service.rs               signup over an unverified account resets created_at
crates/push-auth/tests/schema.rs, accounts.rs the worker's role and the cleanups
crates/push-email/                            NEW
├── Cargo.toml
└── src/
    ├── lib.rs                                exports
    ├── templates.rs                          Templates, Rendered: the three templates
    ├── sender.rs                             EmailSender, OutgoingEmail, SendError, LogSender
    ├── resend.rs                             ResendSender
    └── webhook.rs                            WebhookVerifier, ResendWebhooks, DeliveryEvent
crates/push-worker/                           NEW
├── Cargo.toml
├── migrations/0001_worker.sql                the worker schema (spec §3.3)
├── src/
│   ├── lib.rs, main.rs                       exports, the binary
│   ├── db.rs                                 MIGRATOR, migrate_database
│   ├── queue.rs                              Queue: enqueue, claim, finish, release, backoff
│   ├── relay.rs                              OutboxRelay
│   ├── accounts.rs                           AccountEvent, Accounts (gRPC client)
│   ├── email.rs                              Mailer, Mailbox, MailboxSender
│   ├── deliveries.rs                         Deliveries: webhooks into events and suppression
│   ├── http.rs                               /v1/webhooks/resend, /dev/mailbox
│   ├── jobs.rs                               Jobs: by kind
│   ├── cleanup.rs                            Cleanup, periodic jobs
│   ├── config.rs                             Config from the environment
│   └── server.rs                             serve, loops, health, logging, signals
└── tests/
    ├── queue.rs                              claims, leases, fences, release, burial
    ├── relay.rs                              outbox → queue → message-svc
    ├── email.rs                              sending, suppression, webhooks, HTTP
    ├── cleanup.rs                            the cleanups
    └── server.rs                             the worker as it runs
README.md                                     push-worker configuration, PUSH_AUTH_WORKER_ROLE
docs/superpowers/specs/2026-10-11-push-messenger-design.md   §3.3, §5.3, §9 notes
docs/superpowers/plans/2026-10-11-roadmap.md                 6A in review
```

---

### Task 1: message-svc's `Accounts` API: idempotent in any order, the worker's alone

**Files:**
- Modify: `crates/push-domain/src/error.rs`, `src/store.rs`, `src/memory.rs`,
  `src/service.rs`, `crates/push-store/src/store.rs`,
  `crates/push-testkit/src/contract/users.rs`, `crates/push-testkit/src/scenarios/devices.rs`,
  `crates/push-proto/proto/push/internal/v1/messaging.proto`,
  `crates/push-message-svc/src/config.rs`, `src/convert.rs`, `src/grpc.rs`, `src/server.rs`,
  `src/lib.rs`, `crates/push-message-svc/tests/service.rs`,
  `crates/push-gateway/tests/gateway.rs`, `crates/push-smoke/tests/local.rs`

- [ ] **Step 1: The storage contract and the scenarios, failing**

`Tx::device_user` names a device's owner, revoked or not (decision 4). In
`crates/push-testkit/src/contract/users.rs`:

Replace

```rust
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

with

```rust
}

async fn device_user_names_the_owner_of_revoked_devices_too<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let (active, revoked) = (DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(active, user).await.unwrap();
    tx.insert_device(revoked, user).await.unwrap();
    tx.revoke_device(revoked).await.unwrap();

    assert_eq!(tx.device_user(active).await.unwrap(), Some(user));
    assert_eq!(tx.device_user(revoked).await.unwrap(), Some(user));
    assert_eq!(tx.device_user(DeviceId::new()).await.unwrap(), None);
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

Replace

```rust
        a_device_of_an_unknown_user_is_refused,
        revoking_an_unknown_device_does_nothing,
    ]
}
```

with

```rust
        a_device_of_an_unknown_user_is_refused,
        revoking_an_unknown_device_does_nothing,
        device_user_names_the_owner_of_revoked_devices_too,
    ]
}
```

Relayed events in every order (decisions 4 and 5), and `Identify` asking to retry a device
not relayed yet (decision 16). In `crates/push-testkit/src/scenarios/devices.rs`:

Replace

```rust
use crate::{Backend, Case, World, case_list};
use push_domain::{DeviceId, DomainError, UserId};
```

with

```rust
use crate::{Backend, Case, World, bodies, case_list};
use push_domain::{DeviceId, DomainError, UserId};
```

Replace

```rust
}

async fn a_device_id_of_another_user_cannot_be_registered<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
```

with

```rust
}

async fn identify_asks_to_retry_a_device_not_relayed_yet<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    let refused = world.svc.identify(device, user).await.unwrap_err();
    assert_eq!(refused, DomainError::DeviceNotKnownYet);
    assert!(refused.is_retryable());
    world.svc.device_registered(device, user).await.unwrap();
    assert_eq!(world.svc.identify(device, user).await, Ok(()));
}

async fn a_device_id_of_another_user_cannot_be_registered<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn a_relayed_registration_records_the_user_too<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    world.svc.device_registered(device, user).await.unwrap();
    world.svc.device_registered(device, user).await.unwrap();
    world.svc.register_user(user).await.unwrap();
    assert_eq!(world.svc.identify(device, user).await, Ok(()));
}

async fn a_relayed_registration_starts_up_to_date<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    let tablet = DeviceId::new();
    world.svc.device_registered(tablet, bob.user).await.unwrap();
    assert!(
        world
            .svc
            .sync(tablet, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    world.send(&alice, chat, "two").await;
    let batch = world.svc.sync(tablet, 100).await.unwrap();
    assert_eq!(bodies(&batch.entries), ["two"]);
}

async fn a_revocation_relayed_before_the_registration_wins<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    world.svc.device_revoked(device, user).await.unwrap();
    world.svc.device_registered(device, user).await.unwrap();
    world.svc.device_revoked(device, user).await.unwrap();
    assert_eq!(
        world.svc.identify(device, user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn a_relayed_revocation_closes_an_active_device<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let tablet = world.add_device(&alice).await;
    world.svc.device_revoked(tablet, alice.user).await.unwrap();
    assert_eq!(
        world.svc.identify(tablet, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}

async fn relayed_events_for_another_users_device_are_refused<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    assert_eq!(
        world.svc.device_registered(alice.device, bob.user).await,
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(
        world.svc.device_revoked(alice.device, bob.user).await,
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
```

Replace

```rust
        identify_rejects_a_device_of_another_user,
        identify_rejects_a_revoked_device,
        a_device_id_of_another_user_cannot_be_registered,
        a_revoked_device_cannot_be_registered_again,
        registering_the_same_user_and_device_twice_is_harmless,
    ]
}
```

with

```rust
        identify_rejects_a_device_of_another_user,
        identify_rejects_a_revoked_device,
        identify_asks_to_retry_a_device_not_relayed_yet,
        a_device_id_of_another_user_cannot_be_registered,
        a_revoked_device_cannot_be_registered_again,
        registering_the_same_user_and_device_twice_is_harmless,
        a_relayed_registration_records_the_user_too,
        a_relayed_registration_starts_up_to_date,
        a_revocation_relayed_before_the_registration_wins,
        a_relayed_revocation_closes_an_active_device,
        relayed_events_for_another_users_device_are_refused,
    ]
}
```

```powershell
cargo test -p push-testkit
```

Expected: compile errors — no method `device_user` on the transaction, no methods
`device_registered` and `device_revoked` on `Messaging`, no variant `DeviceNotKnownYet` on
`DomainError`.

- [ ] **Step 2: The domain and both stores**

In `crates/push-domain/src/error.rs` (decision 16: the one error besides `Conflict` worth
retrying unchanged):

Replace

```rust
    #[error("unknown or revoked device")]
    UnknownDevice,
    #[error("conversation not found")]
    ConversationNotFound,
```

with

```rust
    #[error("unknown or revoked device")]
    UnknownDevice,
    /// message-svc has not heard of the device yet: auth-svc registered it, but the worker
    /// has not relayed it (that takes about a second). Unlike `UnknownDevice`, it passes.
    #[error("the device is not known yet; retry shortly")]
    DeviceNotKnownYet,
    #[error("conversation not found")]
    ConversationNotFound,
```

Replace

```rust
impl DomainError {
    /// Whether the same operation may succeed if the caller tries it again unchanged.
    /// Only `Conflict` is retryable: every other error repeats until something changes.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Conflict)
    }
}
```

with

```rust
impl DomainError {
    /// Whether the same operation may succeed if the caller tries it again unchanged.
    /// Only `Conflict` and `DeviceNotKnownYet` are retryable: every other error repeats
    /// until something changes.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Conflict | Self::DeviceNotKnownYet)
    }
}
```

Replace

```rust
    #[test]
    fn only_a_conflict_is_retryable() {
        assert!(DomainError::Conflict.is_retryable());
        assert!(!DomainError::NotMember.is_retryable());
        assert!(!DomainError::Store("connection lost".into()).is_retryable());
```

with

```rust
    #[test]
    fn only_a_conflict_and_a_device_not_known_yet_are_retryable() {
        assert!(DomainError::Conflict.is_retryable());
        assert!(DomainError::DeviceNotKnownYet.is_retryable());
        assert!(!DomainError::UnknownDevice.is_retryable());
        assert!(!DomainError::NotMember.is_retryable());
        assert!(!DomainError::Store("connection lost".into()).is_retryable());
```

In `crates/push-domain/src/store.rs`:

Replace

```rust
    /// Marks a device revoked. A no-op for an unknown device.
    async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()>;
    /// The owner of an active device; `None` if the device is unknown or revoked.
    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
```

with

```rust
    /// Marks a device revoked. A no-op for an unknown device.
    async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()>;
    /// The owner of a device, active or revoked; `None` if the device is unknown.
    async fn device_user(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
    /// The owner of an active device; `None` if the device is unknown or revoked.
    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
```

In `crates/push-domain/src/memory.rs`:

Replace

```rust
    }

    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        Ok(self
```

with

```rust
    }

    async fn device_user(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        Ok(self.work.devices.get(&device).map(|row| row.user))
    }

    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        Ok(self
```

In `crates/push-store/src/store.rs`:

Replace

```rust
    }

    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        let owner: Option<Uuid> = sqlx::query_scalar(
```

with

```rust
    }

    async fn device_user(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        let owner: Option<Uuid> =
            sqlx::query_scalar("SELECT user_id FROM messaging.member_device WHERE device_id = $1")
                .bind(device.0)
                .fetch_optional(self.conn())
                .await
                .map_err(store_error)?;
        Ok(owner.map(UserId))
    }

    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        let owner: Option<Uuid> = sqlx::query_scalar(
```

In `crates/push-domain/src/service.rs`, `register_device`'s cursor loop moves into
`create_cursors`, which the relayed registration shares, and `identify` tells a device it
never heard of from a revoked one:

Replace

```rust
            return Err(DomainError::UnknownDevice);
        }
        for (conversation, member) in tx.memberships(user).await? {
            let position = member.visible_upper(conversation.last_seq);
            tx.create_cursor_if_absent(device, conversation.id, position)
                .await?;
        }
        tx.commit().await
    }
```

with

```rust
            return Err(DomainError::UnknownDevice);
        }
        create_cursors(&mut tx, device, user).await?;
        tx.commit().await
    }
```

Replace

```rust
    }

    /// Publishes events after a commit. Publishes nothing when there is nothing to say.
    pub(crate) async fn publish(&self, events: Vec<Event>) {
```

with

```rust
    }

    /// auth-svc registered `device` for `user` (`device.registered`, relayed by the
    /// worker). Idempotent, and correct in any order with [`Self::device_revoked`] and
    /// [`Self::register_user`]: the user is recorded too, since auth-svc registers devices
    /// only for verified users, and a device already revoked stays revoked. A device id
    /// that belongs to another user is `UnknownDevice`.
    pub async fn device_registered(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.insert_user(user).await?;
        if tx.insert_device(device, user).await? {
            create_cursors(&mut tx, device, user).await?;
        } else if tx.device_user(device).await? != Some(user) {
            return Err(DomainError::UnknownDevice);
        }
        tx.commit().await
    }

    /// auth-svc revoked `device` of `user` (`device.revoked`, relayed by the worker).
    /// Idempotent and correct in any order: a revocation that arrives before the
    /// registration records the device as revoked, so the registration cannot bring it
    /// back. A device id that belongs to another user is `UnknownDevice`.
    pub async fn device_revoked(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.insert_user(user).await?;
        if !tx.insert_device(device, user).await? && tx.device_user(device).await? != Some(user) {
            return Err(DomainError::UnknownDevice);
        }
        tx.revoke_device(device).await?;
        tx.commit().await
    }

    /// Publishes events after a commit. Publishes nothing when there is nothing to say.
    pub(crate) async fn publish(&self, events: Vec<Event>) {
```

Replace

```rust
    /// Confirms that a connecting device is active and belongs to the user in its token.
    pub async fn identify(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        let owner = require_device(&mut tx, device).await?;
        tx.rollback().await?;
        if owner == user {
```

with

```rust
    /// Confirms that a connecting device is active and belongs to the user in its token.
    /// A device never heard of is `DeviceNotKnownYet` (retryable): auth-svc registered it
    /// and the worker has not relayed it yet. A revoked device, or one of another user, is
    /// `UnknownDevice`.
    pub async fn identify(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        let Some(owner) = tx.device_owner(device).await? else {
            let known = tx.device_user(device).await?.is_some();
            tx.rollback().await?;
            return Err(if known {
                DomainError::UnknownDevice
            } else {
                DomainError::DeviceNotKnownYet
            });
        };
        tx.rollback().await?;
        if owner == user {
```

Replace

```rust
}

/// The owner of an active device, or `UnknownDevice`.
pub(crate) async fn require_device<T: Tx>(tx: &mut T, device: DeviceId) -> DomainResult<UserId> {
```

with

```rust
}

/// Starts a new device up to date in every conversation of its user, including ones the
/// user has left (up to where they left).
async fn create_cursors<T: Tx>(tx: &mut T, device: DeviceId, user: UserId) -> DomainResult<()> {
    for (conversation, member) in tx.memberships(user).await? {
        let position = member.visible_upper(conversation.last_seq);
        tx.create_cursor_if_absent(device, conversation.id, position)
            .await?;
    }
    Ok(())
}

/// The owner of an active device, or `UnknownDevice`.
pub(crate) async fn require_device<T: Tx>(tx: &mut T, device: DeviceId) -> DomainResult<UserId> {
```

```powershell
cargo test -p push-domain -p push-testkit
cargo test -p push-store --test postgres
```

Expected: the in-memory suite and the Postgres suite (`postgres`, Docker) pass, each with
7 more cases than before (6 scenarios, 1 contract case), and push-domain's
`only_a_conflict_and_a_device_not_known_yet_are_retryable`.

- [ ] **Step 3: The gRPC contract, the API, the token and their tests**

In `crates/push-proto/proto/push/internal/v1/messaging.proto`:

Replace

```protobuf
// The internal API of message-svc, called by the gateway (spec §2). Never exposed outside
// the cluster. Callers pass the user and device the gateway verified; message-svc trusts
// them.
//
// Errors are gRPC statuses whose `details` hold an encoded `push.v1.Error`, which the
```

with

```protobuf
// The internal API of message-svc, called by the gateway (Messaging) and the worker
// (Accounts) (spec §2). Never exposed outside the cluster. Callers pass the user and
// device the gateway verified, or auth-svc announced; message-svc trusts them.
//
// Errors are gRPC statuses whose `details` hold an encoded `push.v1.Error`, which the
```

Replace

```protobuf
service Messaging {
  // Confirms that the device is active and belongs to the user.
  rpc Identify(IdentifyRequest) returns (IdentifyResponse);
  // Appends a text message; answers after commit.
```

with

```protobuf
service Messaging {
  // Confirms that the device is active and belongs to the user. A revoked device, or one
  // of another user, is PERMISSION_DENIED (UNKNOWN_DEVICE); a device message-svc has never
  // heard of is UNAVAILABLE (retryable): auth-svc registered it and the worker has not
  // relayed it yet, which takes about a second.
  rpc Identify(IdentifyRequest) returns (IdentifyResponse);
  // Appends a text message; answers after commit.
```

Replace

```protobuf
}

message IdentifyRequest {
  bytes user_id = 1;
```

with

```protobuf
}

// auth-svc's user and device events, relayed by the worker from auth-svc's outbox
// (spec §2, §5.3). Delivery is at least once and in no particular order, so every call is
// idempotent and any order of calls ends in the same state. A device id that belongs to
// another user is PERMISSION_DENIED; malformed ids are INVALID_ARGUMENT; both are
// permanent. Storage problems are UNAVAILABLE or ABORTED: retry. Every call carries the
// shared internal token as `authorization: Bearer <token>` metadata; without it the call
// is UNAUTHENTICATED, so the gateway, which can reach message-svc, cannot register devices.
service Accounts {
  // user.verified: the user may use messaging.
  rpc UserVerified(AccountUser) returns (AccountsResponse);
  // device.registered: also records the user, since only verified users get devices.
  rpc DeviceRegistered(AccountDevice) returns (AccountsResponse);
  // device.revoked: Identify refuses the device from now on, even when this arrives
  // before the registration.
  rpc DeviceRevoked(AccountDevice) returns (AccountsResponse);
  // user.password_reset: every device of the user is to be kicked. Accepted and
  // otherwise ignored until message-svc publishes kicks (Plan 7) and the gateway closes
  // kicked sockets and refuses older tokens (Plan 8).
  rpc PasswordReset(AccountUser) returns (AccountsResponse);
}

message IdentifyRequest {
  bytes user_id = 1;
```

Replace

```protobuf
  bytes conversation_id = 1;
}
```

with

```protobuf
  bytes conversation_id = 1;
}

message AccountUser {
  bytes user_id = 1;
}

message AccountDevice {
  bytes user_id = 1;
  bytes device_id = 2;
}

message AccountsResponse {}
```

In `crates/push-message-svc/src/convert.rs` (decision 16: `UNAVAILABLE`, retryable):

Replace

```rust
            (Code::PermissionDenied, ErrorCode::UnknownDevice)
        }
        DomainError::ConversationNotFound => (Code::NotFound, ErrorCode::ConversationNotFound),
        DomainError::NotMember => (Code::FailedPrecondition, ErrorCode::NotMember),
```

with

```rust
            (Code::PermissionDenied, ErrorCode::UnknownDevice)
        }
        DomainError::DeviceNotKnownYet => (Code::Unavailable, ErrorCode::Unavailable),
        DomainError::ConversationNotFound => (Code::NotFound, ErrorCode::ConversationNotFound),
        DomainError::NotMember => (Code::FailedPrecondition, ErrorCode::NotMember),
```

Replace

```rust
        assert_eq!(device.code(), Code::PermissionDenied);
        assert_eq!(error_from_status(&device).code(), ErrorCode::UnknownDevice);
    }
}
```

with

```rust
        assert_eq!(device.code(), Code::PermissionDenied);
        assert_eq!(error_from_status(&device).code(), ErrorCode::UnknownDevice);

        let not_yet = domain_error(DomainError::DeviceNotKnownYet);
        assert_eq!(not_yet.code(), Code::Unavailable);
        let not_yet = error_from_status(&not_yet);
        assert_eq!(not_yet.code(), ErrorCode::Unavailable);
        assert!(not_yet.retryable);
    }
}
```

In `crates/push-message-svc/src/grpc.rs`: the imports, `AccountsApi` after `MessagingApi`,
the `AccountsToken` interceptor (decision 17), an `AccountsApi` in the unit tests' world,
and three unit tests:

Replace

```rust
//! The gRPC services: `Messaging` for the gateway and the DEV ONLY `DevSetup`.

use std::sync::Arc;
```

with

```rust
//! The gRPC services: `Messaging` for the gateway, `Accounts` for the worker and the
//! DEV ONLY `DevSetup`.

use std::sync::Arc;
```

Replace

```rust
use push_proto::id_bytes;
use push_proto::internal::v1::{
    AckRequest, AckResponse, IdentifyRequest, IdentifyResponse, MakeContactsRequest,
    MakeContactsResponse, OpenDirectRequest, OpenDirectResponse, RegisterDeviceRequest,
    RegisterDeviceResponse, RegisterUserRequest, RegisterUserResponse, SendRequest, SyncRequest,
    SyncResponse, dev_setup_server, messaging_server,
};
use push_proto::v1::{ContentType, SendAck};
use tonic::{Request, Response, Status};
```

with

```rust
use push_proto::id_bytes;
use push_proto::internal::v1::{
    AccountDevice, AccountUser, AccountsResponse, AckRequest, AckResponse, IdentifyRequest,
    IdentifyResponse, MakeContactsRequest, MakeContactsResponse, OpenDirectRequest,
    OpenDirectResponse, RegisterDeviceRequest, RegisterDeviceResponse, RegisterUserRequest,
    RegisterUserResponse, SendRequest, SyncRequest, SyncResponse, accounts_server,
    dev_setup_server, messaging_server,
};
use push_proto::v1::{ContentType, SendAck};
use tonic::service::Interceptor;
use tonic::{Request, Response, Status};
```

Replace

```rust
}

/// DEV ONLY: sets up users, devices, contacts and direct conversations without auth-svc.
/// `serve` registers it only when the configuration enables it (`PUSH_DEV_SETUP=1`).
```

with

```rust
}

/// The `Accounts` service: auth-svc's user and device events, relayed by the worker. Every
/// call is idempotent and any order of calls ends in the same state (see
/// [`Messaging::device_registered`] and [`Messaging::device_revoked`]).
pub struct AccountsApi<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
}

impl<S, C, N> AccountsApi<S, C, N> {
    pub fn new(messaging: Arc<Messaging<S, C, N>>) -> Self {
        Self { messaging }
    }
}

#[tonic::async_trait]
impl<S, C, N> accounts_server::Accounts for AccountsApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn user_verified(
        &self,
        request: Request<AccountUser>,
    ) -> Result<Response<AccountsResponse>, Status> {
        let user = parse_id(&request.into_inner().user_id, "user_id", UserId)?;
        self.messaging
            .register_user(user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AccountsResponse {}))
    }

    async fn device_registered(
        &self,
        request: Request<AccountDevice>,
    ) -> Result<Response<AccountsResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        self.messaging
            .device_registered(device, user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AccountsResponse {}))
    }

    async fn device_revoked(
        &self,
        request: Request<AccountDevice>,
    ) -> Result<Response<AccountsResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        self.messaging
            .device_revoked(device, user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AccountsResponse {}))
    }

    /// Accepted and otherwise ignored for now: message-svc publishes kicks in Plan 7, and
    /// the gateway acts on them (and refuses tokens older than the reset) in Plan 8.
    async fn password_reset(
        &self,
        request: Request<AccountUser>,
    ) -> Result<Response<AccountsResponse>, Status> {
        parse_id(&request.into_inner().user_id, "user_id", UserId)?;
        Ok(Response::new(AccountsResponse {}))
    }
}

/// Guards the `Accounts` service: a call passes only with the shared internal token in its
/// metadata, `authorization: Bearer <PUSH_ACCOUNTS_TOKEN>`, compared in constant time. The
/// gateway does not have the token, so it cannot register devices even though it can
/// reach message-svc. Without a configured token every call is refused.
#[derive(Clone)]
pub struct AccountsToken {
    expected: Option<Arc<str>>,
}

impl AccountsToken {
    pub fn new(expected: Option<String>) -> Self {
        Self {
            expected: expected.map(Arc::from),
        }
    }
}

impl std::fmt::Debug for AccountsToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountsToken")
            .field("expected", &self.expected.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Interceptor for AccountsToken {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let Some(expected) = &self.expected else {
            return Err(Status::unauthenticated(
                "the Accounts API is off: PUSH_ACCOUNTS_TOKEN is not set",
            ));
        };
        let presented = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        match presented {
            Some(token) if same(token.as_bytes(), expected.as_bytes()) => Ok(request),
            _ => Err(Status::unauthenticated(
                "the Accounts API needs the internal token",
            )),
        }
    }
}

/// Compares in time that depends only on the lengths, never on where the bytes differ.
fn same(presented: &[u8], expected: &[u8]) -> bool {
    presented.len() == expected.len()
        && presented
            .iter()
            .zip(expected)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// DEV ONLY: sets up users, devices, contacts and direct conversations without auth-svc.
/// `serve` registers it only when the configuration enables it (`PUSH_DEV_SETUP=1`).
```

Replace

```rust
mod tests {
    use push_domain::{Event, InMemoryStore, RecordingNotifier, SystemClock};
    use push_proto::internal::v1::dev_setup_server::DevSetup;
    use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
```

with

```rust
mod tests {
    use push_domain::{Event, InMemoryStore, RecordingNotifier, SystemClock};
    use push_proto::internal::v1::accounts_server::Accounts as AccountsRpc;
    use push_proto::internal::v1::dev_setup_server::DevSetup;
    use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
```

Replace

```rust
    type Api = MessagingApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Setup = DevSetupApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;

    struct World {
        api: Api,
        setup: Setup,
        notifier: Arc<RecordingNotifier>,
```

with

```rust
    type Api = MessagingApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Accounts = AccountsApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Setup = DevSetupApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;

    struct World {
        api: Api,
        accounts: Accounts,
        setup: Setup,
        notifier: Arc<RecordingNotifier>,
```

Replace

```rust
        World {
            api: MessagingApi::new(Arc::clone(&messaging)),
            setup: DevSetupApi::new(messaging),
            notifier,
```

with

```rust
        World {
            api: MessagingApi::new(Arc::clone(&messaging)),
            accounts: AccountsApi::new(Arc::clone(&messaging)),
            setup: DevSetupApi::new(messaging),
            notifier,
```

Replace

```rust
            assert_eq!(error_from_status(&status).code(), ErrorCode::Invalid);
        }
    }
}
```

with

```rust
            assert_eq!(error_from_status(&status).code(), ErrorCode::Invalid);
        }
    }

    fn account_device(user: UserId, device: DeviceId) -> Request<AccountDevice> {
        Request::new(AccountDevice {
            user_id: id_bytes(user.0),
            device_id: id_bytes(device.0),
        })
    }

    async fn identify(world: &World, user: UserId, device: DeviceId) -> Result<(), Code> {
        world
            .api
            .identify(Request::new(IdentifyRequest {
                user_id: id_bytes(user.0),
                device_id: id_bytes(device.0),
            }))
            .await
            .map(|_| ())
            .map_err(|status| status.code())
    }

    #[tokio::test]
    async fn relayed_account_events_register_and_revoke_devices_in_any_order() {
        let world = world();
        let (user, phone, tablet) = (UserId::new(), DeviceId::new(), DeviceId::new());
        let verified = AccountUser {
            user_id: id_bytes(user.0),
        };
        world
            .accounts
            .user_verified(Request::new(verified.clone()))
            .await
            .unwrap();
        world
            .accounts
            .user_verified(Request::new(verified))
            .await
            .unwrap();
        for _ in 0..2 {
            world
                .accounts
                .device_registered(account_device(user, phone))
                .await
                .unwrap();
        }
        assert_eq!(identify(&world, user, phone).await, Ok(()));
        // A device not relayed yet is worth retrying, unlike a revoked one.
        assert_eq!(identify(&world, user, tablet).await, Err(Code::Unavailable));

        // The revocation of the tablet overtakes its registration.
        world
            .accounts
            .device_revoked(account_device(user, tablet))
            .await
            .unwrap();
        world
            .accounts
            .device_registered(account_device(user, tablet))
            .await
            .unwrap();
        assert_eq!(
            identify(&world, user, tablet).await,
            Err(Code::PermissionDenied)
        );
        world
            .accounts
            .password_reset(Request::new(AccountUser {
                user_id: id_bytes(user.0),
            }))
            .await
            .unwrap();
        assert_eq!(identify(&world, user, phone).await, Ok(()));
    }

    #[tokio::test]
    async fn relayed_events_that_can_never_succeed_are_refused_for_good() {
        let world = world();
        let alice = person(&world).await;
        let status = world
            .accounts
            .device_registered(account_device(UserId::new(), alice.device))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::PermissionDenied);
        assert!(!error_from_status(&status).retryable);

        let malformed = AccountDevice {
            user_id: vec![1, 2, 3],
            device_id: id_bytes(alice.device.0),
        };
        let status = world
            .accounts
            .device_revoked(Request::new(malformed))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(identify(&world, alice.user, alice.device).await, Ok(()));
    }

    fn with_authorization(value: &str) -> Request<()> {
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert("authorization", value.parse().unwrap());
        request
    }

    #[test]
    fn accounts_calls_pass_only_with_the_internal_token() {
        let token = "a".repeat(32);
        let mut guard = AccountsToken::new(Some(token.clone()));
        assert!(
            guard
                .call(with_authorization(&format!("Bearer {token}")))
                .is_ok()
        );
        for refused in [
            Request::new(()),
            with_authorization(&token),
            with_authorization(&format!("Bearer {}", "b".repeat(32))),
            with_authorization(&format!("Bearer {token}x")),
            with_authorization("Bearer "),
        ] {
            let status = guard.call(refused).unwrap_err();
            assert_eq!(status.code(), Code::Unauthenticated);
            assert!(!status.message().contains(&token));
        }
        let mut off = AccountsToken::new(None);
        let status = off
            .call(with_authorization(&format!("Bearer {token}")))
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);
        assert!(!format!("{guard:?}").contains(&token));
    }
}
```

In `crates/push-message-svc/src/config.rs` (`PUSH_ACCOUNTS_TOKEN`, optional, checked and
never shown):

Replace

```rust
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_DEV_SETUP` | `1` registers the DEV ONLY `DevSetup` service | off |
#[derive(Clone, PartialEq, Eq)]
```

with

```rust
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_ACCOUNTS_TOKEN` | the worker's token for the `Accounts` API, 32 to 256 visible ASCII characters; without it `Accounts` refuses every call | none |
/// | `PUSH_DEV_SETUP` | `1` registers the DEV ONLY `DevSetup` service | off |
#[derive(Clone, PartialEq, Eq)]
```

Replace

```rust
    pub health_addr: SocketAddr,
    pub store: StoreOptions,
    pub dev_setup: bool,
}
```

with

```rust
    pub health_addr: SocketAddr,
    pub store: StoreOptions,
    pub accounts_token: Option<String>,
    pub dev_setup: bool,
}
```

Replace

```rust
            .field("health_addr", &self.health_addr)
            .field("store", &self.store)
            .field("dev_setup", &self.dev_setup)
            .finish()
```

with

```rust
            .field("health_addr", &self.health_addr)
            .field("store", &self.store)
            .field(
                "accounts_token",
                &self.accounts_token.as_ref().map(|_| "<redacted>"),
            )
            .field("dev_setup", &self.dev_setup)
            .finish()
```

Replace

```rust
            )
        })?;
        Ok(Self {
            database_url,
```

with

```rust
            )
        })?;
        let accounts_token = lookup("PUSH_ACCOUNTS_TOKEN").filter(|value| !value.is_empty());
        if let Some(token) = &accounts_token {
            check_token(token).map_err(|reason| redacted("PUSH_ACCOUNTS_TOKEN", reason))?;
        }
        Ok(Self {
            database_url,
```

Replace

```rust
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            store,
            dev_setup: dev_flag(&lookup, "PUSH_DEV_SETUP")?,
        })
```

with

```rust
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            store,
            accounts_token,
            dev_setup: dev_flag(&lookup, "PUSH_DEV_SETUP")?,
        })
```

Replace

```rust
}

/// An invalid URL variable; the value is never shown because it carries a password.
fn redacted(name: &'static str, reason: String) -> ConfigError {
```

with

```rust
}

/// A shared token fit for a header: 32 to 256 visible ASCII characters.
fn check_token(token: &str) -> Result<(), String> {
    if (32..=256).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_graphic()) {
        Ok(())
    } else {
        Err("use 32 to 256 visible ASCII characters".to_owned())
    }
}

/// An invalid URL variable; the value is never shown because it carries a password.
fn redacted(name: &'static str, reason: String) -> ConfigError {
```

Replace

```rust
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.store, StoreOptions::default());
        assert!(!config.dev_setup);
    }

    #[test]
    fn the_urls_are_required() {
```

with

```rust
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.store, StoreOptions::default());
        assert_eq!(config.accounts_token, None);
        assert!(!config.dev_setup);
    }

    #[test]
    fn the_accounts_token_is_checked_and_never_shown() {
        let token = "t0ken-".repeat(6);
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_ACCOUNTS_TOKEN", &token));
        let configured = config(&variables).unwrap();
        assert_eq!(configured.accounts_token.as_deref(), Some(token.as_str()));
        assert!(!format!("{configured:?}").contains(&token));
        for bad in ["too-short", "with a space but long enough to pass 32"] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_ACCOUNTS_TOKEN", bad));
            let message = config(&variables).unwrap_err().to_string();
            assert!(message.contains("PUSH_ACCOUNTS_TOKEN"), "{message}");
            assert!(!message.contains(bad), "{message}");
        }
    }

    #[test]
    fn the_urls_are_required() {
```

The smoke test's local stack builds message-svc's `Config` too; it gets no token for now
(Plan 6B starts the worker there). In `crates/push-smoke/tests/local.rs`:

Replace

```rust
            health_addr: message_svc_health.local_addr().unwrap(),
            store: StoreOptions::default(),
            dev_setup: true,
        },
```

with

```rust
            health_addr: message_svc_health.local_addr().unwrap(),
            store: StoreOptions::default(),
            accounts_token: None,
            dev_setup: true,
        },
```

In `crates/push-message-svc/tests/service.rs`, the test that proves `DevSetup` is off by
default now also drives `Accounts` over real Postgres — it must be served without dev
setup, refuse a caller without the token, and answer `Identify` for a device not relayed
yet with `UNAVAILABLE`:

Replace

```rust
//! The whole service on real Postgres and Redis (Docker must be running): readiness, the
//! dev setup calls, a send that rings the recipient's Redis channel, sync, ack, and
//! graceful shutdown.

use std::net::SocketAddr;
```

with

```rust
//! The whole service on real Postgres and Redis (Docker must be running): readiness, the
//! dev setup calls, the worker's account events, a send that rings the recipient's Redis
//! channel, sync, ack, and graceful shutdown.

use std::net::SocketAddr;
```

Replace

```rust
use prost::Message;
use push_message_svc::{Config, serve};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AckRequest, MakeContactsRequest, Notification, OpenDirectRequest, RegisterDeviceRequest,
    RegisterUserRequest, SendRequest, SyncRequest, notification::Kind,
};
use push_proto::v1::{Ack, ContentType, Send};
```

with

```rust
use prost::Message;
use push_message_svc::{Config, serve};
use push_proto::internal::v1::accounts_client::AccountsClient;
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AccountDevice, AckRequest, IdentifyRequest, MakeContactsRequest, Notification,
    OpenDirectRequest, RegisterDeviceRequest, RegisterUserRequest, SendRequest, SyncRequest,
    notification::Kind,
};
use push_proto::v1::{Ack, ContentType, Send};
```

Replace

```rust
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Running {
    _postgres: ContainerAsync<Postgres>,
```

with

```rust
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use uuid::Uuid;

/// The worker's token for the `Accounts` API.
const ACCOUNTS_TOKEN: &str = "accounts-token-for-the-service-tests";

struct Running {
    _postgres: ContainerAsync<Postgres>,
```

Replace

```rust
        health_addr: health,
        store: StoreOptions::default(),
        dev_setup,
    };
```

with

```rust
        health_addr: health,
        store: StoreOptions::default(),
        accounts_token: Some(ACCOUNTS_TOKEN.to_owned()),
        dev_setup,
    };
```

Replace

```rust
        health_addr: health.local_addr().unwrap(),
        store: StoreOptions::default(),
        dev_setup: false,
    };
```

with

```rust
        health_addr: health.local_addr().unwrap(),
        store: StoreOptions::default(),
        accounts_token: None,
        dev_setup: false,
    };
```

Replace

```rust
#[tokio::test]
async fn dev_setup_is_absent_unless_enabled() {
    let running = start(false).await;
    let mut setup = DevSetupClient::connect(format!("http://{}", running.grpc))
        .await
        .unwrap();
    let status = setup
        .register_user(RegisterUserRequest {
```

with

```rust
#[tokio::test]
async fn accounts_are_served_and_dev_setup_is_absent_unless_enabled() {
    let running = start(false).await;
    let url = format!("http://{}", running.grpc);
    let channel = Channel::from_shared(url.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut messaging = MessagingClient::connect(url.clone()).await.unwrap();
    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
    let event = AccountDevice {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
    };
    let identify = IdentifyRequest {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
    };
    // Not relayed yet: worth retrying, unlike a revoked device.
    let status = messaging.identify(identify.clone()).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unavailable);

    // Without the worker's token (the gateway has none) the API refuses.
    let mut stranger = AccountsClient::new(channel.clone());
    let status = stranger.device_registered(event.clone()).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);

    let bearer: MetadataValue<_> = format!("Bearer {ACCOUNTS_TOKEN}").parse().unwrap();
    let mut accounts = AccountsClient::with_interceptor(channel, move |mut request: Request<()>| {
        request
            .metadata_mut()
            .insert("authorization", bearer.clone());
        Ok(request)
    });
    accounts.device_registered(event.clone()).await.unwrap();
    messaging.identify(identify.clone()).await.unwrap();
    accounts.device_revoked(event).await.unwrap();
    let status = messaging.identify(identify).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::PermissionDenied);

    let mut setup = DevSetupClient::connect(url).await.unwrap();
    let status = setup
        .register_user(RegisterUserRequest {
```

```powershell
cargo test -p push-message-svc
```

Expected: the unit tests pass, among them the 4 new ones
(`relayed_account_events_register_and_revoke_devices_in_any_order`,
`relayed_events_that_can_never_succeed_are_refused_for_good`,
`accounts_calls_pass_only_with_the_internal_token`,
`the_accounts_token_is_checked_and_never_shown`), and
`accounts_are_served_and_dev_setup_is_absent_unless_enabled` FAILS: the call without the
token answers `Unimplemented`, since the server does not serve `Accounts` yet.

- [ ] **Step 4: Serve it, behind the token**

Register the service in `crates/push-message-svc/src/server.rs` — always, not only in dev
mode, and wrapped in `AccountsToken`; without a token it warns at start-up:

Replace

```rust
use axum::routing::get;
use push_domain::{Messaging, SystemClock};
use push_proto::internal::v1::dev_setup_server::DevSetupServer;
use push_proto::internal::v1::messaging_server::MessagingServer;
```

with

```rust
use axum::routing::get;
use push_domain::{Messaging, SystemClock};
use push_proto::internal::v1::accounts_server::AccountsServer;
use push_proto::internal::v1::dev_setup_server::DevSetupServer;
use push_proto::internal::v1::messaging_server::MessagingServer;
```

Replace

```rust
use crate::config::Config;
use crate::grpc::{DevSetupApi, MessagingApi};
use crate::notify::RedisNotifier;
```

with

```rust
use crate::config::Config;
use crate::grpc::{AccountsApi, AccountsToken, DevSetupApi, MessagingApi};
use crate::notify::RedisNotifier;
```

Replace

```rust
        );
    }
    let Some(store) = retry("postgres", &shutdown, || async {
        // Services migrate with `push_store::migrate_database(url)` on a dedicated
```

with

```rust
        );
    }
    if config.accounts_token.is_none() {
        tracing::warn!(
            "PUSH_ACCOUNTS_TOKEN is not set: the Accounts API refuses every call, so the \
             worker cannot relay users and devices"
        );
    }
    let Some(store) = retry("postgres", &shutdown, || async {
        // Services migrate with `push_store::migrate_database(url)` on a dedicated
```

Replace

```rust
    tracing::info!(addr = %config.grpc_addr, "serving gRPC");
    let result = tonic::transport::Server::builder()
        .add_service(MessagingServer::new(MessagingApi::new(messaging)))
        .add_optional_service(dev_setup)
        .serve_with_incoming_shutdown(
```

with

```rust
    tracing::info!(addr = %config.grpc_addr, "serving gRPC");
    let result = tonic::transport::Server::builder()
        .add_service(MessagingServer::new(MessagingApi::new(Arc::clone(
            &messaging,
        ))))
        .add_service(AccountsServer::with_interceptor(
            AccountsApi::new(messaging),
            AccountsToken::new(config.accounts_token.clone()),
        ))
        .add_optional_service(dev_setup)
        .serve_with_incoming_shutdown(
```

and export both from `crates/push-message-svc/src/lib.rs`:

Replace

```rust
//! push-message-svc: the messaging core of push-messenger (spec §2) as a gRPC service.
//!
//! It owns the `messaging` schema (through `push-store`), answers the gateway's calls
//! and, after every commit, rings the affected users' Redis channels so their gateways
//! sync them.

mod config;
```

with

```rust
//! push-message-svc: the messaging core of push-messenger (spec §2) as a gRPC service.
//!
//! It owns the `messaging` schema (through `push-store`), answers the gateway's calls,
//! records the users and devices the worker relays from auth-svc and, after every commit,
//! rings the affected users' Redis channels so their gateways sync them.

mod config;
```

Replace

```rust
pub use config::{Config, ConfigError};
pub use grpc::{DevSetupApi, MessagingApi};
pub use notify::RedisNotifier;
pub use server::{init_logging, serve, terminated};
```

with

```rust
pub use config::{Config, ConfigError};
pub use grpc::{AccountsApi, AccountsToken, DevSetupApi, MessagingApi};
pub use notify::RedisNotifier;
pub use server::{init_logging, serve, terminated};
```

The gateway's test of an unknown device used a device id nobody registered, which is now
"not known yet" (`UNAVAILABLE`, which the gateway closes with 1011 until Plan 8 gives it a
code of its own); a device of another user is still refused for good with 4001. In
`crates/push-gateway/tests/gateway.rs`:

Replace

```rust
#[tokio::test]
async fn an_unknown_device_is_closed_with_4001() {
    let stack = stack(true).await;
    let (user, _device) = person(&stack).await;
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(user, Uuid::now_v7())).await;
    assert_eq!(close_code(&mut socket).await, 4001);
}
```

with

```rust
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
```

```powershell
cargo test -p push-message-svc -p push-gateway
```

Expected: everything passes, `accounts_are_served_and_dev_setup_is_absent_unless_enabled`
and `a_device_of_another_user_is_closed_with_4001` included.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "feat(message-svc): the Accounts API for relayed auth events"
```

Expected: 724 tests pass, no warnings, no diff.

---

### Task 2: What the worker's role may do in auth-svc's database

**Files:**
- Create: `crates/push-auth/migrations/0002_worker.sql`
- Modify: `crates/push-auth/src/db.rs`, `src/lib.rs`, `src/config.rs`, `src/server.rs`,
  `src/service.rs`, `crates/push-auth/tests/schema.rs`, `tests/accounts.rs`

- [ ] **Step 1: The tests, failing**

The schema tests create a plain role `push_worker` beside `push_auth` — as the chart's
init script will (Plan 6B) — grant it its access, and connect as it: it may drain the
outbox with `FOR UPDATE SKIP LOCKED` and call the two cleanups, and every other statement is
`42501` (insufficient privilege). In `crates/push-auth/tests/schema.rs`:

Replace

```rust
//! The `auth` schema on a real PostgreSQL (Docker must be running): the migrations run as
//! the database's owner, a plain role like the chart's, and the constraints hold.

use push_auth::{MIGRATOR, migrate_database};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
```

with

```rust
//! The `auth` schema on a real PostgreSQL (Docker must be running): the migrations run as
//! the database's owner, a plain role like the chart's, the constraints hold, and the
//! worker's role may drain the outbox and run the cleanups, and nothing else.

use push_auth::{MIGRATOR, grant_worker_access, migrate_database};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
```

Replace

```rust
/// A running Postgres with a database `push_auth` owned by the plain role `push_auth`,
/// and the URL to connect as that role.
async fn auth_database() -> (ContainerAsync<Postgres>, String) {
    let container = Postgres::default()
```

with

```rust
/// A running Postgres with a database `push_auth` owned by the plain role `push_auth`,
/// a plain role `push_worker`, and the URL to connect as `push_auth`.
async fn auth_database() -> (ContainerAsync<Postgres>, String) {
    let container = Postgres::default()
```

Replace

```rust
        "CREATE ROLE push_auth LOGIN PASSWORD 'auth-secret'",
        "CREATE DATABASE push_auth OWNER push_auth",
    ] {
        sqlx::query(statement)
```

with

```rust
        "CREATE ROLE push_auth LOGIN PASSWORD 'auth-secret'",
        "CREATE DATABASE push_auth OWNER push_auth",
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
    ] {
        sqlx::query(statement)
```

Replace

```rust
    assert_eq!(constraint(error), "device_user_id_registration_id_key");
}
```

with

```rust
    assert_eq!(constraint(error), "device_user_id_registration_id_key");
}

/// The URL of the same database as the worker's role.
fn worker_url(url: &str) -> String {
    url.replacen("push_auth:auth-secret@", "push_worker:worker-secret@", 1)
}

/// The SQLSTATE of a failed statement.
fn sqlstate(error: sqlx::Error) -> String {
    error
        .as_database_error()
        .and_then(|database| database.code())
        .unwrap_or_default()
        .into_owned()
}

#[tokio::test]
async fn the_worker_may_drain_the_outbox_and_nothing_else() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    grant_worker_access(&url, "push_worker")
        .await
        .expect("grant");
    grant_worker_access(&url, "push_worker")
        .await
        .expect("grant again");
    let event = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.outbox (event_id, kind, payload) VALUES ($1, 'user.verified', '{}')",
    )
    .bind(event)
    .execute(&pool)
    .await
    .expect("an event");

    let worker = PgPool::connect(&worker_url(&url))
        .await
        .expect("connect as the worker");
    let drained: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM auth.outbox WHERE event_id IN (SELECT event_id FROM auth.outbox \
         ORDER BY created_at, event_id LIMIT 10 FOR UPDATE SKIP LOCKED) RETURNING event_id",
    )
    .fetch_all(&worker)
    .await
    .expect("drain the outbox");
    assert_eq!(drained, [event]);
    for statement in [
        "SELECT count(*) FROM auth.user_account",
        "SELECT count(*) FROM auth.refresh_token",
        "INSERT INTO auth.outbox (event_id, kind, payload) \
         VALUES (gen_random_uuid(), 'user.verified', '{}')",
        "UPDATE auth.outbox SET payload = '{}'",
    ] {
        let error = sqlx::query(statement)
            .execute(&worker)
            .await
            .expect_err(statement);
        assert_eq!(sqlstate(error), "42501", "{statement}");
    }
}

#[tokio::test]
async fn the_cleanups_delete_expired_tokens_and_stale_unverified_accounts() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    grant_worker_access(&url, "push_worker")
        .await
        .expect("grant");
    let alice = insert_user(&pool, "alice@example.com", "alice")
        .await
        .expect("alice");
    sqlx::query("UPDATE auth.user_account SET email_verified_at = now() WHERE user_id = $1")
        .bind(alice)
        .execute(&pool)
        .await
        .expect("verify alice");
    let device = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.device (device_id, user_id, name, platform, registration_id) \
         VALUES ($1, $2, 'Phone', 'ios', $1)",
    )
    .bind(device)
    .bind(alice)
    .execute(&pool)
    .await
    .expect("a device");
    for (hash, expires) in [([1u8; 32], "-1 day"), ([2u8; 32], "1 day")] {
        sqlx::query(
            "INSERT INTO auth.refresh_token (token_hash, device_id, expires_at) \
             VALUES ($1, $2, now() + $3::interval)",
        )
        .bind(hash.as_slice())
        .bind(device)
        .bind(expires)
        .execute(&pool)
        .await
        .expect("a refresh token");
    }
    // Unverified for 8 days, unverified since now, and verified 8 days ago.
    for (email, handle, verified) in [
        ("bob@example.com", "bob", false),
        ("carol@example.com", "carol", false),
        ("dave@example.com", "dave", true),
    ] {
        let user = insert_user(&pool, email, handle).await.expect(handle);
        if handle != "carol" {
            sqlx::query(
                "UPDATE auth.user_account SET created_at = now() - interval '8 days', \
                 email_verified_at = CASE WHEN $2 THEN now() END WHERE user_id = $1",
            )
            .bind(user)
            .bind(verified)
            .execute(&pool)
            .await
            .expect("age the account");
        }
    }

    let worker = PgPool::connect(&worker_url(&url))
        .await
        .expect("connect as the worker");
    let expired = |batch: i32| {
        sqlx::query_scalar::<_, i32>("SELECT auth.delete_expired_refresh_tokens($1)")
            .bind(batch)
            .fetch_one(&worker)
    };
    assert_eq!(expired(100).await.expect("tokens"), 1);
    assert_eq!(expired(100).await.expect("tokens again"), 0);
    // A NULL batch or an age under a day cannot touch carol, unverified since now.
    let misused: Option<i32> =
        sqlx::query_scalar("SELECT auth.delete_unverified_accounts(interval '0 seconds', NULL)")
            .fetch_one(&worker)
            .await
            .expect("a NULL batch");
    assert_eq!(misused, None);
    let misused: i32 =
        sqlx::query_scalar("SELECT auth.delete_unverified_accounts(interval '-1 day', 100)")
            .fetch_one(&worker)
            .await
            .expect("a negative age");
    assert_eq!(misused, 1, "only the 8-day-old account, never carol");
    let unverified: i32 =
        sqlx::query_scalar("SELECT auth.delete_unverified_accounts(interval '7 days', 100)")
            .fetch_one(&worker)
            .await
            .expect("accounts");
    assert_eq!(unverified, 0);

    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.refresh_token")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tokens, 1, "the live token stays");
    let handles: Vec<String> =
        sqlx::query_scalar("SELECT handle::text FROM auth.user_account ORDER BY handle")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(handles, ["alice", "carol", "dave"]);
}
```

A new signup over an unverified account restarts its clock (decision 7). In
`crates/push-auth/tests/accounts.rs`:

Replace

```rust
}

#[tokio::test]
async fn a_taken_handle_and_invalid_input_are_reported() {
```

with

```rust
}

#[tokio::test]
async fn signing_up_again_restarts_an_unverified_accounts_clock() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    sqlx::query("UPDATE auth.user_account SET created_at = now() - interval '10 days'")
        .execute(&world.pool)
        .await
        .unwrap();
    world
        .auth
        .signup(signup("alice@example.com", "alice2"))
        .await
        .unwrap();
    let fresh: bool = sqlx::query_scalar(
        "SELECT created_at > now() - interval '1 minute' FROM auth.user_account",
    )
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert!(
        fresh,
        "the worker would delete it as if nobody had signed up"
    );
}

#[tokio::test]
async fn a_taken_handle_and_invalid_input_are_reported() {
```

```powershell
cargo test -p push-auth --test schema --test accounts
```

Expected: a compile error — `unresolved import push_auth::grant_worker_access`.

- [ ] **Step 2: The cleanup functions**

Create `crates/push-auth/migrations/0002_worker.sql`:

```sql
-- What the worker may do in the auth schema (Plan 6), besides draining auth.outbox: two
-- cleanups, as functions, so the rules stay here and the worker never reads accounts or
-- tokens. auth-svc grants EXECUTE on them, SELECT and DELETE on auth.outbox, and UPDATE of
-- auth.outbox.created_at (which FOR UPDATE needs), to the worker's role at start-up
-- (PUSH_AUTH_WORKER_ROLE).
--
-- STRICT: a NULL argument deletes nothing. The batch is capped at 10000 and the age at no
-- less than a day, so a misused caller cannot wipe fresh signups in one call.
--
-- SECURITY DEFINER: they run with the rights of their owner, auth-svc's role. The
-- search_path is pinned, so a caller cannot slip in objects of its own, and nobody may
-- call them until auth-svc grants it.

CREATE INDEX refresh_token_expires ON auth.refresh_token (expires_at);
CREATE INDEX user_account_unverified ON auth.user_account (created_at)
    WHERE email_verified_at IS NULL;

-- Deletes up to `batch` refresh tokens past their expiry, which nothing can use any more,
-- rotated and revoked ones included. Returns how many it deleted; call it again until 0.
CREATE FUNCTION auth.delete_expired_refresh_tokens(batch integer) RETURNS integer
LANGUAGE sql STRICT SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
    WITH gone AS (
        DELETE FROM auth.refresh_token
        WHERE token_hash IN (
            SELECT token_hash FROM auth.refresh_token
            WHERE expires_at < statement_timestamp()
            LIMIT least(batch, 10000)
            FOR UPDATE SKIP LOCKED)
        RETURNING 1)
    SELECT count(*)::integer FROM gone
$$;

-- Deletes up to `batch` accounts whose email was never verified, created (or taken over
-- by a new signup) more than `older_than` ago. Such an account has no devices; one that
-- somehow has is kept. Returns how many it deleted; call it again until 0.
CREATE FUNCTION auth.delete_unverified_accounts(older_than interval, batch integer)
RETURNS integer
LANGUAGE sql STRICT SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
    WITH gone AS (
        DELETE FROM auth.user_account
        WHERE user_id IN (
            SELECT account.user_id FROM auth.user_account AS account
            WHERE account.email_verified_at IS NULL
              AND account.created_at < statement_timestamp() - greatest(older_than, interval '1 day')
              AND NOT EXISTS (
                  SELECT 1 FROM auth.device WHERE device.user_id = account.user_id)
            LIMIT least(batch, 10000)
            FOR UPDATE SKIP LOCKED)
        RETURNING 1)
    SELECT count(*)::integer FROM gone
$$;

REVOKE ALL ON FUNCTION auth.delete_expired_refresh_tokens(integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION auth.delete_unverified_accounts(interval, integer) FROM PUBLIC;
```

- [ ] **Step 3: The grant**

Replace `crates/push-auth/src/db.rs` with:

```rust
//! The `auth` schema's migrations, and what the worker may do in it.

use sqlx::postgres::PgPoolOptions;

/// The migrations of the `auth` schema, embedded at build time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Applies the migrations the `push_auth` database at `url` does not have yet, on a
/// connection of its own (see [`push_store::run_migrations`]). Run it as the owner of the
/// database: it creates the `citext` extension, which PostgreSQL trusts database owners
/// with.
pub async fn migrate_database(url: &str) -> Result<(), sqlx::migrate::MigrateError> {
    push_store::run_migrations(url, &MIGRATOR).await
}

/// Lets the worker's `role` drain the outbox and run the two cleanups, and nothing else:
/// `USAGE` on the schema; `SELECT` and `DELETE` on `auth.outbox`, plus `UPDATE` of its
/// `created_at` alone, without which PostgreSQL refuses the `FOR UPDATE SKIP LOCKED` that
/// lets several workers drain at once; and `EXECUTE` on
/// `auth.delete_expired_refresh_tokens` and `auth.delete_unverified_accounts`. Idempotent.
/// Run it as the owner, after [`migrate_database`], on a connection of its own. `role`
/// must be a valid role name ([`is_role_name`]) of an existing role.
pub async fn grant_worker_access(url: &str, role: &str) -> Result<(), sqlx::Error> {
    if !is_role_name(role) {
        return Err(sqlx::Error::Configuration(
            "the worker's role name is not valid".into(),
        ));
    }
    let pool = PgPoolOptions::new().max_connections(1).connect(url).await?;
    let grants = format!(
        "GRANT USAGE ON SCHEMA auth TO \"{role}\"; \
         GRANT SELECT, DELETE, UPDATE (created_at) ON auth.outbox TO \"{role}\"; \
         GRANT EXECUTE ON FUNCTION auth.delete_expired_refresh_tokens(integer), \
         auth.delete_unverified_accounts(interval, integer) TO \"{role}\""
    );
    let result = sqlx::raw_sql(&grants).execute(&pool).await;
    pool.close().await;
    result.map(|_| ())
}

/// A plain PostgreSQL role name: a lower-case letter or `_`, then up to 62 lower-case
/// letters, digits or `_`. It is quoted into SQL, so nothing else is accepted.
pub fn is_role_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z' | '_'))
        && name.len() <= 63
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
        // PostgreSQL reads `public` as every role even when quoted; `none` and `pg_*` are
        // reserved.
        && !matches!(name, "public" | "none")
        && !name.starts_with("pg_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_role_names_are_accepted() {
        for name in ["push_worker", "_w", "w2"] {
            assert!(is_role_name(name), "{name}");
        }
        let long = "w".repeat(64);
        for name in [
            "",
            "public",
            "none",
            "pg_monitor",
            "2w",
            "Push",
            "push-worker",
            "w\"; DROP",
            "w w",
            long.as_str(),
        ] {
            assert!(!is_role_name(name), "{name}");
        }
    }
}
```

In `crates/push-auth/src/lib.rs`:

Replace

```rust
pub use config::{Config, ConfigError};
pub use db::{MIGRATOR, migrate_database};
pub use devices::{Caller, DeviceInfo, MAX_ACTIVE_DEVICES, REFRESH_GRACE, Tokens};
pub use error::AuthError;
```

with

```rust
pub use config::{Config, ConfigError};
pub use db::{MIGRATOR, grant_worker_access, is_role_name, migrate_database};
pub use devices::{Caller, DeviceInfo, MAX_ACTIVE_DEVICES, REFRESH_GRACE, Tokens};
pub use error::AuthError;
```

- [ ] **Step 4: Configure the role, grant it at start-up, restart the clock on signup**

In `crates/push-auth/src/config.rs`:

Replace

```rust
use push_store::{StoreOptions, check_database_url};

/// How the service runs. Read from the environment by [`Config::from_env`]:
///
```

with

```rust
use push_store::{StoreOptions, check_database_url};

use crate::db::is_role_name;

/// How the service runs. Read from the environment by [`Config::from_env`]:
///
```

Replace

```rust
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_DEV_MAILBOX` | `1` serves the DEV ONLY `GET /dev/mailbox` (local public URL only) | off |
///
```

with

```rust
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_AUTH_WORKER_ROLE` | role the worker connects as: granted the outbox and the cleanups at start-up | none |
/// | `PUSH_DEV_MAILBOX` | `1` serves the DEV ONLY `GET /dev/mailbox` (local public URL only) | off |
///
```

Replace

```rust
    pub health_addr: SocketAddr,
    pub store: StoreOptions,
    pub dev_mailbox: bool,
}
```

with

```rust
    pub health_addr: SocketAddr,
    pub store: StoreOptions,
    pub worker_role: Option<String>,
    pub dev_mailbox: bool,
}
```

Replace

```rust
            .field("health_addr", &self.health_addr)
            .field("store", &self.store)
            .field("dev_mailbox", &self.dev_mailbox)
            .finish()
```

with

```rust
            .field("health_addr", &self.health_addr)
            .field("store", &self.store)
            .field("worker_role", &self.worker_role)
            .field("dev_mailbox", &self.dev_mailbox)
            .finish()
```

Replace

```rust
            });
        }
        let dev_mailbox = dev_flag(&lookup, "PUSH_DEV_MAILBOX")?;
        if dev_mailbox && !is_local_url(&public_url) {
```

with

```rust
            });
        }
        let worker_role = match lookup("PUSH_AUTH_WORKER_ROLE").filter(|v| !v.is_empty()) {
            None => None,
            Some(role) if is_role_name(&role) => Some(role),
            Some(role) => {
                return Err(ConfigError::Invalid {
                    name: "PUSH_AUTH_WORKER_ROLE",
                    value: role,
                    reason:
                        "a role name: lower-case letters, digits and _, not starting with a digit"
                            .to_owned(),
                });
            }
        };
        let dev_mailbox = dev_flag(&lookup, "PUSH_DEV_MAILBOX")?;
        if dev_mailbox && !is_local_url(&public_url) {
```

Replace

```rust
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            store,
            dev_mailbox,
        })
```

with

```rust
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            store,
            worker_role,
            dev_mailbox,
        })
```

Replace

```rust
        assert_eq!(config.client_ip_header, None);
        assert_eq!(config.store, StoreOptions::default());
        assert!(!config.dev_mailbox);
    }
```

with

```rust
        assert_eq!(config.client_ip_header, None);
        assert_eq!(config.store, StoreOptions::default());
        assert_eq!(config.worker_role, None);
        assert!(!config.dev_mailbox);
    }
```

Replace

```rust
            ("PUSH_HTTP_ADDR", "127.0.0.1:7000"),
            ("PUSH_DB_MAX_CONNECTIONS", "3"),
            ("PUSH_DEV_MAILBOX", "1"),
        ]);
```

with

```rust
            ("PUSH_HTTP_ADDR", "127.0.0.1:7000"),
            ("PUSH_DB_MAX_CONNECTIONS", "3"),
            ("PUSH_AUTH_WORKER_ROLE", "push_worker"),
            ("PUSH_DEV_MAILBOX", "1"),
        ]);
```

Replace

```rust
        assert_eq!(config.http_addr, "127.0.0.1:7000".parse().unwrap());
        assert_eq!(config.store.max_connections, 3);
        assert!(config.dev_mailbox);
    }
```

with

```rust
        assert_eq!(config.http_addr, "127.0.0.1:7000".parse().unwrap());
        assert_eq!(config.store.max_connections, 3);
        assert_eq!(config.worker_role.as_deref(), Some("push_worker"));
        assert!(config.dev_mailbox);
    }
```

Replace

```rust
    }

    #[test]
    fn zero_hash_concurrency_is_refused() {
```

with

```rust
    }

    #[test]
    fn the_worker_role_must_be_a_plain_role_name() {
        for role in ["Push", "push-worker", "w\"; drop", "1w"] {
            assert!(
                matches!(
                    config(&with("PUSH_AUTH_WORKER_ROLE", role)),
                    Err(ConfigError::Invalid {
                        name: "PUSH_AUTH_WORKER_ROLE",
                        ..
                    })
                ),
                "{role}"
            );
        }
    }

    #[test]
    fn zero_hash_concurrency_is_refused() {
```

In `crates/push-auth/src/server.rs` (the grant runs on its own connection right after the
migrations, inside the start-up retry, so a missing role is retried and logged):

Replace

```rust
use crate::api::{Api, router};
use crate::config::Config;
use crate::db::migrate_database;
use crate::limits::{Limits, RedisCounter};
use crate::links::LinkSigner;
```

with

```rust
use crate::api::{Api, router};
use crate::config::Config;
use crate::db::{grant_worker_access, migrate_database};
use crate::limits::{Limits, RedisCounter};
use crate::links::LinkSigner;
```

Replace

```rust
        // statement and lock timeouts are sized for requests.
        migrate_database(&config.database_url).await?;
        let pool = push_store::connect_pool(&config.database_url, &config.store).await?;
        anyhow::Ok(pool)
```

with

```rust
        // statement and lock timeouts are sized for requests.
        migrate_database(&config.database_url).await?;
        if let Some(role) = &config.worker_role {
            grant_worker_access(&config.database_url, role).await?;
        }
        let pool = push_store::connect_pool(&config.database_url, &config.store).await?;
        anyhow::Ok(pool)
```

In `crates/push-auth/src/service.rs`:

Replace

```rust
        // One statement for a new account and for a new start of an unverified one, so
        // concurrent signups for one address cannot both insert. A verified account does
        // not match the WHERE, and no row comes back.
        let upserted = sqlx::query(
            "INSERT INTO auth.user_account (user_id, email, handle, display_name, password_hash) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (email) DO UPDATE SET handle = EXCLUDED.handle, \
             display_name = EXCLUDED.display_name, password_hash = EXCLUDED.password_hash \
             WHERE auth.user_account.email_verified_at IS NULL \
             RETURNING user_id, email::text",
```

with

```rust
        // One statement for a new account and for a new start of an unverified one, so
        // concurrent signups for one address cannot both insert. A verified account does
        // not match the WHERE, and no row comes back. A new start also restarts the
        // account's clock: the worker deletes accounts left unverified for long.
        let upserted = sqlx::query(
            "INSERT INTO auth.user_account (user_id, email, handle, display_name, password_hash) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (email) DO UPDATE SET handle = EXCLUDED.handle, \
             display_name = EXCLUDED.display_name, password_hash = EXCLUDED.password_hash, \
             created_at = now() \
             WHERE auth.user_account.email_verified_at IS NULL \
             RETURNING user_id, email::text",
```

```powershell
cargo test -p push-auth
```

Expected: everything passes; `tests\schema.rs` has 6 tests (2 new), `tests\accounts.rs` 13
(1 new), and the unit tests 2 more (`only_plain_role_names_are_accepted`,
`the_worker_role_must_be_a_plain_role_name`). Without `UPDATE (created_at)` in the grant,
`the_worker_may_drain_the_outbox_and_nothing_else` fails with
`permission denied for table outbox`: `FOR UPDATE` needs some `UPDATE` right.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates/push-auth
git commit -m "feat(auth): outbox and cleanup access for the worker's role"
```

Expected: 729 tests pass.

---

### Task 3: `push-email`: templates, the log sender and the Resend sender

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/push-email/Cargo.toml`, `src/lib.rs`, `src/templates.rs`,
  `src/sender.rs`, `src/resend.rs`

- [ ] **Step 1: The crate**

```powershell
New-Item -ItemType Directory crates\push-email\src -Force
```

In `Cargo.toml`, add the member and the path dependency:

Replace

```toml
    "crates/push-auth",
    "crates/push-domain",
    "crates/push-gateway",
    "crates/push-message-svc",
```

with

```toml
    "crates/push-auth",
    "crates/push-domain",
    "crates/push-email",
    "crates/push-gateway",
    "crates/push-message-svc",
```

Replace

```toml
push-auth = { path = "crates/push-auth" }
push-domain = { path = "crates/push-domain" }
push-gateway = { path = "crates/push-gateway" }
push-message-svc = { path = "crates/push-message-svc" }
```

with

```toml
push-auth = { path = "crates/push-auth" }
push-domain = { path = "crates/push-domain" }
push-email = { path = "crates/push-email" }
push-gateway = { path = "crates/push-gateway" }
push-message-svc = { path = "crates/push-message-svc" }
```

Create `crates/push-email/Cargo.toml` (with Resend's HTTPS client: decision 10 and the Tech
Stack):

```toml
[package]
name = "push-email"
description = "Email for push-messenger: templates, the sender and webhook traits, Resend and a log-only sender."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
async-trait.workspace = true
# Resend is HTTPS: rustls with ring and the webpki roots, no OpenSSL and no cmake.
reqwest = { workspace = true, features = ["rustls-tls"] }
serde_json.workspace = true
thiserror.workspace = true
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
axum.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread"] }

[lints]
workspace = true
```

Create `crates/push-email/src/lib.rs`:

```rust
//! push-email: email for push-messenger (spec §9).
//!
//! Services ask for email by writing `email.send {user_id, to, template, params}` to their
//! outbox; the worker renders the template here ([`Templates`]) and hands the result to an
//! [`EmailSender`]: [`ResendSender`] in production, [`LogSender`] locally, which only logs
//! that it sent something.
//! Nothing here logs an address, a subject or a body: emails carry sign-in links.

mod resend;
mod sender;
mod templates;

pub use resend::{RESEND_API_URL, ResendSender};
pub use sender::{EmailSender, LogSender, OutgoingEmail, SendError, recipient_domain};
pub use templates::{Rendered, TemplateError, Templates};
```

- [ ] **Step 2: The templates, with their tests**

Each test module comes with its code here; the tests are the first thing to read. Create
`crates/push-email/src/templates.rs` (decision 9; the default product name is
"Push Messenger", owner decision 2):

```rust
//! The email templates of auth-svc's `email.send` events (spec §3.1): plain text and a
//! minimal HTML version of each, with every parameter escaped.

use serde_json::Value;

/// A rendered email: what the sender sends.
#[derive(Clone, PartialEq, Eq)]
pub struct Rendered {
    pub subject: String,
    pub text: String,
    pub html: String,
}

impl std::fmt::Debug for Rendered {
    /// Only the subject: the bodies carry sign-in links.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Rendered")
            .field("subject", &self.subject)
            .finish_non_exhaustive()
    }
}

/// Why a template could not be rendered. Never worth retrying.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("unknown email template {0:?}")]
    UnknownTemplate(String),
    #[error("the email template parameter {0} is missing or invalid")]
    BadParam(&'static str),
}

/// The templates, with the product name they sign with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Templates {
    product: String,
}

impl Templates {
    /// `product` names the service in subjects and bodies, `Push Messenger` by default.
    pub fn new(product: impl Into<String>) -> Self {
        Self {
            product: product.into(),
        }
    }

    /// Renders `template` with `params`:
    ///
    /// | template | params |
    /// |---|---|
    /// | `verify_email` | `link`, `handle`, `expires_in_minutes` |
    /// | `account_exists` | `handle` |
    /// | `reset_password` | `link`, `expires_in_minutes` |
    ///
    /// Links must be `https://` or `http://` URLs.
    pub fn render(&self, template: &str, params: &Value) -> Result<Rendered, TemplateError> {
        let product = &self.product;
        match template {
            "verify_email" => {
                let link = link(params)?;
                let handle = text(params, "handle")?;
                let minutes = minutes(params)?;
                Ok(self.page(
                    format!("Verify your email address for {product}"),
                    &[
                        format!("Hi @{handle},"),
                        format!("Open this link to verify your email address for {product}:"),
                    ],
                    Some(link),
                    &[
                        format!("The link works for {minutes} minutes."),
                        "If you did not sign up, ignore this email.".to_owned(),
                    ],
                ))
            }
            "account_exists" => {
                let handle = text(params, "handle")?;
                Ok(self.page(
                    format!("You already have a {product} account"),
                    &[
                        format!(
                            "Someone, perhaps you, tried to sign up for {product} with this \
                             email address. It already has an account: @{handle}."
                        ),
                        "If you forgot your password, reset it from the app. If it was not \
                         you, ignore this email."
                            .to_owned(),
                    ],
                    None,
                    &[],
                ))
            }
            "reset_password" => {
                let link = link(params)?;
                let minutes = minutes(params)?;
                Ok(self.page(
                    format!("Reset your {product} password"),
                    &[format!(
                        "Open this link to choose a new password for {product}:"
                    )],
                    Some(link),
                    &[
                        format!("The link works for {minutes} minutes and only once."),
                        "If you did not ask for it, ignore this email: your password stays as \
                         it is."
                            .to_owned(),
                    ],
                ))
            }
            other => Err(TemplateError::UnknownTemplate(other.to_owned())),
        }
    }

    /// Paragraphs `before`, an optional link, and paragraphs `after`, as text and as HTML.
    fn page(
        &self,
        subject: String,
        before: &[String],
        link: Option<&str>,
        after: &[String],
    ) -> Rendered {
        let mut text = Vec::new();
        let mut html = Vec::new();
        for paragraph in before {
            text.push(paragraph.clone());
            html.push(format!("<p>{}</p>", escape(paragraph)));
        }
        if let Some(link) = link {
            text.push(link.to_owned());
            html.push(format!(
                "<p><a href=\"{link}\">{link}</a></p>",
                link = escape(link)
            ));
        }
        for paragraph in after {
            text.push(paragraph.clone());
            html.push(format!("<p>{}</p>", escape(paragraph)));
        }
        text.push(format!("— {}", self.product));
        html.push(format!("<p>— {}</p>", escape(&self.product)));
        Rendered {
            html: format!(
                "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title></head>\
                 <body>{}</body></html>",
                escape(&subject),
                html.join("")
            ),
            text: text.join("\n\n") + "\n",
            subject,
        }
    }
}

impl Default for Templates {
    fn default() -> Self {
        Self::new("Push Messenger")
    }
}

fn text<'a>(params: &'a Value, name: &'static str) -> Result<&'a str, TemplateError> {
    params[name]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or(TemplateError::BadParam(name))
}

fn link(params: &Value) -> Result<&str, TemplateError> {
    let link = text(params, "link")?;
    if (link.starts_with("https://") || link.starts_with("http://"))
        && !link.contains(char::is_whitespace)
    {
        Ok(link)
    } else {
        Err(TemplateError::BadParam("link"))
    }
}

fn minutes(params: &Value) -> Result<u64, TemplateError> {
    params["expires_in_minutes"]
        .as_u64()
        .ok_or(TemplateError::BadParam("expires_in_minutes"))
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const LINK: &str = "https://api.example.com/v1/auth/verify?token=abc.def";

    #[test]
    fn a_link_with_quotes_or_angle_brackets_stays_inside_its_href() {
        let link = "https://api.example.com/v1/auth/verify?token=a\"><script>";
        let rendered = Templates::default()
            .render(
                "verify_email",
                &json!({"link": link, "handle": "alice", "expires_in_minutes": 30}),
            )
            .unwrap();
        assert!(!rendered.html.contains("<script>"));
        assert!(rendered.html.contains("token=a&quot;&gt;&lt;script&gt;\">"));
    }

    #[test]
    fn verify_email_carries_the_link_the_handle_and_the_lifetime() {
        let rendered = Templates::default()
            .render(
                "verify_email",
                &json!({"link": LINK, "handle": "alice", "expires_in_minutes": 30}),
            )
            .unwrap();
        assert_eq!(
            rendered.subject,
            "Verify your email address for Push Messenger"
        );
        assert!(rendered.text.starts_with("Hi @alice,\n\n"));
        assert!(rendered.text.contains(&format!("\n\n{LINK}\n\n")));
        assert!(rendered.text.contains("The link works for 30 minutes."));
        assert!(
            rendered
                .html
                .contains(&format!("<a href=\"{LINK}\">{LINK}</a>"))
        );
    }

    #[test]
    fn every_template_renders_and_names_the_product() {
        let templates = Templates::new("Acme Chat");
        for (template, params) in [
            (
                "verify_email",
                json!({"link": LINK, "handle": "alice", "expires_in_minutes": 30}),
            ),
            ("account_exists", json!({"handle": "alice"})),
            (
                "reset_password",
                json!({"link": LINK, "expires_in_minutes": 30}),
            ),
        ] {
            let rendered = templates.render(template, &params).unwrap();
            assert!(rendered.subject.contains("Acme Chat"), "{template}");
            assert!(rendered.text.ends_with("— Acme Chat\n"), "{template}");
            assert!(rendered.html.contains("<p>— Acme Chat</p>"), "{template}");
        }
    }

    #[test]
    fn parameters_are_escaped_in_html() {
        let rendered = Templates::new("A&B")
            .render("account_exists", &json!({"handle": "<script>"}))
            .unwrap();
        assert!(rendered.html.contains("@&lt;script&gt;"));
        assert!(!rendered.html.contains("<script>"));
        assert!(rendered.html.contains("A&amp;B"));
    }

    #[test]
    fn unknown_templates_and_bad_parameters_are_refused() {
        let templates = Templates::default();
        assert_eq!(
            templates.render("welcome", &json!({})),
            Err(TemplateError::UnknownTemplate("welcome".to_owned()))
        );
        for (params, missing) in [
            (json!({"handle": "a", "expires_in_minutes": 30}), "link"),
            (
                json!({"link": "javascript:alert(1)", "handle": "a", "expires_in_minutes": 30}),
                "link",
            ),
            (
                json!({"link": "https://x.test/a b", "handle": "a", "expires_in_minutes": 30}),
                "link",
            ),
            (json!({"link": LINK, "expires_in_minutes": 30}), "handle"),
            (
                json!({"link": LINK, "handle": "a", "expires_in_minutes": "30"}),
                "expires_in_minutes",
            ),
        ] {
            assert_eq!(
                templates.render("verify_email", &params),
                Err(TemplateError::BadParam(missing)),
                "{params}"
            );
        }
    }

    #[test]
    fn debug_output_hides_the_bodies() {
        let rendered = Templates::default()
            .render(
                "reset_password",
                &json!({"link": LINK, "expires_in_minutes": 30}),
            )
            .unwrap();
        let shown = format!("{rendered:?}");
        assert!(!shown.contains("token"), "{shown}");
        assert!(
            shown.contains("Reset your Push Messenger password"),
            "{shown}"
        );
    }
}
```

- [ ] **Step 3: The sender trait and the log sender, with their tests**

Create `crates/push-email/src/sender.rs`:

```rust
//! The provider-neutral sender (spec §9) and the log-only sender of local development.

use async_trait::async_trait;
use uuid::Uuid;

use crate::templates::Rendered;

/// One email to one address, rendered.
#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingEmail {
    pub to: String,
    /// The template it was rendered from; senders may tag the email with it.
    pub template: String,
    pub content: Rendered,
}

impl std::fmt::Debug for OutgoingEmail {
    /// The template and the recipient's domain only: no address, no body.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutgoingEmail")
            .field("template", &self.template)
            .field("recipient_domain", &recipient_domain(&self.to))
            .finish_non_exhaustive()
    }
}

/// Why sending failed. The message never contains the address or the email.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// Worth trying again later: a timeout, a rate limit, a provider outage.
    #[error("sending failed for now: {0}")]
    Retryable(String),
    /// Trying again cannot help: the provider refused this email.
    #[error("sending failed for good: {0}")]
    Permanent(String),
}

/// Sends email through a provider.
#[async_trait]
pub trait EmailSender: Send + Sync {
    /// Sends `email` and returns the provider's id for it. `idempotency_key` names this
    /// email: sending it again with the same key must not deliver a second copy.
    async fn send(&self, email: &OutgoingEmail, idempotency_key: Uuid)
    -> Result<String, SendError>;
}

/// The local sender: logs that an email went out, with its template and the recipient's
/// domain, and sends nothing (spec §9).
#[derive(Debug, Clone, Copy, Default)]
pub struct LogSender;

#[async_trait]
impl EmailSender for LogSender {
    async fn send(
        &self,
        email: &OutgoingEmail,
        idempotency_key: Uuid,
    ) -> Result<String, SendError> {
        tracing::info!(
            email_id = %idempotency_key,
            template = %email.template,
            recipient_domain = %recipient_domain(&email.to),
            "email delivered by the log sender (not sent)"
        );
        Ok(format!("log-{idempotency_key}"))
    }
}

/// The lower-cased domain of an address, for metrics and logs; empty without an `@`.
pub fn recipient_domain(address: &str) -> String {
    address
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email() -> OutgoingEmail {
        OutgoingEmail {
            to: "alice@Example.COM".to_owned(),
            template: "verify_email".to_owned(),
            content: Rendered {
                subject: "Verify".to_owned(),
                text: "https://x.test/?token=secret".to_owned(),
                html: "<a>secret</a>".to_owned(),
            },
        }
    }

    #[tokio::test]
    async fn the_log_sender_answers_with_an_id_of_its_own() {
        let key = Uuid::now_v7();
        assert_eq!(
            LogSender.send(&email(), key).await,
            Ok(format!("log-{key}"))
        );
    }

    #[test]
    fn debug_output_shows_neither_the_address_nor_the_body() {
        let shown = format!("{:?}", email());
        assert!(shown.contains("example.com"), "{shown}");
        assert!(!shown.contains("alice"), "{shown}");
        assert!(!shown.contains("secret"), "{shown}");
    }

    #[test]
    fn the_recipient_domain_is_lower_case() {
        assert_eq!(recipient_domain("a@Example.COM"), "example.com");
        assert_eq!(recipient_domain("a@b@c.test"), "c.test");
        assert_eq!(recipient_domain("nobody"), "");
    }
}
```

- [ ] **Step 4: The Resend sender, tested against a stand-in**

The tests start an axum router on `127.0.0.1` that records each request and answers with a
chosen status (decision 10). Create `crates/push-email/src/resend.rs`:

```rust
//! The Resend sender (spec §9): `POST /emails` with the email's id as the idempotency key.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::sender::{EmailSender, OutgoingEmail, SendError};

/// Resend's API.
pub const RESEND_API_URL: &str = "https://api.resend.com";

/// How long one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Sends through Resend's HTTP API. Open and click tracking are settings of the sending
/// domain at Resend, off for ours (spec §9).
#[derive(Clone)]
pub struct ResendSender {
    http: reqwest::Client,
    emails_url: String,
    api_key: String,
    from: String,
}

impl std::fmt::Debug for ResendSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResendSender")
            .field("emails_url", &self.emails_url)
            .field("api_key", &"<redacted>")
            .field("from", &self.from)
            .finish()
    }
}

impl ResendSender {
    /// A sender that posts to `{api_url}/emails` ([`RESEND_API_URL`] in production) with
    /// `api_key`, from the address `from` (`Push Messenger <no-reply@push.kubegames.dk>`).
    pub fn new(api_url: &str, api_key: String, from: String) -> Result<Self, SendError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| SendError::Permanent("could not build the HTTP client".to_owned()))?;
        Ok(Self {
            http,
            emails_url: format!("{}/emails", api_url.trim_end_matches('/')),
            api_key,
            from,
        })
    }
}

#[async_trait]
impl EmailSender for ResendSender {
    async fn send(
        &self,
        email: &OutgoingEmail,
        idempotency_key: Uuid,
    ) -> Result<String, SendError> {
        let body = json!({
            "from": self.from,
            "to": [email.to],
            "subject": email.content.subject,
            "text": email.content.text,
            "html": email.content.html,
            "tags": [{"name": "template", "value": email.template}],
        });
        let response = self
            .http
            .post(&self.emails_url)
            .bearer_auth(&self.api_key)
            .header("Idempotency-Key", idempotency_key.to_string())
            .json(&body)
            .send()
            .await
            // Without the URL or the request: neither belongs in a log.
            .map_err(|_| SendError::Retryable("Resend could not be reached".to_owned()))?;
        let status = response.status();
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return answer["id"].as_str().map(str::to_owned).ok_or_else(|| {
                SendError::Retryable(format!("Resend answered {status} without an id"))
            });
        }
        // Only Resend's error name: its message can quote the address.
        let name: String = answer["name"]
            .as_str()
            .unwrap_or("unknown")
            .chars()
            .filter(|c| c.is_ascii_lowercase() || *c == '_')
            .take(64)
            .collect();
        let reason = format!("Resend answered {status} ({name})");
        if is_retryable(status) {
            Err(SendError::Retryable(reason))
        } else {
            Err(SendError::Permanent(reason))
        }
    }
}

/// Rate limits, a timeout, a concurrent request with the same idempotency key, server
/// errors, and a missing or wrong API key (401, 403): the emails wait while the key is fixed.
fn is_retryable(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::REQUEST_TIMEOUT
            | StatusCode::CONFLICT
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
    ) || status.is_server_error()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Json;
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode as AxumStatus};
    use axum::routing::post;
    use tokio::net::TcpListener;

    use super::*;
    use crate::templates::Rendered;

    /// What the stub received: the headers and the body of each request.
    type Received = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

    /// A stand-in for Resend that answers every request with `status` and `answer`.
    async fn stub(status: u16, answer: Value) -> (String, Received) {
        let received: Received = Arc::default();
        let app = Router::new()
            .route(
                "/emails",
                post(
                    move |State(received): State<Received>,
                          headers: HeaderMap,
                          Json(body): Json<Value>| {
                        let answer = answer.clone();
                        async move {
                            received.lock().unwrap().push((headers, body));
                            (AxumStatus::from_u16(status).unwrap(), Json(answer))
                        }
                    },
                ),
            )
            .with_state(Arc::clone(&received));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, received)
    }

    fn email() -> OutgoingEmail {
        OutgoingEmail {
            to: "alice@example.com".to_owned(),
            template: "verify_email".to_owned(),
            content: Rendered {
                subject: "Verify your email address".to_owned(),
                text: "Open https://x.test/?token=t".to_owned(),
                html: "<p>Open</p>".to_owned(),
            },
        }
    }

    fn sender(url: &str) -> ResendSender {
        ResendSender::new(
            url,
            "re_test_key".to_owned(),
            "Push <no-reply@push.test>".to_owned(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn an_email_is_posted_with_the_key_and_the_idempotency_key() {
        let (url, received) = stub(200, json!({"id": "re-123"})).await;
        let key = Uuid::now_v7();
        assert_eq!(
            sender(&format!("{url}/")).send(&email(), key).await,
            Ok("re-123".to_owned())
        );

        let received = received.lock().unwrap();
        let (headers, body) = &received[0];
        assert_eq!(headers["authorization"], "Bearer re_test_key");
        assert_eq!(headers["idempotency-key"], key.to_string().as_str());
        assert_eq!(
            body,
            &json!({
                "from": "Push <no-reply@push.test>",
                "to": ["alice@example.com"],
                "subject": "Verify your email address",
                "text": "Open https://x.test/?token=t",
                "html": "<p>Open</p>",
                "tags": [{"name": "template", "value": "verify_email"}],
            })
        );
    }

    #[tokio::test]
    async fn rate_limits_key_problems_and_outages_are_retried_refusals_are_not() {
        for status in [401, 403, 409, 429, 500, 503] {
            let (url, _) = stub(status, json!({"name": "busy"})).await;
            let error = sender(&url).send(&email(), Uuid::now_v7()).await;
            assert!(
                matches!(error, Err(SendError::Retryable(_))),
                "{status}: {error:?}"
            );
        }
        let refusal = json!({
            "statusCode": 422,
            "name": "validation_error",
            "message": "Invalid `to` field: alice@example.com",
        });
        let (url, _) = stub(422, refusal).await;
        let error = sender(&url)
            .send(&email(), Uuid::now_v7())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            SendError::Permanent(
                "Resend answered 422 Unprocessable Entity (validation_error)".to_owned()
            )
        );
        assert!(!error.to_string().contains("alice"));
    }

    #[tokio::test]
    async fn an_unreachable_api_is_retried_and_not_named() {
        let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", unused.local_addr().unwrap());
        drop(unused);
        let error = sender(&url)
            .send(&email(), Uuid::now_v7())
            .await
            .unwrap_err();
        assert!(matches!(error, SendError::Retryable(_)), "{error:?}");
        assert!(!error.to_string().contains("127.0.0.1"), "{error}");
    }

    #[tokio::test]
    async fn a_success_without_an_id_is_retried() {
        let (url, _) = stub(200, json!({})).await;
        assert!(matches!(
            sender(&url).send(&email(), Uuid::now_v7()).await,
            Err(SendError::Retryable(_))
        ));
    }

    #[test]
    fn debug_output_hides_the_key() {
        let shown = format!("{:?}", sender(RESEND_API_URL));
        assert!(!shown.contains("re_test_key"), "{shown}");
        assert!(shown.contains("https://api.resend.com/emails"), "{shown}");
    }
}
```

- [ ] **Step 5: Run the tests**

```powershell
cargo test -p push-email
cargo tree -p push-email -i ring -e normal
```

Expected: cargo records the new packages in `Cargo.lock` (`push-email`, `rustls` 0.23,
`hyper-rustls`, `tokio-rustls`, `webpki-roots`) on the first run, then
`test result: ok. 13 passed` (templates 5, sender 3, resend 5;
`an_unreachable_api_is_retried_and_not_named` takes about 2 s on Windows, where a refused
connection is retried). `cargo tree` shows `ring` under `rustls`, and no `aws-lc-rs` or
`openssl` anywhere (`cargo tree -p push-email | Select-String 'aws-lc|openssl'` prints
nothing).

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.toml Cargo.lock crates/push-email
git commit -m "feat(email): templates, the log sender and the Resend sender"
```

Expected: 742 tests pass, and the 1.85 build compiles rustls.

---

### Task 4: Resend webhooks: Svix signatures and delivery events

**Files:**
- Modify: `crates/push-email/Cargo.toml`, `src/lib.rs`
- Create: `crates/push-email/src/webhook.rs`

- [ ] **Step 1: Dependencies and exports**

In `crates/push-email/Cargo.toml`:

Replace

```toml
[dependencies]
async-trait.workspace = true
# Resend is HTTPS: rustls with ring and the webpki roots, no OpenSSL and no cmake.
reqwest = { workspace = true, features = ["rustls-tls"] }
serde_json.workspace = true
thiserror.workspace = true
tracing.workspace = true
```

with

```toml
[dependencies]
async-trait.workspace = true
base64.workspace = true
chrono.workspace = true
hmac.workspace = true
# Resend is HTTPS: rustls with ring and the webpki roots, no OpenSSL and no cmake.
reqwest = { workspace = true, features = ["rustls-tls"] }
serde_json.workspace = true
sha2.workspace = true
thiserror.workspace = true
tracing.workspace = true
```

In `crates/push-email/src/lib.rs`:

Replace

```rust
//! outbox; the worker renders the template here ([`Templates`]) and hands the result to an
//! [`EmailSender`]: [`ResendSender`] in production, [`LogSender`] locally, which only logs
//! that it sent something.
//! Nothing here logs an address, a subject or a body: emails carry sign-in links.
```

with

```rust
//! outbox; the worker renders the template here ([`Templates`]) and hands the result to an
//! [`EmailSender`]: [`ResendSender`] in production, [`LogSender`] locally, which only logs
//! that it sent something. Delivery webhooks are verified and parsed by a
//! [`WebhookVerifier`] ([`ResendWebhooks`]).
//! Nothing here logs an address, a subject or a body: emails carry sign-in links.
```

Replace

```rust
mod sender;
mod templates;

pub use resend::{RESEND_API_URL, ResendSender};
pub use sender::{EmailSender, LogSender, OutgoingEmail, SendError, recipient_domain};
pub use templates::{Rendered, TemplateError, Templates};
```

with

```rust
mod sender;
mod templates;
mod webhook;

pub use resend::{RESEND_API_URL, ResendSender};
pub use sender::{EmailSender, LogSender, OutgoingEmail, SendError, recipient_domain};
pub use templates::{Rendered, TemplateError, Templates};
pub use webhook::{
    DeliveryEvent, DeliveryKind, ResendWebhooks, WEBHOOK_TOLERANCE, WebhookError, WebhookVerifier,
};
```

- [ ] **Step 2: The verifier and the parser, with their tests**

The first test is Svix's published example (secret, id, timestamp, body and signature):
if it verifies, the signed content and the key decoding are right. Then a changed body, id
or timestamp, a signature without `v1,`, a replay outside 5 minutes either way, missing
headers, and Resend's event shapes (decision 11). Create
`crates/push-email/src/webhook.rs`:

```rust
//! Delivery webhooks (spec §9): Resend signs them the Svix way; a verified webhook turns
//! into a [`DeliveryEvent`].

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

/// How far a webhook's timestamp may be from our clock, either way. Older deliveries are
/// refused as replays; Svix uses the same window.
pub const WEBHOOK_TOLERANCE: Duration = Duration::from_secs(5 * 60);

/// What happened to a sent email.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryKind {
    Delivered,
    /// `permanent` is a hard bounce: the address does not take email.
    Bounced {
        permanent: bool,
    },
    /// The recipient marked it as spam.
    Complained,
}

/// One delivery event from the provider.
#[derive(Clone, PartialEq, Eq)]
pub struct DeliveryEvent {
    pub kind: DeliveryKind,
    /// The provider's id of the email, as its sender returned it.
    pub provider_message_id: String,
    pub recipients: Vec<String>,
    /// When the provider says it happened, if it says.
    pub occurred_at: Option<DateTime<Utc>>,
}

impl DeliveryEvent {
    /// Whether the recipients must not get email any more: a hard bounce or a complaint.
    pub fn suppresses(&self) -> bool {
        matches!(
            self.kind,
            DeliveryKind::Bounced { permanent: true } | DeliveryKind::Complained
        )
    }
}

impl std::fmt::Debug for DeliveryEvent {
    /// Without the addresses.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeliveryEvent")
            .field("kind", &self.kind)
            .field("provider_message_id", &self.provider_message_id)
            .field("recipients", &self.recipients.len())
            .field("occurred_at", &self.occurred_at)
            .finish()
    }
}

/// Why a webhook was refused. The messages are fixed: they never quote the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    #[error("the webhook lacks its signature headers")]
    MissingHeaders,
    #[error("the webhook's timestamp is not within the tolerance of our clock")]
    Stale,
    #[error("the webhook's signature does not match")]
    BadSignature,
    #[error("the webhook's body is not a delivery event")]
    Malformed,
}

/// Turns provider webhooks into delivery events (spec §9).
pub trait WebhookVerifier: Send + Sync {
    /// Checks that `body` was signed by the provider within [`WEBHOOK_TOLERANCE`] of
    /// `now`, reading the signature headers through `header`. Returns the webhook's id,
    /// which stays the same when the provider delivers it again.
    fn verify(
        &self,
        header: &dyn Fn(&str) -> Option<String>,
        body: &[u8],
        now: SystemTime,
    ) -> Result<String, WebhookError>;

    /// The delivery event in a verified `body`; `None` for events nothing acts on.
    fn parse(&self, body: &[u8]) -> Result<Option<DeliveryEvent>, WebhookError>;
}

/// Resend's webhooks: Svix signatures (`svix-id`, `svix-timestamp`, `svix-signature`) and
/// `email.*` events.
#[derive(Clone)]
pub struct ResendWebhooks {
    key: Vec<u8>,
}

impl std::fmt::Debug for ResendWebhooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResendWebhooks")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl ResendWebhooks {
    /// The webhook's signing secret, as Resend shows it: `whsec_` and base64.
    pub fn new(secret: &str) -> Result<Self, String> {
        let encoded = secret
            .trim()
            .strip_prefix("whsec_")
            .unwrap_or(secret.trim());
        let key = STANDARD
            .decode(encoded)
            .map_err(|_| "the webhook secret must be whsec_ and base64".to_owned())?;
        if key.len() < 16 {
            return Err("the webhook secret is too short".to_owned());
        }
        Ok(Self { key })
    }
}

impl WebhookVerifier for ResendWebhooks {
    fn verify(
        &self,
        header: &dyn Fn(&str) -> Option<String>,
        body: &[u8],
        now: SystemTime,
    ) -> Result<String, WebhookError> {
        let (Some(id), Some(timestamp), Some(signatures)) = (
            header("svix-id"),
            header("svix-timestamp"),
            header("svix-signature"),
        ) else {
            return Err(WebhookError::MissingHeaders);
        };
        let sent: u64 = timestamp.trim().parse().map_err(|_| WebhookError::Stale)?;
        let now = now
            .duration_since(UNIX_EPOCH)
            .map_err(|_| WebhookError::Stale)?
            .as_secs();
        if now.abs_diff(sent) > WEBHOOK_TOLERANCE.as_secs() {
            return Err(WebhookError::Stale);
        }
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).map_err(|_| WebhookError::BadSignature)?;
        mac.update(id.as_bytes());
        mac.update(b".");
        mac.update(timestamp.trim().as_bytes());
        mac.update(b".");
        mac.update(body);
        // Several signatures while the secret is rotated: any one of them will do.
        let genuine = signatures.split_whitespace().any(|entry| {
            entry
                .strip_prefix("v1,")
                .and_then(|signature| STANDARD.decode(signature).ok())
                .is_some_and(|signature| mac.clone().verify_slice(&signature).is_ok())
        });
        if genuine {
            Ok(id)
        } else {
            Err(WebhookError::BadSignature)
        }
    }

    fn parse(&self, body: &[u8]) -> Result<Option<DeliveryEvent>, WebhookError> {
        let event: Value = serde_json::from_slice(body).map_err(|_| WebhookError::Malformed)?;
        let kind = match event["type"].as_str() {
            Some("email.delivered") => DeliveryKind::Delivered,
            Some("email.bounced") => DeliveryKind::Bounced {
                permanent: event["data"]["bounce"]["type"] == "Permanent",
            },
            Some("email.complained") => DeliveryKind::Complained,
            Some(_) => return Ok(None),
            None => return Err(WebhookError::Malformed),
        };
        let provider_message_id = event["data"]["email_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or(WebhookError::Malformed)?
            .to_owned();
        let recipients = event["data"]["to"]
            .as_array()
            .map(|to| {
                to.iter()
                    .filter_map(|address| address.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let occurred_at = event["created_at"]
            .as_str()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc));
        Ok(Some(DeliveryEvent {
            kind,
            provider_message_id,
            recipients,
            occurred_at,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    // Svix's published example: this secret signed this body at this time.
    const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
    const ID: &str = "msg_p5jXN8AQM9LWM0D4loKWxJek";
    const TIMESTAMP: u64 = 1_614_265_330;
    const SIGNATURE: &str = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";
    const BODY: &[u8] = br#"{"test": 2432232314}"#;

    fn headers(id: &str, timestamp: u64, signature: &str) -> HashMap<String, String> {
        HashMap::from([
            ("svix-id".to_owned(), id.to_owned()),
            ("svix-timestamp".to_owned(), timestamp.to_string()),
            ("svix-signature".to_owned(), signature.to_owned()),
        ])
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn verify(
        headers: &HashMap<String, String>,
        body: &[u8],
        now: SystemTime,
    ) -> Result<String, WebhookError> {
        let webhooks = ResendWebhooks::new(SECRET).unwrap();
        webhooks.verify(&|name| headers.get(name).cloned(), body, now)
    }

    #[test]
    fn svixs_example_verifies() {
        let genuine = headers(ID, TIMESTAMP, SIGNATURE);
        assert_eq!(
            verify(&genuine, BODY, at(TIMESTAMP + 10)),
            Ok(ID.to_owned())
        );
        // One of several signatures is enough, while the secret is rotated.
        let rotated = headers(ID, TIMESTAMP, &format!("v1,AAAA {SIGNATURE}"));
        assert_eq!(verify(&rotated, BODY, at(TIMESTAMP)), Ok(ID.to_owned()));
    }

    #[test]
    fn a_changed_body_id_or_timestamp_breaks_the_signature() {
        let genuine = headers(ID, TIMESTAMP, SIGNATURE);
        assert_eq!(
            verify(&genuine, br#"{"test": 2432232315}"#, at(TIMESTAMP)),
            Err(WebhookError::BadSignature)
        );
        let other_id = headers("msg_other", TIMESTAMP, SIGNATURE);
        assert_eq!(
            verify(&other_id, BODY, at(TIMESTAMP)),
            Err(WebhookError::BadSignature)
        );
        let other_time = headers(ID, TIMESTAMP + 1, SIGNATURE);
        assert_eq!(
            verify(&other_time, BODY, at(TIMESTAMP)),
            Err(WebhookError::BadSignature)
        );
        let unversioned = headers(
            ID,
            TIMESTAMP,
            "g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=",
        );
        assert_eq!(
            verify(&unversioned, BODY, at(TIMESTAMP)),
            Err(WebhookError::BadSignature)
        );
    }

    #[test]
    fn replays_outside_five_minutes_and_unsigned_requests_are_refused() {
        let genuine = headers(ID, TIMESTAMP, SIGNATURE);
        assert!(verify(&genuine, BODY, at(TIMESTAMP + 300)).is_ok());
        assert_eq!(
            verify(&genuine, BODY, at(TIMESTAMP + 301)),
            Err(WebhookError::Stale)
        );
        assert_eq!(
            verify(&genuine, BODY, at(TIMESTAMP - 301)),
            Err(WebhookError::Stale)
        );
        let mut unsigned = genuine.clone();
        unsigned.remove("svix-signature");
        assert_eq!(
            verify(&unsigned, BODY, at(TIMESTAMP)),
            Err(WebhookError::MissingHeaders)
        );
    }

    #[test]
    fn secrets_must_be_base64_keys() {
        assert!(ResendWebhooks::new("whsec_not base64!").is_err());
        assert!(ResendWebhooks::new("whsec_AAAA").is_err());
        let shown = format!("{:?}", ResendWebhooks::new(SECRET).unwrap());
        assert!(!shown.contains("MfKQ"), "{shown}");
    }

    fn parse(event: Value) -> Result<Option<DeliveryEvent>, WebhookError> {
        ResendWebhooks::new(SECRET)
            .unwrap()
            .parse(event.to_string().as_bytes())
    }

    fn resend_event(kind: &str, data: Value) -> Value {
        let mut data = data;
        data["email_id"] = json!("56761188-7520-42d8-8898-ff6fc54ce618");
        data["to"] = json!(["delivered@resend.dev"]);
        json!({"type": kind, "created_at": "2026-11-22T23:41:12.126Z", "data": data})
    }

    #[test]
    fn bounces_complaints_and_deliveries_become_events() {
        let hard = parse(resend_event(
            "email.bounced",
            json!({"bounce": {"type": "Permanent", "subType": "Suppressed"}}),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(hard.kind, DeliveryKind::Bounced { permanent: true });
        assert_eq!(
            hard.provider_message_id,
            "56761188-7520-42d8-8898-ff6fc54ce618"
        );
        assert_eq!(hard.recipients, ["delivered@resend.dev"]);
        assert_eq!(
            hard.occurred_at.unwrap().to_rfc3339(),
            "2026-11-22T23:41:12.126+00:00"
        );
        assert!(hard.suppresses());

        let soft = parse(resend_event(
            "email.bounced",
            json!({"bounce": {"type": "Transient"}}),
        ))
        .unwrap()
        .unwrap();
        assert!(!soft.suppresses());
        let complaint = parse(resend_event("email.complained", json!({})))
            .unwrap()
            .unwrap();
        assert!(complaint.suppresses());
        let delivered = parse(resend_event("email.delivered", json!({})))
            .unwrap()
            .unwrap();
        assert_eq!(delivered.kind, DeliveryKind::Delivered);
        assert!(!delivered.suppresses());
    }

    #[test]
    fn other_events_are_ignored_and_garbage_is_refused() {
        assert_eq!(parse(resend_event("email.opened", json!({}))), Ok(None));
        assert_eq!(parse(json!({"data": {}})), Err(WebhookError::Malformed));
        assert_eq!(
            parse(json!({"type": "email.bounced", "data": {}})),
            Err(WebhookError::Malformed)
        );
        let webhooks = ResendWebhooks::new(SECRET).unwrap();
        assert_eq!(webhooks.parse(b"not json"), Err(WebhookError::Malformed));
    }
}
```

- [ ] **Step 3: Run the tests**

```powershell
cargo test -p push-email
```

Expected: `test result: ok. 19 passed`.

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock crates/push-email
git commit -m "feat(email): Resend webhooks with Svix signatures"
```

Expected: 748 tests pass.

---

### Task 5: `push-worker`: the worker schema and the job queue

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/push-worker/Cargo.toml`, `migrations/0001_worker.sql`, `src/lib.rs`,
  `src/db.rs`, `src/queue.rs`, `tests/queue.rs`

- [ ] **Step 1: The crate**

```powershell
New-Item -ItemType Directory crates\push-worker\src, crates\push-worker\migrations, crates\push-worker\tests -Force
```

In `Cargo.toml`:

Replace

```toml
    "crates/push-store",
    "crates/push-testkit",
]
```

with

```toml
    "crates/push-store",
    "crates/push-testkit",
    "crates/push-worker",
]
```

Replace

```toml
push-store = { path = "crates/push-store" }
push-testkit = { path = "crates/push-testkit" }
rand = "0.9"
redis = { version = "1", default-features = false, features = [
```

with

```toml
push-store = { path = "crates/push-store" }
push-testkit = { path = "crates/push-testkit" }
push-worker = { path = "crates/push-worker" }
rand = "0.9"
redis = { version = "1", default-features = false, features = [
```

Create `crates/push-worker/Cargo.toml` (`chrono` for the claim time, decision 2):

```toml
[package]
name = "push-worker"
description = "The push-messenger worker: outbox relay, email, delivery webhooks and cleanup jobs."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
chrono.workspace = true
push-store.workspace = true
serde_json.workspace = true
sqlx.workspace = true
uuid.workspace = true

[dev-dependencies]
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "time"] }

[lints]
workspace = true
```

- [ ] **Step 2: The queue's tests, failing**

The tests migrate as a plain owner role (the chart's `push_worker`), then cover adding a job
twice, six concurrent claimers over 60 jobs (no job twice), a lease that ends, outcomes
that come after the lease ended and must leave the newer claim alone (a periodic job's
included, whose attempts start again at 0), jobs given back at shutdown, and every outcome,
with a buried email keeping no link (decisions 2 and 3). Create
`crates/push-worker/tests/queue.rs`:

```rust
//! The job queue on a real PostgreSQL (Docker must be running): the migration as a plain
//! owner, idempotent enqueueing, claims that never hand one job to two workers, leases
//! that expire, stale outcomes that never overwrite a newer claim, jobs given back at
//! shutdown, backoff, burial without links and cleanup.

use std::collections::HashSet;
use std::time::Duration;

use push_worker::queue::{NewJob, Outcome, Queue};
use push_worker::{MIGRATOR, migrate_database};
use serde_json::json;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

/// A running Postgres with a database `push_worker` owned by the plain role `push_worker`,
/// migrated as that role, and a pool on it.
async fn worker_database() -> (ContainerAsync<Postgres>, PgPool) {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    // Docker publishes ports on IPv4; "localhost" would try IPv6 first on every connect.
    let admin = PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
    ))
    .await
    .expect("connect as the superuser");
    for statement in [
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement)
            .execute(&admin)
            .await
            .expect(statement);
    }
    admin.close().await;
    let url = format!("postgres://push_worker:worker-secret@127.0.0.1:{port}/push_worker");
    migrate_database(&url).await.expect("migrate as the owner");
    migrate_database(&url).await.expect("a second run");
    let pool = PgPool::connect(&url).await.expect("connect");
    (container, pool)
}

fn job(kind: &str) -> NewJob {
    NewJob {
        id: Uuid::now_v7(),
        kind: kind.to_owned(),
        payload: json!({"n": 1}),
    }
}

async fn job_row(pool: &PgPool, id: Uuid) -> Option<(i32, Option<String>, bool, bool)> {
    sqlx::query_as(
        "SELECT attempts, failed_reason, dead_at IS NOT NULL, run_after > now() \
         FROM worker.job WHERE job_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_owner_migrates_and_a_job_is_added_once_claimed_once_and_deleted() {
    let (_container, pool) = worker_database().await;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applied, MIGRATOR.iter().count() as i64);

    let queue = Queue::new(pool.clone(), Duration::from_secs(60));
    let first = job("user.verified");
    assert_eq!(
        queue
            .enqueue(&[first.clone(), job("email.send")])
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        queue.enqueue(std::slice::from_ref(&first)).await.unwrap(),
        0
    );

    let claimed = queue.claim(10).await.unwrap();
    assert_eq!(claimed.len(), 2);
    let job = &claimed[0];
    assert_eq!(
        (job.id, job.kind.as_str(), &job.payload, job.attempts),
        (first.id, "user.verified", &json!({"n": 1}), 1)
    );
    assert!(
        queue.claim(10).await.unwrap().is_empty(),
        "leased jobs are not due"
    );
    assert!(queue.finish(job, Outcome::Done).await.unwrap());
    assert_eq!(job_row(&pool, first.id).await, None);
}

#[tokio::test]
async fn concurrent_claims_never_share_a_job() {
    let (_container, pool) = worker_database().await;
    let queue = Queue::new(pool, Duration::from_secs(60));
    let jobs: Vec<NewJob> = (0..60).map(|_| job("email.send")).collect();
    queue.enqueue(&jobs).await.unwrap();

    let claims: Vec<_> = (0..6)
        .map(|_| {
            let queue = queue.clone();
            tokio::spawn(async move { queue.claim(15).await.unwrap() })
        })
        .collect();
    let mut seen = HashSet::new();
    for claim in claims {
        for job in claim.await.unwrap() {
            assert!(seen.insert(job.id), "a job was claimed twice");
        }
    }
    assert_eq!(seen.len(), 60);
}

#[tokio::test]
async fn a_job_held_by_a_crashed_worker_comes_back_when_its_lease_ends() {
    let (_container, pool) = worker_database().await;
    let queue = Queue::new(pool, Duration::from_millis(300));
    queue.enqueue(&[job("device.registered")]).await.unwrap();
    let crashed = queue.claim(1).await.unwrap();
    assert_eq!(crashed[0].attempts, 1);
    assert!(queue.claim(1).await.unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let again = queue.claim(1).await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!((again[0].id, again[0].attempts), (crashed[0].id, 2));
}

#[tokio::test]
async fn a_stale_outcome_never_overwrites_a_newer_claim() {
    let (_container, pool) = worker_database().await;
    let queue = Queue::new(pool.clone(), Duration::from_millis(300));
    let periodic = job("cleanup");
    queue
        .enqueue(&[job("email.send"), periodic.clone()])
        .await
        .unwrap();
    let slow = queue.claim(10).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The newer claim takes a long lease, so the checks below never race its expiry.
    let newer = Queue::new(pool.clone(), Duration::from_secs(60))
        .claim(10)
        .await
        .unwrap();
    assert_eq!(newer.len(), 2);

    // The slow worker's outcomes come after its lease ended: all of them are dropped.
    for (job, outcome) in [
        (&slow[0], Outcome::Done),
        (&slow[0], Outcome::Poison("too late".to_owned())),
        (
            &slow[1],
            Outcome::Failed {
                reason: "too late".to_owned(),
                max_attempts: None,
            },
        ),
        (&slow[1], Outcome::RunAgain(Duration::from_secs(3600))),
    ] {
        assert!(!queue.finish(job, outcome).await.unwrap());
    }
    for job in &newer {
        assert_eq!(
            job_row(&pool, job.id).await,
            Some((2, None, false, true)),
            "the newer claim is untouched"
        );
    }

    // A periodic job's attempts start again at 0 after each run; the claim time still
    // tells a later claim from the one that ran it.
    let periodic_claim = newer.iter().find(|job| job.id == periodic.id).unwrap();
    assert!(
        queue
            .finish(periodic_claim, Outcome::RunAgain(Duration::ZERO))
            .await
            .unwrap()
    );
    let next = queue.claim(10).await.unwrap();
    assert_eq!((next[0].id, next[0].attempts), (periodic.id, 1));
    let stale = slow.iter().find(|job| job.id == periodic.id).unwrap();
    assert_eq!(stale.attempts, 1);
    assert!(!queue.finish(stale, Outcome::Done).await.unwrap());
    assert!(queue.finish(&next[0], Outcome::Done).await.unwrap());
}

#[tokio::test]
async fn jobs_given_back_at_shutdown_are_due_at_once_without_the_attempt() {
    let (_container, pool) = worker_database().await;
    let queue = Queue::new(pool.clone(), Duration::from_secs(60));
    queue
        .enqueue(&[job("email.send"), job("device.revoked")])
        .await
        .unwrap();
    let claimed = queue.claim(10).await.unwrap();
    assert_eq!(queue.release(&claimed).await.unwrap(), 2);
    assert_eq!(
        queue.release(&claimed).await.unwrap(),
        0,
        "no longer under that claim"
    );
    for job in &claimed {
        assert_eq!(job_row(&pool, job.id).await, Some((0, None, false, false)));
    }
    let again = queue.claim(10).await.unwrap();
    assert_eq!(again.len(), 2);
    assert!(again.iter().all(|job| job.attempts == 1));
    assert!(!queue.finish(&claimed[0], Outcome::Done).await.unwrap());
}

#[tokio::test]
async fn failures_back_off_poison_is_buried_and_periodic_jobs_run_again() {
    let (_container, pool) = worker_database().await;
    let queue = Queue::new(pool.clone(), Duration::from_secs(60));
    let limited = NewJob {
        payload: json!({
            "template": "verify_email",
            "params": {"link": "https://push.test/v1/auth/verify?token=secret", "handle": "alice"},
        }),
        ..job("email.send")
    };
    let (flaky, poison, periodic) = (job("email.send"), job("device.revoked"), job("cleanup"));
    queue
        .enqueue(&[
            flaky.clone(),
            limited.clone(),
            poison.clone(),
            periodic.clone(),
        ])
        .await
        .unwrap();
    let claimed = queue.claim(10).await.unwrap();
    let find = |id: Uuid| claimed.iter().find(|job| job.id == id).unwrap();

    let failed = |max_attempts| Outcome::Failed {
        reason: "Resend answered 503".to_owned(),
        max_attempts,
    };
    queue.finish(find(flaky.id), failed(None)).await.unwrap();
    queue
        .finish(find(limited.id), failed(Some(1)))
        .await
        .unwrap();
    queue
        .finish(find(poison.id), Outcome::Poison("bad payload".to_owned()))
        .await
        .unwrap();
    queue
        .finish(
            find(periodic.id),
            Outcome::RunAgain(Duration::from_secs(3600)),
        )
        .await
        .unwrap();

    assert_eq!(
        job_row(&pool, flaky.id).await,
        Some((1, Some("Resend answered 503".to_owned()), false, true))
    );
    assert_eq!(
        job_row(&pool, limited.id).await,
        Some((1, Some("Resend answered 503".to_owned()), true, true))
    );
    assert_eq!(
        job_row(&pool, poison.id).await,
        Some((1, Some("bad payload".to_owned()), true, true))
    );
    assert_eq!(
        job_row(&pool, periodic.id).await,
        Some((0, None, false, true))
    );
    // A buried email keeps its payload for inspection, but not the link.
    let kept: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM worker.job WHERE job_id = $1")
            .bind(limited.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        kept,
        json!({"template": "verify_email", "params": {"handle": "alice"}})
    );

    // The backoff of the first failure is 1 s; the buried jobs never come back.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let due: Vec<Uuid> = queue
        .claim(10)
        .await
        .unwrap()
        .iter()
        .map(|job| job.id)
        .collect();
    assert_eq!(due, [flaky.id]);

    assert_eq!(
        queue
            .delete_dead(Duration::from_secs(3600), 10)
            .await
            .unwrap(),
        0
    );
    assert_eq!(queue.delete_dead(Duration::ZERO, 10).await.unwrap(), 2);
    assert_eq!(job_row(&pool, poison.id).await, None);
}
```

Create `crates/push-worker/src/lib.rs` with only its doc comment for now:

```rust
//! push-worker: the worker of push-messenger (spec §2, §3.3, §9).
//!
//! It owns the `worker` schema in its own database (`push_worker`) and drains the other
//! services' outboxes into its job queue: it relays auth-svc's user and device events to
//! message-svc, sends email, takes delivery webhooks, and runs the cleanup jobs.
```

```powershell
cargo test -p push-worker --test queue
```

Expected: cargo records `push-worker` in `Cargo.lock`, then compile errors —
`unresolved imports push_worker::queue, push_worker::MIGRATOR`.

- [ ] **Step 3: The schema**

Spec §3.3's tables, with `job.dead_at` and `job.created_at` (decision 3),
`email_event.event_id text` (decision 11), and indexes for the claims and the cleanups.
Create `crates/push-worker/migrations/0001_worker.sql`:

```sql
-- The worker schema (spec §3.3), owned by the worker in its own database, push_worker.
--
-- job holds the work: auth-svc's outbox events, copied here before they are deleted
-- there, and the periodic cleanups. Workers claim due jobs with SELECT … FOR UPDATE SKIP
-- LOCKED and lease them by moving run_after forward, so several replicas share the work
-- and a crashed worker's jobs come back when their lease ends.

CREATE EXTENSION IF NOT EXISTS citext;

CREATE SCHEMA worker;

CREATE TABLE worker.job (
    -- An outbox event's id, so a second copy of the event is ignored.
    job_id        uuid PRIMARY KEY,
    kind          text NOT NULL,
    payload       jsonb NOT NULL,
    -- Due from then on; a claim moves it to the end of the lease.
    run_after     timestamptz NOT NULL DEFAULT now(),
    attempts      integer NOT NULL DEFAULT 0,
    claimed_at    timestamptz,
    failed_reason text,
    -- Set when the job gave up (poison): kept for inspection, never claimed again.
    dead_at       timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX job_due ON worker.job (run_after, job_id) WHERE dead_at IS NULL;
CREATE INDEX job_dead ON worker.job (dead_at) WHERE dead_at IS NOT NULL;

CREATE TABLE worker.email_message (
    -- The email.send event's id, also Resend's idempotency key.
    email_id            uuid PRIMARY KEY,
    to_address          citext NOT NULL,
    -- For deliverability metrics (spec §9).
    recipient_domain    text NOT NULL,
    template            text NOT NULL,
    -- Without the link: it would sign its holder in.
    params              jsonb NOT NULL CHECK (NOT params ? 'link'),
    provider_message_id text UNIQUE,
    -- 1 queued, 2 sent, 3 suppressed, 4 failed.
    status              smallint NOT NULL CHECK (status BETWEEN 1 AND 4),
    created_at          timestamptz NOT NULL DEFAULT now(),
    sent_at             timestamptz
);
CREATE INDEX email_message_created ON worker.email_message (created_at);

CREATE TABLE worker.email_event (
    -- The webhook's id (svix-id), the same on every delivery of it: a replay is ignored.
    event_id            text PRIMARY KEY,
    provider_message_id text NOT NULL,
    -- 1 delivered, 2 bounced, 3 complained.
    kind                smallint NOT NULL CHECK (kind BETWEEN 1 AND 3),
    occurred_at         timestamptz NOT NULL,
    raw                 jsonb NOT NULL,
    received_at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX email_event_message ON worker.email_event (provider_message_id);
CREATE INDEX email_event_received ON worker.email_event (received_at);

CREATE TABLE worker.email_suppression (
    address    citext PRIMARY KEY,
    -- 1 hard bounce, 2 complaint.
    reason     smallint NOT NULL CHECK (reason BETWEEN 1 AND 2),
    created_at timestamptz NOT NULL DEFAULT now()
);
```

Create `crates/push-worker/src/db.rs`:

```rust
//! The `worker` schema's migrations.

/// The migrations of the `worker` schema, embedded at build time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Applies the migrations the `push_worker` database at `url` does not have yet, on a
/// connection of its own (see [`push_store::run_migrations`]). Run it as the owner of the
/// database: it creates the `citext` extension, which PostgreSQL trusts database owners
/// with.
pub async fn migrate_database(url: &str) -> Result<(), sqlx::migrate::MigrateError> {
    push_store::run_migrations(url, &MIGRATOR).await
}
```

- [ ] **Step 4: The queue**

Create `crates/push-worker/src/queue.rs` (`finish` matches the claim's `attempts` and
`claimed_at` in every statement, and returns whether the job was still under that claim;
`release` gives back jobs not started):

```rust
//! The job queue in `worker.job` (spec §3.3): at least once, shared by every replica.
//!
//! A claim takes due jobs with `FOR UPDATE SKIP LOCKED`, so replicas never wait for each
//! other, and leases them: their `run_after` moves to the end of the lease and `attempts`
//! goes up. A job is deleted when it is done, put back with a backoff when it failed, and
//! buried (`dead_at`) when it can never succeed. A worker that dies holding a job only
//! delays it: the job is due again when the lease ends.
//!
//! A claim is named by the job's `attempts` and `claimed_at`, and every outcome is recorded
//! only while the job is still under that claim: when a slow worker's lease ended and
//! another worker claimed the job again, the slow worker's outcome is dropped instead of
//! overwriting the newer claim.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// The first retry waits this long; each further one twice as long as the one before.
pub const FIRST_RETRY: Duration = Duration::from_secs(1);
/// The longest wait between two attempts.
pub const MAX_RETRY: Duration = Duration::from_secs(10 * 60);
/// A reason is cut to this many characters.
const MAX_REASON_CHARS: usize = 500;

/// A job to add.
#[derive(Debug, Clone, PartialEq)]
pub struct NewJob {
    pub id: Uuid,
    pub kind: String,
    pub payload: Value,
}

/// A claimed job.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: Uuid,
    pub kind: String,
    pub payload: Value,
    /// Attempts so far, this one included.
    pub attempts: i32,
    /// When this claim was made; with `attempts`, it names the claim.
    pub claimed_at: DateTime<Utc>,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Done: the job is deleted.
    Done,
    /// Done for now: a periodic job, due again after this long.
    RunAgain(Duration),
    /// Failed, worth trying again after a [`backoff`]; buried instead once the job has
    /// had `max_attempts` attempts (never, without a maximum).
    Failed {
        reason: String,
        max_attempts: Option<i32>,
    },
    /// Can never succeed (poison): buried at once.
    Poison(String),
}

/// The wait before the next attempt of a job that failed `attempts` times: 1 s, doubling,
/// at most [`MAX_RETRY`].
pub fn backoff(attempts: i32) -> Duration {
    let doublings = attempts.saturating_sub(1).clamp(0, 20) as u32;
    FIRST_RETRY.saturating_mul(1 << doublings).min(MAX_RETRY)
}

/// The queue on the worker's database.
#[derive(Debug, Clone)]
pub struct Queue {
    pool: PgPool,
    lease: Duration,
}

impl Queue {
    /// A claimed job is someone else's for `lease`; make it longer than any attempt takes.
    pub fn new(pool: PgPool, lease: Duration) -> Self {
        Self { pool, lease }
    }

    /// Adds `jobs`, due now. A job whose id is in the queue already is ignored, so adding
    /// the same outbox event twice is harmless. Returns how many were new.
    pub async fn enqueue(&self, jobs: &[NewJob]) -> sqlx::Result<u64> {
        let ids: Vec<Uuid> = jobs.iter().map(|job| job.id).collect();
        let kinds: Vec<&str> = jobs.iter().map(|job| job.kind.as_str()).collect();
        let payloads: Vec<Value> = jobs.iter().map(|job| job.payload.clone()).collect();
        let added = sqlx::query(
            "INSERT INTO worker.job (job_id, kind, payload) \
             SELECT * FROM UNNEST($1::uuid[], $2::text[], $3::jsonb[]) \
             ON CONFLICT (job_id) DO NOTHING",
        )
        .bind(&ids)
        .bind(&kinds)
        .bind(&payloads)
        .execute(&self.pool)
        .await?;
        Ok(added.rows_affected())
    }

    /// Claims up to `limit` due jobs, oldest first, and leases them. Jobs another worker
    /// holds are skipped, not waited for.
    pub async fn claim(&self, limit: i64) -> sqlx::Result<Vec<Job>> {
        let rows: Vec<(Uuid, String, Value, i32, DateTime<Utc>)> = sqlx::query_as(
            "UPDATE worker.job SET attempts = attempts + 1, claimed_at = now(), \
             run_after = now() + make_interval(secs => $2) \
             WHERE job_id IN (SELECT job_id FROM worker.job \
                 WHERE dead_at IS NULL AND run_after <= now() \
                 ORDER BY run_after, job_id LIMIT $1 FOR UPDATE SKIP LOCKED) \
             RETURNING job_id, kind, payload, attempts, claimed_at",
        )
        .bind(limit)
        .bind(self.lease.as_secs_f64())
        .fetch_all(&self.pool)
        .await?;
        let mut jobs: Vec<Job> = rows
            .into_iter()
            .map(|(id, kind, payload, attempts, claimed_at)| Job {
                id,
                kind,
                payload,
                attempts,
                claimed_at,
            })
            .collect();
        // UUIDv7 ids are time-ordered: handle older events first.
        jobs.sort_by_key(|job| job.id);
        Ok(jobs)
    }

    /// Records how the attempt at `job` ended, if the job is still under this claim, and
    /// returns whether it was. `false` means the lease ended and another worker claimed the
    /// job again: the outcome is dropped, never written over the newer claim.
    ///
    /// A buried job keeps its payload for inspection, without `params.link`: an email's
    /// link would sign its holder in.
    pub async fn finish(&self, job: &Job, outcome: Outcome) -> sqlx::Result<bool> {
        let under_claim = |sql: &'static str| {
            sqlx::query(sql)
                .bind(job.id)
                .bind(job.attempts)
                .bind(job.claimed_at)
        };
        let query = match outcome {
            Outcome::Done => under_claim(
                "DELETE FROM worker.job WHERE job_id = $1 AND attempts = $2 AND claimed_at = $3",
            ),
            Outcome::RunAgain(after) => under_claim(
                "UPDATE worker.job SET run_after = now() + make_interval(secs => $4), \
                 attempts = 0, claimed_at = NULL, failed_reason = NULL \
                 WHERE job_id = $1 AND attempts = $2 AND claimed_at = $3",
            )
            .bind(after.as_secs_f64()),
            Outcome::Failed {
                reason,
                max_attempts,
            } if max_attempts.is_none_or(|max| job.attempts < max) => under_claim(
                "UPDATE worker.job SET run_after = now() + make_interval(secs => $4), \
                 claimed_at = NULL, failed_reason = $5 \
                 WHERE job_id = $1 AND attempts = $2 AND claimed_at = $3",
            )
            .bind(backoff(job.attempts).as_secs_f64())
            .bind(cut(reason)),
            Outcome::Failed { reason, .. } | Outcome::Poison(reason) => under_claim(
                "UPDATE worker.job SET dead_at = now(), claimed_at = NULL, failed_reason = $4, \
                 payload = payload #- '{params,link}' \
                 WHERE job_id = $1 AND attempts = $2 AND claimed_at = $3",
            )
            .bind(cut(reason)),
        };
        Ok(query.execute(&self.pool).await?.rows_affected() == 1)
    }

    /// Gives back jobs claimed but not started (at shutdown): due again at once, the
    /// claim's attempt undone. A job no longer under that claim is left alone. Returns how
    /// many went back.
    pub async fn release(&self, jobs: &[Job]) -> sqlx::Result<u64> {
        let ids: Vec<Uuid> = jobs.iter().map(|job| job.id).collect();
        let attempts: Vec<i32> = jobs.iter().map(|job| job.attempts).collect();
        let claimed: Vec<DateTime<Utc>> = jobs.iter().map(|job| job.claimed_at).collect();
        let released = sqlx::query(
            "UPDATE worker.job AS job SET run_after = now(), attempts = job.attempts - 1, \
             claimed_at = NULL \
             FROM UNNEST($1::uuid[], $2::int4[], $3::timestamptz[]) \
                 AS claim (job_id, attempts, claimed_at) \
             WHERE job.job_id = claim.job_id AND job.attempts = claim.attempts \
                 AND job.claimed_at = claim.claimed_at",
        )
        .bind(&ids)
        .bind(&attempts)
        .bind(&claimed)
        .execute(&self.pool)
        .await?;
        Ok(released.rows_affected())
    }

    /// Deletes up to `batch` buried jobs that died more than `older_than` ago. Returns how
    /// many it deleted.
    pub async fn delete_dead(&self, older_than: Duration, batch: i64) -> sqlx::Result<u64> {
        let deleted = sqlx::query(
            "DELETE FROM worker.job WHERE job_id IN (SELECT job_id FROM worker.job \
             WHERE dead_at < now() - make_interval(secs => $1) LIMIT $2)",
        )
        .bind(older_than.as_secs_f64())
        .bind(batch)
        .execute(&self.pool)
        .await?;
        Ok(deleted.rows_affected())
    }
}

fn cut(reason: String) -> String {
    match reason.char_indices().nth(MAX_REASON_CHARS) {
        Some((end, _)) => reason[..end].to_owned(),
        None => reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_from_1_s_to_10_minutes() {
        let waits: Vec<u64> = (1..=12)
            .map(|attempts| backoff(attempts).as_secs())
            .collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 600, 600]);
        assert_eq!(backoff(0), FIRST_RETRY);
        assert_eq!(backoff(i32::MAX), MAX_RETRY);
    }

    #[test]
    fn long_reasons_are_cut() {
        assert_eq!(cut("short".to_owned()), "short");
        assert_eq!(cut("é".repeat(600)).chars().count(), MAX_REASON_CHARS);
    }
}
```

Replace `crates/push-worker/src/lib.rs` with:

```rust
//! push-worker: the worker of push-messenger (spec §2, §3.3, §9).
//!
//! It owns the `worker` schema in its own database (`push_worker`) and drains the other
//! services' outboxes into its job queue: it relays auth-svc's user and device events to
//! message-svc, sends email, takes delivery webhooks, and runs the cleanup jobs.

mod db;
pub mod queue;

pub use db::{MIGRATOR, migrate_database};
```

```powershell
cargo test -p push-worker
```

Expected: the unit tests (`retries_back_off_from_1_s_to_10_minutes`, `long_reasons_are_cut`)
and `test result: ok. 6 passed` for `tests\queue.rs`. Without the fence,
`a_stale_outcome_never_overwrites_a_newer_claim` fails: the slow worker's `Done` deletes
the job the newer claim holds.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.toml Cargo.lock crates/push-worker
git commit -m "feat(worker): the worker schema and the job queue"
```

Expected: 756 tests pass. (Clippy's `cloned_ref_to_slice_refs` is why the test enqueues
`std::slice::from_ref(&first)` rather than `&[first.clone()]`.)

---

### Task 6: The outbox relay and the account events

**Files:**
- Modify: `crates/push-worker/Cargo.toml`, `src/lib.rs`
- Create: `crates/push-worker/src/relay.rs`, `src/accounts.rs`, `src/jobs.rs`,
  `tests/relay.rs`

- [ ] **Step 1: Dependencies**

`tonic` and `push-proto` for message-svc's API; for the tests, auth-svc's migrations and
outbox events, and message-svc's APIs (with its `AccountsToken`) on an in-memory store.
Replace
`crates/push-worker/Cargo.toml` with:

```toml
[package]
name = "push-worker"
description = "The push-messenger worker: outbox relay, email, delivery webhooks and cleanup jobs."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
chrono.workspace = true
push-proto.workspace = true
push-store.workspace = true
serde_json.workspace = true
sqlx.workspace = true
tonic.workspace = true
uuid.workspace = true

[dev-dependencies]
push-auth.workspace = true
push-domain.workspace = true
push-message-svc.workspace = true
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "time"] }
tokio-stream.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: The relay's tests, failing**

The tests put auth-svc's and the worker's databases in one Postgres — auth-svc's migrated
and its grant given, as auth-svc does at start-up — write events with auth-svc's own
`outbox::Event`, and run message-svc's `Messaging` and `Accounts` services on an in-memory
store in the test process. They check that events move once (also after a worker died
between queueing and deleting), that the account events applied in the worst order (all
reversed) leave the revoked device revoked, that a redelivery changes nothing, and which
outcomes are poison and which are retried — a wrong token among the latter. message-svc
requires the token, so every relayed event also proves the worker sends it. Create
`crates/push-worker/tests/relay.rs`:

```rust
//! The relay end to end (Docker must be running): auth-svc's outbox in its own database,
//! drained by the worker's role into the worker's queue, and applied in an in-process
//! message-svc through its `Accounts` API.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use push_auth::outbox::Event;
use push_domain::{InMemoryStore, Messaging, RecordingNotifier, SystemClock};
use push_message_svc::{AccountsApi, AccountsToken, MessagingApi};
use push_proto::id_bytes;
use push_proto::internal::v1::IdentifyRequest;
use push_proto::internal::v1::accounts_server::AccountsServer;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_worker::accounts::Accounts;
use push_worker::jobs::Jobs;
use push_worker::queue::{Job, Outcome, Queue};
use push_worker::relay::OutboxRelay;
use serde_json::json;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Channel;
use uuid::Uuid;

/// The shared token message-svc requires for its `Accounts` API.
const ACCOUNTS_TOKEN: &str = "accounts-token-for-the-relay-tests";

struct World {
    _container: ContainerAsync<Postgres>,
    /// auth-svc's database as its owner, to write events.
    auth: PgPool,
    queue: Queue,
    relay: OutboxRelay,
    jobs: Jobs,
    message_svc: String,
    messaging: MessagingClient<Channel>,
}

/// Postgres with auth-svc's database (migrated, the worker's role granted its access) and
/// the worker's (owned by the worker's role, migrated), and message-svc in this process.
async fn world() -> World {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let server = format!("127.0.0.1:{port}");
    let admin = PgPool::connect(&format!("postgres://postgres:postgres@{server}/postgres"))
        .await
        .unwrap();
    for statement in [
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_auth",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement).execute(&admin).await.unwrap();
    }
    admin.close().await;
    let auth_url = format!("postgres://postgres:postgres@{server}/push_auth");
    push_auth::migrate_database(&auth_url).await.unwrap();
    push_auth::grant_worker_access(&auth_url, "push_worker")
        .await
        .unwrap();
    let worker_url = format!("postgres://push_worker:worker-secret@{server}/push_worker");
    push_worker::migrate_database(&worker_url).await.unwrap();

    let queue = Queue::new(
        PgPool::connect(&worker_url).await.unwrap(),
        Duration::from_secs(60),
    );
    let source = PgPool::connect(&format!(
        "postgres://push_worker:worker-secret@{server}/push_auth"
    ))
    .await
    .unwrap();
    let message_svc = message_svc().await;
    World {
        _container: container,
        auth: PgPool::connect(&auth_url).await.unwrap(),
        relay: OutboxRelay::new(source, queue.clone(), 100),
        queue,
        jobs: Jobs::new(Accounts::new(&message_svc, ACCOUNTS_TOKEN).unwrap()),
        messaging: MessagingClient::connect(message_svc.clone()).await.unwrap(),
        message_svc,
    }
}

/// message-svc's `Messaging` and `Accounts` APIs on an in-memory store; its URL.
async fn message_svc() -> String {
    let messaging = Arc::new(Messaging::new(
        InMemoryStore::default(),
        SystemClock,
        Arc::new(RecordingNotifier::default()),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tonic::transport::Server::builder()
        .add_service(MessagingServer::new(MessagingApi::new(Arc::clone(
            &messaging,
        ))))
        .add_service(AccountsServer::with_interceptor(
            AccountsApi::new(messaging),
            AccountsToken::new(Some(ACCOUNTS_TOKEN.to_owned())),
        ))
        .serve_with_incoming(TcpListenerStream::new(listener));
    tokio::spawn(server);
    url
}

/// Writes `event` to auth-svc's outbox as auth-svc does; returns its id.
async fn write(pool: &PgPool, event: &Event) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO auth.outbox (event_id, kind, payload) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(event.kind())
        .bind(event.payload())
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn outbox_size(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM auth.outbox")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Runs every due job once, in the order given; the outcomes.
async fn run(world: &World, jobs: &[Job]) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for job in jobs {
        let outcome = world.jobs.run(job).await;
        world.queue.finish(job, outcome.clone()).await.unwrap();
        outcomes.push(outcome);
    }
    outcomes
}

async fn identifies(world: &World, user: Uuid, device: Uuid) -> bool {
    world
        .messaging
        .clone()
        .identify(IdentifyRequest {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
        })
        .await
        .is_ok()
}

#[tokio::test]
async fn events_move_from_the_outbox_to_the_queue_once() {
    let world = world().await;
    let user = Uuid::now_v7();
    let first = write(&world.auth, &Event::UserVerified { user_id: user }).await;
    let second = write(&world.auth, &Event::UserPasswordReset { user_id: user }).await;
    // A worker that died after queueing the first event, before deleting it.
    world
        .queue
        .enqueue(&[push_worker::queue::NewJob {
            id: first,
            kind: "user.verified".to_owned(),
            payload: json!({"user_id": user}),
        }])
        .await
        .unwrap();

    assert_eq!(world.relay.drain_once().await.unwrap(), 2);
    assert_eq!(outbox_size(&world.auth).await, 0);
    assert_eq!(world.relay.drain_once().await.unwrap(), 0);
    let ids: Vec<Uuid> = world
        .queue
        .claim(10)
        .await
        .unwrap()
        .iter()
        .map(|job| job.id)
        .collect();
    assert_eq!(ids, [first, second]);
}

#[tokio::test]
async fn relayed_events_register_and_revoke_devices_in_message_svc_in_any_order() {
    let world = world().await;
    let (user, phone, tablet) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for event in [
        Event::UserVerified { user_id: user },
        Event::DeviceRegistered {
            user_id: user,
            device_id: phone,
        },
        Event::DeviceRegistered {
            user_id: user,
            device_id: tablet,
        },
        Event::DeviceRevoked {
            user_id: user,
            device_id: tablet,
        },
        Event::UserPasswordReset { user_id: user },
    ] {
        write(&world.auth, &event).await;
    }
    world.relay.drain_once().await.unwrap();
    let mut jobs = world.queue.claim(10).await.unwrap();
    // The worst order: everything backwards, so the revocation comes first.
    jobs.reverse();
    let outcomes = run(&world, &jobs).await;
    assert_eq!(outcomes, vec![Outcome::Done; 5]);
    assert!(identifies(&world, user, phone).await);
    assert!(!identifies(&world, user, tablet).await);

    // Delivered again (at least once): nothing changes.
    for event in [
        Event::DeviceRegistered {
            user_id: user,
            device_id: tablet,
        },
        Event::DeviceRegistered {
            user_id: user,
            device_id: phone,
        },
    ] {
        write(&world.auth, &event).await;
    }
    world.relay.drain_once().await.unwrap();
    let jobs = world.queue.claim(10).await.unwrap();
    assert_eq!(run(&world, &jobs).await, vec![Outcome::Done; 2]);
    assert!(identifies(&world, user, phone).await);
    assert!(!identifies(&world, user, tablet).await);
}

#[tokio::test]
async fn events_that_can_never_apply_are_poison_and_outages_are_retried() {
    let world = world().await;
    let (alice, bob, device) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    write(
        &world.auth,
        &Event::DeviceRegistered {
            user_id: alice,
            device_id: device,
        },
    )
    .await;
    world.relay.drain_once().await.unwrap();
    let jobs = world.queue.claim(10).await.unwrap();
    assert_eq!(run(&world, &jobs).await, [Outcome::Done]);

    // Bob's event names Alice's device; another event is malformed; a kind nobody knows.
    write(
        &world.auth,
        &Event::DeviceRevoked {
            user_id: bob,
            device_id: device,
        },
    )
    .await;
    world.relay.drain_once().await.unwrap();
    let mut jobs = world.queue.claim(10).await.unwrap();
    jobs.push(Job {
        id: Uuid::now_v7(),
        kind: "device.registered".to_owned(),
        payload: json!({"user_id": bob}),
        attempts: 1,
        claimed_at: Utc::now(),
    });
    jobs.push(Job {
        id: Uuid::now_v7(),
        kind: "sms.send".to_owned(),
        payload: json!({}),
        attempts: 1,
        claimed_at: Utc::now(),
    });
    for job in &jobs {
        let outcome = world.jobs.run(job).await;
        assert!(
            matches!(outcome, Outcome::Poison(_)),
            "{}: {outcome:?}",
            job.kind
        );
    }
    assert!(identifies(&world, alice, device).await);

    // message-svc is not there, or refuses a wrong token: retried, for as long as it
    // takes.
    let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gone = format!("http://{}", unused.local_addr().unwrap());
    drop(unused);
    let wrong_token = "not-the-accounts-token-of-this-cluster";
    for accounts in [
        Accounts::new(&gone, ACCOUNTS_TOKEN).unwrap(),
        Accounts::new(&world.message_svc, wrong_token).unwrap(),
    ] {
        let outcome = Jobs::new(accounts)
            .run(&Job {
                id: Uuid::now_v7(),
                kind: "user.verified".to_owned(),
                payload: json!({"user_id": alice}),
                attempts: 1,
                claimed_at: Utc::now(),
            })
            .await;
        assert!(
            matches!(
                outcome,
                Outcome::Failed {
                    max_attempts: None,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }
}
```

```powershell
cargo test -p push-worker --test relay
```

Expected: compile errors — `push_worker::accounts`, `push_worker::jobs`,
`push_worker::relay` do not exist.

- [ ] **Step 3: The relay**

Create `crates/push-worker/src/relay.rs` (decision 2):

```rust
//! The outbox relay (spec §2): auth-svc's outbox, moved into the job queue.

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::queue::{NewJob, Queue};

/// Moves events from `auth.outbox` into the worker's queue, at least once and never lost.
///
/// One pass locks a batch of the oldest events (`FOR UPDATE SKIP LOCKED`, so several
/// workers drain side by side), adds them to the queue, which commits in the worker's own
/// database, and only then deletes them from the outbox and commits there. A worker that
/// dies in between leaves the events in the outbox; the next pass adds them again, which
/// the queue ignores, since a job's id is its event's id.
#[derive(Debug, Clone)]
pub struct OutboxRelay {
    source: PgPool,
    queue: Queue,
    batch: i64,
}

impl OutboxRelay {
    /// `source` is a pool on auth-svc's database as the worker's role, which may read and
    /// delete `auth.outbox` and nothing else; `batch` events move per pass.
    pub fn new(source: PgPool, queue: Queue, batch: i64) -> Self {
        Self {
            source,
            queue,
            batch,
        }
    }

    /// Moves one batch; returns how many events it moved (0 when the outbox is empty).
    pub async fn drain_once(&self) -> sqlx::Result<usize> {
        let mut tx = self.source.begin().await?;
        let events: Vec<(Uuid, String, Value)> = sqlx::query_as(
            "SELECT event_id, kind, payload FROM auth.outbox \
             ORDER BY created_at, event_id LIMIT $1 FOR UPDATE SKIP LOCKED",
        )
        .bind(self.batch)
        .fetch_all(&mut *tx)
        .await?;
        if events.is_empty() {
            return Ok(0);
        }
        let ids: Vec<Uuid> = events.iter().map(|(id, _, _)| *id).collect();
        let jobs: Vec<NewJob> = events
            .into_iter()
            .map(|(id, kind, payload)| NewJob { id, kind, payload })
            .collect();
        self.queue.enqueue(&jobs).await?;
        sqlx::query("DELETE FROM auth.outbox WHERE event_id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(ids.len())
    }
}
```

- [ ] **Step 4: The account events and the jobs**

Create `crates/push-worker/src/accounts.rs` (decisions 3, 4 and 17: refusals that no retry
can change are poison, everything else — `UNAUTHENTICATED` included — is retried without
limit; every call carries the token):

```rust
//! auth-svc's user and device events, applied in message-svc through its `Accounts` API.

use std::time::Duration;

use push_proto::id_bytes;
use push_proto::internal::v1::accounts_client::AccountsClient;
use push_proto::internal::v1::{AccountDevice, AccountUser};
use serde_json::Value;
use tonic::codegen::InterceptedService;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Status};
use uuid::Uuid;

use crate::queue::Outcome;

/// How long connecting to message-svc may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// One of auth-svc's user and device events (spec §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountEvent {
    UserVerified { user: Uuid },
    DeviceRegistered { user: Uuid, device: Uuid },
    DeviceRevoked { user: Uuid, device: Uuid },
    PasswordReset { user: Uuid },
}

impl AccountEvent {
    /// The event in an outbox row: `None` if `kind` is not an account event, an error if
    /// the payload does not fit the kind.
    pub fn parse(kind: &str, payload: &Value) -> Option<Result<Self, String>> {
        let id = |name: &str| {
            payload[name]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok())
                .ok_or_else(|| format!("{kind}: the payload has no valid {name}"))
        };
        let device = || -> Result<(Uuid, Uuid), String> { Ok((id("user_id")?, id("device_id")?)) };
        Some(match kind {
            "user.verified" => id("user_id").map(|user| Self::UserVerified { user }),
            "user.password_reset" => id("user_id").map(|user| Self::PasswordReset { user }),
            "device.registered" => {
                device().map(|(user, device)| Self::DeviceRegistered { user, device })
            }
            "device.revoked" => device().map(|(user, device)| Self::DeviceRevoked { user, device }),
            _ => return None,
        })
    }
}

/// message-svc's `Accounts` API. Calls are idempotent and order-independent there, so the
/// queue's at-least-once, any-order delivery is safe.
#[derive(Debug, Clone)]
pub struct Accounts {
    client: AccountsClient<InterceptedService<Channel, Bearer>>,
}

impl Accounts {
    /// A client for message-svc at `url` (`http://host:50051`) that presents `token`, the
    /// shared internal token message-svc requires for this API. It connects on first use
    /// and reconnects by itself.
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        let bearer = format!("Bearer {token}")
            .parse()
            .map_err(|_| "the Accounts token cannot be sent as metadata".to_owned())?;
        let mut bearer: MetadataValue<Ascii> = bearer;
        // Never indexed by HTTP/2 or shown in header dumps.
        bearer.set_sensitive(true);
        let channel = Endpoint::from_shared(url.to_owned())
            .map_err(|_| "not a valid message-svc URL".to_owned())?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(CALL_TIMEOUT)
            .connect_lazy();
        Ok(Self {
            client: AccountsClient::with_interceptor(channel, Bearer(bearer)),
        })
    }

    /// Applies `event`. A refusal that can never change (a malformed id, a device id of
    /// another user) is poison; anything else is retried.
    pub async fn apply(&self, event: AccountEvent) -> Outcome {
        let mut client = self.client.clone();
        let user_message = |user: Uuid| AccountUser {
            user_id: id_bytes(user),
        };
        let device_message = |user: Uuid, device: Uuid| AccountDevice {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
        };
        let result = match event {
            AccountEvent::UserVerified { user } => {
                client.user_verified(user_message(user)).await.map(|_| ())
            }
            AccountEvent::PasswordReset { user } => {
                client.password_reset(user_message(user)).await.map(|_| ())
            }
            AccountEvent::DeviceRegistered { user, device } => client
                .device_registered(device_message(user, device))
                .await
                .map(|_| ()),
            AccountEvent::DeviceRevoked { user, device } => client
                .device_revoked(device_message(user, device))
                .await
                .map(|_| ()),
        };
        match result {
            Ok(()) => Outcome::Done,
            Err(status) => outcome(&status),
        }
    }
}

/// Adds `authorization: Bearer <token>` to every call.
#[derive(Clone)]
struct Bearer(MetadataValue<Ascii>);

impl std::fmt::Debug for Bearer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Bearer(<redacted>)")
    }
}

impl Interceptor for Bearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.0.clone());
        Ok(request)
    }
}

/// Refusals that no retry can change are poison; the rest (unavailable, aborted,
/// deadline exceeded, transport errors, and unauthenticated: a token to fix in the
/// configuration) are retried for as long as it takes, since message-svc must learn every
/// account event eventually.
fn outcome(status: &Status) -> Outcome {
    let reason = format!("message-svc answered {:?}", status.code());
    match status.code() {
        Code::InvalidArgument
        | Code::PermissionDenied
        | Code::NotFound
        | Code::AlreadyExists
        | Code::FailedPrecondition
        | Code::OutOfRange => Outcome::Poison(reason),
        _ => Outcome::Failed {
            reason,
            max_attempts: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn account_payloads_parse_and_other_kinds_are_not_ours() {
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        let both = json!({"user_id": user, "device_id": device});
        assert_eq!(
            AccountEvent::parse("device.revoked", &both),
            Some(Ok(AccountEvent::DeviceRevoked { user, device }))
        );
        assert_eq!(
            AccountEvent::parse("user.password_reset", &json!({"user_id": user})),
            Some(Ok(AccountEvent::PasswordReset { user }))
        );
        assert_eq!(AccountEvent::parse("email.send", &both), None);
        assert!(matches!(
            AccountEvent::parse("device.registered", &json!({"user_id": user})),
            Some(Err(_))
        ));
        assert!(matches!(
            AccountEvent::parse("user.verified", &json!({"user_id": "nope"})),
            Some(Err(_))
        ));
    }

    #[test]
    fn permanent_refusals_are_poison_and_outages_are_retried() {
        assert!(matches!(
            outcome(&Status::permission_denied("not yours")),
            Outcome::Poison(_)
        ));
        assert!(matches!(
            outcome(&Status::invalid_argument("bad id")),
            Outcome::Poison(_)
        ));
        for status in [
            Status::unavailable("down"),
            Status::aborted("conflict"),
            Status::deadline_exceeded("slow"),
            Status::unknown("transport"),
            Status::unauthenticated("wrong token"),
        ] {
            assert!(
                matches!(
                    outcome(&status),
                    Outcome::Failed {
                        max_attempts: None,
                        ..
                    }
                ),
                "{status:?}"
            );
        }
    }
}
```

Create `crates/push-worker/src/jobs.rs`:

```rust
//! What the worker does with each kind of job.

use crate::accounts::{AccountEvent, Accounts};
use crate::queue::{Job, Outcome};

/// Runs jobs by kind: auth-svc's account events go to message-svc.
#[derive(Debug, Clone)]
pub struct Jobs {
    accounts: Accounts,
}

impl Jobs {
    pub fn new(accounts: Accounts) -> Self {
        Self { accounts }
    }

    /// One attempt at `job`. A kind nobody handles, or a payload that does not fit its
    /// kind, is poison.
    pub async fn run(&self, job: &Job) -> Outcome {
        match AccountEvent::parse(&job.kind, &job.payload) {
            Some(Ok(event)) => self.accounts.apply(event).await,
            Some(Err(reason)) => Outcome::Poison(reason),
            None => Outcome::Poison(format!("no handler for jobs of kind {:?}", job.kind)),
        }
    }
}
```

In `crates/push-worker/src/lib.rs`:

Replace

```rust
//! message-svc, sends email, takes delivery webhooks, and runs the cleanup jobs.

mod db;
pub mod queue;

pub use db::{MIGRATOR, migrate_database};
```

with

```rust
//! message-svc, sends email, takes delivery webhooks, and runs the cleanup jobs.

pub mod accounts;
mod db;
pub mod jobs;
pub mod queue;
pub mod relay;

pub use db::{MIGRATOR, migrate_database};
```

```powershell
cargo test -p push-worker
```

Expected: 4 unit tests, `tests\queue.rs` 6, `tests\relay.rs` 3 — all pass.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock crates/push-worker
git commit -m "feat(worker): relay auth-svc's account events to message-svc"
```

Expected: 761 tests pass.

---

### Task 7: Email jobs, delivery webhooks and the dev mailbox

**Files:**
- Modify: `crates/push-email/src/webhook.rs`, `crates/push-worker/Cargo.toml`,
  `src/lib.rs`, `src/jobs.rs`, `tests/relay.rs`
- Create: `crates/push-worker/src/email.rs`, `src/deliveries.rs`, `src/http.rs`,
  `tests/email.rs`

- [ ] **Step 1: A signer for stand-ins**

Tests (and Plan 6B's minikube check) post webhooks as Resend would, so `ResendWebhooks`
gets `sign`, checked against Svix's example. In `crates/push-email/src/webhook.rs`:

Replace

```rust
        Ok(Self { key })
    }
}
```

with

```rust
        Ok(Self { key })
    }

    /// The `svix-signature` header for `body`, sent as webhook `id` at `timestamp` (Unix
    /// seconds): what Resend would send. For tests and tools that stand in for Resend.
    pub fn sign(&self, id: &str, timestamp: u64, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC takes any key");
        mac.update(format!("{id}.{timestamp}.").as_bytes());
        mac.update(body);
        format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
    }
}
```

Replace

```rust
    #[test]
    fn svixs_example_verifies() {
        let genuine = headers(ID, TIMESTAMP, SIGNATURE);
        assert_eq!(
```

with

```rust
    #[test]
    fn svixs_example_verifies_and_signs_the_same() {
        let genuine = headers(ID, TIMESTAMP, SIGNATURE);
        assert_eq!(
```

Replace

```rust
        let rotated = headers(ID, TIMESTAMP, &format!("v1,AAAA {SIGNATURE}"));
        assert_eq!(verify(&rotated, BODY, at(TIMESTAMP)), Ok(ID.to_owned()));
    }
```

with

```rust
        let rotated = headers(ID, TIMESTAMP, &format!("v1,AAAA {SIGNATURE}"));
        assert_eq!(verify(&rotated, BODY, at(TIMESTAMP)), Ok(ID.to_owned()));
        let webhooks = ResendWebhooks::new(SECRET).unwrap();
        assert_eq!(webhooks.sign(ID, TIMESTAMP, BODY), SIGNATURE);
    }
```

- [ ] **Step 2: Dependencies**

Replace `crates/push-worker/Cargo.toml` with:

```toml
[package]
name = "push-worker"
description = "The push-messenger worker: outbox relay, email, delivery webhooks and cleanup jobs."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
async-trait.workspace = true
chrono.workspace = true
axum.workspace = true
push-email.workspace = true
push-proto.workspace = true
push-store.workspace = true
serde.workspace = true
serde_json.workspace = true
sqlx.workspace = true
thiserror.workspace = true
tonic.workspace = true
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
http-body-util.workspace = true
push-auth.workspace = true
push-domain.workspace = true
push-message-svc.workspace = true
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "time"] }
tokio-stream.workspace = true
tower.workspace = true

[lints]
workspace = true
```

- [ ] **Step 3: The tests, failing**

A scripted sender records what it was given and answers from a queue of results. The tests
cover sending once and recording without the link, a suppressed address, retries until the
12th attempt and refusals, webhooks recorded once that suppress hard bounces and complaints
(also the address an email went to, found by the provider's id), drop signed bodies they
cannot read and refuse forgeries, and the HTTP API: signed webhooks in (204, a signed but
malformed one too), unsigned out, and the mailbox only when enabled (decisions 9–12).
Create `crates/push-worker/tests/email.rs`:

```rust
//! Email on a real PostgreSQL (Docker must be running): sending once, suppression,
//! retries and refusals, delivery webhooks, and the HTTP API.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use push_email::{EmailSender, LogSender, OutgoingEmail, ResendWebhooks, SendError, Templates};
use push_worker::deliveries::{Deliveries, Received};
use push_worker::email::{EMAIL_MAX_ATTEMPTS, Mailbox, MailboxSender, Mailer};
use push_worker::http::{Http, WEBHOOK_PATH, router};
use push_worker::migrate_database;
use push_worker::queue::{Job, Outcome};
use serde_json::{Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tower::ServiceExt;
use uuid::Uuid;

const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
const LINK: &str = "https://push.test/v1/auth/verify?token=secret-token";

async fn database() -> (ContainerAsync<Postgres>, PgPool) {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    migrate_database(&url).await.unwrap();
    let pool = PgPool::connect(&url).await.unwrap();
    (container, pool)
}

/// A sender that records what it was given and answers from a script (then "re-N").
#[derive(Default)]
struct Scripted {
    sent: Mutex<Vec<(OutgoingEmail, Uuid)>>,
    answers: Mutex<VecDeque<Result<String, SendError>>>,
}

#[async_trait]
impl EmailSender for Scripted {
    async fn send(&self, email: &OutgoingEmail, key: Uuid) -> Result<String, SendError> {
        let mut sent = self.sent.lock().unwrap();
        sent.push((email.clone(), key));
        let answer = self.answers.lock().unwrap().pop_front();
        answer.unwrap_or_else(|| Ok(format!("re-{}", sent.len())))
    }
}

fn verify_email(to: &str) -> Job {
    Job {
        id: Uuid::now_v7(),
        kind: "email.send".to_owned(),
        payload: json!({
            "user_id": Uuid::now_v7(),
            "to": to,
            "template": "verify_email",
            "params": {"link": LINK, "handle": "alice", "expires_in_minutes": 30},
        }),
        attempts: 1,
        claimed_at: Utc::now(),
    }
}

async fn message(pool: &PgPool, id: Uuid) -> Option<(i16, Option<String>, Value, String)> {
    sqlx::query_as(
        "SELECT status, provider_message_id, params, recipient_domain \
         FROM worker.email_message WHERE email_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn an_email_is_rendered_sent_once_and_recorded_without_its_link() {
    let (_container, pool) = database().await;
    let sender = Arc::new(Scripted::default());
    let mailer = Mailer::new(pool.clone(), sender.clone(), Templates::default());
    let job = verify_email("alice@Example.com");
    assert_eq!(mailer.send(&job).await, Outcome::Done);
    assert_eq!(mailer.send(&job).await, Outcome::Done, "a second attempt");

    let sent = sender.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "sent once");
    let (email, key) = &sent[0];
    assert_eq!(*key, job.id);
    assert_eq!(email.to, "alice@Example.com");
    assert!(email.content.text.contains(LINK));
    assert_eq!(
        message(&pool, job.id).await,
        Some((
            2,
            Some("re-1".to_owned()),
            json!({"handle": "alice", "expires_in_minutes": 30}),
            "example.com".to_owned()
        ))
    );
}

#[tokio::test]
async fn a_suppressed_address_gets_nothing() {
    let (_container, pool) = database().await;
    sqlx::query("INSERT INTO worker.email_suppression (address, reason) VALUES ($1, 1)")
        .bind("ALICE@example.com")
        .execute(&pool)
        .await
        .unwrap();
    let sender = Arc::new(Scripted::default());
    let mailer = Mailer::new(pool.clone(), sender.clone(), Templates::default());
    let job = verify_email("alice@example.com");
    assert_eq!(mailer.send(&job).await, Outcome::Done);
    assert!(sender.sent.lock().unwrap().is_empty());
    assert_eq!(message(&pool, job.id).await.unwrap().0, 3);
}

#[tokio::test]
async fn failures_are_retried_until_the_link_expires_and_refusals_fail_at_once() {
    let (_container, pool) = database().await;
    let sender = Arc::new(Scripted::default());
    let mailer = Mailer::new(pool.clone(), sender.clone(), Templates::default());
    let outage = || Err(SendError::Retryable("Resend answered 503".to_owned()));

    let mut job = verify_email("alice@example.com");
    sender.answers.lock().unwrap().push_back(outage());
    assert_eq!(
        mailer.send(&job).await,
        Outcome::Failed {
            reason: "Resend answered 503".to_owned(),
            max_attempts: None,
        }
    );
    assert_eq!(message(&pool, job.id).await.unwrap().0, 1, "still queued");
    job.attempts = EMAIL_MAX_ATTEMPTS;
    sender.answers.lock().unwrap().push_back(outage());
    assert_eq!(
        mailer.send(&job).await,
        Outcome::Poison("Resend answered 503".to_owned())
    );
    assert_eq!(message(&pool, job.id).await.unwrap().0, 4, "failed");

    let refused = verify_email("bob@example.com");
    sender
        .answers
        .lock()
        .unwrap()
        .push_back(Err(SendError::Permanent("Resend answered 422".to_owned())));
    assert!(matches!(mailer.send(&refused).await, Outcome::Poison(_)));
    assert_eq!(message(&pool, refused.id).await.unwrap().0, 4);

    let mut unknown = verify_email("carol@example.com");
    unknown.payload["template"] = json!("welcome");
    assert!(matches!(mailer.send(&unknown).await, Outcome::Poison(_)));
    assert_eq!(message(&pool, unknown.id).await.unwrap().0, 4);
    let mut malformed = verify_email("dave@example.com");
    malformed.payload = json!({"template": "verify_email"});
    assert!(matches!(mailer.send(&malformed).await, Outcome::Poison(_)));
    assert_eq!(sender.sent.lock().unwrap().len(), 3);
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A Resend webhook of `kind` about the email `email_id` to `to`, signed now.
fn webhook(
    id: &str,
    kind: &str,
    email_id: &str,
    to: &str,
    bounce: &str,
) -> (Vec<(String, String)>, Vec<u8>) {
    let body = json!({
        "type": kind,
        "created_at": "2026-10-13T10:00:00.000Z",
        "data": {"email_id": email_id, "to": [to], "bounce": {"type": bounce}},
    })
    .to_string()
    .into_bytes();
    signed(id, body)
}

/// `body` with Resend's signature headers, signed now.
fn signed(id: &str, body: Vec<u8>) -> (Vec<(String, String)>, Vec<u8>) {
    let timestamp = now();
    let signature = ResendWebhooks::new(SECRET)
        .unwrap()
        .sign(id, timestamp, &body);
    let headers = vec![
        ("svix-id".to_owned(), id.to_owned()),
        ("svix-timestamp".to_owned(), timestamp.to_string()),
        ("svix-signature".to_owned(), signature),
    ];
    (headers, body)
}

async fn receive(
    deliveries: &Deliveries,
    (headers, body): &(Vec<(String, String)>, Vec<u8>),
) -> Result<Received, push_worker::deliveries::DeliveryError> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    deliveries.receive(&header, body).await
}

async fn suppressed(pool: &PgPool) -> Vec<(String, i16)> {
    sqlx::query_as("SELECT address::text, reason FROM worker.email_suppression ORDER BY address")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn webhooks_are_recorded_once_and_hard_bounces_and_complaints_suppress() {
    let (_container, pool) = database().await;
    let deliveries = Deliveries::new(pool.clone(), Arc::new(ResendWebhooks::new(SECRET).unwrap()));
    // An email the worker sent to Alice, whose bounce names another address.
    let mailer = Mailer::new(
        pool.clone(),
        Arc::new(Scripted::default()),
        Templates::default(),
    );
    mailer.send(&verify_email("alice@example.com")).await;

    let hard = webhook(
        "msg_1",
        "email.bounced",
        "re-1",
        "alias@example.com",
        "Permanent",
    );
    assert_eq!(
        receive(&deliveries, &hard).await.unwrap(),
        Received::Recorded { suppressed: 2 }
    );
    assert_eq!(receive(&deliveries, &hard).await.unwrap(), Received::Replay);
    let soft = webhook(
        "msg_2",
        "email.bounced",
        "re-9",
        "bob@example.com",
        "Transient",
    );
    assert_eq!(
        receive(&deliveries, &soft).await.unwrap(),
        Received::Recorded { suppressed: 0 }
    );
    let complaint = webhook("msg_3", "email.complained", "re-8", "carol@example.com", "");
    assert_eq!(
        receive(&deliveries, &complaint).await.unwrap(),
        Received::Recorded { suppressed: 1 }
    );
    let opened = webhook("msg_4", "email.opened", "re-7", "dave@example.com", "");
    assert_eq!(
        receive(&deliveries, &opened).await.unwrap(),
        Received::Ignored
    );
    // Signed, but not an event we can read: dropped, yet acknowledged.
    for body in [
        b"not json".to_vec(),
        br#"{"type":"email.bounced"}"#.to_vec(),
    ] {
        assert_eq!(
            receive(&deliveries, &signed("msg_9", body)).await.unwrap(),
            Received::Malformed
        );
    }

    assert_eq!(
        suppressed(&pool).await,
        [
            ("alias@example.com".to_owned(), 1),
            ("alice@example.com".to_owned(), 1),
            ("carol@example.com".to_owned(), 2),
        ]
    );
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM worker.email_event")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(events, 3);

    let (mut forged, body) = webhook("msg_5", "email.complained", "re-6", "eve@example.com", "");
    forged[2].1 = "v1,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned();
    assert!(receive(&deliveries, &(forged, body)).await.is_err());
    assert_eq!(suppressed(&pool).await.len(), 3);
}

async fn call(app: &axum::Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

#[tokio::test]
async fn the_http_api_takes_signed_webhooks_and_serves_the_mailbox_only_in_dev() {
    let (_container, pool) = database().await;
    let deliveries = Deliveries::new(pool.clone(), Arc::new(ResendWebhooks::new(SECRET).unwrap()));
    let mailbox = Mailbox::default();
    let mailer = Mailer::new(
        pool,
        Arc::new(MailboxSender::new(LogSender, mailbox.clone())),
        Templates::default(),
    );
    mailer.send(&verify_email("alice@example.com")).await;
    let dev = router(Http {
        deliveries: Some(deliveries.clone()),
        mailbox: Some(mailbox),
    });
    let production = router(Http {
        deliveries: Some(deliveries),
        mailbox: None,
    });

    let (headers, body) = webhook("msg_1", "email.delivered", "log-1", "alice@example.com", "");
    let post = |headers: &[(String, String)], body: Vec<u8>| {
        let mut request = Request::post(WEBHOOK_PATH);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request.body(Body::from(body)).unwrap()
    };
    assert_eq!(
        call(&production, post(&headers, body.clone())).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&production, post(&headers[..2], body.clone())).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (headers, _) = webhook("msg_2", "email.delivered", "log-1", "alice@example.com", "");
    assert_eq!(
        call(&production, post(&headers, b"not json".to_vec()))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // Signed but malformed: 204, or Resend would send it again for days.
    let (headers, body) = signed("msg_3", b"not json".to_vec());
    assert_eq!(
        call(&production, post(&headers, body)).await.0,
        StatusCode::NO_CONTENT
    );

    let mailbox_request = || {
        Request::get("/dev/mailbox?to=alice@example.com")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        call(&production, mailbox_request()).await.0,
        StatusCode::NOT_FOUND
    );
    let (status, body) = call(&dev, mailbox_request()).await;
    assert_eq!(status, StatusCode::OK);
    let emails: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(emails["emails"][0]["template"], "verify_email");
    assert!(emails["emails"][0]["text"].as_str().unwrap().contains(LINK));
    let disabled = router(Http::default());
    assert_eq!(
        call(&disabled, post(&[], Vec::new())).await.0,
        StatusCode::NOT_FOUND
    );
}
```

```powershell
cargo test -p push-worker --test email
```

Expected: compile errors — `push_worker::deliveries`, `push_worker::email` and
`push_worker::http` do not exist.

- [ ] **Step 4: Sending, and the DEV ONLY mailbox**

Create `crates/push-worker/src/email.rs`:

```rust
//! Sending email (spec §9): `email.send` jobs are rendered, checked against the
//! suppression list, recorded in `worker.email_message` and handed to the sender.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use push_email::{EmailSender, OutgoingEmail, SendError, Templates, recipient_domain};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::queue::{Job, Outcome};

/// An email is given up after this many attempts, about half an hour with the backoff of
/// the queue: by then its link has expired anyway.
pub const EMAIL_MAX_ATTEMPTS: i32 = 12;

const QUEUED: i16 = 1;
const SENT: i16 = 2;
const SUPPRESSED: i16 = 3;
const FAILED: i16 = 4;

/// Sends the emails of `email.send` jobs.
#[derive(Clone)]
pub struct Mailer {
    pool: PgPool,
    sender: Arc<dyn EmailSender>,
    templates: Templates,
}

impl std::fmt::Debug for Mailer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Mailer")
            .field("templates", &self.templates)
            .finish_non_exhaustive()
    }
}

impl Mailer {
    pub fn new(pool: PgPool, sender: Arc<dyn EmailSender>, templates: Templates) -> Self {
        Self {
            pool,
            sender,
            templates,
        }
    }

    /// One attempt at an `email.send` job (`{user_id, to, template, params}`). The job's id
    /// is the email's id and the provider's idempotency key, so a retry never sends a
    /// second copy, and an email already sent, suppressed or failed is not tried again.
    pub async fn send(&self, job: &Job) -> Outcome {
        match self.attempt(job).await {
            Ok(outcome) => outcome,
            Err(_) => Outcome::Failed {
                reason: "the worker's database did not answer".to_owned(),
                max_attempts: None,
            },
        }
    }

    async fn attempt(&self, job: &Job) -> sqlx::Result<Outcome> {
        let payload = &job.payload;
        let (Some(to), Some(template)) = (payload["to"].as_str(), payload["template"].as_str())
        else {
            return Ok(Outcome::Poison(
                "email.send: the payload has no to or template".to_owned(),
            ));
        };
        let params = &payload["params"];
        let status: Option<i16> =
            sqlx::query_scalar("SELECT status FROM worker.email_message WHERE email_id = $1")
                .bind(job.id)
                .fetch_optional(&self.pool)
                .await?;
        if status.is_some_and(|status| status != QUEUED) {
            return Ok(Outcome::Done);
        }
        let suppressed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM worker.email_suppression WHERE address = $1::citext)",
        )
        .bind(to)
        .fetch_one(&self.pool)
        .await?;
        if suppressed {
            self.record(job.id, to, template, params, SUPPRESSED)
                .await?;
            tracing::info!(email_id = %job.id, template, "email not sent: the address is suppressed");
            return Ok(Outcome::Done);
        }
        let content = match self.templates.render(template, params) {
            Ok(content) => content,
            Err(error) => {
                self.record(job.id, to, template, params, FAILED).await?;
                return Ok(Outcome::Poison(error.to_string()));
            }
        };
        self.record(job.id, to, template, params, QUEUED).await?;
        let email = OutgoingEmail {
            to: to.to_owned(),
            template: template.to_owned(),
            content,
        };
        match self.sender.send(&email, job.id).await {
            Ok(provider_message_id) => {
                sqlx::query(
                    "UPDATE worker.email_message SET status = $2, provider_message_id = $3, \
                     sent_at = now() WHERE email_id = $1",
                )
                .bind(job.id)
                .bind(SENT)
                .bind(provider_message_id)
                .execute(&self.pool)
                .await?;
                Ok(Outcome::Done)
            }
            Err(SendError::Retryable(reason)) if job.attempts < EMAIL_MAX_ATTEMPTS => {
                Ok(Outcome::Failed {
                    reason,
                    max_attempts: None,
                })
            }
            Err(SendError::Retryable(reason) | SendError::Permanent(reason)) => {
                // Only while still queued: a late duplicate run must not undo a send.
                sqlx::query(
                    "UPDATE worker.email_message SET status = $2 WHERE email_id = $1 AND status = 1",
                )
                    .bind(job.id)
                    .bind(FAILED)
                    .execute(&self.pool)
                    .await?;
                Ok(Outcome::Poison(reason))
            }
        }
    }

    /// Records the email with `status`, or moves a queued one to `status`. Its link is
    /// left out: it would sign its holder in.
    async fn record(
        &self,
        id: Uuid,
        to: &str,
        template: &str,
        params: &Value,
        status: i16,
    ) -> sqlx::Result<()> {
        let mut kept = params.clone();
        if let Some(params) = kept.as_object_mut() {
            params.remove("link");
        }
        sqlx::query(
            "INSERT INTO worker.email_message \
             (email_id, to_address, recipient_domain, template, params, status) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (email_id) DO UPDATE SET status = EXCLUDED.status \
             WHERE worker.email_message.status = 1",
        )
        .bind(id)
        .bind(to)
        .bind(recipient_domain(to))
        .bind(template)
        .bind(kept)
        .bind(status)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// How many emails the DEV ONLY mailbox keeps, newest first.
const MAILBOX_SIZE: usize = 1000;
/// How many emails one look into the mailbox shows.
const MAILBOX_PAGE: usize = 20;

/// DEV ONLY: the emails the log sender delivered, in memory, so a local cluster and the
/// smoke test can open their links.
#[derive(Debug, Clone, Default)]
pub struct Mailbox {
    emails: Arc<Mutex<VecDeque<OutgoingEmail>>>,
}

impl Mailbox {
    /// The newest emails to `address` (any case), newest first, at most 20.
    pub fn to(&self, address: &str) -> Vec<OutgoingEmail> {
        let emails = self
            .emails
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        emails
            .iter()
            .filter(|email| email.to.eq_ignore_ascii_case(address.trim()))
            .take(MAILBOX_PAGE)
            .cloned()
            .collect()
    }

    fn deliver(&self, email: &OutgoingEmail) {
        let mut emails = self
            .emails
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        emails.push_front(email.clone());
        emails.truncate(MAILBOX_SIZE);
    }
}

/// DEV ONLY: a sender that also drops every email it sent into a [`Mailbox`].
#[derive(Debug, Clone)]
pub struct MailboxSender<S> {
    inner: S,
    mailbox: Mailbox,
}

impl<S> MailboxSender<S> {
    pub fn new(inner: S, mailbox: Mailbox) -> Self {
        Self { inner, mailbox }
    }
}

#[async_trait]
impl<S: EmailSender> EmailSender for MailboxSender<S> {
    async fn send(
        &self,
        email: &OutgoingEmail,
        idempotency_key: Uuid,
    ) -> Result<String, SendError> {
        let id = self.inner.send(email, idempotency_key).await?;
        self.mailbox.deliver(email);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use push_email::{LogSender, Rendered};

    use super::*;

    fn email(to: &str, subject: &str) -> OutgoingEmail {
        OutgoingEmail {
            to: to.to_owned(),
            template: "verify_email".to_owned(),
            content: Rendered {
                subject: subject.to_owned(),
                text: String::new(),
                html: String::new(),
            },
        }
    }

    #[tokio::test]
    async fn the_mailbox_shows_what_was_sent_to_an_address_newest_first() {
        let mailbox = Mailbox::default();
        let sender = MailboxSender::new(LogSender, mailbox.clone());
        for (to, subject) in [
            ("alice@example.com", "first"),
            ("bob@example.com", "other"),
            ("Alice@Example.com", "second"),
        ] {
            sender
                .send(&email(to, subject), Uuid::now_v7())
                .await
                .unwrap();
        }
        let subjects: Vec<String> = mailbox
            .to(" alice@example.com")
            .into_iter()
            .map(|email| email.content.subject)
            .collect();
        assert_eq!(subjects, ["second", "first"]);
        assert!(mailbox.to("carol@example.com").is_empty());
    }

    #[test]
    fn the_mailbox_keeps_the_newest_emails_only() {
        let mailbox = Mailbox::default();
        for n in 0..MAILBOX_SIZE + 5 {
            mailbox.deliver(&email("a@example.com", &n.to_string()));
        }
        assert_eq!(mailbox.emails.lock().unwrap().len(), MAILBOX_SIZE);
        let newest = mailbox.to("a@example.com");
        assert_eq!(newest.len(), MAILBOX_PAGE);
        assert_eq!(newest[0].content.subject, (MAILBOX_SIZE + 4).to_string());
    }
}
```

- [ ] **Step 5: Webhooks and the HTTP API**

Create `crates/push-worker/src/deliveries.rs`. `receive` takes the header lookup as
`&(dyn Fn … + Sync)`: the future holds it across `.await`, and axum's handler must be
`Send`. A verified body it cannot read is `Received::Malformed`, logged with its `svix-id`
(decision 11):

```rust
//! Delivery webhooks (spec §9): verified, recorded once in `worker.email_event`, and hard
//! bounces and complaints added to `worker.email_suppression`.

use std::sync::Arc;
use std::time::SystemTime;

use push_email::{DeliveryKind, WebhookError, WebhookVerifier};
use serde_json::Value;
use sqlx::PgPool;

/// What became of a webhook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    /// A new delivery event, recorded; `suppressed` addresses now get no more email.
    Recorded { suppressed: u64 },
    /// The same webhook again: nothing changed.
    Replay,
    /// A kind of event nothing acts on.
    Ignored,
    /// Signed by the provider, but not a delivery event we can read: logged and dropped.
    /// It is acknowledged all the same, since the provider would send it again for days.
    Malformed,
}

/// Why a webhook was not taken.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    /// Unsigned, forged or too old: never worth sending again.
    #[error(transparent)]
    Refused(#[from] WebhookError),
    /// The worker's database did not answer: the provider should send it again.
    #[error("the worker's database did not answer")]
    Database(#[from] sqlx::Error),
}

/// Takes the provider's delivery webhooks.
#[derive(Clone)]
pub struct Deliveries {
    pool: PgPool,
    verifier: Arc<dyn WebhookVerifier>,
}

impl std::fmt::Debug for Deliveries {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Deliveries").finish_non_exhaustive()
    }
}

impl Deliveries {
    pub fn new(pool: PgPool, verifier: Arc<dyn WebhookVerifier>) -> Self {
        Self { pool, verifier }
    }

    /// Verifies a webhook (headers through `header`, the raw `body`) and records its
    /// delivery event once: its id is the webhook's, the same on every delivery of it. A
    /// hard bounce or a complaint suppresses the recipients, the ones the event names and
    /// the one the email was sent to.
    pub async fn receive(
        &self,
        header: &(dyn Fn(&str) -> Option<String> + Sync),
        body: &[u8],
    ) -> Result<Received, DeliveryError> {
        let id = self.verifier.verify(header, body, SystemTime::now())?;
        let malformed = || {
            tracing::warn!(
                webhook_id = %id,
                "a signed webhook is not a delivery event we can read; dropped"
            );
            Ok(Received::Malformed)
        };
        let (event, raw) = match (
            self.verifier.parse(body),
            serde_json::from_slice::<Value>(body),
        ) {
            (Ok(None), _) => return Ok(Received::Ignored),
            (Ok(Some(event)), Ok(raw)) => (event, raw),
            _ => return malformed(),
        };
        let (kind, reason) = match event.kind {
            DeliveryKind::Delivered => (1_i16, None),
            DeliveryKind::Bounced { .. } => (2, Some(1_i16)),
            DeliveryKind::Complained => (3, Some(2)),
        };
        let mut tx = self.pool.begin().await?;
        let recorded = sqlx::query(
            "INSERT INTO worker.email_event \
             (event_id, provider_message_id, kind, occurred_at, raw) \
             VALUES ($1, $2, $3, coalesce($4, now()), $5) ON CONFLICT (event_id) DO NOTHING",
        )
        .bind(&id)
        .bind(&event.provider_message_id)
        .bind(kind)
        .bind(event.occurred_at)
        .bind(&raw)
        .execute(&mut *tx)
        .await?;
        if recorded.rows_affected() == 0 {
            return Ok(Received::Replay);
        }
        let mut suppressed = 0;
        if let (true, Some(reason)) = (event.suppresses(), reason) {
            suppressed += sqlx::query(
                "INSERT INTO worker.email_suppression (address, reason) \
                 SELECT address, $2 FROM UNNEST($1::text[]) AS address \
                 ON CONFLICT (address) DO NOTHING",
            )
            .bind(&event.recipients)
            .bind(reason)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            suppressed += sqlx::query(
                "INSERT INTO worker.email_suppression (address, reason) \
                 SELECT to_address, $2 FROM worker.email_message \
                 WHERE provider_message_id = $1 ON CONFLICT (address) DO NOTHING",
            )
            .bind(&event.provider_message_id)
            .bind(reason)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        tracing::info!(kind, suppressed, "delivery event recorded");
        Ok(Received::Recorded { suppressed })
    }
}
```

Create `crates/push-worker/src/http.rs` (only a refused signature is 401; everything a
retry could not fix is 204):

```rust
//! The worker's HTTP API: Resend's delivery webhooks and the DEV ONLY mailbox.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

use crate::deliveries::{Deliveries, DeliveryError};
use crate::email::Mailbox;

/// The only path the ingress routes to the worker.
pub const WEBHOOK_PATH: &str = "/v1/webhooks/resend";
/// Webhook bodies are small JSON documents.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// What the API serves: webhooks when a webhook secret is configured, the mailbox in dev.
#[derive(Debug, Clone, Default)]
pub struct Http {
    pub deliveries: Option<Deliveries>,
    /// DEV ONLY: `GET /dev/mailbox?to=…`.
    pub mailbox: Option<Mailbox>,
}

/// The router: `POST /v1/webhooks/resend` and, when enabled, `GET /dev/mailbox`.
pub fn router(http: Http) -> Router {
    let mut router = Router::new();
    if let Some(deliveries) = http.deliveries {
        router = router.route(
            WEBHOOK_PATH,
            post(webhook)
                .with_state(deliveries)
                .layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
        );
    }
    if let Some(mailbox) = http.mailbox {
        router = router.route("/dev/mailbox", get(dev_mailbox).with_state(mailbox));
    }
    router
}

/// 204 once recorded (or a replay, an event nothing acts on, or a signed body that is not
/// a delivery event, which is logged and dropped); 401 for a missing or wrong signature or
/// a stale timestamp; 503 when the database did not answer. Resend sends anything not
/// answered 2xx again, for days, so only what a retry could fix is not a 2xx.
async fn webhook(
    State(deliveries): State<Deliveries>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    match deliveries.receive(&header, &body).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(DeliveryError::Refused(error)) => {
            tracing::warn!(%error, "webhook refused");
            (StatusCode::UNAUTHORIZED, error.to_string()).into_response()
        }
        Err(error @ DeliveryError::Database(_)) => {
            tracing::warn!(%error, "webhook not recorded");
            (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()
        }
    }
}

#[derive(Deserialize)]
struct MailboxQuery {
    #[serde(default)]
    to: String,
}

/// DEV ONLY: the emails the log sender delivered to one address, newest first.
async fn dev_mailbox(
    State(mailbox): State<Mailbox>,
    Query(query): Query<MailboxQuery>,
) -> Response {
    let emails: Vec<_> = mailbox
        .to(&query.to)
        .into_iter()
        .map(|email| {
            json!({
                "template": email.template,
                "subject": email.content.subject,
                "text": email.content.text,
            })
        })
        .collect();
    Json(json!({"emails": emails})).into_response()
}
```

- [ ] **Step 6: `email.send` jobs go to the mailer**

Replace `crates/push-worker/src/jobs.rs` with:

```rust
//! What the worker does with each kind of job.

use crate::accounts::{AccountEvent, Accounts};
use crate::email::Mailer;
use crate::queue::{Job, Outcome};

/// Runs jobs by kind: auth-svc's account events go to message-svc, `email.send` to the
/// mailer.
#[derive(Debug, Clone)]
pub struct Jobs {
    accounts: Accounts,
    mailer: Mailer,
}

impl Jobs {
    pub fn new(accounts: Accounts, mailer: Mailer) -> Self {
        Self { accounts, mailer }
    }

    /// One attempt at `job`. A kind nobody handles, or a payload that does not fit its
    /// kind, is poison.
    pub async fn run(&self, job: &Job) -> Outcome {
        if job.kind == "email.send" {
            return self.mailer.send(job).await;
        }
        match AccountEvent::parse(&job.kind, &job.payload) {
            Some(Ok(event)) => self.accounts.apply(event).await,
            Some(Err(reason)) => Outcome::Poison(reason),
            None => Outcome::Poison(format!("no handler for jobs of kind {:?}", job.kind)),
        }
    }
}
```

In `crates/push-worker/src/lib.rs`:

Replace

```rust
pub mod accounts;
mod db;
pub mod jobs;
pub mod queue;
```

with

```rust
pub mod accounts;
mod db;
pub mod deliveries;
pub mod email;
pub mod http;
pub mod jobs;
pub mod queue;
```

`Jobs::new` now takes the mailer too; in `crates/push-worker/tests/relay.rs`:

Replace

```rust
use push_auth::outbox::Event;
use push_domain::{InMemoryStore, Messaging, RecordingNotifier, SystemClock};
use push_message_svc::{AccountsApi, AccountsToken, MessagingApi};
use push_proto::id_bytes;
```

with

```rust
use push_auth::outbox::Event;
use push_domain::{InMemoryStore, Messaging, RecordingNotifier, SystemClock};
use push_email::{LogSender, Templates};
use push_message_svc::{AccountsApi, AccountsToken, MessagingApi};
use push_proto::id_bytes;
```

Replace

```rust
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_worker::accounts::Accounts;
use push_worker::jobs::Jobs;
use push_worker::queue::{Job, Outcome, Queue};
```

with

```rust
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_worker::accounts::Accounts;
use push_worker::email::Mailer;
use push_worker::jobs::Jobs;
use push_worker::queue::{Job, Outcome, Queue};
```

Replace

```rust
    queue: Queue,
    relay: OutboxRelay,
    jobs: Jobs,
    message_svc: String,
```

with

```rust
    queue: Queue,
    relay: OutboxRelay,
    mailer: Mailer,
    jobs: Jobs,
    message_svc: String,
```

Replace

```rust
    push_worker::migrate_database(&worker_url).await.unwrap();

    let queue = Queue::new(
        PgPool::connect(&worker_url).await.unwrap(),
        Duration::from_secs(60),
    );
    let source = PgPool::connect(&format!(
        "postgres://push_worker:worker-secret@{server}/push_auth"
```

with

```rust
    push_worker::migrate_database(&worker_url).await.unwrap();

    let pool = PgPool::connect(&worker_url).await.unwrap();
    let queue = Queue::new(pool.clone(), Duration::from_secs(60));
    let mailer = Mailer::new(pool, Arc::new(LogSender), Templates::default());
    let source = PgPool::connect(&format!(
        "postgres://push_worker:worker-secret@{server}/push_auth"
```

Replace

```rust
        relay: OutboxRelay::new(source, queue.clone(), 100),
        queue,
        jobs: Jobs::new(Accounts::new(&message_svc, ACCOUNTS_TOKEN).unwrap()),
        messaging: MessagingClient::connect(message_svc.clone()).await.unwrap(),
        message_svc,
```

with

```rust
        relay: OutboxRelay::new(source, queue.clone(), 100),
        queue,
        jobs: Jobs::new(
            Accounts::new(&message_svc, ACCOUNTS_TOKEN).unwrap(),
            mailer.clone(),
        ),
        mailer,
        messaging: MessagingClient::connect(message_svc.clone()).await.unwrap(),
        message_svc,
```

Replace

```rust
        Accounts::new(&world.message_svc, wrong_token).unwrap(),
    ] {
        let outcome = Jobs::new(accounts)
            .run(&Job {
                id: Uuid::now_v7(),
```

with

```rust
        Accounts::new(&world.message_svc, wrong_token).unwrap(),
    ] {
        let outcome = Jobs::new(accounts, world.mailer.clone())
            .run(&Job {
                id: Uuid::now_v7(),
```

```powershell
cargo test -p push-worker -p push-email
```

Expected: `push-email` 19, and for `push-worker` 6 unit tests, `tests\email.rs` 5,
`tests\queue.rs` 6, `tests\relay.rs` 3 — all pass.

- [ ] **Step 7: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock crates/push-email crates/push-worker
git commit -m "feat(worker): email jobs, delivery webhooks and the dev mailbox"
```

Expected: 768 tests pass.

---

### Task 8: The cleanup jobs

**Files:**
- Modify: `crates/push-worker/src/jobs.rs`, `src/lib.rs`
- Create: `crates/push-worker/src/cleanup.rs`, `tests/cleanup.rs`

- [ ] **Step 1: The test, failing**

One Postgres holds both databases. auth-svc's gets an account unverified for 8 days, one
unverified since now, a verified one with a device, an expired and a live refresh token;
the worker's an old and a new email with their delivery events, an old buried job and a live
one. Each cleanup runs once as the worker's role and must leave exactly the young and the
live rows (decisions 7, 8 and 13). Create `crates/push-worker/tests/cleanup.rs`:

```rust
//! The cleanup jobs on a real PostgreSQL (Docker must be running): auth-svc's through its
//! functions, as the worker's role, and the worker's own records.

use std::time::Duration;

use push_worker::cleanup::{
    Cleanup, REFRESH_TOKENS, Retention, UNVERIFIED_ACCOUNTS, WORKER_RECORDS, periodic_jobs,
};
use push_worker::queue::{Outcome, Queue};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use uuid::Uuid;

const RETENTION: Retention = Retention {
    unverified_accounts: Duration::from_secs(7 * 24 * 3600),
    worker_records: Duration::from_secs(30 * 24 * 3600),
    interval: Duration::from_secs(3600),
};

#[tokio::test]
async fn the_cleanups_delete_what_has_expired_and_keep_the_rest() {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let server = format!("127.0.0.1:{port}");
    let admin = PgPool::connect(&format!("postgres://postgres:postgres@{server}/postgres"))
        .await
        .unwrap();
    for statement in [
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_auth",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement).execute(&admin).await.unwrap();
    }
    admin.close().await;
    let auth_url = format!("postgres://postgres:postgres@{server}/push_auth");
    push_auth::migrate_database(&auth_url).await.unwrap();
    push_auth::grant_worker_access(&auth_url, "push_worker")
        .await
        .unwrap();
    let worker_url = format!("postgres://push_worker:worker-secret@{server}/push_worker");
    push_worker::migrate_database(&worker_url).await.unwrap();
    let auth = PgPool::connect(&auth_url).await.unwrap();
    let worker = PgPool::connect(&worker_url).await.unwrap();

    // auth-svc: an account unverified for 8 days, one since now; a verified one with a
    // device, an expired refresh token and a live one.
    for (handle, age, verified) in [
        ("old", "8 days", false),
        ("new", "0 days", false),
        ("alice", "8 days", true),
    ] {
        sqlx::query(
            "INSERT INTO auth.user_account (user_id, email, handle, display_name, \
             password_hash, created_at, email_verified_at) VALUES ($1, $2, $3, 'X', 'h', \
             now() - $4::interval, CASE WHEN $5 THEN now() END)",
        )
        .bind(Uuid::now_v7())
        .bind(format!("{handle}@example.com"))
        .bind(handle)
        .bind(age)
        .bind(verified)
        .execute(&auth)
        .await
        .unwrap();
    }
    let device = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.device (device_id, user_id, name, platform, registration_id) \
         SELECT $1, user_id, 'Phone', 'ios', $1 FROM auth.user_account WHERE handle = 'alice'",
    )
    .bind(device)
    .execute(&auth)
    .await
    .unwrap();
    for (hash, expires) in [([1u8; 32], "-1 hour"), ([2u8; 32], "1 day")] {
        sqlx::query(
            "INSERT INTO auth.refresh_token (token_hash, device_id, expires_at) \
             VALUES ($1, $2, now() + $3::interval)",
        )
        .bind(hash.as_slice())
        .bind(device)
        .bind(expires)
        .execute(&auth)
        .await
        .unwrap();
    }
    // The worker: an old and a new email with their events, an old buried job, a live one.
    for (age, id) in [("40 days", Uuid::now_v7()), ("1 day", Uuid::now_v7())] {
        sqlx::query(
            "INSERT INTO worker.email_message (email_id, to_address, recipient_domain, \
             template, params, status, created_at) \
             VALUES ($1, 'a@example.com', 'example.com', 'verify_email', '{}', 2, \
             now() - $2::interval)",
        )
        .bind(id)
        .bind(age)
        .execute(&worker)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO worker.email_event (event_id, provider_message_id, kind, occurred_at, \
             raw, received_at) VALUES ($1, 're-1', 1, now(), '{}', now() - $2::interval)",
        )
        .bind(id.to_string())
        .bind(age)
        .execute(&worker)
        .await
        .unwrap();
    }
    for dead in ["now() - interval '40 days'", "NULL"] {
        sqlx::query(&format!(
            "INSERT INTO worker.job (job_id, kind, payload, dead_at) \
             VALUES ($1, 'email.send', '{{}}', {dead})"
        ))
        .bind(Uuid::now_v7())
        .execute(&worker)
        .await
        .unwrap();
    }

    let queue = Queue::new(worker.clone(), Duration::from_secs(60));
    assert_eq!(queue.enqueue(&periodic_jobs()).await.unwrap(), 3);
    assert_eq!(queue.enqueue(&periodic_jobs()).await.unwrap(), 0);
    let source = PgPool::connect(&format!(
        "postgres://push_worker:worker-secret@{server}/push_auth"
    ))
    .await
    .unwrap();
    let cleanup = Cleanup::new(source, worker.clone(), queue, RETENTION);
    for kind in [REFRESH_TOKENS, UNVERIFIED_ACCOUNTS, WORKER_RECORDS] {
        assert_eq!(
            cleanup.run(kind).await,
            Some(Outcome::RunAgain(RETENTION.interval)),
            "{kind}"
        );
    }
    assert_eq!(cleanup.run("email.send").await, None);

    let count = |pool: &PgPool, table: &str| {
        let query = format!("SELECT count(*) FROM {table}");
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(&query)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(count(&auth, "auth.refresh_token").await, 1);
    assert_eq!(count(&auth, "auth.user_account").await, 2);
    assert_eq!(count(&worker, "worker.email_message").await, 1);
    assert_eq!(count(&worker, "worker.email_event").await, 1);
    // The live job and the three cleanups.
    assert_eq!(count(&worker, "worker.job").await, 4);
}
```

```powershell
cargo test -p push-worker --test cleanup
```

Expected: a compile error — `push_worker::cleanup` does not exist.

- [ ] **Step 2: The cleanups**

Create `crates/push-worker/src/cleanup.rs` (a run starts no batch after 5 s, so it stays
inside its lease; decision 13):

```rust
//! The cleanup jobs: periodic jobs in the queue, so one replica runs each at a time.

use std::time::{Duration, Instant};

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::queue::{NewJob, Outcome, Queue};

/// Deletes expired refresh tokens in auth-svc's database.
pub const REFRESH_TOKENS: &str = "cleanup.refresh_tokens";
/// Deletes accounts that stayed unverified, in auth-svc's database.
pub const UNVERIFIED_ACCOUNTS: &str = "cleanup.unverified_accounts";
/// Deletes the worker's own old records: buried jobs, emails and delivery events.
pub const WORKER_RECORDS: &str = "cleanup.worker_records";

/// Rows deleted per statement.
const BATCH: i32 = 1000;
/// Batches per run; a run that leaves more behind comes back after [`AGAIN_SOON`].
const MAX_BATCHES: u32 = 50;
/// A run starts no batch after this long, and comes back after [`AGAIN_SOON`] too: with a
/// batch's statement timeout of 10 s, a run takes at most about 15 s, like any other job,
/// well inside the queue's lease and the pod's grace period.
const RUN_BUDGET: Duration = Duration::from_secs(5);
const AGAIN_SOON: Duration = Duration::from_secs(60);

/// How long things are kept, and how often the cleanups run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Accounts unverified for longer are deleted.
    pub unverified_accounts: Duration,
    /// Buried jobs, email records and delivery events older than this are deleted.
    pub worker_records: Duration,
    /// Each cleanup runs this often.
    pub interval: Duration,
}

/// The cleanup jobs, with fixed ids: adding them again on every start changes nothing.
pub fn periodic_jobs() -> Vec<NewJob> {
    [
        (0xc1ea_0001, REFRESH_TOKENS),
        (0xc1ea_0002, UNVERIFIED_ACCOUNTS),
        (0xc1ea_0003, WORKER_RECORDS),
    ]
    .into_iter()
    .map(|(id, kind)| NewJob {
        id: Uuid::from_u128(id),
        kind: kind.to_owned(),
        payload: json!({}),
    })
    .collect()
}

/// Runs the cleanups: auth-svc's through the two functions it lets the worker's role
/// call, the worker's own directly.
#[derive(Debug, Clone)]
pub struct Cleanup {
    auth: PgPool,
    queue: Queue,
    worker: PgPool,
    retention: Retention,
}

impl Cleanup {
    /// `auth` is the pool on auth-svc's database as the worker's role, `worker` the
    /// worker's own database.
    pub fn new(auth: PgPool, worker: PgPool, queue: Queue, retention: Retention) -> Self {
        Self {
            auth,
            queue,
            worker,
            retention,
        }
    }

    /// Runs the cleanup `kind`; `None` if `kind` is not a cleanup.
    pub async fn run(&self, kind: &str) -> Option<Outcome> {
        let deadline = Instant::now() + RUN_BUDGET;
        let result = match kind {
            REFRESH_TOKENS => {
                self.batches(deadline, || {
                    sqlx::query_scalar("SELECT auth.delete_expired_refresh_tokens($1)")
                        .bind(BATCH)
                        .fetch_one(&self.auth)
                })
                .await
            }
            UNVERIFIED_ACCOUNTS => {
                let older_than = self.retention.unverified_accounts.as_secs_f64();
                self.batches(deadline, || {
                    sqlx::query_scalar(
                        "SELECT auth.delete_unverified_accounts(make_interval(secs => $1), $2)",
                    )
                    .bind(older_than)
                    .bind(BATCH)
                    .fetch_one(&self.auth)
                })
                .await
            }
            WORKER_RECORDS => self.worker_records(deadline).await,
            _ => return None,
        };
        Some(match result {
            Ok((deleted, done)) => {
                if deleted > 0 {
                    tracing::info!(cleanup = kind, deleted, "cleaned up");
                }
                Outcome::RunAgain(if done {
                    self.retention.interval
                } else {
                    AGAIN_SOON
                })
            }
            Err(error) => {
                tracing::warn!(cleanup = kind, %error, "cleanup failed; it runs again later");
                Outcome::Failed {
                    reason: format!("{kind}: the database refused or did not answer"),
                    max_attempts: None,
                }
            }
        })
    }

    /// Calls `delete` until a batch comes back short, at most [`MAX_BATCHES`] times and
    /// not after `deadline`. Returns how many rows went, and whether nothing is left.
    async fn batches<F, Fut>(&self, deadline: Instant, delete: F) -> sqlx::Result<(i64, bool)>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = sqlx::Result<i32>>,
    {
        let mut deleted = 0;
        for _ in 0..MAX_BATCHES {
            if Instant::now() >= deadline {
                break;
            }
            let batch = delete().await?;
            deleted += i64::from(batch);
            if batch < BATCH {
                return Ok((deleted, true));
            }
        }
        Ok((deleted, false))
    }

    async fn worker_records(&self, deadline: Instant) -> sqlx::Result<(i64, bool)> {
        let older_than = self.retention.worker_records;
        let jobs = self
            .batches(deadline, || async {
                let deleted = self.queue.delete_dead(older_than, i64::from(BATCH)).await?;
                Ok(deleted as i32)
            })
            .await?;
        let mut total = jobs;
        for (table, column, key) in [
            ("email_message", "created_at", "email_id"),
            ("email_event", "received_at", "event_id"),
        ] {
            let statement = format!(
                "WITH gone AS (DELETE FROM worker.{table} WHERE {key} IN \
                 (SELECT {key} FROM worker.{table} \
                 WHERE {column} < now() - make_interval(secs => $1) LIMIT $2) RETURNING 1) \
                 SELECT count(*)::integer FROM gone"
            );
            let (deleted, done) = self
                .batches(deadline, || {
                    sqlx::query_scalar(&statement)
                        .bind(older_than.as_secs_f64())
                        .bind(BATCH)
                        .fetch_one(&self.worker)
                })
                .await?;
            total = (total.0 + deleted, total.1 && done);
        }
        Ok(total)
    }
}
```

Replace `crates/push-worker/src/jobs.rs` with (`with_cleanup` keeps the relay tests'
`Jobs::new(accounts, mailer)`):

```rust
//! What the worker does with each kind of job.

use crate::accounts::{AccountEvent, Accounts};
use crate::cleanup::Cleanup;
use crate::email::Mailer;
use crate::queue::{Job, Outcome};

/// Runs jobs by kind: auth-svc's account events go to message-svc, `email.send` to the
/// mailer, `cleanup.*` to the cleanups.
#[derive(Debug, Clone)]
pub struct Jobs {
    accounts: Accounts,
    mailer: Mailer,
    cleanup: Option<Cleanup>,
}

impl Jobs {
    pub fn new(accounts: Accounts, mailer: Mailer) -> Self {
        Self {
            accounts,
            mailer,
            cleanup: None,
        }
    }

    /// Runs the cleanup jobs too.
    pub fn with_cleanup(self, cleanup: Cleanup) -> Self {
        Self {
            cleanup: Some(cleanup),
            ..self
        }
    }

    /// One attempt at `job`. A kind nobody handles, or a payload that does not fit its
    /// kind, is poison.
    pub async fn run(&self, job: &Job) -> Outcome {
        if job.kind == "email.send" {
            return self.mailer.send(job).await;
        }
        if let Some(cleanup) = &self.cleanup {
            if let Some(outcome) = cleanup.run(&job.kind).await {
                return outcome;
            }
        }
        match AccountEvent::parse(&job.kind, &job.payload) {
            Some(Ok(event)) => self.accounts.apply(event).await,
            Some(Err(reason)) => Outcome::Poison(reason),
            None => Outcome::Poison(format!("no handler for jobs of kind {:?}", job.kind)),
        }
    }
}
```

In `crates/push-worker/src/lib.rs`:

Replace

```rust
pub mod accounts;
mod db;
pub mod deliveries;
```

with

```rust
pub mod accounts;
pub mod cleanup;
mod db;
pub mod deliveries;
```

```powershell
cargo test -p push-worker --test cleanup
```

Expected: `test result: ok. 1 passed`.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates/push-worker
git commit -m "feat(worker): cleanup jobs"
```

Expected: 769 tests pass.

---

### Task 9: Configuration, serving, health and graceful shutdown

**Files:**
- Modify: `crates/push-worker/Cargo.toml`, `src/lib.rs`
- Create: `crates/push-worker/src/config.rs`, `src/server.rs`, `src/main.rs`,
  `tests/server.rs`

- [ ] **Step 1: Dependencies**

`anyhow`, `tokio`'s runtime and signals, `tokio-util` and `tracing-subscriber` for the
binary. Replace `crates/push-worker/Cargo.toml` with:

```toml
[package]
name = "push-worker"
description = "The push-messenger worker: outbox relay, email, delivery webhooks and cleanup jobs."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
anyhow.workspace = true
async-trait.workspace = true
chrono.workspace = true
axum.workspace = true
push-email.workspace = true
push-proto.workspace = true
push-store.workspace = true
serde.workspace = true
serde_json.workspace = true
sqlx.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
tokio-util.workspace = true
tonic.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
uuid.workspace = true

[dev-dependencies]
http-body-util.workspace = true
push-auth.workspace = true
push-domain.workspace = true
push-message-svc.workspace = true
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["io-util"] }
tokio-stream.workspace = true
tower.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: The configuration, with its tests**

Create `crates/push-worker/src/config.rs` (every variable is in the table of its doc
comment; decisions 10–15, 17 and 18: `PUSH_ACCOUNTS_TOKEN` is required, and
`PUSH_MESSAGE_SVC_URL` must be `http://`):

```rust
//! Configuration from environment variables.

use std::net::SocketAddr;
use std::time::Duration;

use push_store::{StoreOptions, check_database_url};

/// Which sender delivers email.
#[derive(Clone, PartialEq, Eq)]
pub enum Sender {
    /// Logs each email instead of sending it (local development).
    Log,
    /// Resend's API.
    Resend { api_url: String, api_key: String },
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Log => formatter.write_str("Log"),
            Self::Resend { api_url, .. } => formatter
                .debug_struct("Resend")
                .field("api_url", api_url)
                .field("api_key", &"<redacted>")
                .finish(),
        }
    }
}

/// How the worker runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_DATABASE_URL` | Postgres URL of the `push_worker` database, as its owner | required |
/// | `PUSH_AUTH_DATABASE_URL` | Postgres URL of auth-svc's `push_auth` database, as the worker's role | required |
/// | `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc, `http://host:50051` (plain `http://` only: the worker is built without TLS) | required |
/// | `PUSH_ACCOUNTS_TOKEN` | the token message-svc requires for its `Accounts` API, 32 to 256 visible ASCII characters | required |
/// | `PUSH_EMAIL_SENDER` | `log` (logs instead of sending) or `resend` | `log` |
/// | `PUSH_EMAIL_FROM` | the From of every email, `Push Messenger <no-reply@push.kubegames.dk>` | required for `resend` |
/// | `PUSH_EMAIL_PRODUCT_NAME` | the name emails sign with; no control characters, since it goes into the subject | `Push Messenger` |
/// | `PUSH_RESEND_API_KEY` | Resend API key | required for `resend` |
/// | `PUSH_RESEND_API_URL` | Resend's API (`http` only for local hosts, for tests) | `https://api.resend.com` |
/// | `PUSH_RESEND_WEBHOOK_SECRET` | the webhook's signing secret, `whsec_…`; without it `/v1/webhooks/resend` is not served | none |
/// | `PUSH_WORKER_POLL_MS` | how often an idle worker looks for work, in milliseconds | `1000` |
/// | `PUSH_UNVERIFIED_ACCOUNT_DAYS` | accounts unverified for this many days are deleted | `7` |
/// | `PUSH_WORKER_RETENTION_DAYS` | buried jobs, email records and delivery events are kept this many days | `30` |
/// | `PUSH_HTTP_ADDR` | address of the webhook endpoint (and the DEV ONLY mailbox) | `0.0.0.0:8080` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | size of each of the two pools, at least 1 | `5` |
/// | `PUSH_DEV_MAILBOX` | `1` serves the DEV ONLY `GET /dev/mailbox` (log sender only) | off |
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub database_url: String,
    pub auth_database_url: String,
    pub message_svc_url: String,
    pub accounts_token: String,
    pub sender: Sender,
    pub from: String,
    pub product_name: String,
    pub webhook_secret: Option<String>,
    pub poll: Duration,
    pub unverified_after: Duration,
    pub retention: Duration,
    pub http_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub store: StoreOptions,
    pub dev_mailbox: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("database_url", &"<redacted>")
            .field("auth_database_url", &"<redacted>")
            .field("message_svc_url", &self.message_svc_url)
            .field("accounts_token", &"<redacted>")
            .field("sender", &self.sender)
            .field("from", &self.from)
            .field("product_name", &self.product_name)
            .field(
                "webhook_secret",
                &self.webhook_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("poll", &self.poll)
            .field("unverified_after", &self.unverified_after)
            .field("retention", &self.retention)
            .field("http_addr", &self.http_addr)
            .field("health_addr", &self.health_addr)
            .field("store", &self.store)
            .field("dev_mailbox", &self.dev_mailbox)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} must be set")]
    Missing(&'static str),
    #[error("{name}={value:?} is not valid: {reason}")]
    Invalid {
        name: &'static str,
        value: String,
        reason: String,
    },
}

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

impl Config {
    /// Reads the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads variables through `lookup`, which returns a variable's value if it is set.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let set = |name: &str| lookup(name).filter(|value| !value.is_empty());
        let required = |name: &'static str| set(name).ok_or(ConfigError::Missing(name));
        let store = StoreOptions {
            max_connections: parsed(&lookup, "PUSH_DB_MAX_CONNECTIONS", 5)?,
            ..StoreOptions::default()
        };
        store.validate().map_err(|reason| ConfigError::Invalid {
            name: "PUSH_DB_MAX_CONNECTIONS",
            value: store.max_connections.to_string(),
            reason,
        })?;
        let database_url = required("PUSH_DATABASE_URL")?;
        check_database_url(&database_url)
            .map_err(|reason| redacted("PUSH_DATABASE_URL", reason))?;
        let auth_database_url = required("PUSH_AUTH_DATABASE_URL")?;
        check_database_url(&auth_database_url)
            .map_err(|reason| redacted("PUSH_AUTH_DATABASE_URL", reason))?;
        let message_svc_url = required("PUSH_MESSAGE_SVC_URL")?;
        if !message_svc_url.starts_with("http://") {
            return Err(ConfigError::Invalid {
                name: "PUSH_MESSAGE_SVC_URL",
                value: message_svc_url,
                reason: "must start with http:// (the worker speaks plain gRPC to message-svc \
                         inside the cluster and is built without TLS)"
                    .to_owned(),
            });
        }
        let accounts_token = required("PUSH_ACCOUNTS_TOKEN")?;
        if !((32..=256).contains(&accounts_token.len())
            && accounts_token.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            return Err(redacted(
                "PUSH_ACCOUNTS_TOKEN",
                "use 32 to 256 visible ASCII characters".to_owned(),
            ));
        }
        let sender = match set("PUSH_EMAIL_SENDER").as_deref() {
            None | Some("log") => Sender::Log,
            Some("resend") => {
                let api_url = set("PUSH_RESEND_API_URL")
                    .unwrap_or_else(|| push_email::RESEND_API_URL.to_owned());
                if !(api_url.starts_with("https://") || is_local_http(&api_url)) {
                    return Err(ConfigError::Invalid {
                        name: "PUSH_RESEND_API_URL",
                        value: api_url,
                        reason:
                            "must start with https:// (http:// only for 127.0.0.1 or localhost)"
                                .to_owned(),
                    });
                }
                Sender::Resend {
                    api_url,
                    api_key: required("PUSH_RESEND_API_KEY")?,
                }
            }
            Some(other) => {
                return Err(ConfigError::Invalid {
                    name: "PUSH_EMAIL_SENDER",
                    value: other.to_owned(),
                    reason: "use log or resend".to_owned(),
                });
            }
        };
        let from = match sender {
            Sender::Log => set("PUSH_EMAIL_FROM").unwrap_or_default(),
            Sender::Resend { .. } => required("PUSH_EMAIL_FROM")?,
        };
        if from.chars().any(char::is_control) {
            return Err(ConfigError::Invalid {
                name: "PUSH_EMAIL_FROM",
                value: from,
                reason: "must not contain control characters (it goes into an email header)"
                    .to_owned(),
            });
        }
        let product_name =
            set("PUSH_EMAIL_PRODUCT_NAME").unwrap_or_else(|| "Push Messenger".to_owned());
        if product_name.chars().any(char::is_control) {
            return Err(ConfigError::Invalid {
                name: "PUSH_EMAIL_PRODUCT_NAME",
                value: product_name,
                reason: "must not contain control characters (it goes into the email subject)"
                    .to_owned(),
            });
        }
        let webhook_secret = set("PUSH_RESEND_WEBHOOK_SECRET");
        if let Some(secret) = &webhook_secret {
            push_email::ResendWebhooks::new(secret)
                .map_err(|reason| redacted("PUSH_RESEND_WEBHOOK_SECRET", reason))?;
        }
        let poll_ms: u64 = parsed(&lookup, "PUSH_WORKER_POLL_MS", 1000)?;
        let unverified_days: u64 = parsed(&lookup, "PUSH_UNVERIFIED_ACCOUNT_DAYS", 7)?;
        let retention_days: u64 = parsed(&lookup, "PUSH_WORKER_RETENTION_DAYS", 30)?;
        for (name, value) in [
            ("PUSH_WORKER_POLL_MS", poll_ms),
            ("PUSH_UNVERIFIED_ACCOUNT_DAYS", unverified_days),
            ("PUSH_WORKER_RETENTION_DAYS", retention_days),
        ] {
            if value == 0 {
                return Err(ConfigError::Invalid {
                    name,
                    value: "0".to_owned(),
                    reason: "must be at least 1".to_owned(),
                });
            }
        }
        let dev_mailbox = dev_flag(&lookup, "PUSH_DEV_MAILBOX")?;
        if dev_mailbox && sender != Sender::Log {
            return Err(ConfigError::Invalid {
                name: "PUSH_DEV_MAILBOX",
                value: "1".to_owned(),
                reason: "the DEV ONLY mailbox shows every email it sent, links included; it \
                         is only allowed with PUSH_EMAIL_SENDER=log"
                    .to_owned(),
            });
        }
        Ok(Self {
            database_url,
            auth_database_url,
            message_svc_url,
            accounts_token,
            sender,
            from,
            product_name,
            webhook_secret,
            poll: Duration::from_millis(poll_ms),
            unverified_after: DAY.saturating_mul(unverified_days.min(36_500) as u32),
            retention: DAY.saturating_mul(retention_days.min(36_500) as u32),
            http_addr: parsed(&lookup, "PUSH_HTTP_ADDR", ([0, 0, 0, 0], 8080).into())?,
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            store,
            dev_mailbox,
        })
    }
}

/// `http://127.0.0.1…` or `http://localhost…`: a stand-in for Resend in tests.
fn is_local_http(url: &str) -> bool {
    [
        "http://127.0.0.1:",
        "http://127.0.0.1/",
        "http://localhost:",
        "http://localhost/",
    ]
    .iter()
    .any(|prefix| url.starts_with(prefix))
}

/// An invalid secret variable; the value is never shown.
fn redacted(name: &'static str, reason: String) -> ConfigError {
    ConfigError::Invalid {
        name,
        value: "<redacted>".to_owned(),
        reason,
    }
}

/// A variable parsed as `T`, or `default` when it is unset or empty.
fn parsed<T>(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: T,
) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match lookup(name).filter(|value| !value.is_empty()) {
        None => Ok(default),
        Some(value) => value.parse().map_err(|error: T::Err| ConfigError::Invalid {
            name,
            reason: error.to_string(),
            value,
        }),
    }
}

/// A DEV ONLY switch: on only for exactly `1`; off when unset, empty or `0`. Anything
/// else is refused, so a typo can never switch it on or leave it in doubt.
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
    use std::collections::HashMap;

    use super::*;

    fn config(variables: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let variables: HashMap<String, String> = variables
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        Config::from_lookup(|name| variables.get(name).cloned())
    }

    const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

    const TOKEN: &str = "accounts-token-of-the-config-tests";

    const REQUIRED: [(&str, &str); 4] = [
        (
            "PUSH_DATABASE_URL",
            "postgres://push_worker:dbsecret@db/push_worker",
        ),
        (
            "PUSH_AUTH_DATABASE_URL",
            "postgres://push_worker:dbsecret@db/push_auth",
        ),
        ("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051"),
        ("PUSH_ACCOUNTS_TOKEN", TOKEN),
    ];

    fn with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut variables = REQUIRED.to_vec();
        variables.retain(|(name, _)| !extra.iter().any(|(key, _)| key == name));
        variables.extend_from_slice(extra);
        variables
    }

    #[test]
    fn defaults_apply_when_only_the_required_variables_are_set() {
        let config = config(&REQUIRED).unwrap();
        assert_eq!(config.sender, Sender::Log);
        assert_eq!(config.accounts_token, TOKEN);
        assert_eq!(config.product_name, "Push Messenger");
        assert_eq!(config.webhook_secret, None);
        assert_eq!(config.poll, Duration::from_secs(1));
        assert_eq!(config.unverified_after, DAY * 7);
        assert_eq!(config.retention, DAY * 30);
        assert_eq!(config.http_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.store.max_connections, 5);
        assert!(!config.dev_mailbox);
    }

    #[test]
    fn every_required_variable_is_required() {
        for (index, (name, _)) in REQUIRED.iter().enumerate() {
            let mut variables = REQUIRED.to_vec();
            variables.remove(index);
            assert_eq!(config(&variables), Err(ConfigError::Missing(name)));
        }
    }

    #[test]
    fn message_svc_is_reached_over_plain_http_with_a_header_safe_token() {
        for url in [
            "https://message-svc:50051",
            "message-svc:50051",
            "grpc://m:1",
        ] {
            assert!(
                matches!(
                    config(&with(&[("PUSH_MESSAGE_SVC_URL", url)])),
                    Err(ConfigError::Invalid {
                        name: "PUSH_MESSAGE_SVC_URL",
                        ..
                    })
                ),
                "{url}"
            );
        }
        for token in ["short", "a token with spaces, long enough otherwise"] {
            let message = config(&with(&[("PUSH_ACCOUNTS_TOKEN", token)]))
                .unwrap_err()
                .to_string();
            assert!(message.contains("PUSH_ACCOUNTS_TOKEN"), "{message}");
            assert!(!message.contains(token), "{message}");
        }
    }

    #[test]
    fn resend_needs_a_key_and_a_from_address() {
        let resend = config(&with(&[
            ("PUSH_EMAIL_SENDER", "resend"),
            ("PUSH_RESEND_API_KEY", "re_key"),
            ("PUSH_EMAIL_FROM", "Push <no-reply@example.com>"),
            ("PUSH_RESEND_WEBHOOK_SECRET", SECRET),
            ("PUSH_EMAIL_PRODUCT_NAME", "Acme Chat"),
        ]))
        .unwrap();
        assert_eq!(
            resend.sender,
            Sender::Resend {
                api_url: "https://api.resend.com".to_owned(),
                api_key: "re_key".to_owned(),
            }
        );
        assert_eq!(resend.from, "Push <no-reply@example.com>");
        assert_eq!(resend.product_name, "Acme Chat");
        assert_eq!(
            config(&with(&[
                ("PUSH_EMAIL_SENDER", "resend"),
                ("PUSH_EMAIL_FROM", "a@example.com")
            ])),
            Err(ConfigError::Missing("PUSH_RESEND_API_KEY"))
        );
        assert_eq!(
            config(&with(&[
                ("PUSH_EMAIL_SENDER", "resend"),
                ("PUSH_RESEND_API_KEY", "re_key")
            ])),
            Err(ConfigError::Missing("PUSH_EMAIL_FROM"))
        );
        for (name, value) in [
            ("PUSH_EMAIL_SENDER", "smtp"),
            ("PUSH_RESEND_API_URL", "http://api.example.com"),
        ] {
            let result = config(&with(&[
                ("PUSH_EMAIL_SENDER", "resend"),
                ("PUSH_RESEND_API_KEY", "re_key"),
                ("PUSH_EMAIL_FROM", "a@example.com"),
                (name, value),
            ]));
            assert!(
                matches!(result, Err(ConfigError::Invalid { name: n, .. }) if n == name),
                "{name}={value}"
            );
        }
        let local = config(&with(&[
            ("PUSH_EMAIL_SENDER", "resend"),
            ("PUSH_RESEND_API_KEY", "re_key"),
            ("PUSH_EMAIL_FROM", "a@example.com"),
            ("PUSH_RESEND_API_URL", "http://127.0.0.1:9000"),
        ]));
        assert!(local.is_ok(), "{local:?}");
    }

    #[test]
    fn the_product_name_has_no_control_characters() {
        for name in [
            "Acme\r\nBcc: victim@example.com",
            "Acme\nChat",
            "Acme\rChat",
            "Acme\tChat",
            "Acme\u{0}Chat",
            "Acme\u{7f}Chat",
        ] {
            let result = config(&with(&[("PUSH_EMAIL_PRODUCT_NAME", name)]));
            assert!(
                matches!(
                    result,
                    Err(ConfigError::Invalid {
                        name: "PUSH_EMAIL_PRODUCT_NAME",
                        ..
                    })
                ),
                "{name:?}"
            );
        }
        let accented = config(&with(&[("PUSH_EMAIL_PRODUCT_NAME", "Æble Chat")])).unwrap();
        assert_eq!(accented.product_name, "Æble Chat");
        assert!(matches!(
            config(&with(&[(
                "PUSH_EMAIL_FROM",
                "Push <no-reply@push.test>\r\nBcc: victim@example.com"
            )])),
            Err(ConfigError::Invalid {
                name: "PUSH_EMAIL_FROM",
                ..
            })
        ));
    }

    #[test]
    fn the_dev_mailbox_is_strict_and_only_for_the_log_sender() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            let config = config(&with(&[("PUSH_DEV_MAILBOX", value)])).unwrap();
            assert_eq!(config.dev_mailbox, expected, "{value:?}");
        }
        for value in ["true", "yes", " 1"] {
            assert!(config(&with(&[("PUSH_DEV_MAILBOX", value)])).is_err());
        }
        let with_resend = config(&with(&[
            ("PUSH_DEV_MAILBOX", "1"),
            ("PUSH_EMAIL_SENDER", "resend"),
            ("PUSH_RESEND_API_KEY", "re_key"),
            ("PUSH_EMAIL_FROM", "a@example.com"),
        ]));
        assert!(matches!(
            with_resend,
            Err(ConfigError::Invalid {
                name: "PUSH_DEV_MAILBOX",
                ..
            })
        ));
    }

    #[test]
    fn periods_must_be_at_least_one() {
        for name in [
            "PUSH_WORKER_POLL_MS",
            "PUSH_UNVERIFIED_ACCOUNT_DAYS",
            "PUSH_WORKER_RETENTION_DAYS",
        ] {
            assert!(config(&with(&[(name, "0")])).is_err(), "{name}");
        }
        let config = config(&with(&[
            ("PUSH_WORKER_POLL_MS", "250"),
            ("PUSH_UNVERIFIED_ACCOUNT_DAYS", "2"),
            ("PUSH_WORKER_RETENTION_DAYS", "90"),
        ]))
        .unwrap();
        assert_eq!(config.poll, Duration::from_millis(250));
        assert_eq!(config.unverified_after, DAY * 2);
        assert_eq!(config.retention, DAY * 90);
    }

    #[test]
    fn secrets_never_show_in_errors_or_debug_output() {
        let message = config(&with(&[(
            "PUSH_AUTH_DATABASE_URL",
            "mysql://push:hunter2@db/x",
        )]))
        .unwrap_err()
        .to_string();
        assert!(message.contains("PUSH_AUTH_DATABASE_URL") && !message.contains("hunter2"));
        let message = config(&with(&[("PUSH_RESEND_WEBHOOK_SECRET", "whsec_hunter2!")]))
            .unwrap_err()
            .to_string();
        assert!(message.contains("PUSH_RESEND_WEBHOOK_SECRET") && !message.contains("hunter2"));
        let shown = format!(
            "{:?}",
            config(&with(&[
                ("PUSH_EMAIL_SENDER", "resend"),
                ("PUSH_RESEND_API_KEY", "re_hunter2"),
                ("PUSH_EMAIL_FROM", "a@example.com"),
                ("PUSH_RESEND_WEBHOOK_SECRET", SECRET),
            ]))
            .unwrap()
        );
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("MfKQ"), "{shown}");
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("http://message-svc:50051"), "{shown}");
    }
}
```

- [ ] **Step 3: The worker as it runs: its tests, failing**

The first test starts Postgres (both databases, auth-svc's migrated and granted),
message-svc in the process, and the worker with the log sender, the dev mailbox and a
webhook secret; it writes `user.verified`, `device.registered` and `email.send` to the
outbox and waits until message-svc accepts the device and the mailbox shows the email,
posts a signed complaint (204) and an unsigned one (401), waits for the cleanups to be
rescheduled an hour ahead, and stops within 5 s. The second shuts down while both databases
are unreachable. Create `crates/push-worker/tests/server.rs`:

```rust
//! The worker as it runs (Docker must be running): auth-svc's and its own databases in
//! one Postgres, message-svc in this process; readiness, the relay, email through the log
//! sender into the DEV ONLY mailbox, a webhook, the cleanup jobs, and graceful shutdown.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use push_auth::outbox::{Email, Event};
use push_domain::{InMemoryStore, Messaging, RecordingNotifier, SystemClock};
use push_email::ResendWebhooks;
use push_message_svc::{AccountsApi, AccountsToken, MessagingApi};
use push_proto::id_bytes;
use push_proto::internal::v1::IdentifyRequest;
use push_proto::internal::v1::accounts_server::AccountsServer;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_store::StoreOptions;
use push_worker::{Config, Sender, serve};
use serde_json::{Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
/// The shared token message-svc requires for its `Accounts` API.
const ACCOUNTS_TOKEN: &str = "accounts-token-for-the-server-tests";

fn config(database_url: String, auth_database_url: String, message_svc_url: String) -> Config {
    Config {
        database_url,
        auth_database_url,
        message_svc_url,
        accounts_token: ACCOUNTS_TOKEN.to_owned(),
        sender: Sender::Log,
        from: String::new(),
        product_name: "Push Messenger".to_owned(),
        webhook_secret: Some(SECRET.to_owned()),
        poll: Duration::from_millis(100),
        unverified_after: Duration::from_secs(7 * 24 * 3600),
        retention: Duration::from_secs(30 * 24 * 3600),
        http_addr: "127.0.0.1:0".parse().unwrap(),
        health_addr: "127.0.0.1:0".parse().unwrap(),
        store: StoreOptions::default(),
        dev_mailbox: true,
    }
}

/// message-svc's `Messaging` and `Accounts` APIs on an in-memory store; its URL.
async fn message_svc() -> String {
    let messaging = Arc::new(Messaging::new(
        InMemoryStore::default(),
        SystemClock,
        Arc::new(RecordingNotifier::default()),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(MessagingServer::new(MessagingApi::new(Arc::clone(
                &messaging,
            ))))
            .add_service(AccountsServer::with_interceptor(
                AccountsApi::new(messaging),
                AccountsToken::new(Some(ACCOUNTS_TOKEN.to_owned())),
            ))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    url
}

/// A minimal HTTP/1.1 request: the status code and the body.
async fn request(addr: SocketAddr, head: &str, body: &[u8]) -> (u16, String) {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return (0, String::new());
    };
    let request = format!(
        "{head}\r\nHost: test\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    (status, body)
}

/// Polls `check` every 100 ms until it holds; fails after 15 s.
async fn eventually<F, Fut>(what: &str, check: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(15);
    while !check().await {
        assert!(Instant::now() < deadline, "{what} within 15 s");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn the_worker_relays_sends_takes_webhooks_and_stops_cleanly() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let server = format!("127.0.0.1:{port}");
    let admin = PgPool::connect(&format!("postgres://postgres:postgres@{server}/postgres"))
        .await
        .unwrap();
    for statement in [
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_auth",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement).execute(&admin).await.unwrap();
    }
    admin.close().await;
    // auth-svc migrates and grants the worker's role its access at start-up.
    let auth_url = format!("postgres://postgres:postgres@{server}/push_auth");
    push_auth::migrate_database(&auth_url).await.unwrap();
    push_auth::grant_worker_access(&auth_url, "push_worker")
        .await
        .unwrap();
    let auth = PgPool::connect(&auth_url).await.unwrap();
    let message_svc_url = message_svc().await;

    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (http_addr, health_addr) = (http.local_addr().unwrap(), health.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    let worker = tokio::spawn(serve(
        config(
            format!("postgres://push_worker:worker-secret@{server}/push_worker"),
            format!("postgres://push_worker:worker-secret@{server}/push_auth"),
            message_svc_url.clone(),
        ),
        http,
        health,
        shutdown.clone(),
    ));
    assert_eq!(
        request(health_addr, "GET /healthz HTTP/1.1", b"").await.0,
        200
    );
    eventually("the worker is ready", || async {
        request(health_addr, "GET /readyz HTTP/1.1", b"").await.0 == 200
    })
    .await;

    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
    for event in [
        Event::UserVerified { user_id: user },
        Event::DeviceRegistered {
            user_id: user,
            device_id: device,
        },
        Event::EmailSend {
            user_id: user,
            to: "alice@example.com".to_owned(),
            email: Email::VerifyEmail {
                link: "http://push.test/v1/auth/verify?token=t".to_owned(),
                handle: "alice".to_owned(),
            },
        },
    ] {
        sqlx::query("INSERT INTO auth.outbox (event_id, kind, payload) VALUES ($1, $2, $3)")
            .bind(Uuid::now_v7())
            .bind(event.kind())
            .bind(event.payload())
            .execute(&auth)
            .await
            .unwrap();
    }
    let messaging = MessagingClient::connect(message_svc_url).await.unwrap();
    eventually("message-svc knows the device", || {
        let mut messaging = messaging.clone();
        async move {
            messaging
                .identify(IdentifyRequest {
                    user_id: id_bytes(user),
                    device_id: id_bytes(device),
                })
                .await
                .is_ok()
        }
    })
    .await;
    eventually("the log sender delivered the email", || async {
        let (status, body) = request(
            http_addr,
            "GET /dev/mailbox?to=alice@example.com HTTP/1.1",
            b"",
        )
        .await;
        status == 200
            && serde_json::from_str::<Value>(&body).unwrap()["emails"][0]["template"]
                == "verify_email"
    })
    .await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.outbox")
        .fetch_one(&auth)
        .await
        .unwrap();
    assert_eq!(left, 0);

    let body = json!({
        "type": "email.complained",
        "created_at": "2026-10-13T10:00:00.000Z",
        "data": {"email_id": "re-1", "to": ["carol@example.com"]},
    })
    .to_string();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let signature = ResendWebhooks::new(SECRET)
        .unwrap()
        .sign("msg_1", timestamp, body.as_bytes());
    let head = format!(
        "POST /v1/webhooks/resend HTTP/1.1\r\nContent-Type: application/json\r\n\
         svix-id: msg_1\r\nsvix-timestamp: {timestamp}\r\nsvix-signature: {signature}"
    );
    assert_eq!(request(http_addr, &head, body.as_bytes()).await.0, 204);
    let unsigned = "POST /v1/webhooks/resend HTTP/1.1\r\nContent-Type: application/json";
    assert_eq!(request(http_addr, unsigned, body.as_bytes()).await.0, 401);

    let worker_db = PgPool::connect(&format!(
        "postgres://push_worker:worker-secret@{server}/push_worker"
    ))
    .await
    .unwrap();
    let suppressed: i64 = sqlx::query_scalar("SELECT count(*) FROM worker.email_suppression")
        .fetch_one(&worker_db)
        .await
        .unwrap();
    assert_eq!(suppressed, 1);
    eventually("the cleanups ran and wait for their next turn", || async {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM worker.job WHERE kind LIKE 'cleanup.%' \
             AND attempts = 0 AND dead_at IS NULL AND run_after > now() + interval '30 minutes'",
        )
        .fetch_one(&worker_db)
        .await
        .unwrap();
        waiting == 3
    })
    .await;

    let started = Instant::now();
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("the worker stops within 10 s")
        .unwrap()
        .expect("a clean stop");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn shutdown_is_honoured_while_the_databases_are_unreachable() {
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health_addr = health.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let worker = tokio::spawn(serve(
        config(
            "postgres://push_worker:x@127.0.0.1:1/push_worker".to_owned(),
            "postgres://push_worker:x@127.0.0.1:1/push_auth".to_owned(),
            "http://127.0.0.1:1".to_owned(),
        ),
        http,
        health,
        shutdown.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        request(health_addr, "GET /readyz HTTP/1.1", b"").await.0,
        503
    );

    let started = Instant::now();
    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("serve did not stop")
        .unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shutdown took {:?}",
        started.elapsed()
    );
}
```

```powershell
cargo test -p push-worker --test server
```

Expected: compile errors — `push_worker::Config`, `Sender` and `serve` do not exist.

- [ ] **Step 4: Serving**

Create `crates/push-worker/src/server.rs` (`JOB_BATCH` and its arithmetic, decision 2; at
shutdown `run_jobs` gives back the rest of its batch; a dropped late outcome is logged):

```rust
//! Running the worker: the relay and job loops, the HTTP API, the health endpoints and
//! graceful shutdown.

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use push_email::{EmailSender, LogSender, ResendSender, ResendWebhooks, Templates};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::accounts::Accounts;
use crate::cleanup::{Cleanup, Retention, periodic_jobs};
use crate::config::{Config, Sender};
use crate::db::migrate_database;
use crate::deliveries::Deliveries;
use crate::email::{Mailbox, MailboxSender, Mailer};
use crate::http::{Http, router};
use crate::jobs::Jobs;
use crate::queue::{Job, Outcome, Queue};
use crate::relay::OutboxRelay;

/// How long to wait before trying an unreachable dependency again.
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// A claimed job is someone else's for this long.
const LEASE: Duration = Duration::from_secs(120);
/// Jobs claimed at once. They run one after another, so the last one starts only after
/// the others ended, and the whole batch must fit in the lease, or another worker claims
/// the jobs not started yet and they run twice. An attempt takes at most about 15 s
/// (message-svc: a 5 s connect and a 10 s call timeout; Resend: a 10 s request timeout,
/// plus statements of a few milliseconds; a cleanup run starts no batch after 5 s, and a
/// batch's statement times out after 10 s), so 4 jobs take at most about 60 s of the
/// 120 s lease. Even then a late outcome is never recorded over a newer claim
/// (`Queue::finish`).
const JOB_BATCH: i64 = 4;
/// Outbox events moved at once.
const OUTBOX_BATCH: i64 = 50;
/// The cleanups run this often.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Runs the worker until `shutdown` is cancelled, then finishes the job in progress, gives
/// back the jobs it claimed but did not start (due again at once, for any replica), and
/// closes its pools.
///
/// The health endpoints answer at once: `/healthz` always, `/readyz` once both databases
/// answer (its own migrated, auth-svc's with the outbox readable), and never after
/// shutdown began. Both databases are retried until they answer; message-svc is not
/// waited for: its events wait in the queue.
pub async fn serve(
    config: Config,
    http: TcpListener,
    health: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let ready = Health {
        pool: Arc::new(OnceLock::new()),
        shutdown: shutdown.clone(),
    };
    let health_server = tokio::spawn(
        axum::serve(health, health_router(ready.clone()))
            .with_graceful_shutdown(shutdown.clone().cancelled_owned())
            .into_future(),
    );
    if config.dev_mailbox {
        tracing::warn!(
            "DEV ONLY: PUSH_DEV_MAILBOX=1 serves GET /dev/mailbox, which shows anyone every \
             email the log sender delivered, sign-in links included. Never enable it outside \
             a local cluster."
        );
    }
    let accounts = Accounts::new(&config.message_svc_url, &config.accounts_token)
        .map_err(anyhow::Error::msg)?;
    let mailbox = config.dev_mailbox.then(Mailbox::default);
    let sender: Arc<dyn EmailSender> = match (&config.sender, &mailbox) {
        (Sender::Log, Some(mailbox)) => Arc::new(MailboxSender::new(LogSender, mailbox.clone())),
        (Sender::Log, None) => Arc::new(LogSender),
        (Sender::Resend { api_url, api_key }, _) => Arc::new(
            ResendSender::new(api_url, api_key.clone(), config.from.clone())
                .map_err(anyhow::Error::msg)?,
        ),
    };
    let webhooks = match &config.webhook_secret {
        Some(secret) => Some(ResendWebhooks::new(secret).map_err(anyhow::Error::msg)?),
        None => None,
    };

    let Some(pool) = retry("postgres", &shutdown, || async {
        // Migrations run on a dedicated connection, not on the pool, whose statement and
        // lock timeouts are sized for jobs.
        migrate_database(&config.database_url).await?;
        let pool = push_store::connect_pool(&config.database_url, &config.store).await?;
        anyhow::Ok(pool)
    })
    .await
    else {
        return finish(health_server).await;
    };
    let Some(auth) = retry("auth-svc's database", &shutdown, || async {
        let auth = push_store::connect_pool(&config.auth_database_url, &config.store).await?;
        // Until auth-svc has migrated and granted the worker's role its access, this fails.
        if let Err(error) = sqlx::query("SELECT 1 FROM auth.outbox LIMIT 0")
            .execute(&auth)
            .await
        {
            auth.close().await;
            return Err(anyhow::Error::from(error));
        }
        anyhow::Ok(auth)
    })
    .await
    else {
        pool.close().await;
        return finish(health_server).await;
    };

    let queue = Queue::new(pool.clone(), LEASE);
    queue.enqueue(&periodic_jobs()).await?;
    let mailer = Mailer::new(
        pool.clone(),
        sender,
        Templates::new(config.product_name.clone()),
    );
    let cleanup = Cleanup::new(
        auth.clone(),
        pool.clone(),
        queue.clone(),
        Retention {
            unverified_accounts: config.unverified_after,
            worker_records: config.retention,
            interval: CLEANUP_INTERVAL,
        },
    );
    let jobs = Jobs::new(accounts, mailer).with_cleanup(cleanup);
    let relay = OutboxRelay::new(auth.clone(), queue.clone(), OUTBOX_BATCH);
    let deliveries = webhooks.map(|webhooks| Deliveries::new(pool.clone(), Arc::new(webhooks)));
    let _ = ready.pool.set(pool.clone());

    let moved = Arc::new(Notify::new());
    let relay_loop = tokio::spawn(run_relay(
        relay,
        Arc::clone(&moved),
        config.poll,
        shutdown.clone(),
    ));
    let job_loop = tokio::spawn(run_jobs(queue, jobs, moved, config.poll, shutdown.clone()));
    tracing::info!(addr = %config.http_addr, "serving HTTP");
    let result = axum::serve(
        http,
        router(Http {
            deliveries,
            mailbox,
        }),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned())
    .await;
    relay_loop.await?;
    job_loop.await?;
    auth.close().await;
    pool.close().await;
    tracing::info!("stopped");
    result?;
    finish(health_server).await
}

/// Moves auth-svc's outbox into the queue: batch after batch while there is something,
/// then every `poll`. Rings `moved` when it moved events, so the job loop starts at once.
async fn run_relay(
    relay: OutboxRelay,
    moved: Arc<Notify>,
    poll: Duration,
    shutdown: CancellationToken,
) {
    loop {
        let drained = tokio::select! {
            () = shutdown.cancelled() => return,
            drained = relay.drain_once() => drained,
        };
        match drained {
            Ok(0) => {}
            Ok(_) => {
                moved.notify_one();
                continue;
            }
            Err(error) => tracing::warn!(%error, "draining auth-svc's outbox failed; retrying"),
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(poll) => {}
        }
    }
}

/// Claims and runs due jobs, one at a time, until shutdown; when there are none, waits for
/// `poll` or for the relay to move events. At shutdown the jobs of the batch not started
/// yet are given back.
async fn run_jobs(
    queue: Queue,
    jobs: Jobs,
    moved: Arc<Notify>,
    poll: Duration,
    shutdown: CancellationToken,
) {
    loop {
        let claimed = tokio::select! {
            () = shutdown.cancelled() => return,
            claimed = queue.claim(JOB_BATCH) => claimed,
        };
        match claimed {
            Ok(batch) if !batch.is_empty() => {
                for (index, job) in batch.iter().enumerate() {
                    if shutdown.is_cancelled() {
                        release(&queue, &batch[index..]).await;
                        return;
                    }
                    let outcome = jobs.run(job).await;
                    log(job, &outcome);
                    match queue.finish(job, outcome).await {
                        Ok(true) => {}
                        Ok(false) => tracing::warn!(
                            job_id = %job.id,
                            "the job's lease ended before it finished; its outcome is dropped"
                        ),
                        Err(error) => {
                            tracing::warn!(job_id = %job.id, %error, "recording a job's outcome failed");
                        }
                    }
                }
                continue;
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "claiming jobs failed; retrying"),
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = moved.notified() => {}
            () = tokio::time::sleep(poll) => {}
        }
    }
}

/// Gives back jobs claimed but not started, so another replica can take them at once.
/// If that fails, they come back when their lease ends.
async fn release(queue: &Queue, jobs: &[Job]) {
    match queue.release(jobs).await {
        Ok(released) => tracing::info!(released, "gave back the jobs not started"),
        Err(error) => tracing::warn!(%error, "giving back the jobs not started failed"),
    }
}

/// One line per attempt that did not simply succeed; never a payload.
fn log(job: &Job, outcome: &Outcome) {
    match outcome {
        Outcome::Done | Outcome::RunAgain(_) => {
            tracing::debug!(job_id = %job.id, kind = %job.kind, "job done");
        }
        Outcome::Failed { reason, .. } => tracing::warn!(
            job_id = %job.id,
            kind = %job.kind,
            attempts = job.attempts,
            reason,
            "job failed; retrying later"
        ),
        Outcome::Poison(reason) => tracing::error!(
            job_id = %job.id,
            kind = %job.kind,
            attempts = job.attempts,
            reason,
            "job failed for good; buried"
        ),
    }
}

/// Waits for the health server to stop.
async fn finish(health_server: tokio::task::JoinHandle<std::io::Result<()>>) -> anyhow::Result<()> {
    health_server.await??;
    Ok(())
}

/// Calls `connect` until it succeeds, logging each failure. `None` if `shutdown` came
/// first.
async fn retry<T, E, F, Fut>(what: &str, shutdown: &CancellationToken, connect: F) -> Option<T>
where
    E: std::fmt::Display,
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    loop {
        let attempt = tokio::select! {
            () = shutdown.cancelled() => return None,
            attempt = connect() => attempt,
        };
        match attempt {
            Ok(value) => {
                tracing::info!(dependency = what, "connected");
                return Some(value);
            }
            Err(error) => {
                tracing::warn!(dependency = what, %error, "not reachable yet; retrying");
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => return None,
            () = tokio::time::sleep(RETRY_DELAY) => {}
        }
    }
}

#[derive(Clone)]
struct Health {
    pool: Arc<OnceLock<PgPool>>,
    shutdown: CancellationToken,
}

fn health_router(health: Health) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .with_state(health)
}

/// Ready once both databases answered at start-up and the worker's own answers now.
async fn readyz(State(health): State<Health>) -> (StatusCode, &'static str) {
    if health.shutdown.is_cancelled() {
        return (StatusCode::SERVICE_UNAVAILABLE, "shutting down");
    }
    match health.pool.get() {
        None => (StatusCode::SERVICE_UNAVAILABLE, "starting"),
        Some(pool) => match sqlx::query("SELECT 1").execute(pool).await {
            Ok(_) => (StatusCode::OK, "ready"),
            Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "database unreachable"),
        },
    }
}

/// Structured JSON logs on stdout. The level comes from `RUST_LOG` (default `info`).
pub fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(false)
        .init();
}

/// Resolves on Ctrl-C, or on SIGTERM where there is one (Kubernetes sends it).
pub async fn terminated() {
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}
```

Create `crates/push-worker/src/main.rs`:

```rust
use push_worker::{Config, init_logging, serve, terminated};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

// `serve` runs the migrations itself, with `push_worker::migrate_database(url)` on a
// dedicated connection at start-up (not on the pool).
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    let config = Config::from_env()?;
    let http = TcpListener::bind(config.http_addr).await?;
    let health = TcpListener::bind(config.health_addr).await?;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        terminated().await;
        tracing::info!("shutting down");
        signal.cancel();
    });
    serve(config, http, health, shutdown).await
}
```

Replace `crates/push-worker/src/lib.rs` with:

```rust
//! push-worker: the worker of push-messenger (spec §2, §3.3, §9).
//!
//! It owns the `worker` schema in its own database (`push_worker`) and drains the other
//! services' outboxes into its job queue: it relays auth-svc's user and device events to
//! message-svc, sends email, takes delivery webhooks, and runs the cleanup jobs.

pub mod accounts;
pub mod cleanup;
mod config;
mod db;
pub mod deliveries;
pub mod email;
pub mod http;
pub mod jobs;
pub mod queue;
pub mod relay;
mod server;

pub use config::{Config, ConfigError, Sender};
pub use db::{MIGRATOR, migrate_database};
pub use server::{init_logging, serve, terminated};
```

```powershell
cargo test -p push-worker
```

Expected: 13 unit tests (7 configuration), and `tests\cleanup.rs` 1, `tests\email.rs` 5,
`tests\queue.rs` 6, `tests\relay.rs` 3, `tests\server.rs` 2 — all pass. The worker's
first readiness probe answers 503 while it waits for auth-svc's grant, then 200.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock crates/push-worker
git commit -m "feat(worker): configuration, serving and graceful shutdown"
```

Expected: 780 tests pass.

---

### Task 10: Spec, README, roadmap and pull request

**Files:**
- Modify: `README.md`, `docs/superpowers/specs/2026-10-11-push-messenger-design.md`,
  `docs/superpowers/plans/2026-10-11-roadmap.md`

CI needs no change: the `rust` and `msrv` jobs run the whole workspace and already pull
`postgres:17-alpine` and `redis:7-alpine`. The image and the chart come with Plan 6B.

- [ ] **Step 1: Document the configuration**

In `README.md`, in the `push-message-svc` table, insert before its last row (the one for
`PUSH_DEV_SETUP`):

```markdown
| `PUSH_ACCOUNTS_TOKEN` | the worker's token for the `Accounts` API, 32 to 256 visible ASCII characters; without it `Accounts` refuses every call | none |
```

In the `push-auth` table, insert before its last row (the one for `PUSH_DEV_MAILBOX`):

```markdown
| `PUSH_AUTH_WORKER_ROLE` | the worker's role: granted `auth.outbox` and the two cleanup functions at start-up | none |
```

After the `push-auth` table, add a blank line and the following, keeping a blank line
before `## License`:

```markdown
`push-worker`:

| Variable | Meaning | Default |
|----------|---------|---------|
| `PUSH_DATABASE_URL` | Postgres URL of the `push_worker` database, as its owner | required |
| `PUSH_AUTH_DATABASE_URL` | Postgres URL of auth-svc's `push_auth` database, as the worker's role | required |
| `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc, `http://host:50051` (plain `http://` only: the worker is built without TLS) | required |
| `PUSH_ACCOUNTS_TOKEN` | message-svc's token for its `Accounts` API, the same value as message-svc's | required |
| `PUSH_EMAIL_SENDER` | `log` (logs instead of sending) or `resend` | `log` |
| `PUSH_EMAIL_FROM` | the From of every email, `Push Messenger <no-reply@push.kubegames.dk>` | required for `resend` |
| `PUSH_EMAIL_PRODUCT_NAME` | the name emails sign with | `Push Messenger` |
| `PUSH_RESEND_API_KEY` | Resend API key | required for `resend` |
| `PUSH_RESEND_API_URL` | Resend's API (`http` only for `127.0.0.1`/`localhost`) | `https://api.resend.com` |
| `PUSH_RESEND_WEBHOOK_SECRET` | the webhook's signing secret, `whsec_…`; without it the webhook is not served | none |
| `PUSH_WORKER_POLL_MS` | how often an idle worker looks for work, in milliseconds | `1000` |
| `PUSH_UNVERIFIED_ACCOUNT_DAYS` | accounts unverified this many days are deleted | `7` |
| `PUSH_WORKER_RETENTION_DAYS` | buried jobs, email records and delivery events are kept this many days; a buried `email.send` job keeps the recipient address until then | `30` |
| `PUSH_HTTP_ADDR` | address of `/v1/webhooks/resend` (and the DEV ONLY mailbox) | `0.0.0.0:8080` |
| `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
| `PUSH_DB_MAX_CONNECTIONS` | size of each of its two pools, at least 1 | `5` |
| `PUSH_DEV_MAILBOX` | `1` serves the DEV ONLY `GET /dev/mailbox`; refused with any sender but `log` | off |
```

- [ ] **Step 2: Record the implementation in the spec**

In §3.3, after the closing fence of the schema block and before the paragraph that starts
`The worker claims work with`, add a blank line and:

```markdown
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
```

In §5.3, after the last item (`6. The gateway learns the device only from the verified
token. …`, whose sub-bullet ends `never treat it as a revocation or sign the user out.`),
add a blank line and:

```markdown
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
```

In §9, after the bullet `- Local development uses a log-only `EmailSender`.`, add a blank
line and:

```markdown
Implementation notes (push-email, push-worker, Plan 6A):

- Templates `verify_email`, `account_exists` and `reset_password`, as plain text and minimal
  HTML with escaped parameters; the product name is configurable.
- `ResendSender` posts to `/emails` with the email's id as `Idempotency-Key` and a
  `template` tag; 429, 409, 5xx, 401/403 and transport errors are retried, other refusals
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
```

- [ ] **Step 3: Mark plan 6A in review**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the empty status cell at the end
of the row for plan 6A (`| 6A | Worker and email: the services | … | 5B | |`) with
`In review`.

- [ ] **Step 4: Final gates**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 test --workspace --locked
```

Expected: no diff, no warnings, 780 tests on both toolchains.

- [ ] **Step 5: Commit and push**

```powershell
git add README.md docs
git commit -m "docs: record plan 6A in the spec, README and roadmap"
git push -u origin feature/plan-06a-worker-email
```

- [ ] **Step 6: Open the pull request and wait for CI**

```powershell
$body = @'
Implements docs/superpowers/plans/2026-10-13-plan-06a-worker-email.md: push-email (templates, the EmailSender trait with a log sender and a Resend sender on rustls/ring, Svix-verified Resend webhooks) and push-worker (its own database and schema; a job queue claimed with FOR UPDATE SKIP LOCKED, leased, retried with backoff, poison buried; the outbox relay from auth.outbox, at least once; account events relayed to message-svc's new Accounts API; email.send rendered, suppression-checked, recorded and sent once; webhooks feeding the suppression list; hourly cleanups of expired refresh tokens, unverified accounts and old records; health, graceful shutdown). message-svc serves Accounts (idempotent, any order converges: a revocation before the registration wins) to the worker alone, behind a shared internal token; Identify answers a device not relayed yet with a retryable UNAVAILABLE instead of the revoked-device error; claims take at most 4 jobs, outcomes are fenced by the claim, jobs not started are given back at shutdown, and buried jobs keep no link; auth-svc grants the worker's role the outbox and two cleanup functions at start-up, and a signup over an unverified account resets its clock. 780 tests.

Plan 6B moves the smoke test to the real flow and puts the worker on minikube.
'@
gh pr create --base main --title "Plan 6A: worker and email" --body $body
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust`, `msrv`, `images`, `chart`). Do not merge; the
controller merges after the final review.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §2 the worker drains service outboxes, sends email, relays events, runs cleanup jobs; each service owns its database (the worker reads `auth.outbox` and two cleanup functions only); only the worker may call `Accounts` (shared token) | 1, 2, 6, 7, 8, 9 |
| §3.3 `job`, `email_message`, `email_event`, `email_suppression`; claims with `FOR UPDATE SKIP LOCKED`, several replicas, a crashed worker's work claimed again | 5, 6 |
| §5.2 unverified accounts (cleanup; a new signup restarts the clock) | 2, 8 |
| §5.3 the worker relays device events to message-svc; `device.revoked` marks the device revoked; `Identify` refuses a device not active in `member_device`, and asks to retry one not relayed yet | 1, 6 |
| §5.4 `user.password_reset` relayed (kicks: Plans 7 and 8) | 1, 6 |
| §9 Resend behind `EmailSender::send(message, idempotency_key)` and `WebhookVerifier::verify`/`parse`; the worker renders, checks `email_suppression` and sends; hard bounces and complaints suppress; `recipient_domain` recorded; log-only sender locally | 3, 4, 7 |
| §7.6 strict DEV ONLY switches with a warning; URLs and secrets never echoed; migrations on a dedicated connection; start-up retries interruptible | 9 |
| §12 integration tests against real Postgres: outbox relay, `SKIP LOCKED` | 5, 6, 7, 8, 9 |

## Review fixes made after this plan was written

The code blocks above already include these; the commits that made them follow each task's
own.

- **auth-svc, `0002_worker.sql` (Task 2):** the cleanup functions are `STRICT` (a NULL
  argument deletes nothing); the batch is `least(batch, 10000)`; the age is
  `greatest(older_than, interval '1 day')`; `is_role_name` refuses `public`, `none` and
  `pg_*`.
- **push-email (Task 3):** 408 is retryable; `ResendSender` follows no redirects; Resend's
  error name is trimmed to `[a-z_]`, 64 characters at most; a test for link escaping.
- **push-worker (Tasks 5 to 9):** `email_message.params` has `CHECK (NOT params ? 'link')`;
  the stale-claim test claims with a separate 60 s-lease queue; the `Accounts` token
  metadata is marked sensitive; a failed email is marked only while queued; cleanup
  failures are logged; the product name and the From address refuse control characters.
- **Retention:** a buried `email.send` job keeps the recipient address until `delete_dead`
  removes it after 30 days.

## Deferred to later plans

- **The smoke test, the image, the chart and minikube** (Plan 6B): the smoke test waits for
  the worker's relay instead of mirroring users and devices through `DevSetup`, reads the
  verification email from the worker's mailbox, and auth-svc's own DEV ONLY mailbox goes.
- **Kicks** (Plan 7: message-svc publishes `kick:{device_id}` on `device.revoked` and for
  every device on `user.password_reset`; Plan 8: the gateway closes kicked sockets with 4003
  and refuses tokens older than a password reset).
- **`LISTEN/NOTIFY`** to wake the worker at once instead of polling every second.
- **A NetworkPolicy** so only the worker can reach message-svc's `Accounts` API (and only
  the gateway its `Messaging` API) — a Plan 11 must-do. Until then any pod in the namespace
  can reach message-svc, and only the shared token keeps `Accounts` to the worker.
- **A close code of its own for a device not relayed yet** (Plan 8): the gateway closes
  with 1011 on message-svc's `UNAVAILABLE` today; the SDK must reconnect after a short
  backoff and never sign the user out for it (spec §5.3).
- **Rotating the Accounts token** without a restart: message-svc and the worker read it
  at start-up; a rotation is a coordinated restart of both (Plan 11 with Key Vault).
- **Checking `worker.email.webhookSecret`'s format when the chart renders** (`whsec_` and
  base64); today the worker refuses a malformed secret at start-up.
- **A dev webhook secret can outlive dev mode**: the generated secret stays in the kept
  Secret if the same release later leaves dev mode; Plan 11's Key Vault secrets replace it.
- **Unsetting `PUSH_AUTH_WORKER_ROLE` does not revoke the grants** auth-svc gave the role
  earlier; revoking them is manual SQL until a deployment needs it.
- **Deliverability metrics**: the verification-completion rate by `recipient_domain`
  (spec §9) — the data is recorded; the query or dashboard is not.
- **Resend's other events** (`email.failed`, `email.suppressed`, `email.delivery_delayed`)
  are acknowledged and ignored; Resend keeps its own suppression list too.
- **Re-queueing buried jobs** is manual SQL for now (`UPDATE worker.job SET dead_at = NULL,
  attempts = 0, run_after = now() WHERE …`); a tool or an admin API comes later.
- **Old revoked devices** and their positions are not cleaned up.
- **Email design and languages**; a `List-Unsubscribe` header if non-transactional email
  ever appears.
- **Plan 7, must-fix:** a per-user advisory lock in `device_registered` and the
  conversation-join paths. Under READ COMMITTED, an interleaving plus a duplicate
  `device.registered` can make a new device skip unacked messages.
- **Plan 8:** the identify oracle (`UNKNOWN_DEVICE` vs `UNAVAILABLE` tells a caller whether a
  device exists); lower the "identify failed" log level.
- **Periodic cleanup jobs buried by an older replica never come back.** Seed them with
  `ON CONFLICT DO UPDATE SET dead_at = NULL`.
- **Email database errors retry forever** (no `max_attempts` on them).
- **The unsigned-webhook warn log can be flooded** by anyone who can reach the endpoint.
- **A test for 413** (a webhook body over the limit).
- **A verified-body type for webhooks**, so a handler cannot read a body that was not
  verified.
- **A grant-failure log** separate from "postgres not reachable" when auth-svc's start-up
  grant fails.
- **The relay's `batch > 0` guard.**
- **`ResendSender` enforcing `https`** itself instead of relying on the configuration.
