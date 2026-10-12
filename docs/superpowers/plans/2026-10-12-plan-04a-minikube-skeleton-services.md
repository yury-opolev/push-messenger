# Plan 4A — Walking Skeleton: the Services Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The first runnable push-messenger: a message typed by one user's device reaches another user's device. New crates: `push-proto` (the client wire protocol and the internal gRPC API, as protobuf), `push-message-svc` (the messaging core behind gRPC, with Postgres and a Redis doorbell), `push-gateway` (client WebSockets in front of message-svc) and `push-smoke` (an end-to-end smoke test of a running deployment). `push-store` gains `PgStore::connect` with session timeouts, lock timeouts as retryable conflicts, `ping` and `close`. Everything is tested on this machine against Postgres and Redis containers; Plan 4B puts it on minikube.

**Architecture:** The gateway accepts WebSocket connections at `/v1/socket` (axum), decodes binary protobuf `Frame`s with a pure per-connection state machine (`Session`), and forwards `Identify`, `Send`, `Sync` and `Ack` to message-svc over tonic gRPC. message-svc wraps `push_domain::Messaging<PgStore, SystemClock, RedisNotifier>`; after each commit the notifier publishes a small `Notification` (never a body) on the Redis channel `notify:{user_id}`. The gateway's `Hub` subscribes to the channels of the users it holds (first session subscribes, last one unsubscribes), rings their sessions, and each session syncs its device and pushes the entries it has not sent yet as `Deliver` frames. Until real tokens arrive (Plans 5 and 8), a DEV ONLY identity — `Identify{dev_user_id, dev_device_id}` accepted only with `PUSH_DEV_AUTH=1` — and a DEV ONLY `DevSetup` gRPC service (`PUSH_DEV_SETUP=1`) let the smoke test create two users who chat. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §2, §4.2–§4.3, §7, §11, §12.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85). New: `prost` 0.14, `tonic` / `tonic-prost` / `tonic-prost-build` 0.14 (0.14.5), `protoc-bin-vendored` 3, `redis` 1 (1.2.x), `axum` 0.8 (`ws`), `tokio-tungstenite` 0.29, `tokio-util` 0.7, `tokio-stream` 0.1, `tracing-subscriber` 0.3 (`json`, `env-filter`), `anyhow` 1, `futures-util` 0.3; for tests `testcontainers-modules` 0.13 with the `redis` feature (Redis 7).

---

## Before you start

