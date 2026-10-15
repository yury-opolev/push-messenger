# Plan 9A — SDK core: the client library Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `push-sdk`, the Rust core that third-party apps embed (spec §8). A `Client` signs up and in through auth-svc (sign-up, verification resend, password reset request, sign-in with an idempotent device registration that reuses its `registration_id` across retries and restarts, refresh with rotation), keeps the device's credentials only in a `SecretStore` the app provides and never logs them, and holds the WebSocket to the gateway: `Identify` with the access token, `Ready`, heartbeats, `AuthExpiring` → refresh → `ReAuth`, and reconnects with exponential backoff and jitter on every close except 4001 (signed out: sign in again), 4003 (device removed: stop) and 4007 (opened elsewhere: stop) — 4007 is new, so the gateway now tells a takeover apart from a revocation (Task 1); 4002 refreshes first, and 4006 (not relayed yet) is retried within seconds and never signs anyone out. A SQLite database holds conversations, messages, receipt and read positions, contacts and an outbox of sends, edits and deletes: everything is validated before it is queued, every row has its `client_msg_id` before the first attempt, rows go one at a time per conversation, and the outbox survives a restart. Opened with a `DatabaseKey` the app derives from the user's PIN or password, the whole database file is encrypted (SQLite3 Multiple Ciphers, compiled from source like any C dependency: no system library, no OpenSSL); without the key nobody can read it, not even the app. Deliveries are stored, de-duplicated by `(conversation, seq)`, and acknowledged with one `Ack` per conversation naming only the highest `seq` (the gateway's frame budget is 100 at once and 20 a second); a delivery the store cannot take ends the connection, so no later `Ack` can cover it. Receipts keep the highest positions because they can arrive out of order. The app sees one ordered, bounded event stream. Groups are not offered to apps in the first release, so the SDK has no group calls, but its types leave room for them. Plan 9B adds the SDK half of the smoke test and verifies it on minikube.

**Architecture:** One crate, `crates/push-sdk`, in layers that can each be tested alone, plus a small gateway change. `build.rs` compiles SQLite3 Multiple Ciphers; `rusqlite` links to it instead of its bundled SQLite. `secrets.rs` is the app's `SecretStore` trait (three blocking calls, so a C or Node callback can implement it); `auth.rs` is auth-svc's public HTTP API (retrying only the calls that are safe to repeat, and refreshes only within auth-svc's grace); `credentials.rs` serialises refreshes, so a rotated refresh token is never presented twice, and saves new tokens before they are used. `store.rs`, `store/outbox.rs` and `key.rs` are the SQLite database, its rules about positions, replays, edits, deletes and the outbox, and its key. `policy.rs` holds the pure rules: what each close code means, the backoff, the client's own frame budget and the batching of acks. `engine.rs` is everything that rides on a connection, with no socket: it takes frames and returns frames and events. `connection.rs` is the one task that owns the socket and the reconnect loop. `client.rs` and `client/requests.rs` are the public `Client`: it owns its own tokio runtime (spec §8.5), so its async methods can be awaited from any executor and its plain methods change the local store at once. The public types (`model.rs`) are plain data — ids, strings, numbers and enums — so the bindings of Plan 10 copy them across the FFI boundary without logic. `docs/sdk-secure-storage.md` tells app developers how to back the secret store with the platform's keychain and how to derive the database key. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §4, §5, §7, §8, §12.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85). `rusqlite` 0.32.1 with `modern_sqlite` (its bundled bindings) instead of `bundled`: it must stay at 0.32, whose `libsqlite3-sys` (0.30) is the version `sqlx` 0.8 names in the lockfile — two crates that link `sqlite3` cannot share a lockfile even while sqlx's SQLite driver is off. The SQLite it links is SQLite3 Multiple Ciphers 2.5.1 (SQLite 3.53.4), whose amalgamation comes from the `sqlite3mc-src` crate (sources only, no dependencies, rust-version 1.81) and is compiled by `push-sdk`'s `build.rs` with `cc` (MSVC on Windows, gcc in CI and in the Docker builder; no cmake, no perl, no OpenSSL). `push-sdk` also uses, from the workspace: `push-proto`, `push-domain`, `argon2` (Argon2id for `DatabaseKey::derive`), `zeroize`, `tokio`, `tokio-tungstenite` 0.29 (with `rustls-tls-webpki-roots`), `reqwest` 0.12 (with `rustls-tls`: rustls on ring and the webpki roots, as push-email), `futures-util`, `rand` 0.9, `serde`, `serde_json`, `thiserror`, `tracing`, `uuid`, and `axum` 0.8 for its tests. No aws-lc, no OpenSSL.

---

## Before you start

- **Plan 8B is merged** (PR #15). This plan changes the gateway's takeover close code
  (Task 1: `push-proto`'s close constants, the gateway and its tests), adds
  `crates/push-sdk`, gives `push-domain`'s ids `FromStr` and `From<Uuid>`, and adds a README
  section and `docs/sdk-secure-storage.md`. The chart and the smoke test do not change
  (Plan 9B).
- **The spec is already revised.** `docs/superpowers/specs/2026-10-11-push-messenger-design.md`
  on `main` describes the SDK and close code 4007 as built here (it was committed with this
  plan), so no task changes it; Task 14 only marks the roadmap.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-09a-sdk-core
```

- Fix a stale PATH for the current PowerShell session (each new shell needs it again), and
  build without incremental artifacts (they fill the disk):

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
$env:CARGO_INCREMENTAL = '0'
```

- **Docker must be running** for the workspace gates (the other crates' tests start
  `postgres:17-alpine` and `redis:7-alpine`). `push-sdk`'s own tests need no Docker: they
  run against a fake gateway (`tokio-tungstenite`) and a fake auth-svc (`axum`) on
  `127.0.0.1`, and the tests that need database files write them to a directory of their
  own next to the test binary (`target/debug/deps/push-sdk-test-data/`), removed after
  each test. Never touch other Docker containers on this machine.
- **How the code is given.** Every code block of a task was pasted from the commit that
  implemented and verified it. A new file is given whole ("Create"). A changed file is
  given as a git patch ("Change"): save the block to a file outside the tree and run
  `git apply <file>` from the repository root, in the order the task lists them; a patch
  that does not apply means an earlier step was skipped or changed. A new Markdown file is
  given as a patch too, because it contains code blocks of its own.
- **Keep files LF** (`.gitattributes`: `* text=auto eol=lf`). `git apply` needs the tree as
  committed.
- **Do not pipe `cargo fmt`.** Run `cargo fmt --all -- --check` on its own and read the exit
  code. The code in this plan is formatted, so a diff means a copying mistake.
- Baseline (after Plan 8B): `cargo test --workspace --locked` passes 874 tests;
  `deploy/helm/test-chart.ps1` passes 31 checks.
- **Cargo.lock.** Task 2 adds every dependency `push-sdk` will use, so its patch carries the
  whole `Cargo.lock` change and every later gate runs with `--locked`. Never run
  `cargo update`.
- **The first build of `push-sdk` compiles SQLite3 Multiple Ciphers** (one C file of about
  10 MB): about a minute with MSVC, once per profile; later builds reuse it.
- **Red, then green.** The code of a task comes with its tests. Step 2 of every code task
  proves the tests bite: it breaks one rule on purpose, runs the task's tests and expects
  the named test to fail, then restores the file. Only then do the gates run (Step 3).
  Task 13 (a document) writes its test first instead.
- **Every task ends green:**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

- **Disk space.** Watch the target directory; run `cargo clean` if it passes ~30 GB.
- **Scope rule:** the only server change is Task 1's close code; no change to the chart,
  the scripts or the smoke test (Plan 9B); `push-domain` only gains the two id
  conversions; no group calls in the SDK (owner decision); no keychain stores (owner
  decision: the guide shows apps how); no bindings (Plan 10).

## Key rules the code must preserve

- **Only 4001 signs out; only 4003 and 4007 stop.** Every other close — 4002 (after a
  refresh), 4004, 4005, 4006, 1002, 1008, 1009, 1011, a network error, a missing close
  frame — must lead to a reconnect. 4006 means "not relayed yet": retry, never sign out.
  4003 (`DeviceRemoved`) and 4007 (`OpenedElsewhere`) keep the credentials and the local
  data.
- **The SDK never erases local data on the server's word** (owner decision): not on 4001,
  4003 or 4007. It erases only when another user signs in on the database, and on the
  user's own `sign_out()` — then always, also offline.
- **A refresh token is never presented twice by one client,** and never after auth-svc's
  30 s grace: refreshes are serialised; a caller that saw a stale access token gets the
  newer one if another caller already refreshed; one refresh's attempts all end within
  25 s of the first; a refresh in flight is never cut off. New tokens are saved to the
  secret store before they are used.
- **Tokens and contents never leave their place.** No log line, error message or `Debug`
  output contains a token, a password, a database key or a message body; `Debug` shows a
  body or a group title as its length.
- **Nothing invalid is queued** (spec §8.3): empty or oversized text, an unknown
  conversation, an edit or delete of someone else's message, a deleted one or one past the
  48-hour window are refused before anything is stored.
- **`client_msg_id` exists before the first attempt** and is reused on every attempt, also
  after a restart; an outbox row is removed only by its `SendAck` or a refusal.
- **One row in flight per conversation, in order.** A refused row leaves the outbox (with
  the rows that change it) and the next one goes; a retryable error waits with backoff.
- **Acknowledge only what is stored, and only the highest `seq`.** A delivery is acked after
  it is committed locally; acks wait until deliveries pause for 50 ms (at most 250 ms) and
  name each conversation's highest stored `seq`, one frame per conversation. A delivery
  the store cannot take ends the connection before anything else is acknowledged on it.
  The client sends at most 50 frames at once and 10 a second after that, below the
  gateway's 100 and 20.
- **Positions only move forward.** Receipts, read positions and listings keep the highest
  value they have seen; a lower one is ignored, so receipts that arrive out of order change
  nothing. A read position never passes the conversation's last known `seq`.
- **Edits and deletes apply only if newer** than the target's last change, and only to a
  message held locally (spec §4.6); a delete erases the body.
- **A wrong database key changes nothing.** The key is checked by the first read, before
  anything is written; the SDK never stores the key.

## Decisions made for this plan

These were settled while writing the plan and verified in a worktree. The owner's earlier
decisions and those of this plan's review apply (below). Do not reopen any of this during
execution.

1. **Plan 9 is split.** 9A (this plan) delivers the crate and the gateway's new close code,
   tested against a fake gateway and a fake auth-svc; 9B adds the SDK half of the smoke
   test, runs it against the whole stack locally and on minikube.
2. **SQLite through `rusqlite` 0.32, linked to SQLite3 Multiple Ciphers.** The store is
   small, synchronous and transactional, and a synchronous API keeps it free of the
   runtime, which suits the FFI. Whole-database encryption (owner decision 4) needs an
   SQLite with a codec. What was tried: `rusqlite`'s `bundled-sqlcipher-vendored-openssl`
   builds OpenSSL from source with `openssl-src`, which needs a native Perl (and nmake) on
   Windows — this machine has only Git's MSYS Perl, and the build stops with "Command
   'perl' not found" — and Perl's `IPC::Cmd`, which the Docker builder
   (`rust:1.85-slim-bookworm`) lacks; plain `bundled-sqlcipher` needs a system OpenSSL.
   Both add system dependencies, so neither was taken. SQLite3 Multiple Ciphers carries its
   own ciphers in one C file: `build.rs` compiles the amalgamation from the `sqlite3mc-src`
   crate into a static library named `sqlite3` in `OUT_DIR` and gives the linker only that
   directory; `rusqlite` without `bundled` (its `modern_sqlite` feature keeps the bundled
   bindings) lets `libsqlite3-sys` ask for `sqlite3`, and the linker finds this one before
   any system library. Verified with MSVC, in the Docker builder, and in that image with
   `libsqlite3-dev` and `pkg-config` installed (as on CI's ubuntu-latest); a test asserts
   `sqlite3mc_version()` answers, so a build that linked a plain SQLite fails at once.
   Compile options follow `libsqlite3-sys`'s bundled build, plus `SQLITE_SECURE_DELETE`,
   `SQLITE_TEMP_STORE=2` (temporary data never in a plain file) and `SQLITE_DQS=0`;
   `SQLITE_OMIT_SHARED_CACHE` is not set (the tests use shared in-memory databases). The
   database uses WAL with `synchronous = NORMAL`: a crashed app never loses a committed
   outbox row. One file per app profile; the schema has a version (`PRAGMA user_version`),
   and a database from a newer SDK is refused rather than misread.
3. **The database belongs to one user.** It records its owner; when another user signs in
   on it, everything is erased first, outbox included. A sign-out by the server (4001, a
   password reset), a removal (4003) and a takeover (4007) keep the data and the outbox
   (owner decision 1); an explicit `sign_out()` revokes the device and erases the database.
   Erasing leaves nothing in the files: `PRAGMA secure_delete = ON` overwrites deleted
   content, and the write-ahead log, which still holds the old pages, is checkpointed and
   truncated (`wal_checkpoint(TRUNCATE)`); a test reads the raw files.
4. **The connection is a state machine with the rules apart.** `policy.rs` maps close codes
   to actions (`SignOut`, `DeviceRemoved`, `OpenedElsewhere`, `Refresh`, `NotReadyYet`,
   `Reconnect`) and computes the backoff; `connection.rs` runs the loop. States the app
   sees: `Offline`, `Connecting`, `Connected`, `Reconnecting{attempt, retry_in_ms}`,
   `SignedOut`, `DeviceRemoved`, `OpenedElsewhere`. Backoff: 1 s doubling to 60 s, each
   delay drawn between half and all of it; 4006 has its own: 500 ms doubling to 5 s (the
   worker relays in about a second; at least 2.5 s apart once capped, so the gateway's 30
   `Identify` a minute is never reached). A connection that stayed `Ready` 10 s resets the
   backoff. 4002 refreshes and reconnects at once, but a second 4002 in a row backs off (a
   skewed clock must not loop). The first `Ping` comes at a random point of the first
   heartbeat, then one every heartbeat; no `Pong` within 15 s ends the connection.
5. **Credentials.** The access token is refreshed before a connection when it has less
   than 90 s left (the gateway warns 60 s before expiry); `AuthExpiring` starts a refresh
   in the background while deliveries continue, then `ReAuth`; an anonymous retryable
   `Error` (the gateway could not check the `ReAuth`) sends it again after 1 s; a failed
   refresh is tried again every 5 s until the gateway's 4002 deadline. A refresh refused
   with 401 (`unauthorized`, `token_expired`) signs out. Requests that are safe to repeat
   (device registration with its `registration_id`, reads, revocation, refresh) are
   retried up to three times on transport errors and 502–504, after 0.5, 1 and 2 s (3.5 s
   of waiting in all); each attempt may take up to 15 s. A refresh is different: auth-svc
   accepts a rotated refresh token again only for 30 s, so each refresh attempt is cut off
   after 8 s and no attempt starts that could end later than 25 s after the first; and a
   refresh in flight when a connection ends is left to finish (it saves what auth-svc
   answered) rather than aborted. Sign-up, sign-in and the email requests are never
   retried, since they count against rate limits. The `registration_id` is created and
   saved before the first attempt and forgotten only after the session it produced is
   saved, so a retry after a crash or a failed save finds the same device; an id auth-svc
   answers `device_not_found` for (it belonged to a device revoked since) is replaced with
   a new one and the registration tried once more. The device id is remembered after a
   sign-out by the server, so signing in again uses `POST /v1/auth/login {…, device_id}`
   and keeps the device (no new device each password reset); a revoked device
   (`device_not_found`) registers anew.
6. **The secret store is a trait of three blocking calls** — `load`, `save`, `delete` of
   opaque bytes under fixed keys (`push.session`, `push.device`, `push.registration`,
   exported as `SECRET_KEYS`) — so a keychain, a C callback or a Node function can
   implement it; errors are strings. The SDK ships only `MemorySecretStore`, for tests and
   tools: platform stores are left to apps, with `docs/sdk-secure-storage.md` as the guide
   (owner decision 3).
7. **The FFI-friendly shape.** One `Client` (cheap to clone, `Send + Sync`) owning one
   multi-thread tokio runtime of two workers, dropped with `shutdown_background` so the
   last clone may be dropped inside an async context; it is built after the store opened,
   so a failed `open` never drops a runtime in async code. Async methods run on that
   runtime and can be awaited from any executor (a binding blocks on them or turns them
   into a Promise); plain methods read or change the local store at once. One event stream
   (`Events::next`, `try_next`) that a binding forwards to its callback. Public types are
   plain data: `push-domain`'s id newtypes (now `FromStr` and `From<Uuid>`), `String`,
   `i64`/`u32`, `bool`, and enums with data; no lifetimes, generics or trait objects except
   the `SecretStore`. One error type, `SdkError`, with a stable string `code()` (the
   server's own code for server refusals: `not_contact`, `invalid_credentials`, …) and
   `is_retryable()`. `send` returns the local echo, whose `client_msg_id` the app keeps;
   `cancel(conversation, client_msg_id)` replaces spec §8.2's `SendHandle`, because a
   handle object is harder to carry across FFI than an id.
8. **Events.** `ConnectionStateChanged`, `MessageAdded` (the local echo, or a message
   received), `MessageSent` (stored by the server: it has its `seq`), `MessageEdited`,
   `MessageDeleted`, `SendFailed{conversation, client_msg_id, kind, error}`,
   `ReceiptChanged{conversation, peer_delivered_seq, peer_read_seq}`,
   `ConversationChanged`, `ContactChanged`, `SignedOut`, `DeviceRemoved`,
   `OpenedElsewhere`, `EventsLost`. A message's status is computed when it is read:
   `Pending` without a `seq`, else `Read`, `Delivered` or `Sent` from the conversation's
   highest receipt positions, and `Received` for others' messages. Receipts are
   positions, so one `ReceiptChanged` updates every tick at once. **The queue is bounded:**
   at most 10,000 events wait; when only one place is left, newer events are dropped and
   the last place takes one `EventsLost`, which tells the app to reload what it shows from
   the client's local data (the store has everything; events after it may repeat what the
   reload shows, so the UI applies them by id). Without the bound, an app whose UI stopped
   reading would make the client's memory grow without limit. A state change is queued
   while the state is held, so the client's `connect` and the connection task report
   changes in the order they made them.
9. **Deliveries.** Entries at or below the conversation's received position are replays
   (the same entry on a second connection of the device, or again before the ack) and
   change nothing but are acknowledged again; an edit or delete applies only if its `seq`
   is above the target's `edit_seq`, and is skipped when the target is not held; a
   superseded edit carries no body and is skipped. A delivery for a conversation not held
   yet creates it and lists the conversations again. **A delivery the store cannot take**
   (a full disk, a broken file) is not acknowledged, and the connection ends at once with a
   normal backoff reconnect: the gateway does not send an entry twice on one connection,
   so the next stored delivery's `Ack`, naming a higher `seq`, would acknowledge the lost
   one for good; the next connection syncs again from the last `Ack`. An entry whose ids
   are not ids can never be stored and is skipped.
10. **The outbox.** Rows go in insertion order, the first of each conversation only, at
    most 20 per wake (the rest follow as `SendAck`s come back, so a full outbox never holds
    the connection in the frame budget). A retryable error (`retryable`, or
    `RATE_LIMITED` with `retry_after_s`) waits 1 s doubling to 60 s with jitter; anything
    else removes the row and the rows that change it, removes a refused message's local
    echo, and reports `SendFailed`. An edit of a message never sent yet changes it in
    place, a delete drops it; a message being sent gets its edit or delete queued after it
    (`parent_id`), aimed at its `client_msg_id`. Edits and deletes of sent messages are
    applied locally when their `SendAck` (or their delivery) arrives, so the local state
    never shows a change the server refused. A connection that ends queues its rows in
    flight again, to go with the same id.
11. **Listings.** Every `Ready` lists the conversations (without `changed_since`, to refresh
    the receipt positions, spec §4.5) and the contacts, and sends read positions the server
    has not confirmed. A `ContactEvent` lists the conversations again (block and unblock
    hide and show them). Unread counts come from the listing and are recounted from the
    messages held when the read position moves; `mark_read` never moves the position past
    the conversation's last known `seq`.
12. **Requests** wait for the connection up to the request timeout (15 s by default) and then
    for their answer as long again; they fail at once with `Offline` when the app did not
    ask to connect, `NotSignedIn` after a sign-out, `DeviceRemoved` after a removal and
    `OpenedElsewhere` after a takeover. `connect()` itself moves a stopped state to
    `Connecting` before it returns, so a request made right after it waits for the
    connection instead of failing as offline. A request in flight when the connection drops
    fails with `Disconnected`; requests are never repeated by the SDK (an invite created
    twice would be two invites). Contact changes answered `Done` list the contacts to learn
    the new state.
13. **No proxies, TLS by rustls.** The SDK ignores `HTTP_PROXY` (as the smoke test does);
    `https://` and `wss://` use rustls with the webpki roots and ring. A `Host` override
    serves local clusters whose ingress is reached by address.
14. **One crate-level `#![allow(dead_code)]` while the parts arrive.** Tasks 2 to 11 add
    crate-private parts that only the complete `Client` calls; Task 12 removes the allow,
    and from then on the crate has none.
15. **The database key** (owner decision 4). `Config::database_key: Option<DatabaseKey>`;
    without it the store is plain, as before. A `DatabaseKey` is 32 bytes
    (`DatabaseKey::from_bytes`), or `DatabaseKey::derive(secret, salt)`: Argon2id with
    19 MiB, 2 passes and 1 lane (OWASP's recommendation and auth-svc's), a salt of at least
    16 bytes the app makes once with `DatabaseKey::new_salt()` and keeps (it is not
    secret). The store applies it with `PRAGMA hexkey` before anything else, so every page,
    the write-ahead log included, is encrypted with SQLite3 Multiple Ciphers' default
    scheme (ChaCha20-Poly1305, with its own per-database salt and key stretching on top);
    the first read (`SELECT count(*) FROM sqlite_master`) fails with "not a database" for a
    wrong key, for an encrypted database opened without one and for a plain one opened with
    one, which becomes `SdkError::WrongKey` before anything was written. `Client::
    change_database_key(Some(key) | None)` encrypts, re-encrypts or decrypts in place:
    SQLite3 Multiple Ciphers changes keys only outside WAL mode, so the store switches to
    the rollback journal, rekeys (one transaction: a crash leaves the old key or the new
    one), and switches back. The key lives only in the caller's `Config` (wiped on drop)
    and in SQLite's memory; `Debug` shows `DatabaseKey(<redacted>)`; the SDK never writes
    it anywhere. Tests read the raw files: no text of a message, not even SQLite's header,
    is in an encrypted database or its log.
16. **`sign_out()` signs out locally first, always.** It disconnects, forgets the credentials
    and the device — waiting for a refresh in flight, so that none saves them again — and
    erases the database; only then does it ask auth-svc to revoke the device, with the
    access token it held (refreshed in memory if it was about to expire). An app that ends
    during the revocation has still signed out; the call returns once auth-svc answered or
    the attempts ran out. The revocation's failure is the call's error, so the app can say
    "signed out here; remove this device from another one". The same lock keeps a refresh
    detached from a connection that ended from saving a session the server just signed
    out (4001).

## Owner decisions

Decisions the owner made on this plan's review (2026-10-15), recorded in the spec:

1. **A sign-out by the server keeps the data.** On 4001 (a password reset, or credentials
   refused) the SDK keeps the local history and the unsent messages, so the same person
   finds them when signing in again; it erases them only when a different person signs in
   on that device (and, as before, on the user's own `sign_out()`). A remote wipe is not a
   security feature: a thief can keep the device offline. `docs/sdk-secure-storage.md`
   says so to app developers. (Decision 3; Task 4, Task 10, Task 13.)
2. **"Opened elsewhere" has its own close code, 4007**, apart from 4003 "device removed".
   The gateway closes a connection that another connection of the same device took over
   with 4007; the SDK reports `OpenedElsewhere` (state and event), stops, keeps the
   credentials and does not erase anything. Plan 9 owns this small server change (Task 1:
   `push-proto`'s close constants, the gateway, its tests; spec §7.4). The raw smoke test
   checks only a revocation (4003), so it does not change; Plan 9B's SDK half checks a
   takeover.
3. **Secure storage of the tokens is left to app developers, with a guide.** The SDK ships
   no platform store; `docs/sdk-secure-storage.md` shows how to implement `SecretStore`
   with the Windows Credential Manager, the macOS and iOS Keychain, the Android Keystore
   and libsecret, what never to do (plain files and the like), and how to handle the
   bytes. (Task 13.)
4. **Optional whole-database encryption with a key the app provides** — for example derived
   from a PIN or a password the user enters, so that without it nobody can read the user's
   content, not even the app. Built as true whole-database encryption (decisions 2 and 15),
   with `WrongKey`, a change of key, no stored key, a derivation helper, and the guide.
   (Task 2, Task 6, Task 10, Task 13.)

Earlier owner decisions this plan follows: a password reset signs in again (4001) and a
revoked device stops (4003) (Plan 7); 4006 is retried and never signs out (Plan 8); groups
are not offered to apps in the first release (Plan 7); device registration is idempotent
with a client-chosen `registration_id` and refresh has a 30 s grace (Plan 5).

## Owner decisions needed

The plan works as written; these choose a default the owner may want otherwise. Neither
changes the wire protocol.

1. **Should a revoked device erase its local data?**
   When a user removes a device from another one ("sign out my old phone"), the gateway
   closes that device's connection with 4003 and the SDK reports `DeviceRemoved` and stops.
   The device can no longer send or receive anything. The question is what happens to the
   history and the unsent messages already on it.
   *As built:* they are kept, like after a password reset (owner decision 1): the SDK
   never erases on the server's word. The app may still erase them itself (`sign_out()`
   erases at once, also offline), for example when it shows "this device was removed".
   *The alternative:* the SDK erases the database when it is told 4003.
   *Example for keeping:* Dana removes her old laptop from her phone by mistake; she signs
   in again on the laptop and its history is still there (a new device is registered).
   *Example for erasing:* Dana's laptop is stolen; she removes it from her phone. If the
   thief has the laptop online and the app open when the 4003 arrives, its history is
   erased. If the thief keeps it offline — as a careful thief does — nothing is erased
   either way, which is why owner decision 1 called remote wipe "not a security
   feature"; the database key (owner decision 4) is what protects that history.
   *Recommendation:* keep (as built), for the same reason as decision 1.

2. **Where should the SQLite3 Multiple Ciphers source come from?**
   The encryption code is C compiled into the SDK. As built, `build.rs` takes it from the
   `sqlite3mc-src` crate on crates.io: the official amalgamation, byte for byte, which the
   crate's CI checks against the release's signed checksums. The crate is young (first
   published September 2026) and has one maintainer, so it is one more party the build
   trusts, and a future version could change what we compile.
   *Options:* (a) keep the crate, pinned by `Cargo.lock` (an update needs a deliberate
   `cargo update -p sqlite3mc-src`, and a review of the diff); (b) copy the amalgamation
   (two files, about 10 MB) into the repository under `vendor/sqlite3mc/`, with a script
   that downloads a release and checks its signature, and compile it from there.
   *Example:* a malicious 205.1.1 of the crate could add code that sends keys away; with
   (a) it reaches us only if someone runs `cargo update` and misses it in review; with (b)
   only if someone replaces the vendored files.
   *Recommendation:* (a) for now; (b) before the first public release of the SDK.
   *Decided (owner, 2026-10-15):* (b) at once; see spec §8, "The SQLite3 Multiple Ciphers
   source is vendored".

## File structure

```
Cargo.toml, Cargo.lock                     the push-sdk member; rusqlite 0.32 (modern_sqlite), zeroize
crates/push-proto/src/lib.rs               close codes: DEVICE_REMOVED (4003), OPENED_ELSEWHERE (4007)
crates/push-proto/proto/push/internal/v1/messaging.proto   the takeover kick's comment
crates/push-gateway/src/server.rs          a takeover closes with 4007
crates/push-gateway/tests/gateway.rs       the takeover test expects 4007
crates/push-domain/src/ids.rs              ids: FromStr, From<Uuid>
crates/push-sdk/Cargo.toml                 NEW
crates/push-sdk/build.rs                   NEW: compiles SQLite3 Multiple Ciphers
crates/push-sdk/src/lib.rs                 NEW: the public surface
crates/push-sdk/src/error.rs               NEW: SdkError, its codes
crates/push-sdk/src/model.rs               NEW: Conversation, Message, Contact, Event, …
crates/push-sdk/src/secrets.rs             NEW: SecretStore, MemorySecretStore, Session
crates/push-sdk/src/auth.rs                NEW: auth-svc's HTTP API
crates/push-sdk/src/credentials.rs         NEW: refreshes, registration id, last device
crates/push-sdk/src/store.rs               NEW: the SQLite store
crates/push-sdk/src/store/outbox.rs        NEW: the outbox
crates/push-sdk/src/key.rs                 NEW: DatabaseKey
crates/push-sdk/src/policy.rs              NEW: close codes, backoff, frame budget, acks
crates/push-sdk/src/testing.rs             NEW (tests only): fake gateway, fake auth-svc
crates/push-sdk/src/engine.rs              NEW: what rides on a connection, without I/O
crates/push-sdk/src/connection.rs          NEW: the socket and the reconnect loop
crates/push-sdk/src/client.rs              NEW: Client, Config, Events, Account
crates/push-sdk/src/client/sending.rs      NEW: send, edit, delete, cancel, mark_read
crates/push-sdk/src/client/requests.rs     NEW: the requests and auth-svc's signed-in calls
docs/sdk-secure-storage.md                 NEW: secret stores and the database key, for apps
README.md, roadmap                         docs
```

---

### Task 1: The gateway tells a takeover apart from a revocation (4007)

**Files:**
- Modify: `crates/push-proto/src/lib.rs`, `crates/push-proto/proto/push/internal/v1/messaging.proto`, `crates/push-gateway/src/server.rs`, `crates/push-gateway/tests/gateway.rs`

Owner decision 2. `close::TAKEN_OVER` (4003, used for both) becomes
`close::DEVICE_REMOVED` (still 4003, now only for a revoked device), and
`close::OPENED_ELSEWHERE` (4007) is new: a takeover kick, and a kick without a reason,
close with 4007; a revocation, by kick or found at `Identify`, with 4003 as before. The
gateway's takeover test now expects 4007; the revocation tests keep 4003.

- [ ] **Step 1: Make the changes**

Change `crates/push-proto/src/lib.rs`:

```diff
diff --git a/crates/push-proto/src/lib.rs b/crates/push-proto/src/lib.rs
index 3c569bb..75b244e 100644
--- a/crates/push-proto/src/lib.rs
+++ b/crates/push-proto/src/lib.rs
@@ -27,8 +27,9 @@ pub use codec::{
 };
 
 /// WebSocket close codes (spec §7.4). Codes from 4000 are the protocol's own; the others
-/// are RFC 6455's. A client signs out only on 4001; it never reconnects after 4003; on
-/// every other code it reconnects with backoff (after refreshing its token on 4002).
+/// are RFC 6455's. A client signs out only on 4001; it never reconnects by itself after
+/// 4003 or 4007; on every other code it reconnects with backoff (after refreshing its
+/// token on 4002).
 pub mod close {
     /// The access token is invalid (signature, audience, an unverified email) or older
     /// than the user's last password reset: sign in again.
@@ -36,8 +37,8 @@ pub mod close {
     /// The access token expired, names a key the gateway does not hold, or no `ReAuth`
     /// came by `exp + 30 s`: refresh it, then reconnect.
     pub const REAUTH_REQUIRED: u16 = 4002;
-    /// Another connection of the device took over, or the device was revoked: stop.
-    pub const TAKEN_OVER: u16 = 4003;
+    /// The device was revoked: stop.
+    pub const DEVICE_REMOVED: u16 = 4003;
     /// Rate limited: back off, then reconnect.
     pub const RATE_LIMITED: u16 = 4004;
     /// The server is shutting down: back off with jitter, then reconnect.
@@ -45,6 +46,10 @@ pub mod close {
     /// The device was registered moments ago and the server is not ready for it yet:
     /// reconnect after a short backoff. Never a reason to sign out.
     pub const NOT_READY_YET: u16 = 4006;
+    /// Another connection of the same device took over: the same app data was opened
+    /// elsewhere (a second window, a copy restored on another machine). Stop; the device
+    /// is still signed in.
+    pub const OPENED_ELSEWHERE: u16 = 4007;
     /// A protocol violation before `Ready`: no `Identify` within 5 s, or another frame
     /// first. Reconnect with backoff.
     pub const PROTOCOL_ERROR: u16 = 1002;
```

Change `crates/push-proto/proto/push/internal/v1/messaging.proto`:

```diff
diff --git a/crates/push-proto/proto/push/internal/v1/messaging.proto b/crates/push-proto/proto/push/internal/v1/messaging.proto
index 32f8a25..908dab6 100644
--- a/crates/push-proto/proto/push/internal/v1/messaging.proto
+++ b/crates/push-proto/proto/push/internal/v1/messaging.proto
@@ -211,7 +211,7 @@ enum KickReason {
   // message-svc.
   KICK_REASON_PASSWORD_RESET = 2;
   // A newer connection of the device identified, on this or another gateway: the older
-  // one stops (close code 4003). From the gateway.
+  // one stops (close code 4007, "opened elsewhere"). From the gateway.
   KICK_REASON_TAKEN_OVER = 3;
 }
 
```

Change `crates/push-gateway/src/server.rs`:

```diff
diff --git a/crates/push-gateway/src/server.rs b/crates/push-gateway/src/server.rs
index da7e256..d6cc986 100644
--- a/crates/push-gateway/src/server.rs
+++ b/crates/push-gateway/src/server.rs
@@ -563,9 +563,9 @@ fn kick_close(reason: KickReason) -> (u16, &'static str) {
             close::INVALID_IDENTITY,
             "the password was changed; sign in again",
         ),
-        KickReason::DeviceRevoked => (close::TAKEN_OVER, "the device was revoked"),
+        KickReason::DeviceRevoked => (close::DEVICE_REMOVED, "the device was revoked"),
         KickReason::TakenOver | KickReason::Unspecified => (
-            close::TAKEN_OVER,
+            close::OPENED_ELSEWHERE,
             "another connection of this device took over",
         ),
     }
@@ -617,9 +617,10 @@ async fn admit(messaging: &mut MessagingApi, claims: &Claims) -> Result<(), Refu
             close::INVALID_IDENTITY,
             "the password was changed; sign in again",
         )),
-        ErrorCode::UnknownDevice => {
-            Err(Refusal::Close(close::TAKEN_OVER, "the device was revoked"))
-        }
+        ErrorCode::UnknownDevice => Err(Refusal::Close(
+            close::DEVICE_REMOVED,
+            "the device was revoked",
+        )),
         ErrorCode::DeviceNotReady => {
             // Normal for a second after a device registers: the worker has not relayed it.
             tracing::debug!(device = %claims.device, "device not relayed yet");
```

Change `crates/push-gateway/tests/gateway.rs`:

```diff
diff --git a/crates/push-gateway/tests/gateway.rs b/crates/push-gateway/tests/gateway.rs
index e467892..4e5a918 100644
--- a/crates/push-gateway/tests/gateway.rs
+++ b/crates/push-gateway/tests/gateway.rs
@@ -540,14 +540,15 @@ async fn a_revocation_and_a_password_reset_close_the_socket_at_once() {
     assert_eq!(next(&mut laptop_socket).await, Body::Pong(Pong {}));
 }
 
-/// Spec §7.3: the newest connection of a device wins; the older one is told to stop.
+/// Spec §7.3, §7.4: the newest connection of a device wins; the older one is told it was
+/// opened elsewhere (4007), not that the device was removed (4003).
 #[tokio::test]
 async fn a_newer_connection_of_the_device_takes_over() {
     let stack = stack().await;
     let (user, device) = person(&stack).await;
     let mut first = ready(&stack, user, device).await;
     let mut second = ready(&stack, user, device).await;
-    assert_eq!(close_code(&mut first).await, 4003);
+    assert_eq!(close_code(&mut first).await, 4007);
     send(&mut second, Body::Ping(Ping {})).await;
     assert_eq!(next(&mut second).await, Body::Pong(Pong {}));
 }
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-gateway/src/server.rs`, in `kick_close`, change `close::OPENED_ELSEWHERE,`
to `close::DEVICE_REMOVED,` and run:

```powershell
cargo test -p push-gateway --test gateway --locked takes_over
```

Expected: `test a_newer_connection_of_the_device_takes_over ... FAILED` with
`left: 4003, right: 4007`, and `0 passed; 1 failed`. Undo the edit and run it again:
1 passed.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "gateway: a takeover closes with 4007 (opened elsewhere), apart from 4003 (device removed)"
```

Expected: 874 tests pass (as before: the takeover test now expects 4007), no warnings, no diff.

---

### Task 2: The crate, its errors, its types and the secret store; SQLite3 Multiple Ciphers; ids parse

**Files:**
- Create: `crates/push-sdk/Cargo.toml`, `crates/push-sdk/build.rs`, `crates/push-sdk/src/lib.rs`, `crates/push-sdk/src/error.rs`, `crates/push-sdk/src/model.rs`, `crates/push-sdk/src/secrets.rs`
- Modify: `Cargo.toml`, `Cargo.lock`, `crates/push-domain/src/ids.rs`

Decisions 2, 6, 7, 8 and 14. The workspace gets the member, `rusqlite` with
`modern_sqlite` instead of `bundled`, and `zeroize`; `push-sdk`'s manifest lists every
dependency the crate will use, so `Cargo.lock` changes only here. `build.rs` compiles SQLite3
Multiple Ciphers and gives the linker its directory; `the_linked_sqlite_can_encrypt` proves
`rusqlite` uses it. `SdkError` maps the gateway's `ErrorCode` to stable snake-case codes
(with `opened_elsewhere` and `wrong_key`); `model.rs` holds the public data types and the
event enum, whose `Debug` shows a body or a title only as its length; the secret store and
the stored `Session` (whose `Debug` redacts both tokens) are the first parts. The id
newtypes of `push-domain` gain `FromStr` and `From<Uuid>`, which the SDK and the bindings
need to turn strings into ids.

- [ ] **Step 1: Make the changes**

Change `Cargo.toml`:

```diff
diff --git a/Cargo.toml b/Cargo.toml
index 08a8fad..eec66c8 100644
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -7,6 +7,7 @@ members = [
     "crates/push-gateway",
     "crates/push-message-svc",
     "crates/push-proto",
+    "crates/push-sdk",
     "crates/push-smoke",
     "crates/push-store",
     "crates/push-testkit",
@@ -40,6 +41,7 @@ push-email = { path = "crates/push-email" }
 push-gateway = { path = "crates/push-gateway" }
 push-message-svc = { path = "crates/push-message-svc" }
 push-proto = { path = "crates/push-proto" }
+push-sdk = { path = "crates/push-sdk" }
 push-store = { path = "crates/push-store" }
 push-testkit = { path = "crates/push-testkit" }
 push-worker = { path = "crates/push-worker" }
@@ -50,6 +52,10 @@ redis = { version = "1", default-features = false, features = [
 ] }
 reqwest = { version = "0.12", default-features = false, features = ["json"] }
 ring = "0.17"
+# 0.32 links libsqlite3-sys 0.30, the version sqlx 0.8 names: two crates linking
+# sqlite3 cannot share a lockfile, even when sqlx's SQLite driver is off. Not `bundled`:
+# push-sdk compiles SQLite3 Multiple Ciphers itself and libsqlite3-sys links to it.
+rusqlite = { version = "0.32", features = ["modern_sqlite"] }
 serde = { version = "1", features = ["derive"] }
 serde_json = "1"
 sha2 = "0.10"
@@ -75,6 +81,7 @@ tower = { version = "0.5", features = ["util"] }
 tracing = "0.1"
 tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
 uuid = { version = "1", features = ["serde", "v4", "v7"] }
+zeroize = "1"
 
 # Argon2id is far too slow unoptimised, and the auth tests hash many passwords.
 [profile.dev.package.argon2]
```

Change `Cargo.lock`:

```diff
diff --git a/Cargo.lock b/Cargo.lock
index c2c912b..9290ddd 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -2,6 +2,18 @@
 # It is not intended for manual editing.
 version = 4
 
+[[package]]
+name = "ahash"
+version = "0.8.12"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "5a15f179cd60c4584b8a8c596927aadc462e27f2ca70c04e0071964a73ba7a75"
+dependencies = [
+ "cfg-if",
+ "once_cell",
+ "version_check",
+ "zerocopy",
+]
+
 [[package]]
 name = "aho-corasick"
 version = "1.1.5"
@@ -749,6 +761,18 @@ dependencies = [
  "pin-project-lite",
 ]
 
+[[package]]
+name = "fallible-iterator"
+version = "0.3.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "2acce4a10f12dc2fb14a218589d4f1f62ef011b2d0cc4b3cb1bba8e94da14649"
+
+[[package]]
+name = "fallible-streaming-iterator"
+version = "0.1.9"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "7360491ce676a36bf9bb3c56c1aa791658183a54d2744120f27285738d90465a"
+
 [[package]]
 name = "fastrand"
 version = "2.5.0"
@@ -982,6 +1006,15 @@ version = "0.12.3"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "8a9ee70c43aaf417c914396645a0fa852624801b24ebb7ae78fe8272889ac888"
 
+[[package]]
+name = "hashbrown"
+version = "0.14.5"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "e5274423e17b7c9fc20b6e7e208532f9b19825d82dfd615708b70edd83df41f1"
+dependencies = [
+ "ahash",
+]
+
 [[package]]
 name = "hashbrown"
 version = "0.15.5"
@@ -999,6 +1032,15 @@ version = "0.17.1"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "ed5909b6e89a2db4456e54cd5f673791d7eca6732202bbf2a9cc504fe2f9b84a"
 
+[[package]]
+name = "hashlink"
+version = "0.9.1"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "6ba4ff7128dee98c7dc9794b6a411377e1404dba1c97deb8d1a55297bd25d8af"
+dependencies = [
+ "hashbrown 0.14.5",
+]
+
 [[package]]
 name = "hashlink"
 version = "0.10.0"
@@ -2161,6 +2203,30 @@ dependencies = [
  "uuid",
 ]
 
+[[package]]
+name = "push-sdk"
+version = "0.1.0"
+dependencies = [
+ "argon2",
+ "axum",
+ "cc",
+ "futures-util",
+ "push-domain",
+ "push-proto",
+ "rand 0.9.5",
+ "reqwest",
+ "rusqlite",
+ "serde",
+ "serde_json",
+ "sqlite3mc-src",
+ "thiserror",
+ "tokio",
+ "tokio-tungstenite",
+ "tracing",
+ "uuid",
+ "zeroize",
+]
+
 [[package]]
 name = "push-smoke"
 version = "0.1.0"
@@ -2255,7 +2321,7 @@ dependencies = [
  "quinn-udp",
  "rustc-hash",
  "rustls",
- "socket2 0.5.10",
+ "socket2 0.6.5",
  "thiserror",
  "tokio",
  "tracing",
@@ -2293,9 +2359,9 @@ dependencies = [
  "cfg_aliases",
  "libc",
  "once_cell",
- "socket2 0.5.10",
+ "socket2 0.6.5",
  "tracing",
- "windows-sys 0.52.0",
+ "windows-sys 0.61.2",
 ]
 
 [[package]]
@@ -2578,6 +2644,20 @@ dependencies = [
  "zeroize",
 ]
 
+[[package]]
+name = "rusqlite"
+version = "0.32.1"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "7753b721174eb8ff87a9a0e799e2d7bc3749323e773db92e0984debb00019d6e"
+dependencies = [
+ "bitflags 2.13.2",
+ "fallible-iterator",
+ "fallible-streaming-iterator",
+ "hashlink 0.9.1",
+ "libsqlite3-sys",
+ "smallvec",
+]
+
 [[package]]
 name = "rustc-hash"
 version = "2.1.3"
@@ -2947,6 +3027,12 @@ dependencies = [
  "der",
 ]
 
+[[package]]
+name = "sqlite3mc-src"
+version = "205.1.0+sqlite3mc-2.5.1-sqlite-3.53.4"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "f155b7d3de6d72fb7b7c7affcba3773634c1fb2978b944b425c3d3ed5474448c"
+
 [[package]]
 name = "sqlx"
 version = "0.8.6"
@@ -2978,7 +3064,7 @@ dependencies = [
  "futures-io",
  "futures-util",
  "hashbrown 0.15.5",
- "hashlink",
+ "hashlink 0.10.0",
  "indexmap 2.14.2",
  "log",
  "memchr",
@@ -3436,8 +3522,12 @@ checksum = "8f72a05e828585856dacd553fba484c242c46e391fb0e58917c942ee9202915c"
 dependencies = [
  "futures-util",
  "log",
+ "rustls",
+ "rustls-pki-types",
  "tokio",
+ "tokio-rustls",
  "tungstenite",
+ "webpki-roots 0.26.11",
 ]
 
 [[package]]
@@ -3694,6 +3784,8 @@ dependencies = [
  "httparse",
  "log",
  "rand 0.9.5",
+ "rustls",
+ "rustls-pki-types",
  "sha1",
  "thiserror",
 ]
```

Change `crates/push-domain/src/ids.rs`:

```diff
diff --git a/crates/push-domain/src/ids.rs b/crates/push-domain/src/ids.rs
index ac5e316..c67f43b 100644
--- a/crates/push-domain/src/ids.rs
+++ b/crates/push-domain/src/ids.rs
@@ -2,6 +2,7 @@
 //! database indexes compact. They serialise as plain UUID strings.
 
 use std::fmt;
+use std::str::FromStr;
 
 use serde::{Deserialize, Serialize};
 use uuid::Uuid;
@@ -33,6 +34,21 @@ macro_rules! id_type {
                 self.0.fmt(f)
             }
         }
+
+        impl From<Uuid> for $name {
+            fn from(uuid: Uuid) -> Self {
+                Self(uuid)
+            }
+        }
+
+        impl FromStr for $name {
+            type Err = uuid::Error;
+
+            /// Reads the hyphenated form that `Display` writes (other UUID forms too).
+            fn from_str(text: &str) -> Result<Self, Self::Err> {
+                Uuid::parse_str(text).map(Self)
+            }
+        }
     };
 }
 
@@ -56,3 +72,16 @@ id_type!(
     /// A log entry's idempotency key, chosen by the client before the first attempt.
     ClientMsgId
 );
+
+#[cfg(test)]
+mod tests {
+    use super::*;
+
+    #[test]
+    fn an_id_parses_what_it_displays_and_comes_from_a_uuid() {
+        let user = UserId::new();
+        assert_eq!(user.to_string().parse::<UserId>(), Ok(user));
+        assert_eq!(UserId::from(user.0), user);
+        assert!("not-a-uuid".parse::<ConversationId>().is_err());
+    }
+}
```

Create `crates/push-sdk/Cargo.toml`:

```toml
[package]
name = "push-sdk"
description = "The push-messenger client core: sign-in, the connection, the local store, the outbox and one event stream."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
# Argon2id for `DatabaseKey::derive`, as auth-svc hashes passwords.
argon2.workspace = true
futures-util.workspace = true
push-domain.workspace = true
push-proto.workspace = true
rand.workspace = true
# HTTPS to auth-svc: rustls with ring and the webpki roots, no OpenSSL and no cmake.
reqwest = { workspace = true, features = ["rustls-tls"] }
rusqlite.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "sync", "time"] }
# wss:// with rustls; ring is its provider through reqwest's rustls-tls.
tokio-tungstenite = { workspace = true, features = ["rustls-tls-webpki-roots"] }
tracing.workspace = true
uuid.workspace = true
zeroize.workspace = true

[build-dependencies]
# SQLite3 Multiple Ciphers (build.rs): the amalgamation, compiled with `cc`.
cc = "1"
sqlite3mc-src = "205.1"

[dev-dependencies]
axum.workspace = true

[lints]
workspace = true
```

Create `crates/push-sdk/build.rs`:

```rust
//! Compiles SQLite3 Multiple Ciphers, the SQLite that can encrypt the whole database file,
//! into a static library named `sqlite3` that `libsqlite3-sys` (not `bundled`) links to.
//! Only a C compiler is needed: MSVC on Windows, gcc or clang elsewhere.

use std::env;
use std::path::PathBuf;

fn main() {
    let source = sqlite3mc_src::source_dir();
    println!("cargo:rerun-if-changed={}", source.display());
    let mut build = cc::Build::new();
    build
        .file(source.join(sqlite3mc_src::SOURCE_FILE))
        .include(source)
        .define("SQLITE_CORE", None)
        .define("SQLITE_THREADSAFE", "1")
        .define("SQLITE_DEFAULT_FOREIGN_KEYS", "1")
        .define("SQLITE_DEFAULT_MEMSTATUS", "0")
        .define("SQLITE_DQS", "0")
        .define("SQLITE_ENABLE_API_ARMOR", None)
        .define("SQLITE_OMIT_DEPRECATED", None)
        .define("SQLITE_OMIT_LOAD_EXTENSION", None)
        .define("SQLITE_USE_URI", "1")
        // Temporary tables and indexes stay in memory, never in a plain file.
        .define("SQLITE_TEMP_STORE", "2")
        .define("SQLITE_SECURE_DELETE", None)
        .define("HAVE_ISNAN", None)
        .warnings(false)
        // libsqlite3-sys asks the linker for `sqlite3`; only the search path is given.
        .cargo_metadata(false);
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        build
            .define("HAVE_USLEEP", "1")
            .define("HAVE_LOCALTIME_R", None);
    }
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
        && env::var("CARGO_CFG_TARGET_FEATURE")
            .is_ok_and(|features| features.contains("crt-static"))
    {
        build.static_crt(true);
    }
    build.compile("sqlite3");
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    println!("cargo:rustc-link-search=native={}", out.display());
}
```

Create `crates/push-sdk/src/lib.rs`:

```rust
//! push-sdk: the client core of push-messenger, which third-party apps embed (spec §8).
//!
//! It owns everything below the user interface: signing up and in through auth-svc,
//! the device's credentials (in a [`SecretStore`] the app provides), the WebSocket
//! connection to the gateway with its reconnects, a local SQLite store of conversations,
//! messages and contacts (encrypted as a whole when the app gives a key), the outbox that
//! sends messages, edits and deletes exactly once, and one stream of events for the UI.
//!
//! The Node and C bindings (Plan 10) wrap this crate and add no logic. The public types
//! are therefore plain data: identifiers, strings, numbers and enums, with no lifetimes
//! or generics.

// The parts arrive one task at a time and the `Client` (Task 10) ties them together; until
// then some crate-private items have no caller outside their tests.
#![allow(dead_code)]

mod error;
mod model;
mod secrets;

pub use error::SdkError;
pub use model::{
    ConnectionState, Contact, ContactState, Conversation, ConversationKind, Event, Invite, Message,
    MessageKind, MessageStatus, OutboxKind,
};
pub use push_domain::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
pub use secrets::{MemorySecretStore, SecretError, SecretStore};

#[cfg(test)]
mod tests {
    /// build.rs compiles SQLite3 Multiple Ciphers and libsqlite3-sys links to it, not to a
    /// system SQLite that would ignore the database key.
    #[test]
    fn the_linked_sqlite_can_encrypt() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        let version: String = connection
            .query_row("SELECT sqlite3mc_version()", [], |row| row.get(0))
            .unwrap();
        assert!(version.starts_with("SQLite3 Multiple Ciphers"), "{version}");
    }
}
```

Create `crates/push-sdk/src/error.rs`:

```rust
//! The SDK's one error type. Every error has a stable, machine-readable [`code`] that
//! bindings pass on as a string, and says whether the same call may succeed later
//! ([`is_retryable`]).
//!
//! [`code`]: SdkError::code
//! [`is_retryable`]: SdkError::is_retryable

use push_proto::v1::ErrorCode;

/// Why an SDK call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SdkError {
    /// The input breaks a rule the server shares (spec §8.3): nothing was stored or sent.
    #[error("invalid: {0}")]
    Invalid(String),
    /// No device is signed in on this client: sign in (again).
    #[error("not signed in")]
    NotSignedIn,
    /// The server revoked this device: it does not reconnect until the app connects
    /// again.
    #[error("this device was removed")]
    DeviceRemoved,
    /// The same app data was opened elsewhere (another window, a copy restored on another
    /// machine) and that copy took over the connection. The device is still signed in;
    /// it does not reconnect until the app connects again.
    #[error("this device was opened elsewhere")]
    OpenedElsewhere,
    /// No connection to the server came up within the request timeout.
    #[error("offline: no connection to the server")]
    Offline,
    /// The connection dropped before the answer came; the request may or may not have
    /// been carried out.
    #[error("the connection dropped before the answer")]
    Disconnected,
    /// The server refused the request. `code` is the gateway's error code
    /// (`not_contact`, `invite_not_found`, …) or auth-svc's (`invalid_credentials`,
    /// `email_not_verified`, …).
    #[error("{code}: {message}")]
    Server {
        code: String,
        message: String,
        retryable: bool,
        /// Rate limited: when the same request may succeed.
        retry_after_s: Option<u32>,
    },
    /// The server could not be reached, or answered something unreadable.
    #[error("network: {0}")]
    Network(String),
    /// The local database failed.
    #[error("local storage: {0}")]
    Storage(String),
    /// The local database is encrypted and the key given is wrong, or none was given
    /// (or a key was given for a database that is not encrypted). Nothing was changed.
    #[error("the database key is wrong or missing")]
    WrongKey,
    /// The app's secret store failed.
    #[error("secret store: {0}")]
    Secrets(String),
    /// The client was closed.
    #[error("the client is closed")]
    Closed,
}

impl SdkError {
    /// The stable code of the error: the server's code for [`SdkError::Server`], else
    /// the variant's name in snake case.
    pub fn code(&self) -> &str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::NotSignedIn => "not_signed_in",
            Self::DeviceRemoved => "device_removed",
            Self::OpenedElsewhere => "opened_elsewhere",
            Self::Offline => "offline",
            Self::Disconnected => "disconnected",
            Self::Server { code, .. } => code,
            Self::Network(_) => "network",
            Self::Storage(_) => "storage",
            Self::WrongKey => "wrong_key",
            Self::Secrets(_) => "secrets",
            Self::Closed => "closed",
        }
    }

    /// Whether the same call may succeed later, unchanged.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Offline | Self::Disconnected | Self::Network(_) => true,
            Self::Server { retryable, .. } => *retryable,
            _ => false,
        }
    }

    /// A gateway `Error` frame.
    pub(crate) fn from_frame(error: &push_proto::v1::Error) -> Self {
        Self::Server {
            code: error_code_name(error.code()),
            message: error.message.clone(),
            retryable: error.retryable,
            retry_after_s: error.retry_after_s,
        }
    }
}

impl From<rusqlite::Error> for SdkError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

/// `ERROR_CODE_NOT_CONTACT` → `not_contact`.
pub(crate) fn error_code_name(code: ErrorCode) -> String {
    code.as_str_name()
        .trim_start_matches("ERROR_CODE_")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_errors_keep_the_server_code_and_say_whether_to_retry() {
        let frame = push_proto::v1::Error {
            code: ErrorCode::RateLimited.into(),
            message: "slow down".to_owned(),
            retryable: true,
            retry_after_s: Some(7),
            ..Default::default()
        };
        let error = SdkError::from_frame(&frame);
        assert_eq!(error.code(), "rate_limited");
        assert!(error.is_retryable());
        assert_eq!(
            error,
            SdkError::Server {
                code: "rate_limited".to_owned(),
                message: "slow down".to_owned(),
                retryable: true,
                retry_after_s: Some(7),
            }
        );
        assert_eq!(error_code_name(ErrorCode::NotContact), "not_contact");
    }

    #[test]
    fn local_errors_have_their_own_codes() {
        assert_eq!(SdkError::Offline.code(), "offline");
        assert!(SdkError::Offline.is_retryable());
        assert_eq!(SdkError::Invalid("empty".to_owned()).code(), "invalid");
        assert!(!SdkError::DeviceRemoved.is_retryable());
        assert_eq!(SdkError::OpenedElsewhere.code(), "opened_elsewhere");
        assert_eq!(SdkError::WrongKey.code(), "wrong_key");
        assert!(!SdkError::WrongKey.is_retryable());
        assert!(!SdkError::NotSignedIn.is_retryable());
    }
}
```

Create `crates/push-sdk/src/model.rs`:

```rust
//! The data the SDK hands to apps: plain values with no lifetimes or generics, so the
//! bindings can copy them across the FFI boundary.
//!
//! Times are microseconds since the Unix epoch, UTC, as on the wire; `seq` numbers are the
//! conversation log's positions (spec §4.8).
//!
//! `Debug` never shows what people wrote: message bodies and group titles appear as their
//! length only, so a `{:?}` in an app's log carries no message content.

use push_domain::{ClientMsgId, ConversationId, MessageId, UserId};
use push_proto::v1;

use crate::SdkError;

/// What kind of conversation it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConversationKind {
    /// Two users who are accepted contacts.
    Direct,
    /// A group. Not offered to apps in the first release: no operation creates one.
    Group,
    /// Learned from a delivery before any listing; the next listing tells.
    Unknown,
}

impl ConversationKind {
    pub(crate) fn code(self) -> i64 {
        match self {
            Self::Unknown => 0,
            Self::Direct => 1,
            Self::Group => 2,
        }
    }

    pub(crate) fn from_code(code: i64) -> Self {
        match code {
            1 => Self::Direct,
            2 => Self::Group,
            _ => Self::Unknown,
        }
    }

    pub(crate) fn from_wire(kind: v1::ConversationKind) -> Self {
        match kind {
            v1::ConversationKind::Direct => Self::Direct,
            v1::ConversationKind::Group => Self::Group,
            v1::ConversationKind::Unspecified => Self::Unknown,
        }
    }
}

/// What `Debug` shows instead of a text: its length.
pub(crate) struct Redacted(pub usize);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "<{} bytes>", self.0)
    }
}

fn redacted(text: &Option<String>) -> Option<Redacted> {
    text.as_ref().map(|text| Redacted(text.len()))
}

/// A conversation as this device knows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    /// The other member of a direct conversation.
    pub peer: Option<UserId>,
    /// Groups only.
    pub title: Option<String>,
    /// The highest `seq` known in the log.
    pub last_seq: i64,
    /// The user's own read position.
    pub read_seq: i64,
    /// Messages from others after `read_seq`, at most 100 ("99+").
    pub unread: u32,
    /// Direct: everything up to here reached one of the peer's devices.
    pub peer_delivered_seq: i64,
    /// Direct: everything up to here was read by the peer.
    pub peer_read_seq: i64,
    /// When it was created or last appended to.
    pub updated_at: i64,
}

impl std::fmt::Debug for Conversation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Conversation")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("peer", &self.peer)
            .field("title", &redacted(&self.title))
            .field("last_seq", &self.last_seq)
            .field("read_seq", &self.read_seq)
            .field("unread", &self.unread)
            .field("peer_delivered_seq", &self.peer_delivered_seq)
            .field("peer_read_seq", &self.peer_read_seq)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// What a message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageKind {
    /// Text from a user.
    Text,
    /// A system entry: its body is a small JSON document (spec §6).
    System,
}

/// Where one of the user's own messages stands (spec §4.4). Others' messages are
/// [`MessageStatus::Received`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MessageStatus {
    /// In the outbox, not stored by the server yet.
    Pending,
    /// Stored by the server.
    Sent,
    /// Reached one of the peer's devices.
    Delivered,
    /// The peer read it.
    Read,
    /// Someone else's message.
    Received,
}

/// A message in a conversation, in its current state.
#[derive(Clone, PartialEq, Eq)]
pub struct Message {
    pub conversation_id: ConversationId,
    /// Chosen by the sending device; stable from the moment it was queued.
    pub client_msg_id: ClientMsgId,
    /// The server's id, once stored.
    pub message_id: Option<MessageId>,
    /// The position in the log, once stored.
    pub seq: Option<i64>,
    pub sender_id: UserId,
    pub kind: MessageKind,
    /// The text; `None` once deleted.
    pub body: Option<String>,
    pub client_ts: Option<i64>,
    pub server_ts: Option<i64>,
    pub edited_at: Option<i64>,
    pub deleted: bool,
    pub status: MessageStatus,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Message")
            .field("conversation_id", &self.conversation_id)
            .field("client_msg_id", &self.client_msg_id)
            .field("message_id", &self.message_id)
            .field("seq", &self.seq)
            .field("sender_id", &self.sender_id)
            .field("kind", &self.kind)
            .field("body", &redacted(&self.body))
            .field("client_ts", &self.client_ts)
            .field("server_ts", &self.server_ts)
            .field("edited_at", &self.edited_at)
            .field("deleted", &self.deleted)
            .field("status", &self.status)
            .finish()
    }
}

/// The user's relationship with another user, as the user sees it (spec §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContactState {
    None,
    /// The user asked them.
    RequestedOut,
    /// They asked the user.
    RequestedIn,
    Accepted,
    /// The user blocked them.
    Blocked,
}

impl ContactState {
    pub(crate) fn from_wire(state: v1::ContactState) -> Self {
        match state {
            v1::ContactState::None => Self::None,
            v1::ContactState::RequestedOut => Self::RequestedOut,
            v1::ContactState::RequestedIn => Self::RequestedIn,
            v1::ContactState::Accepted => Self::Accepted,
            v1::ContactState::Blocked => Self::Blocked,
        }
    }

    pub(crate) fn code(self) -> i64 {
        match self {
            Self::None => 0,
            Self::RequestedOut => 1,
            Self::RequestedIn => 2,
            Self::Accepted => 3,
            Self::Blocked => 4,
        }
    }

    pub(crate) fn from_code(code: i64) -> Self {
        match code {
            1 => Self::RequestedOut,
            2 => Self::RequestedIn,
            3 => Self::Accepted,
            4 => Self::Blocked,
            _ => Self::None,
        }
    }
}

/// One of the user's relationships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Contact {
    pub user_id: UserId,
    pub state: ContactState,
}

/// An invite code to share as a link or QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    /// Ten characters of Crockford base32.
    pub code: String,
    pub expires_at: i64,
    pub max_uses: u32,
    pub uses: u32,
}

/// What an outbox entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutboxKind {
    Send,
    Edit,
    Delete,
}

impl OutboxKind {
    pub(crate) fn code(self) -> i64 {
        match self {
            Self::Send => 1,
            Self::Edit => 2,
            Self::Delete => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Self {
        match code {
            2 => Self::Edit,
            3 => Self::Delete,
            _ => Self::Send,
        }
    }
}

/// The connection to the gateway (spec §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    /// Not connecting: the app has not called `connect`, or called `disconnect`.
    Offline,
    /// Opening the socket and identifying.
    Connecting,
    /// `Ready`: deliveries flow and the outbox sends.
    Connected,
    /// Waiting before the next attempt, after attempt number `attempt` failed.
    Reconnecting { attempt: u32, retry_in_ms: u64 },
    /// The credentials are no longer accepted (a password reset, or the refresh token was
    /// refused): sign in again. Local data and the outbox are kept for the same user.
    SignedOut,
    /// The server revoked this device (close code 4003): the client stopped and
    /// reconnects only when the app calls `connect` again. Local data is kept.
    DeviceRemoved,
    /// Another copy of this app's data took over the device's connection (close code
    /// 4007): the same app opened twice, or restored on a second machine. The device is
    /// still signed in and local data is kept; the client stopped and reconnects only
    /// when the app calls `connect` again (which takes the connection back).
    OpenedElsewhere,
}

/// What the SDK tells the UI, in one ordered stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    ConnectionStateChanged(ConnectionState),
    /// A message the UI has not seen: the user's own just queued (the local echo), or one
    /// delivered.
    MessageAdded(Message),
    /// One of the user's messages was stored by the server: it has its `seq` now.
    MessageSent(Message),
    MessageEdited(Message),
    /// A message was deleted: its body is gone.
    MessageDeleted(Message),
    /// The server refused a send, edit or delete for good (not a contact, the time
    /// window passed, …). It left the outbox; a refused message's local echo is removed.
    SendFailed {
        conversation_id: ConversationId,
        client_msg_id: ClientMsgId,
        kind: OutboxKind,
        error: SdkError,
    },
    /// The peer's positions moved: the user's messages up to them are delivered or read.
    ReceiptChanged {
        conversation_id: ConversationId,
        peer_delivered_seq: i64,
        peer_read_seq: i64,
    },
    /// A conversation appeared or its summary changed (last message, read position,
    /// unread count).
    ConversationChanged(Conversation),
    ContactChanged(Contact),
    /// Sign in again (a password reset, or the credentials were refused). Also reported
    /// as [`ConnectionState::SignedOut`].
    SignedOut,
    /// This device was removed. Also reported as [`ConnectionState::DeviceRemoved`].
    DeviceRemoved,
    /// This device's data was opened elsewhere. Also reported as
    /// [`ConnectionState::OpenedElsewhere`].
    OpenedElsewhere,
    /// The app stopped reading events and the queue filled up (10,000 events): newer ones
    /// were dropped, and this event stands in their place. Nothing was lost from the
    /// store: reload what the UI shows from the client's local data (conversations,
    /// messages, contacts).
    EventsLost,
}
```

Create `crates/push-sdk/src/secrets.rs`:

```rust
//! Where the SDK keeps the device's credentials: a store the app provides, such as the
//! platform's keychain. The SDK never writes tokens anywhere else, never logs them, and
//! leaves them out of every `Debug` output.

use std::collections::HashMap;
use std::sync::Mutex;

use push_domain::{DeviceId, UserId};
use serde::{Deserialize, Serialize};

/// The key of the signed-in device's credentials.
pub(crate) const SESSION_KEY: &str = "push.session";
/// The key of a device registration that has not been answered yet: its
/// `registration_id`, reused when the registration is retried (spec §5.3).
pub(crate) const REGISTRATION_KEY: &str = "push.registration";

/// A failure of the app's secret store, in its own words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SecretError(pub String);

/// The app's secret store: a few small values under fixed keys. On a phone this is the
/// keychain or keystore; on a desktop the credential manager.
///
/// The methods are blocking and called from SDK threads; they should return quickly.
/// Values are opaque bytes; the SDK writes the credentials before it uses them, so a
/// store that fails to save never leaves the SDK holding tokens it could lose.
pub trait SecretStore: Send + Sync {
    /// The value under `key`, if any.
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError>;
    /// Replaces the value under `key`.
    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError>;
    /// Removes the value under `key`; removing a missing key succeeds.
    fn delete(&self, key: &str) -> Result<(), SecretError>;
}

/// A secret store in memory, lost when the process ends: for tests and tools.
#[derive(Default)]
pub struct MemorySecretStore {
    values: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for MemorySecretStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MemorySecretStore { .. }")
    }
}

impl SecretStore for MemorySecretStore {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError> {
        Ok(self.values.lock().expect("not poisoned").get(key).cloned())
    }

    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError> {
        self.values
            .lock()
            .expect("not poisoned")
            .insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.values.lock().expect("not poisoned").remove(key);
        Ok(())
    }
}

/// A signed-in device's credentials, as kept in the secret store.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Session {
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub access_token: String,
    /// When the access token expires, in seconds since the Unix epoch.
    pub access_expires_at: i64,
    pub refresh_token: String,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Session")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("access_token", &"<redacted>")
            .field("access_expires_at", &self.access_expires_at)
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

impl Session {
    pub fn load(store: &dyn SecretStore) -> Result<Option<Self>, SecretError> {
        match store.load(SESSION_KEY)? {
            // An unreadable value is no session: the user signs in again.
            Some(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            None => Ok(None),
        }
    }

    pub fn save(&self, store: &dyn SecretStore) -> Result<(), SecretError> {
        let bytes = serde_json::to_vec(self).map_err(|error| SecretError(error.to_string()))?;
        store.save(SESSION_KEY, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            user_id: UserId::new(),
            device_id: DeviceId::new(),
            access_token: "access-token-value".to_owned(),
            access_expires_at: 1_900_000_000,
            refresh_token: "refresh-token-value".to_owned(),
        }
    }

    #[test]
    fn a_session_survives_the_store() {
        let store = MemorySecretStore::new();
        assert_eq!(Session::load(&store), Ok(None));
        let saved = session();
        saved.save(&store).unwrap();
        assert_eq!(Session::load(&store), Ok(Some(saved)));
        store.delete(SESSION_KEY).unwrap();
        store.delete(SESSION_KEY).unwrap();
        assert_eq!(Session::load(&store), Ok(None));
    }

    #[test]
    fn an_unreadable_session_is_no_session() {
        let store = MemorySecretStore::new();
        store.save(SESSION_KEY, b"not json").unwrap();
        assert_eq!(Session::load(&store), Ok(None));
    }

    #[test]
    fn debug_output_never_shows_a_token() {
        let shown = format!("{:?}", session());
        assert!(!shown.contains("access-token-value"));
        assert!(!shown.contains("refresh-token-value"));
        assert!(shown.contains("access_expires_at"));
    }
}
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-sdk/src/secrets.rs`, change `.field("access_token", &"<redacted>")` to
`.field("access_token", &self.access_token)` and run:

```powershell
cargo test -p push-sdk --lib --locked secrets
```

Expected (the first run compiles SQLite3 Multiple Ciphers):
`test secrets::tests::debug_output_never_shows_a_token ... FAILED` and
`test result: FAILED. 2 passed; 1 failed`. Undo the edit and run it again: 3 passed.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A Cargo.toml Cargo.lock crates
git commit -m "push-sdk: the crate, its errors, its types and the secret store; SQLite3 Multiple Ciphers; ids parse"
```

Expected: 881 tests pass (1 more in `push-domain`, 6 in `push-sdk`), no warnings, no
diff; the 1.85 build compiles `rusqlite` 0.32.1 and SQLite3 Multiple Ciphers with MSVC.

---

### Task 3: auth-svc's API and the device's credentials

**Files:**
- Create: `crates/push-sdk/src/auth.rs`, `crates/push-sdk/src/credentials.rs`
- Modify: `crates/push-sdk/src/lib.rs`

Decision 5. `AuthApi` covers every public endpoint the SDK needs — sign-up, verification
resend, password forgot, sign-in (with and without a device), device registration,
refresh, device list and revocation, profiles by id, handle lookup — and retries only what
is safe to repeat; a refresh's attempts are cut off at 8 s each and all end within 25 s of
the first (`Attempts::REFRESH`). Errors keep auth-svc's `code` and `message` and never
quote a body. `Credentials` serialises refreshes (a second caller with the same stale token
gets the first caller's result), saves before use, and signs out on a refused refresh;
`sign_out` waits for a refresh in flight, so that none saves the session again after it.

- [ ] **Step 1: Make the changes**

Create `crates/push-sdk/src/auth.rs`:

```rust
//! auth-svc's public HTTP API (spec §5.2–§5.6), as the SDK calls it.
//!
//! Requests that are safe to repeat are retried up to three times on transport errors and
//! on 502, 503 and 504, after 0.5, 1 and 2 s: registering a device (the `registration_id`
//! makes it idempotent), reads, revocations and refreshing. auth-svc accepts a rotated
//! refresh token again only for 30 s, so a refresh attempt may take 8 s at most and no
//! attempt starts that could end later than 25 s after the first one. Signing up, signing
//! in and the email requests are never repeated by the SDK: they count against rate
//! limits.
//!
//! Nothing here logs or formats a token, a password or a response body: errors quote only
//! auth-svc's error code and message.

use std::time::Duration;

use push_domain::{DeviceId, UserId};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::SdkError;

/// How long one HTTP request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// How long one refresh attempt may take.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(8);
/// Every refresh attempt ends within this long of the first, inside auth-svc's 30 s grace
/// for a rotated refresh token.
const REFRESH_WINDOW: Duration = Duration::from_secs(25);
/// Delays before the retries of a request that is safe to repeat.
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// The kind of device, as auth-svc records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Ios,
    Android,
    Web,
    Desktop,
    Other,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Android => "android",
            Self::Web => "web",
            Self::Desktop => "desktop",
            Self::Other => "other",
        }
    }
}

/// A user's public profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub user_id: UserId,
    pub handle: String,
    pub display_name: String,
}

/// One of the user's devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub device_id: DeviceId,
    pub name: String,
    pub platform: String,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339.
    pub last_seen_at: Option<String>,
    /// The device this client runs on.
    pub current: bool,
}

/// What auth-svc answers when it signs a device in.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Tokens {
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub access_token: String,
    pub expires_in_s: i64,
    pub refresh_token: String,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Tokens")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("expires_in_s", &self.expires_in_s)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokensBody {
    user_id: Uuid,
    device_id: Uuid,
    access_token: String,
    expires_in_s: i64,
    refresh_token: String,
}

impl From<TokensBody> for Tokens {
    fn from(body: TokensBody) -> Self {
        Self {
            user_id: UserId(body.user_id),
            device_id: DeviceId(body.device_id),
            access_token: body.access_token,
            expires_in_s: body.expires_in_s,
            refresh_token: body.refresh_token,
        }
    }
}

#[derive(Deserialize)]
struct ProfileBody {
    user_id: Uuid,
    handle: String,
    display_name: String,
}

impl From<ProfileBody> for Profile {
    fn from(body: ProfileBody) -> Self {
        Self {
            user_id: UserId(body.user_id),
            handle: body.handle,
            display_name: body.display_name,
        }
    }
}

/// auth-svc, reached at `base_url` (`https://push.example`), with `host` as the `Host`
/// header when the URL names a load balancer rather than the virtual host it routes.
#[derive(Clone)]
pub(crate) struct AuthApi {
    http: reqwest::Client,
    base_url: String,
    host: Option<String>,
    refresh_attempts: Attempts,
}

/// How long each attempt of a request may take, and the time from the first attempt by
/// which every attempt must have ended.
#[derive(Debug, Clone, Copy)]
struct Attempts {
    timeout: Duration,
    window: Option<Duration>,
}

impl Attempts {
    const ORDINARY: Self = Self {
        timeout: REQUEST_TIMEOUT,
        window: None,
    };
    const REFRESH: Self = Self {
        timeout: REFRESH_TIMEOUT,
        window: Some(REFRESH_WINDOW),
    };
}

impl AuthApi {
    pub fn new(base_url: &str, host: Option<&str>) -> Result<Self, SdkError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .no_proxy()
            .build()
            .map_err(|error| SdkError::Network(error.to_string()))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            host: host.map(str::to_owned),
            refresh_attempts: Attempts::REFRESH,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let request = self
            .http
            .request(method, format!("{}{path}", self.base_url));
        match &self.host {
            Some(host) => request.header(reqwest::header::HOST, host),
            None => request,
        }
    }

    /// `POST /v1/auth/signup`: answered the same whether or not the email is known.
    pub async fn sign_up(
        &self,
        email: &str,
        password: &str,
        handle: &str,
        display_name: &str,
    ) -> Result<(), SdkError> {
        let body = json!({
            "email": email,
            "password": password,
            "handle": handle,
            "display_name": display_name,
        });
        once(self.request(Method::POST, "/v1/auth/signup").json(&body)).await?;
        Ok(())
    }

    /// `POST /v1/auth/verify/resend`.
    pub async fn resend_verification(&self, email: &str) -> Result<(), SdkError> {
        let body = json!({ "email": email });
        once(
            self.request(Method::POST, "/v1/auth/verify/resend")
                .json(&body),
        )
        .await?;
        Ok(())
    }

    /// `POST /v1/auth/password/forgot`.
    pub async fn forgot_password(&self, email: &str) -> Result<(), SdkError> {
        let body = json!({ "email": email });
        once(
            self.request(Method::POST, "/v1/auth/password/forgot")
                .json(&body),
        )
        .await?;
        Ok(())
    }

    /// `POST /v1/auth/login`: a registration token, good for registering one device.
    pub async fn login(&self, email: &str, password: &str) -> Result<String, SdkError> {
        let body = json!({ "email": email, "password": password });
        let answer = once(self.request(Method::POST, "/v1/auth/login").json(&body)).await?;
        answer["registration_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| unreadable("the sign-in answer"))
    }

    /// `POST /v1/auth/login` with an active device: its new tokens.
    pub async fn login_device(
        &self,
        email: &str,
        password: &str,
        device: DeviceId,
    ) -> Result<Tokens, SdkError> {
        let body = json!({ "email": email, "password": password, "device_id": device });
        let answer = once(self.request(Method::POST, "/v1/auth/login").json(&body)).await?;
        tokens(answer)
    }

    /// `POST /v1/devices`: registers a device, or finds the one an earlier attempt with
    /// the same `registration_id` registered.
    pub async fn register_device(
        &self,
        registration_token: &str,
        registration_id: Uuid,
        name: &str,
        platform: Platform,
    ) -> Result<Tokens, SdkError> {
        let body = json!({
            "name": name,
            "platform": platform.as_str(),
            "registration_id": registration_id,
        });
        let answer = retried(Attempts::ORDINARY, || {
            self.request(Method::POST, "/v1/devices")
                .bearer_auth(registration_token)
                .json(&body)
        })
        .await?;
        tokens(answer)
    }

    /// `POST /v1/auth/refresh`: new tokens; the refresh token is rotated.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, SdkError> {
        let body = json!({ "refresh_token": refresh_token });
        let answer = retried(self.refresh_attempts, || {
            self.request(Method::POST, "/v1/auth/refresh").json(&body)
        })
        .await?;
        tokens(answer)
    }

    /// `GET /v1/devices`.
    pub async fn devices(&self, access_token: &str) -> Result<Vec<Device>, SdkError> {
        #[derive(Deserialize)]
        struct DeviceBody {
            device_id: Uuid,
            name: String,
            platform: String,
            created_at: String,
            last_seen_at: Option<String>,
            current: bool,
        }
        #[derive(Deserialize)]
        struct DevicesBody {
            devices: Vec<DeviceBody>,
        }
        let answer = retried(Attempts::ORDINARY, || {
            self.request(Method::GET, "/v1/devices")
                .bearer_auth(access_token)
        })
        .await?;
        let body: DevicesBody =
            serde_json::from_value(answer).map_err(|_| unreadable("the device list"))?;
        Ok(body
            .devices
            .into_iter()
            .map(|device| Device {
                device_id: DeviceId(device.device_id),
                name: device.name,
                platform: device.platform,
                created_at: device.created_at,
                last_seen_at: device.last_seen_at,
                current: device.current,
            })
            .collect())
    }

    /// `DELETE /v1/devices/{id}`.
    pub async fn revoke_device(
        &self,
        access_token: &str,
        device: DeviceId,
    ) -> Result<(), SdkError> {
        let path = format!("/v1/devices/{device}");
        retried(Attempts::ORDINARY, || {
            self.request(Method::DELETE, &path)
                .bearer_auth(access_token)
        })
        .await?;
        Ok(())
    }

    /// `GET /v1/users?ids=…`: the profiles among `ids` the caller may see.
    pub async fn profiles(
        &self,
        access_token: &str,
        ids: &[UserId],
    ) -> Result<Vec<Profile>, SdkError> {
        #[derive(Deserialize)]
        struct UsersBody {
            users: Vec<ProfileBody>,
        }
        let ids: Vec<String> = ids.iter().map(UserId::to_string).collect();
        let ids = ids.join(",");
        let answer = retried(Attempts::ORDINARY, || {
            self.request(Method::GET, "/v1/users")
                .query(&[("ids", ids.as_str())])
                .bearer_auth(access_token)
        })
        .await?;
        let body: UsersBody =
            serde_json::from_value(answer).map_err(|_| unreadable("the profiles"))?;
        Ok(body.users.into_iter().map(Profile::from).collect())
    }

    /// `GET /v1/users/lookup?handle=…`.
    pub async fn lookup(&self, access_token: &str, handle: &str) -> Result<Profile, SdkError> {
        let answer = retried(Attempts::ORDINARY, || {
            self.request(Method::GET, "/v1/users/lookup")
                .query(&[("handle", handle)])
                .bearer_auth(access_token)
        })
        .await?;
        serde_json::from_value::<ProfileBody>(answer)
            .map(Profile::from)
            .map_err(|_| unreadable("the profile"))
    }
}

fn tokens(answer: Value) -> Result<Tokens, SdkError> {
    serde_json::from_value::<TokensBody>(answer)
        .map(Tokens::from)
        .map_err(|_| unreadable("the tokens"))
}

fn unreadable(what: &str) -> SdkError {
    SdkError::Network(format!("auth-svc answered {what} in an unexpected form"))
}

/// Sends a request once; its JSON answer (`null` for an empty one).
async fn once(request: RequestBuilder) -> Result<Value, SdkError> {
    let response = request
        .send()
        .await
        .map_err(|error| SdkError::Network(error.without_url().to_string()))?;
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let body = response
        .bytes()
        .await
        .map_err(|error| SdkError::Network(error.without_url().to_string()))?;
    if status.is_success() {
        return Ok(serde_json::from_slice(&body).unwrap_or(Value::Null));
    }
    Err(refusal(status, &body, retry_after))
}

/// Sends a request that is safe to repeat, retrying transport errors and 502, 503 and
/// 504 a few times, each attempt within `attempts.timeout`; with a window, no attempt
/// starts that could end after it.
async fn retried(
    attempts: Attempts,
    request: impl Fn() -> RequestBuilder,
) -> Result<Value, SdkError> {
    let started = tokio::time::Instant::now();
    let mut delays = RETRY_DELAYS.iter();
    loop {
        match once(request().timeout(attempts.timeout)).await {
            Err(error) if is_transient(&error) => match delays.next() {
                Some(delay)
                    if attempts.window.is_none_or(|window| {
                        started.elapsed() + *delay + attempts.timeout <= window
                    }) =>
                {
                    tokio::time::sleep(*delay).await;
                }
                _ => return Err(error),
            },
            answer => return answer,
        }
    }
}

fn is_transient(error: &SdkError) -> bool {
    match error {
        SdkError::Network(_) => true,
        SdkError::Server { code, .. } => {
            matches!(
                code.as_str(),
                "unavailable" | "http_502" | "http_503" | "http_504"
            )
        }
        _ => false,
    }
}

/// An error answer: auth-svc's `{"error": {"code", "message", "retryable"}}`, or the
/// status alone when the body is something else (a proxy's page). The body is never
/// quoted otherwise.
fn refusal(status: StatusCode, body: &[u8], retry_after_s: Option<u32>) -> SdkError {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: ErrorDetail,
    }
    #[derive(Deserialize)]
    struct ErrorDetail {
        code: String,
        message: String,
        #[serde(default)]
        retryable: bool,
    }
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(ErrorBody { error }) => SdkError::Server {
            code: error.code,
            message: error.message,
            retryable: error.retryable,
            retry_after_s,
        },
        Err(_) => SdkError::Server {
            code: format!("http_{}", status.as_u16()),
            message: format!("auth-svc answered {status}"),
            retryable: matches!(status.as_u16(), 502..=504),
            retry_after_s,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode as Status};
    use axum::response::IntoResponse;
    use axum::routing::post;

    use super::*;

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
    }

    fn tokens_json() -> Value {
        json!({
            "user_id": Uuid::now_v7(),
            "device_id": Uuid::now_v7(),
            "token_type": "Bearer",
            "access_token": "access-1",
            "expires_in_s": 900,
            "refresh_token": "refresh-1",
            "refresh_expires_in_s": 2_592_000,
        })
    }

    #[tokio::test]
    async fn a_refresh_is_retried_while_the_service_is_unavailable() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let router = Router::new().route(
            "/v1/auth/refresh",
            post(move || {
                let counter = counter.clone();
                async move {
                    if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                        let error = json!({"error": {"code": "unavailable", "message": "try again", "retryable": true}});
                        (Status::SERVICE_UNAVAILABLE, axum::Json(error)).into_response()
                    } else {
                        axum::Json(tokens_json()).into_response()
                    }
                }
            }),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        let tokens = api.refresh("refresh-0").await.unwrap();
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(!format!("{tokens:?}").contains("access-1"));
    }

    /// A refresh retry must not reach auth-svc after its grace for the rotated token: each
    /// attempt is cut off, and none starts that could end after the window.
    #[tokio::test]
    async fn refresh_attempts_end_within_the_window() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let router = Router::new().route(
            "/v1/auth/refresh",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    axum::Json(tokens_json()).into_response()
                }
            }),
        );
        let mut api = AuthApi::new(&serve(router).await, None).unwrap();
        // The real 8 s and 25 s, scaled down: two attempts fit, a third would end late.
        api.refresh_attempts = Attempts {
            timeout: Duration::from_millis(200),
            window: Some(Duration::from_millis(1_000)),
        };
        let started = std::time::Instant::now();
        let error = api.refresh("refresh-0").await.unwrap_err();
        assert_eq!(error.code(), "network");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() < Duration::from_millis(1_000));
    }

    #[tokio::test]
    async fn a_sign_in_is_never_repeated_and_errors_keep_the_code() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let router = Router::new().route(
            "/v1/auth/login",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let error = json!({"error": {"code": "unavailable", "message": "busy", "retryable": true}});
                    (Status::SERVICE_UNAVAILABLE, [("retry-after", "1")], axum::Json(error))
                }
            }),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        let error = api.login("a@example.com", "password").await.unwrap_err();
        assert_eq!(
            error,
            SdkError::Server {
                code: "unavailable".to_owned(),
                message: "busy".to_owned(),
                retryable: true,
                retry_after_s: Some(1),
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_body_that_is_not_an_api_error_is_never_quoted() {
        let router = Router::new().route(
            "/v1/auth/refresh",
            post(|| async { (Status::UNAUTHORIZED, "secret-looking body") }),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        let error = api.refresh("refresh-0").await.unwrap_err();
        assert_eq!(error.code(), "http_401");
        assert!(!error.to_string().contains("secret-looking"));
    }

    #[tokio::test]
    async fn the_host_override_and_the_bearer_token_are_sent() {
        let router = Router::new().route(
            "/v1/devices",
            post(|headers: HeaderMap| async move {
                let host = headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let bearer = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if host == "push.local" && bearer == "Bearer registration-1" {
                    (Status::CREATED, axum::Json(tokens_json())).into_response()
                } else {
                    Status::BAD_REQUEST.into_response()
                }
            }),
        );
        let api = AuthApi::new(&serve(router).await, Some("push.local")).unwrap();
        let tokens = api
            .register_device("registration-1", Uuid::now_v7(), "phone", Platform::Ios)
            .await
            .unwrap();
        assert_eq!(tokens.refresh_token, "refresh-1");
    }
}
```

Create `crates/push-sdk/src/credentials.rs`:

```rust
//! The signed-in device's credentials (spec §5.1, §5.3): kept in the app's secret store,
//! refreshed before they expire, and saved before they are used.
//!
//! Refreshes are serialised, so a refresh token is never presented twice by this client:
//! auth-svc rotates it on every refresh and treats a second use after its 30 s grace as
//! theft, revoking the device's tokens. A caller that saw an expired access token asks
//! for a refresh naming it; if another caller refreshed meanwhile, the newer token is
//! returned without a second refresh.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use crate::SdkError;
use crate::auth::{AuthApi, Tokens};
use crate::secrets::{REGISTRATION_KEY, SESSION_KEY, SecretStore, Session};

/// An access token with less than this left is refreshed before it is used: the gateway
/// warns 60 s before expiry, and a token used for a connection should outlast that.
pub(crate) const MIN_ACCESS_LIFETIME_S: i64 = 90;

pub(crate) struct Credentials {
    secrets: Arc<dyn SecretStore>,
    auth: AuthApi,
    refreshing: tokio::sync::Mutex<()>,
}

impl Credentials {
    pub fn new(secrets: Arc<dyn SecretStore>, auth: AuthApi) -> Self {
        Self {
            secrets,
            auth,
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    /// The stored session, if a device is signed in.
    pub fn session(&self) -> Result<Option<Session>, SdkError> {
        Session::load(self.secrets.as_ref()).map_err(|error| SdkError::Secrets(error.0))
    }

    /// Keeps the tokens of a fresh sign-in.
    pub fn sign_in(&self, tokens: Tokens) -> Result<Session, SdkError> {
        let session = Session {
            user_id: tokens.user_id,
            device_id: tokens.device_id,
            access_token: tokens.access_token,
            access_expires_at: unix_now() + tokens.expires_in_s,
            refresh_token: tokens.refresh_token,
        };
        self.save(&session)?;
        Ok(session)
    }

    /// Forgets the session. Waits for a refresh in flight, so that it cannot save the
    /// session again afterwards.
    pub async fn sign_out(&self) -> Result<(), SdkError> {
        let _refreshing = self.refreshing.lock().await;
        self.forget_session()
    }

    fn forget_session(&self) -> Result<(), SdkError> {
        self.secrets
            .delete(SESSION_KEY)
            .map_err(|error| SdkError::Secrets(error.0))
    }

    /// A session whose access token is good for at least [`MIN_ACCESS_LIFETIME_S`],
    /// refreshed first if needed. [`SdkError::NotSignedIn`] when no device is signed in
    /// or auth-svc refuses the refresh token (the session is forgotten then).
    pub async fn fresh(&self) -> Result<Session, SdkError> {
        let _refreshing = self.refreshing.lock().await;
        let session = self.session()?.ok_or(SdkError::NotSignedIn)?;
        if session.access_expires_at - unix_now() >= MIN_ACCESS_LIFETIME_S {
            return Ok(session);
        }
        self.refresh_locked(session).await
    }

    /// A session with an access token other than `stale`, which the server refused or
    /// is about to expire: refreshed now unless another caller already did.
    pub async fn refresh(&self, stale: &str) -> Result<Session, SdkError> {
        let _refreshing = self.refreshing.lock().await;
        let session = self.session()?.ok_or(SdkError::NotSignedIn)?;
        if session.access_token != stale {
            return Ok(session);
        }
        self.refresh_locked(session).await
    }

    async fn refresh_locked(&self, session: Session) -> Result<Session, SdkError> {
        match self.auth.refresh(&session.refresh_token).await {
            Ok(tokens)
                if tokens.user_id == session.user_id && tokens.device_id == session.device_id =>
            {
                // Saved before it is used: a token the store lost could not be refreshed.
                self.sign_in(tokens)
            }
            Ok(_) => Err(SdkError::Network(
                "auth-svc refreshed another device's session".to_owned(),
            )),
            Err(error) if is_refused(&error) => {
                tracing::info!(
                    code = error.code(),
                    "the refresh token was refused: signed out"
                );
                self.forget_session()?;
                Err(SdkError::NotSignedIn)
            }
            Err(error) => Err(error),
        }
    }

    fn save(&self, session: &Session) -> Result<(), SdkError> {
        session
            .save(self.secrets.as_ref())
            .map_err(|error| SdkError::Secrets(error.0))
    }

    /// The `registration_id` of the device registration in progress, created and kept
    /// before the first attempt so that every retry, even after a restart, names it.
    pub fn registration_id(&self) -> Result<Uuid, SdkError> {
        let stored = self
            .secrets
            .load(REGISTRATION_KEY)
            .map_err(|error| SdkError::Secrets(error.0))?
            .and_then(|bytes| Uuid::from_slice(&bytes).ok());
        if let Some(id) = stored {
            return Ok(id);
        }
        let id = Uuid::now_v7();
        self.secrets
            .save(REGISTRATION_KEY, id.as_bytes())
            .map_err(|error| SdkError::Secrets(error.0))?;
        Ok(id)
    }

    /// The registration was answered: the next one is a new device.
    pub fn registration_done(&self) -> Result<(), SdkError> {
        self.secrets
            .delete(REGISTRATION_KEY)
            .map_err(|error| SdkError::Secrets(error.0))
    }
}

/// auth-svc refused the credentials for good: they were revoked, reused or expired.
pub(crate) fn is_refused(error: &SdkError) -> bool {
    matches!(error, SdkError::Server { code, .. }
        if matches!(code.as_str(), "unauthorized" | "token_expired" | "http_401"))
}

pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use push_domain::{DeviceId, UserId};
    use serde_json::json;

    use super::*;
    use crate::MemorySecretStore;

    struct Fixture {
        credentials: Arc<Credentials>,
        secrets: Arc<MemorySecretStore>,
        calls: Arc<AtomicUsize>,
        session: Session,
    }

    /// auth-svc answering refreshes with new tokens for the same device (or refusing
    /// them), and a stored session whose access token expires in `left` seconds.
    async fn fixture(left: i64, refuse: bool) -> Fixture {
        let session = Session {
            user_id: UserId::new(),
            device_id: DeviceId::new(),
            access_token: "access-0".to_owned(),
            access_expires_at: unix_now() + left,
            refresh_token: "refresh-0".to_owned(),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let (counter, user, device) = (calls.clone(), session.user_id, session.device_id);
        let router = Router::new().route(
            "/v1/auth/refresh",
            post(move || {
                let counter = counter.clone();
                async move {
                    let call = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    // Slow enough for a concurrent caller to queue behind it.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    if refuse {
                        let error = json!({"error": {"code": "unauthorized", "message": "sign in again", "retryable": false}});
                        return (StatusCode::UNAUTHORIZED, axum::Json(error)).into_response();
                    }
                    axum::Json(json!({
                        "user_id": user,
                        "device_id": device,
                        "access_token": format!("access-{call}"),
                        "expires_in_s": 900,
                        "refresh_token": format!("refresh-{call}"),
                    }))
                    .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let secrets = Arc::new(MemorySecretStore::new());
        session.save(secrets.as_ref()).unwrap();
        let credentials = Arc::new(Credentials::new(
            secrets.clone(),
            AuthApi::new(&url, None).unwrap(),
        ));
        Fixture {
            credentials,
            secrets,
            calls,
            session,
        }
    }

    #[tokio::test]
    async fn a_fresh_token_is_used_as_it_is() {
        let fixture = fixture(600, false).await;
        let session = fixture.credentials.fresh().await.unwrap();
        assert_eq!(session, fixture.session);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_expiring_token_is_refreshed_and_saved_before_it_is_used() {
        let fixture = fixture(30, false).await;
        let session = fixture.credentials.fresh().await.unwrap();
        assert_eq!(session.access_token, "access-1");
        assert_eq!(session.refresh_token, "refresh-1");
        assert!(session.access_expires_at >= unix_now() + 890);
        let stored = Session::load(fixture.secrets.as_ref()).unwrap();
        assert_eq!(stored, Some(session));
    }

    #[tokio::test]
    async fn concurrent_refreshes_of_one_token_refresh_once() {
        let fixture = fixture(600, false).await;
        let (first, second) = tokio::join!(
            fixture.credentials.refresh("access-0"),
            fixture.credentials.refresh("access-0"),
        );
        assert_eq!(first.unwrap().access_token, "access-1");
        assert_eq!(second.unwrap().access_token, "access-1");
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_refused_refresh_signs_out() {
        let fixture = fixture(30, true).await;
        assert_eq!(
            fixture.credentials.fresh().await,
            Err(SdkError::NotSignedIn)
        );
        assert_eq!(Session::load(fixture.secrets.as_ref()), Ok(None));
        assert_eq!(
            fixture.credentials.fresh().await,
            Err(SdkError::NotSignedIn)
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_registration_keeps_its_id_until_it_is_answered() {
        let fixture = fixture(600, false).await;
        let first = fixture.credentials.registration_id().unwrap();
        assert_eq!(fixture.credentials.registration_id().unwrap(), first);
        fixture.credentials.registration_done().unwrap();
        assert_ne!(fixture.credentials.registration_id().unwrap(), first);
    }
}
```

Change `crates/push-sdk/src/lib.rs`:

```diff
diff --git a/crates/push-sdk/src/lib.rs b/crates/push-sdk/src/lib.rs
index 8405c45..bf0141f 100644
--- a/crates/push-sdk/src/lib.rs
+++ b/crates/push-sdk/src/lib.rs
@@ -14,10 +14,13 @@
 // then some crate-private items have no caller outside their tests.
 #![allow(dead_code)]
 
+mod auth;
+mod credentials;
 mod error;
 mod model;
 mod secrets;
 
+pub use auth::{Device, Platform, Profile};
 pub use error::SdkError;
 pub use model::{
     ConnectionState, Contact, ContactState, Conversation, ConversationKind, Event, Invite, Message,
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-sdk/src/auth.rs`, in `retried`, change
`if attempts.window.is_none_or(|window| {` to
`if true || attempts.window.is_none_or(|window| {` and run:

```powershell
cargo test -p push-sdk --lib --locked auth
```

Expected: `test auth::tests::refresh_attempts_end_within_the_window ... FAILED` (four
attempts instead of two: a retry would reach auth-svc after its grace) and
`4 passed; 1 failed`. Undo the edit.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "push-sdk: auth-svc's API and the device's credentials"
```

Expected: 891 tests pass, no warnings, no diff.

---

### Task 4: The local store

**Files:**
- Create: `crates/push-sdk/src/store.rs`
- Modify: `crates/push-sdk/src/lib.rs`

Decisions 2, 3, 8, 9 and 11. `store.rs` creates the schema (the outbox table included, used
from Task 5), binds the database to its owner, and applies deliveries, history pages,
listings, receipts and read positions with the rules of spec §4: replays change nothing,
edits and deletes apply only if newer and only to held messages, positions only move
forward and a read position never passes the last known `seq`, unread counts are recounted
from held messages when the read position moves. `wipe` erases another user's data, and
`erase_all` everything, leaving nothing in the files. The test fixture
(`store::tests::fixture`, `entry`) and `Scratch` (a database file in a directory of its own
next to the test binary, removed afterwards) are shared with later tasks' tests.

- [ ] **Step 1: Make the changes**

Create `crates/push-sdk/src/store.rs`:

```rust
//! The local store (spec §8.3, §8.4): one SQLite database per signed-in user holding the
//! conversations, the messages (history), the receipt and read positions, the contacts
//! and the outbox of sends, edits and deletes not yet acknowledged.
//!
//! Every change that must survive a crash is one transaction: an outbox row and its local
//! echo are written together, before the first attempt, so a retry after a restart sends
//! the same `client_msg_id`. Positions only move forward (`MAX`), because receipts can
//! arrive in any order (spec §7.2).

use std::time::Duration;

use push_domain::{ClientMsgId, ConversationId, MAX_UNREAD, MessageId, UserId};
use push_proto::v1::{self, ContentType, Deliver, EntryKind};
use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;

use crate::SdkError;
use crate::model::{
    Contact, ContactState, Conversation, ConversationKind, Message, MessageKind, MessageStatus,
};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
);
CREATE TABLE conversation (
    id                 BLOB PRIMARY KEY,
    kind               INTEGER NOT NULL DEFAULT 0,
    peer_id            BLOB,
    title              TEXT,
    last_seq           INTEGER NOT NULL DEFAULT 0,
    -- The user's read position, including marks the server has not confirmed yet.
    read_seq           INTEGER NOT NULL DEFAULT 0,
    -- The highest read position the server confirmed.
    read_synced_seq    INTEGER NOT NULL DEFAULT 0,
    unread             INTEGER NOT NULL DEFAULT 0,
    peer_delivered_seq INTEGER NOT NULL DEFAULT 0,
    peer_read_seq      INTEGER NOT NULL DEFAULT 0,
    -- The highest seq this device stored from deliveries: what it acks.
    received_seq       INTEGER NOT NULL DEFAULT 0,
    updated_at         INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE message (
    conversation_id BLOB NOT NULL,
    client_msg_id   BLOB NOT NULL,
    seq             INTEGER,
    message_id      BLOB,
    sender_id       BLOB NOT NULL,
    content_type    INTEGER NOT NULL,
    body            BLOB,
    client_ts       INTEGER,
    server_ts       INTEGER,
    edited_at       INTEGER,
    edit_seq        INTEGER NOT NULL DEFAULT 0,
    deleted         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (conversation_id, client_msg_id)
);
CREATE UNIQUE INDEX message_seq ON message (conversation_id, seq) WHERE seq IS NOT NULL;
CREATE TABLE contact (
    user_id BLOB PRIMARY KEY,
    state   INTEGER NOT NULL
);
CREATE TABLE outbox (
    client_msg_id        BLOB PRIMARY KEY,
    conversation_id      BLOB NOT NULL,
    kind                 INTEGER NOT NULL,
    target_message_id    BLOB,
    target_client_msg_id BLOB,
    content_type         INTEGER,
    body                 BLOB,
    -- 0 queued, 1 in flight.
    state                INTEGER NOT NULL DEFAULT 0,
    enqueued_at          INTEGER NOT NULL,
    attempts             INTEGER NOT NULL DEFAULT 0,
    -- Milliseconds since the Unix epoch.
    next_attempt_at      INTEGER NOT NULL DEFAULT 0,
    -- Send only after this row is acknowledged.
    parent_id            BLOB
);
CREATE INDEX outbox_order ON outbox (conversation_id);
";

/// What a delivered entry changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Applied {
    /// Stored already: a replay, or the same entry on a second connection.
    Duplicate,
    /// A message this device did not hold.
    Added(Message),
    /// One of this device's own messages, now stored by the server.
    Confirmed(Message),
    Edited(Message),
    Deleted(Message),
    /// An edit or delete of a message this device does not hold, or an older edit.
    Skipped,
}

/// The local database.
pub(crate) struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (or creates) the database at `path`; `:memory:` keeps it in memory.
    pub fn open(path: &str) -> Result<Self, SdkError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // WAL: a crash never loses a committed outbox row; NORMAL syncs at checkpoints.
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Deleted content is overwritten, not left in free space (see `wipe`).
        conn.pragma_update(None, "secure_delete", "ON")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(SdkError::Storage(format!(
                "the database is from a newer SDK (schema {version})"
            )));
        }
        if version < SCHEMA_VERSION {
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self { conn })
    }

    /// The user the database belongs to.
    pub fn owner(&self) -> Result<Option<UserId>, SdkError> {
        let value: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'owner'", [], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(value.and_then(|value| Uuid::from_slice(&value).ok().map(UserId)))
    }

    /// Makes the database `user`'s. Another user's data is erased first, outbox
    /// included: whatever they had not sent is not this user's to send. True if it erased.
    pub fn bind_owner(&mut self, user: UserId) -> Result<bool, SdkError> {
        let owner = self.owner()?;
        if owner == Some(user) {
            return Ok(false);
        }
        self.wipe(Some(user))?;
        Ok(owner.is_some())
    }

    /// Erases everything: the user signed this device out.
    pub fn erase_all(&mut self) -> Result<(), SdkError> {
        self.wipe(None)
    }

    /// Deletes every row, then records `owner`, and leaves nothing of the old rows in the
    /// files: `secure_delete` overwrites deleted content, and the write-ahead log, which
    /// still holds the old pages, is checkpointed and truncated.
    fn wipe(&mut self, owner: Option<UserId>) -> Result<(), SdkError> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM outbox; DELETE FROM message; DELETE FROM conversation; DELETE FROM contact; DELETE FROM meta;",
        )?;
        if let Some(owner) = owner {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('owner', ?1)",
                params![bytes(owner.0)],
            )?;
        }
        tx.commit()?;
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    fn me(&self) -> Result<UserId, SdkError> {
        self.owner()?.ok_or(SdkError::NotSignedIn)
    }

    // ----- conversations -----

    pub fn conversation(&self, id: ConversationId) -> Result<Option<Conversation>, SdkError> {
        Ok(self
            .conn
            .query_row(
                &format!("{CONVERSATION_COLUMNS} WHERE id = ?1"),
                params![bytes(id.0)],
                conversation_row,
            )
            .optional()?)
    }

    /// Every conversation, most recently changed first.
    pub fn conversations(&self) -> Result<Vec<Conversation>, SdkError> {
        let mut statement = self.conn.prepare(&format!(
            "{CONVERSATION_COLUMNS} ORDER BY updated_at DESC, id"
        ))?;
        let rows = statement.query_map([], conversation_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records a direct conversation the user opened.
    pub fn open_direct(
        &mut self,
        id: ConversationId,
        peer: UserId,
    ) -> Result<Conversation, SdkError> {
        self.conn.execute(
            "INSERT INTO conversation (id, kind, peer_id) VALUES (?1, 1, ?2)
             ON CONFLICT (id) DO UPDATE SET kind = 1, peer_id = excluded.peer_id",
            params![bytes(id.0), bytes(peer.0)],
        )?;
        self.conversation(id)?
            .ok_or_else(|| SdkError::Storage("the conversation vanished".to_owned()))
    }

    /// Takes a conversation from a listing: positions only move forward, the unread count
    /// is the server's. The conversation if it changed.
    pub fn apply_summary(
        &mut self,
        summary: &v1::ConversationSummary,
    ) -> Result<Option<Conversation>, SdkError> {
        let me = self.me()?;
        let id = ConversationId(id_of(&summary.conversation_id)?);
        let before = self.conversation(id)?;
        let peer = summary
            .members
            .iter()
            .filter_map(|member| Uuid::from_slice(member).ok())
            .find(|member| *member != me.0);
        let (delivered, read) = match &summary.receipts {
            Some(v1::conversation_summary::Receipts::Direct(direct)) => {
                (direct.peer_delivered_seq, direct.peer_read_seq)
            }
            Some(v1::conversation_summary::Receipts::Group(group)) => {
                (group.all_delivered_seq, group.all_read_seq)
            }
            None => (0, 0),
        };
        let kind = ConversationKind::from_wire(summary.kind());
        self.conn.execute(
            "INSERT INTO conversation (id, kind, peer_id, title, last_seq, read_seq, read_synced_seq,
                 unread, peer_delivered_seq, peer_read_seq, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (id) DO UPDATE SET
                 kind = excluded.kind,
                 peer_id = COALESCE(excluded.peer_id, peer_id),
                 title = excluded.title,
                 last_seq = MAX(last_seq, excluded.last_seq),
                 read_seq = MAX(read_seq, excluded.read_seq),
                 read_synced_seq = MAX(read_synced_seq, excluded.read_synced_seq),
                 unread = CASE WHEN excluded.read_seq >= read_seq THEN excluded.unread ELSE unread END,
                 peer_delivered_seq = MAX(peer_delivered_seq, excluded.peer_delivered_seq),
                 peer_read_seq = MAX(peer_read_seq, excluded.peer_read_seq),
                 updated_at = MAX(updated_at, excluded.updated_at)",
            params![
                bytes(id.0),
                kind.code(),
                peer.map(bytes),
                summary.title,
                summary.last_seq,
                summary.read_seq,
                i64::from(summary.unread.min(MAX_UNREAD)),
                delivered,
                read,
                summary.updated_at,
            ],
        )?;
        let after = self.conversation(id)?;
        Ok(if after == before { None } else { after })
    }

    /// A receipt: the highest positions win. The conversation if they moved.
    pub fn apply_receipt(
        &mut self,
        id: ConversationId,
        delivered_seq: i64,
        read_seq: i64,
    ) -> Result<Option<Conversation>, SdkError> {
        let changed = self.conn.execute(
            "UPDATE conversation SET
                 peer_delivered_seq = MAX(peer_delivered_seq, ?2, ?3),
                 peer_read_seq = MAX(peer_read_seq, ?3)
             WHERE id = ?1 AND (peer_delivered_seq < MAX(?2, ?3) OR peer_read_seq < ?3)",
            params![bytes(id.0), delivered_seq, read_seq],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.conversation(id)
    }

    /// The user's read position as the server confirmed it (`ReadPosition`, or after a
    /// `ReadUpTo`). The conversation if it moved.
    pub fn apply_read_position(
        &mut self,
        id: ConversationId,
        read_seq: i64,
    ) -> Result<Option<Conversation>, SdkError> {
        let before = self.conversation(id)?;
        self.conn.execute(
            "UPDATE conversation SET read_synced_seq = MAX(read_synced_seq, ?2),
                 read_seq = MAX(read_seq, ?2) WHERE id = ?1",
            params![bytes(id.0), read_seq],
        )?;
        self.recount_unread(id)?;
        let after = self.conversation(id)?;
        Ok(if after == before { None } else { after })
    }

    /// The user read up to `seq`: moves the local position forward at once, to be sent
    /// as `ReadUpTo`, but never past the last `seq` the device knows. The conversation if
    /// it moved.
    pub fn mark_read(
        &mut self,
        id: ConversationId,
        seq: i64,
    ) -> Result<Option<Conversation>, SdkError> {
        let changed = self.conn.execute(
            "UPDATE conversation SET read_seq = MIN(?2, last_seq)
             WHERE id = ?1 AND read_seq < MIN(?2, last_seq)",
            params![bytes(id.0), seq],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.recount_unread(id)?;
        self.conversation(id)
    }

    /// Read positions not confirmed by the server yet: `(conversation, read_seq)`.
    pub fn unsynced_reads(&self) -> Result<Vec<(ConversationId, i64)>, SdkError> {
        let mut statement = self
            .conn
            .prepare("SELECT id, read_seq FROM conversation WHERE read_seq > read_synced_seq")?;
        let rows = statement.query_map([], |row| Ok((uuid_at(row, 0)?, row.get(1)?)))?;
        Ok(rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(id, seq)| (ConversationId(id), seq))
            .collect())
    }

    /// The unread count from the messages held: others' live messages after the read
    /// position, at most 100.
    fn recount_unread(&mut self, id: ConversationId) -> Result<(), SdkError> {
        let me = self.me()?;
        self.conn.execute(
            "UPDATE conversation SET unread = (
                 SELECT MIN(COUNT(*), ?3) FROM message m
                 WHERE m.conversation_id = conversation.id AND m.seq > conversation.read_seq
                   AND m.sender_id <> ?2 AND m.deleted = 0 AND m.content_type = 1)
             WHERE id = ?1",
            params![bytes(id.0), bytes(me.0), i64::from(MAX_UNREAD)],
        )?;
        Ok(())
    }

    /// Per conversation, the highest `seq` stored from deliveries.
    pub fn received(&self, id: ConversationId) -> Result<i64, SdkError> {
        Ok(self
            .conn
            .query_row(
                "SELECT received_seq FROM conversation WHERE id = ?1",
                params![bytes(id.0)],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    // ----- messages -----

    /// Applies a delivered entry (spec §4.3, §4.6): new messages are stored, an edit or a
    /// delete changes its target only if it is newer than the target's last change, and
    /// entries at or below the conversation's received position are replays. Returns what
    /// changed and the conversation when it was new or its summary changed.
    pub fn apply_deliver(
        &mut self,
        entry: &Deliver,
    ) -> Result<(Applied, Option<Conversation>, bool), SdkError> {
        let me = self.me()?;
        let id = ConversationId(id_of(&entry.conversation_id)?);
        let before = self.conversation(id)?;
        let created = before.is_none();
        if let Some(conversation) = &before {
            if entry.seq <= self.received(conversation.id)? {
                return Ok((Applied::Duplicate, None, false));
            }
        } else {
            self.conn.execute(
                "INSERT INTO conversation (id) VALUES (?1)",
                params![bytes(id.0)],
            )?;
        }
        let applied = match entry.kind() {
            EntryKind::Message | EntryKind::System => self.store_entry(entry)?,
            EntryKind::Edit => self.apply_edit_entry(id, entry)?,
            EntryKind::Delete => self.apply_delete_entry(id, entry)?,
            EntryKind::Unspecified => Applied::Skipped,
        };
        self.conn.execute(
            "UPDATE conversation SET received_seq = MAX(received_seq, ?2),
                 last_seq = MAX(last_seq, ?2), updated_at = MAX(updated_at, ?3)
             WHERE id = ?1",
            params![bytes(id.0), entry.seq, entry.server_ts],
        )?;
        let counts = matches!(&applied, Applied::Added(message)
            if message.sender_id != me && message.kind == MessageKind::Text && !message.deleted);
        if counts || matches!(applied, Applied::Deleted(_)) {
            self.recount_unread(id)?;
        }
        let after = self.conversation(id)?;
        let changed = if after == before { None } else { after };
        Ok((applied, changed, created))
    }

    /// A message or system entry, from a delivery or a history page: the device's own
    /// pending message is confirmed; one held already is brought to its current state.
    fn store_entry(&mut self, entry: &Deliver) -> Result<Applied, SdkError> {
        let id = ConversationId(id_of(&entry.conversation_id)?);
        let client_msg_id = ClientMsgId(id_of(&entry.client_msg_id)?);
        let existing = self.message(id, client_msg_id)?;
        let body = entry.body.as_deref();
        match existing {
            Some(held) => {
                self.conn.execute(
                    "UPDATE message SET seq = ?3, message_id = ?4, server_ts = ?5, body = ?6,
                         edited_at = ?7, edit_seq = MAX(edit_seq, ?8), deleted = ?9
                     WHERE conversation_id = ?1 AND client_msg_id = ?2",
                    params![
                        bytes(id.0),
                        bytes(client_msg_id.0),
                        entry.seq,
                        entry.message_id,
                        entry.server_ts,
                        if entry.deleted { None } else { body },
                        entry.edited_at,
                        entry.edit_seq.unwrap_or(0),
                        entry.deleted,
                    ],
                )?;
                let now = self.message(id, client_msg_id)?.expect("just updated");
                Ok(if held.seq.is_none() {
                    Applied::Confirmed(now)
                } else {
                    Applied::Duplicate
                })
            }
            None => {
                let inserted = self.conn.execute(
                    "INSERT OR IGNORE INTO message (conversation_id, client_msg_id, seq, message_id,
                         sender_id, content_type, body, client_ts, server_ts, edited_at, edit_seq, deleted)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        bytes(id.0),
                        bytes(client_msg_id.0),
                        entry.seq,
                        entry.message_id,
                        entry.sender_id,
                        content_type_code(entry),
                        if entry.deleted { None } else { body },
                        entry.client_ts,
                        entry.server_ts,
                        entry.edited_at,
                        entry.edit_seq.unwrap_or(0),
                        entry.deleted,
                    ],
                )?;
                if inserted == 0 {
                    return Ok(Applied::Duplicate);
                }
                Ok(Applied::Added(
                    self.message(id, client_msg_id)?.expect("just inserted"),
                ))
            }
        }
    }

    fn target_at(
        &self,
        id: ConversationId,
        seq: Option<i64>,
    ) -> Result<Option<(ClientMsgId, i64)>, SdkError> {
        let Some(seq) = seq else {
            return Ok(None);
        };
        Ok(self
            .conn
            .query_row(
                "SELECT client_msg_id, edit_seq FROM message WHERE conversation_id = ?1 AND seq = ?2",
                params![bytes(id.0), seq],
                |row| Ok((uuid_at(row, 0)?, row.get(1)?)),
            )
            .optional()?
            .map(|(client_msg_id, edit_seq)| (ClientMsgId(client_msg_id), edit_seq)))
    }

    fn apply_edit_entry(
        &mut self,
        id: ConversationId,
        entry: &Deliver,
    ) -> Result<Applied, SdkError> {
        let Some((target, edit_seq)) = self.target_at(id, entry.target_seq)? else {
            return Ok(Applied::Skipped);
        };
        // A superseded edit carries no body; the newer one follows.
        let Some(body) = entry.body.as_deref() else {
            return Ok(Applied::Skipped);
        };
        if entry.seq <= edit_seq {
            return Ok(Applied::Skipped);
        }
        self.edit(id, target, body, entry.server_ts, entry.seq)
    }

    fn apply_delete_entry(
        &mut self,
        id: ConversationId,
        entry: &Deliver,
    ) -> Result<Applied, SdkError> {
        let Some((target, edit_seq)) = self.target_at(id, entry.target_seq)? else {
            return Ok(Applied::Skipped);
        };
        if entry.seq <= edit_seq {
            return Ok(Applied::Skipped);
        }
        self.erase(id, target, entry.seq)
    }

    /// Gives a message a new body, from an edit entry at `seq`.
    pub fn edit(
        &mut self,
        id: ConversationId,
        target: ClientMsgId,
        body: &[u8],
        edited_at: i64,
        seq: i64,
    ) -> Result<Applied, SdkError> {
        let changed = self.conn.execute(
            "UPDATE message SET body = ?3, edited_at = ?4, edit_seq = ?5
             WHERE conversation_id = ?1 AND client_msg_id = ?2 AND deleted = 0 AND edit_seq < ?5",
            params![bytes(id.0), bytes(target.0), body, edited_at, seq],
        )?;
        if changed == 0 {
            return Ok(Applied::Skipped);
        }
        Ok(Applied::Edited(
            self.message(id, target)?.expect("just edited"),
        ))
    }

    /// Erases a message's body, from a delete entry at `seq`.
    pub fn erase(
        &mut self,
        id: ConversationId,
        target: ClientMsgId,
        seq: i64,
    ) -> Result<Applied, SdkError> {
        let changed = self.conn.execute(
            "UPDATE message SET body = NULL, deleted = 1, edit_seq = MAX(edit_seq, ?3)
             WHERE conversation_id = ?1 AND client_msg_id = ?2 AND deleted = 0",
            params![bytes(id.0), bytes(target.0), seq],
        )?;
        if changed == 0 {
            return Ok(Applied::Skipped);
        }
        self.recount_unread(id)?;
        Ok(Applied::Deleted(
            self.message(id, target)?.expect("just erased"),
        ))
    }

    /// Stores a history page: messages and system entries in their current state. The
    /// received position is not moved: history is older than what deliveries bring.
    pub fn store_history(&mut self, entries: &[Deliver]) -> Result<Vec<Message>, SdkError> {
        let mut stored = Vec::with_capacity(entries.len());
        for entry in entries {
            let id = ConversationId(id_of(&entry.conversation_id)?);
            if self.conversation(id)?.is_none() {
                self.conn.execute(
                    "INSERT INTO conversation (id) VALUES (?1)",
                    params![bytes(id.0)],
                )?;
            }
            self.store_entry(entry)?;
            let client_msg_id = ClientMsgId(id_of(&entry.client_msg_id)?);
            if let Some(message) = self.message(id, client_msg_id)? {
                stored.push(message);
            }
        }
        Ok(stored)
    }

    pub fn message(
        &self,
        id: ConversationId,
        client_msg_id: ClientMsgId,
    ) -> Result<Option<Message>, SdkError> {
        let me = self.me()?;
        Ok(self
            .conn
            .query_row(
                &format!("{MESSAGE_COLUMNS} WHERE m.conversation_id = ?1 AND m.client_msg_id = ?2"),
                params![bytes(id.0), bytes(client_msg_id.0)],
                |row| message_row(row, me),
            )
            .optional()?)
    }

    /// Up to `limit` stored messages before `before_seq`, oldest first; without
    /// `before_seq`, the newest ones followed by the messages still in the outbox.
    pub fn messages(
        &self,
        id: ConversationId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, SdkError> {
        let me = self.me()?;
        let mut statement = self.conn.prepare(&format!(
            "{MESSAGE_COLUMNS} WHERE m.conversation_id = ?1 AND m.seq IS NOT NULL AND m.seq < ?2
             ORDER BY m.seq DESC LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![bytes(id.0), before_seq.unwrap_or(i64::MAX), limit],
            |row| message_row(row, me),
        )?;
        let mut messages: Vec<Message> = rows.collect::<Result<_, _>>()?;
        messages.reverse();
        if before_seq.is_none() {
            let mut statement = self.conn.prepare(&format!(
                "{MESSAGE_COLUMNS} WHERE m.conversation_id = ?1 AND m.seq IS NULL ORDER BY m.rowid"
            ))?;
            let pending = statement.query_map(params![bytes(id.0)], |row| message_row(row, me))?;
            messages.extend(pending.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(messages)
    }

    // ----- contacts -----

    pub fn contacts(&self) -> Result<Vec<Contact>, SdkError> {
        let mut statement = self
            .conn
            .prepare("SELECT user_id, state FROM contact WHERE state <> 0 ORDER BY user_id")?;
        let rows = statement.query_map([], |row| {
            Ok(Contact {
                user_id: UserId(uuid_at(row, 0)?),
                state: ContactState::from_code(row.get(1)?),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records a relationship; true if it changed.
    pub fn set_contact(&mut self, contact: Contact) -> Result<bool, SdkError> {
        let changed = self.conn.execute(
            "INSERT INTO contact (user_id, state) VALUES (?1, ?2)
             ON CONFLICT (user_id) DO UPDATE SET state = excluded.state WHERE state <> excluded.state",
            params![bytes(contact.user_id.0), contact.state.code()],
        )?;
        Ok(changed > 0)
    }

    /// Replaces every relationship with a listing; the ones that changed.
    pub fn replace_contacts(&mut self, listed: &[Contact]) -> Result<Vec<Contact>, SdkError> {
        let mut changed = Vec::new();
        for contact in self.contacts()? {
            if !listed.iter().any(|other| other.user_id == contact.user_id) {
                let none = Contact {
                    user_id: contact.user_id,
                    state: ContactState::None,
                };
                if self.set_contact(none)? {
                    changed.push(none);
                }
            }
        }
        for contact in listed {
            if self.set_contact(*contact)? {
                changed.push(*contact);
            }
        }
        Ok(changed)
    }
}

const CONVERSATION_COLUMNS: &str = "SELECT id, kind, peer_id, title, last_seq, read_seq, unread,
    peer_delivered_seq, peer_read_seq, updated_at FROM conversation";

fn conversation_row(row: &Row<'_>) -> rusqlite::Result<Conversation> {
    let peer: Option<Vec<u8>> = row.get(2)?;
    let unread: i64 = row.get(6)?;
    Ok(Conversation {
        id: ConversationId(uuid_at(row, 0)?),
        kind: ConversationKind::from_code(row.get(1)?),
        peer: peer.and_then(|peer| Uuid::from_slice(&peer).ok().map(UserId)),
        title: row.get(3)?,
        last_seq: row.get(4)?,
        read_seq: row.get(5)?,
        unread: u32::try_from(unread).unwrap_or(0),
        peer_delivered_seq: row.get(7)?,
        peer_read_seq: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

const MESSAGE_COLUMNS: &str = "SELECT m.conversation_id, m.client_msg_id, m.message_id, m.seq,
    m.sender_id, m.content_type, m.body, m.client_ts, m.server_ts, m.edited_at, m.deleted,
    c.peer_delivered_seq, c.peer_read_seq
    FROM message m JOIN conversation c ON c.id = m.conversation_id";

fn message_row(row: &Row<'_>, me: UserId) -> rusqlite::Result<Message> {
    let message_id: Option<Vec<u8>> = row.get(2)?;
    let seq: Option<i64> = row.get(3)?;
    let sender_id = UserId(uuid_at(row, 4)?);
    let content_type: i64 = row.get(5)?;
    let body: Option<Vec<u8>> = row.get(6)?;
    let (delivered, read): (i64, i64) = (row.get(11)?, row.get(12)?);
    let status = match seq {
        _ if sender_id != me => MessageStatus::Received,
        None => MessageStatus::Pending,
        Some(seq) if seq <= read => MessageStatus::Read,
        Some(seq) if seq <= delivered => MessageStatus::Delivered,
        Some(_) => MessageStatus::Sent,
    };
    Ok(Message {
        conversation_id: ConversationId(uuid_at(row, 0)?),
        client_msg_id: ClientMsgId(uuid_at(row, 1)?),
        message_id: message_id.and_then(|id| Uuid::from_slice(&id).ok().map(MessageId)),
        seq,
        sender_id,
        kind: if content_type == 2 {
            MessageKind::System
        } else {
            MessageKind::Text
        },
        body: body.map(|body| String::from_utf8_lossy(&body).into_owned()),
        client_ts: row.get(7)?,
        server_ts: row.get(8)?,
        edited_at: row.get(9)?,
        deleted: row.get(10)?,
        status,
    })
}

fn uuid_at(row: &Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    let value: Vec<u8> = row.get(index)?;
    Uuid::from_slice(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            Box::new(error),
        )
    })
}

fn content_type_code(entry: &Deliver) -> i64 {
    match entry.content_type() {
        ContentType::SystemEvent => 2,
        _ => 1,
    }
}

fn id_of(value: &[u8]) -> Result<Uuid, SdkError> {
    Uuid::from_slice(value)
        .map_err(|_| SdkError::Network("the server sent an id that is not 16 bytes".to_owned()))
}

pub(crate) fn bytes(id: Uuid) -> Vec<u8> {
    id.as_bytes().to_vec()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) struct Fixture {
        pub store: Store,
        pub me: UserId,
        pub peer: UserId,
        pub conversation: ConversationId,
    }

    pub(crate) fn fixture() -> Fixture {
        let mut store = Store::open(":memory:").unwrap();
        let (me, peer) = (UserId::new(), UserId::new());
        store.bind_owner(me).unwrap();
        let conversation = ConversationId::new();
        store.open_direct(conversation, peer).unwrap();
        Fixture {
            store,
            me,
            peer,
            conversation,
        }
    }

    pub(crate) fn entry(
        conversation: ConversationId,
        sender: UserId,
        seq: i64,
        kind: EntryKind,
    ) -> Deliver {
        Deliver {
            conversation_id: bytes(conversation.0),
            seq,
            message_id: bytes(Uuid::now_v7()),
            client_msg_id: bytes(Uuid::now_v7()),
            kind: kind.into(),
            target_seq: None,
            sender_id: bytes(sender.0),
            content_type: ContentType::Text.into(),
            body: Some(format!("message {seq}").into_bytes()),
            client_ts: None,
            server_ts: 1_000 + seq,
            edited_at: None,
            edit_seq: None,
            deleted: false,
        }
    }

    #[test]
    fn a_delivered_message_is_stored_once_and_counts_as_unread() {
        let mut f = fixture();
        let first = entry(f.conversation, f.peer, 1, EntryKind::Message);
        let (applied, conversation, created) = f.store.apply_deliver(&first).unwrap();
        assert!(
            matches!(applied, Applied::Added(ref message) if message.body.as_deref() == Some("message 1"))
        );
        assert!(!created);
        let conversation = conversation.unwrap();
        assert_eq!((conversation.last_seq, conversation.unread), (1, 1));
        assert_eq!(f.store.received(f.conversation).unwrap(), 1);
        // The same entry again (a second connection, a replay before the ack) changes nothing.
        let (again, changed, _) = f.store.apply_deliver(&first).unwrap();
        assert_eq!((again, changed), (Applied::Duplicate, None));
        assert_eq!(f.store.messages(f.conversation, None, 50).unwrap().len(), 1);
    }

    #[test]
    fn an_edit_or_delete_applies_only_if_newer_and_only_to_a_held_message() {
        let mut f = fixture();
        f.store
            .apply_deliver(&entry(f.conversation, f.peer, 1, EntryKind::Message))
            .unwrap();
        let mut delete = entry(f.conversation, f.peer, 3, EntryKind::Delete);
        delete.target_seq = Some(1);
        delete.body = None;
        let (applied, _, _) = f.store.apply_deliver(&delete).unwrap();
        assert!(
            matches!(applied, Applied::Deleted(ref message) if message.deleted && message.body.is_none())
        );
        // An older edit arriving later (seq 2 < 3) cannot bring the body back... it is a
        // replay below the received position anyway, but even a held target refuses it.
        let applied = f
            .store
            .edit(
                f.conversation,
                message_at(&f, 1).client_msg_id,
                b"back",
                5,
                2,
            )
            .unwrap();
        assert_eq!(applied, Applied::Skipped);
        assert_eq!(message_at(&f, 1).body, None);

        let mut orphan = entry(f.conversation, f.peer, 4, EntryKind::Edit);
        orphan.target_seq = Some(99);
        assert_eq!(f.store.apply_deliver(&orphan).unwrap().0, Applied::Skipped);
        assert_eq!(f.store.received(f.conversation).unwrap(), 4);
    }

    fn message_at(f: &Fixture, seq: i64) -> Message {
        f.store
            .messages(f.conversation, None, 100)
            .unwrap()
            .into_iter()
            .find(|message| message.seq == Some(seq))
            .unwrap()
    }

    #[test]
    fn reading_moves_forward_only_and_recounts_unread() {
        let mut f = fixture();
        for seq in 1..=3 {
            f.store
                .apply_deliver(&entry(f.conversation, f.peer, seq, EntryKind::Message))
                .unwrap();
        }
        let read = f.store.mark_read(f.conversation, 2).unwrap().unwrap();
        assert_eq!((read.read_seq, read.unread), (2, 1));
        assert!(f.store.mark_read(f.conversation, 1).unwrap().is_none());
        assert_eq!(f.store.unsynced_reads().unwrap(), [(f.conversation, 2)]);
        f.store.apply_read_position(f.conversation, 2).unwrap();
        assert!(f.store.unsynced_reads().unwrap().is_empty());
        // Not past the last message: a read position beyond it would hide the next ones.
        let read = f.store.mark_read(f.conversation, 1_000).unwrap().unwrap();
        assert_eq!((read.read_seq, read.unread), (3, 0));
    }

    /// A database file in a directory of its own next to the test binary, removed when
    /// the value is dropped.
    pub(crate) struct Scratch {
        dir: std::path::PathBuf,
    }

    impl Scratch {
        pub fn new() -> Self {
            let exe = std::env::current_exe().unwrap();
            let dir = exe
                .parent()
                .unwrap()
                .join("push-sdk-test-data")
                .join(Uuid::now_v7().to_string());
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        pub fn database(&self) -> String {
            self.dir.join("push.db").to_string_lossy().into_owned()
        }

        /// Whether the database or its write-ahead log holds `marker`.
        pub fn holds(&self, marker: &str) -> bool {
            ["push.db", "push.db-wal"].iter().any(|name| {
                std::fs::read(self.dir.join(name)).is_ok_and(|bytes| {
                    bytes
                        .windows(marker.len())
                        .any(|window| window == marker.as_bytes())
                })
            })
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn erasing_leaves_no_trace_in_the_files() {
        let scratch = Scratch::new();
        let mut store = Store::open(&scratch.database()).unwrap();
        let (me, peer, conversation) = (UserId::new(), UserId::new(), ConversationId::new());
        store.bind_owner(me).unwrap();
        store.open_direct(conversation, peer).unwrap();
        let marker = format!("secret-{}", Uuid::now_v7());
        for seq in 1..=20 {
            let mut delivered = entry(conversation, peer, seq, EntryKind::Message);
            delivered.body = Some(format!("{marker} {seq}").into_bytes());
            store.apply_deliver(&delivered).unwrap();
        }
        assert!(scratch.holds(&marker), "a plain database holds the text");

        store.erase_all().unwrap();
        assert!(!scratch.holds(&marker), "erased text is still in the files");
        assert!(store.conversations().unwrap().is_empty());

        // Another user's sign-in erases the same way.
        store.bind_owner(me).unwrap();
        store.open_direct(conversation, peer).unwrap();
        let mut delivered = entry(conversation, peer, 1, EntryKind::Message);
        delivered.body = Some(marker.clone().into_bytes());
        store.apply_deliver(&delivered).unwrap();
        assert!(scratch.holds(&marker));
        assert!(store.bind_owner(UserId::new()).unwrap());
        assert!(!scratch.holds(&marker));
    }

    #[test]
    fn debug_output_never_shows_a_body() {
        let mut f = fixture();
        let mut delivered = entry(f.conversation, f.peer, 1, EntryKind::Message);
        delivered.body = Some(b"a private text".to_vec());
        let (Applied::Added(message), _, _) = f.store.apply_deliver(&delivered).unwrap() else {
            panic!("a new message");
        };
        let shown = format!("{:?}", crate::model::Event::MessageAdded(message));
        assert!(!shown.contains("a private text"), "{shown}");
        assert!(shown.contains("<14 bytes>"), "{shown}");
    }

    #[test]
    fn contacts_follow_the_listing() {
        let mut f = fixture();
        let other = UserId::new();
        let accepted = Contact {
            user_id: f.peer,
            state: ContactState::Accepted,
        };
        assert!(f.store.set_contact(accepted).unwrap());
        assert!(!f.store.set_contact(accepted).unwrap());
        let asked = Contact {
            user_id: other,
            state: ContactState::RequestedIn,
        };
        let changed = f.store.replace_contacts(&[asked]).unwrap();
        assert_eq!(
            changed,
            [
                Contact {
                    user_id: f.peer,
                    state: ContactState::None
                },
                asked
            ]
        );
        assert_eq!(f.store.contacts().unwrap(), [asked]);
    }
}
```

Change `crates/push-sdk/src/lib.rs`:

```diff
diff --git a/crates/push-sdk/src/lib.rs b/crates/push-sdk/src/lib.rs
index bf0141f..2802174 100644
--- a/crates/push-sdk/src/lib.rs
+++ b/crates/push-sdk/src/lib.rs
@@ -19,6 +19,7 @@ mod credentials;
 mod error;
 mod model;
 mod secrets;
+mod store;
 
 pub use auth::{Device, Platform, Profile};
 pub use error::SdkError;
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-sdk/src/store.rs`, in `mark_read`, change
`"UPDATE conversation SET read_seq = MIN(?2, last_seq)` to
`"UPDATE conversation SET read_seq = ?2` and run:

```powershell
cargo test -p push-sdk --lib --locked store::tests
```

Expected: `test store::tests::reading_moves_forward_only_and_recounts_unread ... FAILED`
(a read position past the last message) and `5 passed; 1 failed`. Undo the edit.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "push-sdk: the local store"
```

Expected: 897 tests pass, no warnings, no diff.

---

### Task 5: The outbox

**Files:**
- Create: `crates/push-sdk/src/store/outbox.rs`
- Modify: `crates/push-sdk/src/store.rs`, `crates/push-sdk/src/model.rs`

Decision 10 and spec §8.4. A message and its local echo are written in one transaction
before any attempt; `due` returns the first row of each conversation when it is queued and
due; rows in flight go back to the queue when a connection ends; an unsent message can be
edited in place or dropped, but not once it was sent; `acknowledged` gives the echo its
server id and `seq`; `refused` removes a row with the rows that change it and a refused
message's echo. The frames a row sends (`Send`, `Edit`, `Delete`) are built here. An
`OutboxRow`'s `Debug` shows its body as a length, like a `Message`'s.

- [ ] **Step 1: Make the changes**

Create `crates/push-sdk/src/store/outbox.rs`:

```rust
//! The outbox (spec §8.4): sends, edits and deletes waiting for their `SendAck`, sent in
//! order per conversation, one at a time, with the same `client_msg_id` on every attempt.

use push_domain::{ClientMsgId, ConversationId, MessageId};
use push_proto::v1::{self, ContentType};
use rusqlite::{OptionalExtension, Row, params};
use uuid::Uuid;

use super::{Store, bytes, id_of, uuid_at};
use crate::SdkError;
use crate::model::{Message, OutboxKind, redacted};

/// A row of the outbox.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OutboxRow {
    pub client_msg_id: ClientMsgId,
    pub conversation_id: ConversationId,
    pub kind: OutboxKind,
    pub target_message_id: Option<MessageId>,
    pub target_client_msg_id: Option<ClientMsgId>,
    pub body: Option<String>,
    pub in_flight: bool,
    pub enqueued_at: i64,
    pub attempts: u32,
    pub parent_id: Option<ClientMsgId>,
}

impl std::fmt::Debug for OutboxRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutboxRow")
            .field("client_msg_id", &self.client_msg_id)
            .field("conversation_id", &self.conversation_id)
            .field("kind", &self.kind)
            .field("target_message_id", &self.target_message_id)
            .field("target_client_msg_id", &self.target_client_msg_id)
            .field("body", &redacted(&self.body))
            .field("in_flight", &self.in_flight)
            .field("enqueued_at", &self.enqueued_at)
            .field("attempts", &self.attempts)
            .field("parent_id", &self.parent_id)
            .finish()
    }
}

impl OutboxRow {
    /// The frame that sends this row.
    pub fn frame(&self) -> v1::frame::Body {
        let conversation_id = bytes(self.conversation_id.0);
        let client_msg_id = bytes(self.client_msg_id.0);
        match self.kind {
            OutboxKind::Send => v1::frame::Body::Send(v1::Send {
                client_msg_id,
                conversation_id,
                content_type: ContentType::Text.into(),
                body: self.body.clone().unwrap_or_default().into_bytes(),
                client_ts: Some(self.enqueued_at),
            }),
            OutboxKind::Edit => v1::frame::Body::Edit(v1::Edit {
                client_msg_id,
                conversation_id,
                target: Some(match (self.target_message_id, self.target_client_msg_id) {
                    (Some(id), _) => v1::edit::Target::TargetMessageId(bytes(id.0)),
                    (None, Some(id)) => v1::edit::Target::TargetClientMsgId(bytes(id.0)),
                    (None, None) => v1::edit::Target::TargetMessageId(Vec::new()),
                }),
                body: self.body.clone().unwrap_or_default().into_bytes(),
                client_ts: Some(self.enqueued_at),
            }),
            OutboxKind::Delete => v1::frame::Body::Delete(v1::Delete {
                client_msg_id,
                conversation_id,
                target: Some(match (self.target_message_id, self.target_client_msg_id) {
                    (Some(id), _) => v1::delete::Target::TargetMessageId(bytes(id.0)),
                    (None, Some(id)) => v1::delete::Target::TargetClientMsgId(bytes(id.0)),
                    (None, None) => v1::delete::Target::TargetMessageId(Vec::new()),
                }),
            }),
        }
    }
}

impl Store {
    /// Queues a text message and stores its local echo, in one transaction.
    pub fn queue_send(
        &mut self,
        id: ConversationId,
        client_msg_id: ClientMsgId,
        body: &str,
        now_us: i64,
    ) -> Result<Message, SdkError> {
        let me = self.me()?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO outbox (client_msg_id, conversation_id, kind, content_type, body, enqueued_at)
             VALUES (?1, ?2, 1, 1, ?3, ?4)",
            params![bytes(client_msg_id.0), bytes(id.0), body.as_bytes(), now_us],
        )?;
        tx.execute(
            "INSERT INTO message (conversation_id, client_msg_id, sender_id, content_type, body, client_ts)
             VALUES (?1, ?2, ?3, 1, ?4, ?5)",
            params![bytes(id.0), bytes(client_msg_id.0), bytes(me.0), body.as_bytes(), now_us],
        )?;
        tx.commit()?;
        Ok(self.message(id, client_msg_id)?.expect("just queued"))
    }

    /// Queues an edit or a delete of `target`, after `parent` when the target is still in
    /// the outbox.
    #[allow(clippy::too_many_arguments)]
    pub fn queue_change(
        &mut self,
        id: ConversationId,
        client_msg_id: ClientMsgId,
        kind: OutboxKind,
        target: &Message,
        body: Option<&str>,
        parent: Option<ClientMsgId>,
        now_us: i64,
    ) -> Result<(), SdkError> {
        self.conn.execute(
            "INSERT INTO outbox (client_msg_id, conversation_id, kind, target_message_id,
                 target_client_msg_id, content_type, body, enqueued_at, parent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8)",
            params![
                bytes(client_msg_id.0),
                bytes(id.0),
                kind.code(),
                target.message_id.map(|id| bytes(id.0)),
                bytes(target.client_msg_id.0),
                body.map(str::as_bytes),
                now_us,
                parent.map(|id| bytes(id.0)),
            ],
        )?;
        Ok(())
    }

    pub fn outbox_row(&self, client_msg_id: ClientMsgId) -> Result<Option<OutboxRow>, SdkError> {
        Ok(self
            .conn
            .query_row(
                &format!("{OUTBOX_COLUMNS} WHERE client_msg_id = ?1"),
                params![bytes(client_msg_id.0)],
                outbox_row,
            )
            .optional()?)
    }

    /// Every row, in sending order.
    pub fn outbox(&self) -> Result<Vec<OutboxRow>, SdkError> {
        let mut statement = self
            .conn
            .prepare(&format!("{OUTBOX_COLUMNS} ORDER BY rowid"))?;
        let rows = statement.query_map([], outbox_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// An unsent queued message gets a new body in place (spec §8.4); false if it was
    /// sent at least once already.
    pub fn edit_unsent(
        &mut self,
        client_msg_id: ClientMsgId,
        body: &str,
    ) -> Result<bool, SdkError> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE outbox SET body = ?2 WHERE client_msg_id = ?1 AND kind = 1 AND attempts = 0 AND state = 0",
            params![bytes(client_msg_id.0), body.as_bytes()],
        )?;
        if changed > 0 {
            tx.execute(
                "UPDATE message SET body = ?2 WHERE client_msg_id = ?1 AND seq IS NULL",
                params![bytes(client_msg_id.0), body.as_bytes()],
            )?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    /// An unsent queued message is dropped with its local echo (spec §8.4: cancel, or a
    /// delete before sending); false if it was sent at least once already.
    pub fn drop_unsent(&mut self, client_msg_id: ClientMsgId) -> Result<bool, SdkError> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "DELETE FROM outbox WHERE client_msg_id = ?1 AND attempts = 0 AND state = 0",
            params![bytes(client_msg_id.0)],
        )?;
        if changed > 0 {
            tx.execute(
                "DELETE FROM message WHERE client_msg_id = ?1 AND seq IS NULL",
                params![bytes(client_msg_id.0)],
            )?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    /// The rows to send now: per conversation the first row, if it is queued and due.
    pub fn due(&self, now_ms: i64) -> Result<Vec<OutboxRow>, SdkError> {
        let mut statement = self.conn.prepare(&format!(
            "{OUTBOX_COLUMNS} WHERE rowid IN (SELECT MIN(rowid) FROM outbox GROUP BY conversation_id)
                 AND state = 0 AND next_attempt_at <= ?1 ORDER BY rowid"
        ))?;
        let rows = statement.query_map(params![now_ms], outbox_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// When the next row waiting for a retry is due, in milliseconds since the epoch.
    pub fn next_due(&self) -> Result<Option<i64>, SdkError> {
        Ok(self.conn.query_row(
            "SELECT MIN(next_attempt_at) FROM outbox
             WHERE rowid IN (SELECT MIN(rowid) FROM outbox GROUP BY conversation_id) AND state = 0",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn mark_in_flight(&mut self, client_msg_id: ClientMsgId) -> Result<(), SdkError> {
        self.conn.execute(
            "UPDATE outbox SET state = 1, attempts = attempts + 1 WHERE client_msg_id = ?1",
            params![bytes(client_msg_id.0)],
        )?;
        Ok(())
    }

    /// The connection dropped: rows in flight are queued again, to be sent again with the
    /// same `client_msg_id`.
    pub fn requeue_in_flight(&mut self) -> Result<(), SdkError> {
        self.conn
            .execute("UPDATE outbox SET state = 0 WHERE state = 1", [])?;
        Ok(())
    }

    pub fn retry_at(&mut self, client_msg_id: ClientMsgId, at_ms: i64) -> Result<(), SdkError> {
        self.conn.execute(
            "UPDATE outbox SET state = 0, next_attempt_at = ?2 WHERE client_msg_id = ?1",
            params![bytes(client_msg_id.0), at_ms],
        )?;
        Ok(())
    }

    /// The server stored the row: it leaves the outbox, and a message's local echo gets
    /// its server id and position. The row, if it was still there.
    pub fn acknowledged(&mut self, ack: &v1::SendAck) -> Result<Option<OutboxRow>, SdkError> {
        let client_msg_id = ClientMsgId(id_of(&ack.client_msg_id)?);
        let Some(row) = self.outbox_row(client_msg_id)? else {
            return Ok(None);
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM outbox WHERE client_msg_id = ?1",
            params![bytes(client_msg_id.0)],
        )?;
        if row.kind == OutboxKind::Send {
            tx.execute(
                "UPDATE message SET seq = COALESCE(seq, ?3), message_id = ?4, server_ts = ?5
                 WHERE conversation_id = ?1 AND client_msg_id = ?2",
                params![
                    bytes(row.conversation_id.0),
                    bytes(client_msg_id.0),
                    ack.seq,
                    ack.message_id,
                    ack.server_ts
                ],
            )?;
            tx.execute(
                "UPDATE conversation SET last_seq = MAX(last_seq, ?2), updated_at = MAX(updated_at, ?3)
                 WHERE id = ?1",
                params![bytes(row.conversation_id.0), ack.seq, ack.server_ts],
            )?;
        }
        tx.commit()?;
        Ok(Some(row))
    }

    /// The server refused the row for good: it leaves the outbox with a message's local
    /// echo, and so do the rows that change the refused message. Every row removed.
    pub fn refused(&mut self, client_msg_id: ClientMsgId) -> Result<Vec<OutboxRow>, SdkError> {
        let Some(row) = self.outbox_row(client_msg_id)? else {
            return Ok(Vec::new());
        };
        let mut removed = vec![row.clone()];
        if row.kind == OutboxKind::Send {
            removed.extend(self.outbox()?.into_iter().filter(|other| {
                other.target_client_msg_id == Some(client_msg_id)
                    && other.client_msg_id != client_msg_id
            }));
        }
        let tx = self.conn.transaction()?;
        for gone in &removed {
            tx.execute(
                "DELETE FROM outbox WHERE client_msg_id = ?1",
                params![bytes(gone.client_msg_id.0)],
            )?;
        }
        if row.kind == OutboxKind::Send {
            tx.execute(
                "DELETE FROM message WHERE conversation_id = ?1 AND client_msg_id = ?2 AND seq IS NULL",
                params![bytes(row.conversation_id.0), bytes(client_msg_id.0)],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }
}

const OUTBOX_COLUMNS: &str = "SELECT client_msg_id, conversation_id, kind, target_message_id,
    target_client_msg_id, body, state, enqueued_at, attempts, parent_id FROM outbox";

fn outbox_row(row: &Row<'_>) -> rusqlite::Result<OutboxRow> {
    let optional_id = |index: usize| -> rusqlite::Result<Option<Uuid>> {
        let value: Option<Vec<u8>> = row.get(index)?;
        Ok(value.and_then(|value| Uuid::from_slice(&value).ok()))
    };
    let body: Option<Vec<u8>> = row.get(5)?;
    let state: i64 = row.get(6)?;
    let attempts: i64 = row.get(8)?;
    Ok(OutboxRow {
        client_msg_id: ClientMsgId(uuid_at(row, 0)?),
        conversation_id: ConversationId(uuid_at(row, 1)?),
        kind: OutboxKind::from_code(row.get(2)?),
        target_message_id: optional_id(3)?.map(MessageId),
        target_client_msg_id: optional_id(4)?.map(ClientMsgId),
        body: body.map(|body| String::from_utf8_lossy(&body).into_owned()),
        in_flight: state == 1,
        enqueued_at: row.get(7)?,
        attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        parent_id: optional_id(9)?.map(ClientMsgId),
    })
}

#[cfg(test)]
mod tests {
    use push_domain::UserId;
    use push_proto::v1::EntryKind;

    use super::*;
    use crate::model::MessageStatus;
    use crate::store::Applied;
    use crate::store::tests::{Fixture, entry, fixture};

    #[test]
    fn the_outbox_sends_one_row_per_conversation_in_order() {
        let mut f = fixture();
        let other = ConversationId::new();
        f.store.open_direct(other, UserId::new()).unwrap();
        let (a, b, c) = (ClientMsgId::new(), ClientMsgId::new(), ClientMsgId::new());
        f.store.queue_send(f.conversation, a, "a", 1).unwrap();
        f.store.queue_send(f.conversation, b, "b", 2).unwrap();
        f.store.queue_send(other, c, "c", 3).unwrap();
        let due: Vec<_> = f
            .store
            .due(0)
            .unwrap()
            .iter()
            .map(|row| row.client_msg_id)
            .collect();
        assert_eq!(due, [a, c]);
        f.store.mark_in_flight(a).unwrap();
        let due: Vec<_> = f
            .store
            .due(0)
            .unwrap()
            .iter()
            .map(|row| row.client_msg_id)
            .collect();
        assert_eq!(due, [c], "b waits for a");
        // A dropped connection queues a again, to be sent with the same id.
        f.store.requeue_in_flight().unwrap();
        let row = f.store.outbox_row(a).unwrap().unwrap();
        assert_eq!((row.in_flight, row.attempts), (false, 1));
        // A retry later holds the conversation back until it is due.
        f.store.retry_at(a, 5_000).unwrap();
        let due: Vec<_> = f
            .store
            .due(4_999)
            .unwrap()
            .iter()
            .map(|row| row.client_msg_id)
            .collect();
        assert_eq!(due, [c]);
        assert_eq!(f.store.next_due().unwrap(), Some(0));
        assert_eq!(f.store.due(5_000).unwrap()[0].client_msg_id, a);
    }

    #[test]
    fn an_unsent_message_is_edited_in_place_or_dropped_but_a_sent_one_is_not() {
        let mut f = fixture();
        let (a, b) = (ClientMsgId::new(), ClientMsgId::new());
        f.store.queue_send(f.conversation, a, "first", 1).unwrap();
        f.store.queue_send(f.conversation, b, "second", 2).unwrap();
        assert!(f.store.edit_unsent(a, "first, fixed").unwrap());
        assert_eq!(
            f.store
                .message(f.conversation, a)
                .unwrap()
                .unwrap()
                .body
                .as_deref(),
            Some("first, fixed")
        );
        let row = f.store.outbox_row(a).unwrap().unwrap();
        assert!(!format!("{row:?}").contains("fixed"), "Debug shows no body");
        assert!(f.store.drop_unsent(b).unwrap());
        assert!(f.store.message(f.conversation, b).unwrap().is_none());
        f.store.mark_in_flight(a).unwrap();
        assert!(!f.store.edit_unsent(a, "too late").unwrap());
        assert!(!f.store.drop_unsent(a).unwrap());
    }

    #[test]
    fn a_refused_message_takes_its_changes_and_its_echo_along() {
        let mut f = fixture();
        let original = ClientMsgId::new();
        let message = f
            .store
            .queue_send(f.conversation, original, "hi", 1)
            .unwrap();
        f.store.mark_in_flight(original).unwrap();
        let edit = ClientMsgId::new();
        f.store
            .queue_change(
                f.conversation,
                edit,
                OutboxKind::Edit,
                &message,
                Some("hi!"),
                Some(original),
                2,
            )
            .unwrap();
        let removed: Vec<_> = f
            .store
            .refused(original)
            .unwrap()
            .iter()
            .map(|row| row.client_msg_id)
            .collect();
        assert_eq!(removed, [original, edit]);
        assert!(f.store.outbox().unwrap().is_empty());
        assert!(f.store.message(f.conversation, original).unwrap().is_none());
    }

    #[test]
    fn receipts_keep_the_highest_positions_and_drive_the_status() {
        let mut f = fixture();
        let client_msg_id = ClientMsgId::new();
        let queued = f
            .store
            .queue_send(f.conversation, client_msg_id, "hi", 10)
            .unwrap();
        assert_eq!(queued.status, MessageStatus::Pending);
        let ack = v1::SendAck {
            client_msg_id: bytes(client_msg_id.0),
            message_id: bytes(Uuid::now_v7()),
            seq: 1,
            server_ts: 20,
        };
        assert!(f.store.acknowledged(&ack).unwrap().is_some());
        assert!(f.store.acknowledged(&ack).unwrap().is_none());
        let status = |f: &Fixture| {
            f.store
                .message(f.conversation, client_msg_id)
                .unwrap()
                .unwrap()
                .status
        };
        assert_eq!(status(&f), MessageStatus::Sent);
        assert!(
            f.store
                .apply_receipt(f.conversation, 1, 0)
                .unwrap()
                .is_some()
        );
        assert_eq!(status(&f), MessageStatus::Delivered);
        assert!(
            f.store
                .apply_receipt(f.conversation, 1, 1)
                .unwrap()
                .is_some()
        );
        assert_eq!(status(&f), MessageStatus::Read);
        // A late, lower receipt moves nothing.
        assert!(
            f.store
                .apply_receipt(f.conversation, 0, 0)
                .unwrap()
                .is_none()
        );
        assert_eq!(status(&f), MessageStatus::Read);
        assert_eq!(
            f.store
                .message(f.conversation, client_msg_id)
                .unwrap()
                .unwrap()
                .sender_id,
            f.me
        );
    }

    #[test]
    fn the_own_message_delivered_back_is_confirmed_not_added() {
        let mut f = fixture();
        let client_msg_id = ClientMsgId::new();
        f.store
            .queue_send(f.conversation, client_msg_id, "mine", 1)
            .unwrap();
        let mut echo = entry(f.conversation, f.me, 1, EntryKind::Message);
        echo.client_msg_id = bytes(client_msg_id.0);
        let (applied, conversation, _) = f.store.apply_deliver(&echo).unwrap();
        assert!(matches!(applied, Applied::Confirmed(ref message) if message.seq == Some(1)));
        assert_eq!(
            conversation.unwrap().unread,
            0,
            "the user's own message is not unread"
        );
    }

    #[test]
    fn another_user_gets_an_empty_database() {
        let mut f = fixture();
        f.store
            .queue_send(f.conversation, ClientMsgId::new(), "hi", 1)
            .unwrap();
        assert!(!f.store.bind_owner(f.me).unwrap());
        assert!(f.store.bind_owner(UserId::new()).unwrap());
        assert!(f.store.outbox().unwrap().is_empty());
        assert!(f.store.conversations().unwrap().is_empty());
    }
}
```

Change `crates/push-sdk/src/store.rs`:

```diff
diff --git a/crates/push-sdk/src/store.rs b/crates/push-sdk/src/store.rs
index 2aa1b4f..a1ad9c6 100644
--- a/crates/push-sdk/src/store.rs
+++ b/crates/push-sdk/src/store.rs
@@ -19,6 +19,8 @@ use crate::model::{
     Contact, ContactState, Conversation, ConversationKind, Message, MessageKind, MessageStatus,
 };
 
+pub(crate) mod outbox;
+
 const SCHEMA_VERSION: i64 = 1;
 
 const SCHEMA: &str = "
```

Change `crates/push-sdk/src/model.rs`:

```diff
diff --git a/crates/push-sdk/src/model.rs b/crates/push-sdk/src/model.rs
index 3d23cfa..89cacb0 100644
--- a/crates/push-sdk/src/model.rs
+++ b/crates/push-sdk/src/model.rs
@@ -58,7 +58,7 @@ impl std::fmt::Debug for Redacted {
     }
 }
 
-fn redacted(text: &Option<String>) -> Option<Redacted> {
+pub(crate) fn redacted(text: &Option<String>) -> Option<Redacted> {
     text.as_ref().map(|text| Redacted(text.len()))
 }
 
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-sdk/src/store/outbox.rs`, in `due`, replace
`WHERE rowid IN (SELECT MIN(rowid) FROM outbox GROUP BY conversation_id)` and the line
after it (`AND state = 0 AND next_attempt_at <= ?1`) with
`WHERE state = 0 AND next_attempt_at <= ?1`, and run:

```powershell
cargo test -p push-sdk --lib --locked store::outbox
```

Expected: `test store::outbox::tests::the_outbox_sends_one_row_per_conversation_in_order ... FAILED`
(a second row of one conversation went out) and `5 passed; 1 failed`. Undo the edit.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "push-sdk: the outbox"
```

Expected: 903 tests pass, no warnings, no diff.

---

### Task 6: The database key

**Files:**
- Create: `crates/push-sdk/src/key.rs`
- Modify: `crates/push-sdk/src/store.rs`, `crates/push-sdk/src/lib.rs`

Decision 15 and owner decision 4. `DatabaseKey` (32 bytes, wiped on drop, never shown by
`Debug`), its Argon2id derivation and salts. `Store::open` takes the key: `PRAGMA hexkey`
first, then the first read decides (`WrongKey` before anything is written); `change_key`
encrypts, re-encrypts or decrypts in place outside WAL mode. The tests read the raw files:
an encrypted database and its log hold no message text and not even SQLite's header; a
wrong or missing key is refused and leaves the data as it was; a key can be added, changed
and removed.

- [ ] **Step 1: Make the changes**

Create `crates/push-sdk/src/key.rs`:

```rust
//! The key of the local database (owner decision): an app may open the client with a key,
//! and then the whole SQLite file — every page of the history, the outbox, the contacts and
//! the write-ahead log — is encrypted with it (SQLite3 Multiple Ciphers, its default
//! ChaCha20-Poly1305 scheme). Without the key nobody can read the user's messages from the
//! file, not even the app itself.
//!
//! The SDK never stores the key: the app provides it every time it opens the client, for
//! example by deriving it from a PIN or a password the user enters ([`DatabaseKey::derive`]).
//! A lost key cannot be recovered: the data must be erased and loaded again from the
//! server.

use argon2::{Algorithm, Argon2, Params, Version};
use rand::RngCore;
use zeroize::{Zeroize, Zeroizing};

use crate::SdkError;

/// A database key: 32 bytes. Wiped from memory when dropped; `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct DatabaseKey {
    bytes: [u8; DatabaseKey::LEN],
}

impl DatabaseKey {
    /// The length of a key, in bytes.
    pub const LEN: usize = 32;
    /// The length of a salt [`DatabaseKey::new_salt`] makes, in bytes; [`DatabaseKey::derive`]
    /// accepts any salt of at least this length.
    pub const SALT_LEN: usize = 16;

    /// A key from 32 bytes the app holds (from a hardware keystore, say).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SdkError> {
        let bytes: [u8; Self::LEN] = bytes
            .try_into()
            .map_err(|_| SdkError::Invalid(format!("a database key is {} bytes", Self::LEN)))?;
        Ok(Self { bytes })
    }

    /// Derives a key from what the user knows — a PIN, a password — with Argon2id
    /// (19 MiB, 2 passes, 1 lane: OWASP's recommendation, as auth-svc uses for passwords).
    /// `salt` is random, made once per database with [`DatabaseKey::new_salt`] and kept by
    /// the app next to it (it is not secret); the same secret and salt always give the same
    /// key. Takes a few tens of milliseconds: call it off the UI thread.
    pub fn derive(secret: &[u8], salt: &[u8]) -> Result<Self, SdkError> {
        if salt.len() < Self::SALT_LEN {
            return Err(SdkError::Invalid(format!(
                "a salt has at least {} bytes",
                Self::SALT_LEN
            )));
        }
        let params = Params::new(19 * 1024, 2, 1, Some(Self::LEN))
            .map_err(|error| SdkError::Invalid(error.to_string()))?;
        let mut bytes = [0; Self::LEN];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(secret, salt, &mut bytes)
            .map_err(|error| SdkError::Invalid(error.to_string()))?;
        Ok(Self { bytes })
    }

    /// A new random salt for [`DatabaseKey::derive`].
    pub fn new_salt() -> [u8; Self::SALT_LEN] {
        let mut salt = [0; Self::SALT_LEN];
        rand::rng().fill_bytes(&mut salt);
        salt
    }

    /// The key as SQLite3 Multiple Ciphers' `hexkey` and `hexrekey` take it.
    pub(crate) fn hex(&self) -> Zeroizing<String> {
        let mut hex = Zeroizing::new(String::with_capacity(Self::LEN * 2));
        for byte in self.bytes {
            hex.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
            hex.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
        }
        hex
    }
}

impl Drop for DatabaseKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl std::fmt::Debug for DatabaseKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DatabaseKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_derived_the_same_way_every_time_and_never_shown() {
        let salt = [7; DatabaseKey::SALT_LEN];
        let key = DatabaseKey::derive(b"1234", &salt).unwrap();
        assert_eq!(key, DatabaseKey::derive(b"1234", &salt).unwrap());
        assert_ne!(key, DatabaseKey::derive(b"1235", &salt).unwrap());
        assert_ne!(
            key,
            DatabaseKey::derive(b"1234", &DatabaseKey::new_salt()).unwrap()
        );
        assert_eq!(key.hex().len(), 64);
        assert_eq!(format!("{key:?}"), "DatabaseKey(<redacted>)");
        assert!(DatabaseKey::derive(b"1234", &[0; 8]).is_err());
        assert!(DatabaseKey::from_bytes(&[1; 31]).is_err());
        assert_eq!(
            DatabaseKey::from_bytes(&[0xab; 32]).unwrap().hex().as_str(),
            "ab".repeat(32)
        );
    }
}
```

Change `crates/push-sdk/src/store.rs`:

```diff
diff --git a/crates/push-sdk/src/store.rs b/crates/push-sdk/src/store.rs
index a1ad9c6..87470b3 100644
--- a/crates/push-sdk/src/store.rs
+++ b/crates/push-sdk/src/store.rs
@@ -6,6 +6,10 @@
 //! echo are written together, before the first attempt, so a retry after a restart sends
 //! the same `client_msg_id`. Positions only move forward (`MAX`), because receipts can
 //! arrive in any order (spec §7.2).
+//!
+//! Opened with a [`DatabaseKey`], the whole file is encrypted (SQLite3 Multiple Ciphers):
+//! the key is checked by the first read, before anything is written, so a wrong key never
+//! damages the data.
 
 use std::time::Duration;
 
@@ -15,6 +19,7 @@ use rusqlite::{Connection, OptionalExtension, Row, params};
 use uuid::Uuid;
 
 use crate::SdkError;
+use crate::key::DatabaseKey;
 use crate::model::{
     Contact, ContactState, Conversation, ConversationKind, Message, MessageKind, MessageStatus,
 };
@@ -106,10 +111,25 @@ pub(crate) struct Store {
 }
 
 impl Store {
-    /// Opens (or creates) the database at `path`; `:memory:` keeps it in memory.
-    pub fn open(path: &str) -> Result<Self, SdkError> {
+    /// Opens (or creates) the database at `path`; `:memory:` keeps it in memory. With a
+    /// key the file is encrypted; [`SdkError::WrongKey`] if the key does not open it (or a
+    /// database that has one is opened without, or one without with a key).
+    pub fn open(path: &str, key: Option<&DatabaseKey>) -> Result<Self, SdkError> {
         let conn = Connection::open(path)?;
         conn.busy_timeout(Duration::from_secs(5))?;
+        if let Some(key) = key {
+            conn.execute_batch(&format!("PRAGMA hexkey = '{}'", key.hex().as_str()))?;
+        }
+        // The first read of the file: it fails with "not a database" when the key does
+        // not fit, before anything is written.
+        if let Err(error) = conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
+            row.get::<_, i64>(0)
+        }) {
+            return Err(match error.sqlite_error_code() {
+                Some(rusqlite::ErrorCode::NotADatabase) => SdkError::WrongKey,
+                _ => error.into(),
+            });
+        }
         // WAL: a crash never loses a committed outbox row; NORMAL syncs at checkpoints.
         let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
         conn.pragma_update(None, "synchronous", "NORMAL")?;
@@ -128,6 +148,26 @@ impl Store {
         Ok(Self { conn })
     }
 
+    /// Encrypts the database with `key`, re-encrypts it with a new one, or with `None`
+    /// decrypts it. Every page is rewritten in one transaction: a crash leaves the old key
+    /// or the new one, never a mix. The next [`Store::open`] needs the new key.
+    pub fn change_key(&mut self, key: Option<&DatabaseKey>) -> Result<(), SdkError> {
+        // SQLite3 Multiple Ciphers changes keys only outside WAL mode.
+        let _mode: String = self
+            .conn
+            .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
+        let changed = match key {
+            Some(key) => self
+                .conn
+                .execute_batch(&format!("PRAGMA hexrekey = '{}'", key.hex().as_str())),
+            None => self.conn.execute_batch("PRAGMA rekey = ''"),
+        };
+        let _mode: String = self
+            .conn
+            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
+        Ok(changed?)
+    }
+
     /// The user the database belongs to.
     pub fn owner(&self) -> Result<Option<UserId>, SdkError> {
         let value: Option<Vec<u8>> = self
@@ -785,7 +825,7 @@ pub(crate) mod tests {
     }
 
     pub(crate) fn fixture() -> Fixture {
-        let mut store = Store::open(":memory:").unwrap();
+        let mut store = Store::open(":memory:", None).unwrap();
         let (me, peer) = (UserId::new(), UserId::new());
         store.bind_owner(me).unwrap();
         let conversation = ConversationId::new();
@@ -945,7 +985,7 @@ pub(crate) mod tests {
     #[test]
     fn erasing_leaves_no_trace_in_the_files() {
         let scratch = Scratch::new();
-        let mut store = Store::open(&scratch.database()).unwrap();
+        let mut store = Store::open(&scratch.database(), None).unwrap();
         let (me, peer, conversation) = (UserId::new(), UserId::new(), ConversationId::new());
         store.bind_owner(me).unwrap();
         store.open_direct(conversation, peer).unwrap();
@@ -972,6 +1012,99 @@ pub(crate) mod tests {
         assert!(!scratch.holds(&marker));
     }
 
+    /// Fills `store` for `me` with a delivered message and a queued one, both carrying
+    /// `marker`; the conversation.
+    fn fill(store: &mut Store, me: UserId, marker: &str) -> ConversationId {
+        let (peer, conversation) = (UserId::new(), ConversationId::new());
+        store.bind_owner(me).unwrap();
+        store.open_direct(conversation, peer).unwrap();
+        let mut delivered = entry(conversation, peer, 1, EntryKind::Message);
+        delivered.body = Some(format!("{marker} received").into_bytes());
+        store.apply_deliver(&delivered).unwrap();
+        store
+            .queue_send(
+                conversation,
+                ClientMsgId::new(),
+                &format!("{marker} queued"),
+                1,
+            )
+            .unwrap();
+        conversation
+    }
+
+    fn key(byte: u8) -> DatabaseKey {
+        DatabaseKey::from_bytes(&[byte; DatabaseKey::LEN]).unwrap()
+    }
+
+    #[test]
+    fn with_a_key_the_files_hold_no_plaintext() {
+        let scratch = Scratch::new();
+        let marker = format!("secret-{}", Uuid::now_v7());
+        let me = UserId::new();
+        let mut store = Store::open(&scratch.database(), Some(&key(1))).unwrap();
+        let conversation = fill(&mut store, me, &marker);
+        assert!(!scratch.holds(&marker), "the open database and its log");
+        drop(store);
+        assert!(!scratch.holds(&marker), "the closed database");
+        assert!(
+            !scratch.holds("SQLite format 3"),
+            "even the header is encrypted"
+        );
+
+        let store = Store::open(&scratch.database(), Some(&key(1))).unwrap();
+        assert_eq!(store.messages(conversation, None, 10).unwrap().len(), 2);
+    }
+
+    #[test]
+    fn a_wrong_or_missing_key_is_refused_and_changes_nothing() {
+        let scratch = Scratch::new();
+        let me = UserId::new();
+        let mut store = Store::open(&scratch.database(), Some(&key(1))).unwrap();
+        let conversation = fill(&mut store, me, "kept");
+        drop(store);
+        for wrong in [Some(key(2)), None] {
+            assert_eq!(
+                Store::open(&scratch.database(), wrong.as_ref()).err(),
+                Some(SdkError::WrongKey)
+            );
+        }
+        let store = Store::open(&scratch.database(), Some(&key(1))).unwrap();
+        assert_eq!(store.owner().unwrap(), Some(me));
+        assert_eq!(store.messages(conversation, None, 10).unwrap().len(), 2);
+        assert_eq!(store.outbox().unwrap().len(), 1);
+    }
+
+    #[test]
+    fn a_key_can_be_added_changed_and_removed() {
+        let scratch = Scratch::new();
+        let marker = format!("secret-{}", Uuid::now_v7());
+        let me = UserId::new();
+        let mut store = Store::open(&scratch.database(), None).unwrap();
+        let conversation = fill(&mut store, me, &marker);
+        store.change_key(Some(&key(1))).unwrap();
+        assert!(!scratch.holds(&marker), "encrypted in place");
+        drop(store);
+        assert_eq!(
+            Store::open(&scratch.database(), None).err(),
+            Some(SdkError::WrongKey)
+        );
+
+        let mut store = Store::open(&scratch.database(), Some(&key(1))).unwrap();
+        store.change_key(Some(&key(2))).unwrap();
+        drop(store);
+        assert_eq!(
+            Store::open(&scratch.database(), Some(&key(1))).err(),
+            Some(SdkError::WrongKey)
+        );
+
+        let mut store = Store::open(&scratch.database(), Some(&key(2))).unwrap();
+        store.change_key(None).unwrap();
+        drop(store);
+        let store = Store::open(&scratch.database(), None).unwrap();
+        assert_eq!(store.messages(conversation, None, 10).unwrap().len(), 2);
+        assert!(scratch.holds(&marker), "decrypted again");
+    }
+
     #[test]
     fn debug_output_never_shows_a_body() {
         let mut f = fixture();
```

Change `crates/push-sdk/src/lib.rs`:

```diff
diff --git a/crates/push-sdk/src/lib.rs b/crates/push-sdk/src/lib.rs
index 2802174..97ea65f 100644
--- a/crates/push-sdk/src/lib.rs
+++ b/crates/push-sdk/src/lib.rs
@@ -17,12 +17,14 @@
 mod auth;
 mod credentials;
 mod error;
+mod key;
 mod model;
 mod secrets;
 mod store;
 
 pub use auth::{Device, Platform, Profile};
 pub use error::SdkError;
+pub use key::DatabaseKey;
 pub use model::{
     ConnectionState, Contact, ContactState, Conversation, ConversationKind, Event, Invite, Message,
     MessageKind, MessageStatus, OutboxKind,
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-sdk/src/store.rs`, in `open`, change `"PRAGMA hexkey = '{}'"` to
`"PRAGMA no_such_pragma = '{}'"` (SQLite ignores pragmas it does not know, so the key is
silently not applied) and run:

```powershell
cargo test -p push-sdk --lib --locked store::tests
```

Expected: `with_a_key_the_files_hold_no_plaintext`,
`a_wrong_or_missing_key_is_refused_and_changes_nothing` and
`a_key_can_be_added_changed_and_removed` FAILED, and `6 passed; 3 failed`. Undo the edit.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "push-sdk: the database key"
```

Expected: 907 tests pass, no warnings, no diff.

---

### Task 7: Close codes, backoff, the frame budget and batched acks; the test doubles

**Files:**
- Create: `crates/push-sdk/src/policy.rs`, `crates/push-sdk/src/testing.rs`
- Modify: `crates/push-sdk/src/lib.rs`

Decision 4 and the frame-budget rule. Pure functions and two small state holders:
`after_close` (spec §7.4 and the owner's decisions: 4001 signs out, 4003 and 4007 stop),
`Backoff` (reconnects, 4006, the outbox), `first_ping`, `FrameBudget` (50 at once, 10 a
second) and `Acks` (one per conversation, the highest `seq`, after 50 ms of quiet or
250 ms at most). `testing.rs` (compiled for tests only) is a gateway the tests of Tasks 9
to 12 drive frame by frame and an auth-svc that answers sign-in, registration, refresh and
revocation with counters; it can delay its refresh answers (after counting them, as
auth-svc rotates first), fail revocations, answer one registration `device_not_found`, and
records the `registration_id`s it saw.

- [ ] **Step 1: Make the changes**

Create `crates/push-sdk/src/policy.rs`:

```rust
//! The connection's rules, free of I/O so they can be tested alone: what a close code
//! means (spec §7.4 and the owner's decisions), how long to wait before reconnecting, how
//! many frames the client may send (the gateway closes a connection that sends more than
//! 100 at once or 20 a second after that with 4004), and when to acknowledge deliveries.

use std::collections::BTreeMap;
use std::time::Duration;

use push_domain::ConversationId;
use push_proto::close;
use rand::Rng;
use tokio::time::Instant;

/// What to do after the connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterClose {
    /// 4001: the token is invalid or older than a password reset. Stop and ask the user
    /// to sign in again.
    SignOut,
    /// 4003: the device was revoked. Stop.
    DeviceRemoved,
    /// 4007: another connection of the device took over (the same app data opened
    /// elsewhere). Stop; the device is still signed in.
    OpenedElsewhere,
    /// 4002: the token expired. Refresh it and reconnect at once.
    Refresh,
    /// 4006: the device was registered moments ago and is not relayed yet. Reconnect
    /// soon; never a reason to sign out.
    NotReadyYet,
    /// Anything else — 4004, 4005, 1002, 1008, 1009, 1011, a network error, a missing
    /// close frame: reconnect with backoff.
    Reconnect,
}

/// The action for a close code; `None` when the connection ended without one.
pub(crate) fn after_close(code: Option<u16>) -> AfterClose {
    match code {
        Some(close::INVALID_IDENTITY) => AfterClose::SignOut,
        Some(close::DEVICE_REMOVED) => AfterClose::DeviceRemoved,
        Some(close::OPENED_ELSEWHERE) => AfterClose::OpenedElsewhere,
        Some(close::REAUTH_REQUIRED) => AfterClose::Refresh,
        Some(close::NOT_READY_YET) => AfterClose::NotReadyYet,
        _ => AfterClose::Reconnect,
    }
}

/// Exponential backoff with jitter (spec §7.3): the delay doubles from `base` to `cap`,
/// and each delay is drawn between half of it and all of it, so clients that dropped
/// together do not come back together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Backoff {
    pub base: Duration,
    pub cap: Duration,
}

impl Backoff {
    /// Reconnects after an error: 1 s up to 60 s.
    pub const RECONNECT: Self = Self {
        base: Duration::from_secs(1),
        cap: Duration::from_secs(60),
    };
    /// Reconnects while the device is not relayed yet (4006): 500 ms up to 5 s, so the
    /// worker's relay, about a second, is caught quickly while the gateway's limit of 30
    /// `Identify` a minute per device is never reached (at least 2.5 s apart once capped).
    pub const NOT_READY: Self = Self {
        base: Duration::from_millis(500),
        cap: Duration::from_secs(5),
    };
    /// Retries of an outbox row the server could not take (spec §8.4): 1 s up to 60 s.
    pub const OUTBOX: Self = Self {
        base: Duration::from_secs(1),
        cap: Duration::from_secs(60),
    };

    /// The delay before attempt `attempt` (0 for the first retry).
    pub fn delay(self, attempt: u32) -> Duration {
        let full = self.full(attempt);
        let half = full / 2;
        half + rand::rng().random_range(Duration::ZERO..=full - half)
    }

    /// The delay without jitter.
    fn full(self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.cap)
    }
}

/// When to send the first `Ping` after `Ready`: somewhere within one heartbeat, so
/// clients that reconnected together do not ping together (spec §7.3).
pub(crate) fn first_ping(heartbeat: Duration) -> Duration {
    rand::rng().random_range(Duration::ZERO..=heartbeat)
}

/// The client's own frame budget, below the gateway's 100 at once and 20 a second: a
/// token bucket of 50 frames refilled at 10 a second. A frame that finds the bucket empty
/// waits rather than being dropped.
#[derive(Debug, Clone)]
pub(crate) struct FrameBudget {
    tokens: f64,
    updated: Instant,
}

impl FrameBudget {
    pub const BURST: f64 = 50.0;
    pub const PER_SECOND: f64 = 10.0;

    pub fn new(now: Instant) -> Self {
        Self {
            tokens: Self::BURST,
            updated: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * Self::PER_SECOND).min(Self::BURST);
        self.updated = now;
    }

    /// How long to wait before one more frame may go: zero if it may go now.
    pub fn wait(&mut self, now: Instant) -> Duration {
        self.refill(now);
        if self.tokens >= 1.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - self.tokens) / Self::PER_SECOND)
        }
    }

    /// Counts one frame sent.
    pub fn spend(&mut self, now: Instant) {
        self.refill(now);
        self.tokens -= 1.0;
    }
}

/// Acknowledgements of deliveries, batched (spec §7.3, the frame budget): every stored
/// entry raises its conversation's position, and one `Ack` per conversation goes out once
/// deliveries pause for 50 ms, or 250 ms after the first one waiting. A backlog of a
/// thousand entries is acknowledged with a handful of frames, each naming only the
/// highest `seq`, which acknowledges everything before it.
#[derive(Debug, Default)]
pub(crate) struct Acks {
    waiting: BTreeMap<ConversationId, i64>,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Acks {
    pub const QUIET: Duration = Duration::from_millis(50);
    pub const MAX_DELAY: Duration = Duration::from_millis(250);

    /// `conversation` holds everything up to `seq` now.
    pub fn stored(&mut self, conversation: ConversationId, seq: i64, now: Instant) {
        let position = self.waiting.entry(conversation).or_insert(seq);
        *position = (*position).max(seq);
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// When the waiting acknowledgements are due, if any wait.
    pub fn due_at(&self) -> Option<Instant> {
        match (self.first, self.last) {
            (Some(first), Some(last)) => Some((last + Self::QUIET).min(first + Self::MAX_DELAY)),
            _ => None,
        }
    }

    /// The acknowledgements to send: the highest `seq` per conversation.
    pub fn take(&mut self) -> Vec<(ConversationId, i64)> {
        self.first = None;
        self.last = None;
        std::mem::take(&mut self.waiting).into_iter().collect()
    }

    /// Forgets what waits: the connection dropped, and the next one is acknowledged from
    /// the store's positions.
    pub fn clear(&mut self) {
        self.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_4001_signs_out_and_only_4003_and_4007_stop() {
        assert_eq!(after_close(Some(4001)), AfterClose::SignOut);
        assert_eq!(after_close(Some(4003)), AfterClose::DeviceRemoved);
        assert_eq!(after_close(Some(4007)), AfterClose::OpenedElsewhere);
        assert_eq!(after_close(Some(4002)), AfterClose::Refresh);
        assert_eq!(after_close(Some(4006)), AfterClose::NotReadyYet);
        for code in [4004, 4005, 1000, 1001, 1002, 1006, 1008, 1009, 1011] {
            assert_eq!(after_close(Some(code)), AfterClose::Reconnect, "{code}");
        }
        assert_eq!(after_close(None), AfterClose::Reconnect);
    }

    #[test]
    fn the_backoff_doubles_up_to_its_cap_with_jitter() {
        let backoff = Backoff::RECONNECT;
        for _ in 0..100 {
            let first = backoff.delay(0);
            assert!(first >= Duration::from_millis(500) && first <= Duration::from_secs(1));
            let fourth = backoff.delay(3);
            assert!(fourth >= Duration::from_secs(4) && fourth <= Duration::from_secs(8));
            let late = backoff.delay(40);
            assert!(late >= Duration::from_secs(30) && late <= Duration::from_secs(60));
        }
        let not_ready = Backoff::NOT_READY;
        assert!(not_ready.delay(0) <= Duration::from_millis(500));
        assert!(not_ready.delay(10) >= Duration::from_millis(2500));
        assert!(first_ping(Duration::from_secs(30)) <= Duration::from_secs(30));
    }

    #[test]
    fn the_frame_budget_stays_below_the_gateways() {
        let start = Instant::now();
        let mut budget = FrameBudget::new(start);
        for _ in 0..50 {
            assert_eq!(budget.wait(start), Duration::ZERO);
            budget.spend(start);
        }
        assert!(budget.wait(start) > Duration::ZERO, "the burst is used up");
        let later = start + Duration::from_millis(100);
        assert_eq!(budget.wait(later), Duration::ZERO, "10 a second refill");
    }

    #[test]
    fn a_backlog_is_acknowledged_with_one_ack_per_conversation() {
        let start = Instant::now();
        let mut acks = Acks::default();
        assert_eq!(acks.due_at(), None);
        let (a, b) = (ConversationId::new(), ConversationId::new());
        for seq in 1..=500 {
            acks.stored(a, seq, start);
        }
        acks.stored(b, 7, start);
        assert_eq!(acks.due_at(), Some(start + Acks::QUIET));
        let mut expected = vec![(a, 500), (b, 7)];
        expected.sort();
        assert_eq!(acks.take(), expected);
        assert_eq!(acks.due_at(), None);
    }

    #[test]
    fn a_steady_trickle_is_still_acknowledged_within_250_ms() {
        let start = Instant::now();
        let mut acks = Acks::default();
        let conversation = ConversationId::new();
        for step in 0..10 {
            acks.stored(
                conversation,
                step + 1,
                start + Duration::from_millis(40 * step as u64),
            );
        }
        assert_eq!(acks.due_at(), Some(start + Acks::MAX_DELAY));
    }
}
```

Create `crates/push-sdk/src/testing.rs`:

```rust
//! Test doubles shared by the connection's and the client's tests: a gateway driven frame
//! by frame, and auth-svc answering with counters.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, post};
use futures_util::{SinkExt, StreamExt};
use push_domain::{DeviceId, UserId};
use push_proto::v1::frame::Body;
use push_proto::{decode_frame, encode_frame};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_async};

pub(crate) type Server = WebSocketStream<TcpStream>;

/// A gateway the test drives frame by frame: each accepted connection is handed over.
pub(crate) struct FakeGateway {
    pub url: String,
    connections: mpsc::UnboundedReceiver<Server>,
}

impl FakeGateway {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
        let (sender, connections) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if let Ok(socket) = accept_async(stream).await {
                    let _ = sender.send(socket);
                }
            }
        });
        Self { url, connections }
    }

    pub async fn next(&mut self) -> Server {
        timeout(Duration::from_secs(10), self.connections.recv())
            .await
            .expect("a connection within 10 s")
            .unwrap()
    }

    /// No connection comes within `wait`.
    pub async fn quiet(&mut self, wait: Duration) {
        assert!(timeout(wait, self.connections.recv()).await.is_err());
    }
}

/// The next frame from the client.
pub(crate) async fn recv(socket: &mut Server) -> Body {
    loop {
        let message = timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a frame within 10 s")
            .unwrap()
            .unwrap();
        if let Message::Binary(bytes) = message {
            return decode_frame(&bytes).unwrap();
        }
    }
}

/// The next frame `wanted` accepts, skipping the others.
pub(crate) async fn recv_until(socket: &mut Server, wanted: impl Fn(&Body) -> bool) -> Body {
    loop {
        let body = recv(socket).await;
        if wanted(&body) {
            return body;
        }
    }
}

pub(crate) async fn send(socket: &mut Server, body: Body) {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .unwrap();
}

pub(crate) async fn close(socket: &mut Server, code: u16) {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: "".into(),
    };
    let _ = socket.close(Some(frame)).await;
}

/// auth-svc for one user and device: sign-in, registration, refresh (answering
/// `access-N`) and revocation, each counted.
pub(crate) struct FakeAuth {
    pub url: String,
    pub user: UserId,
    pub device: DeviceId,
    pub registrations: Arc<AtomicUsize>,
    pub refreshes: Arc<AtomicUsize>,
    pub device_logins: Arc<AtomicUsize>,
    pub revocations: Arc<AtomicUsize>,
    /// How long a refresh takes after auth-svc counted (and rotated) it.
    pub refresh_delay_ms: Arc<AtomicU64>,
    /// Revocations fail with 500 while set.
    pub fail_revocations: Arc<AtomicBool>,
    /// The next registration answers `device_not_found`, as for a revoked device's id.
    pub spend_next_registration: Arc<AtomicBool>,
    /// The `registration_id` of every registration, in order.
    pub registration_ids: Arc<Mutex<Vec<String>>>,
}

impl FakeAuth {
    pub async fn start() -> Self {
        let (user, device) = (UserId::new(), DeviceId::new());
        let registrations = Arc::new(AtomicUsize::new(0));
        let refreshes = Arc::new(AtomicUsize::new(0));
        let device_logins = Arc::new(AtomicUsize::new(0));
        let revocations = Arc::new(AtomicUsize::new(0));
        let refresh_delay_ms = Arc::new(AtomicU64::new(0));
        let fail_revocations = Arc::new(AtomicBool::new(false));
        let spend_next_registration = Arc::new(AtomicBool::new(false));
        let registration_ids = Arc::new(Mutex::new(Vec::new()));
        let tokens = move |n: usize| {
            json!({
                "user_id": user,
                "device_id": device,
                "access_token": format!("access-{n}"),
                "expires_in_s": 900,
                "refresh_token": format!("refresh-{n}"),
            })
        };
        let (logins, registered, refreshed, revoked, delay) = (
            device_logins.clone(),
            registrations.clone(),
            refreshes.clone(),
            revocations.clone(),
            refresh_delay_ms.clone(),
        );
        let (fail, spend, ids) = (
            fail_revocations.clone(),
            spend_next_registration.clone(),
            registration_ids.clone(),
        );
        let router = Router::new()
            .route(
                "/v1/auth/login",
                post(move |Json(body): Json<Value>| {
                    let logins = logins.clone();
                    async move {
                        if body.get("device_id").is_some() {
                            let n = logins.fetch_add(1, Ordering::SeqCst) + 100;
                            Json(tokens(n)).into_response()
                        } else {
                            Json(json!({"registration_token": "registration", "expires_in_s": 300}))
                                .into_response()
                        }
                    }
                }),
            )
            .route(
                "/v1/devices",
                post(move |Json(body): Json<Value>| {
                    let (registered, spend, ids) = (registered.clone(), spend.clone(), ids.clone());
                    async move {
                        let id = body["registration_id"].as_str().expect("a registration_id");
                        ids.lock().unwrap().push(id.to_owned());
                        if spend.swap(false, Ordering::SeqCst) {
                            let error = json!({"error": {"code": "device_not_found", "message": "no such device", "retryable": false}});
                            return (StatusCode::NOT_FOUND, Json(error)).into_response();
                        }
                        registered.fetch_add(1, Ordering::SeqCst);
                        (StatusCode::CREATED, Json(tokens(0))).into_response()
                    }
                }),
            )
            .route(
                "/v1/devices/{id}",
                delete(move || {
                    let (revoked, fail) = (revoked.clone(), fail.clone());
                    async move {
                        if fail.load(Ordering::SeqCst) {
                            return StatusCode::INTERNAL_SERVER_ERROR;
                        }
                        revoked.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }),
            )
            .route(
                "/v1/auth/refresh",
                post(move || {
                    let (refreshed, delay) = (refreshed.clone(), delay.clone());
                    async move {
                        let n = refreshed.fetch_add(1, Ordering::SeqCst) + 1;
                        let delay = Duration::from_millis(delay.load(Ordering::SeqCst));
                        tokio::time::sleep(delay).await;
                        Json(tokens(n))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            url,
            user,
            device,
            registrations,
            refreshes,
            device_logins,
            revocations,
            refresh_delay_ms,
            fail_revocations,
            spend_next_registration,
            registration_ids,
        }
    }
}
```

Change `crates/push-sdk/src/lib.rs`:

```diff
diff --git a/crates/push-sdk/src/lib.rs b/crates/push-sdk/src/lib.rs
index 97ea65f..9aba1d1 100644
--- a/crates/push-sdk/src/lib.rs
+++ b/crates/push-sdk/src/lib.rs
@@ -19,8 +19,11 @@ mod credentials;
 mod error;
 mod key;
 mod model;
+mod policy;
 mod secrets;
 mod store;
+#[cfg(test)]
+mod testing;
 
 pub use auth::{Device, Platform, Profile};
 pub use error::SdkError;
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-sdk/src/policy.rs`, change
`Some(close::OPENED_ELSEWHERE) => AfterClose::OpenedElsewhere,` to
`Some(close::OPENED_ELSEWHERE) => AfterClose::Reconnect,` and run:

```powershell
cargo test -p push-sdk --lib --locked policy
```

Expected: `test policy::tests::only_4001_signs_out_and_only_4003_and_4007_stop ... FAILED`
and `4 passed; 1 failed`. Undo the edit.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "push-sdk: close codes, backoff, frame budget and batched acks; the test doubles"
```

Expected: 912 tests pass, no warnings, no diff.

---

### Task 8: The engine: deliveries, acks, the outbox, receipts and listings

**Files:**
- Create: `crates/push-sdk/src/engine.rs`
- Modify: `crates/push-sdk/src/lib.rs`, `crates/push-sdk/src/store.rs`

Decisions 8–11. `Shared` (the store behind a mutex and the bounded event queue with its
`EventsLost` rule) is what the connection and the app's calls share. `Engine` has no
socket: `start` (a connection is `Ready`: list conversations and contacts with its own
request ids, from `1 << 63` up, send unconfirmed read positions and the outbox),
`on_frame` (deliveries, `SendAck`, errors that name an outbox row, listings, receipts, read
positions, contact events; the rest is handed back), `wake`, `flush_acks`, `retry_due`,
`stop`, and `broken`, which tells the connection to end after a delivery the store could
not take. `Store::set_read_only` (tests only) makes every write fail, as a full disk would.
Twelve tests drive it frame by frame, among them a backlog of 300 deliveries acknowledged
with one `Ack` naming 300.

- [ ] **Step 1: Make the changes**

Create `crates/push-sdk/src/engine.rs`:

```rust
//! What rides on a connection, kept apart from the socket so it can be tested without one
//! (spec §4, §8.3, §8.4): deliveries are stored and then acknowledged in batches, the
//! outbox is sent one row per conversation in order, receipts, read positions and contact
//! changes are applied, and each change becomes an event for the UI. The connection
//! (`connection.rs`) feeds the [`Engine`] the frames it receives and sends the frames the
//! engine asks for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use push_domain::{ClientMsgId, ConversationId};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, ListContacts, ListConversations, ReadUpTo, request, response};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::Instant;
use uuid::Uuid;

use crate::SdkError;
use crate::model::{Contact, ContactState, Event, OutboxKind};
use crate::policy::{Acks, Backoff};
use crate::store::{Applied, Store, bytes};

/// How many events wait for the app at most. An app that stops reading loses the newer
/// ones and is told so with [`Event::EventsLost`]; the store holds everything anyway.
pub(crate) const EVENT_QUEUE: usize = 10_000;

/// What the connection and the app's calls share: the store and the event stream.
pub(crate) struct Shared {
    store: Mutex<Store>,
    events: Mutex<Queue>,
}

/// The sending end of the event queue.
struct Queue {
    sender: mpsc::Sender<Event>,
    /// Events were dropped and `EventsLost` is in the queue for them.
    lost: bool,
}

impl Shared {
    /// `events` is bounded ([`EVENT_QUEUE`]).
    pub fn new(store: Store, events: mpsc::Sender<Event>) -> Self {
        Self {
            store: Mutex::new(store),
            events: Mutex::new(Queue {
                sender: events,
                lost: false,
            }),
        }
    }

    /// The store. Each use is short: one statement or one transaction.
    pub fn store(&self) -> MutexGuard<'_, Store> {
        self.store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Tells the UI. An app that dropped its event stream is not told. When only one
    /// place is left in the queue, events are dropped and the last place takes one
    /// `EventsLost`, which tells the app to reload from the store once it reads again.
    pub fn emit(&self, event: Event) {
        let mut queue = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.sender.capacity() > 1 {
            queue.lost = false;
            let _ = queue.sender.try_send(event);
        } else if !queue.lost {
            queue.lost = true;
            if let Err(TrySendError::Full(_)) = queue.sender.try_send(Event::EventsLost) {
                tracing::warn!("the event queue is full");
            }
        }
    }
}

/// Most outbox rows one wake sends: the rest follow as `SendAck`s come back, so a full
/// outbox never holds the connection in the frame budget for long.
const MAX_SENDS_AT_ONCE: usize = 20;

/// The engine's own requests have ids from here up; the app's count up from 1.
pub(crate) const INTERNAL_REQUESTS: u64 = 1 << 63;

/// One of the engine's own requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Internal {
    Conversations,
    Contacts,
}

pub(crate) struct Engine {
    shared: Arc<Shared>,
    acks: Acks,
    /// Read positions sent as `ReadUpTo` on this connection.
    reads_sent: HashMap<ConversationId, i64>,
    next_internal: u64,
    internal: HashMap<u64, Internal>,
    /// Another conversation listing is wanted once the one in flight is answered.
    relist: bool,
    /// A delivery could not be stored: the connection must end (see `on_deliver`).
    broken: bool,
}

impl Engine {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            acks: Acks::default(),
            reads_sent: HashMap::new(),
            next_internal: INTERNAL_REQUESTS,
            internal: HashMap::new(),
            relist: false,
            broken: false,
        }
    }

    /// A delivery could not be stored: end the connection and reconnect with backoff.
    pub fn broken(&self) -> bool {
        self.broken
    }

    /// A connection is `Ready`: list conversations and contacts (spec §4.5, to catch up on
    /// receipts and changes missed while away), send unconfirmed read positions, and
    /// start the outbox. Rows left in flight by the previous connection go again with
    /// the same `client_msg_id`.
    pub fn start(&mut self, now_ms: i64, out: &mut Vec<Body>) {
        self.stop();
        self.ask(Internal::Conversations, out);
        self.ask(Internal::Contacts, out);
        self.wake(now_ms, out);
    }

    /// The connection ended.
    pub fn stop(&mut self) {
        if let Err(error) = self.shared.store().requeue_in_flight() {
            tracing::warn!(code = error.code(), "could not requeue the outbox");
        }
        self.acks.clear();
        self.reads_sent.clear();
        self.internal.clear();
        self.relist = false;
        self.broken = false;
    }

    /// The app queued something or marked something read.
    pub fn wake(&mut self, now_ms: i64, out: &mut Vec<Body>) {
        let reads = self.shared.store().unsynced_reads();
        match reads {
            Ok(reads) => {
                for (conversation, seq) in reads {
                    if self
                        .reads_sent
                        .get(&conversation)
                        .is_some_and(|sent| *sent >= seq)
                    {
                        continue;
                    }
                    self.reads_sent.insert(conversation, seq);
                    out.push(Body::ReadUpTo(ReadUpTo {
                        conversation_id: bytes(conversation.0),
                        seq,
                    }));
                }
            }
            Err(error) => tracing::warn!(code = error.code(), "could not read the read positions"),
        }
        self.pump(now_ms, out);
    }

    /// Sends the outbox rows that are due: the first of each conversation, one in flight
    /// at a time (spec §4.2, §8.4).
    fn pump(&mut self, now_ms: i64, out: &mut Vec<Body>) {
        let mut store = self.shared.store();
        let due = match store.due(now_ms) {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!(code = error.code(), "could not read the outbox");
                return;
            }
        };
        for row in due.into_iter().take(MAX_SENDS_AT_ONCE) {
            if let Err(error) = store.mark_in_flight(row.client_msg_id) {
                tracing::warn!(code = error.code(), "could not mark an outbox row");
                return;
            }
            out.push(row.frame());
        }
    }

    /// When the next outbox row waiting for a retry is due, in milliseconds since the
    /// Unix epoch, if it is later than `now_ms`.
    pub fn retry_due(&self, now_ms: i64) -> Option<i64> {
        self.shared
            .store()
            .next_due()
            .ok()
            .flatten()
            .filter(|due| *due > now_ms)
    }

    /// When the waiting acknowledgements are due.
    pub fn acks_due(&self) -> Option<Instant> {
        self.acks.due_at()
    }

    /// One `Ack` per conversation, naming the highest `seq` stored.
    pub fn flush_acks(&mut self, out: &mut Vec<Body>) {
        for (conversation, seq) in self.acks.take() {
            out.push(Body::Ack(v1::Ack {
                conversation_id: bytes(conversation.0),
                seq,
            }));
        }
    }

    fn ask(&mut self, what: Internal, out: &mut Vec<Body>) {
        let request_id = self.next_internal;
        self.next_internal += 1;
        self.internal.insert(request_id, what);
        let body = match what {
            Internal::Conversations => {
                request::Body::ListConversations(ListConversations::default())
            }
            Internal::Contacts => request::Body::ListContacts(ListContacts {}),
        };
        out.push(Body::Request(v1::Request {
            request_id,
            body: Some(body),
        }));
    }

    /// Lists the conversations again, once the listing in flight is answered.
    fn relist(&mut self, out: &mut Vec<Body>) {
        if self
            .internal
            .values()
            .any(|what| *what == Internal::Conversations)
        {
            self.relist = true;
        } else {
            self.ask(Internal::Conversations, out);
        }
    }

    /// Handles a frame from the gateway. Frames that are the connection's business
    /// (answers to the app's requests, `AuthExpiring`, `Pong`, errors that name no outbox
    /// row) are handed back.
    pub fn on_frame(
        &mut self,
        body: Body,
        now: Instant,
        now_ms: i64,
        out: &mut Vec<Body>,
    ) -> Option<Body> {
        match body {
            Body::Deliver(entry) => self.on_deliver(&entry, now, out),
            Body::SendAck(ack) => self.on_send_ack(&ack, now_ms, out),
            Body::Error(error) if error.client_msg_id.is_some() => {
                self.on_send_error(&error, now_ms, out);
            }
            Body::Error(error)
                if error
                    .request_id
                    .is_some_and(|id| self.internal.contains_key(&id)) =>
            {
                let what = error.request_id.and_then(|id| self.internal.remove(&id));
                tracing::info!(?what, code = ?error.code(), "a listing failed; the next connection lists again");
            }
            Body::Response(answer) if self.internal.contains_key(&answer.request_id) => {
                let what = self.internal.remove(&answer.request_id);
                self.on_listing(what, answer.body, out);
            }
            Body::Receipt(receipt) => self.on_receipt(&receipt),
            Body::ReadPosition(position) => self.on_read_position(&position),
            Body::ContactEvent(event) => self.on_contact_event(&event, out),
            other => return Some(other),
        }
        None
    }

    fn on_deliver(&mut self, entry: &v1::Deliver, now: Instant, out: &mut Vec<Body>) {
        let Ok(conversation) = Uuid::from_slice(&entry.conversation_id).map(ConversationId) else {
            tracing::warn!("a delivery names no conversation");
            return;
        };
        let result = {
            let mut store = self.shared.store();
            store
                .apply_deliver(entry)
                .and_then(|applied| Ok((applied, store.received(conversation)?)))
        };
        let ((applied, changed, created), received) = match result {
            Ok(result) => result,
            // An entry with ids that are not ids can never be stored: skipped.
            Err(error @ SdkError::Network(_)) => {
                tracing::warn!(code = error.code(), "skipped a malformed delivery");
                return;
            }
            Err(error) => {
                // Not acknowledged, and nothing after it may be on this connection: the
                // gateway does not send an entry twice on one connection, and the next
                // Ack, naming a higher seq, would cover this one for good. The connection
                // ends; the next one syncs again from the last Ack.
                tracing::warn!(
                    code = error.code(),
                    "could not store a delivery: reconnecting"
                );
                self.broken = true;
                return;
            }
        };
        match applied {
            Applied::Added(message) => self.shared.emit(Event::MessageAdded(message)),
            Applied::Confirmed(message) => self.shared.emit(Event::MessageSent(message)),
            Applied::Edited(message) => self.shared.emit(Event::MessageEdited(message)),
            Applied::Deleted(message) => self.shared.emit(Event::MessageDeleted(message)),
            Applied::Duplicate | Applied::Skipped => {}
        }
        if let Some(conversation) = changed {
            self.shared.emit(Event::ConversationChanged(conversation));
        }
        if created {
            // Its kind and peer come with the listing.
            self.relist(out);
        }
        // A replay is acknowledged again: the last ack may have been lost.
        self.acks.stored(conversation, received, now);
    }

    fn on_send_ack(&mut self, ack: &v1::SendAck, now_ms: i64, out: &mut Vec<Body>) {
        let acknowledged = self.shared.store().acknowledged(ack);
        let row = match acknowledged {
            Ok(Some(row)) => row,
            // Acknowledged before, on an earlier attempt.
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(code = error.code(), "could not record a SendAck");
                return;
            }
        };
        let event = {
            let mut store = self.shared.store();
            match (row.kind, row.target_client_msg_id) {
                (OutboxKind::Send, _) => store
                    .message(row.conversation_id, row.client_msg_id)
                    .map(|message| message.map(Event::MessageSent)),
                (OutboxKind::Edit, Some(target)) => store
                    .edit(
                        row.conversation_id,
                        target,
                        row.body.as_deref().unwrap_or_default().as_bytes(),
                        ack.server_ts,
                        ack.seq,
                    )
                    .map(|applied| match applied {
                        Applied::Edited(message) => Some(Event::MessageEdited(message)),
                        _ => None,
                    }),
                (OutboxKind::Delete, Some(target)) => store
                    .erase(row.conversation_id, target, ack.seq)
                    .map(|applied| match applied {
                        Applied::Deleted(message) => Some(Event::MessageDeleted(message)),
                        _ => None,
                    }),
                (_, None) => Ok(None),
            }
        };
        match event {
            Ok(Some(event)) => self.shared.emit(event),
            Ok(None) => {}
            Err(error) => tracing::warn!(code = error.code(), "could not apply a SendAck"),
        }
        self.pump(now_ms, out);
    }

    /// A send, edit or delete failed: retried later if the server says it may succeed,
    /// else it leaves the outbox (with whatever depended on it) and the UI is told.
    fn on_send_error(&mut self, error: &v1::Error, now_ms: i64, out: &mut Vec<Body>) {
        let Some(id) = error
            .client_msg_id
            .as_deref()
            .and_then(|id| Uuid::from_slice(id).ok())
            .map(ClientMsgId)
        else {
            return;
        };
        let outcome = {
            let mut store = self.shared.store();
            match store.outbox_row(id) {
                Ok(Some(row)) if error.retryable => {
                    let delay_ms = match error.retry_after_s {
                        Some(seconds) => i64::from(seconds) * 1000,
                        None => {
                            let delay = Backoff::OUTBOX.delay(row.attempts.saturating_sub(1));
                            i64::try_from(delay.as_millis()).unwrap_or(i64::MAX)
                        }
                    };
                    store.retry_at(id, now_ms + delay_ms).map(|()| Vec::new())
                }
                Ok(Some(_)) => store.refused(id),
                Ok(None) => Ok(Vec::new()),
                Err(error) => Err(error),
            }
        };
        match outcome {
            Ok(removed) => {
                for row in removed {
                    self.shared.emit(Event::SendFailed {
                        conversation_id: row.conversation_id,
                        client_msg_id: row.client_msg_id,
                        kind: row.kind,
                        error: SdkError::from_frame(error),
                    });
                }
            }
            Err(error) => tracing::warn!(code = error.code(), "could not record a failed send"),
        }
        self.pump(now_ms, out);
    }

    fn on_listing(
        &mut self,
        what: Option<Internal>,
        answer: Option<response::Body>,
        out: &mut Vec<Body>,
    ) {
        match (what, answer) {
            (Some(Internal::Conversations), Some(response::Body::Conversations(list))) => {
                for summary in &list.conversations {
                    let applied = self.shared.store().apply_summary(summary);
                    match applied {
                        Ok(Some(conversation)) => {
                            self.shared.emit(Event::ConversationChanged(conversation));
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(code = error.code(), "could not store a listing");
                        }
                    }
                }
                if std::mem::take(&mut self.relist) {
                    self.ask(Internal::Conversations, out);
                }
            }
            (Some(Internal::Contacts), Some(response::Body::Contacts(list))) => {
                let listed: Vec<Contact> = list
                    .contacts
                    .iter()
                    .filter_map(|contact| {
                        Some(Contact {
                            user_id: Uuid::from_slice(&contact.user_id).ok()?.into(),
                            state: ContactState::from_wire(contact.state()),
                        })
                    })
                    .collect();
                let replaced = self.shared.store().replace_contacts(&listed);
                match replaced {
                    Ok(changed) => {
                        for contact in changed {
                            self.shared.emit(Event::ContactChanged(contact));
                        }
                    }
                    Err(error) => tracing::warn!(code = error.code(), "could not store contacts"),
                }
            }
            _ => tracing::warn!("a listing was answered with something else"),
        }
    }

    /// Receipts can arrive in any order: the store keeps the highest positions, and
    /// only a move is reported.
    fn on_receipt(&mut self, receipt: &v1::Receipt) {
        let Ok(conversation) = Uuid::from_slice(&receipt.conversation_id).map(ConversationId)
        else {
            return;
        };
        let (delivered, read) = match &receipt.positions {
            Some(v1::receipt::Positions::Direct(direct)) => (direct.delivered_seq, direct.read_seq),
            Some(v1::receipt::Positions::Group(group)) => {
                (group.all_delivered_seq, group.all_read_seq)
            }
            None => return,
        };
        let applied = self
            .shared
            .store()
            .apply_receipt(conversation, delivered, read);
        match applied {
            Ok(Some(moved)) => self.shared.emit(Event::ReceiptChanged {
                conversation_id: moved.id,
                peer_delivered_seq: moved.peer_delivered_seq,
                peer_read_seq: moved.peer_read_seq,
            }),
            Ok(None) => {}
            Err(error) => tracing::warn!(code = error.code(), "could not store a receipt"),
        }
    }

    fn on_read_position(&mut self, position: &v1::ReadPosition) {
        let Ok(conversation) = Uuid::from_slice(&position.conversation_id).map(ConversationId)
        else {
            return;
        };
        let applied = self
            .shared
            .store()
            .apply_read_position(conversation, position.read_seq);
        match applied {
            Ok(Some(changed)) => self.shared.emit(Event::ConversationChanged(changed)),
            Ok(None) => {}
            Err(error) => tracing::warn!(code = error.code(), "could not store a read position"),
        }
    }

    /// A relationship changed. Block and unblock hide and show conversations without
    /// changing them, so the conversations are listed again (spec §4.5).
    fn on_contact_event(&mut self, event: &v1::ContactEvent, out: &mut Vec<Body>) {
        let Ok(user) = Uuid::from_slice(&event.user_id) else {
            return;
        };
        let contact = Contact {
            user_id: user.into(),
            state: ContactState::from_wire(event.state()),
        };
        let changed = self.shared.store().set_contact(contact);
        match changed {
            Ok(true) => self.shared.emit(Event::ContactChanged(contact)),
            Ok(false) => {}
            Err(error) => tracing::warn!(code = error.code(), "could not store a contact"),
        }
        self.relist(out);
    }
}

#[cfg(test)]
mod tests {
    use push_domain::{MessageId, UserId};
    use push_proto::v1::{EntryKind, ErrorCode};

    use super::*;
    use crate::model::{ConversationKind, MessageStatus};
    use crate::store::tests::entry;

    struct Fixture {
        engine: Engine,
        shared: Arc<Shared>,
        events: mpsc::Receiver<Event>,
        me: UserId,
        peer: UserId,
        conversation: ConversationId,
    }

    fn fixture() -> Fixture {
        let mut store = Store::open(":memory:", None).unwrap();
        let (me, peer) = (UserId::new(), UserId::new());
        store.bind_owner(me).unwrap();
        let conversation = ConversationId::new();
        store.open_direct(conversation, peer).unwrap();
        let (sender, events) = mpsc::channel(EVENT_QUEUE);
        let shared = Arc::new(Shared::new(store, sender));
        Fixture {
            engine: Engine::new(shared.clone()),
            shared,
            events,
            me,
            peer,
            conversation,
        }
    }

    impl Fixture {
        fn events(&mut self) -> Vec<Event> {
            let mut events = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                events.push(event);
            }
            events
        }

        fn frame(&mut self, body: Body) -> (Option<Body>, Vec<Body>) {
            let mut out = Vec::new();
            let back = self.engine.on_frame(body, Instant::now(), 0, &mut out);
            (back, out)
        }

        fn queue(&self, text: &str) -> ClientMsgId {
            let id = ClientMsgId::new();
            self.shared
                .store()
                .queue_send(self.conversation, id, text, 1)
                .unwrap();
            id
        }
    }

    fn sends(out: &[Body]) -> Vec<Vec<u8>> {
        out.iter()
            .filter_map(|body| match body {
                Body::Send(send) => Some(send.client_msg_id.clone()),
                Body::Edit(edit) => Some(edit.client_msg_id.clone()),
                _ => None,
            })
            .collect()
    }

    fn send_ack(id: ClientMsgId, seq: i64) -> Body {
        Body::SendAck(v1::SendAck {
            client_msg_id: bytes(id.0),
            message_id: bytes(MessageId::new().0),
            seq,
            server_ts: 100 + seq,
        })
    }

    #[test]
    fn a_connection_lists_and_sends_one_row_per_conversation() {
        let mut f = fixture();
        let (a, _b) = (f.queue("a"), f.queue("b"));
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        let requests: Vec<_> = out
            .iter()
            .filter_map(|body| match body {
                Body::Request(request) => Some(request.request_id >= INTERNAL_REQUESTS),
                _ => None,
            })
            .collect();
        assert_eq!(
            requests,
            [true, true],
            "conversations and contacts are listed"
        );
        assert_eq!(sends(&out), [bytes(a.0)], "b waits for a");

        let (_, out) = f.frame(send_ack(a, 1));
        assert_eq!(sends(&out), [bytes(_b.0)]);
        let events = f.events();
        assert!(matches!(&events[..], [Event::MessageSent(message)]
            if message.seq == Some(1) && message.status == MessageStatus::Sent));
    }

    #[test]
    fn a_full_outbox_is_sent_twenty_rows_at_a_time() {
        let mut f = fixture();
        let mut first = Vec::new();
        for _ in 0..25 {
            let conversation = ConversationId::new();
            f.shared
                .store()
                .open_direct(conversation, UserId::new())
                .unwrap();
            let id = ClientMsgId::new();
            f.shared
                .store()
                .queue_send(conversation, id, "hi", 1)
                .unwrap();
            first.push(id);
        }
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        assert_eq!(sends(&out).len(), MAX_SENDS_AT_ONCE);
        let (_, out) = f.frame(send_ack(first[0], 1));
        assert_eq!(sends(&out).len(), 5, "an ack lets the rest go");
    }

    #[test]
    fn a_backlog_is_acknowledged_once_with_the_highest_seq() {
        let mut f = fixture();
        for seq in 1..=300 {
            f.frame(Body::Deliver(entry(
                f.conversation,
                f.peer,
                seq,
                EntryKind::Message,
            )));
        }
        let added = f
            .events()
            .into_iter()
            .filter(|event| matches!(event, Event::MessageAdded(_)))
            .count();
        assert_eq!(added, 300);
        // A replay of an entry held already is no new message, but is acknowledged again.
        f.frame(Body::Deliver(entry(
            f.conversation,
            f.peer,
            5,
            EntryKind::Message,
        )));
        assert!(f.events().is_empty());
        assert!(f.engine.acks_due().is_some());
        let mut out = Vec::new();
        f.engine.flush_acks(&mut out);
        assert_eq!(
            out,
            [Body::Ack(v1::Ack {
                conversation_id: bytes(f.conversation.0),
                seq: 300,
            })]
        );
    }

    #[test]
    fn a_retryable_error_retries_with_the_same_id_and_a_refusal_fails_the_row() {
        let mut f = fixture();
        let (a, b) = (f.queue("a"), f.queue("b"));
        let mut out = Vec::new();
        f.engine.start(1_000, &mut out);
        let busy = v1::Error {
            code: ErrorCode::Unavailable.into(),
            retryable: true,
            client_msg_id: Some(bytes(a.0)),
            ..Default::default()
        };
        let mut out = Vec::new();
        f.engine
            .on_frame(Body::Error(busy), Instant::now(), 1_000, &mut out);
        assert!(sends(&out).is_empty(), "a waits, and b waits behind it");
        let due = f.engine.retry_due(1_000).expect("a retry is scheduled");
        assert!((1_500..=2_000).contains(&due), "{due}");
        let mut out = Vec::new();
        f.engine.wake(due, &mut out);
        assert_eq!(sends(&out), [bytes(a.0)], "the same client_msg_id again");

        let refused = v1::Error {
            code: ErrorCode::NotContact.into(),
            message: "not a contact".to_owned(),
            client_msg_id: Some(bytes(a.0)),
            ..Default::default()
        };
        let mut out = Vec::new();
        f.engine
            .on_frame(Body::Error(refused), Instant::now(), due, &mut out);
        assert_eq!(sends(&out), [bytes(b.0)], "the next row proceeds");
        let events = f.events();
        assert!(
            matches!(&events[..], [Event::SendFailed { client_msg_id, kind: OutboxKind::Send, error, .. }]
            if *client_msg_id == a && error.code() == "not_contact")
        );
        assert!(
            f.shared
                .store()
                .message(f.conversation, a)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn an_edit_of_a_message_in_flight_waits_for_it_and_applies_on_its_ack() {
        let mut f = fixture();
        let original = f.queue("helo");
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        let message = f
            .shared
            .store()
            .message(f.conversation, original)
            .unwrap()
            .unwrap();
        let edit = ClientMsgId::new();
        f.shared
            .store()
            .queue_change(
                f.conversation,
                edit,
                OutboxKind::Edit,
                &message,
                Some("hello"),
                Some(original),
                2,
            )
            .unwrap();
        let mut out = Vec::new();
        f.engine.wake(0, &mut out);
        assert!(
            sends(&out).is_empty(),
            "the edit waits for the original's SendAck"
        );
        let (_, out) = f.frame(send_ack(original, 1));
        assert_eq!(sends(&out), [bytes(edit.0)]);
        f.events();
        f.frame(send_ack(edit, 2));
        let events = f.events();
        assert!(matches!(&events[..], [Event::MessageEdited(message)]
            if message.body.as_deref() == Some("hello") && message.edited_at == Some(102)));
    }

    #[test]
    fn receipts_report_only_forward_moves() {
        let mut f = fixture();
        let (conversation, peer) = (f.conversation, f.peer);
        let receipt = |delivered, read| {
            Body::Receipt(v1::Receipt {
                conversation_id: bytes(conversation.0),
                positions: Some(v1::receipt::Positions::Direct(v1::DirectReceipt {
                    user_id: bytes(peer.0),
                    delivered_seq: delivered,
                    read_seq: read,
                })),
            })
        };
        f.frame(receipt(5, 5));
        f.frame(receipt(4, 3));
        let events = f.events();
        assert_eq!(
            events,
            [Event::ReceiptChanged {
                conversation_id: f.conversation,
                peer_delivered_seq: 5,
                peer_read_seq: 5,
            }]
        );
    }

    #[test]
    fn a_contact_event_is_reported_and_the_conversations_are_listed_again() {
        let mut f = fixture();
        let (_, out) = f.frame(Body::ContactEvent(v1::ContactEvent {
            user_id: bytes(f.peer.0),
            state: v1::ContactState::Blocked.into(),
        }));
        assert!(matches!(&out[..], [Body::Request(request)]
            if matches!(request.body, Some(request::Body::ListConversations(_)))));
        assert_eq!(
            f.events(),
            [Event::ContactChanged(Contact {
                user_id: f.peer,
                state: ContactState::Blocked,
            })]
        );
    }

    #[test]
    fn listings_fill_in_a_conversation_learned_from_a_delivery() {
        let mut f = fixture();
        let unknown = ConversationId::new();
        let (_, out) = f.frame(Body::Deliver(entry(unknown, f.peer, 1, EntryKind::Message)));
        let Some(Body::Request(listing)) = out.first() else {
            panic!("a new conversation is listed");
        };
        let summary = v1::ConversationSummary {
            conversation_id: bytes(unknown.0),
            kind: v1::ConversationKind::Direct.into(),
            last_seq: 1,
            unread: 1,
            members: vec![bytes(f.me.0), bytes(f.peer.0)],
            updated_at: 101,
            ..Default::default()
        };
        f.events();
        let (back, _) = f.frame(Body::Response(v1::Response {
            request_id: listing.request_id,
            body: Some(response::Body::Conversations(v1::ConversationList {
                conversations: vec![summary],
            })),
        }));
        assert!(back.is_none());
        let events = f.events();
        assert!(
            matches!(&events[..], [Event::ConversationChanged(conversation)]
            if conversation.kind == ConversationKind::Direct && conversation.peer == Some(f.peer))
        );
        // An answer to the app's own request is handed back to the connection.
        let (back, _) = f.frame(Body::Response(v1::Response {
            request_id: 1,
            body: Some(response::Body::Done(v1::Done {})),
        }));
        assert!(back.is_some());
    }

    #[test]
    fn a_read_is_sent_once_and_confirmed_by_the_read_position() {
        let mut f = fixture();
        f.frame(Body::Deliver(entry(
            f.conversation,
            f.peer,
            1,
            EntryKind::Message,
        )));
        f.shared.store().mark_read(f.conversation, 1).unwrap();
        let mut out = Vec::new();
        f.engine.wake(0, &mut out);
        f.engine.wake(0, &mut out);
        assert_eq!(
            out,
            [Body::ReadUpTo(ReadUpTo {
                conversation_id: bytes(f.conversation.0),
                seq: 1,
            })]
        );
        f.frame(Body::ReadPosition(v1::ReadPosition {
            conversation_id: bytes(f.conversation.0),
            read_seq: 1,
        }));
        assert!(f.shared.store().unsynced_reads().unwrap().is_empty());
    }

    #[test]
    fn a_delivery_that_cannot_be_stored_ends_the_connection_unacknowledged() {
        let mut f = fixture();
        let deliver = |seq| Body::Deliver(entry(f.conversation, f.peer, seq, EntryKind::Message));
        let (first, second, third) = (deliver(1), deliver(2), deliver(3));
        f.frame(first);
        f.shared.store().set_read_only(true);
        f.frame(second.clone());
        // An Ack of 3 on this connection would acknowledge 2, which was never stored.
        assert!(f.engine.broken(), "the connection must end");
        f.engine.stop();

        // The next connection: the gateway syncs from the last Ack again.
        f.shared.store().set_read_only(false);
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        assert!(!f.engine.broken());
        f.frame(second);
        f.frame(third);
        let mut out = Vec::new();
        f.engine.flush_acks(&mut out);
        assert_eq!(
            out,
            [Body::Ack(v1::Ack {
                conversation_id: bytes(f.conversation.0),
                seq: 3,
            })]
        );
        let held = f.shared.store().messages(f.conversation, None, 10).unwrap();
        assert_eq!(held.len(), 3);
    }

    #[test]
    fn an_app_that_stops_reading_is_told_that_events_were_lost() {
        let (sender, mut events) = mpsc::channel(3);
        let shared = Shared::new(Store::open(":memory:", None).unwrap(), sender);
        for _ in 0..5 {
            shared.emit(Event::SignedOut);
        }
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            seen.push(event);
        }
        assert_eq!(
            seen,
            [Event::SignedOut, Event::SignedOut, Event::EventsLost]
        );
        shared.emit(Event::DeviceRemoved);
        assert_eq!(events.try_recv(), Ok(Event::DeviceRemoved));
    }

    #[test]
    fn a_dropped_connection_sends_the_row_in_flight_again() {
        let mut f = fixture();
        let a = f.queue("a");
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        assert_eq!(sends(&out), [bytes(a.0)]);
        f.engine.stop();
        let mut out = Vec::new();
        f.engine.start(0, &mut out);
        assert_eq!(sends(&out), [bytes(a.0)]);
        let _ = f.me;
    }
}
```

Change `crates/push-sdk/src/lib.rs`:

```diff
diff --git a/crates/push-sdk/src/lib.rs b/crates/push-sdk/src/lib.rs
index 9aba1d1..9c597e0 100644
--- a/crates/push-sdk/src/lib.rs
+++ b/crates/push-sdk/src/lib.rs
@@ -16,6 +16,7 @@
 
 mod auth;
 mod credentials;
+mod engine;
 mod error;
 mod key;
 mod model;
```

Change `crates/push-sdk/src/store.rs`:

```diff
diff --git a/crates/push-sdk/src/store.rs b/crates/push-sdk/src/store.rs
index 87470b3..e90fb41 100644
--- a/crates/push-sdk/src/store.rs
+++ b/crates/push-sdk/src/store.rs
@@ -168,6 +168,14 @@ impl Store {
         Ok(changed?)
     }
 
+    /// Makes every write fail, as a full disk would.
+    #[cfg(test)]
+    pub fn set_read_only(&self, read_only: bool) {
+        self.conn
+            .pragma_update(None, "query_only", read_only)
+            .unwrap();
+    }
+
     /// The user the database belongs to.
     pub fn owner(&self) -> Result<Option<UserId>, SdkError> {
         let value: Option<Vec<u8>> = self
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-sdk/src/engine.rs`, in `on_deliver`, delete the line
`self.broken = true;` and run:

```powershell
cargo test -p push-sdk --lib --locked engine
```

Expected: `test engine::tests::a_delivery_that_cannot_be_stored_ends_the_connection_unacknowledged ... FAILED`
(the connection would go on, and the next `Ack` would cover the lost entry) and
`11 passed; 1 failed`. Undo the edit.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "push-sdk: the engine: deliveries, acks, the outbox over a connection, receipts"
```

Expected: 924 tests pass, no warnings, no diff.

---

### Task 9: The connection: identify, heartbeats, reconnects, close codes, ReAuth

**Files:**
- Create: `crates/push-sdk/src/connection.rs`
- Modify: `crates/push-sdk/src/lib.rs`

Decisions 4, 5 and 12. `Connection::run` is the task that owns the socket: it waits for
`Connect`, gets a fresh access token, binds the store to the user, opens the socket (with
the `Host` override), sends `Identify`, checks that `Ready` names the token's user and
device, and serves the connection — frames to the engine, the app's requests with their
own ids, pings, acks, outbox retries, the background refresh and `ReAuth` — until it ends
(or the engine is broken); then it applies the close code's rule and waits out the
backoff, still answering commands. `set_state` queues the state's event while the state is
held. A refresh in flight when the connection ends is detached, not aborted.

- [ ] **Step 1: Make the changes**

Create `crates/push-sdk/src/connection.rs`:

```rust
//! The connection to the gateway (spec §7.3, §7.4): one task that connects, identifies
//! with the device's access token, serves the connection until it ends, and decides what
//! comes next from the close code:
//!
//! - 4001 (the token is invalid or older than a password reset): forget the credentials,
//!   report `SignedOut` and stop;
//! - 4003 (the device was revoked): report `DeviceRemoved` and stop, keeping the
//!   credentials;
//! - 4007 (another connection of the device took over: the same app data opened
//!   elsewhere): report `OpenedElsewhere` and stop, keeping the credentials;
//! - 4002 (the token expired): refresh it and reconnect at once;
//! - 4006 (the device is not relayed yet): reconnect within a few seconds, never sign out;
//! - anything else (4004, 4005, 1002, 1008, 1009, 1011, a network error), and a delivery
//!   the store could not take: reconnect with exponential backoff and jitter, 1 s up to
//!   60 s.
//!
//! Local data is never erased here: not on a sign-out by the server, a removal or a
//! takeover (owner decision).
//!
//! While connected it pings every heartbeat (the first ping at a random point of the
//! first one), refreshes the token and sends `ReAuth` when the gateway announces the
//! expiry, keeps its own frame budget below the gateway's, and lets the [`Engine`] handle
//! deliveries and the outbox.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, Identify, Ping, ReAuth, request, response};
use push_proto::{decode_frame, encode_frame, id_bytes};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::SdkError;
use crate::credentials::Credentials;
use crate::engine::{Engine, Shared};
use crate::model::{ConnectionState, Event};
use crate::policy::{AfterClose, Backoff, FrameBudget, after_close, first_ping};
use crate::secrets::Session;

/// How long opening the socket may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the gateway may take to answer `Identify`.
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a `Ping` may wait for its `Pong` before the connection counts as dead.
const PONG_TIMEOUT: Duration = Duration::from_secs(15);
/// How long sending one frame may take.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// A connection that stayed up this long resets the backoff.
const STABLE: Duration = Duration::from_secs(10);
/// How soon a refused `ReAuth` (the gateway could not check it) or a failed refresh is
/// tried again; the gateway closes with 4002 at `exp + 30 s` regardless.
const REAUTH_RETRY: Duration = Duration::from_secs(1);
const REFRESH_RETRY: Duration = Duration::from_secs(5);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Reply = oneshot::Sender<Result<response::Body, SdkError>>;

/// What the app asks of the connection.
pub(crate) enum Command {
    /// Connect, and keep reconnecting.
    Connect,
    /// Close the connection and stop reconnecting.
    Disconnect,
    /// The outbox or a read position changed.
    Wake,
    /// A request (spec §7.1); answered with its `Response`, or an error.
    Request { body: request::Body, reply: Reply },
}

/// Where the gateway is.
#[derive(Clone)]
pub(crate) struct Endpoint {
    /// `wss://host/v1/socket`.
    pub socket_url: String,
    /// `Host` header when the URL names a load balancer rather than the virtual host.
    pub host: Option<String>,
}

/// How a connection ended.
enum Ended {
    /// The app disconnected.
    Disconnected,
    /// The client is gone.
    Closed,
    /// No credentials, or auth-svc refused them.
    SignedOut,
    Lost {
        after: AfterClose,
        /// How long it was `Ready`, if it got there.
        ready_for: Option<Duration>,
        /// The access token it used.
        token: String,
    },
}

/// Moves the connection state to `state` and, if it changed, reports it. The event is
/// queued while the state is held, so the client's `connect` and the connection task
/// report changes in the order they made them.
pub(crate) fn set_state(
    sender: &watch::Sender<ConnectionState>,
    shared: &Shared,
    state: ConnectionState,
) {
    sender.send_if_modified(|current| {
        if *current == state {
            return false;
        }
        *current = state;
        shared.emit(Event::ConnectionStateChanged(state));
        true
    });
}

/// The connection task.
pub(crate) struct Connection {
    shared: Arc<Shared>,
    credentials: Arc<Credentials>,
    endpoint: Endpoint,
    state: Arc<watch::Sender<ConnectionState>>,
    commands: mpsc::UnboundedReceiver<Command>,
    engine: Engine,
}

impl Connection {
    pub fn new(
        shared: Arc<Shared>,
        credentials: Arc<Credentials>,
        endpoint: Endpoint,
        state: Arc<watch::Sender<ConnectionState>>,
        commands: mpsc::UnboundedReceiver<Command>,
    ) -> Self {
        Self {
            engine: Engine::new(shared.clone()),
            shared,
            credentials,
            endpoint,
            state,
            commands,
        }
    }

    fn set_state(&self, state: ConnectionState) {
        set_state(&self.state, &self.shared, state);
    }

    fn signed_out(&self) {
        self.set_state(ConnectionState::SignedOut);
        self.shared.emit(Event::SignedOut);
    }

    /// Runs until the client is dropped.
    pub async fn run(mut self) {
        let mut wanted = false;
        let mut attempts = 0u32;
        let mut not_ready = 0u32;
        let mut refreshed = false;
        loop {
            if !wanted {
                match self.commands.recv().await {
                    None => return,
                    Some(Command::Connect) => {
                        wanted = true;
                        attempts = 0;
                        not_ready = 0;
                    }
                    Some(Command::Request { reply, .. }) => {
                        let _ = reply.send(Err(SdkError::Offline));
                        continue;
                    }
                    Some(Command::Disconnect | Command::Wake) => continue,
                }
            }
            self.set_state(ConnectionState::Connecting);
            let (after, ready_for, token) = match self.connect_and_serve().await {
                Ended::Closed => return,
                Ended::Disconnected => {
                    wanted = false;
                    self.set_state(ConnectionState::Offline);
                    continue;
                }
                Ended::SignedOut => {
                    wanted = false;
                    self.signed_out();
                    continue;
                }
                Ended::Lost {
                    after,
                    ready_for,
                    token,
                } => (after, ready_for, token),
            };
            if ready_for.is_some_and(|ready_for| ready_for >= STABLE) {
                attempts = 0;
                not_ready = 0;
            }
            let delay = match after {
                AfterClose::SignOut => {
                    if let Err(error) = self.credentials.sign_out().await {
                        tracing::warn!(code = error.code(), "could not forget the credentials");
                    }
                    wanted = false;
                    self.signed_out();
                    continue;
                }
                AfterClose::DeviceRemoved => {
                    wanted = false;
                    self.set_state(ConnectionState::DeviceRemoved);
                    self.shared.emit(Event::DeviceRemoved);
                    continue;
                }
                AfterClose::OpenedElsewhere => {
                    wanted = false;
                    self.set_state(ConnectionState::OpenedElsewhere);
                    self.shared.emit(Event::OpenedElsewhere);
                    continue;
                }
                AfterClose::Refresh => match self.credentials.refresh(&token).await {
                    // Once at once; a second 4002 in a row backs off (a skewed clock).
                    Ok(_) if !refreshed => {
                        refreshed = true;
                        Duration::ZERO
                    }
                    Err(SdkError::NotSignedIn) => {
                        wanted = false;
                        self.signed_out();
                        continue;
                    }
                    _ => Backoff::RECONNECT.delay(next(&mut attempts)),
                },
                AfterClose::NotReadyYet => Backoff::NOT_READY.delay(next(&mut not_ready)),
                AfterClose::Reconnect => Backoff::RECONNECT.delay(next(&mut attempts)),
            };
            if after != AfterClose::Refresh {
                refreshed = false;
            }
            if delay.is_zero() {
                continue;
            }
            self.set_state(ConnectionState::Reconnecting {
                attempt: attempts + not_ready,
                retry_in_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            });
            let deadline = Instant::now() + delay;
            loop {
                tokio::select! {
                    () = sleep_until(deadline) => break,
                    command = self.commands.recv() => match command {
                        None => return,
                        Some(Command::Connect) => break,
                        Some(Command::Disconnect) => {
                            wanted = false;
                            self.set_state(ConnectionState::Offline);
                            break;
                        }
                        Some(Command::Request { reply, .. }) => {
                            let _ = reply.send(Err(SdkError::Offline));
                        }
                        Some(Command::Wake) => {}
                    },
                }
            }
        }
    }

    async fn connect_and_serve(&mut self) -> Ended {
        let session = match self.credentials.fresh().await {
            Ok(session) => session,
            Err(SdkError::NotSignedIn) => return Ended::SignedOut,
            Err(error) => {
                tracing::info!(code = error.code(), "could not get an access token");
                return lost(AfterClose::Reconnect, None, String::new());
            }
        };
        let bound = self.shared.store().bind_owner(session.user_id);
        match bound {
            Ok(true) => tracing::info!("another user signed in: the local store was emptied"),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(code = error.code(), "could not open the local store");
                return lost(AfterClose::Reconnect, None, session.access_token);
            }
        }
        let token = session.access_token.clone();
        let mut socket = match timeout(CONNECT_TIMEOUT, open(&self.endpoint)).await {
            Ok(Ok(socket)) => socket,
            Ok(Err(error)) => {
                tracing::info!(%error, "could not connect");
                return lost(AfterClose::Reconnect, None, token);
            }
            Err(_) => {
                tracing::info!("connecting timed out");
                return lost(AfterClose::Reconnect, None, token);
            }
        };
        let identify = Body::Identify(Identify {
            token: token.clone(),
        });
        if send_now(&mut socket, identify).await.is_err() {
            return lost(AfterClose::Reconnect, None, token);
        }
        let ready = match timeout(IDENTIFY_TIMEOUT, first_answer(&mut socket)).await {
            Ok(Ok(ready)) => ready,
            Ok(Err(code)) => return lost(after_close(code), None, token),
            Err(_) => {
                tracing::info!("no answer to Identify");
                return lost(AfterClose::Reconnect, None, token);
            }
        };
        if ready.user_id != id_bytes(session.user_id.0)
            || ready.device_id != id_bytes(session.device_id.0)
        {
            tracing::warn!("Ready named another user or device than the token");
            return lost(AfterClose::Reconnect, None, token);
        }
        self.serve(socket, &session, &ready).await
    }

    async fn serve(&mut self, socket: Socket, session: &Session, ready: &v1::Ready) -> Ended {
        let ready_at = Instant::now();
        let heartbeat = Duration::from_millis(u64::from(ready.heartbeat_ms).clamp(1_000, 600_000));
        let mut live = Live {
            socket,
            budget: FrameBudget::new(ready_at),
            pending: HashMap::new(),
            next_request: 1,
            ping_at: ready_at + first_ping(heartbeat),
            pong_by: None,
            token: session.access_token.clone(),
            refresh: None,
            refresh_at: None,
            reauth_at: None,
        };
        self.set_state(ConnectionState::Connected);
        let mut out = Vec::new();
        self.engine.start(unix_ms(), &mut out);
        let after = loop {
            if live.send_all(&mut out).await.is_err() {
                break AfterClose::Reconnect;
            }
            let acks_at = self.engine.acks_due();
            let retry_at = self.engine.retry_due(unix_ms()).map(|due| {
                Instant::now() + Duration::from_millis(u64::try_from(due - unix_ms()).unwrap_or(0))
            });
            tokio::select! {
                message = live.socket.next() => match message {
                    Some(Ok(Message::Binary(bytes))) => match decode_frame(&bytes) {
                        Ok(body) => {
                            let back = self.engine.on_frame(body, Instant::now(), unix_ms(), &mut out);
                            if self.engine.broken() {
                                break AfterClose::Reconnect;
                            }
                            if let Some(body) = back {
                                if let Some(ended) = self.on_own_frame(&mut live, body, &mut out) {
                                    break ended;
                                }
                            }
                        }
                        Err(error) => tracing::warn!(%error, "an unreadable frame"),
                    },
                    Some(Ok(Message::Close(frame))) => {
                        break after_close(frame.map(|frame| u16::from(frame.code)));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        tracing::info!(%error, "the connection failed");
                        break AfterClose::Reconnect;
                    }
                    None => break AfterClose::Reconnect,
                },
                command = self.commands.recv() => match command {
                    None => {
                        live.close().await;
                        self.end(&mut live);
                        return Ended::Closed;
                    }
                    Some(Command::Disconnect) => {
                        live.close().await;
                        self.end(&mut live);
                        return Ended::Disconnected;
                    }
                    Some(Command::Connect) => {}
                    Some(Command::Wake) => self.engine.wake(unix_ms(), &mut out),
                    Some(Command::Request { body, reply }) => {
                        let request_id = live.next_request;
                        live.next_request += 1;
                        live.pending.insert(request_id, reply);
                        out.push(Body::Request(v1::Request { request_id, body: Some(body) }));
                    }
                },
                () = sleep_until(live.ping_at) => {
                    out.push(Body::Ping(Ping {}));
                    live.ping_at = Instant::now() + heartbeat;
                    live.pong_by.get_or_insert(Instant::now() + PONG_TIMEOUT);
                }
                () = sleep_until(live.pong_by.unwrap_or_else(far_future)), if live.pong_by.is_some() => {
                    tracing::info!("no Pong: the connection is dead");
                    break AfterClose::Reconnect;
                }
                () = sleep_until(acks_at.unwrap_or_else(far_future)), if acks_at.is_some() => {
                    self.engine.flush_acks(&mut out);
                }
                () = sleep_until(retry_at.unwrap_or_else(far_future)), if retry_at.is_some() => {
                    self.engine.wake(unix_ms(), &mut out);
                }
                () = sleep_until(live.refresh_at.unwrap_or_else(far_future)), if live.refresh_at.is_some() => {
                    live.refresh_at = None;
                    let (credentials, token) = (self.credentials.clone(), live.token.clone());
                    live.refresh = Some(tokio::spawn(async move { credentials.refresh(&token).await }));
                }
                () = sleep_until(live.reauth_at.unwrap_or_else(far_future)), if live.reauth_at.is_some() => {
                    live.reauth_at = None;
                    out.push(Body::ReAuth(ReAuth { token: live.token.clone() }));
                }
                refreshed = async { live.refresh.as_mut().expect("guarded").await }, if live.refresh.is_some() => {
                    live.refresh = None;
                    match refreshed {
                        Ok(Ok(fresh)) => {
                            live.token = fresh.access_token;
                            out.push(Body::ReAuth(ReAuth { token: live.token.clone() }));
                        }
                        Ok(Err(SdkError::NotSignedIn)) => {
                            live.close().await;
                            self.end(&mut live);
                            return Ended::SignedOut;
                        }
                        Ok(Err(error)) => {
                            tracing::info!(code = error.code(), "the refresh failed; trying again");
                            live.refresh_at = Some(Instant::now() + REFRESH_RETRY);
                        }
                        Err(_) => live.refresh_at = Some(Instant::now() + REFRESH_RETRY),
                    }
                }
            }
        };
        self.end(&mut live);
        Ended::Lost {
            after,
            ready_for: Some(ready_at.elapsed()),
            token: live.token,
        }
    }

    /// A frame the engine handed back: answers to the app's requests, the token's expiry,
    /// `Pong`. `Some` when the connection must end.
    fn on_own_frame(
        &mut self,
        live: &mut Live,
        body: Body,
        _out: &mut [Body],
    ) -> Option<AfterClose> {
        match body {
            Body::Response(answer) => {
                if let Some(reply) = live.pending.remove(&answer.request_id) {
                    let _ = reply.send(answer.body.ok_or_else(|| {
                        SdkError::Network("the server answered a request with nothing".to_owned())
                    }));
                }
            }
            Body::Error(error) => match error.request_id {
                Some(request_id) => {
                    if let Some(reply) = live.pending.remove(&request_id) {
                        let _ = reply.send(Err(SdkError::from_frame(&error)));
                    }
                }
                // The only frame answered with an anonymous retryable error is ReAuth,
                // when the gateway cannot check it now.
                None if error.retryable => live.reauth_at = Some(Instant::now() + REAUTH_RETRY),
                None => tracing::info!(code = ?error.code(), "the gateway reported an error"),
            },
            Body::AuthExpiring(_) => {
                if live.refresh.is_none() {
                    live.refresh_at = Some(Instant::now());
                }
            }
            Body::Pong(_) => live.pong_by = None,
            _ => {}
        }
        None
    }

    /// The connection ended: rows in flight go again on the next one, and requests
    /// waiting for an answer learn it will not come. A refresh in flight is left to
    /// finish: auth-svc may have rotated the token already, and the refresh saves the new
    /// one; aborting it could lose the only refresh token that still works.
    fn end(&mut self, live: &mut Live) {
        self.engine.stop();
        for (_, reply) in live.pending.drain() {
            let _ = reply.send(Err(SdkError::Disconnected));
        }
        // Dropping the handle detaches the task.
        drop(live.refresh.take());
    }
}

/// An identified connection.
struct Live {
    socket: Socket,
    budget: FrameBudget,
    pending: HashMap<u64, Reply>,
    next_request: u64,
    ping_at: Instant,
    /// When the `Pong` for the last `Ping` is due.
    pong_by: Option<Instant>,
    /// The access token the connection holds now.
    token: String,
    refresh: Option<JoinHandle<Result<Session, SdkError>>>,
    refresh_at: Option<Instant>,
    reauth_at: Option<Instant>,
}

impl Live {
    /// Sends the frames in order, within the client's frame budget.
    async fn send_all(&mut self, out: &mut Vec<Body>) -> Result<(), ()> {
        for body in out.drain(..) {
            let wait = self.budget.wait(Instant::now());
            if !wait.is_zero() {
                sleep(wait).await;
            }
            self.budget.spend(Instant::now());
            send_now(&mut self.socket, body).await?;
        }
        Ok(())
    }

    async fn close(&mut self) {
        let frame = CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        };
        let _ = timeout(Duration::from_secs(2), self.socket.close(Some(frame))).await;
    }
}

fn lost(after: AfterClose, ready_for: Option<Duration>, token: String) -> Ended {
    Ended::Lost {
        after,
        ready_for,
        token,
    }
}

/// The attempt number to use, counting it.
fn next(counter: &mut u32) -> u32 {
    let attempt = *counter;
    *counter = counter.saturating_add(1);
    attempt
}

async fn open(endpoint: &Endpoint) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = endpoint.socket_url.as_str().into_client_request()?;
    if let Some(host) = &endpoint.host {
        let value = HeaderValue::from_str(host)
            .map_err(|error| tokio_tungstenite::tungstenite::Error::HttpFormat(error.into()))?;
        request.headers_mut().insert("Host", value);
    }
    let (socket, _) = connect_async(request).await?;
    Ok(socket)
}

async fn send_now(socket: &mut Socket, body: Body) -> Result<(), ()> {
    match timeout(
        SEND_TIMEOUT,
        socket.send(Message::Binary(encode_frame(body).into())),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

/// The answer to `Identify`: `Ready`, or the close code (`None` without one).
async fn first_answer(socket: &mut Socket) -> Result<v1::Ready, Option<u16>> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Binary(bytes))) => {
                return match decode_frame(&bytes) {
                    Ok(Body::Ready(ready)) => Ok(ready),
                    _ => Err(None),
                };
            }
            Some(Ok(Message::Close(frame))) => return Err(frame.map(|frame| u16::from(frame.code))),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return Err(None),
        }
    }
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(24 * 60 * 60)
}

pub(crate) fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use push_proto::v1::{AuthExpiring, ListContacts, Pong, Ready};

    use super::*;
    use crate::MemorySecretStore;
    use crate::auth::AuthApi;
    use crate::credentials::unix_now;
    use crate::secrets::SecretStore;
    use crate::store::Store;
    use crate::testing::{FakeAuth, FakeGateway, Server, close, recv, send};

    struct Client {
        commands: mpsc::UnboundedSender<Command>,
        state: watch::Receiver<ConnectionState>,
        events: mpsc::Receiver<Event>,
        secrets: Arc<MemorySecretStore>,
        session: Session,
        refreshes: Arc<AtomicUsize>,
        refresh_delay_ms: Arc<AtomicU64>,
    }

    impl Client {
        async fn wait_for(&mut self, state: ConnectionState) {
            timeout(
                Duration::from_secs(10),
                self.state.wait_for(|now| *now == state),
            )
            .await
            .unwrap_or_else(|_| panic!("no {state:?} within 10 s"))
            .unwrap();
        }

        fn events(&mut self) -> Vec<Event> {
            let mut events = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                events.push(event);
            }
            events
        }

        async fn request(
            &self,
            body: request::Body,
        ) -> oneshot::Receiver<Result<response::Body, SdkError>> {
            let (reply, answer) = oneshot::channel();
            self.commands
                .send(Command::Request { body, reply })
                .unwrap();
            answer
        }
    }

    /// A signed-in client of `gateway`, connecting; refreshes answer `access-N`.
    async fn connect(gateway: &FakeGateway) -> Client {
        let auth = FakeAuth::start().await;
        let session = Session {
            user_id: auth.user,
            device_id: auth.device,
            access_token: "access-0".to_owned(),
            access_expires_at: unix_now() + 900,
            refresh_token: "refresh-0".to_owned(),
        };
        let auth_url = auth.url.clone();
        let (refreshes, refresh_delay_ms) = (auth.refreshes.clone(), auth.refresh_delay_ms.clone());
        let secrets = Arc::new(MemorySecretStore::new());
        session.save(secrets.as_ref()).unwrap();
        let credentials = Arc::new(Credentials::new(
            secrets.clone() as Arc<dyn SecretStore>,
            AuthApi::new(&auth_url, None).unwrap(),
        ));
        let (event_sender, events) = mpsc::channel(crate::engine::EVENT_QUEUE);
        let shared = Arc::new(Shared::new(
            Store::open(":memory:", None).unwrap(),
            event_sender,
        ));
        let (state_sender, state) = watch::channel(ConnectionState::Offline);
        let state_sender = Arc::new(state_sender);
        let (commands, receiver) = mpsc::unbounded_channel();
        let endpoint = Endpoint {
            socket_url: gateway.url.clone(),
            host: None,
        };
        let connection = Connection::new(shared, credentials, endpoint, state_sender, receiver);
        tokio::spawn(connection.run());
        commands.send(Command::Connect).unwrap();
        Client {
            commands,
            state,
            events,
            secrets,
            session,
            refreshes,
            refresh_delay_ms,
        }
    }

    /// Reads `Identify` and answers `Ready`; the token it carried.
    async fn identify(client: &Client, socket: &mut Server, heartbeat_ms: u32) -> String {
        let Body::Identify(identify) = recv(socket).await else {
            panic!("the first frame is Identify");
        };
        send(
            socket,
            Body::Ready(Ready {
                user_id: id_bytes(client.session.user_id.0),
                device_id: id_bytes(client.session.device_id.0),
                heartbeat_ms,
            }),
        )
        .await;
        identify.token
    }

    #[tokio::test]
    async fn it_identifies_with_the_access_token_and_lists_once_ready() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        assert_eq!(identify(&client, &mut socket, 30_000).await, "access-0");
        client.wait_for(ConnectionState::Connected).await;
        let listings: Vec<_> = [recv(&mut socket).await, recv(&mut socket).await]
            .into_iter()
            .map(|body| match body {
                Body::Request(request) => request.body,
                _ => None,
            })
            .collect();
        assert!(matches!(
            &listings[..],
            [
                Some(request::Body::ListConversations(_)),
                Some(request::Body::ListContacts(_))
            ]
        ));
    }

    #[tokio::test]
    async fn not_ready_yet_is_retried_and_never_signs_out() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        for _ in 0..2 {
            let mut socket = gateway.next().await;
            recv(&mut socket).await;
            close(&mut socket, 4006).await;
        }
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        let events = client.events();
        assert!(!events.contains(&Event::SignedOut));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::ConnectionStateChanged(ConnectionState::Reconnecting { .. })
        )));
        assert!(Session::load(client.secrets.as_ref()).unwrap().is_some());
    }

    #[tokio::test]
    async fn an_invalid_identity_signs_out_and_stops() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        recv(&mut socket).await;
        close(&mut socket, 4001).await;
        client.wait_for(ConnectionState::SignedOut).await;
        assert!(client.events().contains(&Event::SignedOut));
        assert_eq!(Session::load(client.secrets.as_ref()).unwrap(), None);
        gateway.quiet(Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn a_removed_device_stops_and_keeps_its_credentials() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        close(&mut socket, 4003).await;
        client.wait_for(ConnectionState::DeviceRemoved).await;
        assert!(client.events().contains(&Event::DeviceRemoved));
        assert!(Session::load(client.secrets.as_ref()).unwrap().is_some());
        gateway.quiet(Duration::from_secs(2)).await;
        // The app may try again; the gateway decides.
        client.commands.send(Command::Connect).unwrap();
        gateway.next().await;
    }

    #[tokio::test]
    async fn opened_elsewhere_stops_and_keeps_its_credentials() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        close(&mut socket, 4007).await;
        client.wait_for(ConnectionState::OpenedElsewhere).await;
        let events = client.events();
        assert!(events.contains(&Event::OpenedElsewhere));
        assert!(!events.contains(&Event::DeviceRemoved));
        assert!(Session::load(client.secrets.as_ref()).unwrap().is_some());
        gateway.quiet(Duration::from_secs(2)).await;
    }

    /// auth-svc rotates the refresh token when it answers; a refresh cut off by the end of
    /// the connection would lose the only refresh token that still works.
    #[tokio::test]
    async fn a_refresh_in_flight_finishes_when_the_connection_ends() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        client.refresh_delay_ms.store(300, Ordering::SeqCst);
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        send(
            &mut socket,
            Body::AuthExpiring(AuthExpiring { expires_in_s: 60 }),
        )
        .await;
        while client.refreshes.load(Ordering::SeqCst) == 0 {
            sleep(Duration::from_millis(10)).await;
        }
        client.commands.send(Command::Disconnect).unwrap();
        client.wait_for(ConnectionState::Offline).await;
        sleep(Duration::from_millis(600)).await;
        let stored = Session::load(client.secrets.as_ref()).unwrap().unwrap();
        assert_eq!(stored.access_token, "access-1");
    }

    #[tokio::test]
    async fn an_expired_token_is_refreshed_before_reconnecting() {
        let mut gateway = FakeGateway::start().await;
        let client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        recv(&mut socket).await;
        close(&mut socket, 4002).await;
        let mut socket = gateway.next().await;
        assert_eq!(identify(&client, &mut socket, 30_000).await, "access-1");
        assert_eq!(client.refreshes.load(Ordering::SeqCst), 1);
        let stored = Session::load(client.secrets.as_ref()).unwrap().unwrap();
        assert_eq!(stored.access_token, "access-1");
    }

    #[tokio::test]
    async fn auth_expiring_refreshes_and_reauths_on_the_same_connection() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        send(
            &mut socket,
            Body::AuthExpiring(AuthExpiring { expires_in_s: 60 }),
        )
        .await;
        let reauth = loop {
            if let Body::ReAuth(reauth) = recv(&mut socket).await {
                break reauth;
            }
        };
        assert_eq!(reauth.token, "access-1");
    }

    #[tokio::test]
    async fn a_lost_connection_is_reconnected_and_pings_follow_the_heartbeat() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let socket = gateway.next().await;
        drop(socket);
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 1_000).await;
        client.wait_for(ConnectionState::Connected).await;
        let ping = loop {
            if let Body::Ping(ping) = recv(&mut socket).await {
                break ping;
            }
        };
        assert_eq!(ping, Ping {});
        send(&mut socket, Body::Pong(Pong {})).await;
    }

    #[tokio::test]
    async fn requests_are_answered_or_fail_when_the_connection_drops() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        let answer = client
            .request(request::Body::ListContacts(ListContacts {}))
            .await;
        let request_id = loop {
            if let Body::Request(request) = recv(&mut socket).await {
                if request.request_id < crate::engine::INTERNAL_REQUESTS {
                    break request.request_id;
                }
            }
        };
        send(
            &mut socket,
            Body::Response(v1::Response {
                request_id,
                body: Some(response::Body::Done(v1::Done {})),
            }),
        )
        .await;
        assert_eq!(answer.await.unwrap(), Ok(response::Body::Done(v1::Done {})));

        let answer = client
            .request(request::Body::ListContacts(ListContacts {}))
            .await;
        loop {
            if let Body::Request(request) = recv(&mut socket).await {
                if request.request_id < crate::engine::INTERNAL_REQUESTS {
                    break;
                }
            }
        }
        drop(socket);
        assert_eq!(answer.await.unwrap(), Err(SdkError::Disconnected));
        client.commands.send(Command::Disconnect).unwrap();
        client.wait_for(ConnectionState::Offline).await;
    }
}
```

Change `crates/push-sdk/src/lib.rs`:

```diff
diff --git a/crates/push-sdk/src/lib.rs b/crates/push-sdk/src/lib.rs
index 9c597e0..9fdb820 100644
--- a/crates/push-sdk/src/lib.rs
+++ b/crates/push-sdk/src/lib.rs
@@ -15,6 +15,7 @@
 #![allow(dead_code)]
 
 mod auth;
+mod connection;
 mod credentials;
 mod engine;
 mod error;
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-sdk/src/connection.rs`, in `end`, replace `drop(live.refresh.take());` with

```rust
        if let Some(refresh) = live.refresh.take() {
            refresh.abort();
        }
```

and run:

```powershell
cargo test -p push-sdk --lib --locked connection
```

Expected after about 2 s: `test connection::tests::a_refresh_in_flight_finishes_when_the_connection_ends ... FAILED`
(the stored token stayed `access-0`: the rotated one auth-svc had answered was lost) and
`12 passed; 1 failed`. Undo the edit.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "push-sdk: the connection: identify, heartbeats, reconnects, close codes, ReAuth"
```

Expected: 934 tests pass, no warnings, no diff. The connection tests take about 2 s
together (the 4006 test waits out two short backoffs, the stop tests 2 s of quiet).

---

### Task 10: The Client

**Files:**
- Create: `crates/push-sdk/src/client.rs`
- Modify: `crates/push-sdk/src/lib.rs`, `crates/push-sdk/src/credentials.rs`, `crates/push-sdk/src/secrets.rs`

Decisions 3, 5, 7, 12, 15 and 16. The public `Client` without sending (Task 11) and
requests (Task 12): `open` (the store with `Config::database_key`, then the runtime), the
account calls (`sign_up`, `resend_verification`, `forgot_password`, `sign_in`, `account`,
`sign_out`), the connection (`connect`, `disconnect`, `connection_state`,
`wait_connected`), the key (`change_database_key`) and the local data (`conversations`,
`conversation`, `messages`, `contacts`).
`sign_in` saves the session before it forgets the `registration_id`, and replaces an id
auth-svc says is spent; `sign_out` erases locally first and then revokes, reporting a failed
revocation. `credentials.rs` remembers the device for the next sign-in and forgets the
session and the device together, after any refresh in flight (`sign_out_for_good`);
`secrets.rs` exports `SECRET_KEYS`. The tests queue messages straight in the store
(sending is Task 11).

- [ ] **Step 1: Make the changes**

Create `crates/push-sdk/src/client.rs`:

```rust
//! The SDK's public face (spec §8.2): one [`Client`] per signed-in app profile, and the
//! [`Events`] stream it reports on.
//!
//! The client owns its own tokio runtime (spec §8.5), so it works the same from an async
//! Rust app, a Node binding or a C binding: its async methods run on the client's runtime
//! and can be awaited from any executor; its plain methods read or change the local store
//! at once. It is cheap to clone, and every clone is the same client.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use push_domain::{ConversationId, DeviceId, UserId};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};

use crate::SdkError;
use crate::auth::{AuthApi, Platform};
use crate::connection::{Command, Connection, Endpoint, set_state};
use crate::credentials::{Credentials, MIN_ACCESS_LIFETIME_S, is_refused, unix_now};
use crate::engine::{EVENT_QUEUE, Shared};
use crate::key::DatabaseKey;
use crate::model::{ConnectionState, Contact, Conversation, Event, Message};
use crate::secrets::SecretStore;
use crate::store::Store;

/// How long after sending a message its sender may still edit or delete it: the server's
/// default (spec §4.6). The server checks again with its own setting.
pub const EDIT_WINDOW: Duration = Duration::from_secs(48 * 60 * 60);

/// Where the servers are, and where the client keeps its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// auth-svc's public URL: `https://push.example`.
    pub auth_url: String,
    /// The gateway's WebSocket: `wss://push.example/v1/socket`.
    pub socket_url: String,
    /// The `Host` header for both, when the URLs name an address rather than the virtual
    /// host a load balancer routes (a local cluster's ingress). Usually `None`.
    pub host: Option<String>,
    /// The SQLite file of this app profile; `:memory:` keeps nothing.
    pub database_path: String,
    /// Encrypts the whole database with this key (see [`DatabaseKey`]); `None` keeps it
    /// plain. The client uses it to open the file and keeps it only in SQLite's memory;
    /// the app provides it each time it opens the client.
    pub database_key: Option<DatabaseKey>,
    /// How long a request waits for the connection, and then for its answer.
    pub request_timeout: Duration,
}

impl Config {
    pub fn new(auth_url: &str, socket_url: &str, database_path: &str) -> Self {
        Self {
            auth_url: auth_url.to_owned(),
            socket_url: socket_url.to_owned(),
            host: None,
            database_path: database_path.to_owned(),
            database_key: None,
            request_timeout: Duration::from_secs(15),
        }
    }
}

/// The signed-in user and device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Account {
    pub user_id: UserId,
    pub device_id: DeviceId,
}

/// The client's events, in order (spec §8.2). Bindings forward them to the app's
/// callback; a Rust app awaits [`Events::next`].
///
/// At most 10,000 events wait to be read. An app that stops reading (a UI in the
/// background, say) does not make the client hold more: newer events are dropped and one
/// [`Event::EventsLost`] takes their place, after which the app reloads what it shows from
/// the client's local data. Nothing is lost from the store.
pub struct Events {
    receiver: mpsc::Receiver<Event>,
}

impl Events {
    /// The next event; `None` once the client is gone.
    pub async fn next(&mut self) -> Option<Event> {
        self.receiver.recv().await
    }

    /// The next event if one is waiting.
    pub fn try_next(&mut self) -> Option<Event> {
        self.receiver.try_recv().ok()
    }
}

/// A push-messenger client.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    runtime: Option<Runtime>,
    shared: Arc<Shared>,
    credentials: Arc<Credentials>,
    auth: AuthApi,
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<ConnectionState>,
    state_sender: Arc<watch::Sender<ConnectionState>>,
    request_timeout: Duration,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Never blocks, so the last clone may be dropped inside an async context.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("state", &*self.inner.state.borrow())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Opens the local store and starts the client's runtime. It does not connect until
    /// [`Client::connect`]; once connected, whatever the outbox held is sent.
    /// [`SdkError::WrongKey`] if `config.database_key` does not open the database.
    pub fn open(config: Config, secrets: Arc<dyn SecretStore>) -> Result<(Self, Events), SdkError> {
        let auth = AuthApi::new(&config.auth_url, config.host.as_deref())?;
        let credentials = Arc::new(Credentials::new(secrets, auth.clone()));
        let mut store = Store::open(&config.database_path, config.database_key.as_ref())?;
        if let Some(session) = credentials.session()? {
            store.bind_owner(session.user_id)?;
        }
        // Last: a runtime dropped by an early return could not be dropped in async code.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("push-sdk")
            .enable_all()
            .build()
            .map_err(|error| SdkError::Storage(format!("cannot start the runtime: {error}")))?;
        let (event_sender, receiver) = mpsc::channel(EVENT_QUEUE);
        let shared = Arc::new(Shared::new(store, event_sender));
        let (state_sender, state) = watch::channel(ConnectionState::Offline);
        let state_sender = Arc::new(state_sender);
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let endpoint = Endpoint {
            socket_url: config.socket_url,
            host: config.host,
        };
        let connection = Connection::new(
            shared.clone(),
            credentials.clone(),
            endpoint,
            state_sender.clone(),
            command_receiver,
        );
        runtime.spawn(connection.run());
        let inner = Inner {
            runtime: Some(runtime),
            shared,
            credentials,
            auth,
            commands,
            state,
            state_sender,
            request_timeout: config.request_timeout,
        };
        Ok((
            Self {
                inner: Arc::new(inner),
            },
            Events { receiver },
        ))
    }

    /// Runs `future` on the client's runtime.
    async fn run<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T, SdkError>> + Send + 'static,
    ) -> Result<T, SdkError> {
        let runtime = self.inner.runtime.as_ref().ok_or(SdkError::Closed)?;
        runtime.spawn(future).await.unwrap_or(Err(SdkError::Closed))
    }

    fn command(&self, command: Command) -> Result<(), SdkError> {
        self.inner
            .commands
            .send(command)
            .map_err(|_| SdkError::Closed)
    }

    // ----- account -----

    /// Signs up; auth-svc emails a verification link. Answered the same whether or not
    /// the address is known.
    pub async fn sign_up(
        &self,
        email: &str,
        password: &str,
        handle: &str,
        display_name: &str,
    ) -> Result<(), SdkError> {
        let auth = self.inner.auth.clone();
        let (email, password, handle, display_name) = (
            email.to_owned(),
            password.to_owned(),
            handle.to_owned(),
            display_name.to_owned(),
        );
        self.run(async move {
            auth.sign_up(&email, &password, &handle, &display_name)
                .await
        })
        .await
    }

    /// Sends the verification email again.
    pub async fn resend_verification(&self, email: &str) -> Result<(), SdkError> {
        let (auth, email) = (self.inner.auth.clone(), email.to_owned());
        self.run(async move { auth.resend_verification(&email).await })
            .await
    }

    /// Emails a password-reset link. Resetting signs every device out.
    pub async fn forgot_password(&self, email: &str) -> Result<(), SdkError> {
        let (auth, email) = (self.inner.auth.clone(), email.to_owned());
        self.run(async move { auth.forgot_password(&email).await })
            .await
    }

    /// Signs in on this device (spec §5.3). The device this client registered before is
    /// signed in again if it is still active; otherwise a device is registered, with a
    /// `registration_id` kept until the registration is answered and its tokens saved, so
    /// a retry, even after a restart, never creates a second device. Then call
    /// [`Client::connect`].
    pub async fn sign_in(
        &self,
        email: &str,
        password: &str,
        device_name: &str,
        platform: Platform,
    ) -> Result<Account, SdkError> {
        let inner = self.inner.clone();
        let (email, password, device_name) = (
            email.to_owned(),
            password.to_owned(),
            device_name.to_owned(),
        );
        self.run(async move {
            let credentials = &inner.credentials;
            let mut tokens = None;
            if let Some(device) = credentials.last_device()? {
                match inner.auth.login_device(&email, &password, device).await {
                    Ok(found) => tokens = Some(found),
                    // Revoked meanwhile: register anew.
                    Err(SdkError::Server { code, .. }) if code == "device_not_found" => {
                        credentials.forget_device()?;
                    }
                    Err(error) => return Err(error),
                }
            }
            let tokens = match tokens {
                Some(tokens) => tokens,
                None => {
                    let registration = inner.auth.login(&email, &password).await?;
                    let register = |registration_id| {
                        inner.auth.register_device(
                            &registration,
                            registration_id,
                            &device_name,
                            platform,
                        )
                    };
                    match register(credentials.registration_id()?).await {
                        // The id belongs to a device revoked since: it stays spent, so the
                        // registration starts again with a new one.
                        Err(SdkError::Server { code, .. }) if code == "device_not_found" => {
                            credentials.registration_done()?;
                            register(credentials.registration_id()?).await?
                        }
                        answer => answer?,
                    }
                }
            };
            // Saved before a registration_id is forgotten: if saving fails, a retry finds
            // the same device again.
            let session = credentials.sign_in(tokens)?;
            credentials.registration_done()?;
            inner.shared.store().bind_owner(session.user_id)?;
            Ok(Account {
                user_id: session.user_id,
                device_id: session.device_id,
            })
        })
        .await
    }

    /// The signed-in user and device, if any.
    pub fn account(&self) -> Result<Option<Account>, SdkError> {
        Ok(self.inner.credentials.session()?.map(|session| Account {
            user_id: session.user_id,
            device_id: session.device_id,
        }))
    }

    /// Signs this device out for good: the client disconnects, forgets the credentials,
    /// erases the local store, outbox included, and then asks auth-svc to revoke the
    /// device. The local part comes first and always happens, also offline (an app that
    /// ends meanwhile has still signed out); an error says that auth-svc did not confirm
    /// the revocation, and the device stays in the account's device list until it is
    /// removed from another device ([`Client::revoke_device`]).
    pub async fn sign_out(&self) -> Result<(), SdkError> {
        let inner = self.inner.clone();
        self.command(Command::Disconnect)?;
        self.run(async move {
            let session = inner.credentials.sign_out_for_good().await?;
            inner.shared.store().erase_all()?;
            let Some(session) = session else {
                return Ok(());
            };
            // The tokens are no longer stored: an access token about to expire is
            // refreshed in memory only, for this one call.
            let access_token = if session.access_expires_at - unix_now() >= MIN_ACCESS_LIFETIME_S {
                session.access_token
            } else {
                match inner.auth.refresh(&session.refresh_token).await {
                    Ok(tokens) => tokens.access_token,
                    // Refused: the device's tokens are dead already.
                    Err(error) if is_refused(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
            };
            match inner
                .auth
                .revoke_device(&access_token, session.device_id)
                .await
            {
                Err(error) if is_refused(&error) => Ok(()),
                revoked => revoked,
            }
        })
        .await
    }

    // ----- connection -----

    /// Connects, and keeps reconnecting until [`Client::disconnect`], a sign-out (4001),
    /// the device's removal (4003) or a takeover by a copy opened elsewhere (4007). The
    /// state is `Connecting` when this returns, so a request made right after waits for
    /// the connection instead of failing as offline.
    pub fn connect(&self) -> Result<(), SdkError> {
        let state = *self.inner.state.borrow();
        if matches!(
            state,
            ConnectionState::Offline
                | ConnectionState::SignedOut
                | ConnectionState::DeviceRemoved
                | ConnectionState::OpenedElsewhere
        ) {
            set_state(
                &self.inner.state_sender,
                &self.inner.shared,
                ConnectionState::Connecting,
            );
        }
        self.command(Command::Connect)
    }

    /// Closes the connection and stops reconnecting. The outbox waits.
    pub fn disconnect(&self) -> Result<(), SdkError> {
        self.command(Command::Disconnect)
    }

    pub fn connection_state(&self) -> ConnectionState {
        *self.inner.state.borrow()
    }

    /// Encrypts the local database with `key`, re-encrypts it with a new one (the user
    /// changed their PIN), or with `None` decrypts it. The next [`Client::open`] needs the
    /// new key in [`Config::database_key`]; the client does not keep it.
    pub fn change_database_key(&self, key: Option<&DatabaseKey>) -> Result<(), SdkError> {
        self.inner.shared.store().change_key(key)
    }

    /// Waits until the client is connected, up to `wait`.
    pub async fn wait_connected(&self, wait: Duration) -> Result<(), SdkError> {
        let mut state = self.inner.state.clone();
        self.run(async move {
            match tokio::time::timeout(
                wait,
                state.wait_for(|state| *state == ConnectionState::Connected),
            )
            .await
            {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(_)) => Err(SdkError::Closed),
                Err(_) => Err(SdkError::Offline),
            }
        })
        .await
    }

    // ----- local data -----

    /// Every conversation this device knows, most recently changed first.
    pub fn conversations(&self) -> Result<Vec<Conversation>, SdkError> {
        self.inner.shared.store().conversations()
    }

    pub fn conversation(&self, id: ConversationId) -> Result<Option<Conversation>, SdkError> {
        self.inner.shared.store().conversation(id)
    }

    /// Up to `limit` stored messages before `before_seq`, oldest first; without
    /// `before_seq`, the newest ones followed by those still in the outbox. Older ones
    /// come with [`Client::fetch_history`].
    pub fn messages(
        &self,
        conversation: ConversationId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, SdkError> {
        self.inner
            .shared
            .store()
            .messages(conversation, before_seq, limit)
    }

    /// The user's relationships as this device knows them.
    pub fn contacts(&self) -> Result<Vec<Contact>, SdkError> {
        self.inner.shared.store().contacts()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use push_proto::v1::Ready;
    use push_proto::v1::frame::Body;

    use uuid::Uuid;

    use super::*;
    use crate::MemorySecretStore;
    use crate::secrets::REGISTRATION_KEY;
    use crate::store::bytes;
    use crate::testing::{FakeAuth, FakeGateway, recv, send};

    pub(super) struct Fixture {
        pub client: Client,
        pub events: Events,
        pub auth: FakeAuth,
        pub gateway: FakeGateway,
        pub secrets: Arc<MemorySecretStore>,
        pub database: String,
    }

    impl Fixture {
        /// A client signed in to fake services, not connected, on its own database.
        pub async fn new() -> Self {
            let database = format!("file:client-{}?mode=memory&cache=shared", Uuid::now_v7());
            let (auth, gateway) = (FakeAuth::start().await, FakeGateway::start().await);
            let secrets = Arc::new(MemorySecretStore::new());
            let (client, events) = open(&auth, &gateway, &database, &secrets);
            client
                .sign_in("a@example.com", "password", "phone", Platform::Ios)
                .await
                .unwrap();
            Self {
                client,
                events,
                auth,
                gateway,
                secrets,
                database,
            }
        }

        pub fn events(&mut self) -> Vec<Event> {
            let mut events = Vec::new();
            while let Some(event) = self.events.try_next() {
                events.push(event);
            }
            events
        }

        /// A direct conversation with someone, known locally.
        pub fn conversation(&self) -> (ConversationId, UserId) {
            let (id, peer) = (ConversationId::new(), UserId::new());
            self.client
                .inner
                .shared
                .store()
                .open_direct(id, peer)
                .unwrap();
            (id, peer)
        }

        /// Accepts the connection and answers `Identify` with `Ready`.
        pub async fn ready(&mut self) -> crate::testing::Server {
            let mut socket = self.gateway.next().await;
            let Body::Identify(_) = recv(&mut socket).await else {
                panic!("Identify first");
            };
            let ready = Ready {
                user_id: bytes(self.auth.user.0),
                device_id: bytes(self.auth.device.0),
                heartbeat_ms: 30_000,
            };
            send(&mut socket, Body::Ready(ready)).await;
            socket
        }
    }

    /// Queues a message straight in the store (sending comes with `client/sending.rs`).
    fn queue(client: &Client, conversation: ConversationId, text: &str) {
        client
            .inner
            .shared
            .store()
            .queue_send(conversation, push_domain::ClientMsgId::new(), text, 1)
            .unwrap();
    }

    pub(super) fn open(
        auth: &FakeAuth,
        gateway: &FakeGateway,
        database: &str,
        secrets: &Arc<MemorySecretStore>,
    ) -> (Client, Events) {
        let config = Config {
            request_timeout: Duration::from_secs(3),
            ..Config::new(&auth.url, &gateway.url, database)
        };
        Client::open(config, secrets.clone()).unwrap()
    }

    #[tokio::test]
    async fn signing_in_registers_once_and_finds_the_same_device_again() {
        let f = Fixture::new().await;
        assert_eq!(f.auth.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(f.secrets.load(REGISTRATION_KEY).unwrap(), None);
        let account = f.client.account().unwrap().unwrap();
        assert_eq!(
            (account.user_id, account.device_id),
            (f.auth.user, f.auth.device)
        );

        // Signed out by the server (a password reset): the same device signs in again.
        f.client.inner.credentials.sign_out().await.unwrap();
        f.client
            .sign_in("a@example.com", "new password", "phone", Platform::Ios)
            .await
            .unwrap();
        assert_eq!(f.auth.device_logins.load(Ordering::SeqCst), 1);
        assert_eq!(f.auth.registrations.load(Ordering::SeqCst), 1);

        // Signing out for good revokes the device and empties the store.
        let (conversation, _) = f.conversation();
        queue(&f.client, conversation, "unsent");
        f.client.sign_out().await.unwrap();
        assert_eq!(f.auth.revocations.load(Ordering::SeqCst), 1);
        assert_eq!(f.client.account().unwrap(), None);
        assert!(f.client.conversations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn signing_out_offline_still_erases_and_reports_the_failed_revocation() {
        let f = Fixture::new().await;
        let (conversation, _) = f.conversation();
        queue(&f.client, conversation, "unsent");
        f.auth.fail_revocations.store(true, Ordering::SeqCst);
        let error = f.client.sign_out().await.unwrap_err();
        assert_eq!(error.code(), "http_500");
        assert_eq!(f.client.account().unwrap(), None);
        assert!(f.client.conversations().unwrap().is_empty());
        assert!(f.client.inner.shared.store().outbox().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_spent_registration_id_is_replaced() {
        let (auth, gateway) = (FakeAuth::start().await, FakeGateway::start().await);
        let secrets = Arc::new(MemorySecretStore::new());
        let database = format!("file:client-{}?mode=memory&cache=shared", Uuid::now_v7());
        let (client, _events) = open(&auth, &gateway, &database, &secrets);
        auth.spend_next_registration.store(true, Ordering::SeqCst);
        client
            .sign_in("a@example.com", "password", "phone", Platform::Ios)
            .await
            .unwrap();
        let ids = auth.registration_ids.lock().unwrap().clone();
        assert_eq!(ids.len(), 2);
        assert_ne!(
            ids[0], ids[1],
            "a new registration_id after device_not_found"
        );
        assert_eq!(secrets.load(REGISTRATION_KEY).unwrap(), None);
    }

    /// A secret store whose next save of the session fails, as a locked keychain might.
    #[derive(Default)]
    struct LockedOnce {
        values: MemorySecretStore,
        locked: std::sync::atomic::AtomicBool,
    }

    impl SecretStore for LockedOnce {
        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, crate::SecretError> {
            self.values.load(key)
        }

        fn save(&self, key: &str, value: &[u8]) -> Result<(), crate::SecretError> {
            if key == crate::secrets::SESSION_KEY && self.locked.swap(false, Ordering::SeqCst) {
                return Err(crate::SecretError("the keychain is locked".to_owned()));
            }
            self.values.save(key, value)
        }

        fn delete(&self, key: &str) -> Result<(), crate::SecretError> {
            self.values.delete(key)
        }
    }

    #[tokio::test]
    async fn the_registration_id_is_kept_until_the_session_is_saved() {
        let (auth, gateway) = (FakeAuth::start().await, FakeGateway::start().await);
        let secrets = Arc::new(LockedOnce::default());
        secrets.locked.store(true, Ordering::SeqCst);
        let database = format!("file:client-{}?mode=memory&cache=shared", Uuid::now_v7());
        let config = Config::new(&auth.url, &gateway.url, &database);
        let (client, _events) = Client::open(config, secrets.clone()).unwrap();
        let failed = client
            .sign_in("a@example.com", "password", "phone", Platform::Ios)
            .await;
        assert!(matches!(failed, Err(SdkError::Secrets(_))));
        assert!(
            secrets.load(REGISTRATION_KEY).unwrap().is_some(),
            "a retry must name the same registration"
        );
        let account = client
            .sign_in("a@example.com", "password", "phone", Platform::Ios)
            .await
            .unwrap();
        assert_eq!(account.device_id, auth.device);
        assert_eq!(auth.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(secrets.load(REGISTRATION_KEY).unwrap(), None);
    }

    #[tokio::test]
    async fn an_encrypted_database_opens_only_with_its_key() {
        let scratch = crate::store::tests::Scratch::new();
        let (auth, gateway) = (FakeAuth::start().await, FakeGateway::start().await);
        let secrets = Arc::new(MemorySecretStore::new());
        let salt = DatabaseKey::new_salt();
        let key = DatabaseKey::derive(b"2468", &salt).unwrap();
        let config = |key: Option<&DatabaseKey>| Config {
            database_key: key.cloned(),
            ..Config::new(&auth.url, &gateway.url, &scratch.database())
        };
        let (client, events) = Client::open(config(Some(&key)), secrets.clone()).unwrap();
        client
            .sign_in("a@example.com", "password", "phone", Platform::Ios)
            .await
            .unwrap();
        let conversation = ConversationId::new();
        client
            .inner
            .shared
            .store()
            .open_direct(conversation, UserId::new())
            .unwrap();
        queue(&client, conversation, "for the key holder");
        drop((client, events));
        assert!(!scratch.holds("for the key holder"));

        for wrong in [None, Some(DatabaseKey::derive(b"1357", &salt).unwrap())] {
            let opened = Client::open(config(wrong.as_ref()), secrets.clone());
            assert_eq!(opened.err(), Some(SdkError::WrongKey));
        }
        let (client, _events) = Client::open(config(Some(&key)), secrets.clone()).unwrap();
        assert_eq!(client.messages(conversation, None, 10).unwrap().len(), 1);
        let changed = DatabaseKey::derive(b"new pin", &salt).unwrap();
        client.change_database_key(Some(&changed)).unwrap();
        drop(client);
        let (client, _events) = Client::open(config(Some(&changed)), secrets).unwrap();
        assert_eq!(client.messages(conversation, None, 10).unwrap().len(), 1);
    }
}
```

Change `crates/push-sdk/src/lib.rs`:

```diff
diff --git a/crates/push-sdk/src/lib.rs b/crates/push-sdk/src/lib.rs
index 9fdb820..9601f0f 100644
--- a/crates/push-sdk/src/lib.rs
+++ b/crates/push-sdk/src/lib.rs
@@ -15,6 +15,7 @@
 #![allow(dead_code)]
 
 mod auth;
+mod client;
 mod connection;
 mod credentials;
 mod engine;
@@ -28,6 +29,7 @@ mod store;
 mod testing;
 
 pub use auth::{Device, Platform, Profile};
+pub use client::{Account, Client, Config, EDIT_WINDOW, Events};
 pub use error::SdkError;
 pub use key::DatabaseKey;
 pub use model::{
@@ -35,7 +37,7 @@ pub use model::{
     MessageKind, MessageStatus, OutboxKind,
 };
 pub use push_domain::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
-pub use secrets::{MemorySecretStore, SecretError, SecretStore};
+pub use secrets::{MemorySecretStore, SECRET_KEYS, SecretError, SecretStore};
 
 #[cfg(test)]
 mod tests {
```

Change `crates/push-sdk/src/credentials.rs`:

```diff
diff --git a/crates/push-sdk/src/credentials.rs b/crates/push-sdk/src/credentials.rs
index 9c51629..c955dfe 100644
--- a/crates/push-sdk/src/credentials.rs
+++ b/crates/push-sdk/src/credentials.rs
@@ -14,7 +14,9 @@ use uuid::Uuid;
 
 use crate::SdkError;
 use crate::auth::{AuthApi, Tokens};
-use crate::secrets::{REGISTRATION_KEY, SESSION_KEY, SecretStore, Session};
+use push_domain::DeviceId;
+
+use crate::secrets::{DEVICE_KEY, REGISTRATION_KEY, SESSION_KEY, SecretStore, Session};
 
 /// An access token with less than this left is refreshed before it is used: the gateway
 /// warns 60 s before expiry, and a token used for a connection should outlast that.
@@ -40,8 +42,11 @@ impl Credentials {
         Session::load(self.secrets.as_ref()).map_err(|error| SdkError::Secrets(error.0))
     }
 
-    /// Keeps the tokens of a fresh sign-in.
+    /// Keeps the tokens of a fresh sign-in, and remembers the device.
     pub fn sign_in(&self, tokens: Tokens) -> Result<Session, SdkError> {
+        self.secrets
+            .save(DEVICE_KEY, tokens.device_id.0.as_bytes())
+            .map_err(|error| SdkError::Secrets(error.0))?;
         let session = Session {
             user_id: tokens.user_id,
             device_id: tokens.device_id,
@@ -66,6 +71,33 @@ impl Credentials {
             .map_err(|error| SdkError::Secrets(error.0))
     }
 
+    /// The device this client registered last, if it was not signed out for good.
+    pub fn last_device(&self) -> Result<Option<DeviceId>, SdkError> {
+        Ok(self
+            .secrets
+            .load(DEVICE_KEY)
+            .map_err(|error| SdkError::Secrets(error.0))?
+            .and_then(|bytes| Uuid::from_slice(&bytes).ok())
+            .map(DeviceId))
+    }
+
+    /// The device is gone (revoked): the next sign-in registers a new one.
+    pub fn forget_device(&self) -> Result<(), SdkError> {
+        self.secrets
+            .delete(DEVICE_KEY)
+            .map_err(|error| SdkError::Secrets(error.0))
+    }
+
+    /// Signs the device out for good: forgets the session and the device once no refresh
+    /// is in flight (so none saves them again), and returns the session it forgot.
+    pub async fn sign_out_for_good(&self) -> Result<Option<Session>, SdkError> {
+        let _refreshing = self.refreshing.lock().await;
+        let session = self.session()?;
+        self.forget_session()?;
+        self.forget_device()?;
+        Ok(session)
+    }
+
     /// A session whose access token is good for at least [`MIN_ACCESS_LIFETIME_S`],
     /// refreshed first if needed. [`SdkError::NotSignedIn`] when no device is signed in
     /// or auth-svc refuses the refresh token (the session is forgotten then).
```

Change `crates/push-sdk/src/secrets.rs`:

```diff
diff --git a/crates/push-sdk/src/secrets.rs b/crates/push-sdk/src/secrets.rs
index bdd7106..53f4fd1 100644
--- a/crates/push-sdk/src/secrets.rs
+++ b/crates/push-sdk/src/secrets.rs
@@ -10,10 +10,18 @@ use serde::{Deserialize, Serialize};
 
 /// The key of the signed-in device's credentials.
 pub(crate) const SESSION_KEY: &str = "push.session";
+/// The key of the device this client registered last, kept after a sign-out by the server
+/// (a password reset) so that signing in again finds the same device rather than
+/// registering another.
+pub(crate) const DEVICE_KEY: &str = "push.device";
 /// The key of a device registration that has not been answered yet: its
 /// `registration_id`, reused when the registration is retried (spec §5.3).
 pub(crate) const REGISTRATION_KEY: &str = "push.registration";
 
+/// Every key the SDK uses in the [`SecretStore`]: a store may map them to keychain items,
+/// and an app that moves its data to another store copies these.
+pub const SECRET_KEYS: [&str; 3] = [SESSION_KEY, DEVICE_KEY, REGISTRATION_KEY];
+
 /// A failure of the app's secret store, in its own words.
 #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
 #[error("{0}")]
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-sdk/src/client.rs`, in `sign_out`, delete the line
`inner.shared.store().erase_all()?;` and run:

```powershell
cargo test -p push-sdk --lib --locked client
```

Expected: `test client::tests::signing_out_offline_still_erases_and_reports_the_failed_revocation ... FAILED`
and `test client::tests::signing_in_registers_once_and_finds_the_same_device_again ... FAILED`
(the history survived a sign-out), and `3 passed; 2 failed`. Undo the edit.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "push-sdk: the Client"
```

Expected: 939 tests pass, no warnings, no diff. The key test uses a database file.

---

### Task 11: Sending

**Files:**
- Create: `crates/push-sdk/src/client/sending.rs`
- Modify: `crates/push-sdk/src/client.rs`

Decisions 10 and 11 and spec §8.3. `send` (checked, stored with its echo, the echo
returned and reported), `edit` and `delete` of the user's own messages (an unsent one
changed in place or dropped, one being sent followed by its change, a sent one changed
when the server acknowledges it), `cancel` before the first attempt, and `mark_read`. The
restart test runs two clients on one shared in-memory database
(`file:…?mode=memory&cache=shared`); Plan 9B restarts on a real file.

- [ ] **Step 1: Make the changes**

Create `crates/push-sdk/src/client/sending.rs`:

```rust
//! Sending (spec §4.2, §8.3, §8.4): messages, edits and deletes are checked, stored in
//! the outbox with their `client_msg_id` and sent once connected, in order; read positions
//! move at once and are sent too.

use push_domain::{ClientMsgId, ConversationId, UserId, validate_text};

use super::{Client, EDIT_WINDOW};
use crate::SdkError;
use crate::connection::{Command, unix_ms};
use crate::model::{Event, Message, MessageKind, OutboxKind};

impl Client {
    fn me(&self) -> Result<UserId, SdkError> {
        self.inner
            .shared
            .store()
            .owner()?
            .ok_or(SdkError::NotSignedIn)
    }

    fn wake(&self) {
        let _ = self.inner.commands.send(Command::Wake);
    }

    /// Queues a text message (spec §4.2, §8.3): checked first, stored in the outbox with
    /// a new `client_msg_id` before it is sent, and sent once connected, in order. The
    /// local echo is returned and reported as [`Event::MessageAdded`].
    pub fn send(&self, conversation: ConversationId, text: &str) -> Result<Message, SdkError> {
        validate_text(text.as_bytes()).map_err(|error| SdkError::Invalid(error.to_string()))?;
        self.me()?;
        let message = {
            let mut store = self.inner.shared.store();
            if store.conversation(conversation)?.is_none() {
                return Err(SdkError::Invalid("no such conversation".to_owned()));
            }
            store.queue_send(conversation, ClientMsgId::new(), text, unix_ms() * 1000)?
        };
        self.inner.shared.emit(Event::MessageAdded(message.clone()));
        self.wake();
        Ok(message)
    }

    /// The user's own message `target`, checked for an edit or delete (spec §8.3).
    fn own_target(
        &self,
        conversation: ConversationId,
        target: ClientMsgId,
    ) -> Result<Message, SdkError> {
        let me = self.me()?;
        let message = self
            .inner
            .shared
            .store()
            .message(conversation, target)?
            .ok_or_else(|| SdkError::Invalid("no such message".to_owned()))?;
        if message.sender_id != me {
            return Err(SdkError::Invalid("not the user's own message".to_owned()));
        }
        if message.deleted {
            return Err(SdkError::Invalid("the message is deleted".to_owned()));
        }
        if message.kind != MessageKind::Text {
            return Err(SdkError::Invalid("not a text message".to_owned()));
        }
        let window = i64::try_from(EDIT_WINDOW.as_micros()).unwrap_or(i64::MAX);
        if message
            .server_ts
            .is_some_and(|sent| unix_ms() * 1000 - sent > window)
        {
            return Err(SdkError::Invalid(
                "the time to change it has passed".to_owned(),
            ));
        }
        Ok(message)
    }

    /// Changes the text of one of the user's messages. A message still waiting in the
    /// outbox is changed in place; otherwise the edit is queued (after the message, if it
    /// is being sent) and applied locally when the server acknowledges it. The id of the
    /// outbox entry that carries the change.
    pub fn edit(
        &self,
        conversation: ConversationId,
        target: ClientMsgId,
        text: &str,
    ) -> Result<ClientMsgId, SdkError> {
        validate_text(text.as_bytes()).map_err(|error| SdkError::Invalid(error.to_string()))?;
        let message = self.own_target(conversation, target)?;
        let mut parent = None;
        if message.seq.is_none() {
            let edited = self.inner.shared.store().edit_unsent(target, text)?;
            if edited {
                let message = self.inner.shared.store().message(conversation, target)?;
                if let Some(message) = message {
                    self.inner.shared.emit(Event::MessageEdited(message));
                }
                return Ok(target);
            }
            parent = Some(target);
        }
        let id = ClientMsgId::new();
        self.inner.shared.store().queue_change(
            conversation,
            id,
            OutboxKind::Edit,
            &message,
            Some(text),
            parent,
            unix_ms() * 1000,
        )?;
        self.wake();
        Ok(id)
    }

    /// Deletes one of the user's messages: a message still waiting in the outbox is
    /// dropped; otherwise the delete is queued and the body erased locally when the
    /// server acknowledges it. The id of the outbox entry that carries the change.
    pub fn delete(
        &self,
        conversation: ConversationId,
        target: ClientMsgId,
    ) -> Result<ClientMsgId, SdkError> {
        let message = self.own_target(conversation, target)?;
        let mut parent = None;
        if message.seq.is_none() {
            if self.inner.shared.store().drop_unsent(target)? {
                self.inner.shared.emit(Event::MessageDeleted(Message {
                    body: None,
                    deleted: true,
                    ..message
                }));
                return Ok(target);
            }
            parent = Some(target);
        }
        let id = ClientMsgId::new();
        self.inner.shared.store().queue_change(
            conversation,
            id,
            OutboxKind::Delete,
            &message,
            None,
            parent,
            unix_ms() * 1000,
        )?;
        self.wake();
        Ok(id)
    }

    /// Takes back a message that has not been sent yet (spec §8.2); false once it was
    /// sent at least once, when the server may have stored it.
    pub fn cancel(
        &self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
    ) -> Result<bool, SdkError> {
        let message = self
            .inner
            .shared
            .store()
            .message(conversation, client_msg_id)?;
        let Some(message) = message else {
            return Ok(false);
        };
        if !self.inner.shared.store().drop_unsent(client_msg_id)? {
            return Ok(false);
        }
        self.inner.shared.emit(Event::MessageDeleted(Message {
            body: None,
            deleted: true,
            ..message
        }));
        Ok(true)
    }

    /// The user read everything up to `seq` (spec §4.5): the unread count drops at once
    /// and the position is sent once connected. Positions only move forward, and never
    /// past the conversation's last message the client knows.
    pub fn mark_read(&self, conversation: ConversationId, seq: i64) -> Result<(), SdkError> {
        let changed = self.inner.shared.store().mark_read(conversation, seq)?;
        if let Some(conversation) = changed {
            self.inner
                .shared
                .emit(Event::ConversationChanged(conversation));
            self.wake();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use push_proto::v1::{self, EntryKind, frame::Body};
    use uuid::Uuid;

    use super::*;
    use crate::client::tests::{Fixture, open};
    use crate::model::MessageStatus;
    use crate::store::bytes;
    use crate::store::tests::entry;
    use crate::testing::{recv_until, send};

    fn invalid<T>(result: Result<T, SdkError>) -> bool {
        matches!(result, Err(SdkError::Invalid(_)))
    }

    #[tokio::test]
    async fn nothing_invalid_is_queued() {
        let mut f = Fixture::new().await;
        let (conversation, peer) = f.conversation();
        assert!(invalid(f.client.send(conversation, "")));
        assert!(invalid(
            f.client.send(conversation, &"x".repeat(64 * 1024 + 1))
        ));
        assert!(invalid(f.client.send(ConversationId::new(), "hi")));
        assert!(f.client.inner.shared.store().outbox().unwrap().is_empty());
        assert!(f.events().is_empty());

        // Someone else's message cannot be edited or deleted.
        let theirs = entry(conversation, peer, 1, EntryKind::Message);
        f.client
            .inner
            .shared
            .store()
            .apply_deliver(&theirs)
            .unwrap();
        let theirs = ClientMsgId(Uuid::from_slice(&theirs.client_msg_id).unwrap());
        assert!(invalid(f.client.edit(conversation, theirs, "mine now")));
        assert!(invalid(f.client.delete(conversation, theirs)));

        // Nor one of the user's own after the 48 hours.
        let old = f.client.send(conversation, "old").unwrap();
        let sent_long_ago = unix_ms() * 1000 - i64::try_from(EDIT_WINDOW.as_micros()).unwrap() - 1;
        let ack = v1::SendAck {
            client_msg_id: bytes(old.client_msg_id.0),
            message_id: bytes(Uuid::now_v7()),
            seq: 2,
            server_ts: sent_long_ago,
        };
        f.client.inner.shared.store().acknowledged(&ack).unwrap();
        assert!(invalid(f.client.edit(
            conversation,
            old.client_msg_id,
            "too late"
        )));
    }

    #[tokio::test]
    async fn an_unsent_message_is_changed_in_place_or_taken_back() {
        let mut f = Fixture::new().await;
        let (conversation, _) = f.conversation();
        let message = f.client.send(conversation, "helo").unwrap();
        assert_eq!(message.status, MessageStatus::Pending);
        let id = message.client_msg_id;
        assert_eq!(f.client.edit(conversation, id, "hello").unwrap(), id);
        let second = f.client.send(conversation, "oops").unwrap();
        assert!(f.client.cancel(conversation, second.client_msg_id).unwrap());
        let events = f.events();
        assert!(matches!(&events[..], [
            Event::MessageAdded(_),
            Event::MessageEdited(edited),
            Event::MessageAdded(_),
            Event::MessageDeleted(cancelled),
        ] if edited.body.as_deref() == Some("hello") && cancelled.deleted));
        let outbox = f.client.inner.shared.store().outbox().unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].body.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn the_outbox_survives_a_restart() {
        let f = Fixture::new().await;
        // Holds the shared in-memory database open between the two clients.
        let _keep = rusqlite::Connection::open(&f.database).unwrap();
        let (conversation, _) = f.conversation();
        let queued = f.client.send(conversation, "sent after a restart").unwrap();
        let Fixture {
            client,
            events,
            auth,
            gateway,
            secrets,
            database,
        } = f;
        drop((client, events));

        let (client, events) = open(&auth, &gateway, &database, &secrets);
        let mut f = Fixture {
            client,
            events,
            auth,
            gateway,
            secrets,
            database,
        };
        f.client.connect().unwrap();
        let mut socket = f.ready().await;
        let Body::Send(sent) = recv_until(&mut socket, |body| matches!(body, Body::Send(_))).await
        else {
            unreachable!();
        };
        assert_eq!(sent.client_msg_id, bytes(queued.client_msg_id.0));
        let ack = v1::SendAck {
            client_msg_id: sent.client_msg_id,
            message_id: bytes(Uuid::now_v7()),
            seq: 1,
            server_ts: unix_ms() * 1000,
        };
        send(&mut socket, Body::SendAck(ack)).await;
        let sent = loop {
            match tokio::time::timeout(Duration::from_secs(10), f.events.next()).await {
                Ok(Some(Event::MessageSent(message))) => break message,
                Ok(Some(_)) => {}
                other => panic!("no MessageSent: {other:?}"),
            }
        };
        assert_eq!(
            (sent.client_msg_id, sent.seq),
            (queued.client_msg_id, Some(1))
        );
    }
}
```

Change `crates/push-sdk/src/client.rs`:

```diff
diff --git a/crates/push-sdk/src/client.rs b/crates/push-sdk/src/client.rs
index 94675c3..0eeda47 100644
--- a/crates/push-sdk/src/client.rs
+++ b/crates/push-sdk/src/client.rs
@@ -6,6 +6,8 @@
 //! and can be awaited from any executor; its plain methods read or change the local store
 //! at once. It is cheap to clone, and every clone is the same client.
 
+mod sending;
+
 use std::future::Future;
 use std::sync::Arc;
 use std::time::Duration;
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-sdk/src/client/sending.rs`, in `send`, replace the first line of the body,
`validate_text(text.as_bytes()).map_err(|error| SdkError::Invalid(error.to_string()))?;`,
with `let _ = validate_text(text.as_bytes());` and run:

```powershell
cargo test -p push-sdk --lib --locked client
```

Expected: `test client::sending::tests::nothing_invalid_is_queued ... FAILED` (an empty
message was queued) and `7 passed; 1 failed`. Undo the edit.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "push-sdk: sending"
```

Expected: 942 tests pass, no warnings, no diff.

---

### Task 12: The Client's requests

**Files:**
- Create: `crates/push-sdk/src/client/requests.rs`
- Modify: `crates/push-sdk/src/client.rs`, `crates/push-sdk/src/lib.rs`, `README.md`

Decisions 12 and 14. The requests over the connection (`list_conversations`,
`fetch_history`, `open_direct`, `list_contacts`, `request_contact`, `accept_contact`,
`decline_contact`, `block`, `unblock`, `create_invite`, `redeem_invite`, `revoke_invite`)
and auth-svc's signed-in calls (`profiles`, `lookup`, `devices`, `revoke_device`). No group
call exists. The crate-level `allow(dead_code)` goes. The README gets a section.

- [ ] **Step 1: Make the changes**

Create `crates/push-sdk/src/client/requests.rs`:

```rust
//! The client's requests (spec §7.1, §5.5): they go over the connection once it is up and
//! are answered there; and auth-svc's calls for a signed-in device.

use push_domain::{
    ConversationId, DeviceId, MAX_INVITE_LIFETIME_SECS, MAX_INVITE_USES, MIN_INVITE_LIFETIME_SECS,
    UserId,
};
use push_proto::v1::{
    ContactAccept, ContactBlock, ContactDecline, ContactRequest, ContactUnblock, CreateInvite,
    FetchHistory, ListContacts, ListConversations, OpenDirect, RedeemInvite, RevokeInvite, request,
    response,
};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::Client;
use crate::SdkError;
use crate::auth::{Device, Profile};
use crate::connection::Command;
use crate::model::{ConnectionState, Contact, ContactState, Conversation, Event, Invite, Message};
use crate::store::bytes;

impl Client {
    /// Sends a request once connected, waiting for the connection and then for the
    /// answer up to the request timeout each. Fails at once while signed out, removed,
    /// opened elsewhere or not asked to connect.
    async fn request(&self, body: request::Body) -> Result<response::Body, SdkError> {
        let inner = self.inner.clone();
        self.run(async move {
            let mut state = inner.state.clone();
            let ready = tokio::time::timeout(
                inner.request_timeout,
                state.wait_for(|state| {
                    !matches!(
                        state,
                        ConnectionState::Connecting | ConnectionState::Reconnecting { .. }
                    )
                }),
            )
            .await;
            match ready.map(|state| state.map(|state| *state)) {
                Ok(Ok(ConnectionState::Connected)) => {}
                Ok(Ok(ConnectionState::SignedOut)) => return Err(SdkError::NotSignedIn),
                Ok(Ok(ConnectionState::DeviceRemoved)) => return Err(SdkError::DeviceRemoved),
                Ok(Ok(ConnectionState::OpenedElsewhere)) => {
                    return Err(SdkError::OpenedElsewhere);
                }
                Ok(Ok(_)) | Err(_) => return Err(SdkError::Offline),
                Ok(Err(_)) => return Err(SdkError::Closed),
            }
            let (reply, answer) = oneshot::channel();
            inner
                .commands
                .send(Command::Request { body, reply })
                .map_err(|_| SdkError::Closed)?;
            match tokio::time::timeout(inner.request_timeout, answer).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(SdkError::Disconnected),
                Err(_) => Err(SdkError::Network("no answer in time".to_owned())),
            }
        })
        .await
    }

    /// Lists the conversations from the server and stores what changed.
    pub async fn list_conversations(&self) -> Result<Vec<Conversation>, SdkError> {
        let body = request::Body::ListConversations(ListConversations::default());
        let response::Body::Conversations(list) = self.request(body).await? else {
            return Err(unexpected());
        };
        for summary in &list.conversations {
            if let Some(changed) = self.inner.shared.store().apply_summary(summary)? {
                self.inner.shared.emit(Event::ConversationChanged(changed));
            }
        }
        self.conversations()
    }

    /// Loads older messages from the server (spec §4.6: messages and system entries in
    /// their current state) and stores them; oldest first.
    pub async fn fetch_history(
        &self,
        conversation: ConversationId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, SdkError> {
        let body = request::Body::FetchHistory(FetchHistory {
            conversation_id: bytes(conversation.0),
            before_seq,
            limit,
        });
        let response::Body::History(page) = self.request(body).await? else {
            return Err(unexpected());
        };
        self.inner.shared.store().store_history(&page.entries)
    }

    /// The direct conversation with an accepted contact, created the first time.
    pub async fn open_direct(&self, peer: UserId) -> Result<Conversation, SdkError> {
        let body = request::Body::OpenDirect(OpenDirect {
            peer_user_id: bytes(peer.0),
        });
        let response::Body::DirectOpened(opened) = self.request(body).await? else {
            return Err(unexpected());
        };
        let id = Uuid::from_slice(&opened.conversation_id).map_err(|_| unexpected())?;
        let conversation = self
            .inner
            .shared
            .store()
            .open_direct(ConversationId(id), peer)?;
        self.inner
            .shared
            .emit(Event::ConversationChanged(conversation.clone()));
        Ok(conversation)
    }

    /// Lists the user's relationships from the server and stores what changed.
    pub async fn list_contacts(&self) -> Result<Vec<Contact>, SdkError> {
        let response::Body::Contacts(list) = self
            .request(request::Body::ListContacts(ListContacts {}))
            .await?
        else {
            return Err(unexpected());
        };
        let listed: Vec<Contact> = list
            .contacts
            .iter()
            .filter_map(|contact| {
                Some(Contact {
                    user_id: UserId(Uuid::from_slice(&contact.user_id).ok()?),
                    state: ContactState::from_wire(contact.state()),
                })
            })
            .collect();
        let changed = self.inner.shared.store().replace_contacts(&listed)?;
        for contact in changed {
            self.inner.shared.emit(Event::ContactChanged(contact));
        }
        Ok(listed)
    }

    /// Runs a contact request that answers `Done`, then lists the contacts: the user's
    /// relationship with `user` now.
    async fn contact_change(&self, body: request::Body, user: UserId) -> Result<Contact, SdkError> {
        self.request(body).await?;
        let listed = self.list_contacts().await?;
        Ok(listed
            .into_iter()
            .find(|contact| contact.user_id == user)
            .unwrap_or(Contact {
                user_id: user,
                state: ContactState::None,
            }))
    }

    /// Asks `user` to become a contact (spec §5.5).
    pub async fn request_contact(&self, user: UserId) -> Result<Contact, SdkError> {
        let body = request::Body::ContactRequest(ContactRequest {
            user_id: bytes(user.0),
        });
        let response::Body::Contact(contact) = self.request(body).await? else {
            return Err(unexpected());
        };
        let contact = Contact {
            user_id: user,
            state: ContactState::from_wire(contact.state()),
        };
        if self.inner.shared.store().set_contact(contact)? {
            self.inner.shared.emit(Event::ContactChanged(contact));
        }
        Ok(contact)
    }

    pub async fn accept_contact(&self, user: UserId) -> Result<Contact, SdkError> {
        let body = request::Body::ContactAccept(ContactAccept {
            user_id: bytes(user.0),
        });
        self.contact_change(body, user).await
    }

    pub async fn decline_contact(&self, user: UserId) -> Result<Contact, SdkError> {
        let body = request::Body::ContactDecline(ContactDecline {
            user_id: bytes(user.0),
        });
        self.contact_change(body, user).await
    }

    pub async fn block(&self, user: UserId) -> Result<Contact, SdkError> {
        let body = request::Body::ContactBlock(ContactBlock {
            user_id: bytes(user.0),
        });
        self.contact_change(body, user).await
    }

    pub async fn unblock(&self, user: UserId) -> Result<Contact, SdkError> {
        let body = request::Body::ContactUnblock(ContactUnblock {
            user_id: bytes(user.0),
        });
        self.contact_change(body, user).await
    }

    /// An invite code (spec §5.5): valid for `expires_in_s` (60 s to 30 days) and
    /// `max_uses` redemptions (1 to 100).
    pub async fn create_invite(
        &self,
        expires_in_s: u32,
        max_uses: u32,
    ) -> Result<Invite, SdkError> {
        if !(MIN_INVITE_LIFETIME_SECS..=MAX_INVITE_LIFETIME_SECS).contains(&i64::from(expires_in_s))
        {
            return Err(SdkError::Invalid(format!(
                "an invite lasts {MIN_INVITE_LIFETIME_SECS} to {MAX_INVITE_LIFETIME_SECS} seconds"
            )));
        }
        if !(1..=MAX_INVITE_USES).contains(&max_uses) {
            return Err(SdkError::Invalid(format!(
                "an invite allows 1 to {MAX_INVITE_USES} uses"
            )));
        }
        let body = request::Body::CreateInvite(CreateInvite {
            expires_in_s,
            max_uses,
        });
        let response::Body::Invite(invite) = self.request(body).await? else {
            return Err(unexpected());
        };
        Ok(Invite {
            code: invite.code,
            expires_at: invite.expires_at,
            max_uses: invite.max_uses,
            uses: invite.uses,
        })
    }

    /// Redeems an invite: its creator becomes an accepted contact. Returns the creator.
    pub async fn redeem_invite(&self, code: &str) -> Result<UserId, SdkError> {
        let body = request::Body::RedeemInvite(RedeemInvite {
            code: code.to_owned(),
        });
        let response::Body::InviteRedeemed(redeemed) = self.request(body).await? else {
            return Err(unexpected());
        };
        let creator = UserId(Uuid::from_slice(&redeemed.user_id).map_err(|_| unexpected())?);
        let contact = Contact {
            user_id: creator,
            state: ContactState::Accepted,
        };
        if self.inner.shared.store().set_contact(contact)? {
            self.inner.shared.emit(Event::ContactChanged(contact));
        }
        Ok(creator)
    }

    pub async fn revoke_invite(&self, code: &str) -> Result<(), SdkError> {
        let body = request::Body::RevokeInvite(RevokeInvite {
            code: code.to_owned(),
        });
        self.request(body).await?;
        Ok(())
    }

    // ----- auth-svc, signed in -----

    /// The profiles among `ids` the user may see: themselves, contacts, and anyone they
    /// share or shared a conversation with (spec §5.5).
    pub async fn profiles(&self, ids: Vec<UserId>) -> Result<Vec<Profile>, SdkError> {
        let inner = self.inner.clone();
        self.run(async move {
            let session = inner.credentials.fresh().await?;
            inner.auth.profiles(&session.access_token, &ids).await
        })
        .await
    }

    /// The verified user with this handle.
    pub async fn lookup(&self, handle: &str) -> Result<Profile, SdkError> {
        let (inner, handle) = (self.inner.clone(), handle.to_owned());
        self.run(async move {
            let session = inner.credentials.fresh().await?;
            inner.auth.lookup(&session.access_token, &handle).await
        })
        .await
    }

    /// The user's devices.
    pub async fn devices(&self) -> Result<Vec<Device>, SdkError> {
        let inner = self.inner.clone();
        self.run(async move {
            let session = inner.credentials.fresh().await?;
            inner.auth.devices(&session.access_token).await
        })
        .await
    }

    /// Revokes another of the user's devices; it is disconnected within seconds.
    pub async fn revoke_device(&self, device: DeviceId) -> Result<(), SdkError> {
        let inner = self.inner.clone();
        self.run(async move {
            let session = inner.credentials.fresh().await?;
            inner
                .auth
                .revoke_device(&session.access_token, device)
                .await
        })
        .await
    }
}

fn unexpected() -> SdkError {
    SdkError::Network("the server answered with something unexpected".to_owned())
}

#[cfg(test)]
mod tests {
    use push_proto::v1::{self, frame::Body};

    use super::*;
    use crate::client::tests::Fixture;
    use crate::testing::{recv_until, send};

    #[tokio::test]
    async fn requests_fail_fast_offline_and_round_trip_once_connected() {
        let mut f = Fixture::new().await;
        let peer = UserId::new();
        assert_eq!(f.client.open_direct(peer).await, Err(SdkError::Offline));

        f.client.connect().unwrap();
        let mut socket = f.ready().await;
        let client = f.client.clone();
        let opening = tokio::spawn(async move { client.open_direct(peer).await });
        let Body::Request(request) = recv_until(&mut socket, |body| {
            matches!(body, Body::Request(request) if matches!(request.body, Some(request::Body::OpenDirect(_))))
        })
        .await
        else {
            unreachable!();
        };
        let conversation = ConversationId::new();
        let answer = v1::Response {
            request_id: request.request_id,
            body: Some(response::Body::DirectOpened(v1::DirectOpened {
                conversation_id: bytes(conversation.0),
            })),
        };
        send(&mut socket, Body::Response(answer)).await;
        let opened = opening.await.unwrap().unwrap();
        assert_eq!((opened.id, opened.peer), (conversation, Some(peer)));
        assert_eq!(f.client.conversation(conversation).unwrap(), Some(opened));
    }

    #[tokio::test]
    async fn a_request_right_after_connect_waits_for_the_connection() {
        let mut f = Fixture::new().await;
        f.client.connect().unwrap();
        // Set by `connect` itself, before the connection task has even seen the command.
        assert_eq!(f.client.connection_state(), ConnectionState::Connecting);
        let client = f.client.clone();
        let peer = UserId::new();
        let opening = tokio::spawn(async move { client.open_direct(peer).await });
        let mut socket = f.ready().await;
        let Body::Request(request) = recv_until(&mut socket, |body| {
            matches!(body, Body::Request(request) if matches!(request.body, Some(request::Body::OpenDirect(_))))
        })
        .await
        else {
            unreachable!();
        };
        let answer = v1::Response {
            request_id: request.request_id,
            body: Some(response::Body::DirectOpened(v1::DirectOpened {
                conversation_id: bytes(Uuid::now_v7()),
            })),
        };
        send(&mut socket, Body::Response(answer)).await;
        assert!(opening.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn an_invite_is_checked_before_it_is_asked_for() {
        let f = Fixture::new().await;
        let invalid =
            |result: Result<Invite, SdkError>| matches!(result, Err(SdkError::Invalid(_)));
        assert!(invalid(f.client.create_invite(30, 1).await));
        assert!(invalid(f.client.create_invite(600, 0).await));
    }
}
```

Change `crates/push-sdk/src/client.rs`:

```diff
diff --git a/crates/push-sdk/src/client.rs b/crates/push-sdk/src/client.rs
index 0eeda47..b032b3d 100644
--- a/crates/push-sdk/src/client.rs
+++ b/crates/push-sdk/src/client.rs
@@ -6,6 +6,7 @@
 //! and can be awaited from any executor; its plain methods read or change the local store
 //! at once. It is cheap to clone, and every clone is the same client.
 
+mod requests;
 mod sending;
 
 use std::future::Future;
```

Change `crates/push-sdk/src/lib.rs`:

```diff
diff --git a/crates/push-sdk/src/lib.rs b/crates/push-sdk/src/lib.rs
index 9601f0f..701de57 100644
--- a/crates/push-sdk/src/lib.rs
+++ b/crates/push-sdk/src/lib.rs
@@ -10,10 +10,6 @@
 //! are therefore plain data: identifiers, strings, numbers and enums, with no lifetimes
 //! or generics.
 
-// The parts arrive one task at a time and the `Client` (Task 10) ties them together; until
-// then some crate-private items have no caller outside their tests.
-#![allow(dead_code)]
-
 mod auth;
 mod client;
 mod connection;
```

Change `README.md`:

```diff
diff --git a/README.md b/README.md
index 565e4a3..a02a6ce 100644
--- a/README.md
+++ b/README.md
@@ -17,6 +17,28 @@ Docker, so Docker must be running (the first run pulls the images):
 Building needs neither a database nor a system `protoc` (`push-proto` uses a vendored
 one).
 
+## The client SDK
+
+`crates/push-sdk` is the Rust core that apps embed (the Node and C bindings come with
+Plan 10). A `Client` signs up and in through auth-svc, keeps the device's credentials in
+a `SecretStore` the app provides (the platform's keychain), and holds the WebSocket to the
+gateway, reconnecting with backoff; it signs out only when the server says so (4001) and
+stops only when the device is removed (4003) or its data was opened elsewhere (4007).
+Messages, edits and deletes go through an outbox in the client's SQLite database, so they
+survive a restart and are stored once, and the UI follows one stream of events:
+
+    let (client, mut events) = Client::open(Config::new(auth_url, socket_url, "push.db"), secrets)?;
+    client.sign_in(email, password, "Alice's phone", Platform::Ios).await?;
+    client.connect()?;
+    let chat = client.open_direct(bob).await?;
+    client.send(chat.id, "hello")?;
+    while let Some(event) = events.next().await { /* update the UI */ }
+
+Given a `DatabaseKey` (`Config::database_key`), the client encrypts the whole database;
+an app derives the key from a PIN or password the user enters (`DatabaseKey::derive`), so
+without it nobody can read the history, not even the app. Groups are not offered to apps
+in the first release, so the SDK has no group calls yet.
+
 ## Run it on minikube
 
 The whole system runs on a local [minikube](https://minikube.sigs.k8s.io/) cluster with
```

- [ ] **Step 2: Watch a test catch a mistake (red)**

In `crates/push-sdk/src/client.rs`, in `connect`, change the `ConnectionState::Connecting,`
that `set_state` is given to `ConnectionState::Offline,` and run:

```powershell
cargo test -p push-sdk --lib --locked client
```

Expected: `test client::requests::tests::a_request_right_after_connect_waits_for_the_connection ... FAILED`
(the state was still `Offline` when `connect` returned, so a request at once would fail as
offline) and `10 passed; 1 failed`. Undo the edit.

- [ ] **Step 3: Run the gates and commit (green)**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates README.md
git commit -m "push-sdk: the Client's requests"
```

Expected: 945 tests pass (70 in `push-sdk`), no warnings, no diff.

---

### Task 13: The guide: secret stores and the database key

**Files:**
- Create: `docs/sdk-secure-storage.md`
- Modify: `crates/push-sdk/src/secrets.rs`, `README.md`

Owner decisions 1, 3 and 4. A guide for app developers: implementing `SecretStore` with
the Windows Credential Manager, the macOS and iOS Keychain, the Android Keystore and
libsecret (code for Rust's `keyring` crate, Swift and Kotlin), the rules for the bytes,
what never to do; deriving the database key from a PIN or a password, why a short PIN
needs the platform's help, changing the key safely, a forgotten key, what the key does not
cover; and why a remote wipe is not a security feature. A test keeps the guide's list of
keys in step with `SECRET_KEYS`.

- [ ] **Step 1: The test first (red)**

Change `crates/push-sdk/src/secrets.rs`:

```diff
diff --git a/crates/push-sdk/src/secrets.rs b/crates/push-sdk/src/secrets.rs
index 53f4fd1..551ed08 100644
--- a/crates/push-sdk/src/secrets.rs
+++ b/crates/push-sdk/src/secrets.rs
@@ -151,6 +151,15 @@ mod tests {
         assert_eq!(Session::load(&store), Ok(None));
     }
 
+    /// docs/sdk-secure-storage.md tells app developers which keys to expect.
+    #[test]
+    fn the_guide_names_every_secret_key() {
+        let guide = include_str!("../../../docs/sdk-secure-storage.md");
+        for key in SECRET_KEYS {
+            assert!(guide.contains(&format!("`{key}`")), "{key}");
+        }
+    }
+
     #[test]
     fn debug_output_never_shows_a_token() {
         let shown = format!("{:?}", session());
```

```powershell
cargo test -p push-sdk --lib --locked secrets
```

Expected: the test does not compile, because the guide does not exist yet:

```
error: couldn't read `crates\push-sdk\src\../../../docs/sdk-secure-storage.md`: The system cannot find the file specified. (os error 2)
```

- [ ] **Step 2: Write the guide (green)**

Change `docs/sdk-secure-storage.md`:

```diff
diff --git a/docs/sdk-secure-storage.md b/docs/sdk-secure-storage.md
new file mode 100644
index 0000000..e4e4652
--- /dev/null
+++ b/docs/sdk-secure-storage.md
@@ -0,0 +1,313 @@
+# Keeping a push-sdk app's secrets safe
+
+`push-sdk` keeps two kinds of things on the device, and the app decides how each is
+protected:
+
+- **The device's credentials** — its access and refresh tokens, its device id and a
+  registration in progress. The SDK never writes them anywhere itself: it hands them to a
+  `SecretStore` the app provides. Anyone who copies them can act as the user on that
+  device until the device is revoked.
+- **The local database** — conversations, message history, contacts and the outbox of
+  unsent messages, in one SQLite file. It is plain unless the app opens the client with a
+  `DatabaseKey`; then the whole file is encrypted.
+
+The SDK ships no keychain integration (owner decision): every platform has its own, and
+the app already knows which one it runs on. This guide shows how to connect the SDK to the
+platform's secure storage, what never to do, and how to derive and change the database
+key.
+
+## The secret store
+
+```rust
+pub trait SecretStore: Send + Sync {
+    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError>;
+    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError>;
+    fn delete(&self, key: &str) -> Result<(), SecretError>;
+}
+```
+
+The SDK uses three keys, listed in `push_sdk::SECRET_KEYS`: `push.session` (the tokens),
+`push.device` (the device id, kept after a password reset so the same device signs in
+again) and `push.registration` (the id of a device registration not answered yet).
+
+Rules for an implementation:
+
+- **The values are opaque bytes.** Store them exactly as given and return them unchanged;
+  never parse, trim or re-encode them. If the platform API only takes text, encode with
+  base64 and decode on the way back. Each value is small (under 2 KiB).
+- **`load` of a missing key is `Ok(None)`, not an error;** `delete` of a missing key
+  succeeds. An error means the store could not answer (locked, unavailable): the SDK then
+  fails the call and does not guess.
+- **Error messages never contain a value.** Map the platform's error to a fixed
+  description; some platform errors carry the data they failed on.
+- **The calls block and come from SDK threads.** Keep them quick; never call back into
+  the client from inside them. The store must be safe to call from several threads.
+- **Make the items device-only.** They must not sync to other devices or be restored onto
+  another one: a copy of the credentials on a second machine makes two apps share one
+  device, and the gateway lets only one of them stay connected (the other reports
+  `OpenedElsewhere`).
+- **Overwrite buffers you made.** Where the language allows, clear temporary copies of a
+  value after use.
+
+### Never do this
+
+- Never write the values to a plain file, a database, `SharedPreferences`,
+  `UserDefaults`, `localStorage`, the registry or an environment variable. Any program
+  running as the same user — or anyone with a backup — can read them and sign in as the
+  user.
+- Never log them, put them in crash reports or analytics, or show them in a UI.
+- Never fall back to a file when the keychain is unavailable (a Linux machine without a
+  Secret Service, say). Fail, and tell the user why.
+- Never keep a PIN or password the user entered once the key is derived.
+
+### Desktop: Windows, macOS and Linux from Rust
+
+The [`keyring`](https://crates.io/crates/keyring) crate (version 4, with its default `v1`
+feature) reaches the Windows Credential Manager, the macOS Keychain and the Linux Secret
+Service (GNOME Keyring, KWallet) behind one API, choosing the store for the platform it
+runs on. An app that wants to pick its stores itself uses `keyring-core` and the store
+crates directly; the calls are the same.
+
+```rust
+use keyring::v1::{Entry, Error};
+use push_sdk::{SecretError, SecretStore};
+
+/// The operating system's credential store, under one service name per app profile.
+pub struct KeyringStore {
+    service: String,
+}
+
+impl KeyringStore {
+    pub fn new(service: &str) -> Self {
+        Self { service: service.to_owned() }
+    }
+
+    fn entry(&self, key: &str) -> Result<Entry, SecretError> {
+        Entry::new(&self.service, key).map_err(failed)
+    }
+}
+
+impl SecretStore for KeyringStore {
+    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError> {
+        match self.entry(key)?.get_secret() {
+            Ok(value) => Ok(Some(value)),
+            Err(Error::NoEntry) => Ok(None),
+            Err(error) => Err(failed(error)),
+        }
+    }
+
+    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError> {
+        self.entry(key)?.set_secret(value).map_err(failed)
+    }
+
+    fn delete(&self, key: &str) -> Result<(), SecretError> {
+        match self.entry(key)?.delete_credential() {
+            Ok(()) | Err(Error::NoEntry) => Ok(()),
+            Err(error) => Err(failed(error)),
+        }
+    }
+}
+
+/// A fixed description: some keyring errors (`BadEncoding`, `BadDataFormat`) carry the
+/// bytes they failed on.
+fn failed(error: Error) -> SecretError {
+    let what = match error {
+        Error::NoStorageAccess(_) => "the credential store is locked or unavailable",
+        Error::PlatformFailure(_) => "the credential store failed",
+        _ => "the credential store refused the request",
+    };
+    SecretError(what.to_owned())
+}
+```
+
+Platform notes:
+
+- **Windows Credential Manager** keeps generic credentials per user, protected with DPAPI.
+  A credential's blob holds at most 2,560 bytes, which the SDK's values fit. Written with
+  `CRED_PERSIST_LOCAL_MACHINE`, an item stays on this machine and does not roam with the
+  user's profile; check the persistence your store uses.
+- **macOS Keychain**: items go to the user's login keychain; a signed app is the only one
+  allowed to read them without asking the user.
+- **Linux libsecret**: the Secret Service must be running and unlocked (it is in a normal
+  desktop session). On a headless machine there is none: fail rather than store elsewhere.
+
+### iOS and macOS apps in Swift
+
+Through the C or Node binding (Plan 10) the three calls become callbacks. Back them with
+generic-password items, one per key, readable only on this device after its first unlock
+(so the app can reconnect in the background) and never synced or restored elsewhere:
+
+```swift
+let service = "com.example.chat.push"
+
+func save(_ key: String, _ value: Data) -> Bool {
+    let item: [String: Any] = [
+        kSecClass as String: kSecClassGenericPassword,
+        kSecAttrService as String: service,
+        kSecAttrAccount as String: key,
+    ]
+    SecItemDelete(item as CFDictionary)
+    var add = item
+    add[kSecValueData as String] = value
+    add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
+    return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
+}
+
+func load(_ key: String) -> (found: Bool, value: Data?, ok: Bool) {
+    let query: [String: Any] = [
+        kSecClass as String: kSecClassGenericPassword,
+        kSecAttrService as String: service,
+        kSecAttrAccount as String: key,
+        kSecReturnData as String: true,
+        kSecMatchLimit as String: kSecMatchLimitOne,
+    ]
+    var result: AnyObject?
+    switch SecItemCopyMatching(query as CFDictionary, &result) {
+    case errSecSuccess: return (true, result as? Data, true)
+    case errSecItemNotFound: return (false, nil, true)
+    default: return (false, nil, false)
+    }
+}
+
+func delete(_ key: String) -> Bool {
+    let item: [String: Any] = [
+        kSecClass as String: kSecClassGenericPassword,
+        kSecAttrService as String: service,
+        kSecAttrAccount as String: key,
+    ]
+    let status = SecItemDelete(item as CFDictionary)
+    return status == errSecSuccess || status == errSecItemNotFound
+}
+```
+
+### Android in Kotlin
+
+The Android Keystore holds keys, not arbitrary data: keep an AES-256-GCM key in it (it
+never leaves the secure hardware where there is one) and store the values sealed with it
+in the app's private preferences. Exclude that file from backups
+(`android:dataExtractionRules` / `android:fullBackupContent`), so a sealed value never
+reaches a device whose Keystore cannot open it.
+
+```kotlin
+private const val ALIAS = "push-sdk"
+private val prefs = context.getSharedPreferences("push-sdk-secrets", Context.MODE_PRIVATE)
+
+private fun keystoreKey(): SecretKey {
+    val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
+    (store.getKey(ALIAS, null) as? SecretKey)?.let { return it }
+    val spec = KeyGenParameterSpec.Builder(
+        ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
+    )
+        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
+        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
+        .setKeySize(256)
+        .build()
+    return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
+        .apply { init(spec) }
+        .generateKey()
+}
+
+fun save(key: String, value: ByteArray) {
+    val cipher = Cipher.getInstance("AES/GCM/NoPadding")
+    cipher.init(Cipher.ENCRYPT_MODE, keystoreKey())
+    val sealed = cipher.iv + cipher.doFinal(value) // the 12-byte IV first
+    check(prefs.edit().putString(key, Base64.encodeToString(sealed, Base64.NO_WRAP)).commit())
+}
+
+fun load(key: String): ByteArray? {
+    val text = prefs.getString(key, null) ?: return null
+    val sealed = Base64.decode(text, Base64.NO_WRAP)
+    val cipher = Cipher.getInstance("AES/GCM/NoPadding")
+    cipher.init(Cipher.DECRYPT_MODE, keystoreKey(), GCMParameterSpec(128, sealed, 0, 12))
+    return cipher.doFinal(sealed, 12, sealed.size - 12)
+}
+
+fun delete(key: String) {
+    check(prefs.edit().remove(key).commit())
+}
+```
+
+## The database key
+
+Opened with a key, the client encrypts the whole SQLite file — every page of the history,
+the contacts, the outbox and the write-ahead log — with SQLite3 Multiple Ciphers
+(ChaCha20-Poly1305, its default). Without the key nobody can read the user's messages from
+the file: not another program, not a backup, not the app itself. The SDK never stores the
+key; the app provides it every time it opens the client.
+
+```rust
+let mut config = Config::new(auth_url, socket_url, &database_path);
+config.database_key = Some(DatabaseKey::derive(pin.as_bytes(), &salt)?);
+let (client, events) = match Client::open(config, secrets) {
+    Err(SdkError::WrongKey) => return ask_for_the_pin_again(),
+    opened => opened?,
+};
+```
+
+`SdkError::WrongKey` means the key does not open the file — a wrong PIN, a database that
+has a key opened without one, or a plain database opened with one. Nothing is written
+before the key is checked, so a wrong key never damages the data.
+
+### Deriving the key from a PIN or a password
+
+`DatabaseKey::derive(secret, salt)` runs Argon2id (19 MiB, 2 passes, 1 lane), which turns
+what the user typed into a 32-byte key and makes every guess cost tens of milliseconds.
+
+1. The first time, make a salt with `DatabaseKey::new_salt()` and keep it next to the
+   database: in the app's preferences, or a small file beside it. The salt is not
+   secret, but it must not be lost — without it the same PIN gives another key.
+2. Each time the app starts, ask for the PIN, derive the key with the stored salt and open
+   the client. Derive it off the UI thread.
+3. Forget the PIN and the key as soon as the client is open.
+
+**A short PIN is weak on its own.** Someone holding a copy of the file can try every
+four-digit PIN — 10,000 guesses — in minutes, however slow each one is. If a PIN is all
+the user will type, let the platform add what a copy of the file lacks: generate a random
+key once (`DatabaseKey::from_bytes` with 32 random bytes), keep it sealed by a hardware key
+that requires the user's authentication (an Android Keystore key with
+`setUserAuthenticationRequired`, a Secure Enclave key with an access control), and unseal
+it when the user unlocks. The hardware then limits the guesses; the file alone is
+useless. With a password instead of a PIN, `derive` alone is strong.
+
+Bytes from elsewhere — a hardware-held key, a key from your own backend — go in with
+`DatabaseKey::from_bytes` (exactly 32 bytes).
+
+### Changing, adding and removing the key
+
+`Client::change_database_key(Some(&new_key))` re-encrypts the file in one transaction (a
+crash leaves the old key or the new one, never a mix); `Some` on a plain database encrypts
+it, `None` decrypts it. When the user changes the PIN:
+
+1. derive the new key with a **new** salt;
+2. save the new salt under a second name ("pending"), keeping the old one;
+3. call `change_database_key`;
+4. replace the old salt with the pending one.
+
+If the app stops between 2 and 4, the next start tries the old salt and then the pending
+one.
+
+### A forgotten key
+
+It cannot be recovered: that is the point. Delete the database file (and its `-wal` and
+`-shm` files), sign in again, and the client starts empty; history comes back from the
+server as the user scrolls (`fetch_history`). Messages that were waiting in the outbox are
+lost.
+
+### What the key does not cover
+
+- While the client is open the data is in the app's memory, readable by anyone who can
+  read that memory.
+- The file's size and modification times stay visible, so an observer can tell roughly how
+  much the app stores and when it changed.
+- The credentials are in the secret store, not the database: protect them as above.
+
+## Signing out and remote wipe
+
+A sign-out by the server — after a password reset — keeps the history and the unsent
+messages on the device, so the same person finds them when signing in again; they are
+erased only when a different person signs in on that device, or when the user signs out
+in the app (`Client::sign_out`), which erases them at once, also offline (owner decision).
+
+A remote wipe is not a security feature and the SDK does not offer one: a thief can keep
+the device offline, so a wipe that waits for a connection protects nothing. What protects
+the data on a lost device is the device's own lock and, for the history, the database key.
```

Change `README.md`:

```diff
diff --git a/README.md b/README.md
index a02a6ce..4ec2662 100644
--- a/README.md
+++ b/README.md
@@ -36,8 +36,11 @@ survive a restart and are stored once, and the UI follows one stream of events:
 
 Given a `DatabaseKey` (`Config::database_key`), the client encrypts the whole database;
 an app derives the key from a PIN or password the user enters (`DatabaseKey::derive`), so
-without it nobody can read the history, not even the app. Groups are not offered to apps
-in the first release, so the SDK has no group calls yet.
+without it nobody can read the history, not even the app. The SDK ships no keychain store
+and never stores the key: [docs/sdk-secure-storage.md](docs/sdk-secure-storage.md) shows
+how to keep the credentials in the platform's keychain and how to derive and change the
+key. Groups are not offered to apps in the first release, so the SDK has no group calls
+yet.
 
 ## Run it on minikube
 
```

```powershell
cargo test -p push-sdk --lib --locked secrets
```

Expected: 4 passed, `the_guide_names_every_secret_key` among them.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates docs README.md
git commit -m "docs: keeping an app's secrets: secret stores and the database key"
```

Expected: 946 tests pass, no warnings, no diff.

---

### Task 14: Roadmap and pull request

**Files:**
- Modify: `docs/superpowers/plans/2026-10-11-roadmap.md`

CI needs no change: the `rust` job runs the workspace tests (the SDK's need no Docker), the
`msrv` job builds with 1.85 and compiles SQLite3 Multiple Ciphers with gcc, and the
`images` job builds the services, which do not link the SDK yet (Plan 9B's smoke image
does). The spec already describes the SDK and 4007 (committed with this plan). The roadmap
marks 9A in review.

- [ ] **Step 1: Mark the roadmap**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, set the last column of the 9A row to
`In review`.

- [ ] **Step 2: Final gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A docs
git commit -m "docs: mark plan 9A in review"
```

Expected: 946 tests pass, no warnings, no diff.

- [ ] **Step 3: Push, open the pull request and wait for CI**

```powershell
git push -u origin feature/plan-09a-sdk-core
$body = @'
Implements docs/superpowers/plans/2026-10-15-plan-09a-sdk-core.md: push-sdk, the client core apps embed, and the gateway's new close code 4007 ("opened elsewhere", apart from 4003 "device removed"; owner decision). Sign-up and sign-in through auth-svc (idempotent device registration with a persisted registration_id, forgotten only after the session is saved; refresh with rotation, serialised, saved before use, every attempt within auth-svc's 30 s grace, never cut off), credentials only in an app-provided SecretStore and never logged; the WebSocket connection with Identify, Ready, heartbeats, AuthExpiring -> refresh -> ReAuth, and reconnects with backoff and jitter on everything but 4001 (SignedOut), 4003 (DeviceRemoved) and 4007 (OpenedElsewhere), 4002 refreshing first and 4006 retried without signing out; local data kept on every server sign-out (owner decision); a SQLite store linked to SQLite3 Multiple Ciphers, optionally encrypted as a whole with an app-provided DatabaseKey (Argon2id helper, WrongKey, change of key; owner decision), with conversations, messages, receipts kept at their highest, contacts and an outbox that validates before queuing, keeps client_msg_id across retries and restarts, and sends one row per conversation; one Ack per conversation naming the highest seq, and a delivery the store cannot take ends the connection; one bounded event stream (EventsLost); docs/sdk-secure-storage.md for app developers. No group calls (owner decision). 946 tests.
'@
gh pr create --base main --title "Plan 9A: the SDK core" --body $body
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust`, `msrv`, `images`, `chart`). Do not merge; the
controller merges after the final review.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §4.2 validate, store under a UUIDv7 `client_msg_id`, local echo, one send in flight per conversation, outbox row deleted on `SendAck` | 5, 8, 11 |
| §4.3 store then `Ack` (highest `seq`), replays harmless; a delivery not stored ends the connection | 4, 7, 8, 9 |
| §4.4 pending / sent / delivered / read from receipt positions | 4, 5 |
| §4.5 receipts, `ReadPosition`, listing on connect and after `ContactChanged`; highest positions kept (§7.2); reads capped at the last `seq` | 4, 8 |
| §4.6 edits and deletes applied only if newer, only to held messages; delete erases the body | 4, 8 |
| §5.2–§5.4 sign-up, resend, sign-in, idempotent registration, refresh with rotation within the grace, password reset request, device list and revocation | 3, 10, 12 |
| §5.5 contacts, invites, `OpenDirect`, profiles | 12 |
| §7.3 `Identify` first, `Ready`, heartbeats with a random first ping, `AuthExpiring` → `ReAuth`, backoff with jitter 1–60 s, duplicate `Deliver` ignored | 7, 9 |
| §7.4 close codes (only 4001 signs out, only 4003 and 4007 stop, 4006 retryable); 4007 for a takeover | 1, 7, 9 |
| §7.3 frame budget (100 at once, 20/s): the client stays under 50 and 10/s and acks once per batch | 7, 8, 9 |
| §8.1 one Rust core, no logic left for the bindings | all |
| §8.2 `Client`, cancel before acknowledgement, one bounded event stream | 8, 10, 11, 12 |
| §8.3 validation before queuing | 11 |
| §8.4 outbox: order per conversation, transient retries with backoff, rejections fail and free the next row, edit/delete of unsent and in-flight messages, survives restarts | 5, 8, 11 |
| §8.5 one runtime owned by the client; plain data types; string error codes | 2, 10 |
| §8 local data: kept on server sign-outs, erased without trace on `sign_out` and another user's sign-in; optional whole-database encryption (owner decisions) | 4, 6, 10, 13 |
| §13 groups not exposed to apps | 12 (no group calls) |

## Verified in a worktree

Every task was applied in order to a worktree of `main` at `9f1bf19` and committed; each
commit passed the four gates. Test counts after each task: 874, 881, 891, 897, 903, 907,
912, 924, 934, 939, 942, 945, 946 (71 in `push-sdk` at the end). Every Step 2 mutation (Task 13:
its Step 1) failed exactly the named tests, as recorded above. Before that, the choice of SQLite3 Multiple Ciphers was
tried for real: `rusqlite`'s `bundled-sqlcipher-vendored-openssl` stopped on this machine
with "Command 'perl' not found" (openssl-src), and the Docker builder image lacks Perl's
`IPC::Cmd`; a probe of `push-sdk`'s SQLite (encrypt, wrong key, no key, rekey) passed with
MSVC and in `rust:1.85-slim-bookworm`, there both without and with `libsqlite3-dev` and
`pkg-config` installed. The guide's `keyring` example compiles against `keyring` 4.2. The
plan's blocks, replayed onto a fresh `main`, reproduce the verified tree exactly. Plan 9B
ran the SDK against the whole stack, locally and on minikube.

## Review fixes made after the plan was written

The code was reviewed after the tasks above were built, and these fixes were committed on
the branch; the guide (Task 13) and the docs describe the code as it is now.

- **4007** is the takeover's close code in the gateway and the SDK (Task 1).
- **Refresh:** a refresh runs spawned, so it finishes when its caller goes away; an access
  token near expiry is refreshed before use.
- **Temp store in memory:** SQLite's temporary data never touches a file.
- **`encryption_unavailable`:** a key given to a SQLite that cannot encrypt is refused
  (fatal) instead of being ignored.
- **Atomic versioned migrations:** the schema changes in one transaction under
  `user_version`.
- **Stale-history guard:** an old history page never brings back the body of a message
  that was deleted or edited since.
- **`change_key` safety, the erase's checkpoint check, `Zeroizing` keys:** a failed rekey
  leaves the file usable; an erase that cannot truncate the log is an error; key strings
  are wiped.
- **4002 back-off reset, capped frames per turn, reads never starve:** a long healthy
  connection resets the 4002 back-off; a wake sends a bounded number of frames; reads of
  the socket are served between them.
- **Stop acknowledgement before a wipe or a rebind:** `sign_out` and `sign_in` stop the
  connection first (5 s bound) and wait for it; `sign_out` always wipes, even if the
  keychain delete fails, and returns `connection_busy` without erasing anything if the stop
  is not confirmed.
- **`timeout` and `unexpected_answer` are not retryable;** `connection_busy` is.
- **`Client::close`** waits until the connection has stopped and the database is closed;
  `change_database_key` is async.
- **Vendored sqlite3mc:** the amalgamation lives in `crates/push-sdk/vendor/sqlite3mc`,
  checked against pinned SHA-256s on every build, with an update script that verifies the
  release's signatures (the choice of Owner decision needed 2, taken as option b).

## Deferred to later plans

- **`open_async` for bindings:** `Client::open` is synchronous (it blocks on the file, the
  key and the migrations); bindings call it on a blocking thread until an async variant
  exists.
- **Timed-out pending requests:** a request that timed out is not cleaned up on its own.
- **Cancel on an edit id:** `cancel` is given an edit's id and does not treat it as a
  message's.
- **Unchecked empty or limit inputs:** some calls do not refuse an empty or out-of-range
  input before asking the server.
- **`connect` state is not atomic:** its check of the state and its change are two steps.
- **A `sign_out` whose stop times out skips revocation:** it returns `connection_busy`,
  erasing nothing, and the call is repeated by the app.
- **A `SendAck` the store cannot take stalls the outbox** until the next connection.
- **A malformed delivery is never acked** and comes again on every connection.
- **`apply_deliver` is not one transaction.**
- **Missing tests:** a second 4002 in a row, a `ReAuth` retry, a `Pong` timeout and a
  broken reconnect have no test of their own.
- **Wall-clock connection tests:** they wait real time, so a loaded machine can make them
  flaky.
- **Argon2 parameters for PINs:** the helper's 19 MiB and 2 passes are a floor; a short PIN
  can be brute-forced offline. The guide recommends a passphrase or higher parameters and
  gives an entropy table; the SDK does not enforce a minimum.
- **Old plaintext after adding a key:** encrypting a plain database in use can leave
  plaintext in deleted WAL or journal files; the guide recommends choosing encryption at
  the first open, or wiping and syncing again.
- **`parent_id` is unused.**
- **`edit_unsent` matches a message without its `conversation_id`.**
- **Unread recount** when a read position moves.
- **Tokens are not zeroized** in memory (the database key is).
- **Error code name overlap:** the SDK's own `invalid` shares its name with a server
  refusal code.
- **The vendored sqlite3mc `LICENSE` is unsigned:** it is not covered by the signature
  check.
- **The bindings** (Plan 10): napi-rs and cbindgen over `Client`, `Events`, `SecretStore`
  and `DatabaseKey`, with `catch_unwind`, `*_free` and the callback rules of spec §8.5;
  building SQLite3 Multiple Ciphers for the mobile targets.
- **TLS end to end:** `https://` and `wss://` are compiled in (rustls, ring, webpki roots)
  but first used against a real certificate in Plan 11.
- **Proxies:** the SDK ignores `HTTP_PROXY`/`HTTPS_PROXY`.
- **Profiles cache:** `profiles()` asks auth-svc each time; a local cache of names belongs
  with the UI needs of Plan 10.
- **Listing with `changed_since`:** every connection lists all conversations; fine for the
  first release, to revisit with the load test (Plan 11).
- **Unread counts of a new device** start from the server's listing; messages older than the
  device that are never fetched are not recounted when the read position moves.
- **A refresh answer lost for more than 30 s** cannot be retried safely: auth-svc treats a
  second use after its grace as reuse and signs the device out. One refresh never retries
  past 25 s; a later refresh of the same token (the network was down for longer) takes
  that chance. Inherent to rotation.
- **Groups** (owner decision): the store and the types already carry group kinds and
  group receipts; group calls come when the owner turns groups on.
- **Owner decisions needed 1–2** above (erasing on a revocation, where the SQLite3 Multiple
  Ciphers source comes from).