- **Plans 3A and 3B are merged** (PRs #4 and #5). This plan builds on `push-domain`,
  `push-testkit` and `push-store` as they are on `main`.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-04a-skeleton-services
```

- The toolchain is rustup-managed (`C:\Users\yurio\.cargo\bin`), with the `1.85` toolchain
  installed for the MSRV check. If `cargo --version` prints something older than 1.85, or
  `cargo clippy` is missing, the shell has a stale PATH. Fix it for the current PowerShell
  session (each new shell needs it again):

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
```

- **Docker must be running** (Docker Desktop; `docker version` must print a server
  version). The tests start `postgres:17-alpine` and, from Task 3 on, `redis:7-alpine`
  containers; the first run pulls them. Each test removes its containers; a run killed
  half-way can leave some behind (`docker ps -a`, then `docker rm -f <id>`).
- **No system `protoc` is needed** and none should be installed for this plan:
  `push-proto`'s build script uses the `protoc` binary that the `protoc-bin-vendored`
  crate ships.
- **minikube is not needed for this plan.** Plan 4B builds the images, the Helm chart and
  the bring-up script, and runs the smoke test on minikube.
- **Edit files with the edit tool, and keep them LF.** The repository stores text with LF
  line endings (`.gitattributes`: `* text=auto eol=lf`). Change existing files with the
  edit tool, which keeps their line endings; never rewrite a file through PowerShell
  `Set-Content` or `Out-File`. The file-creation tool on Windows may write CRLF; git
  normalises it on commit (it prints `CRLF will be replaced by LF`), which is harmless.
- **Do not pipe `cargo fmt`.** Run `cargo fmt --all -- --check` on its own and read the
  exit code; piping it into `Select-String` or `Out-Null` hides the diff and the failure.
  If it reports a diff, run `cargo fmt --all` and check again. The code in this plan is
  already formatted, so a diff usually means a copying mistake.
- Baseline (after Plan 3B): `cargo test --workspace` passes 537 tests (`push-domain` 22
  unit; `push-testkit` `in_memory` 250; `push-store` 7 unit and `postgres` 258).
- **Cargo.lock.** Tasks that add dependencies first run `cargo test -p <crate>` without
  `--locked`, which records the new packages in `Cargo.lock`; the gates then run with
  `--locked`. The workspace declares `rust-version = "1.85"` and resolver 3, so cargo picks
  versions that build on Rust 1.85 (for example `tonic` 0.14.5, not 0.14.6, which needs
  1.88). Never run `cargo update`.
- **Every task ends green.** The last step of each task runs all four gates:

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

  The last one is what the `msrv` CI job needs. Never add `#[allow(dead_code)]` or other
  `allow`s: every item a task adds is used by that task's code or public API.
- **Scope rule:** no changes to `push-domain` or `push-testkit`; no Dockerfiles, Helm or
  minikube (Plan 4B); no JWTs (Plan 5); no frames beyond those listed in Task 2 (Plans 7
  and 8).

## Key rules the code must preserve

- **The gateway is transport only:** no database credentials, no business rules. It
  forwards frames and pushes what message-svc returns.
- **Never acknowledge before commit:** the gateway sends `SendAck` only with what
  message-svc's `Send` returned, and message-svc returns only after `send_text` committed.
- **Never put a message body on Redis:** notifications carry ids and positions only.
- **Postgres is the truth, Redis a doorbell:** a lost notification only delays a client.
  A session syncs right after `Ready`, and every session is rung again whenever the Redis
  subscription is re-established.
- **Delivery positions move only on `Ack`** (the domain guarantees it; the gateway never
  acks on the client's behalf).
- **A device's backlog never stalls:** an ack that moves the position syncs again while
  the last sync left entries behind, and a failed sync is retried with backoff.
- **The DEV ONLY identity is off unless explicitly enabled:** `PUSH_DEV_AUTH` and
  `PUSH_DEV_SETUP` accept exactly `1` (on) or `0`/empty/unset (off) and refuse anything
  else; without dev auth, `Identify` closes the connection with 4001.

## Decisions made for this plan

These were settled while writing the plan. Do not reopen them during execution.

1. **Plan 4 is split.** 4A (this plan) delivers the runnable services, tested locally
   against containers. 4B adds the Dockerfile, the Helm chart, ingress-nginx, the
   minikube bring-up scripts and runs the smoke test on minikube. Each is its own pull
   request.
2. **Dependencies, chosen for Rust 1.85** (resolver 3 picks these; the `+1.85` gate checks
   it): `prost` 0.14 (0.14.4), `tonic`, `tonic-prost` and `tonic-prost-build` 0.14 (0.14.5;
   0.14.6 needs 1.88), `protoc-bin-vendored` 3 (3.3), `redis` 1 with only
   `connection-manager` and `tokio-comp` (1.2.x; 1.3 and later need 1.88), `axum` 0.8
   (0.8.9, MSRV 1.80) with `ws` for the gateway, `tokio-tungstenite` 0.29 (the version axum
   0.8.9 uses, so only one is built), `tokio-util` 0.7 with `rt` (`CancellationToken`,
   `TaskTracker`), `tokio-stream` 0.1 with `net`, `tracing-subscriber` 0.3 with `json` and
   `env-filter`, `anyhow` 1 (binaries and the smoke test only), `futures-util` 0.3 with
   only `sink`. Tests: `testcontainers-modules` 0.13 gains the `redis` feature.
3. **No system protoc.** `push-proto/build.rs` hands `protoc_bin_vendored::protoc_bin_path()`
   to `prost_build::Config::protoc_executable` (re-exported as
   `tonic_prost_build::Config`), so the build never reads `PROTOC` or `PATH`, and needs no
   `unsafe` `set_var`.
4. **Wire encoding** (`push/v1/frame.proto`): ids are 16-byte UUIDs; times are
   microseconds since the Unix epoch (what Postgres stores); client frames take `Frame`
   field numbers 1–31 and server frames 32–63. This plan defines `Identify`, `Send`, `Ack`,
   `Ping`, `Ready`, `SendAck`, `Deliver`, `Error` and `Pong`. Reserved field numbers:
   `ephemeral` 16, `urgent` 17 and media `attachments` 18 on `Send` and `Deliver`;
   `private_reads` 16 on `Identify`. **`Deliver` carries `target_seq`** (as `Entry`
   stores it), not the `target_message_id` the spec listed; Task 10 corrects the spec.
   `Identify` has the spec's `token` (field 1, unused until Plan 8) and the DEV ONLY
   `dev_user_id` / `dev_device_id` (fields 14 and 15).
5. **Internal API** (`push/internal/v1/messaging.proto`): service `Messaging` with
   `Identify`, `Send`, `Sync`, `Ack`, reusing the wire messages (`Send`, `SendAck`,
   `Deliver`, `Ack`) so the gateway forwards without converting; DEV ONLY service
   `DevSetup` with `RegisterUser`, `RegisterDevice`, `MakeContacts` (a request accepted by
   the peer) and `OpenDirect`. Failures are gRPC statuses whose `details` hold an encoded
   `push.v1.Error`; `push_proto::error_status` / `error_from_status` are the two ends, and
   a status without details (message-svc unreachable) becomes a retryable `UNAVAILABLE`.
   The gateway adds the `Send`'s `client_msg_id` to the error it forwards. Storage errors
   reach the client as "a temporary storage problem; retry", never with database text.
6. **Notifications.** After each commit message-svc publishes, per user an event
   concerns, a protobuf `Notification` (entries available, receipt, contact change, read
   position) on `notify:{user_id}`, pipelined in one round trip, best effort (failures
   are logged). The gateway acts only on `EntriesAvailable` in this plan; receipts,
   contact changes and read positions reach clients with Plans 7–8.
7. **message-svc** is a library plus a thin `main.rs`: `serve(config, grpc_listener,
   health_listener, shutdown)` so tests run it in-process on free ports. The gRPC services
   are generic over `Store`, `Clock` and `Notifier`, so unit tests use `InMemoryStore`.
   It serves `/healthz` (always 200) and `/readyz` (200 once the database is migrated and
   answers `SELECT 1`; 503 before, and after shutdown begins) on a separate port (8081),
   retries Postgres and Redis every 2 s until they answer (pods may start before them),
   runs the migrations on start on a connection of its own (`push_store::migrate_database`,
   without the pool's request-sized statement and lock timeouts; it connects as the owner
   of `push_messaging`), stops on
   SIGTERM or Ctrl-C (finishing calls in progress, then closing the pool), and logs JSON
   through `tracing-subscriber` (`RUST_LOG`, default `info`). Configuration is
   environment variables (`Config::from_env`; the table is in `config.rs`).
8. **The gateway uses axum**, not bare tokio-tungstenite: tonic already depends on axum
   and hyper, so it adds almost nothing, and it gives routing, the WebSocket upgrade with
   message-size limits (`MAX_FRAME_BYTES` = 128 KiB), and the health server in one crate.
   One task per connection runs a `select!` loop over the socket, the session's doorbell
   and shutdown, so a device's syncs are serialised. `Session` (pure, unit-tested) decides
   what each frame means: `Identify` first within 5 s or close 4001; bad frames after
   `Ready` get an `Error` frame and keep the connection; `Ping` gets `Pong`. A session
   remembers the highest `seq` it sent per conversation, so a sync that repeats unacked
   entries sends nothing twice. A backlog larger than one sync batch (500) keeps flowing:
   the session remembers that the last sync reported `more`, and an `Ack` that moved the
   position syncs again (an ack rings nobody, so nothing else would). A failed sync is
   retried from the connection's loop after 1 s, doubling to 30 s, reset by a successful
   sync (`RetryDelay`), so a session that connects while message-svc restarts still gets
   its entries. On shutdown the HTTP server stops accepting, every session closes with
   4005, and `serve` waits up to 10 s for them (`TaskTracker`). `/readyz` is 200 until
   shutdown and does **not** depend on Redis: without Redis devices still get their
   entries (after `Ready`, after acks, on retries), only later, while unready gateways
   would refuse every client; the subscriber reconnects by itself, so liveness does not
   depend on it either.
9. **DEV ONLY identity safety (binaries).** `PUSH_DEV_AUTH` / `PUSH_DEV_SETUP` are parsed
   strictly: `1` on; unset, empty or `0` off; anything else (`true`, `yes`, ` 1`) is a
   start-up error. When on, each service logs a `WARN` at start-up saying that anyone who
   can reach it can act as any user. Without `PUSH_DEV_SETUP=1` the `DevSetup` service is
   not registered (calls get `UNIMPLEMENTED`). Plan 4B adds the chart's guards.
10. **push-smoke** is a library (`run(&Target)`) and a binary (`--setup`, `--socket`,
    `--host`, or `PUSH_SMOKE_*` variables). It creates Alice and Bob as contacts with a
    direct chat through `DevSetup`, connects Bob and then Alice, sends from Alice, waits
    for her `SendAck` and Bob's `Deliver`, acks from Bob and then pings: frames are
    handled in order, so a `Pong` (not an `Error`) proves the ack succeeded. `--host` sets
    the `Host` header so it can go through an ingress controller by IP (Plan 4B). It
    retries connecting for up to 60 s and gives each step 10 s.
11. **CI** needs no new job in this plan: both jobs also pull `redis:7-alpine` first.

## File structure

```
Cargo.toml                         + 4 members; new workspace dependencies
README.md                          Docker images, no protoc needed
.github/workflows/ci.yml           pull redis:7-alpine
crates/push-store/
├── Cargo.toml                     tokio "macros" for tests
├── src/lib.rs                     re-export StoreOptions; docs
├── src/options.rs                 NEW: StoreOptions, session SET statements
├── src/store.rs                   connect, ping, pool, close
├── src/error.rs                   55P03 is retryable
└── tests/connect.rs               NEW: timeouts, lock timeout, migrations twice, ping/close
crates/push-proto/                 NEW
├── Cargo.toml
├── build.rs                       vendored protoc
├── proto/push/v1/frame.proto      the client wire protocol
├── proto/push/internal/v1/messaging.proto   the internal gRPC API and Notification
└── src/lib.rs, src/codec.rs       frames, ids, statuses, channel names, close codes
crates/push-message-svc/           NEW
├── Cargo.toml
├── src/lib.rs
├── src/notify.rs                  RedisNotifier
├── src/convert.rs                 domain <-> protobuf, errors -> statuses
├── src/grpc.rs                    MessagingApi, DevSetupApi
├── src/config.rs                  Config from environment variables
├── src/server.rs                  serve, health, logging, signals
├── src/main.rs
└── tests/redis.rs, tests/service.rs
crates/push-gateway/               NEW
├── Cargo.toml
├── src/lib.rs
├── src/config.rs                  Config from environment variables
├── src/session.rs                 the per-connection state machine; sync retry delays
├── src/hub.rs                     sessions by user; the Redis subscriber
├── src/server.rs                  serve, connections, health, logging, signals
├── src/main.rs
└── tests/hub.rs, tests/gateway.rs
crates/push-smoke/                 NEW
├── Cargo.toml
├── src/lib.rs                     Target, run
├── src/main.rs
└── tests/local.rs                 the whole stack on containers
docs/superpowers/specs/2026-10-11-push-messenger-design.md   §7.2 target_seq, §7.6, §10
docs/superpowers/plans/2026-10-11-roadmap.md                 rows 4A and 4B
```

---

### Task 1: `PgStore::connect` with session timeouts, ping and close

**Files:**
- Create: `crates/push-store/src/options.rs`, `crates/push-store/tests/connect.rs`
- Modify: `crates/push-store/Cargo.toml`, `src/lib.rs`, `src/store.rs`, `src/error.rs`

- [ ] **Step 1: Let the store's tests use `#[tokio::test]`**

In `crates/push-store/Cargo.toml`, replace the `tokio` line under `[dev-dependencies]`
with:

```toml
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 2: Write the failing tests**

Create `crates/push-store/tests/connect.rs` (a normal test harness, so it needs no
`[[test]]` entry):

```rust
//! `PgStore::connect` and the operations a service needs around the store: session
//! timeouts, lock timeouts as conflicts, idempotent migrations, ping and close.
//!
//! Each test starts a Postgres container of its own (Docker must be running); the tests
//! run in parallel, so the file takes about as long as one container start.

use std::time::Duration;

use push_domain::{DomainError, Store, Tx, UserId};
use push_store::{MIGRATOR, PgStore, StoreOptions, migrate_database};
use sqlx::{Connection, PgConnection};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

/// A running Postgres and the URL of its `postgres` database.
async fn postgres() -> (ContainerAsync<Postgres>, String) {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("the Postgres port");
    // Docker publishes ports on IPv4; "localhost" would try IPv6 first on every connect.
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    (container, url)
}

#[tokio::test]
async fn every_connection_gets_the_session_timeouts() {
    let (_container, url) = postgres().await;
    let options = StoreOptions {
        lock_timeout: Duration::from_millis(250),
        statement_timeout: Duration::from_secs(7),
        idle_in_transaction_timeout: Duration::from_secs(9),
        ..StoreOptions::default()
    };
    let store = PgStore::connect(&url, &options).await.expect("connect");
    let settings: (String, String, String) = sqlx::query_as(
        "SELECT current_setting('lock_timeout'), current_setting('statement_timeout'), \
         current_setting('idle_in_transaction_session_timeout')",
    )
    .fetch_one(store.pool())
    .await
    .expect("read the settings");
    assert_eq!(
        settings,
        ("250ms".to_owned(), "7s".to_owned(), "9s".to_owned())
    );
}

#[tokio::test]
async fn a_lock_timeout_is_a_retryable_conflict() {
    let (_container, url) = postgres().await;
    let options = StoreOptions {
        lock_timeout: Duration::from_millis(200),
        ..StoreOptions::default()
    };
    let store = PgStore::connect(&url, &options).await.expect("connect");
    store.migrate().await.expect("migrate");

    // Another session inserts the same user and keeps its transaction open, so the
    // store's insert has to wait for that row lock until `lock_timeout` ends it.
    let user = UserId::new();
    let mut holder = PgConnection::connect(&url).await.expect("connect");
    let mut held = holder.begin().await.expect("begin");
    sqlx::query("INSERT INTO messaging.known_user (user_id) VALUES ($1)")
        .bind(user.0)
        .execute(&mut *held)
        .await
        .expect("insert the user");

    let mut tx = store.begin().await.expect("begin");
    let error = tx
        .insert_user(user)
        .await
        .expect_err("the lock wait must time out");
    assert_eq!(error, DomainError::Conflict);
    assert!(error.is_retryable());
    held.rollback().await.expect("roll back");
}

#[tokio::test]
async fn running_the_migrations_again_changes_nothing() {
    let (_container, url) = postgres().await;
    migrate_database(&url).await.expect("first run");
    migrate_database(&url).await.expect("second run");
    let store = PgStore::connect(&url, &StoreOptions::default())
        .await
        .expect("connect");
    store.migrate().await.expect("third run, through the pool");
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(store.pool())
        .await
        .expect("count the applied migrations");
    assert_eq!(applied, MIGRATOR.iter().count() as i64);
}

#[tokio::test]
async fn ping_answers_until_the_store_is_closed() {
    let (_container, url) = postgres().await;
    let store = PgStore::connect(&url, &StoreOptions::default())
        .await
        .expect("connect");
    store.ping().await.expect("ping an open store");
    store.close().await;
    assert!(
        store.ping().await.is_err(),
        "a closed store must not answer"
    );
}
```

- [ ] **Step 3: Run them to see them fail**

```powershell
cargo test -p push-store --test connect
```

Expected: compile errors — `unresolved import push_store::StoreOptions`, and no function
`connect`, `pool`, `ping` or `close` on `PgStore`.

- [ ] **Step 4: Add `StoreOptions`**

Create `crates/push-store/src/options.rs`:

```rust
//! Connection settings for a service's pool.

use std::time::Duration;

/// How [`PgStore::connect`](crate::PgStore::connect) builds its pool, and the timeouts
/// every connection gets for its whole life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreOptions {
    /// Most connections the pool opens.
    pub max_connections: u32,
    /// How long a caller waits for a free connection before the call fails.
    pub acquire_timeout: Duration,
    /// How long a statement waits for a lock (`lock_timeout`). A statement that waits
    /// longer fails with SQLSTATE 55P03, reported as the retryable `Conflict`.
    pub lock_timeout: Duration,
    /// How long one statement may run (`statement_timeout`).
    pub statement_timeout: Duration,
    /// How long a transaction may sit idle before the server ends the session
    /// (`idle_in_transaction_session_timeout`), so a stuck caller cannot hold locks.
    pub idle_in_transaction_timeout: Duration,
}

impl Default for StoreOptions {
    /// 10 connections, 5 s to acquire one, 2 s lock timeout, 10 s statement timeout,
    /// 30 s idle-in-transaction timeout.
    fn default() -> Self {
        Self {
            max_connections: 10,
            acquire_timeout: Duration::from_secs(5),
            lock_timeout: Duration::from_secs(2),
            statement_timeout: Duration::from_secs(10),
            idle_in_transaction_timeout: Duration::from_secs(30),
        }
    }
}

impl StoreOptions {
    /// The `SET` statements run on every new connection. Values are whole milliseconds.
    pub(crate) fn session_settings(&self) -> String {
        format!(
            "SET lock_timeout = {}; SET statement_timeout = {}; \
             SET idle_in_transaction_session_timeout = {}",
            self.lock_timeout.as_millis(),
            self.statement_timeout.as_millis(),
            self.idle_in_transaction_timeout.as_millis(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_settings_set_every_timeout_in_milliseconds() {
        let options = StoreOptions {
            lock_timeout: Duration::from_millis(250),
            ..StoreOptions::default()
        };
        assert_eq!(
            options.session_settings(),
            "SET lock_timeout = 250; SET statement_timeout = 10000; \
             SET idle_in_transaction_session_timeout = 30000"
        );
    }
}
```

- [ ] **Step 5: Connect, ping, close**

In `crates/push-store/src/store.rs`, replace the imports and the whole
`impl PgStore { … }` block (from `use sqlx::postgres::{PgPool, PgRow};` down to the closing
brace of `impl PgStore`) with:

```rust
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::{Executor, PgConnection, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::codes::{decode, encode};
use crate::error::store_error;
use crate::locks;
use crate::options::StoreOptions;
use crate::rows::{self, conversation_columns, entry_columns, member_columns};

/// The push-domain [`Store`] on PostgreSQL.
#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// A store over `pool`, whose database holds the migrated `messaging` schema (see
    /// [`PgStore::migrate`]).
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Connects a pool to `url` as `options` say. Every connection the pool opens first
    /// sets the lock, statement and idle-in-transaction timeouts. Fails if the first
    /// connection cannot be made.
    pub async fn connect(url: &str, options: &StoreOptions) -> Result<Self, sqlx::Error> {
        let settings = options.session_settings();
        let pool = PgPoolOptions::new()
            .max_connections(options.max_connections)
            .acquire_timeout(options.acquire_timeout)
            .after_connect(move |connection, _| {
                let settings = settings.clone();
                Box::pin(async move {
                    connection.execute(settings.as_str()).await?;
                    Ok(())
                })
            })
            .connect(url)
            .await?;
        Ok(Self::new(pool))
    }

    /// Applies the migrations the database does not have yet. Run it as the owner of the
    /// database: since PostgreSQL 15 other users cannot create objects in it.
    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        crate::MIGRATOR.run(&self.pool).await
    }

    /// Checks that the database answers, for readiness probes.
    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// The pool the store runs on.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Closes every connection and waits until they are closed. Later calls fail.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
```

The `SET` statements run with the simple query protocol (`execute` on a `&str`), which
allows several statements in one round trip.

- [ ] **Step 6: Report a lock timeout as a retryable conflict**

In `crates/push-store/src/error.rs`, replace

```rust
/// SQLSTATE codes a caller may retry: `serialization_failure` and `deadlock_detected`.
const RETRYABLE: [&str; 2] = ["40001", "40P01"];
```

with

```rust
/// SQLSTATE codes a caller may retry: `serialization_failure`, `deadlock_detected` and
/// `lock_not_available` (a `lock_timeout` expired).
const RETRYABLE: [&str; 3] = ["40001", "40P01", "55P03"];
```

and in its tests replace the first lines of the first test,

```rust
    fn only_serialization_failures_and_deadlocks_are_retryable() {
        assert!(is_retryable(Some("40001")));
        assert!(is_retryable(Some("40P01")));
```

with

```rust
    fn only_serialization_failures_deadlocks_and_lock_timeouts_are_retryable() {
        assert!(is_retryable(Some("40001")));
        assert!(is_retryable(Some("40P01")));
        assert!(is_retryable(Some("55P03")));
        assert!(
            !is_retryable(Some("57014")),
            "query_canceled: a statement timeout"
        );
```

A statement timeout (`57014`) stays a `Store` error: retrying the same slow statement
would only time out again.

- [ ] **Step 7: Export the options, migrate on a connection of its own, document**

Replace `crates/push-store/src/lib.rs` with (`migrate_database` uses a one-connection
pool without the session settings; a borrowed `PgConnection` would make the future
unusable in `tokio::spawn`, which message-svc's tests need):

```rust
//! PostgreSQL storage for push-messenger: the migrations of the `messaging` schema
//! (spec §3.2) and the push-domain [`Store`](push_domain::Store) on sqlx.
//!
//! Every query is checked when it runs (`sqlx::query`, not the `query!` macros), so
//! building needs no database. Transactions run at READ COMMITTED; they are serialised
//! where the domain needs it by row locks (`next_seq`, `lock_invite`) and by
//! transaction-scoped advisory locks (contact pairs, conversation positions).
//!
//! Errors: serialization failures, deadlocks and lock timeouts become the retryable
//! `DomainError::Conflict`; every other database error, constraint violations included,
//! becomes `DomainError::Store`. Both are logged with `tracing` first.
//!
//! A service connects with [`PgStore::connect`], which sets the lock, statement and
//! idle-in-transaction timeouts of [`StoreOptions`] on every connection. Each service has
//! a database of its own (message-svc: `push_messaging`), so sqlx's migrations table
//! never mixes two services' migrations.

mod codes;
mod error;
mod locks;
mod options;
mod rows;
mod store;

pub use options::StoreOptions;
pub use store::{PgStore, PgTx};

/// The migrations of the `messaging` schema, embedded at build time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Applies the migrations the database at `url` does not have yet, on a connection of its
/// own: a migration must not inherit the pool's statement and lock timeouts, which are
/// sized for requests. Run it as the owner of the database.
pub async fn migrate_database(url: &str) -> Result<(), sqlx::migrate::MigrateError> {
    // A one-connection pool without the session settings of `PgStore::connect`.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await?;
    let result = MIGRATOR.run(&pool).await;
    pool.close().await;
    result
}
```

- [ ] **Step 8: Run the store's tests**

```powershell
cargo test -p push-store
```

Expected: `push-store` unit tests 8 passed (the new `session_settings_set_every_timeout_in_milliseconds`), `connect` 4 passed, `postgres` 258 passed.

- [ ] **Step 9: Run the gates**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 542 tests pass (537 + 1 unit + 4 `connect`); no warnings; no diff.

- [ ] **Step 10: Commit**

```powershell
git add crates/push-store
git commit -m "feat(push-store): connect with session timeouts, ping and close"
```

---

### Task 2: `push-proto` — the wire protocol and the internal API

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/push-proto/Cargo.toml`, `build.rs`, `proto/push/v1/frame.proto`,
  `proto/push/internal/v1/messaging.proto`, `src/lib.rs`, `src/codec.rs`

- [ ] **Step 1: Add the crate and its dependencies**

```powershell
New-Item -ItemType Directory crates\push-proto\src, crates\push-proto\proto\push\v1, crates\push-proto\proto\push\internal\v1 -Force
```

Replace `Cargo.toml` with:

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-domain",
    "crates/push-proto",
    "crates/push-store",
    "crates/push-testkit",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "MIT"
repository = "https://github.com/yury-opolev/push-messenger"

[workspace.dependencies]
async-trait = "0.1"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
libtest-mimic = "0.8"
prost = "0.14"
protoc-bin-vendored = "3"
push-domain = { path = "crates/push-domain" }
push-proto = { path = "crates/push-proto" }
push-testkit = { path = "crates/push-testkit" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sqlx = { version = "0.8", default-features = false, features = [
    "chrono",
    "macros",
    "migrate",
    "postgres",
    "runtime-tokio",
    "uuid",
] }
testcontainers-modules = { version = "0.13", features = ["postgres"] }
thiserror = "2"
tokio = { version = "1", features = ["sync"] }
tonic = "0.14"
tonic-prost = "0.14"
tonic-prost-build = "0.14"
tracing = "0.1"
uuid = { version = "1", features = ["serde", "v4", "v7"] }

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

Create `crates/push-proto/Cargo.toml`:

```toml
[package]
name = "push-proto"
description = "The push-messenger wire protocol and the internal gRPC API, as protobuf."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
prost.workspace = true
thiserror.workspace = true
tonic.workspace = true
tonic-prost.workspace = true
uuid.workspace = true

[build-dependencies]
protoc-bin-vendored.workspace = true
tonic-prost-build.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Write the client wire protocol**

Create `crates/push-proto/proto/push/v1/frame.proto`:

```protobuf
// The client wire protocol (spec §7). Every WebSocket binary message is one `Frame`.
//
// Conventions:
// - Identifiers (users, devices, conversations, messages, client message ids) are UUIDs
//   as 16 bytes, big-endian, as `uuid::Uuid::as_bytes` gives them.
// - Times are microseconds since the Unix epoch, UTC: the precision PostgreSQL stores.
// - `seq` is a signed 64-bit integer, per conversation, starting at 1.
//
// Field numbers set aside for features the spec reserves (§7) are `reserved` below:
// ephemeral and urgent messages, media attachments, and private reads. A later plan
// removes the `reserved` line when it adds the field, keeping the number.

syntax = "proto3";

package push.v1;

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
    Deliver deliver = 34;
    Error error = 35;
    Pong pong = 36;
  }
  // 5-31: the other client frames of spec §7.1 (ReAuth, Edit, Delete, ReadUpTo and the
  // request frames); 37-63: the other server frames of §7.2. Assigned as they are added.
}

// Must be the first frame, within 5 seconds of connecting.
message Identify {
  // The access token (a JWT, Plan 5). Not checked yet.
  string token = 1;
  // DEV ONLY: accepted only by a gateway started with PUSH_DEV_AUTH=1, which trusts
  // these ids without any proof. Removed when real tokens arrive (Plan 8).
  bytes dev_user_id = 14;
  bytes dev_device_id = 15;
  // private_reads: the device does not reveal reads to others.
  reserved 16;
  reserved "private_reads";
}

message Ready {
  bytes user_id = 1;
  bytes device_id = 2;
  // How often the client should send Ping.
  uint32 heartbeat_ms = 3;
}

enum ContentType {
  CONTENT_TYPE_UNSPECIFIED = 0;
  CONTENT_TYPE_TEXT = 1;
  CONTENT_TYPE_SYSTEM_EVENT = 2;
}

enum EntryKind {
  ENTRY_KIND_UNSPECIFIED = 0;
  ENTRY_KIND_MESSAGE = 1;
  ENTRY_KIND_EDIT = 2;
  ENTRY_KIND_DELETE = 3;
  ENTRY_KIND_SYSTEM = 4;
}

message Send {
  // UUIDv7 chosen by the client before the first attempt, reused on every retry.
  bytes client_msg_id = 1;
  bytes conversation_id = 2;
  ContentType content_type = 3;
  bytes body = 4;
  optional int64 client_ts = 5;
  // 16 ephemeral, 17 urgent, 18 attachments (media).
  reserved 16, 17, 18;
  reserved "ephemeral", "urgent", "attachments";
}

// The entry is durably stored. Also the answer to Edit and Delete (later plans).
message SendAck {
  bytes client_msg_id = 1;
  bytes message_id = 2;
  int64 seq = 3;
  int64 server_ts = 4;
}

// One log entry, in its current state.
message Deliver {
  bytes conversation_id = 1;
  int64 seq = 2;
  bytes message_id = 3;
  bytes client_msg_id = 4;
  EntryKind kind = 5;
  // Edit and delete entries: the seq of the message they change (as the log stores it).
  optional int64 target_seq = 6;
  bytes sender_id = 7;
  ContentType content_type = 8;
  // Absent once the message is deleted, and on edit and delete entries superseded later.
  optional bytes body = 9;
  optional int64 client_ts = 10;
  int64 server_ts = 11;
  optional int64 edited_at = 12;
  optional int64 edit_seq = 13;
  bool deleted = 14;
  // 16 ephemeral, 17 urgent, 18 attachments (media).
  reserved 16, 17, 18;
  reserved "ephemeral", "urgent", "attachments";
}

// "I have everything up to seq in this conversation."
message Ack {
  bytes conversation_id = 1;
  int64 seq = 2;
}

message Ping {}

message Pong {}

enum ErrorCode {
  ERROR_CODE_UNSPECIFIED = 0;
  // The frame could not be decoded, or is not allowed now.
  ERROR_CODE_BAD_FRAME = 1;
  // A field is invalid: an id that is not 16 bytes, an empty or too large body.
  ERROR_CODE_INVALID = 2;
  ERROR_CODE_UNKNOWN_DEVICE = 3;
  ERROR_CODE_CONVERSATION_NOT_FOUND = 4;
  ERROR_CODE_NOT_MEMBER = 5;
  ERROR_CODE_NOT_CONTACT = 6;
  ERROR_CODE_CLIENT_ID_CONFLICT = 7;
  ERROR_CODE_SEQ_OUT_OF_RANGE = 8;
  // A concurrent change or a server problem; retry with backoff.
  ERROR_CODE_UNAVAILABLE = 9;
  // Any other rejection; `message` says which.
  ERROR_CODE_REJECTED = 10;
}

message Error {
  ErrorCode code = 1;
  string message = 2;
  // Whether the same frame may succeed if sent again unchanged.
  bool retryable = 3;
  // Set when the error answers a Send.
  optional bytes client_msg_id = 4;
}
```

- [ ] **Step 3: Write the internal API**

Create `crates/push-proto/proto/push/internal/v1/messaging.proto`:

```protobuf
// The internal API of message-svc, called by the gateway (spec §2). Never exposed outside
// the cluster. Callers pass the user and device the gateway verified; message-svc trusts
// them.
//
// Errors are gRPC statuses whose `details` hold an encoded `push.v1.Error`, which the
// gateway forwards to the client.

syntax = "proto3";

package push.internal.v1;

import "push/v1/frame.proto";

service Messaging {
  // Confirms that the device is active and belongs to the user.
  rpc Identify(IdentifyRequest) returns (IdentifyResponse);
  // Appends a text message; answers after commit.
  rpc Send(SendRequest) returns (push.v1.SendAck);
  // Entries after the device's delivery positions, oldest first per conversation.
  rpc Sync(SyncRequest) returns (SyncResponse);
  // Records that the device holds everything up to a seq.
  rpc Ack(AckRequest) returns (AckResponse);
}

// DEV ONLY: setup calls for local clusters and the smoke test, registered only by a
// message-svc started with PUSH_DEV_SETUP=1. Plans 5-7 replace them with auth-svc events
// and the real contact and conversation requests.
service DevSetup {
  rpc RegisterUser(RegisterUserRequest) returns (RegisterUserResponse);
  rpc RegisterDevice(RegisterDeviceRequest) returns (RegisterDeviceResponse);
  // Makes two users accepted contacts: a request from `user_id`, accepted by `peer_id`.
  rpc MakeContacts(MakeContactsRequest) returns (MakeContactsResponse);
  rpc OpenDirect(OpenDirectRequest) returns (OpenDirectResponse);
}

message IdentifyRequest {
  bytes user_id = 1;
  bytes device_id = 2;
}

message IdentifyResponse {}

message SendRequest {
  bytes user_id = 1;
  bytes device_id = 2;
  push.v1.Send send = 3;
}

message SyncRequest {
  bytes device_id = 1;
  // At most this many entries; 0 means the server's maximum.
  uint32 limit = 2;
}

message SyncResponse {
  repeated push.v1.Deliver entries = 1;
  // More entries are waiting beyond this batch.
  bool more = 2;
}

message AckRequest {
  bytes device_id = 1;
  push.v1.Ack ack = 2;
}

message AckResponse {
  // Whether the device's position moved.
  bool moved = 1;
}

// Published on the Redis channel `notify:{user_id}` after a commit (spec §4.3). Never
// carries message bodies: a lost notification only delays the client.
message Notification {
  oneof kind {
    // New log entries exist for the user; their devices should sync.
    EntriesAvailable entries_available = 1;
    ReceiptChanged receipt_changed = 2;
    ContactChanged contact_changed = 3;
    ReadPositionChanged read_position_changed = 4;
  }
}

message EntriesAvailable {}

message ReceiptChanged {
  bytes conversation_id = 1;
  oneof positions {
    DirectReceipt direct = 2;
    GroupReceipt group = 3;
  }
}

message DirectReceipt {
  bytes user_id = 1;
  int64 delivered_seq = 2;
  int64 read_seq = 3;
}

message GroupReceipt {
  int64 all_delivered_seq = 1;
  int64 all_read_seq = 2;
}

enum ContactState {
  CONTACT_STATE_NONE = 0;
  CONTACT_STATE_REQUESTED_OUT = 1;
  CONTACT_STATE_REQUESTED_IN = 2;
  CONTACT_STATE_ACCEPTED = 3;
  CONTACT_STATE_BLOCKED = 4;
}

message ContactChanged {
  bytes peer_id = 1;
  ContactState state = 2;
}

message ReadPositionChanged {
  bytes conversation_id = 1;
  int64 read_seq = 2;
}

message RegisterUserRequest {
  bytes user_id = 1;
}

message RegisterUserResponse {}

message RegisterDeviceRequest {
  bytes user_id = 1;
  bytes device_id = 2;
}

message RegisterDeviceResponse {}

message MakeContactsRequest {
  bytes user_id = 1;
  bytes peer_id = 2;
}

message MakeContactsResponse {}

message OpenDirectRequest {
  bytes user_id = 1;
  bytes peer_id = 2;
}

message OpenDirectResponse {
  bytes conversation_id = 1;
}
```

- [ ] **Step 4: Generate the code with the vendored protoc**

Create `crates/push-proto/build.rs`:

```rust
//! Generates the protobuf types and the gRPC code with the protoc that
//! `protoc-bin-vendored` ships, so building needs no system protoc.

use std::io;

fn main() -> io::Result<()> {
    let protoc = protoc_bin_vendored::protoc_bin_path().map_err(io::Error::other)?;
    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(protoc);
    tonic_prost_build::configure().compile_with_config(
        config,
        &[
            "proto/push/v1/frame.proto",
            "proto/push/internal/v1/messaging.proto",
        ],
        &["proto"],
    )
}
```

- [ ] **Step 5: The crate root**

Create `crates/push-proto/src/lib.rs`:

```rust
//! The push-messenger protocols as protobuf (spec §7): the client wire protocol
//! ([`v1`], one [`v1::Frame`] per WebSocket binary message) and the internal gRPC API of
//! message-svc ([`internal::v1`]). The `.proto` files under `proto/` are the contract; the
//! Rust code is generated at build time with a vendored `protoc`.
//!
//! Identifiers travel as 16-byte UUIDs ([`id`], [`id_bytes`]); times as microseconds since
//! the Unix epoch.

pub mod v1 {
    //! The client wire protocol, package `push.v1`.
    tonic::include_proto!("push.v1");
}

pub mod internal {
    //! The internal API between the gateway and message-svc.
    pub mod v1 {
        //! Package `push.internal.v1`.
        tonic::include_proto!("push.internal.v1");
    }
}

mod codec;

pub use codec::{
    FrameError, InvalidId, MAX_FRAME_BYTES, decode_frame, encode_frame, error_from_status,
    error_status, id, id_bytes, notify_channel,
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
    pub const RATE_LIMITED: u16 = 4004;
    /// The server is shutting down: back off with jitter, then reconnect.
    pub const SHUTTING_DOWN: u16 = 4005;
}
```

- [ ] **Step 6: Frames, ids, statuses and channel names, with their tests**

Create `crates/push-proto/src/codec.rs`:

```rust
//! Encoding frames, identifiers and errors.

use prost::Message;
use uuid::Uuid;

use crate::v1::{Error, ErrorCode, Frame, frame::Body};

/// Largest WebSocket message a gateway accepts: a text body of the maximum size
/// (64 KiB, `push_domain::MAX_TEXT_BYTES`) with room for the other fields.
pub const MAX_FRAME_BYTES: usize = 128 * 1024;

/// Why a WebSocket message is not a usable frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("a frame is at most {limit} bytes, not {size}")]
    TooLarge { size: usize, limit: usize },
    #[error("not a protobuf frame: {0}")]
    Malformed(String),
    #[error("the frame has no body")]
    Empty,
}

/// An identifier field that is not a 16-byte UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("an id must be 16 bytes, not {0}")]
pub struct InvalidId(pub usize);

/// One frame, ready to send as a WebSocket binary message.
pub fn encode_frame(body: Body) -> Vec<u8> {
    Frame { body: Some(body) }.encode_to_vec()
}

/// The body of a received WebSocket binary message.
pub fn decode_frame(bytes: &[u8]) -> Result<Body, FrameError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            size: bytes.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    let frame = Frame::decode(bytes).map_err(|error| FrameError::Malformed(error.to_string()))?;
    frame.body.ok_or(FrameError::Empty)
}

/// Reads an identifier field.
pub fn id(bytes: &[u8]) -> Result<Uuid, InvalidId> {
    Uuid::from_slice(bytes).map_err(|_| InvalidId(bytes.len()))
}

/// Writes an identifier field.
pub fn id_bytes(id: Uuid) -> Vec<u8> {
    id.as_bytes().to_vec()
}

/// The Redis channel message-svc publishes a user's
/// [`Notification`](crate::internal::v1::Notification)s on, and the gateways holding the
/// user's devices subscribe to: `notify:{user_id}`.
pub fn notify_channel(user: Uuid) -> String {
    format!("notify:{user}")
}

/// A gRPC status carrying `error` for the client, as message-svc answers.
pub fn error_status(code: tonic::Code, error: Error) -> tonic::Status {
    let message = error.message.clone();
    tonic::Status::with_details(code, message, error.encode_to_vec().into())
}

/// The client error a gRPC status carries. A status without one — message-svc is
/// unreachable, or failed before answering — becomes a retryable `UNAVAILABLE`.
pub fn error_from_status(status: &tonic::Status) -> Error {
    match Error::decode(status.details()) {
        Ok(error) if error.code() != ErrorCode::Unspecified => error,
        _ => Error {
            code: ErrorCode::Unavailable.into(),
            message: "the message service is unavailable".to_owned(),
            retryable: true,
            client_msg_id: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{ContentType, Ping, Send};

    #[test]
    fn a_frame_survives_encoding() {
        let send = Body::Send(Send {
            client_msg_id: id_bytes(Uuid::now_v7()),
            conversation_id: id_bytes(Uuid::now_v7()),
            content_type: ContentType::Text.into(),
            body: b"hello".to_vec(),
            client_ts: Some(1_700_000_000_000_000),
        });
        assert_eq!(decode_frame(&encode_frame(send.clone())), Ok(send));
        assert_eq!(
            decode_frame(&encode_frame(Body::Ping(Ping {}))),
            Ok(Body::Ping(Ping {}))
        );
    }

    #[test]
    fn a_frame_without_a_body_is_empty() {
        assert_eq!(decode_frame(&[]), Err(FrameError::Empty));
    }

    #[test]
    fn bytes_that_are_not_protobuf_are_malformed() {
        assert!(matches!(
            decode_frame(&[0xff, 0xff, 0xff]),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn a_frame_over_the_limit_is_refused_before_decoding() {
        let bytes = vec![0; MAX_FRAME_BYTES + 1];
        assert_eq!(
            decode_frame(&bytes),
            Err(FrameError::TooLarge {
                size: MAX_FRAME_BYTES + 1,
                limit: MAX_FRAME_BYTES,
            })
        );
    }

    #[test]
    fn an_id_is_exactly_16_bytes() {
        let uuid = Uuid::now_v7();
        assert_eq!(id(&id_bytes(uuid)), Ok(uuid));
        assert_eq!(id(&[1; 15]), Err(InvalidId(15)));
        assert_eq!(id(&[]), Err(InvalidId(0)));
    }

    #[test]
    fn a_users_channel_is_named_after_the_user() {
        let user = Uuid::parse_str("0192f5a0-0000-7000-8000-000000000001").unwrap();
        assert_eq!(
            notify_channel(user),
            "notify:0192f5a0-0000-7000-8000-000000000001"
        );
    }

    #[test]
    fn a_status_carries_the_client_error() {
        let error = Error {
            code: ErrorCode::NotContact.into(),
            message: "the users are not accepted contacts".to_owned(),
            retryable: false,
            client_msg_id: Some(id_bytes(Uuid::now_v7())),
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(status.message(), "the users are not accepted contacts");
        assert_eq!(error_from_status(&status), error);
    }

    #[test]
    fn a_status_without_a_client_error_is_a_retryable_unavailable() {
        let error = error_from_status(&tonic::Status::unavailable("connection refused"));
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert!(error.retryable);
    }
}
```

- [ ] **Step 7: Build and test**

```powershell
cargo test -p push-proto
```

The first run downloads the new crates (including `protoc-bin-vendored`'s binaries for
every platform) and updates `Cargo.lock`. Expected: 8 passed. If the build script fails
with "Could not find `protoc`", `build.rs` was not copied exactly: it must call
`config.protoc_executable(...)`.

- [ ] **Step 8: Run the gates**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 550 tests pass. Clippy also checks the generated code (it is `include!`d into
`push-proto`); it passes as generated.

- [ ] **Step 9: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/push-proto
git commit -m "feat(push-proto): wire frames and the internal gRPC API"
```

---

### Task 3: message-svc — the Redis notifier

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/push-message-svc/Cargo.toml`, `src/lib.rs`, `src/notify.rs`,
  `tests/redis.rs`

- [ ] **Step 1: Add the crate and its dependencies**

```powershell
New-Item -ItemType Directory crates\push-message-svc\src, crates\push-message-svc\tests -Force
```

Replace `Cargo.toml` with:

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-domain",
    "crates/push-message-svc",
    "crates/push-proto",
    "crates/push-store",
    "crates/push-testkit",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "MIT"
repository = "https://github.com/yury-opolev/push-messenger"

[workspace.dependencies]
async-trait = "0.1"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
libtest-mimic = "0.8"
prost = "0.14"
protoc-bin-vendored = "3"
push-domain = { path = "crates/push-domain" }
push-proto = { path = "crates/push-proto" }
push-store = { path = "crates/push-store" }
push-testkit = { path = "crates/push-testkit" }
redis = { version = "1", default-features = false, features = [
    "connection-manager",
    "tokio-comp",
] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sqlx = { version = "0.8", default-features = false, features = [
    "chrono",
    "macros",
    "migrate",
    "postgres",
    "runtime-tokio",
    "uuid",
] }
testcontainers-modules = { version = "0.13", features = ["postgres", "redis"] }
thiserror = "2"
tokio = { version = "1", features = ["sync"] }
tokio-stream = "0.1"
tonic = "0.14"
tonic-prost = "0.14"
tonic-prost-build = "0.14"
tracing = "0.1"
uuid = { version = "1", features = ["serde", "v4", "v7"] }

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

Create `crates/push-message-svc/Cargo.toml`:

```toml
[package]
name = "push-message-svc"
description = "The push-messenger message service: the messaging core over gRPC."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
async-trait.workspace = true
prost.workspace = true
push-domain.workspace = true
push-proto.workspace = true
redis.workspace = true
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "time"] }
tokio-stream.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing Redis test**

Create `crates/push-message-svc/tests/redis.rs`:

```rust
//! `RedisNotifier` against a real Redis (Docker must be running).

use std::time::Duration;

use prost::Message;
use push_domain::{Event, Notifier, UserId};
use push_message_svc::RedisNotifier;
use push_proto::internal::v1::{EntriesAvailable, Notification, notification::Kind};
use push_proto::notify_channel;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio_stream::StreamExt;

async fn redis() -> (ContainerAsync<Redis>, String) {
    let container = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start a Redis container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("the Redis port");
    (container, format!("redis://127.0.0.1:{port}"))
}

#[tokio::test]
async fn a_published_event_reaches_the_users_channel() {
    let (_container, url) = redis().await;
    let user = UserId::new();
    let mut subscriber = redis::Client::open(url.as_str())
        .unwrap()
        .get_async_pubsub()
        .await
        .expect("connect a subscriber");
    subscriber
        .subscribe(notify_channel(user.0))
        .await
        .expect("subscribe");

    let notifier = RedisNotifier::connect(&url).await.expect("connect");
    notifier
        .publish(&[Event::EntriesAvailable { users: vec![user] }])
        .await;

    let mut messages = subscriber.on_message();
    let message = tokio::time::timeout(Duration::from_secs(5), messages.next())
        .await
        .expect("a notification within 5 s")
        .expect("the subscription is open");
    assert_eq!(message.get_channel_name(), notify_channel(user.0));
    let notification = Notification::decode(message.get_payload_bytes()).unwrap();
    assert_eq!(
        notification.kind,
        Some(Kind::EntriesAvailable(EntriesAvailable {}))
    );
}

#[tokio::test]
async fn publishing_without_redis_returns_without_failing() {
    let (container, url) = redis().await;
    let notifier = RedisNotifier::connect(&url).await.expect("connect");
    container.stop().await.expect("stop Redis");
    let events = [Event::EntriesAvailable {
        users: vec![UserId::new()],
    }];
    tokio::time::timeout(Duration::from_secs(30), notifier.publish(&events))
        .await
        .expect("a failed publish gives up instead of hanging");
}
```

- [ ] **Step 3: Run it to see it fail**

Create `crates/push-message-svc/src/lib.rs` with only its doc comment for now:

```rust
//! push-message-svc: the messaging core of push-messenger (spec §2) as a gRPC service.
//!
//! It owns the `messaging` schema (through `push-store`), answers the gateway's calls
//! and, after every commit, rings the affected users' Redis channels so their gateways
//! sync them.
```

```powershell
cargo test -p push-message-svc --test redis
```

Expected: a compile error — `unresolved import push_message_svc::RedisNotifier`.

- [ ] **Step 4: Write the notifier**

Create `crates/push-message-svc/src/notify.rs`:

```rust
//! Publishing domain events to the gateways through Redis.

use async_trait::async_trait;
use prost::Message;
use push_domain::{ContactState, Event, Notifier, ReceiptUpdate};
use push_proto::internal::v1::{
    self as internal, ContactChanged, DirectReceipt, EntriesAvailable, GroupReceipt, Notification,
    ReadPositionChanged, ReceiptChanged, notification::Kind, receipt_changed,
};
use push_proto::{id_bytes, notify_channel};
use redis::aio::ConnectionManager;

/// Publishes each event as small [`Notification`]s on the Redis channels of the users it
/// concerns (`notify:{user_id}`). Never a message body: a gateway that hears
/// `EntriesAvailable` syncs the user's devices.
///
/// Best effort, as `Notifier` requires: a failed publish is logged and dropped, since
/// clients catch up on their next sync.
#[derive(Clone)]
pub struct RedisNotifier {
    connection: ConnectionManager,
}

impl RedisNotifier {
    /// Connects to Redis at `url` (`redis://:password@host:6379`). The connection
    /// reconnects by itself when it breaks.
    pub async fn connect(url: &str) -> redis::RedisResult<Self> {
        let client = redis::Client::open(url)?;
        let connection = client.get_connection_manager().await?;
        Ok(Self { connection })
    }
}

#[async_trait]
impl Notifier for RedisNotifier {
    async fn publish(&self, events: &[Event]) {
        let messages = notifications(events);
        if messages.is_empty() {
            return;
        }
        let mut pipeline = redis::pipe();
        for (channel, payload) in &messages {
            pipeline.publish(channel, payload).ignore();
        }
        let mut connection = self.connection.clone();
        if let Err(error) = pipeline.query_async::<()>(&mut connection).await {
            tracing::warn!(
                %error,
                count = messages.len(),
                "could not publish notifications; clients catch up on their next sync"
            );
        }
    }
}

/// The channel and encoded [`Notification`] for every user each event concerns, in
/// event order.
pub(crate) fn notifications(events: &[Event]) -> Vec<(String, Vec<u8>)> {
    let mut messages = Vec::new();
    for event in events {
        match event {
            Event::EntriesAvailable { users } => {
                for user in users {
                    let kind = Kind::EntriesAvailable(EntriesAvailable {});
                    messages.push(message(user.0, kind));
                }
            }
            Event::Receipt {
                conversation,
                to,
                receipt,
            } => {
                let positions = match *receipt {
                    ReceiptUpdate::Direct {
                        user_id,
                        delivered_seq,
                        read_seq,
                    } => receipt_changed::Positions::Direct(DirectReceipt {
                        user_id: id_bytes(user_id.0),
                        delivered_seq,
                        read_seq,
                    }),
                    ReceiptUpdate::Group {
                        all_delivered_seq,
                        all_read_seq,
                    } => receipt_changed::Positions::Group(GroupReceipt {
                        all_delivered_seq,
                        all_read_seq,
                    }),
                    _ => continue,
                };
                for user in to {
                    let kind = Kind::ReceiptChanged(ReceiptChanged {
                        conversation_id: id_bytes(conversation.0),
                        positions: Some(positions.clone()),
                    });
                    messages.push(message(user.0, kind));
                }
            }
            Event::ContactChanged { user, peer, state } => {
                let kind = Kind::ContactChanged(ContactChanged {
                    peer_id: id_bytes(peer.0),
                    state: contact_state(*state).into(),
                });
                messages.push(message(user.0, kind));
            }
            Event::ReadPositionChanged {
                user,
                conversation,
                read_seq,
            } => {
                let kind = Kind::ReadPositionChanged(ReadPositionChanged {
                    conversation_id: id_bytes(conversation.0),
                    read_seq: *read_seq,
                });
                messages.push(message(user.0, kind));
            }
            _ => {}
        }
    }
    messages
}

fn message(user: uuid::Uuid, kind: Kind) -> (String, Vec<u8>) {
    let notification = Notification { kind: Some(kind) };
    (notify_channel(user), notification.encode_to_vec())
}

fn contact_state(state: Option<ContactState>) -> internal::ContactState {
    match state {
        None => internal::ContactState::None,
        Some(ContactState::RequestedOut) => internal::ContactState::RequestedOut,
        Some(ContactState::RequestedIn) => internal::ContactState::RequestedIn,
        Some(ContactState::Accepted) => internal::ContactState::Accepted,
        Some(ContactState::Blocked) => internal::ContactState::Blocked,
    }
}

#[cfg(test)]
mod tests {
    use push_domain::{ConversationId, UserId};

    use super::*;

    fn decode(messages: &[(String, Vec<u8>)]) -> Vec<(String, Kind)> {
        messages
            .iter()
            .map(|(channel, payload)| {
                let notification = Notification::decode(payload.as_slice()).unwrap();
                (channel.clone(), notification.kind.unwrap())
            })
            .collect()
    }

    #[test]
    fn new_entries_ring_every_member() {
        let (alice, bob) = (UserId::new(), UserId::new());
        let messages = notifications(&[Event::EntriesAvailable {
            users: vec![alice, bob],
        }]);
        assert_eq!(
            decode(&messages),
            vec![
                (
                    notify_channel(alice.0),
                    Kind::EntriesAvailable(EntriesAvailable {})
                ),
                (
                    notify_channel(bob.0),
                    Kind::EntriesAvailable(EntriesAvailable {})
                ),
            ]
        );
    }

    #[test]
    fn a_receipt_goes_to_every_recipient_with_its_positions() {
        let (alice, bob) = (UserId::new(), UserId::new());
        let conversation = ConversationId::new();
        let messages = notifications(&[Event::Receipt {
            conversation,
            to: vec![alice],
            receipt: ReceiptUpdate::Direct {
                user_id: bob,
                delivered_seq: 4,
                read_seq: 3,
            },
        }]);
        assert_eq!(
            decode(&messages),
            vec![(
                notify_channel(alice.0),
                Kind::ReceiptChanged(ReceiptChanged {
                    conversation_id: id_bytes(conversation.0),
                    positions: Some(receipt_changed::Positions::Direct(DirectReceipt {
                        user_id: id_bytes(bob.0),
                        delivered_seq: 4,
                        read_seq: 3,
                    })),
                })
            )]
        );
    }

    #[test]
    fn contact_and_read_position_changes_go_to_their_user_only() {
        let (alice, bob) = (UserId::new(), UserId::new());
        let conversation = ConversationId::new();
        let messages = notifications(&[
            Event::ContactChanged {
                user: alice,
                peer: bob,
                state: None,
            },
            Event::ReadPositionChanged {
                user: alice,
                conversation,
                read_seq: 9,
            },
        ]);
        assert_eq!(
            decode(&messages),
            vec![
                (
                    notify_channel(alice.0),
                    Kind::ContactChanged(ContactChanged {
                        peer_id: id_bytes(bob.0),
                        state: internal::ContactState::None.into(),
                    })
                ),
                (
                    notify_channel(alice.0),
                    Kind::ReadPositionChanged(ReadPositionChanged {
                        conversation_id: id_bytes(conversation.0),
                        read_seq: 9,
                    })
                ),
            ]
        );
    }

    #[test]
    fn no_events_publish_nothing() {
        assert!(notifications(&[]).is_empty());
    }
}
```

Replace `crates/push-message-svc/src/lib.rs` with:

```rust
//! push-message-svc: the messaging core of push-messenger (spec §2) as a gRPC service.
//!
//! It owns the `messaging` schema (through `push-store`), answers the gateway's calls
//! and, after every commit, rings the affected users' Redis channels so their gateways
//! sync them.

mod notify;

pub use notify::RedisNotifier;
```

- [ ] **Step 5: Run the tests**

```powershell
cargo test -p push-message-svc
```

Expected: unit tests 4 passed; `redis` 2 passed (it starts two Redis containers).

- [ ] **Step 6: Run the gates**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 556 tests pass.

- [ ] **Step 7: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/push-message-svc
git commit -m "feat(push-message-svc): publish domain events on Redis"
```

---

### Task 4: message-svc — the gRPC services

**Files:**
- Modify: `crates/push-message-svc/Cargo.toml`, `src/lib.rs`
- Create: `crates/push-message-svc/src/convert.rs`, `src/grpc.rs`

- [ ] **Step 1: Add `chrono` and `tonic`**

Replace `crates/push-message-svc/Cargo.toml` with:

```toml
[package]
name = "push-message-svc"
description = "The push-messenger message service: the messaging core over gRPC."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
async-trait.workspace = true
chrono.workspace = true
prost.workspace = true
push-domain.workspace = true
push-proto.workspace = true
redis.workspace = true
tonic.workspace = true
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "time"] }
tokio-stream.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Conversions between the domain and protobuf**

Create `crates/push-message-svc/src/convert.rs`:

```rust
//! Between domain values and protobuf messages.

use chrono::{DateTime, Utc};
use push_domain::{ContentType, DomainError, Entry, EntryKind, SendReceipt};
use push_proto::v1::{self, Deliver, ErrorCode, SendAck};
use push_proto::{error_status, id, id_bytes};
use tonic::{Code, Status};
use uuid::Uuid;

/// A time as microseconds since the Unix epoch.
pub(crate) fn micros(at: DateTime<Utc>) -> i64 {
    at.timestamp_micros()
}

/// A time from microseconds since the Unix epoch.
pub(crate) fn from_micros(micros: i64) -> Result<DateTime<Utc>, Status> {
    DateTime::from_timestamp_micros(micros).ok_or_else(|| invalid("a time is out of range"))
}

/// An identifier field, wrapped in its domain type. A field that is not 16 bytes is an
/// `INVALID_ARGUMENT` naming the field.
pub(crate) fn parse_id<T>(bytes: &[u8], field: &str, wrap: fn(Uuid) -> T) -> Result<T, Status> {
    id(bytes)
        .map(wrap)
        .map_err(|error| invalid(format!("{field}: {error}")))
}

/// An `INVALID_ARGUMENT` status for a malformed request.
pub(crate) fn invalid(message: impl Into<String>) -> Status {
    error_status(
        Code::InvalidArgument,
        v1::Error {
            code: ErrorCode::Invalid.into(),
            message: message.into(),
            retryable: false,
            client_msg_id: None,
        },
    )
}

/// The status for a failed domain operation. Storage problems are reported without their
/// details, which the store has already logged.
pub(crate) fn domain_error(error: DomainError) -> Status {
    let (code, error_code) = match &error {
        DomainError::Invalid(_) => (Code::InvalidArgument, ErrorCode::Invalid),
        DomainError::UnknownUser | DomainError::UnknownDevice => {
            (Code::PermissionDenied, ErrorCode::UnknownDevice)
        }
        DomainError::ConversationNotFound => (Code::NotFound, ErrorCode::ConversationNotFound),
        DomainError::NotMember => (Code::FailedPrecondition, ErrorCode::NotMember),
        DomainError::NotContact => (Code::FailedPrecondition, ErrorCode::NotContact),
        DomainError::ClientIdConflict => (Code::AlreadyExists, ErrorCode::ClientIdConflict),
        DomainError::SeqOutOfRange => (Code::OutOfRange, ErrorCode::SeqOutOfRange),
        DomainError::Conflict => (Code::Aborted, ErrorCode::Unavailable),
        DomainError::Store(_) => (Code::Unavailable, ErrorCode::Unavailable),
        _ => (Code::FailedPrecondition, ErrorCode::Rejected),
    };
    let message = match &error {
        DomainError::Store(_) => "a temporary storage problem; retry".to_owned(),
        other => other.to_string(),
    };
    error_status(
        code,
        v1::Error {
            code: error_code.into(),
            message,
            retryable: error.is_retryable() || matches!(error, DomainError::Store(_)),
            client_msg_id: None,
        },
    )
}

/// A log entry as the client receives it, in its current state.
pub(crate) fn deliver(entry: Entry) -> Deliver {
    Deliver {
        conversation_id: id_bytes(entry.conversation_id.0),
        seq: entry.seq,
        message_id: id_bytes(entry.message_id.0),
        client_msg_id: id_bytes(entry.client_msg_id.0),
        kind: entry_kind(entry.kind).into(),
        target_seq: entry.target_seq,
        sender_id: id_bytes(entry.sender_id.0),
        content_type: content_type(entry.content_type).into(),
        body: entry.body,
        client_ts: entry.client_ts.map(micros),
        server_ts: micros(entry.server_ts),
        edited_at: entry.edited_at.map(micros),
        edit_seq: entry.edit_seq,
        deleted: entry.deleted_at.is_some(),
    }
}

/// What the sender learns once its message is stored.
pub(crate) fn send_ack(receipt: &SendReceipt) -> SendAck {
    SendAck {
        client_msg_id: id_bytes(receipt.client_msg_id.0),
        message_id: id_bytes(receipt.message_id.0),
        seq: receipt.seq,
        server_ts: micros(receipt.server_ts),
    }
}

fn entry_kind(kind: EntryKind) -> v1::EntryKind {
    match kind {
        EntryKind::Message => v1::EntryKind::Message,
        EntryKind::Edit => v1::EntryKind::Edit,
        EntryKind::Delete => v1::EntryKind::Delete,
        EntryKind::System => v1::EntryKind::System,
    }
}

fn content_type(content_type: ContentType) -> v1::ContentType {
    match content_type {
        ContentType::Text => v1::ContentType::Text,
        ContentType::SystemEvent => v1::ContentType::SystemEvent,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use push_domain::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
    use push_proto::error_from_status;

    use super::*;

    #[test]
    fn times_travel_as_microseconds() {
        let at = Utc.with_ymd_and_hms(2026, 10, 12, 9, 30, 0).unwrap()
            + chrono::TimeDelta::microseconds(123_456);
        assert_eq!(from_micros(micros(at)).unwrap(), at);
        assert_eq!(
            from_micros(i64::MAX).unwrap_err().code(),
            Code::InvalidArgument
        );
    }

    #[test]
    fn an_id_field_of_the_wrong_length_is_invalid() {
        let user = Uuid::now_v7();
        assert_eq!(
            parse_id(&id_bytes(user), "user_id", UserId).unwrap(),
            UserId(user)
        );
        let status = parse_id(&[1, 2, 3], "user_id", UserId).unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        let error = error_from_status(&status);
        assert_eq!(error.code(), ErrorCode::Invalid);
        assert_eq!(error.message, "user_id: an id must be 16 bytes, not 3");
    }

    #[test]
    fn an_edit_is_delivered_with_its_target_seq() {
        let at = Utc.with_ymd_and_hms(2026, 10, 12, 9, 30, 0).unwrap();
        let entry = Entry {
            conversation_id: ConversationId::new(),
            seq: 7,
            message_id: MessageId::new(),
            client_msg_id: ClientMsgId::new(),
            kind: EntryKind::Edit,
            target_seq: Some(5),
            sender_id: UserId::new(),
            sender_device_id: Some(DeviceId::new()),
            content_type: ContentType::Text,
            body: Some(b"fixed".to_vec()),
            client_ts: Some(at),
            server_ts: at,
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        };
        let deliver = deliver(entry.clone());
        assert_eq!(deliver.conversation_id, id_bytes(entry.conversation_id.0));
        assert_eq!(deliver.seq, 7);
        assert_eq!(deliver.message_id, id_bytes(entry.message_id.0));
        assert_eq!(deliver.client_msg_id, id_bytes(entry.client_msg_id.0));
        assert_eq!(deliver.kind(), v1::EntryKind::Edit);
        assert_eq!(deliver.target_seq, Some(5));
        assert_eq!(deliver.sender_id, id_bytes(entry.sender_id.0));
        assert_eq!(deliver.content_type(), v1::ContentType::Text);
        assert_eq!(deliver.body, Some(b"fixed".to_vec()));
        assert_eq!(deliver.client_ts, Some(micros(at)));
        assert_eq!(deliver.server_ts, micros(at));
        assert!(!deliver.deleted);
    }

    #[test]
    fn a_deleted_message_is_delivered_without_a_body() {
        let at = Utc.with_ymd_and_hms(2026, 10, 12, 9, 30, 0).unwrap();
        let entry = Entry {
            conversation_id: ConversationId::new(),
            seq: 5,
            message_id: MessageId::new(),
            client_msg_id: ClientMsgId::new(),
            kind: EntryKind::Message,
            target_seq: None,
            sender_id: UserId::new(),
            sender_device_id: Some(DeviceId::new()),
            content_type: ContentType::Text,
            body: None,
            client_ts: None,
            server_ts: at,
            edited_at: Some(at),
            edit_seq: Some(6),
            deleted_at: Some(at),
        };
        let deliver = deliver(entry);
        assert_eq!(deliver.body, None);
        assert!(deliver.deleted);
        assert_eq!(deliver.edited_at, Some(micros(at)));
        assert_eq!(deliver.edit_seq, Some(6));
    }

    #[test]
    fn domain_errors_become_client_errors() {
        let status = domain_error(DomainError::NotContact);
        assert_eq!(status.code(), Code::FailedPrecondition);
        let error = error_from_status(&status);
        assert_eq!(error.code(), ErrorCode::NotContact);
        assert_eq!(error.message, "the users are not accepted contacts");
        assert!(!error.retryable);

        let conflict = error_from_status(&domain_error(DomainError::Conflict));
        assert_eq!(conflict.code(), ErrorCode::Unavailable);
        assert!(conflict.retryable);

        let store = domain_error(DomainError::Store("relation missing".into()));
        assert_eq!(store.code(), Code::Unavailable);
        let store = error_from_status(&store);
        assert!(store.retryable);
        assert!(!store.message.contains("relation"), "{}", store.message);

        let device = domain_error(DomainError::UnknownDevice);
        assert_eq!(device.code(), Code::PermissionDenied);
        assert_eq!(error_from_status(&device).code(), ErrorCode::UnknownDevice);
    }
}
```

- [ ] **Step 3: The services and their tests**

Create `crates/push-message-svc/src/grpc.rs`. Its tests drive both services on
`InMemoryStore` with a `RecordingNotifier`:

```rust
//! The gRPC services: `Messaging` for the gateway and the DEV ONLY `DevSetup`.

use std::sync::Arc;

use push_domain::{
    ClientMsgId, Clock, ContactState, ConversationId, DeviceId, MAX_SYNC_BATCH, Messaging,
    Notifier, SendText, Store, UserId,
};
use push_proto::id_bytes;
use push_proto::internal::v1::{
    AckRequest, AckResponse, IdentifyRequest, IdentifyResponse, MakeContactsRequest,
    MakeContactsResponse, OpenDirectRequest, OpenDirectResponse, RegisterDeviceRequest,
    RegisterDeviceResponse, RegisterUserRequest, RegisterUserResponse, SendRequest, SyncRequest,
    SyncResponse, dev_setup_server, messaging_server,
};
use push_proto::v1::{ContentType, SendAck};
use tonic::{Request, Response, Status};

use crate::convert::{deliver, domain_error, from_micros, invalid, parse_id, send_ack};

/// The `Messaging` service: what the gateway calls for a connected device. The gateway
/// passes the user and device it verified.
pub struct MessagingApi<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
}

impl<S, C, N> MessagingApi<S, C, N> {
    pub fn new(messaging: Arc<Messaging<S, C, N>>) -> Self {
        Self { messaging }
    }
}

#[tonic::async_trait]
impl<S, C, N> messaging_server::Messaging for MessagingApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn identify(
        &self,
        request: Request<IdentifyRequest>,
    ) -> Result<Response<IdentifyResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        self.messaging
            .identify(device, user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(IdentifyResponse {}))
    }

    async fn send(&self, request: Request<SendRequest>) -> Result<Response<SendAck>, Status> {
        let request = request.into_inner();
        let sender = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        let send = request.send.ok_or_else(|| invalid("send is required"))?;
        if send.content_type() != ContentType::Text {
            return Err(invalid("only text messages can be sent"));
        }
        let command = SendText {
            sender,
            device,
            conversation: parse_id(&send.conversation_id, "conversation_id", ConversationId)?,
            client_msg_id: parse_id(&send.client_msg_id, "client_msg_id", ClientMsgId)?,
            client_ts: send.client_ts.map(from_micros).transpose()?,
            body: send.body,
        };
        let receipt = self
            .messaging
            .send_text(command)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(send_ack(&receipt)))
    }

    async fn sync(&self, request: Request<SyncRequest>) -> Result<Response<SyncResponse>, Status> {
        let request = request.into_inner();
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        let limit = match request.limit {
            0 => MAX_SYNC_BATCH,
            limit => limit,
        };
        let batch = self
            .messaging
            .sync(device, limit)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(SyncResponse {
            entries: batch.entries.into_iter().map(deliver).collect(),
            more: batch.more,
        }))
    }

    async fn ack(&self, request: Request<AckRequest>) -> Result<Response<AckResponse>, Status> {
        let request = request.into_inner();
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        let ack = request.ack.ok_or_else(|| invalid("ack is required"))?;
        let conversation = parse_id(&ack.conversation_id, "conversation_id", ConversationId)?;
        let moved = self
            .messaging
            .ack(device, conversation, ack.seq)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AckResponse { moved }))
    }
}

/// DEV ONLY: sets up users, devices, contacts and direct conversations without auth-svc.
/// `serve` registers it only when the configuration enables it (`PUSH_DEV_SETUP=1`).
pub struct DevSetupApi<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
}

impl<S, C, N> DevSetupApi<S, C, N> {
    pub fn new(messaging: Arc<Messaging<S, C, N>>) -> Self {
        Self { messaging }
    }
}

#[tonic::async_trait]
impl<S, C, N> dev_setup_server::DevSetup for DevSetupApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn register_user(
        &self,
        request: Request<RegisterUserRequest>,
    ) -> Result<Response<RegisterUserResponse>, Status> {
        let user = parse_id(&request.into_inner().user_id, "user_id", UserId)?;
        self.messaging
            .register_user(user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(RegisterUserResponse {}))
    }

    async fn register_device(
        &self,
        request: Request<RegisterDeviceRequest>,
    ) -> Result<Response<RegisterDeviceResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        self.messaging
            .register_device(device, user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(RegisterDeviceResponse {}))
    }

    async fn make_contacts(
        &self,
        request: Request<MakeContactsRequest>,
    ) -> Result<Response<MakeContactsResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let peer = parse_id(&request.peer_id, "peer_id", UserId)?;
        let state = self
            .messaging
            .contact_request(user, peer)
            .await
            .map_err(domain_error)?;
        if state != ContactState::Accepted {
            self.messaging
                .contact_accept(peer, user)
                .await
                .map_err(domain_error)?;
        }
        Ok(Response::new(MakeContactsResponse {}))
    }

    async fn open_direct(
        &self,
        request: Request<OpenDirectRequest>,
    ) -> Result<Response<OpenDirectResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let peer = parse_id(&request.peer_id, "peer_id", UserId)?;
        let conversation = self
            .messaging
            .open_direct(user, peer)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(OpenDirectResponse {
            conversation_id: id_bytes(conversation.0),
        }))
    }
}

#[cfg(test)]
mod tests {
    use push_domain::{Event, InMemoryStore, RecordingNotifier, SystemClock};
    use push_proto::internal::v1::dev_setup_server::DevSetup;
    use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
    use push_proto::v1::{Ack, ErrorCode, Send};
    use push_proto::{error_from_status, id};
    use tonic::Code;

    use super::*;

    type Api = MessagingApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Setup = DevSetupApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;

    struct World {
        api: Api,
        setup: Setup,
        notifier: Arc<RecordingNotifier>,
    }

    fn world() -> World {
        let notifier = Arc::new(RecordingNotifier::default());
        let messaging = Arc::new(Messaging::new(
            InMemoryStore::default(),
            SystemClock,
            Arc::clone(&notifier),
        ));
        World {
            api: MessagingApi::new(Arc::clone(&messaging)),
            setup: DevSetupApi::new(messaging),
            notifier,
        }
    }

    /// A user with one device.
    struct Person {
        user: UserId,
        device: DeviceId,
    }

    async fn person(world: &World) -> Person {
        let person = Person {
            user: UserId::new(),
            device: DeviceId::new(),
        };
        world
            .setup
            .register_user(Request::new(RegisterUserRequest {
                user_id: id_bytes(person.user.0),
            }))
            .await
            .unwrap();
        world
            .setup
            .register_device(Request::new(RegisterDeviceRequest {
                user_id: id_bytes(person.user.0),
                device_id: id_bytes(person.device.0),
            }))
            .await
            .unwrap();
        person
    }

    /// Alice and Bob as contacts with a direct conversation.
    async fn chat(world: &World) -> (Person, Person, ConversationId) {
        let (alice, bob) = (person(world).await, person(world).await);
        world
            .setup
            .make_contacts(Request::new(MakeContactsRequest {
                user_id: id_bytes(alice.user.0),
                peer_id: id_bytes(bob.user.0),
            }))
            .await
            .unwrap();
        let opened = world
            .setup
            .open_direct(Request::new(OpenDirectRequest {
                user_id: id_bytes(alice.user.0),
                peer_id: id_bytes(bob.user.0),
            }))
            .await
            .unwrap()
            .into_inner();
        let conversation = ConversationId(id(&opened.conversation_id).unwrap());
        (alice, bob, conversation)
    }

    fn send_request(from: &Person, conversation: ConversationId, body: &str) -> SendRequest {
        SendRequest {
            user_id: id_bytes(from.user.0),
            device_id: id_bytes(from.device.0),
            send: Some(Send {
                client_msg_id: id_bytes(ClientMsgId::new().0),
                conversation_id: id_bytes(conversation.0),
                content_type: ContentType::Text.into(),
                body: body.as_bytes().to_vec(),
                client_ts: None,
            }),
        }
    }

    async fn sync(world: &World, device: DeviceId, limit: u32) -> SyncResponse {
        world
            .api
            .sync(Request::new(SyncRequest {
                device_id: id_bytes(device.0),
                limit,
            }))
            .await
            .unwrap()
            .into_inner()
    }

    #[tokio::test]
    async fn a_sent_message_is_synced_and_acked() {
        let world = world();
        let (alice, bob, conversation) = chat(&world).await;
        world.notifier.take();

        let request = send_request(&alice, conversation, "hello");
        let client_msg_id = request.send.as_ref().unwrap().client_msg_id.clone();
        let sent = world
            .api
            .send(Request::new(request))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(sent.client_msg_id, client_msg_id);
        assert_eq!(sent.seq, 1);
        let mut rung = Vec::new();
        for event in world.notifier.take() {
            if let Event::EntriesAvailable { users } = event {
                rung.extend(users);
            }
        }
        rung.sort();
        let mut members = vec![alice.user, bob.user];
        members.sort();
        assert_eq!(rung, members);

        let batch = sync(&world, bob.device, 0).await;
        assert_eq!(batch.entries.len(), 1);
        assert!(!batch.more);
        let entry = &batch.entries[0];
        assert_eq!(entry.seq, 1);
        assert_eq!(entry.message_id, sent.message_id);
        assert_eq!(entry.sender_id, id_bytes(alice.user.0));
        assert_eq!(entry.body.as_deref(), Some(b"hello".as_slice()));

        let acked = world
            .api
            .ack(Request::new(AckRequest {
                device_id: id_bytes(bob.device.0),
                ack: Some(Ack {
                    conversation_id: id_bytes(conversation.0),
                    seq: 1,
                }),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(acked.moved);
        assert!(sync(&world, bob.device, 0).await.entries.is_empty());
    }

    #[tokio::test]
    async fn a_sync_limit_of_zero_means_the_largest_batch() {
        let world = world();
        let (alice, bob, conversation) = chat(&world).await;
        for body in ["one", "two"] {
            let request = send_request(&alice, conversation, body);
            world.api.send(Request::new(request)).await.unwrap();
        }
        assert_eq!(sync(&world, bob.device, 0).await.entries.len(), 2);
        let first = sync(&world, bob.device, 1).await;
        assert_eq!(first.entries.len(), 1);
        assert!(first.more);
    }

    #[tokio::test]
    async fn identify_refuses_a_device_of_another_user() {
        let world = world();
        let (alice, bob) = (person(&world).await, person(&world).await);
        let own = world
            .api
            .identify(Request::new(IdentifyRequest {
                user_id: id_bytes(alice.user.0),
                device_id: id_bytes(alice.device.0),
            }))
            .await;
        assert!(own.is_ok());
        let status = world
            .api
            .identify(Request::new(IdentifyRequest {
                user_id: id_bytes(alice.user.0),
                device_id: id_bytes(bob.device.0),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::PermissionDenied);
        assert_eq!(error_from_status(&status).code(), ErrorCode::UnknownDevice);
    }

    #[tokio::test]
    async fn a_send_to_an_unknown_conversation_is_not_found() {
        let world = world();
        let alice = person(&world).await;
        let request = send_request(&alice, ConversationId::new(), "hello");
        let status = world.api.send(Request::new(request)).await.unwrap_err();
        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(
            error_from_status(&status).code(),
            ErrorCode::ConversationNotFound
        );
    }

    #[tokio::test]
    async fn malformed_sends_are_invalid() {
        let world = world();
        let (alice, _bob, conversation) = chat(&world).await;

        let mut short_id = send_request(&alice, conversation, "hello");
        short_id.send.as_mut().unwrap().conversation_id = vec![1, 2, 3];
        let not_text = SendRequest {
            send: Some(Send {
                content_type: ContentType::SystemEvent.into(),
                ..send_request(&alice, conversation, "hello").send.unwrap()
            }),
            ..send_request(&alice, conversation, "hello")
        };
        let empty = send_request(&alice, conversation, "");
        let missing = SendRequest {
            send: None,
            ..send_request(&alice, conversation, "hello")
        };
        for request in [short_id, not_text, empty, missing] {
            let status = world.api.send(Request::new(request)).await.unwrap_err();
            assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
            assert_eq!(error_from_status(&status).code(), ErrorCode::Invalid);
        }
    }
}
```

Replace `crates/push-message-svc/src/lib.rs` with:

```rust
//! push-message-svc: the messaging core of push-messenger (spec §2) as a gRPC service.
//!
//! It owns the `messaging` schema (through `push-store`), answers the gateway's calls
//! and, after every commit, rings the affected users' Redis channels so their gateways
//! sync them.

mod convert;
mod grpc;
mod notify;

pub use grpc::{DevSetupApi, MessagingApi};
pub use notify::RedisNotifier;
```

- [ ] **Step 4: Run the tests**

```powershell
cargo test -p push-message-svc --lib
```

Expected: 14 passed (4 `notify`, 5 `convert`, 5 `grpc`).

- [ ] **Step 5: Run the gates**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 566 tests pass.

- [ ] **Step 6: Commit**

```powershell
git add Cargo.lock crates/push-message-svc
git commit -m "feat(push-message-svc): Messaging and DevSetup gRPC services"
```

---

### Task 5: message-svc — configuration, serving and shutdown

**Files:**
- Modify: `Cargo.toml`, `crates/push-message-svc/Cargo.toml`, `src/lib.rs`
- Create: `crates/push-message-svc/src/config.rs`, `src/server.rs`, `src/main.rs`,
  `tests/service.rs`

- [ ] **Step 1: Dependencies**

Replace `Cargo.toml` with:

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-domain",
    "crates/push-message-svc",
    "crates/push-proto",
    "crates/push-store",
    "crates/push-testkit",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "MIT"
repository = "https://github.com/yury-opolev/push-messenger"

[workspace.dependencies]
anyhow = "1"
async-trait = "0.1"
axum = "0.8"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
libtest-mimic = "0.8"
prost = "0.14"
protoc-bin-vendored = "3"
push-domain = { path = "crates/push-domain" }
push-proto = { path = "crates/push-proto" }
push-store = { path = "crates/push-store" }
push-testkit = { path = "crates/push-testkit" }
redis = { version = "1", default-features = false, features = [
    "connection-manager",
    "tokio-comp",
] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sqlx = { version = "0.8", default-features = false, features = [
    "chrono",
    "macros",
    "migrate",
    "postgres",
    "runtime-tokio",
    "uuid",
] }
testcontainers-modules = { version = "0.13", features = ["postgres", "redis"] }
thiserror = "2"
tokio = { version = "1", features = ["sync"] }
tokio-stream = { version = "0.1", features = ["net"] }
tokio-util = "0.7"
tonic = "0.14"
tonic-prost = "0.14"
tonic-prost-build = "0.14"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
uuid = { version = "1", features = ["serde", "v4", "v7"] }

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

Replace `crates/push-message-svc/Cargo.toml` with:

```toml
[package]
name = "push-message-svc"
description = "The push-messenger message service: the messaging core over gRPC."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
anyhow.workspace = true
async-trait.workspace = true
axum.workspace = true
chrono.workspace = true
prost.workspace = true
push-domain.workspace = true
push-proto.workspace = true
push-store.workspace = true
redis.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
tokio-stream.workspace = true
tokio-util.workspace = true
tonic.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
uuid.workspace = true

[dev-dependencies]
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["io-util"] }

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing service test**

Create `crates/push-message-svc/tests/service.rs`. It starts Postgres and Redis, runs the
service in-process on free ports, waits for `/readyz`, goes through setup, send (checking
that Bob's channel rings), sync and ack, and stops it; a second test checks that
`DevSetup` is absent unless enabled:

```rust
//! The whole service on real Postgres and Redis (Docker must be running): readiness, the
//! dev setup calls, a send that rings the recipient's Redis channel, sync, ack, and
//! graceful shutdown.

use std::net::SocketAddr;
use std::time::Duration;

use prost::Message;
use push_message_svc::{Config, serve};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AckRequest, MakeContactsRequest, Notification, OpenDirectRequest, RegisterDeviceRequest,
    RegisterUserRequest, SendRequest, SyncRequest, notification::Kind,
};
use push_proto::v1::{Ack, ContentType, Send};
use push_proto::{id_bytes, notify_channel};
use push_store::StoreOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Running {
    _postgres: ContainerAsync<Postgres>,
    _redis: ContainerAsync<Redis>,
    redis_url: String,
    grpc: SocketAddr,
    health: SocketAddr,
    shutdown: CancellationToken,
    server: JoinHandle<anyhow::Result<()>>,
}

/// Starts Postgres, Redis and the service on free local ports.
async fn start(dev_setup: bool) -> Running {
    let postgres = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start Postgres (is Docker running?)");
    let redis = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start Redis");
    let postgres_port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();
    let redis_url = format!("redis://127.0.0.1:{redis_port}");

    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (grpc, health) = (
        grpc_listener.local_addr().unwrap(),
        health_listener.local_addr().unwrap(),
    );
    let config = Config {
        database_url: format!("postgres://postgres:postgres@127.0.0.1:{postgres_port}/postgres"),
        redis_url: redis_url.clone(),
        grpc_addr: grpc,
        health_addr: health,
        store: StoreOptions::default(),
        dev_setup,
    };
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        config,
        grpc_listener,
        health_listener,
        shutdown.clone(),
    ));
    let running = Running {
        _postgres: postgres,
        _redis: redis,
        redis_url,
        grpc,
        health,
        shutdown,
        server,
    };
    for _ in 0..120 {
        if get(running.health, "/readyz").await.0 == 200 {
            return running;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("the service did not become ready within 30 s");
}

/// A minimal HTTP/1.1 GET: the status code and the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return (0, String::new());
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    (status, body)
}

#[tokio::test]
async fn a_message_goes_from_send_to_ack_and_rings_the_recipient() {
    let running = start(true).await;
    assert_eq!(
        get(running.health, "/healthz").await,
        (200, "ok".to_owned())
    );
    assert_eq!(
        get(running.health, "/readyz").await,
        (200, "ready".to_owned())
    );

    let url = format!("http://{}", running.grpc);
    let mut setup = DevSetupClient::connect(url.clone()).await.unwrap();
    let mut messaging = MessagingClient::connect(url).await.unwrap();
    let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
    let (alice_device, bob_device) = (Uuid::now_v7(), Uuid::now_v7());
    for (user, device) in [(alice, alice_device), (bob, bob_device)] {
        setup
            .register_user(RegisterUserRequest {
                user_id: id_bytes(user),
            })
            .await
            .unwrap();
        setup
            .register_device(RegisterDeviceRequest {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
            })
            .await
            .unwrap();
    }
    setup
        .make_contacts(MakeContactsRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap();
    let conversation = setup
        .open_direct(OpenDirectRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap()
        .into_inner()
        .conversation_id;

    let mut bell = redis::Client::open(running.redis_url.as_str())
        .unwrap()
        .get_async_pubsub()
        .await
        .unwrap();
    bell.subscribe(notify_channel(bob)).await.unwrap();

    let ack = messaging
        .send(SendRequest {
            user_id: id_bytes(alice),
            device_id: id_bytes(alice_device),
            send: Some(Send {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: conversation.clone(),
                content_type: ContentType::Text.into(),
                body: b"hello".to_vec(),
                client_ts: None,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ack.seq, 1);

    let rung = tokio::time::timeout(Duration::from_secs(5), bell.on_message().next())
        .await
        .expect("Bob's channel rings within 5 s")
        .unwrap();
    let notification = Notification::decode(rung.get_payload_bytes()).unwrap();
    assert!(matches!(notification.kind, Some(Kind::EntriesAvailable(_))));

    let batch = messaging
        .sync(SyncRequest {
            device_id: id_bytes(bob_device),
            limit: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(batch.entries.len(), 1);
    assert_eq!(batch.entries[0].body.as_deref(), Some(b"hello".as_slice()));
    let acked = messaging
        .ack(AckRequest {
            device_id: id_bytes(bob_device),
            ack: Some(Ack {
                conversation_id: conversation,
                seq: 1,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(acked.moved);

    running.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), running.server)
        .await
        .expect("the service stops within 10 s")
        .unwrap()
        .expect("a clean stop");
}

#[tokio::test]
async fn dev_setup_is_absent_unless_enabled() {
    let running = start(false).await;
    let mut setup = DevSetupClient::connect(format!("http://{}", running.grpc))
        .await
        .unwrap();
    let status = setup
        .register_user(RegisterUserRequest {
            user_id: id_bytes(Uuid::now_v7()),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
}
```

```powershell
cargo test -p push-message-svc --test service
```

Expected: compile errors — `push_message_svc::Config` and `serve` do not exist.

- [ ] **Step 3: Configuration**

Create `crates/push-message-svc/src/config.rs`:

```rust
//! Configuration from environment variables.

use std::net::SocketAddr;

use push_store::StoreOptions;

/// How the service runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_DATABASE_URL` | Postgres URL of the `push_messaging` database, as its owner | required |
/// | `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
/// | `PUSH_GRPC_ADDR` | address of the gRPC API | `0.0.0.0:50051` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size | `10` |
/// | `PUSH_DEV_SETUP` | `1` registers the DEV ONLY `DevSetup` service | off |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub database_url: String,
    pub redis_url: String,
    pub grpc_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub store: StoreOptions,
    pub dev_setup: bool,
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

impl Config {
    /// Reads the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads variables through `lookup`, which returns a variable's value if it is set.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let required = |name: &'static str| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .ok_or(ConfigError::Missing(name))
        };
        let store = StoreOptions {
            max_connections: parsed(&lookup, "PUSH_DB_MAX_CONNECTIONS", 10)?,
            ..StoreOptions::default()
        };
        Ok(Self {
            database_url: required("PUSH_DATABASE_URL")?,
            redis_url: required("PUSH_REDIS_URL")?,
            grpc_addr: parsed(&lookup, "PUSH_GRPC_ADDR", ([0, 0, 0, 0], 50051).into())?,
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            store,
            dev_setup: dev_flag(&lookup, "PUSH_DEV_SETUP")?,
        })
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
    use std::collections::HashMap;

    use super::*;

    fn config(variables: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let variables: HashMap<String, String> = variables
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        Config::from_lookup(|name| variables.get(name).cloned())
    }

    const REQUIRED: [(&str, &str); 2] = [
        ("PUSH_DATABASE_URL", "postgres://push@db/push_messaging"),
        ("PUSH_REDIS_URL", "redis://:secret@redis:6379"),
    ];

    #[test]
    fn defaults_apply_when_only_the_urls_are_set() {
        let config = config(&REQUIRED).unwrap();
        assert_eq!(config.database_url, "postgres://push@db/push_messaging");
        assert_eq!(config.redis_url, "redis://:secret@redis:6379");
        assert_eq!(config.grpc_addr, "0.0.0.0:50051".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.store, StoreOptions::default());
        assert!(!config.dev_setup);
    }

    #[test]
    fn the_urls_are_required() {
        assert_eq!(
            config(&REQUIRED[1..]),
            Err(ConfigError::Missing("PUSH_DATABASE_URL"))
        );
        assert_eq!(
            config(&[REQUIRED[0], ("PUSH_REDIS_URL", "")]),
            Err(ConfigError::Missing("PUSH_REDIS_URL"))
        );
    }

    #[test]
    fn addresses_and_pool_size_can_be_set() {
        let mut variables = REQUIRED.to_vec();
        variables.extend([
            ("PUSH_GRPC_ADDR", "127.0.0.1:6000"),
            ("PUSH_HEALTH_ADDR", "127.0.0.1:6001"),
            ("PUSH_DB_MAX_CONNECTIONS", "3"),
        ]);
        let config = config(&variables).unwrap();
        assert_eq!(config.grpc_addr, "127.0.0.1:6000".parse().unwrap());
        assert_eq!(config.health_addr, "127.0.0.1:6001".parse().unwrap());
        assert_eq!(config.store.max_connections, 3);
    }

    #[test]
    fn dev_setup_is_on_only_for_exactly_1() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_SETUP", value));
            assert_eq!(config(&variables).unwrap().dev_setup, expected, "{value:?}");
        }
        for value in ["true", "yes", " 1"] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_SETUP", value));
            assert!(
                matches!(
                    config(&variables),
                    Err(ConfigError::Invalid {
                        name: "PUSH_DEV_SETUP",
                        ..
                    })
                ),
                "{value:?}"
            );
        }
    }

    #[test]
    fn a_malformed_address_is_refused() {
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_GRPC_ADDR", "not-an-address"));
        assert!(matches!(
            config(&variables),
            Err(ConfigError::Invalid {
                name: "PUSH_GRPC_ADDR",
                ..
            })
        ));
    }
}
```

- [ ] **Step 4: Serving, health, logging and signals**

Create `crates/push-message-svc/src/server.rs`:

```rust
//! Running the service: the gRPC API, the health endpoints and graceful shutdown.

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use push_domain::{Messaging, SystemClock};
use push_proto::internal::v1::dev_setup_server::DevSetupServer;
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_store::{PgStore, migrate_database};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::grpc::{DevSetupApi, MessagingApi};
use crate::notify::RedisNotifier;

/// How long to wait before trying an unreachable dependency again.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Runs the service until `shutdown` is cancelled, then finishes the calls in progress
/// and closes the database pool.
///
/// The health endpoints answer at once: `/healthz` always, `/readyz` only once the
/// database is migrated and answers a ping, and never after shutdown began. Postgres and
/// Redis are retried until they answer, so the service may start before them.
pub async fn serve(
    config: Config,
    grpc: TcpListener,
    health: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let ready = Health {
        store: Arc::new(OnceLock::new()),
        shutdown: shutdown.clone(),
    };
    let health_server = tokio::spawn(
        axum::serve(health, health_router(ready.clone()))
            .with_graceful_shutdown(shutdown.clone().cancelled_owned())
            .into_future(),
    );

    if config.dev_setup {
        tracing::warn!(
            "DEV ONLY: PUSH_DEV_SETUP=1 registers the DevSetup service, which creates users, \
             devices, contacts and conversations without authentication. Never enable it \
             outside a local cluster."
        );
    }
    let Some(store) = retry("postgres", &shutdown, || async {
        // Migrations run on a connection of their own, without the pool's request-sized
        // statement and lock timeouts.
        migrate_database(&config.database_url).await?;
        let store = PgStore::connect(&config.database_url, &config.store).await?;
        anyhow::Ok(store)
    })
    .await
    else {
        return finish(health_server).await;
    };
    let Some(notifier) = retry("redis", &shutdown, || {
        RedisNotifier::connect(&config.redis_url)
    })
    .await
    else {
        store.close().await;
        return finish(health_server).await;
    };
    let _ = ready.store.set(store.clone());

    let messaging = Arc::new(Messaging::new(store.clone(), SystemClock, notifier));
    let dev_setup = config
        .dev_setup
        .then(|| DevSetupServer::new(DevSetupApi::new(Arc::clone(&messaging))));
    tracing::info!(addr = %config.grpc_addr, "serving gRPC");
    let result = tonic::transport::Server::builder()
        .add_service(MessagingServer::new(MessagingApi::new(messaging)))
        .add_optional_service(dev_setup)
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(grpc),
            shutdown.clone().cancelled_owned(),
        )
        .await;
    store.close().await;
    tracing::info!("stopped");
    result?;
    finish(health_server).await
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
        match connect().await {
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
    store: Arc<OnceLock<PgStore>>,
    shutdown: CancellationToken,
}

fn health_router(health: Health) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .with_state(health)
}

async fn readyz(State(health): State<Health>) -> (StatusCode, &'static str) {
    if health.shutdown.is_cancelled() {
        return (StatusCode::SERVICE_UNAVAILABLE, "shutting down");
    }
    match health.store.get() {
        None => (StatusCode::SERVICE_UNAVAILABLE, "starting"),
        Some(store) => match store.ping().await {
            Ok(()) => (StatusCode::OK, "ready"),
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

Create `crates/push-message-svc/src/main.rs`:

```rust
use push_message_svc::{Config, init_logging, serve, terminated};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    let config = Config::from_env()?;
    let grpc = TcpListener::bind(config.grpc_addr).await?;
    let health = TcpListener::bind(config.health_addr).await?;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        terminated().await;
        tracing::info!("shutting down");
        signal.cancel();
    });
    serve(config, grpc, health, shutdown).await
}
```

Replace `crates/push-message-svc/src/lib.rs` with:

```rust
//! push-message-svc: the messaging core of push-messenger (spec §2) as a gRPC service.
//!
//! It owns the `messaging` schema (through `push-store`), answers the gateway's calls
//! and, after every commit, rings the affected users' Redis channels so their gateways
//! sync them.

mod config;
mod convert;
mod grpc;
mod notify;
mod server;

pub use config::{Config, ConfigError};
pub use grpc::{DevSetupApi, MessagingApi};
pub use notify::RedisNotifier;
pub use server::{init_logging, serve, terminated};
```

- [ ] **Step 5: Run the tests**

```powershell
cargo test -p push-message-svc
```

Expected: unit tests 19 passed (5 new `config`), `main` 0, `redis` 2, `service` 2.

- [ ] **Step 6: Run the gates**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 573 tests pass.

- [ ] **Step 7: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/push-message-svc
git commit -m "feat(push-message-svc): serve gRPC with health, migrations and shutdown"
```

---

### Task 6: gateway — configuration and the session state machine

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/push-gateway/Cargo.toml`, `src/lib.rs`, `src/config.rs`,
  `src/session.rs`

- [ ] **Step 1: Add the crate**

```powershell
New-Item -ItemType Directory crates\push-gateway\src, crates\push-gateway\tests -Force
```

Replace `Cargo.toml` with:

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-domain",
    "crates/push-gateway",
    "crates/push-message-svc",
    "crates/push-proto",
    "crates/push-store",
    "crates/push-testkit",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "MIT"
repository = "https://github.com/yury-opolev/push-messenger"

[workspace.dependencies]
anyhow = "1"
async-trait = "0.1"
axum = "0.8"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
libtest-mimic = "0.8"
prost = "0.14"
protoc-bin-vendored = "3"
push-domain = { path = "crates/push-domain" }
push-proto = { path = "crates/push-proto" }
push-store = { path = "crates/push-store" }
push-testkit = { path = "crates/push-testkit" }
redis = { version = "1", default-features = false, features = [
    "connection-manager",
    "tokio-comp",
] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sqlx = { version = "0.8", default-features = false, features = [
    "chrono",
    "macros",
    "migrate",
    "postgres",
    "runtime-tokio",
    "uuid",
] }
testcontainers-modules = { version = "0.13", features = ["postgres", "redis"] }
thiserror = "2"
tokio = { version = "1", features = ["sync"] }
tokio-stream = { version = "0.1", features = ["net"] }
tokio-util = "0.7"
tonic = "0.14"
tonic-prost = "0.14"
tonic-prost-build = "0.14"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
uuid = { version = "1", features = ["serde", "v4", "v7"] }

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

Create `crates/push-gateway/Cargo.toml`:

```toml
[package]
name = "push-gateway"
description = "The push-messenger gateway: client WebSockets in front of message-svc."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
push-proto.workspace = true
thiserror.workspace = true
uuid.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Configuration**

Create `crates/push-gateway/src/config.rs`:

```rust
//! Configuration from environment variables.

use std::net::SocketAddr;
use std::time::Duration;

/// How the gateway runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc | required |
/// | `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
/// | `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
/// | `PUSH_DEV_AUTH` | `1` accepts DEV ONLY identities without a token | off |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub message_svc_url: String,
    pub redis_url: String,
    pub listen_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub heartbeat: Duration,
    pub dev_auth: bool,
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

impl Config {
    /// Reads the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads variables through `lookup`, which returns a variable's value if it is set.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let required = |name: &'static str| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .ok_or(ConfigError::Missing(name))
        };
        let heartbeat_ms: u64 = parsed(&lookup, "PUSH_HEARTBEAT_MS", 30_000)?;
        if !(1_000..=600_000).contains(&heartbeat_ms) {
            return Err(ConfigError::Invalid {
                name: "PUSH_HEARTBEAT_MS",
                value: heartbeat_ms.to_string(),
                reason: "use 1000 to 600000 milliseconds".to_owned(),
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
    use std::collections::HashMap;

    use super::*;

    fn config(variables: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let variables: HashMap<String, String> = variables
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        Config::from_lookup(|name| variables.get(name).cloned())
    }

    const REQUIRED: [(&str, &str); 2] = [
        ("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051"),
        ("PUSH_REDIS_URL", "redis://:secret@redis:6379"),
    ];

    #[test]
    fn defaults_apply_when_only_the_urls_are_set() {
        let config = config(&REQUIRED).unwrap();
        assert_eq!(config.message_svc_url, "http://message-svc:50051");
        assert_eq!(config.redis_url, "redis://:secret@redis:6379");
        assert_eq!(config.listen_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.heartbeat, Duration::from_secs(30));
        assert!(!config.dev_auth);
    }

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

    #[test]
    fn the_heartbeat_must_be_between_one_second_and_ten_minutes() {
        for (value, ok) in [
            ("1000", true),
            ("600000", true),
            ("999", false),
            ("600001", false),
        ] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_HEARTBEAT_MS", value));
            assert_eq!(config(&variables).is_ok(), ok, "{value}");
        }
    }

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
            assert!(
                matches!(
                    config(&variables),
                    Err(ConfigError::Invalid {
                        name: "PUSH_DEV_AUTH",
                        ..
                    })
                ),
                "{value:?}"
            );
        }
    }
}
```

- [ ] **Step 3: The session state machine and its tests**

Create `crates/push-gateway/src/session.rs`. It has no I/O: each client frame becomes a
`Step` for the connection task to carry out, and `fresh` filters out entries already
sent on this connection:

```rust
//! The protocol state of one WebSocket connection, without any I/O: which frames are
//! allowed when, what each one asks the gateway to do, and which entries the client has
//! already been sent.

use std::collections::HashMap;
use std::time::Duration;

use push_proto::close;
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, Deliver, Error, ErrorCode, Pong, Ready, Send};
use push_proto::{FrameError, decode_frame, id, id_bytes};
use uuid::Uuid;

/// What the gateway must do for a frame the client sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Send this frame to the client.
    Reply(Body),
    /// Ask message-svc whether `device` is an active device of `user`, then call
    /// [`Session::identified`] or close with [`close::INVALID_IDENTITY`].
    Identify { user: Uuid, device: Uuid },
    /// Forward a send to message-svc; answer with its `SendAck` or an error.
    Send(Send),
    /// Forward an ack to message-svc; answer only if it fails.
    Ack(Ack),
    /// Close the connection with this code and reason.
    Close(u16, &'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Connected; the first frame must be `Identify`.
    New,
    /// `Identify` is being checked.
    Identifying,
    Ready {
        device: Uuid,
    },
}

#[derive(Debug)]
pub struct Session {
    dev_auth: bool,
    state: State,
    /// The highest `seq` sent to the client per conversation, so a sync that repeats
    /// entries the client has not acked yet does not send them twice.
    sent: HashMap<Vec<u8>, i64>,
}

impl Session {
    /// A new connection. `dev_auth` is whether DEV ONLY identities are accepted.
    pub fn new(dev_auth: bool) -> Self {
        Self {
            dev_auth,
            state: State::New,
            sent: HashMap::new(),
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
                return Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify");
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
                if !self.dev_auth {
                    return Step::Close(
                        close::INVALID_IDENTITY,
                        "token sign-in is not available yet",
                    );
                }
                match (id(&identify.dev_user_id), id(&identify.dev_device_id)) {
                    (Ok(user), Ok(device)) => {
                        self.state = State::Identifying;
                        Step::Identify { user, device }
                    }
                    _ => Step::Close(close::INVALID_IDENTITY, "invalid dev identity"),
                }
            }
            (_, State::New | State::Identifying) => {
                Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify")
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
                Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify")
            }
        }
    }

    /// message-svc confirmed the identity: the `Ready` frame to send.
    pub fn identified(&mut self, user: Uuid, device: Uuid, heartbeat: Duration) -> Body {
        self.state = State::Ready { device };
        Body::Ready(Ready {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
            heartbeat_ms: u32::try_from(heartbeat.as_millis()).unwrap_or(u32::MAX),
        })
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
}

/// An `Error` frame for the client.
pub(crate) fn error_frame(code: ErrorCode, message: &str) -> Body {
    Body::Error(Error {
        code: code.into(),
        message: message.to_owned(),
        retryable: false,
        client_msg_id: None,
    })
}

fn reply_error(code: ErrorCode, message: &str) -> Step {
    Step::Reply(error_frame(code, message))
}

#[cfg(test)]
mod tests {
    use push_proto::encode_frame;
    use push_proto::v1::{Identify, Ping};

    use super::*;

    fn identify(user: Uuid, device: Uuid) -> Vec<u8> {
        encode_frame(Body::Identify(Identify {
            token: String::new(),
            dev_user_id: id_bytes(user),
            dev_device_id: id_bytes(device),
        }))
    }

    fn ready_session() -> (Session, Uuid) {
        let mut session = Session::new(true);
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(
            session.on_binary(&identify(user, device)),
            Step::Identify { user, device }
        );
        session.identified(user, device, Duration::from_secs(30));
        (session, device)
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
    fn a_dev_identity_is_checked_then_ready() {
        let mut session = Session::new(true);
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(
            session.on_binary(&identify(user, device)),
            Step::Identify { user, device }
        );
        assert_eq!(session.device(), None);
        let ready = session.identified(user, device, Duration::from_secs(30));
        assert_eq!(
            ready,
            Body::Ready(Ready {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
                heartbeat_ms: 30_000,
            })
        );
        assert_eq!(session.device(), Some(device));
    }

    #[test]
    fn without_dev_auth_identify_closes_with_4001() {
        let mut session = Session::new(false);
        assert_eq!(
            session.on_binary(&identify(Uuid::now_v7(), Uuid::now_v7())),
            Step::Close(4001, "token sign-in is not available yet")
        );
    }

    #[test]
    fn anything_but_identify_first_closes_with_4001() {
        let ping = encode_frame(Body::Ping(Ping {}));
        for bytes in [ping.as_slice(), &[0xff, 0xff], &[]] {
            let mut session = Session::new(true);
            assert_eq!(
                session.on_binary(bytes),
                Step::Close(4001, "the first frame must be Identify")
            );
        }
        assert_eq!(
            Session::new(true).on_text(),
            Step::Close(4001, "the first frame must be Identify")
        );
    }

    #[test]
    fn a_dev_identity_with_malformed_ids_closes_with_4001() {
        let mut session = Session::new(true);
        let bytes = encode_frame(Body::Identify(Identify {
            token: String::new(),
            dev_user_id: vec![1, 2, 3],
            dev_device_id: id_bytes(Uuid::now_v7()),
        }));
        assert_eq!(
            session.on_binary(&bytes),
            Step::Close(4001, "invalid dev identity")
        );
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
        let again = identify(Uuid::now_v7(), Uuid::now_v7());
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
}
```

Create `crates/push-gateway/src/lib.rs`:

```rust
//! push-gateway: terminates client WebSockets (spec §2). It decodes frames, forwards them
//! to message-svc over gRPC, and pushes new entries to connected devices when Redis rings
//! their user's channel. It holds no database credentials and no business rules.

mod config;
mod session;

pub use config::{Config, ConfigError};
pub use session::{Session, Step};
```

- [ ] **Step 4: Run the tests**

```powershell
cargo test -p push-gateway
```

Expected: 11 passed (4 `config`, 7 `session`).

- [ ] **Step 5: Run the gates**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 584 tests pass.

- [ ] **Step 6: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/push-gateway
git commit -m "feat(push-gateway): configuration and the session state machine"
```

---

### Task 7: gateway — the hub and its Redis subscriber

**Files:**
- Modify: `crates/push-gateway/Cargo.toml`, `src/lib.rs`
- Create: `crates/push-gateway/src/hub.rs`, `tests/hub.rs`

- [ ] **Step 1: Dependencies**

Replace `crates/push-gateway/Cargo.toml` with:

```toml
[package]
name = "push-gateway"
description = "The push-messenger gateway: client WebSockets in front of message-svc."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
prost.workspace = true
push-proto.workspace = true
redis.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "time"] }
tokio-stream.workspace = true
tokio-util.workspace = true
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["test-util"] }

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing Redis test**

Create `crates/push-gateway/tests/hub.rs`:

```rust
//! The hub's Redis subscriber against a real Redis (Docker must be running).

use std::time::Duration;

use prost::Message;
use push_gateway::Hub;
use push_proto::internal::v1::{EntriesAvailable, Notification, notification::Kind};
use push_proto::notify_channel;
use redis::AsyncCommands;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn redis() -> (ContainerAsync<Redis>, redis::Client) {
    let container = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start a Redis container (is Docker running?)");
    let port = container.get_host_port_ipv4(6379).await.unwrap();
    let client = redis::Client::open(format!("redis://127.0.0.1:{port}")).unwrap();
    (container, client)
}

async fn connected(hub: &Hub) {
    for _ in 0..100 {
        if hub.is_connected() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the hub did not connect to Redis within 5 s");
}

async fn subscribers(client: &redis::Client, channel: &str) -> i64 {
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
    let counts: Vec<(String, i64)> = redis::cmd("PUBSUB")
        .arg("NUMSUB")
        .arg(channel)
        .query_async(&mut connection)
        .await
        .unwrap();
    counts[0].1
}

#[tokio::test]
async fn a_notification_published_on_redis_rings_the_users_session() {
    let (_container, client) = redis().await;
    let shutdown = CancellationToken::new();
    let (hub, subscriber) = Hub::start(client.clone(), shutdown.clone());
    connected(&hub).await;

    let user = Uuid::now_v7();
    let mut session = hub.join(user).await;
    let payload = Notification {
        kind: Some(Kind::EntriesAvailable(EntriesAvailable {})),
    }
    .encode_to_vec();
    let mut publisher = client.get_multiplexed_async_connection().await.unwrap();
    let _: i64 = publisher
        .publish(notify_channel(user), payload)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), session.rung())
        .await
        .expect("the session is rung within 5 s");

    shutdown.cancel();
    subscriber.await.unwrap();
}

#[tokio::test]
async fn the_channel_is_unsubscribed_when_the_last_session_leaves() {
    let (_container, client) = redis().await;
    let shutdown = CancellationToken::new();
    let (hub, _subscriber) = Hub::start(client.clone(), shutdown.clone());
    connected(&hub).await;

    let user = Uuid::now_v7();
    let channel = notify_channel(user);
    let session = hub.join(user).await;
    assert_eq!(subscribers(&client, &channel).await, 1);
    drop(session);
    for _ in 0..100 {
        if subscribers(&client, &channel).await == 0 {
            shutdown.cancel();
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the channel is still subscribed 5 s after the last session left");
}
```

```powershell
cargo test -p push-gateway --test hub
```

Expected: a compile error — `unresolved import push_gateway::Hub`.

- [ ] **Step 3: The hub**

Create `crates/push-gateway/src/hub.rs`. Its unit tests build a hub without the
subscriber task and check the bookkeeping (they run on paused time, so the 2 s
subscription wait costs nothing):

```rust
//! Which connected devices to wake when Redis rings a user's channel.
//!
//! Every identified session joins the hub under its user. The first session of a user
//! makes the subscriber subscribe to `notify:{user_id}`; the last one to leave makes it
//! unsubscribe. A ring wakes every session of the user; rings coalesce, so a session that
//! is busy syncing syncs once more afterwards, not once per ring.
//!
//! Redis is a doorbell, not the truth (spec §2): whenever the subscription is
//! re-established after a break, every session is rung, since notifications may have been
//! lost meanwhile.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use prost::Message;
use push_proto::internal::v1::{Notification, notification::Kind};
use push_proto::notify_channel;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How long a joining session waits for its user's subscription to become active.
const SUBSCRIBE_WAIT: Duration = Duration::from_secs(2);
/// How long to wait before reconnecting to Redis.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// The sessions of this gateway, by user, and the Redis subscriber that wakes them.
#[derive(Clone)]
pub struct Hub {
    members: Arc<Mutex<Members>>,
    commands: mpsc::UnboundedSender<Command>,
    connected: Arc<AtomicBool>,
}

#[derive(Default)]
struct Members {
    next_key: u64,
    users: HashMap<Uuid, HashMap<u64, mpsc::Sender<()>>>,
}

enum Command {
    Subscribe(Uuid, oneshot::Sender<()>),
    Unsubscribe(Uuid),
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

    /// Adds a session of `user`. Waits up to 2 s for the user's subscription to become
    /// active, then returns anyway: the session syncs after joining, which covers
    /// whatever a late subscription misses.
    pub async fn join(&self, user: Uuid) -> Membership {
        let (doorbell, rung) = mpsc::channel(1);
        let (key, subscribed) = {
            let mut members = lock(&self.members);
            let key = members.next_key;
            members.next_key += 1;
            let sessions = members.users.entry(user).or_default();
            let first = sessions.is_empty();
            sessions.insert(key, doorbell);
            // Sent under the lock, so subscribe and unsubscribe commands for a user reach
            // the subscriber in the order the sessions joined and left.
            let subscribed = first.then(|| {
                let (done, subscribed) = oneshot::channel();
                let _ = self.commands.send(Command::Subscribe(user, done));
                subscribed
            });
            (key, subscribed)
        };
        if let Some(subscribed) = subscribed {
            let _ = tokio::time::timeout(SUBSCRIBE_WAIT, subscribed).await;
        }
        Membership {
            user,
            key,
            rung,
            hub: self.clone(),
        }
    }

    /// Wakes every session of `user`.
    fn ring(&self, user: Uuid) {
        if let Some(sessions) = lock(&self.members).users.get(&user) {
            for doorbell in sessions.values() {
                // A full doorbell has already been rung and not answered yet.
                let _ = doorbell.try_send(());
            }
        }
    }

    /// Wakes every session.
    fn ring_all(&self) {
        for sessions in lock(&self.members).users.values() {
            for doorbell in sessions.values() {
                let _ = doorbell.try_send(());
            }
        }
    }

    /// The users with at least one session.
    fn users(&self) -> Vec<Uuid> {
        lock(&self.members).users.keys().copied().collect()
    }

    /// Acts on a message from Redis: `EntriesAvailable` on `notify:{user_id}` rings the
    /// user's sessions. Other notifications reach clients in a later plan (receipts,
    /// contact changes, read positions); malformed ones are logged and dropped.
    fn dispatch(&self, channel: &str, payload: &[u8]) {
        let Some(user) = channel
            .strip_prefix("notify:")
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            tracing::warn!(channel, "message on an unexpected channel");
            return;
        };
        match Notification::decode(payload) {
            Ok(Notification {
                kind: Some(Kind::EntriesAvailable(_)),
            }) => self.ring(user),
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, channel, "malformed notification"),
        }
    }
}

fn lock(members: &Mutex<Members>) -> MutexGuard<'_, Members> {
    members.lock().expect("hub lock poisoned")
}

/// One session's place in the hub. Dropping it leaves.
pub struct Membership {
    user: Uuid,
    key: u64,
    rung: mpsc::Receiver<()>,
    hub: Hub,
}

impl Membership {
    /// Waits until the session's user is rung.
    pub async fn rung(&mut self) {
        if self.rung.recv().await.is_none() {
            // The hub keeps the sender while this membership exists; never reached.
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        let mut members = lock(&self.hub.members);
        if let Some(sessions) = members.users.get_mut(&self.user) {
            sessions.remove(&self.key);
            if sessions.is_empty() {
                members.users.remove(&self.user);
                let _ = self.hub.commands.send(Command::Unsubscribe(self.user));
            }
        }
    }
}

/// The subscriber task: keeps one Redis pub/sub connection, follows the hub's subscribe
/// and unsubscribe commands, and dispatches what arrives. Reconnects after a break,
/// subscribes again to every user with a session and rings them all.
async fn subscribe(
    client: redis::Client,
    hub: Hub,
    mut commands: mpsc::UnboundedReceiver<Command>,
    shutdown: CancellationToken,
) {
    loop {
        match client.get_async_pubsub().await {
            Ok(pubsub) => {
                let (mut sink, mut messages) = pubsub.split();
                let mut resubscribed = true;
                for user in hub.users() {
                    if sink.subscribe(notify_channel(user)).await.is_err() {
                        resubscribed = false;
                        break;
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
                                let result = match command {
                                    None => return,
                                    Some(Command::Subscribe(user, done)) => {
                                        let result = sink.subscribe(notify_channel(user)).await;
                                        let _ = done.send(());
                                        result
                                    }
                                    Some(Command::Unsubscribe(user)) => {
                                        sink.unsubscribe(notify_channel(user)).await
                                    }
                                };
                                if result.is_err() {
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
    use push_proto::internal::v1::{EntriesAvailable, ReadPositionChanged};

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

    fn rang(membership: &mut Membership) -> bool {
        membership.rung.try_recv().is_ok()
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_session_subscribes_and_the_last_unsubscribes() {
        let (hub, mut commands) = hub();
        let user = Uuid::now_v7();
        let first = hub.join(user).await;
        assert!(matches!(commands.try_recv(), Ok(Command::Subscribe(u, _)) if u == user));
        let second = hub.join(user).await;
        assert!(commands.try_recv().is_err(), "already subscribed");
        drop(first);
        assert!(commands.try_recv().is_err(), "a session is still there");
        drop(second);
        assert!(matches!(commands.try_recv(), Ok(Command::Unsubscribe(u)) if u == user));
        assert!(hub.users().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn entries_available_rings_only_that_users_sessions() {
        let (hub, _commands) = hub();
        let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
        let (mut alice_phone, mut alice_laptop) = (hub.join(alice).await, hub.join(alice).await);
        let mut bob_phone = hub.join(bob).await;
        hub.dispatch(&notify_channel(alice), &entries_available());
        assert!(rang(&mut alice_phone));
        assert!(rang(&mut alice_laptop));
        assert!(!rang(&mut bob_phone));
    }

    #[tokio::test(start_paused = true)]
    async fn rings_coalesce_until_answered() {
        let (hub, _commands) = hub();
        let user = Uuid::now_v7();
        let mut session = hub.join(user).await;
        for _ in 0..3 {
            hub.dispatch(&notify_channel(user), &entries_available());
        }
        assert!(rang(&mut session));
        assert!(!rang(&mut session));
    }

    #[tokio::test(start_paused = true)]
    async fn other_notifications_and_strange_channels_ring_nobody() {
        let (hub, _commands) = hub();
        let user = Uuid::now_v7();
        let mut session = hub.join(user).await;
        let read = Notification {
            kind: Some(Kind::ReadPositionChanged(ReadPositionChanged::default())),
        };
        hub.dispatch(&notify_channel(user), &read.encode_to_vec());
        hub.dispatch("notify:not-a-uuid", &entries_available());
        hub.dispatch(&format!("other:{user}"), &entries_available());
        hub.dispatch(&notify_channel(user), &[0xff, 0xff]);
        assert!(!rang(&mut session));
    }

    #[tokio::test(start_paused = true)]
    async fn ring_all_wakes_every_session() {
        let (hub, _commands) = hub();
        let mut a = hub.join(Uuid::now_v7()).await;
        let mut b = hub.join(Uuid::now_v7()).await;
        hub.ring_all();
        assert!(rang(&mut a));
        assert!(rang(&mut b));
    }
}
```

Replace `crates/push-gateway/src/lib.rs` with:

```rust
//! push-gateway: terminates client WebSockets (spec §2). It decodes frames, forwards them
//! to message-svc over gRPC, and pushes new entries to connected devices when Redis rings
//! their user's channel. It holds no database credentials and no business rules.

mod config;
mod hub;
mod session;

pub use config::{Config, ConfigError};
pub use hub::{Hub, Membership};
pub use session::{Session, Step};
```

- [ ] **Step 4: Run the tests**

```powershell
cargo test -p push-gateway
```

Expected: unit tests 16 passed (5 new `hub`); `hub` 2 passed.

- [ ] **Step 5: Run the gates**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 591 tests pass.

- [ ] **Step 6: Commit**

```powershell
git add Cargo.lock crates/push-gateway
git commit -m "feat(push-gateway): hub of sessions rung through Redis"
```

---

### Task 8: gateway — WebSockets, delivery, health and shutdown

**Files:**
- Modify: `Cargo.toml`, `crates/push-gateway/Cargo.toml`, `src/lib.rs`, `src/session.rs`
- Create: `crates/push-gateway/src/server.rs`, `src/main.rs`, `tests/gateway.rs`

- [ ] **Step 1: Dependencies**

Replace `Cargo.toml` with:

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-domain",
    "crates/push-gateway",
    "crates/push-message-svc",
    "crates/push-proto",
    "crates/push-store",
    "crates/push-testkit",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "MIT"
repository = "https://github.com/yury-opolev/push-messenger"

[workspace.dependencies]
anyhow = "1"
async-trait = "0.1"
axum = "0.8"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
futures-util = { version = "0.3", default-features = false, features = ["sink"] }
libtest-mimic = "0.8"
prost = "0.14"
protoc-bin-vendored = "3"
push-domain = { path = "crates/push-domain" }
push-gateway = { path = "crates/push-gateway" }
push-message-svc = { path = "crates/push-message-svc" }
push-proto = { path = "crates/push-proto" }
push-store = { path = "crates/push-store" }
push-testkit = { path = "crates/push-testkit" }
redis = { version = "1", default-features = false, features = [
    "connection-manager",
    "tokio-comp",
] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sqlx = { version = "0.8", default-features = false, features = [
    "chrono",
    "macros",
    "migrate",
    "postgres",
    "runtime-tokio",
    "uuid",
] }
testcontainers-modules = { version = "0.13", features = ["postgres", "redis"] }
thiserror = "2"
tokio = { version = "1", features = ["sync"] }
tokio-stream = { version = "0.1", features = ["net"] }
tokio-tungstenite = "0.29"
tokio-util = { version = "0.7", features = ["rt"] }
tonic = "0.14"
tonic-prost = "0.14"
tonic-prost-build = "0.14"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
uuid = { version = "1", features = ["serde", "v4", "v7"] }

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

Replace `crates/push-gateway/Cargo.toml` with:

```toml
[package]
name = "push-gateway"
description = "The push-messenger gateway: client WebSockets in front of message-svc."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
anyhow.workspace = true
axum = { workspace = true, features = ["ws"] }
prost.workspace = true
push-proto.workspace = true
redis.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
tokio-stream.workspace = true
tokio-util.workspace = true
tonic.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
uuid.workspace = true

[dev-dependencies]
futures-util.workspace = true
push-domain.workspace = true
push-message-svc.workspace = true
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["test-util"] }
tokio-tungstenite.workspace = true

[lints]
workspace = true
```

`push-message-svc` and `push-domain` are dev-dependencies only: the tests run
message-svc's gRPC services on the in-memory store, so they need Redis but no Postgres.
The gateway itself never depends on them.

- [ ] **Step 2: Write the failing WebSocket tests**

Create `crates/push-gateway/tests/gateway.rs`:

```rust
//! The gateway over real WebSockets, in front of message-svc's API (on the in-memory
//! store) and a real Redis (Docker must be running).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use push_domain::{InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock};
use push_gateway::{Config, serve};
use push_message_svc::{DevSetupApi, MessagingApi, RedisNotifier};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::dev_setup_server::DevSetupServer;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::messaging_server::{self, MessagingServer};
use push_proto::internal::v1::{
    AckRequest, AckResponse, IdentifyRequest, IdentifyResponse, MakeContactsRequest,
    OpenDirectRequest, RegisterDeviceRequest, RegisterUserRequest, SendRequest, SyncRequest,
    SyncResponse,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, ContentType, Identify, Ping, Pong, Send, SendAck};
use push_proto::{decode_frame, encode_frame, id_bytes};
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Api = MessagingApi<InMemoryStore, SystemClock, RedisNotifier>;

/// message-svc's `Messaging` service, whose first `sync_failures` syncs fail as if the
/// service were restarting.
struct FlakySync {
    api: Api,
    sync_failures: Mutex<u32>,
}

#[tonic::async_trait]
impl messaging_server::Messaging for FlakySync {
    async fn identify(
        &self,
        request: Request<IdentifyRequest>,
    ) -> Result<Response<IdentifyResponse>, Status> {
        self.api.identify(request).await
    }

    async fn send(&self, request: Request<SendRequest>) -> Result<Response<SendAck>, Status> {
        self.api.send(request).await
    }

    async fn sync(&self, request: Request<SyncRequest>) -> Result<Response<SyncResponse>, Status> {
        let failing = {
            let mut left = self.sync_failures.lock().unwrap();
            let failing = *left > 0;
            *left = left.saturating_sub(1);
            failing
        };
        if failing {
            return Err(Status::unavailable("message-svc is restarting"));
        }
        self.api.sync(request).await
    }

    async fn ack(&self, request: Request<AckRequest>) -> Result<Response<AckResponse>, Status> {
        self.api.ack(request).await
    }
}

struct Stack {
    _redis: ContainerAsync<Redis>,
    message_svc: String,
    gateway: SocketAddr,
    shutdown: CancellationToken,
    server: JoinHandle<anyhow::Result<()>>,
}

/// Redis, message-svc's API on the in-memory store, and the gateway, on free ports.
async fn stack(dev_auth: bool) -> Stack {
    stack_with(dev_auth, 0).await
}

/// As [`stack`], with message-svc failing its first `sync_failures` syncs.
async fn stack_with(dev_auth: bool, sync_failures: u32) -> Stack {
    let redis = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start Redis (is Docker running?)");
    let redis_url = format!(
        "redis://127.0.0.1:{}",
        redis.get_host_port_ipv4(6379).await.unwrap()
    );

    let notifier = RedisNotifier::connect(&redis_url).await.unwrap();
    let messaging = Arc::new(Messaging::new(
        InMemoryStore::default(),
        SystemClock,
        notifier,
    ));
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc = format!("http://{}", grpc.local_addr().unwrap());
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(MessagingServer::new(FlakySync {
                api: MessagingApi::new(Arc::clone(&messaging)),
                sync_failures: Mutex::new(sync_failures),
            }))
            .add_service(DevSetupServer::new(DevSetupApi::new(messaging)))
            .serve_with_incoming(TcpListenerStream::new(grpc)),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = listener.local_addr().unwrap();
    let config = Config {
        message_svc_url: message_svc.clone(),
        redis_url,
        listen_addr: gateway,
        health_addr: health.local_addr().unwrap(),
        heartbeat: Duration::from_secs(30),
        dev_auth,
    };
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(config, listener, health, shutdown.clone()));
    Stack {
        _redis: redis,
        message_svc,
        gateway,
        shutdown,
        server,
    }
}

/// A user with one device, registered through the dev setup API.
async fn person(stack: &Stack) -> (Uuid, Uuid) {
    let mut setup = DevSetupClient::connect(stack.message_svc.clone())
        .await
        .unwrap();
    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
    setup
        .register_user(RegisterUserRequest {
            user_id: id_bytes(user),
        })
        .await
        .unwrap();
    setup
        .register_device(RegisterDeviceRequest {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
        })
        .await
        .unwrap();
    (user, device)
}

async fn connect(stack: &Stack) -> Socket {
    let (socket, _) = connect_async(format!("ws://{}/v1/socket", stack.gateway))
        .await
        .expect("connect to the gateway");
    socket
}

async fn send(socket: &mut Socket, body: Body) {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .unwrap();
}

fn identify(user: Uuid, device: Uuid) -> Body {
    Body::Identify(Identify {
        token: String::new(),
        dev_user_id: id_bytes(user),
        dev_device_id: id_bytes(device),
    })
}

/// The next frame, within 5 s.
async fn next(socket: &mut Socket) -> Body {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("a frame within 5 s")
            .expect("the connection is open")
            .expect("a WebSocket message");
        match message {
            Message::Binary(bytes) => return decode_frame(&bytes).expect("a valid frame"),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("expected a frame, got {other:?}"),
        }
    }
}

/// The close code the gateway sends, within 10 s.
async fn close_code(socket: &mut Socket) -> u16 {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a close within 10 s");
        match message {
            Some(Ok(Message::Close(Some(frame)))) => return frame.code.into(),
            Some(Ok(_)) => {}
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_message_reaches_the_other_device_and_the_sender_gets_its_ack() {
    let stack = stack(true).await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    let mut setup = DevSetupClient::connect(stack.message_svc.clone())
        .await
        .unwrap();
    setup
        .make_contacts(MakeContactsRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap();
    let conversation = setup
        .open_direct(OpenDirectRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap()
        .into_inner()
        .conversation_id;

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
        &mut alice_socket,
        Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.clone(),
            content_type: ContentType::Text.into(),
            body: b"hello Bob".to_vec(),
            client_ts: None,
        }),
    )
    .await;
    let ack = loop {
        // Alice's own device is also told about her message.
        match next(&mut alice_socket).await {
            Body::SendAck(ack) => break ack,
            Body::Deliver(_) => {}
            other => panic!("expected SendAck, got {other:?}"),
        }
    };
    assert_eq!(ack.client_msg_id, client_msg_id);
    assert_eq!(ack.seq, 1);

    let Body::Deliver(entry) = next(&mut bob_socket).await else {
        panic!("expected Deliver");
    };
    assert_eq!(entry.conversation_id, conversation);
    assert_eq!(entry.seq, 1);
    assert_eq!(entry.sender_id, id_bytes(alice));
    assert_eq!(entry.body.as_deref(), Some(b"hello Bob".as_slice()));

    send(
        &mut bob_socket,
        Body::Ack(Ack {
            conversation_id: conversation,
            seq: 1,
        }),
    )
    .await;
    // Frames are handled in order: a Pong after the Ack means the Ack succeeded.
    send(&mut bob_socket, Body::Ping(Ping {})).await;
    assert_eq!(next(&mut bob_socket).await, Body::Pong(Pong {}));
}

#[tokio::test]
async fn without_dev_auth_identify_is_closed_with_4001() {
    let stack = stack(false).await;
    let (user, device) = person(&stack).await;
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(user, device)).await;
    assert_eq!(close_code(&mut socket).await, 4001);
}

#[tokio::test]
async fn an_unknown_device_is_closed_with_4001() {
    let stack = stack(true).await;
    let (user, _device) = person(&stack).await;
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(user, Uuid::now_v7())).await;
    assert_eq!(close_code(&mut socket).await, 4001);
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
    assert_eq!(close_code(&mut socket).await, 4005);
    tokio::time::timeout(Duration::from_secs(15), stack.server)
        .await
        .expect("the gateway stops within 15 s")
        .unwrap()
        .expect("a clean stop");
}

/// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
/// messages from Alice in it, sent through message-svc before anyone connects.
async fn chat_with_backlog(stack: &Stack, count: u32) -> ((Uuid, Uuid), (Uuid, Uuid), Vec<u8>) {
    let (alice, alice_device) = person(stack).await;
    let (bob, bob_device) = person(stack).await;
    let mut setup = DevSetupClient::connect(stack.message_svc.clone())
        .await
        .unwrap();
    setup
        .make_contacts(MakeContactsRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap();
    let conversation = setup
        .open_direct(OpenDirectRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap()
        .into_inner()
        .conversation_id;
    let mut messaging = MessagingClient::connect(stack.message_svc.clone())
        .await
        .unwrap();
    for n in 0..count {
        messaging
            .send(SendRequest {
                user_id: id_bytes(alice),
                device_id: id_bytes(alice_device),
                send: Some(Send {
                    client_msg_id: id_bytes(Uuid::now_v7()),
                    conversation_id: conversation.clone(),
                    content_type: ContentType::Text.into(),
                    body: format!("message {n}").into_bytes(),
                    client_ts: None,
                }),
            })
            .await
            .unwrap();
    }
    ((alice, alice_device), (bob, bob_device), conversation)
}

#[tokio::test]
async fn a_backlog_larger_than_one_sync_batch_is_delivered_as_the_device_acks() {
    let stack = stack(true).await;
    let backlog = MAX_SYNC_BATCH + 1;
    let (_alice, (bob, bob_device), conversation) = chat_with_backlog(&stack, backlog).await;

    let mut socket = connect(&stack).await;
    send(&mut socket, identify(bob, bob_device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
    for expected in 1..=i64::from(MAX_SYNC_BATCH) {
        let Body::Deliver(entry) = next(&mut socket).await else {
            panic!("expected Deliver {expected}");
        };
        assert_eq!(entry.seq, expected);
    }

    // Nothing rings Bob's channel again: only his ack can bring the rest.
    send(
        &mut socket,
        Body::Ack(Ack {
            conversation_id: conversation,
            seq: i64::from(MAX_SYNC_BATCH),
        }),
    )
    .await;
    let Body::Deliver(entry) = next(&mut socket).await else {
        panic!("expected the last Deliver");
    };
    assert_eq!(entry.seq, i64::from(backlog));
}

#[tokio::test]
async fn a_failed_sync_is_retried() {
    let stack = stack_with(true, 1).await;
    let (_alice, (bob, bob_device), _conversation) = chat_with_backlog(&stack, 1).await;

    let mut socket = connect(&stack).await;
    send(&mut socket, identify(bob, bob_device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
    // The sync after Ready fails; nothing rings Bob's channel, so only a retry delivers.
    let Body::Deliver(entry) = next(&mut socket).await else {
        panic!("expected Deliver");
    };
    assert_eq!(entry.seq, 1);
}
```

The last two tests guard delivery that no Redis ring triggers: a backlog of
`MAX_SYNC_BATCH + 1` messages must flow on as Bob acks, and a sync that fails once (the
`FlakySync` wrapper around message-svc's API) must be retried.

```powershell
cargo test -p push-gateway --test gateway
```

Expected: compile errors — `push_gateway::serve` does not exist.

- [ ] **Step 3: Teach the session when to sync again**

Replace `crates/push-gateway/src/session.rs` with the version below. Against Task 6 it
adds `more_waiting` with `synced` and `sync_after_ack`, the `RetryDelay` backoff, and
their two tests:

```rust
//! The protocol state of one WebSocket connection, without any I/O: which frames are
//! allowed when, what each one asks the gateway to do, which entries the client has
//! already been sent, and when to sync again.

use std::collections::HashMap;
use std::time::Duration;

use push_proto::close;
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, Deliver, Error, ErrorCode, Pong, Ready, Send};
use push_proto::{FrameError, decode_frame, id, id_bytes};
use uuid::Uuid;

/// What the gateway must do for a frame the client sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Send this frame to the client.
    Reply(Body),
    /// Ask message-svc whether `device` is an active device of `user`, then call
    /// [`Session::identified`] or close with [`close::INVALID_IDENTITY`].
    Identify { user: Uuid, device: Uuid },
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
    /// `Identify` is being checked.
    Identifying,
    Ready {
        device: Uuid,
    },
}

#[derive(Debug)]
pub struct Session {
    dev_auth: bool,
    state: State,
    /// The highest `seq` sent to the client per conversation, so a sync that repeats
    /// entries the client has not acked yet does not send them twice.
    sent: HashMap<Vec<u8>, i64>,
    /// The last sync left entries behind (`more`). They come only after an ack, which
    /// rings nobody, so the session must sync again itself.
    more_waiting: bool,
}

impl Session {
    /// A new connection. `dev_auth` is whether DEV ONLY identities are accepted.
    pub fn new(dev_auth: bool) -> Self {
        Self {
            dev_auth,
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
                return Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify");
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
                if !self.dev_auth {
                    return Step::Close(
                        close::INVALID_IDENTITY,
                        "token sign-in is not available yet",
                    );
                }
                match (id(&identify.dev_user_id), id(&identify.dev_device_id)) {
                    (Ok(user), Ok(device)) => {
                        self.state = State::Identifying;
                        Step::Identify { user, device }
                    }
                    _ => Step::Close(close::INVALID_IDENTITY, "invalid dev identity"),
                }
            }
            (_, State::New | State::Identifying) => {
                Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify")
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
                Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify")
            }
        }
    }

    /// message-svc confirmed the identity: the `Ready` frame to send.
    pub fn identified(&mut self, user: Uuid, device: Uuid, heartbeat: Duration) -> Body {
        self.state = State::Ready { device };
        Body::Ready(Ready {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
            heartbeat_ms: u32::try_from(heartbeat.as_millis()).unwrap_or(u32::MAX),
        })
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
    })
}

fn reply_error(code: ErrorCode, message: &str) -> Step {
    Step::Reply(error_frame(code, message))
}

#[cfg(test)]
mod tests {
    use push_proto::encode_frame;
    use push_proto::v1::{Identify, Ping};

    use super::*;

    fn identify(user: Uuid, device: Uuid) -> Vec<u8> {
        encode_frame(Body::Identify(Identify {
            token: String::new(),
            dev_user_id: id_bytes(user),
            dev_device_id: id_bytes(device),
        }))
    }

    fn ready_session() -> (Session, Uuid) {
        let mut session = Session::new(true);
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(
            session.on_binary(&identify(user, device)),
            Step::Identify { user, device }
        );
        session.identified(user, device, Duration::from_secs(30));
        (session, device)
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
    fn a_dev_identity_is_checked_then_ready() {
        let mut session = Session::new(true);
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(
            session.on_binary(&identify(user, device)),
            Step::Identify { user, device }
        );
        assert_eq!(session.device(), None);
        let ready = session.identified(user, device, Duration::from_secs(30));
        assert_eq!(
            ready,
            Body::Ready(Ready {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
                heartbeat_ms: 30_000,
            })
        );
        assert_eq!(session.device(), Some(device));
    }

    #[test]
    fn without_dev_auth_identify_closes_with_4001() {
        let mut session = Session::new(false);
        assert_eq!(
            session.on_binary(&identify(Uuid::now_v7(), Uuid::now_v7())),
            Step::Close(4001, "token sign-in is not available yet")
        );
    }

    #[test]
    fn anything_but_identify_first_closes_with_4001() {
        let ping = encode_frame(Body::Ping(Ping {}));
        for bytes in [ping.as_slice(), &[0xff, 0xff], &[]] {
            let mut session = Session::new(true);
            assert_eq!(
                session.on_binary(bytes),
                Step::Close(4001, "the first frame must be Identify")
            );
        }
        assert_eq!(
            Session::new(true).on_text(),
            Step::Close(4001, "the first frame must be Identify")
        );
    }

    #[test]
    fn a_dev_identity_with_malformed_ids_closes_with_4001() {
        let mut session = Session::new(true);
        let bytes = encode_frame(Body::Identify(Identify {
            token: String::new(),
            dev_user_id: vec![1, 2, 3],
            dev_device_id: id_bytes(Uuid::now_v7()),
        }));
        assert_eq!(
            session.on_binary(&bytes),
            Step::Close(4001, "invalid dev identity")
        );
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
        let again = identify(Uuid::now_v7(), Uuid::now_v7());
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

- [ ] **Step 4: The server**

Create `crates/push-gateway/src/server.rs`. Every sync goes through `Connected::deliver`,
which records `more` in the session and, on failure, schedules a retry that the
connection's `select!` loop waits for:

```rust
//! Running the gateway: the WebSocket endpoint, one task per connection, the health
//! endpoints and graceful shutdown.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{AckRequest, IdentifyRequest, SendRequest, SyncRequest};
use push_proto::v1::ErrorCode;
use push_proto::v1::frame::Body;
use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

use crate::config::Config;
use crate::hub::Hub;
use crate::session::{RetryDelay, Session, Step, error_frame};

/// The first frame must be `Identify`, within this time (spec §7.1).
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long shutdown waits for connections to close.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Close code for "the server cannot serve this connection now" (RFC 6455).
const INTERNAL_ERROR: u16 = 1011;

#[derive(Clone)]
struct App {
    config: Arc<Config>,
    messaging: MessagingClient<Channel>,
    hub: Hub,
    sessions: TaskTracker,
    shutdown: CancellationToken,
}

/// The connection was closed, or must be.
struct Closed;

/// Runs the gateway until `shutdown` is cancelled. Then it stops accepting connections,
/// closes every open one with 4005, and waits up to 10 s for them to finish.
pub async fn serve(
    config: Config,
    listener: TcpListener,
    health: TcpListener,
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
        .timeout(Duration::from_secs(10));
    let messaging = MessagingClient::new(endpoint.connect_lazy());
    let (hub, subscriber) = Hub::start(
        redis::Client::open(config.redis_url.as_str())?,
        shutdown.clone(),
    );
    let sessions = TaskTracker::new();
    let listen_addr = config.listen_addr;
    let app = App {
        config: Arc::new(config),
        messaging,
        hub: hub.clone(),
        sessions: sessions.clone(),
        shutdown: shutdown.clone(),
    };

    let health_server = tokio::spawn(
        axum::serve(health, health_router(shutdown.clone()))
            .with_graceful_shutdown(shutdown.clone().cancelled_owned())
            .into_future(),
    );
    let router = Router::new()
        .route("/v1/socket", get(upgrade))
        .with_state(app);
    tracing::info!(addr = %listen_addr, "serving WebSockets");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await?;

    // Upgraded connections outlive the HTTP server; each closes itself with 4005.
    sessions.close();
    if tokio::time::timeout(DRAIN_TIMEOUT, sessions.wait())
        .await
        .is_err()
    {
        tracing::warn!(open = sessions.len(), "connections still open after 10 s");
    }
    subscriber.await?;
    health_server.await??;
    tracing::info!("stopped");
    Ok(())
}

/// `/healthz` always answers; `/readyz` answers until shutdown begins. Readiness does not
/// depend on Redis: without it devices still get their entries, only later (after
/// `Ready`, after acks and on retries), while an unready gateway would refuse every
/// client. The subscriber reconnects by itself, so liveness does not depend on it either.
fn health_router(shutdown: CancellationToken) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/readyz",
            get(move || async move {
                if shutdown.is_cancelled() {
                    (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
                } else {
                    (StatusCode::OK, "ready")
                }
            }),
        )
}

async fn upgrade(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response {
    let sessions = app.sessions.clone();
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| sessions.track_future(connection(socket, app)))
}

/// One client connection, from `Identify` to close.
async fn connection(mut socket: WebSocket, app: App) {
    let mut session = Session::new(app.config.dev_auth);
    let Some((user, device)) = identify(&mut socket, &mut session, &app).await else {
        return;
    };
    let mut membership = app.hub.join(user).await;
    let ready = session.identified(user, device, app.config.heartbeat);
    let mut connected = Connected {
        socket,
        session,
        messaging: app.messaging.clone(),
        user,
        device,
        retry: RetryDelay::default(),
        retry_at: None,
    };
    if send(&mut connected.socket, ready).await.is_err() || connected.deliver().await.is_err() {
        return;
    }
    loop {
        let retry_at = connected.retry_at;
        tokio::select! {
            () = app.shutdown.cancelled() => {
                close_with(&mut connected.socket, close::SHUTTING_DOWN, "server shutting down").await;
                return;
            }
            () = membership.rung() => {
                if connected.deliver().await.is_err() {
                    return;
                }
            }
            // The precondition disables the branch; the far-away instant is never awaited.
            () = tokio::time::sleep_until(retry_at.unwrap_or_else(far_future)), if retry_at.is_some() => {
                if connected.deliver().await.is_err() {
                    return;
                }
            }
            message = connected.socket.recv() => {
                let step = match message {
                    Some(Ok(Message::Binary(bytes))) => connected.session.on_binary(&bytes),
                    Some(Ok(Message::Text(_))) => connected.session.on_text(),
                    // WebSocket pings are answered by the library.
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                };
                if connected.run(step).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(24 * 60 * 60)
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
    let step = loop {
        tokio::select! {
            () = &mut deadline => {
                close_with(socket, close::INVALID_IDENTITY, "Identify must come within 5 s").await;
                return None;
            }
            () = app.shutdown.cancelled() => {
                close_with(socket, close::SHUTTING_DOWN, "server shutting down").await;
                return None;
            }
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(bytes))) => break session.on_binary(&bytes),
                Some(Ok(Message::Text(_))) => break session.on_text(),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
            },
        }
    };
    let (user, device) = match step {
        Step::Identify { user, device } => (user, device),
        Step::Close(code, reason) => {
            close_with(socket, code, reason).await;
            return None;
        }
        // Before Ready the session only asks to identify or to close.
        Step::Reply(_) | Step::Send(_) | Step::Ack(_) => {
            close_with(
                socket,
                close::INVALID_IDENTITY,
                "the first frame must be Identify",
            )
            .await;
            return None;
        }
    };
    let request = IdentifyRequest {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
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
}

/// An identified connection.
struct Connected {
    socket: WebSocket,
    session: Session,
    messaging: MessagingClient<Channel>,
    user: Uuid,
    device: Uuid,
    retry: RetryDelay,
    /// When to retry a failed sync; `None` while the last sync succeeded.
    retry_at: Option<Instant>,
}

impl Connected {
    /// Does what the session asked for a client frame.
    async fn run(&mut self, step: Step) -> Result<(), Closed> {
        match step {
            Step::Reply(body) => send(&mut self.socket, body).await,
            Step::Send(frame) => {
                let client_msg_id = frame.client_msg_id.clone();
                let request = SendRequest {
                    user_id: id_bytes(self.user),
                    device_id: id_bytes(self.device),
                    send: Some(frame),
                };
                let body = match self.messaging.send(request).await {
                    Ok(ack) => Body::SendAck(ack.into_inner()),
                    Err(status) => {
                        let mut error = error_from_status(&status);
                        error.client_msg_id = Some(client_msg_id);
                        Body::Error(error)
                    }
                };
                send(&mut self.socket, body).await
            }
            Step::Ack(ack) => {
                let request = AckRequest {
                    device_id: id_bytes(self.device),
                    ack: Some(ack),
                };
                match self.messaging.ack(request).await {
                    // An ack rings nobody: if the last sync left entries behind, this is
                    // the moment to fetch them.
                    Ok(response) if self.session.sync_after_ack(response.get_ref().moved) => {
                        self.deliver().await
                    }
                    Ok(_) => Ok(()),
                    Err(status) => {
                        let error = Body::Error(error_from_status(&status));
                        send(&mut self.socket, error).await
                    }
                }
            }
            Step::Close(code, reason) => {
                close_with(&mut self.socket, code, reason).await;
                Err(Closed)
            }
            Step::Identify { .. } => {
                let error = error_frame(ErrorCode::BadFrame, "already identified");
                send(&mut self.socket, error).await
            }
        }
    }

    /// Pushes every entry the device has not been sent yet. A failed sync is retried
    /// after [`RetryDelay`], from the connection's loop.
    async fn deliver(&mut self) -> Result<(), Closed> {
        loop {
            let request = SyncRequest {
                device_id: id_bytes(self.device),
                limit: 0,
            };
            let batch = match self.messaging.sync(request).await {
                Ok(batch) => batch.into_inner(),
                Err(status) => {
                    let delay = self.retry.failed();
                    self.retry_at = Some(Instant::now() + delay);
                    tracing::warn!(
                        code = ?status.code(),
                        message = status.message(),
                        retry_in = ?delay,
                        "sync failed"
                    );
                    return Ok(());
                }
            };
            self.retry.succeeded();
            self.retry_at = None;
            self.session.synced(batch.more);
            let fresh = self.session.fresh(batch.entries);
            let progressed = !fresh.is_empty();
            for entry in fresh {
                send(&mut self.socket, Body::Deliver(entry)).await?;
            }
            // Without acks a repeated sync returns the same batch: stop here. The ack that
            // moves the position syncs again (`Session::sync_after_ack`).
            if !(batch.more && progressed) {
                return Ok(());
            }
        }
    }
}

async fn send(socket: &mut WebSocket, body: Body) -> Result<(), Closed> {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .map_err(|_| Closed)
}

async fn close_with(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = socket.send(Message::Close(Some(frame))).await;
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

`init_logging` and `terminated` are the same as message-svc's; the two binaries share no
runtime crate yet (see Deferred).

Create `crates/push-gateway/src/main.rs`:

```rust
use push_gateway::{Config, init_logging, serve, terminated};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    let config = Config::from_env()?;
    let listener = TcpListener::bind(config.listen_addr).await?;
    let health = TcpListener::bind(config.health_addr).await?;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        terminated().await;
        tracing::info!("shutting down");
        signal.cancel();
    });
    serve(config, listener, health, shutdown).await
}
```

Replace `crates/push-gateway/src/lib.rs` with:

```rust
//! push-gateway: terminates client WebSockets (spec §2). It decodes frames, forwards them
//! to message-svc over gRPC, and pushes new entries to connected devices when Redis rings
//! their user's channel. It holds no database credentials and no business rules.

mod config;
mod hub;
mod server;
mod session;

pub use config::{Config, ConfigError};
pub use hub::{Hub, Membership};
pub use server::{init_logging, serve, terminated};
pub use session::{RetryDelay, Session, Step};
```

- [ ] **Step 5: Run the tests**

```powershell
cargo test -p push-gateway
```

Expected: unit tests 18 (2 new `session`), `gateway` 7 (about 6 s: one test waits out
the 5 s `Identify` deadline), `hub` 2. The two delivery tests fail without their fixes:
with the `Ok(response) if self.session.sync_after_ack(…)` arm removed, the backlog test
waits in vain for seq 501; with the retry branch of the `select!` disabled, the retry
test waits in vain for its `Deliver`.

- [ ] **Step 6: Run the gates**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 600 tests pass.

- [ ] **Step 7: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/push-gateway
git commit -m "feat(push-gateway): WebSocket sessions in front of message-svc"
```

---

### Task 9: `push-smoke` — the end-to-end smoke test

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/push-smoke/Cargo.toml`, `src/lib.rs`, `src/main.rs`, `tests/local.rs`

- [ ] **Step 1: Add the crate**

```powershell
New-Item -ItemType Directory crates\push-smoke\src, crates\push-smoke\tests -Force
```

Replace `Cargo.toml` with:

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-domain",
    "crates/push-gateway",
    "crates/push-message-svc",
    "crates/push-proto",
    "crates/push-smoke",
    "crates/push-store",
    "crates/push-testkit",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "MIT"
repository = "https://github.com/yury-opolev/push-messenger"

[workspace.dependencies]
anyhow = "1"
async-trait = "0.1"
axum = "0.8"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
futures-util = { version = "0.3", default-features = false, features = ["sink"] }
libtest-mimic = "0.8"
prost = "0.14"
protoc-bin-vendored = "3"
push-domain = { path = "crates/push-domain" }
push-gateway = { path = "crates/push-gateway" }
push-message-svc = { path = "crates/push-message-svc" }
push-proto = { path = "crates/push-proto" }
push-store = { path = "crates/push-store" }
push-testkit = { path = "crates/push-testkit" }
redis = { version = "1", default-features = false, features = [
    "connection-manager",
    "tokio-comp",
] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sqlx = { version = "0.8", default-features = false, features = [
    "chrono",
    "macros",
    "migrate",
    "postgres",
    "runtime-tokio",
    "uuid",
] }
testcontainers-modules = { version = "0.13", features = ["postgres", "redis"] }
thiserror = "2"
tokio = { version = "1", features = ["sync"] }
tokio-stream = { version = "0.1", features = ["net"] }
tokio-tungstenite = "0.29"
tokio-util = { version = "0.7", features = ["rt"] }
tonic = "0.14"
tonic-prost = "0.14"
tonic-prost-build = "0.14"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
uuid = { version = "1", features = ["serde", "v4", "v7"] }

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

Create `crates/push-smoke/Cargo.toml`:

```toml
[package]
name = "push-smoke"
description = "End-to-end smoke test of a running push-messenger deployment."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
anyhow.workspace = true
futures-util.workspace = true
push-proto.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "time"] }
tokio-tungstenite.workspace = true
tonic.workspace = true
uuid.workspace = true

[dev-dependencies]
push-gateway.workspace = true
push-message-svc.workspace = true
push-store.workspace = true
testcontainers-modules.workspace = true
tokio-util.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing whole-stack test**

Create `crates/push-smoke/tests/local.rs`. It starts Postgres and Redis, runs message-svc
(on Postgres) and the gateway in-process, and runs the smoke test against them:

```rust
//! The walking skeleton on this machine: Postgres and Redis in containers (Docker must be
//! running), message-svc and the gateway in this process, and the smoke test against
//! them over real gRPC and WebSockets.

use std::time::Duration;

use push_smoke::{Target, run};
use push_store::StoreOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn the_smoke_test_passes_against_the_whole_stack() {
    let postgres = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start Postgres (is Docker running?)");
    let redis = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start Redis");
    let database_url = format!(
        "postgres://postgres:postgres@127.0.0.1:{}/postgres",
        postgres.get_host_port_ipv4(5432).await.unwrap()
    );
    let redis_url = format!(
        "redis://127.0.0.1:{}",
        redis.get_host_port_ipv4(6379).await.unwrap()
    );
    let shutdown = CancellationToken::new();

    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_url = format!("http://{}", grpc.local_addr().unwrap());
    let message_svc = tokio::spawn(push_message_svc::serve(
        push_message_svc::Config {
            database_url,
            redis_url: redis_url.clone(),
            grpc_addr: grpc.local_addr().unwrap(),
            health_addr: message_svc_health.local_addr().unwrap(),
            store: StoreOptions::default(),
            dev_setup: true,
        },
        grpc,
        message_svc_health,
        shutdown.clone(),
    ));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket_url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
    let gateway = tokio::spawn(push_gateway::serve(
        push_gateway::Config {
            message_svc_url: message_svc_url.clone(),
            redis_url,
            listen_addr: listener.local_addr().unwrap(),
            health_addr: gateway_health.local_addr().unwrap(),
            heartbeat: Duration::from_secs(30),
            dev_auth: true,
        },
        listener,
        gateway_health,
        shutdown.clone(),
    ));

    // message-svc migrates the database before it serves; the smoke test's first call
    // waits for it.
    let target = Target {
        setup_url: message_svc_url,
        socket_url,
        host: None,
    };
    let mut result = run(&target).await;
    for _ in 0..20 {
        if result.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        result = run(&target).await;
    }
    result.expect("the smoke test passes");

    shutdown.cancel();
    message_svc
        .await
        .unwrap()
        .expect("message-svc stops cleanly");
    gateway.await.unwrap().expect("the gateway stops cleanly");
}
```

- [ ] **Step 3: The smoke test**

Create `crates/push-smoke/src/lib.rs`:

```rust
//! push-smoke: the end-to-end smoke test of a running push-messenger.
//!
//! Against a deployment it:
//!
//! 1. creates Alice and Bob, one device each, as accepted contacts with a direct
//!    conversation, through message-svc's DEV ONLY `DevSetup` API;
//! 2. connects Bob's WebSocket and then Alice's, each with a DEV ONLY `Identify`, and
//!    waits for `Ready`;
//! 3. sends a message from Alice and waits for her `SendAck`;
//! 4. waits for Bob's `Deliver` of that message;
//! 5. acks it from Bob, then pings: the `Pong` proves the ack went through, since the
//!    gateway handles a connection's frames in order and answers a failed ack with an
//!    `Error`.
//!
//! The deployment must run message-svc with `PUSH_DEV_SETUP=1` and the gateway with
//! `PUSH_DEV_AUTH=1`.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::{
    MakeContactsRequest, OpenDirectRequest, RegisterDeviceRequest, RegisterUserRequest,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, ContentType, Identify, Ping, Send};
use push_proto::{decode_frame, encode_frame, id_bytes};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

/// How long each step may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to keep trying to connect: an ingress may answer 503 for a few seconds after
/// its backends become ready.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Where the deployment is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// gRPC URL of message-svc, for the dev setup calls: `http://host:50051`.
    pub setup_url: String,
    /// WebSocket URL of the gateway: `ws://host/v1/socket`.
    pub socket_url: String,
    /// `Host` header for the WebSocket request, when the URL names an ingress controller
    /// rather than the virtual host it routes (`push.local`).
    pub host: Option<String>,
}

/// The usage line of the `push-smoke` binary.
pub const USAGE: &str = "usage: push-smoke --setup <grpc-url> --socket <ws-url> [--host <host>]";

impl Target {
    /// Reads `--setup`, `--socket` and `--host` from `args`; each one missing there is read
    /// from `env` (`PUSH_SMOKE_SETUP_URL`, `PUSH_SMOKE_SOCKET_URL`, `PUSH_SMOKE_HOST`).
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let (mut setup_url, mut socket_url, mut host) = (None, None, None);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let slot = match flag.as_str() {
                "--setup" => &mut setup_url,
                "--socket" => &mut socket_url,
                "--host" => &mut host,
                other => return Err(format!("unknown argument {other:?}; {USAGE}")),
            };
            *slot = Some(args.next().ok_or_else(|| format!("{flag} needs a value"))?);
        }
        let setup_url = setup_url
            .or_else(|| env("PUSH_SMOKE_SETUP_URL"))
            .ok_or_else(|| format!("--setup is required; {USAGE}"))?;
        let socket_url = socket_url
            .or_else(|| env("PUSH_SMOKE_SOCKET_URL"))
            .ok_or_else(|| format!("--socket is required; {USAGE}"))?;
        let host = host
            .or_else(|| env("PUSH_SMOKE_HOST"))
            .filter(|host| !host.is_empty());
        Ok(Self {
            setup_url,
            socket_url,
            host,
        })
    }
}

/// One user with one device and a connected socket.
struct Client {
    name: &'static str,
    user: Uuid,
    device: Uuid,
}

/// Runs the smoke test, printing each step. Fails at the first step that does not
/// complete within 10 s.
pub async fn run(target: &Target) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut setup = DevSetupClient::connect(target.setup_url.clone())
        .await
        .with_context(|| format!("connect to message-svc at {}", target.setup_url))?;
    let alice = Client::register("alice", &mut setup).await?;
    let bob = Client::register("bob", &mut setup).await?;
    setup
        .make_contacts(MakeContactsRequest {
            user_id: id_bytes(alice.user),
            peer_id: id_bytes(bob.user),
        })
        .await
        .context("make Alice and Bob contacts")?;
    let conversation = setup
        .open_direct(OpenDirectRequest {
            user_id: id_bytes(alice.user),
            peer_id: id_bytes(bob.user),
        })
        .await
        .context("open their direct conversation")?
        .into_inner()
        .conversation_id;
    step(&format!(
        "set up alice={} bob={} as contacts with a direct conversation",
        alice.user, bob.user
    ));

    let mut bob_socket = bob.connect(target).await?;
    let mut alice_socket = alice.connect(target).await?;

    let client_msg_id = id_bytes(Uuid::now_v7());
    let text = format!("hello from push-smoke {}", Uuid::now_v7());
    let sent_at = Instant::now();
    send(
        &mut alice_socket,
        Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.clone(),
            content_type: ContentType::Text.into(),
            body: text.clone().into_bytes(),
            client_ts: None,
        }),
    )
    .await?;
    let ack = loop {
        match next(&mut alice_socket, "alice's SendAck").await? {
            Body::SendAck(ack) if ack.client_msg_id == client_msg_id => break ack,
            Body::Error(error) => bail!("alice's send failed: {error:?}"),
            // Alice's own devices also receive her message.
            _ => {}
        }
    };
    step(&format!(
        "alice got SendAck seq={} after {} ms",
        ack.seq,
        sent_at.elapsed().as_millis()
    ));

    let entry = loop {
        match next(&mut bob_socket, "bob's Deliver").await? {
            Body::Deliver(entry)
                if entry.seq == ack.seq && entry.conversation_id == conversation =>
            {
                break entry;
            }
            Body::Error(error) => bail!("bob got an error: {error:?}"),
            _ => {}
        }
    };
    if entry.body.as_deref() != Some(text.as_bytes()) || entry.sender_id != id_bytes(alice.user) {
        bail!("bob received the wrong message: {entry:?}");
    }
    step(&format!(
        "bob got Deliver seq={} after {} ms",
        entry.seq,
        sent_at.elapsed().as_millis()
    ));

    send(
        &mut bob_socket,
        Body::Ack(Ack {
            conversation_id: conversation,
            seq: entry.seq,
        }),
    )
    .await?;
    send(&mut bob_socket, Body::Ping(Ping {})).await?;
    loop {
        match next(&mut bob_socket, "bob's Pong").await? {
            Body::Pong(_) => break,
            Body::Error(error) => bail!("bob's ack failed: {error:?}"),
            _ => {}
        }
    }
    step("bob acked; the gateway answered the following Ping");

    for socket in [&mut alice_socket, &mut bob_socket] {
        let _ = socket.close(None).await;
    }
    step(&format!("PASS in {} ms", started.elapsed().as_millis()));
    Ok(())
}

impl Client {
    /// Registers a new user with one device.
    async fn register(
        name: &'static str,
        setup: &mut DevSetupClient<tonic::transport::Channel>,
    ) -> anyhow::Result<Self> {
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        setup
            .register_user(RegisterUserRequest {
                user_id: id_bytes(user),
            })
            .await
            .with_context(|| format!("register {name}"))?;
        setup
            .register_device(RegisterDeviceRequest {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
            })
            .await
            .with_context(|| format!("register {name}'s device"))?;
        Ok(Self { name, user, device })
    }

    /// Connects and identifies; returns once `Ready` arrived.
    async fn connect(&self, target: &Target) -> anyhow::Result<Socket> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut socket = loop {
            let mut request = target.socket_url.as_str().into_client_request()?;
            if let Some(host) = &target.host {
                request
                    .headers_mut()
                    .insert("Host", HeaderValue::from_str(host)?);
            }
            match tokio::time::timeout(STEP_TIMEOUT, connect_async(request)).await {
                Ok(Ok((socket, _))) => break socket,
                Ok(Err(error)) if Instant::now() < deadline => {
                    step(&format!(
                        "{} cannot connect yet ({error}); retrying",
                        self.name
                    ));
                }
                Ok(Err(error)) => {
                    return Err(error)
                        .context(format!("connect {} to {}", self.name, target.socket_url));
                }
                Err(_) => bail!("connecting {} timed out", self.name),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        send(
            &mut socket,
            Body::Identify(Identify {
                token: String::new(),
                dev_user_id: id_bytes(self.user),
                dev_device_id: id_bytes(self.device),
            }),
        )
        .await?;
        match next(&mut socket, "Ready").await? {
            Body::Ready(ready) => {
                step(&format!(
                    "{} connected (Ready, heartbeat {} ms)",
                    self.name, ready.heartbeat_ms
                ));
                Ok(socket)
            }
            other => bail!("{} expected Ready, got {other:?}", self.name),
        }
    }
}

async fn send(socket: &mut Socket, body: Body) -> anyhow::Result<()> {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .context("send a frame")
}

/// The next frame, within the step timeout.
async fn next(socket: &mut Socket, waiting_for: &str) -> anyhow::Result<Body> {
    loop {
        let message = tokio::time::timeout(STEP_TIMEOUT, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("no {waiting_for} within {STEP_TIMEOUT:?}"))?
            .with_context(|| format!("the connection ended while waiting for {waiting_for}"))?
            .with_context(|| format!("the connection failed while waiting for {waiting_for}"))?;
        match message {
            Message::Binary(bytes) => return decode_frame(&bytes).context("decode a frame"),
            Message::Close(frame) => bail!("closed while waiting for {waiting_for}: {frame:?}"),
            _ => {}
        }
    }
}

fn step(message: &str) {
    println!("smoke: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn flags_name_the_target() {
        let target = Target::parse(
            args(&[
                "--setup",
                "http://svc:50051",
                "--socket",
                "ws://ingress/v1/socket",
                "--host",
                "push.local",
            ]),
            |_| None,
        );
        assert_eq!(
            target,
            Ok(Target {
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://ingress/v1/socket".to_owned(),
                host: Some("push.local".to_owned()),
            })
        );
    }

    #[test]
    fn missing_flags_come_from_the_environment() {
        let target = Target::parse(args(&["--socket", "ws://gw/v1/socket"]), |name| {
            (name == "PUSH_SMOKE_SETUP_URL").then(|| "http://svc:50051".to_owned())
        });
        assert_eq!(
            target,
            Ok(Target {
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://gw/v1/socket".to_owned(),
                host: None,
            })
        );
    }

    #[test]
    fn a_missing_url_or_unknown_flag_is_refused() {
        assert!(Target::parse(args(&["--setup", "http://svc:50051"]), |_| None).is_err());
        assert!(Target::parse(args(&["--verbose"]), |_| None).is_err());
        assert!(Target::parse(args(&["--setup"]), |_| None).is_err());
    }
}
```

Create `crates/push-smoke/src/main.rs`:

```rust
use std::process::ExitCode;

use push_smoke::{Target, run};

#[tokio::main]
async fn main() -> ExitCode {
    let target = match Target::parse(std::env::args().skip(1), |name| std::env::var(name).ok()) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    match run(&target).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("smoke: FAIL: {error:#}");
            ExitCode::FAILURE
        }
    }
}
```

- [ ] **Step 4: Run it**

```powershell
cargo test -p push-smoke -- --nocapture
```

Expected: unit tests 3 passed; `local` 1 passed, printing (ids and times differ):

```
smoke: set up alice=… bob=… as contacts with a direct conversation
smoke: bob connected (Ready, heartbeat 30000 ms)
smoke: alice connected (Ready, heartbeat 30000 ms)
smoke: alice got SendAck seq=1 after 19 ms
smoke: bob got Deliver seq=1 after 25 ms
smoke: bob acked; the gateway answered the following Ping
smoke: PASS in 197 ms
```

- [ ] **Step 5: Run the gates**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 604 tests pass.

- [ ] **Step 6: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/push-smoke
git commit -m "feat(push-smoke): end-to-end smoke test of a running deployment"
```

---

### Task 10: CI, spec, roadmap and pull request

**Files:**
- Modify: `.github/workflows/ci.yml`, `README.md`,
  `docs/superpowers/specs/2026-10-11-push-messenger-design.md`,
  `docs/superpowers/plans/2026-10-11-roadmap.md`

- [ ] **Step 1: Pull the Redis image in CI**

Replace `.github/workflows/ci.yml` with:

```yaml
name: ci

on:
  push:
    branches: [main]
  pull_request:

permissions:
  contents: read

jobs:
  rust:
    runs-on: ubuntu-latest
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace --all-targets --locked -- -D warnings
      # The integration tests start these images through the runner's Docker.
      - run: docker pull postgres:17-alpine
      - run: docker pull redis:7-alpine
      - run: cargo test --workspace --locked

  msrv:
    runs-on: ubuntu-latest
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.85
      - uses: Swatinem/rust-cache@v2
      - run: docker pull postgres:17-alpine
      - run: docker pull redis:7-alpine
      - run: cargo test --workspace --locked
```

- [ ] **Step 2: Tell developers about the images and protoc**

In `README.md`, replace the text under the `## Build and test` heading (keep the heading;
replace everything from the line after it up to `## License`) with:

```markdown
The integration tests start `postgres:17-alpine` and `redis:7-alpine` containers through
Docker, so Docker must be running (the first run pulls the images):

    docker version
    cargo test --workspace

Building needs neither a database nor a system `protoc` (`push-proto` uses a vendored
one).
```

Keep one blank line before `## License`.

- [ ] **Step 3: Correct `Deliver` and record the encoding in the spec**

In §7.2 of the spec, replace the `Deliver` row

```markdown
| Deliver | `conversation_id, seq, message_id, client_msg_id, kind, target_message_id, sender_id, content_type, body, client_ts, server_ts, edited_at, edit_seq, deleted` |
```

with

```markdown
| Deliver | `conversation_id, seq, message_id, client_msg_id, kind, target_seq, sender_id, content_type, body, client_ts, server_ts, edited_at, edit_seq, deleted` — `target_seq` is set on edit and delete entries: the `seq` of the message they change, as the log stores it |
```

At the end of §7 (after the §7.5 timeouts table, before `## 8. SDK`), add:

```markdown
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
  warning at start-up when enabled.
```

In §10, replace the line `│   └── push-sdk          client core` with:

```
│   ├── push-sdk          client core
│   └── push-smoke        end-to-end smoke test of a running deployment (not published)
```

- [ ] **Step 4: Split plan 4 in the roadmap and mark 4A in review**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the row that starts
`| 4 | **Minikube walking skeleton** |` with these two rows:

```markdown
| 4A | **Walking skeleton: the services** | `push-proto` (wire and gRPC), `PgStore::connect` with session timeouts, a minimal `push-message-svc` and `push-gateway` able to send and deliver a message end to end, and `push-smoke`; tested locally against Postgres and Redis containers | 3B | In review |
| 4B | **Walking skeleton: minikube** | Dockerfiles; Helm chart with probes; Postgres and Redis in-cluster; ingress-nginx with WebSocket upgrade and timeouts; one script to bring it all up on minikube; the smoke test against the cluster | 4A | |
```

The later rows keep "4" as a dependency: it means both halves.

- [ ] **Step 5: Run the workspace gates**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 test --workspace --locked
```

Expected: no formatting diff, no warnings, 604 tests pass on both toolchains.

- [ ] **Step 6: Commit and push**

```powershell
git add .github README.md docs
git commit -m "docs: record plan 4A in the spec, roadmap and CI"
git push -u origin feature/plan-04a-skeleton-services
```

- [ ] **Step 7: Open the pull request and wait for CI**

```powershell
gh pr create --base main --title "Plan 4A: walking skeleton services" --body "Implements docs/superpowers/plans/2026-10-12-plan-04a-minikube-skeleton-services.md: push-proto (client frames and the internal gRPC API, vendored protoc), PgStore::connect with lock/statement/idle-in-transaction timeouts (55P03 -> Conflict), ping and close; push-message-svc (Messaging and DEV ONLY DevSetup over tonic, Redis notifications, health, migrations, graceful shutdown); push-gateway (axum WebSockets, session state machine, Redis hub, 4001/4005 close codes, DEV ONLY identity behind PUSH_DEV_AUTH=1); push-smoke with a whole-stack test on Postgres and Redis containers. 604 tests. Minikube comes with Plan 4B."
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust` and `msrv`). Do not merge; the controller
merges after the final review.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §2 gateway is transport only: no database, forwards over gRPC, listens for wake-up signals | 6, 7, 8 |
| §2 Postgres is the truth, Redis a doorbell: re-sync after Ready and on re-subscription; never a body on Redis | 3, 7, 8 |
| §4.2 SendAck only after commit; retries return the original receipt (domain, unchanged) | 4, 8, 9 |
| §4.3 delivering: notify → Sync per connected device, serialised per device → Deliver → Ack | 4, 7, 8, 9 |
| §4.3 batches: a backlog beyond one batch flows on as the device acks; a failed sync is retried with backoff | 8 |
| §7 wire protocol as protobuf in `push-proto`, the single contract; reserved numbers for ephemeral, urgent, private reads, media | 2, 10 |
| §7.2 `Deliver` with `target_seq` (the flagged mismatch) | 2, 4, 10 |
| §7.3 Identify first within 5 s; Ready with `heartbeat_ms`; Ping/Pong | 6, 8 |
| §7.4 close codes 4001 (identity) and 4005 (shutting down) | 2, 6, 8 |
| §11 graceful shutdown; health and readiness for probes; migrations as the database owner, one database per service | 1, 5, 8 |
| §12 integration tests on real Postgres and Redis; protocol tests on raw frames; an end-to-end test through the full stack | 1, 3, 5, 7, 8, 9 |
| Plan 3B review: `connect(options)`, session timeouts, 55P03 → Conflict, `ping`, `close`, migrations re-run as a no-op | 1 |

## Deferred to later plans

- **Minikube** (Plan 4B): Dockerfile, Helm chart, ingress-nginx timeouts, bring-up
  scripts, the smoke test on the cluster, CI image builds.
- **Real identity** (Plans 5 and 8): JWT verification, `ReAuth`, `AuthExpiring`, 4002;
  removing `dev_user_id` / `dev_device_id` and the `DevSetup` service.
- **The rest of the protocol** (Plans 7 and 8): `Edit`, `Delete`, `ReadUpTo`, request
  frames with `request_id`, `Receipt`, `ReadPosition`, `ContactEvent` (message-svc already
  publishes the notifications; the gateway ignores all but `EntriesAvailable`), group
  receipt coalescing (≤ 1/s per conversation).
- **Gateway robustness** (Plan 8): `kick:{device_id}` and takeover (4003), rate limits
  (4004), the 5-minute sweep, a server-side idle timeout, batched draining in a `preStop`
  hook, metrics.
- **gRPC load balancing across message-svc replicas** (Plan 7 or 8): HTTP/2 keeps one
  long-lived connection per gateway, so a ClusterIP Service pins each gateway to one
  replica; needs client-side balancing over a headless Service, or a mesh.
- **message-svc** (Plan 7): retrying `Conflict` inside the service, gRPC health checking,
  connection pool sizing per replica, TLS between services if they ever leave the
  cluster.
- A shared runtime crate for `init_logging`, `terminated` and the strict `dev_flag`
  parser, now duplicated in the two binaries.
- Plan 2B and 3B deferrals that remain open: `create_group` idempotency, title control
  characters, coalescing group receipts at the gateway, registering a new device at
  `last_seq` (decide in Plan 5).
