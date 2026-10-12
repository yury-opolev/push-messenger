# Plan 3B — Postgres Store Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the push-domain `Store` and `Tx` traits on PostgreSQL in a new crate, `push-store`: the `messaging` schema as an embedded sqlx migration, every `Tx` method with runtime-checked queries (no database needed to build), READ COMMITTED transactions with row and advisory locks, and the database errors mapped to domain errors. Run the whole `push-testkit` suite from Plan 3A against Postgres on testcontainers, plus concurrency scenarios that only a real database can run.

**Architecture:** `PgStore` wraps a `PgPool`; `PgTx` wraps a `sqlx::Transaction` begun with `BEGIN ISOLATION LEVEL READ COMMITTED`. Each `Tx` method is one statement (two for `insert_device` and for `raise_read_seq` when nothing moves). Inserts that the contract answers with `Duplicate` use `INSERT … ON CONFLICT (<that key>) DO NOTHING`, so the transaction stays usable and every other key still fails. `next_seq` is a single `UPDATE … RETURNING` whose row lock serialises appends; contact pairs and group positions are serialised with `pg_advisory_xact_lock`; invites with `SELECT … FOR UPDATE`. The set-based methods of Plan 3A (`group_positions`, `member_positions`, `sync_positions`, `conversation_overviews`) are one statement each, built on three SQL functions in the migration. The test binary `push-store/tests/postgres.rs` starts one `postgres:17-alpine` container per run, migrates a template database and gives every case its own copy. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §2, §3.2, §4, §12.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85); new: `sqlx` 0.8 (runtime-tokio, postgres, uuid, chrono, migrate, macros; no TLS yet), `tracing` 0.1, and for tests `testcontainers-modules` 0.13 (testcontainers 0.25) with PostgreSQL 17.

---

## Before you start

- **Plan 3A is merged** (PR #4). This plan builds on `push-testkit` and the `Tx` contract
  as Plan 3A left them on `main`.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-03b-postgres-store
```

- The toolchain is rustup-managed (`C:\Users\yurio\.cargo\bin`), with the `1.85` toolchain
  installed for the MSRV check. If `cargo --version` prints something older than 1.85, or
  `cargo clippy` is missing, the shell has a stale PATH. Fix it for the current PowerShell
  session:

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
```

- **Docker must be running** (Docker Desktop on this machine; `docker version` must print a
  server version). From Task 1 on, `cargo test --workspace` starts a PostgreSQL container
  through it. The first run pulls `postgres:17-alpine`. The run removes its container when
  it ends; a run killed half-way can leave one behind (`docker ps -a`, then
  `docker rm -f <id>`).
- **Edit files with the edit tool, and keep them LF.** The repository stores text with LF
  line endings (`.gitattributes`: `* text=auto eol=lf`). Change existing files with the
  edit tool, which keeps their line endings; never rewrite a file through PowerShell
  `Set-Content` or `Out-File`. Some file-creation tools on Windows write CRLF: convert a
  new file to LF if one did. Git normalises line endings on commit either way. This
  matters most for the SQL migration: sqlx checksums it, and the checksum must not depend
  on the machine that built it.
- **Do not pipe `cargo fmt`.** Run `cargo fmt --all -- --check` on its own and read the
  exit code; piping it into `Select-String` or `Out-Null` hides the diff and the failure.
  If it reports a diff, run `cargo fmt --all` and check again. The code in this plan is
  already formatted, so a diff usually means a copying mistake.
- Baseline (after Plan 3A): `cargo test --workspace` passes 272 tests (`push-domain` 22
  unit, `push-testkit` `in_memory` 250, of which 51 are contract cases).
- **Testing rule:** the contract cases and scenarios already exist (Plan 3A). Each task
  first switches on the cases for the methods it implements and runs them to watch them
  fail (`not implemented: plan 3B task N`), then implements the methods. New helpers
  (error mapping, enum codes, lock keys) come with their unit tests in the same file.
- **Every task ends green.** The last step of each task runs all four gates:

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

  The last one is what the `msrv` CI job needs. Never add `#[allow(dead_code)]` or other
  `allow`s; the tasks introduce each helper with its first user.
- **Scope rule:** no domain changes (Plan 3A made them; Tasks 3–6 only correct doc
  comments in `push-domain/src/store.rs` and `push-testkit`), no connection settings, pool
  sizing or TLS for production (Plan 4 wires the service), no `docker compose` file (tests
  start their own container).

## Key rules the code must preserve

- The `Tx` contract in `crates/push-domain/src/store.rs` is the specification: every
  contract case and every scenario of `push-testkit` passes on Postgres exactly as on the
  in-memory store.
- `seq` is gapless: `next_seq` is one `UPDATE … RETURNING`, undone by rollback.
- Never catch a unique violation: a failed statement aborts a Postgres transaction. Every
  place the contract returns `Duplicate` uses `ON CONFLICT (<key>) DO NOTHING`.
- Positions only move forward: `raise_cursor` and `raise_read_seq` update only upwards.
- READ COMMITTED everywhere; serialisation comes from row locks and advisory locks, never
  from a stricter isolation level.

## Decisions made for this plan

These were settled while writing the plan. Do not reopen them during execution.

1. **Dependencies, chosen for Rust 1.85.** `sqlx = "0.8"` resolves to 0.8.6 (0.9 needs
   Rust 1.94), with `default-features = false` and `runtime-tokio`, `postgres`, `uuid`,
   `chrono`, `migrate`, `macros` (the last only for `sqlx::migrate!`). No TLS feature yet:
   the database runs in the cluster (Plan 4 decides). Tests: `testcontainers-modules =
   "0.13"` with `postgres` (testcontainers 0.25, MSRV 1.82; 0.15 / 0.28 need a newer Rust).
   `tracing = "0.1"`. The workspace uses resolver 3, which picks versions whose
   `rust-version` allows 1.85, and `cargo +1.85 build --locked` checks it.
2. **Runtime-checked queries.** `sqlx::query`, `query_as` and `query_scalar` with `bind`;
   never `query!`, so building needs no database and no offline data. Column lists shared
   by several queries are `macro_rules!` that expand to string literals, so queries stay
   `&'static str` through `concat!`.
3. **The schema** is one migration, `crates/push-store/migrations/0001_messaging.sql`, in
   the schema `messaging` (auth tables come with Plan 5), embedded with
   `sqlx::migrate!()` as `push_store::MIGRATOR`; `PgStore::migrate` runs it. Every table,
   key and index of spec §3.2 plus: `invite.revoked_at` and checks (Crockford code, 1–100
   uses, `uses <= max_uses`); `smallint` enums with `CHECK`s; `conversation.direct_key`
   exactly 32 bytes and only on direct conversations; `member_device (user_id)`; partial
   indexes on `message` for edit targets, history kinds (`kind in (1, 4)`) and live
   messages (`kind = 1 and deleted_at is null`, including `sender_id`). Three `STABLE` SQL
   functions hold the per-member rules once: `messaging.delivered_seq`,
   `messaging.effective_position` and `messaging.group_positions` (no row when a group has
   no current members).
4. **Transactions** begin with `pool.begin_with("BEGIN ISOLATION LEVEL READ COMMITTED")`
   (explicit, whatever the server default). Dropping a `PgTx` rolls back (sqlx).
5. **Errors.** `error::store_error` logs every database error with `tracing` and returns
   `DomainError::Conflict` for SQLSTATE `40001` (serialization failure) and `40P01`
   (deadlock), `DomainError::Store(message)` for everything else, constraint violations
   included. Rows that fail to decode (an unknown enum code) are `Store` errors too.
6. **Advisory lock keys** are the 64-bit FNV-1a hash of a tag (`contact-pair`,
   `conversation-positions`) followed by the 16-byte ids (a pair ordered lower first), read
   as a signed `bigint`. FNV-1a is fixed by its definition, so every replica computes the
   same key. The tag namespaces each kind of lock, so a key shared across kinds is as
   unlikely as any other 64-bit collision. Two subjects sharing a key, of one kind or of
   two, only wait for each other; correctness never depends on keys being distinct.
7. **The test harness.** `tests/postgres.rs` (`harness = false`) starts one container on
   the first case that needs it (`OnceLock`, through the main runtime's handle), so
   `--list` or a filter matching nothing starts no container; it removes the container
   after the run. The server runs with `fsync=off` and `max_connections=500`. A template
   database is created and migrated once; each case gets `CREATE DATABASE case_<uuid>
   TEMPLATE push_template` (about 20 ms) and a pool of 8 connections, and the database is
   dropped `WITH (FORCE)` when the case passes. The host `localhost` is replaced by
   `127.0.0.1`: on Windows every connection to `localhost` first tried IPv6 and cost about
   2 seconds (the suite took 130 s instead of 8 s).
8. **Building the store in steps.** Task 1 writes every `Tx` method, the later ones as
   stubs that panic with `unimplemented!("plan 3B task N")` (parameters prefixed `_`).
   The runner runs only the contract modules listed in `READY` until Task 6 removes the
   filter and runs everything.
9. **Concurrency scenarios** (Task 7) live in `push-testkit` (`concurrency_cases`), use only
   the public API, and only `push-store` runs them: concurrent sends stay gapless and
   ordered; concurrent retries of one `client_msg_id` store one entry; concurrent
   `open_direct` calls create one conversation; a concurrent accept never undoes a block;
   concurrent redeems never exceed `max_uses`; concurrent acks by different members still
   produce the group receipt; concurrent registrations of one device id by two users give
   it exactly one owner, and the loser gets `UnknownDevice` and no delivery positions; a
   deadlock is reported as a retryable `Conflict`. Each was checked to fail when the lock
   or check it relies on is removed (Task 7, Step 4).
10. **CI** needs no new job: `cargo test --workspace --locked` in both jobs runs the Postgres
    tests with the runner's Docker. Each job pulls `postgres:17-alpine` first (a registry
    failure then shows as such) and gets a 30-minute timeout.

## File structure

```
Cargo.toml                    + push-store member; sqlx, testcontainers-modules,
                                tracing, push-testkit
README.md                     Docker for the tests
.github/workflows/ci.yml      pull the Postgres image, timeouts
crates/push-store/            NEW
├── Cargo.toml
├── migrations/
│   └── 0001_messaging.sql    the messaging schema, indexes, SQL functions
├── src/
│   ├── lib.rs                MIGRATOR, re-exports
│   ├── error.rs              database errors -> DomainError (Conflict / Store)
│   ├── codes.rs              smallint codes of the domain enums
│   ├── locks.rs              advisory lock keys (FNV-1a)
│   ├── rows.rs               reading Conversation, Member, Entry, InviteRecord rows
│   └── store.rs              PgStore, PgTx, impl Store, impl Tx
└── tests/
    └── postgres.rs           harness = false: the suite on testcontainers
crates/push-domain/src/store.rs   doc comments only (Tasks 3–5)
crates/push-testkit/
├── Cargo.toml                tokio "macros"
└── src/
    ├── lib.rs                + concurrency_cases()
    ├── concurrency.rs        NEW: the concurrency scenarios
    └── scenarios/send.rs     doc comment only (Task 6)
```

---

### Task 1: The crate, the schema, users and devices

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/push-store/Cargo.toml`
- Create: `crates/push-store/migrations/0001_messaging.sql`
- Create: `crates/push-store/src/lib.rs`, `error.rs`, `store.rs`
- Create: `crates/push-store/tests/postgres.rs`

- [ ] **Step 1: Add the crate and its dependencies**

```powershell
New-Item -ItemType Directory crates\push-store\src, crates\push-store\migrations, crates\push-store\tests -Force
```

In `Cargo.toml`, replace the `members` line with:

```toml
members = ["crates/push-domain", "crates/push-store", "crates/push-testkit"]
```

add this line to `[workspace.dependencies]` after the `push-domain` line:

```toml
push-testkit = { path = "crates/push-testkit" }
```

this after the `serde_json` line:

```toml
sqlx = { version = "0.8", default-features = false, features = [
    "chrono",
    "macros",
    "migrate",
    "postgres",
    "runtime-tokio",
    "uuid",
] }
testcontainers-modules = { version = "0.13", features = ["postgres"] }
```

and this after the `tokio` line:

```toml
tracing = "0.1"
```

Create `crates/push-store/Cargo.toml`:

```toml
[package]
name = "push-store"
description = "PostgreSQL storage for push-messenger: migrations and the push-domain Store."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
async-trait.workspace = true
chrono.workspace = true
push-domain.workspace = true
sqlx.workspace = true
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
push-testkit.workspace = true
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["rt-multi-thread"] }

[[test]]
name = "postgres"
harness = false

[lints]
workspace = true
```

- [ ] **Step 2: Write the schema**

Create `crates/push-store/migrations/0001_messaging.sql` (LF line endings):

```sql
-- The messaging schema (spec §3.2), owned by message-svc.
--
-- Enumerations are smallints with CHECK constraints; push-store's codes module maps them:
--   contact.state              1 requested_out, 2 requested_in, 3 accepted, 4 blocked
--   conversation.kind          1 direct, 2 group
--   conversation_member.role   1 admin, 2 member
--   message.kind               1 message, 2 edit, 3 delete, 4 system
--   message.content_type       1 text, 2 system event

CREATE SCHEMA messaging;

-- Mirrored from auth through user.verified events.
CREATE TABLE messaging.known_user (
    user_id    uuid PRIMARY KEY,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Mirrored from auth through device events.
CREATE TABLE messaging.member_device (
    device_id  uuid PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES messaging.known_user,
    revoked_at timestamptz
);
CREATE INDEX member_device_user ON messaging.member_device (user_id);

-- One row per direction.
CREATE TABLE messaging.contact (
    owner_id   uuid NOT NULL REFERENCES messaging.known_user,
    peer_id    uuid NOT NULL REFERENCES messaging.known_user,
    state      smallint NOT NULL CHECK (state BETWEEN 1 AND 4),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (owner_id, peer_id),
    CHECK (owner_id <> peer_id)
);

CREATE TABLE messaging.invite (
    code       text PRIMARY KEY CHECK (code ~ '^[0-9A-HJKMNP-TV-Z]{10}$'),
    user_id    uuid NOT NULL REFERENCES messaging.known_user,
    expires_at timestamptz NOT NULL,
    max_uses   integer NOT NULL DEFAULT 1 CHECK (max_uses BETWEEN 1 AND 100),
    uses       integer NOT NULL DEFAULT 0,
    revoked_at timestamptz,
    CHECK (uses BETWEEN 0 AND max_uses)
);

CREATE TABLE messaging.conversation (
    conversation_id uuid PRIMARY KEY,
    kind            smallint NOT NULL CHECK (kind IN (1, 2)),
    -- Direct only: the two user ids, lower first.
    direct_key      bytea UNIQUE CHECK (octet_length(direct_key) = 32),
    title           text,
    created_by      uuid NOT NULL,
    -- The single-writer counter; see next_seq.
    last_seq        bigint NOT NULL DEFAULT 0 CHECK (last_seq >= 0),
    created_at      timestamptz NOT NULL DEFAULT now(),
    -- Creation or last append; never moves back.
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CHECK (direct_key IS NULL OR kind = 1)
);

CREATE TABLE messaging.conversation_member (
    conversation_id  uuid NOT NULL REFERENCES messaging.conversation,
    user_id          uuid NOT NULL REFERENCES messaging.known_user,
    role             smallint NOT NULL CHECK (role IN (1, 2)),
    visible_from_seq bigint NOT NULL CHECK (visible_from_seq >= 1),
    left_at_seq      bigint CHECK (left_at_seq >= 0),
    read_seq         bigint NOT NULL DEFAULT 0 CHECK (read_seq >= 0),
    PRIMARY KEY (conversation_id, user_id)
);
-- "My conversations".
CREATE INDEX conversation_member_user ON messaging.conversation_member (user_id);

CREATE TABLE messaging.message (
    conversation_id  uuid NOT NULL REFERENCES messaging.conversation,
    seq              bigint NOT NULL CHECK (seq >= 1),
    message_id       uuid NOT NULL UNIQUE,
    client_msg_id    uuid NOT NULL,
    kind             smallint NOT NULL CHECK (kind BETWEEN 1 AND 4),
    target_seq       bigint,
    sender_id        uuid NOT NULL,
    sender_device_id uuid,
    content_type     smallint NOT NULL CHECK (content_type IN (1, 2)),
    body             bytea,
    client_ts        timestamptz,
    server_ts        timestamptz NOT NULL DEFAULT now(),
    edited_at        timestamptz,
    edit_seq         bigint,
    deleted_at       timestamptz,
    PRIMARY KEY (conversation_id, seq),
    UNIQUE (conversation_id, client_msg_id)
);
-- Edits and deletes of one message (erase_edit_bodies).
CREATE INDEX message_target ON messaging.message (conversation_id, target_seq)
    WHERE target_seq IS NOT NULL;
-- History pages: messages and system entries only (messages_before).
CREATE INDEX message_history ON messaging.message (conversation_id, seq)
    WHERE kind IN (1, 4);
-- Unread counts and effective positions: live messages only.
CREATE INDEX message_live ON messaging.message (conversation_id, seq) INCLUDE (sender_id)
    WHERE kind = 1 AND deleted_at IS NULL;

CREATE TABLE messaging.device_cursor (
    device_id       uuid NOT NULL REFERENCES messaging.member_device,
    conversation_id uuid NOT NULL REFERENCES messaging.conversation,
    -- The device has everything up to here.
    delivered_seq   bigint NOT NULL CHECK (delivered_seq >= 0),
    PRIMARY KEY (device_id, conversation_id)
);

-- The furthest delivery position of any of a user's devices, revoked ones included; 0 if
-- there is none.
CREATE FUNCTION messaging.delivered_seq(in_conversation uuid, in_user uuid)
RETURNS bigint
LANGUAGE sql STABLE
AS $$
    SELECT coalesce(max(c.delivered_seq), 0)
    FROM messaging.member_device d
    JOIN messaging.device_cursor c
        ON c.device_id = d.device_id AND c.conversation_id = in_conversation
    WHERE d.user_id = in_user
$$;

-- How far a member at a raw position has effectively got: just before the first live
-- message from someone else after max(position, visible_from_seq - 1), or `upper` if
-- there is none. See Tx::group_positions in push-domain.
CREATE FUNCTION messaging.effective_position(
    in_conversation uuid,
    in_user uuid,
    in_visible_from_seq bigint,
    in_upper bigint,
    in_position bigint
)
RETURNS bigint
LANGUAGE sql STABLE
AS $$
    SELECT CASE
        WHEN greatest(in_position, in_visible_from_seq - 1) >= in_upper THEN in_upper
        ELSE coalesce(
            (
                SELECT m.seq - 1
                FROM messaging.message m
                WHERE m.conversation_id = in_conversation
                    AND m.seq > greatest(in_position, in_visible_from_seq - 1)
                    AND m.seq <= in_upper
                    AND m.kind = 1
                    AND m.deleted_at IS NULL
                    AND m.sender_id <> in_user
                ORDER BY m.seq
                LIMIT 1
            ),
            in_upper
        )
    END
$$;

-- A group's group-wide positions with its log ending at in_last_seq: the minimums over
-- its current members. No row if it has no current members.
CREATE FUNCTION messaging.group_positions(in_conversation uuid, in_last_seq bigint)
RETURNS TABLE (delivered bigint, read bigint)
LANGUAGE sql STABLE
AS $$
    SELECT
        min(messaging.effective_position(
            in_conversation, m.user_id, m.visible_from_seq, in_last_seq,
            greatest(messaging.delivered_seq(in_conversation, m.user_id), m.read_seq)
        )),
        min(messaging.effective_position(
            in_conversation, m.user_id, m.visible_from_seq, in_last_seq, m.read_seq
        ))
    FROM messaging.conversation_member m
    WHERE m.conversation_id = in_conversation AND m.left_at_seq IS NULL
    HAVING count(*) > 0
$$;
```

- [ ] **Step 3: Write the Postgres runner**

The manifest names this test target, so it must exist before anything in the workspace builds. Create `crates/push-store/tests/postgres.rs`:

```rust
//! The behaviour suite against PostgreSQL.
//!
//! The first case to run starts one Postgres container for the whole run (Docker must be
//! running) and migrates a template database. Every case then gets a database of its own,
//! copied from the template, so cases run in parallel; it is dropped when the case passes.
//! The container is removed when the run ends.

use std::sync::{Arc, Mutex, OnceLock};

use push_store::PgStore;
use push_testkit::Backend;
use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};
use testcontainers_modules::postgres::Postgres as PostgresImage;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::runtime::Handle;
use uuid::Uuid;

/// The migrated database every case's database is copied from.
const TEMPLATE: &str = "push_template";

/// The running container and the URL of its server, without a database name.
struct Server {
    container: ContainerAsync<PostgresImage>,
    url: String,
}

async fn start_server() -> Server {
    let container = PostgresImage::default()
        .with_tag("17-alpine")
        .with_cmd(["-c", "fsync=off", "-c", "max_connections=500"])
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let host = container.get_host().await.expect("the container host");
    // Docker publishes ports on IPv4; "localhost" would try IPv6 first on every connect.
    let host = match host.to_string().as_str() {
        "localhost" => "127.0.0.1".to_owned(),
        other => other.to_owned(),
    };
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("the Postgres port");
    let url = format!("postgres://postgres:postgres@{host}:{port}");

    let mut admin = PgConnection::connect(&format!("{url}/postgres"))
        .await
        .expect("connect to Postgres");
    admin
        .execute(format!("CREATE DATABASE {TEMPLATE}").as_str())
        .await
        .expect("create the template database");
    admin.close().await.expect("disconnect");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{url}/{TEMPLATE}"))
        .await
        .expect("connect to the template database");
    PgStore::new(pool.clone())
        .migrate()
        .await
        .expect("migrate the template database");
    // CREATE DATABASE … TEMPLATE fails while anyone is connected to the template.
    pool.close().await;
    Server { container, url }
}

/// Makes a database per store and drops them when the case passes.
#[derive(Clone)]
struct Postgres {
    url: String,
    databases: Arc<Mutex<Vec<String>>>,
}

impl Postgres {
    async fn admin(&self) -> PgConnection {
        PgConnection::connect(&format!("{}/postgres", self.url))
            .await
            .expect("connect to Postgres")
    }
}

impl Backend for Postgres {
    type Store = PgStore;

    async fn store(&self) -> PgStore {
        let name = format!("case_{}", Uuid::new_v4().simple());
        let mut admin = self.admin().await;
        admin
            .execute(format!("CREATE DATABASE {name} TEMPLATE {TEMPLATE}").as_str())
            .await
            .expect("create a database for the case");
        admin.close().await.expect("disconnect");
        self.databases.lock().unwrap().push(name.clone());
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(&format!("{}/{name}", self.url))
            .await
            .expect("connect to the case's database");
        PgStore::new(pool)
    }

    async fn finish(&self) {
        let names = std::mem::take(&mut *self.databases.lock().unwrap());
        let mut admin = self.admin().await;
        for name in names {
            admin
                .execute(format!("DROP DATABASE {name} WITH (FORCE)").as_str())
                .await
                .expect("drop the case's database");
        }
        admin.close().await.expect("disconnect");
    }
}

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let handle: Handle = runtime.handle().clone();
    let server: Arc<OnceLock<Server>> = Arc::new(OnceLock::new());

    let shared = Arc::clone(&server);
    let backend = move || Postgres {
        url: shared
            .get_or_init(|| handle.block_on(start_server()))
            .url
            .clone(),
        databases: Arc::default(),
    };
    // The parts of the suite the store implements so far; task 6 runs all of it.
    const READY: &[&str] = &["contract::transactions::", "contract::users::"];
    let cases = push_testkit::cases()
        .into_iter()
        .filter(|case| READY.iter().any(|prefix| case.name.starts_with(prefix)))
        .collect();
    let conclusion = push_testkit::run(backend, cases);

    let started = Arc::into_inner(server).and_then(OnceLock::into_inner);
    if let Some(server) = started {
        runtime
            .block_on(server.container.rm())
            .expect("remove the Postgres container");
    }
    conclusion.exit();
}
```

Create `crates/push-store/src/lib.rs`:

```rust
//! PostgreSQL storage for push-messenger: the migrations of the `messaging` schema
//! (spec §3.2) and the push-domain [`Store`](push_domain::Store) on sqlx.
//!
//! Every query is checked when it runs (`sqlx::query`, not the `query!` macros), so
//! building needs no database. Transactions run at READ COMMITTED; they are serialised
//! where the domain needs it by row locks (`next_seq`, `lock_invite`) and by
//! transaction-scoped advisory locks (contact pairs, conversation positions).
//!
//! Errors: serialization failures and deadlocks become the retryable
//! `DomainError::Conflict`; every other database error, constraint violations included,
//! becomes `DomainError::Store`. Both are logged with `tracing` first.

mod error;
mod store;

pub use store::{PgStore, PgTx};

/// The migrations of the `messaging` schema, embedded at build time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
```

```powershell
cargo test -p push-store
```

Expected: compile errors, `file not found for module `error`` and `file not found for module `store``.

- [ ] **Step 4: Map database errors**

Create `crates/push-store/src/error.rs`, with its unit tests:

```rust
//! Turning database errors into domain errors.

use push_domain::DomainError;

/// SQLSTATE codes a caller may retry: `serialization_failure` and `deadlock_detected`.
const RETRYABLE: [&str; 2] = ["40001", "40P01"];

/// Whether an error with this SQLSTATE goes away when the transaction is retried.
fn is_retryable(code: Option<&str>) -> bool {
    code.is_some_and(|code| RETRYABLE.contains(&code))
}

/// The domain error for a failed database call, logged first: retryable conflicts become
/// `Conflict`, everything else (constraint violations included) `Store`.
pub(crate) fn store_error(error: sqlx::Error) -> DomainError {
    let code = error
        .as_database_error()
        .and_then(|database| database.code())
        .map(|code| code.into_owned());
    if is_retryable(code.as_deref()) {
        tracing::warn!(%error, code = ?code, "retryable database conflict");
        DomainError::Conflict
    } else {
        tracing::error!(%error, code = ?code, "database error");
        DomainError::Store(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_serialization_failures_and_deadlocks_are_retryable() {
        assert!(is_retryable(Some("40001")));
        assert!(is_retryable(Some("40P01")));
        assert!(!is_retryable(Some("23505")), "unique_violation");
        assert!(!is_retryable(Some("23503")), "foreign_key_violation");
        assert!(!is_retryable(Some("23514")), "check_violation");
        assert!(!is_retryable(None));
    }

    #[test]
    fn errors_without_a_database_code_are_store_errors() {
        assert!(matches!(
            store_error(sqlx::Error::RowNotFound),
            DomainError::Store(_)
        ));
    }
}
```

- [ ] **Step 5: Write the store**

Create `crates/push-store/src/store.rs`. Users and devices are implemented; every other method is a stub for the task that implements it:

```rust
//! [`PgStore`] and its transactions.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use push_domain::{
    ClientMsgId, ContactState, Conversation, ConversationId, ConversationOverview, DeviceId,
    DomainResult, Entry, GroupPositions, InsertOutcome, InviteRecord, Member, MessageId, Store, Tx,
    UserId,
};
use sqlx::postgres::PgPool;
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::error::store_error;

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

    /// Applies the migrations the database does not have yet.
    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        crate::MIGRATOR.run(&self.pool).await
    }
}

/// A READ COMMITTED transaction. Dropping it without [`Tx::commit`] rolls it back.
#[derive(Debug)]
pub struct PgTx {
    tx: Transaction<'static, Postgres>,
}

impl PgTx {
    fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }
}

#[async_trait]
impl Store for PgStore {
    type Tx = PgTx;

    async fn begin(&self) -> DomainResult<PgTx> {
        let tx = self
            .pool
            .begin_with("BEGIN ISOLATION LEVEL READ COMMITTED")
            .await
            .map_err(store_error)?;
        Ok(PgTx { tx })
    }
}

#[async_trait]
impl Tx for PgTx {
    async fn commit(self) -> DomainResult<()> {
        self.tx.commit().await.map_err(store_error)
    }

    async fn rollback(self) -> DomainResult<()> {
        self.tx.rollback().await.map_err(store_error)
    }

    async fn insert_user(&mut self, user: UserId) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.known_user (user_id) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(user.0)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn user_exists(&mut self, user: UserId) -> DomainResult<bool> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM messaging.known_user WHERE user_id = $1)")
            .bind(user.0)
            .fetch_one(self.conn())
            .await
            .map_err(store_error)
    }

    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool> {
        // A concurrent insert of the same id makes this wait for its outcome, so the check
        // below sees whichever row won.
        sqlx::query(
            "INSERT INTO messaging.member_device (device_id, user_id) VALUES ($1, $2) \
             ON CONFLICT (device_id) DO NOTHING",
        )
        .bind(device.0)
        .bind(user.0)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        sqlx::query_scalar(
            "SELECT user_id = $2 AND revoked_at IS NULL \
             FROM messaging.member_device WHERE device_id = $1",
        )
        .bind(device.0)
        .bind(user.0)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)
    }

    async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()> {
        sqlx::query(
            "UPDATE messaging.member_device SET revoked_at = coalesce(revoked_at, now()) \
             WHERE device_id = $1",
        )
        .bind(device.0)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        let owner: Option<Uuid> = sqlx::query_scalar(
            "SELECT user_id FROM messaging.member_device \
             WHERE device_id = $1 AND revoked_at IS NULL",
        )
        .bind(device.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        Ok(owner.map(UserId))
    }

    async fn devices_of_user(&mut self, user: UserId) -> DomainResult<Vec<DeviceId>> {
        let devices: Vec<Uuid> = sqlx::query_scalar(
            "SELECT device_id FROM messaging.member_device \
             WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        Ok(devices.into_iter().map(DeviceId).collect())
    }

    async fn lock_contact_pair(&mut self, _a: UserId, _b: UserId) -> DomainResult<()> {
        unimplemented!("plan 3B task 2")
    }

    async fn get_contact(
        &mut self,
        _owner: UserId,
        _peer: UserId,
    ) -> DomainResult<Option<ContactState>> {
        unimplemented!("plan 3B task 2")
    }

    async fn put_contact(
        &mut self,
        _owner: UserId,
        _peer: UserId,
        _state: ContactState,
    ) -> DomainResult<()> {
        unimplemented!("plan 3B task 2")
    }

    async fn delete_contact(&mut self, _owner: UserId, _peer: UserId) -> DomainResult<()> {
        unimplemented!("plan 3B task 2")
    }

    async fn contacts_of(&mut self, _owner: UserId) -> DomainResult<Vec<(UserId, ContactState)>> {
        unimplemented!("plan 3B task 2")
    }

    async fn insert_invite(&mut self, _invite: InviteRecord) -> DomainResult<InsertOutcome> {
        unimplemented!("plan 3B task 2")
    }

    async fn lock_invite(&mut self, _code: &str) -> DomainResult<Option<InviteRecord>> {
        unimplemented!("plan 3B task 2")
    }

    async fn update_invite(&mut self, _invite: InviteRecord) -> DomainResult<()> {
        unimplemented!("plan 3B task 2")
    }

    async fn find_direct(&mut self, _key: [u8; 32]) -> DomainResult<Option<ConversationId>> {
        unimplemented!("plan 3B task 3")
    }

    async fn insert_conversation(
        &mut self,
        _conversation: Conversation,
    ) -> DomainResult<InsertOutcome> {
        unimplemented!("plan 3B task 3")
    }

    async fn get_conversation(
        &mut self,
        _id: ConversationId,
    ) -> DomainResult<Option<Conversation>> {
        unimplemented!("plan 3B task 3")
    }

    async fn next_seq(&mut self, _id: ConversationId, _at: DateTime<Utc>) -> DomainResult<i64> {
        unimplemented!("plan 3B task 3")
    }

    async fn set_title(&mut self, _id: ConversationId, _title: String) -> DomainResult<()> {
        unimplemented!("plan 3B task 3")
    }

    async fn insert_member(&mut self, _member: Member) -> DomainResult<()> {
        unimplemented!("plan 3B task 3")
    }

    async fn get_member(
        &mut self,
        _conversation: ConversationId,
        _user: UserId,
    ) -> DomainResult<Option<Member>> {
        unimplemented!("plan 3B task 3")
    }

    async fn members(&mut self, _conversation: ConversationId) -> DomainResult<Vec<Member>> {
        unimplemented!("plan 3B task 3")
    }

    async fn memberships(&mut self, _user: UserId) -> DomainResult<Vec<(Conversation, Member)>> {
        unimplemented!("plan 3B task 3")
    }

    async fn raise_read_seq(
        &mut self,
        _conversation: ConversationId,
        _user: UserId,
        _seq: i64,
    ) -> DomainResult<bool> {
        unimplemented!("plan 3B task 3")
    }

    async fn lock_conversation_positions(
        &mut self,
        _conversation: ConversationId,
    ) -> DomainResult<()> {
        unimplemented!("plan 3B task 3")
    }

    async fn insert_entry(&mut self, _entry: Entry) -> DomainResult<InsertOutcome> {
        unimplemented!("plan 3B task 4")
    }

    async fn entry_by_client_id(
        &mut self,
        _conversation: ConversationId,
        _client_msg_id: ClientMsgId,
    ) -> DomainResult<Option<Entry>> {
        unimplemented!("plan 3B task 4")
    }

    async fn entry_by_message_id(
        &mut self,
        _conversation: ConversationId,
        _message_id: MessageId,
    ) -> DomainResult<Option<Entry>> {
        unimplemented!("plan 3B task 4")
    }

    async fn update_entry(&mut self, _entry: Entry) -> DomainResult<()> {
        unimplemented!("plan 3B task 4")
    }

    async fn erase_edit_bodies(
        &mut self,
        _conversation: ConversationId,
        _target_seq: i64,
    ) -> DomainResult<()> {
        unimplemented!("plan 3B task 4")
    }

    async fn entries_after(
        &mut self,
        _conversation: ConversationId,
        _after: i64,
        _up_to: i64,
        _targets_from: i64,
        _limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        unimplemented!("plan 3B task 4")
    }

    async fn count_incoming_messages(
        &mut self,
        _conversation: ConversationId,
        _user: UserId,
        _after: i64,
        _up_to: i64,
        _cap: u32,
    ) -> DomainResult<u32> {
        unimplemented!("plan 3B task 4")
    }

    async fn first_live_message(
        &mut self,
        _conversation: ConversationId,
        _after: i64,
        _up_to: i64,
        _not_from: Option<UserId>,
    ) -> DomainResult<Option<i64>> {
        unimplemented!("plan 3B task 4")
    }

    async fn messages_before(
        &mut self,
        _conversation: ConversationId,
        _from: i64,
        _before: i64,
        _limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        unimplemented!("plan 3B task 4")
    }

    async fn get_cursor(
        &mut self,
        _device: DeviceId,
        _conversation: ConversationId,
    ) -> DomainResult<Option<i64>> {
        unimplemented!("plan 3B task 5")
    }

    async fn create_cursor_if_absent(
        &mut self,
        _device: DeviceId,
        _conversation: ConversationId,
        _seq: i64,
    ) -> DomainResult<()> {
        unimplemented!("plan 3B task 5")
    }

    async fn raise_cursor(
        &mut self,
        _device: DeviceId,
        _conversation: ConversationId,
        _seq: i64,
    ) -> DomainResult<bool> {
        unimplemented!("plan 3B task 5")
    }

    async fn delivered_seq(
        &mut self,
        _conversation: ConversationId,
        _user: UserId,
    ) -> DomainResult<i64> {
        unimplemented!("plan 3B task 5")
    }

    async fn member_positions(
        &mut self,
        _conversation: ConversationId,
    ) -> DomainResult<Vec<(Member, i64)>> {
        unimplemented!("plan 3B task 5")
    }

    async fn sync_positions(
        &mut self,
        _user: UserId,
        _device: DeviceId,
    ) -> DomainResult<Vec<(Conversation, Member, i64)>> {
        unimplemented!("plan 3B task 5")
    }

    async fn group_positions(
        &mut self,
        _conversation: ConversationId,
        _last_seq: i64,
    ) -> DomainResult<Option<GroupPositions>> {
        unimplemented!("plan 3B task 5")
    }

    async fn conversation_overviews(
        &mut self,
        _user: UserId,
        _changed_since: Option<DateTime<Utc>>,
        _unread_cap: u32,
    ) -> DomainResult<Vec<ConversationOverview>> {
        unimplemented!("plan 3B task 6")
    }
}
```

- [ ] **Step 6: Run the cases against Postgres**

```powershell
cargo test -p push-store
```

Expected: the unit tests pass (2) and the `postgres` target runs 8 cases (`contract::transactions::…` and `contract::users::…`), all `ok`, in a few seconds after the image is pulled. `docker ps -a` shows no Postgres container afterwards.

- [ ] **Step 7: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 282 tests pass (`push-domain` 22, `push-store` 2 unit and 8 `postgres`, `push-testkit` 250 `in_memory`), no warnings, no diff, the 1.85 build succeeds.

```powershell
git add Cargo.toml Cargo.lock crates/push-store
git commit -m "feat(store): add push-store with the schema, users and devices"
```

---

### Task 2: Contacts and invites

**Files:**
- Create: `crates/push-store/src/codes.rs`, `locks.rs`, `rows.rs`
- Modify: `crates/push-store/src/lib.rs`, `store.rs`
- Modify: `crates/push-store/tests/postgres.rs`

- [ ] **Step 1: Switch on the contacts cases and watch them fail**

In `crates/push-store/tests/postgres.rs`, replace the `READY` list with:

```rust
    const READY: &[&str] = &[
        "contract::transactions::",
        "contract::users::",
        "contract::contacts::",
    ];
```

```powershell
cargo test -p push-store --test postgres
```

Expected: the 6 `contract::contacts::…` cases fail with `not implemented: plan 3B task 2`; the other 8 pass.

- [ ] **Step 2: Write the enum codes and lock keys with their tests**

Create `crates/push-store/src/codes.rs`:

```rust
//! The smallint codes the schema stores for domain enumerations.

use push_domain::{ContactState, ContentType, ConversationKind, EntryKind, Role};

/// A domain enumeration stored as a `smallint`.
pub(crate) trait Code: Sized + Copy + 'static {
    /// Every value with its code.
    const CODES: &'static [(Self, i16)];
    /// The column holding it, for error messages.
    const COLUMN: &'static str;
}

impl Code for ContactState {
    const CODES: &'static [(Self, i16)] = &[
        (Self::RequestedOut, 1),
        (Self::RequestedIn, 2),
        (Self::Accepted, 3),
        (Self::Blocked, 4),
    ];
    const COLUMN: &'static str = "contact.state";
}

impl Code for ConversationKind {
    const CODES: &'static [(Self, i16)] = &[(Self::Direct, 1), (Self::Group, 2)];
    const COLUMN: &'static str = "conversation.kind";
}

impl Code for Role {
    const CODES: &'static [(Self, i16)] = &[(Self::Admin, 1), (Self::Member, 2)];
    const COLUMN: &'static str = "conversation_member.role";
}

impl Code for EntryKind {
    const CODES: &'static [(Self, i16)] = &[
        (Self::Message, 1),
        (Self::Edit, 2),
        (Self::Delete, 3),
        (Self::System, 4),
    ];
    const COLUMN: &'static str = "message.kind";
}

impl Code for ContentType {
    const CODES: &'static [(Self, i16)] = &[(Self::Text, 1), (Self::SystemEvent, 2)];
    const COLUMN: &'static str = "message.content_type";
}

/// The code stored for `value`.
pub(crate) fn encode<T: Code + PartialEq>(value: T) -> i16 {
    T::CODES
        .iter()
        .find(|(known, _)| *known == value)
        .map(|(_, code)| *code)
        .expect("every value has a code")
}

/// The value stored as `code`; a decode error for a code the schema does not allow.
pub(crate) fn decode<T: Code>(code: i16) -> Result<T, sqlx::Error> {
    T::CODES
        .iter()
        .find(|(_, known)| *known == code)
        .map(|(value, _)| *value)
        .ok_or_else(|| sqlx::Error::Decode(format!("unknown {} code {code}", T::COLUMN).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trips<T: Code + PartialEq + std::fmt::Debug>() {
        for (value, code) in T::CODES {
            assert_eq!(encode(*value), *code);
            assert_eq!(decode::<T>(*code).unwrap(), *value);
        }
        assert!(decode::<T>(0).is_err());
    }

    #[test]
    fn every_code_round_trips_and_unknown_codes_are_refused() {
        round_trips::<ContactState>();
        round_trips::<ConversationKind>();
        round_trips::<Role>();
        round_trips::<EntryKind>();
        round_trips::<ContentType>();
    }

    #[test]
    fn codes_match_the_schema_comments() {
        assert_eq!(encode(ContactState::Blocked), 4);
        assert_eq!(encode(ConversationKind::Group), 2);
        assert_eq!(encode(Role::Admin), 1);
        assert_eq!(encode(EntryKind::System), 4);
        assert_eq!(encode(ContentType::SystemEvent), 2);
    }
}
```

Create `crates/push-store/src/locks.rs`:

```rust
//! Keys for transaction-scoped advisory locks (`pg_advisory_xact_lock(bigint)`).
//!
//! A key is the 64-bit FNV-1a hash of a fixed tag followed by the 16-byte ids it covers.
//! FNV-1a is fixed by its definition, so every replica and every release computes the same
//! key. The tag namespaces each kind of lock, so a key shared by two kinds is as unlikely
//! as any other 64-bit collision. Any shared key, within a kind or across kinds, only
//! makes two unrelated operations wait for each other; it never lets two operations on one
//! subject run together.

use push_domain::UserId;
use uuid::Uuid;

const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(tag: &[u8], ids: &[Uuid]) -> i64 {
    let bytes = tag
        .iter()
        .chain(ids.iter().flat_map(|id| id.as_bytes().iter()));
    let hash = bytes.fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    });
    // The same 64 bits, read as the signed key Postgres takes.
    hash as i64
}

/// The lock serialising contact operations between two users, the same in either order.
pub(crate) fn contact_pair(a: UserId, b: UserId) -> i64 {
    let (low, high) = if a <= b { (a, b) } else { (b, a) };
    fnv1a(b"contact-pair", &[low.0, high.0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_matches_the_published_test_vectors() {
        assert_eq!(fnv1a(b"", &[]) as u64, 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a", &[]) as u64, 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar", &[]) as u64, 0x8594_4171_f739_67e8);
    }

    #[test]
    fn a_pair_has_one_key_in_either_order() {
        let (a, b) = (UserId::new(), UserId::new());
        assert_eq!(contact_pair(a, b), contact_pair(b, a));
        assert_ne!(contact_pair(a, b), contact_pair(a, UserId::new()));
    }
}
```

Create `crates/push-store/src/rows.rs`:

```rust
//! Reading domain records from result rows.

use push_domain::{InviteRecord, UserId};
use sqlx::Row;
use sqlx::postgres::PgRow;

fn decode_error(message: String) -> sqlx::Error {
    sqlx::Error::Decode(message.into())
}

pub(crate) fn invite(row: &PgRow) -> Result<InviteRecord, sqlx::Error> {
    let count = |column: &str| -> Result<u32, sqlx::Error> {
        let value: i32 = row.try_get(column)?;
        u32::try_from(value).map_err(|_| decode_error(format!("invite.{column} is {value}")))
    };
    Ok(InviteRecord {
        code: row.try_get("code")?,
        user_id: UserId(row.try_get("user_id")?),
        expires_at: row.try_get("expires_at")?,
        max_uses: count("max_uses")?,
        uses: count("uses")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}
```

In `crates/push-store/src/lib.rs`, replace the lines `mod error;` and `mod store;` with:

```rust
mod codes;
mod error;
mod locks;
mod rows;
mod store;
```

- [ ] **Step 3: Implement the methods**

In `crates/push-store/src/store.rs`, replace the imports (everything from `use async_trait::async_trait;` down to the blank line before `/// The push-domain [`Store`] on PostgreSQL.`) with:

```rust
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use push_domain::{
    ClientMsgId, ContactState, Conversation, ConversationId, ConversationOverview, DeviceId,
    DomainError, DomainResult, Entry, GroupPositions, InsertOutcome, InviteRecord, Member,
    MessageId, Store, Tx, UserId,
};
use sqlx::postgres::PgPool;
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::codes::{decode, encode};
use crate::error::store_error;
use crate::locks;
use crate::rows;
```

add this helper after the `impl Store for PgStore` block:

```rust
/// A count the domain keeps as `u32`, as the `integer` the schema stores.
fn to_integer(value: u32, what: &str) -> DomainResult<i32> {
    i32::try_from(value).map_err(|_| DomainError::Store(format!("{what} {value} is too large")))
}
```

and replace the stubs of `lock_contact_pair`, `get_contact`, `put_contact`, `delete_contact`, `contacts_of`, `insert_invite`, `lock_invite` and `update_invite` with:

```rust
    async fn lock_contact_pair(&mut self, a: UserId, b: UserId) -> DomainResult<()> {
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(locks::contact_pair(a, b))
            .execute(self.conn())
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn get_contact(
        &mut self,
        owner: UserId,
        peer: UserId,
    ) -> DomainResult<Option<ContactState>> {
        let state: Option<i16> = sqlx::query_scalar(
            "SELECT state FROM messaging.contact WHERE owner_id = $1 AND peer_id = $2",
        )
        .bind(owner.0)
        .bind(peer.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        state
            .map(|state| decode(state).map_err(store_error))
            .transpose()
    }

    async fn put_contact(
        &mut self,
        owner: UserId,
        peer: UserId,
        state: ContactState,
    ) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.contact (owner_id, peer_id, state) VALUES ($1, $2, $3) \
             ON CONFLICT (owner_id, peer_id) \
             DO UPDATE SET state = excluded.state, updated_at = now()",
        )
        .bind(owner.0)
        .bind(peer.0)
        .bind(encode(state))
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn delete_contact(&mut self, owner: UserId, peer: UserId) -> DomainResult<()> {
        sqlx::query("DELETE FROM messaging.contact WHERE owner_id = $1 AND peer_id = $2")
            .bind(owner.0)
            .bind(peer.0)
            .execute(self.conn())
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn contacts_of(&mut self, owner: UserId) -> DomainResult<Vec<(UserId, ContactState)>> {
        let rows: Vec<(Uuid, i16)> =
            sqlx::query_as("SELECT peer_id, state FROM messaging.contact WHERE owner_id = $1")
                .bind(owner.0)
                .fetch_all(self.conn())
                .await
                .map_err(store_error)?;
        rows.into_iter()
            .map(|(peer, state)| Ok((UserId(peer), decode(state).map_err(store_error)?)))
            .collect()
    }

    async fn insert_invite(&mut self, invite: InviteRecord) -> DomainResult<InsertOutcome> {
        let inserted = sqlx::query(
            "INSERT INTO messaging.invite \
             (code, user_id, expires_at, max_uses, uses, revoked_at) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (code) DO NOTHING",
        )
        .bind(&invite.code)
        .bind(invite.user_id.0)
        .bind(invite.expires_at)
        .bind(to_integer(invite.max_uses, "max_uses")?)
        .bind(to_integer(invite.uses, "uses")?)
        .bind(invite.revoked_at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(if inserted == 1 {
            InsertOutcome::Inserted
        } else {
            InsertOutcome::Duplicate
        })
    }

    async fn lock_invite(&mut self, code: &str) -> DomainResult<Option<InviteRecord>> {
        let row = sqlx::query(
            "SELECT code, user_id, expires_at, max_uses, uses, revoked_at \
             FROM messaging.invite WHERE code = $1 FOR UPDATE",
        )
        .bind(code)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::invite(&row).map_err(store_error))
            .transpose()
    }

    async fn update_invite(&mut self, invite: InviteRecord) -> DomainResult<()> {
        let updated =
            sqlx::query("UPDATE messaging.invite SET uses = $2, revoked_at = $3 WHERE code = $1")
                .bind(&invite.code)
                .bind(to_integer(invite.uses, "uses")?)
                .bind(invite.revoked_at)
                .execute(self.conn())
                .await
                .map_err(store_error)?
                .rows_affected();
        if updated == 0 {
            return Err(DomainError::Store(
                "update of an invite that does not exist".into(),
            ));
        }
        Ok(())
    }
```

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 292 tests pass (`push-domain` 22, `push-store` 6 unit and 14 `postgres`, `in_memory` 250), no warnings, no diff, the 1.85 build succeeds.

```powershell
git add Cargo.lock crates/push-store
git commit -m "feat(store): contacts and invites on Postgres"
```

---

### Task 3: Conversations and members

**Files:**
- Modify: `crates/push-store/src/rows.rs`, `locks.rs`, `store.rs`
- Modify: `crates/push-store/tests/postgres.rs`
- Modify: `crates/push-domain/src/store.rs` (a doc comment)

- [ ] **Step 1: Switch on the conversations cases and watch them fail**

In `crates/push-store/tests/postgres.rs`, replace the `READY` list with:

```rust
    const READY: &[&str] = &[
        "contract::transactions::",
        "contract::users::",
        "contract::contacts::",
        "contract::conversations::",
    ];
```

```powershell
cargo test -p push-store --test postgres
```

Expected: the 13 `contract::conversations::…` cases fail with `not implemented: plan 3B task 3`.

- [ ] **Step 2: Write the positions lock key with its test**

In `crates/push-store/src/locks.rs`, replace `use push_domain::UserId;` with `use push_domain::{ConversationId, UserId};`, add after the function `contact_pair`:

```rust
/// The lock serialising changes to a conversation's delivery and read positions. Its tag
/// namespaces it: sharing a key with another kind of lock is unlikely and only adds
/// waiting.
pub(crate) fn conversation_positions(conversation: ConversationId) -> i64 {
    fnv1a(b"conversation-positions", &[conversation.0])
}
```

and add this test at the end of its `tests` module:

```rust
    #[test]
    fn the_kinds_of_lock_use_different_keys() {
        let id = Uuid::now_v7();
        assert_ne!(
            conversation_positions(ConversationId(id)),
            fnv1a(b"contact-pair", &[id])
        );
    }
```

In `crates/push-domain/src/store.rs`, the doc comment of `lock_conversation_positions`
overstates what the tag gives. Replace its last two lines:

```rust
    /// namespaced (tagged) hash of the conversation id, so it cannot share a key with the
    /// contact-pair lock.
```

with:

```rust
    /// namespaced (tagged) hash of the conversation id, so sharing a key with another kind
    /// of lock, such as a contact pair's, is unlikely and would only add waiting.
```

- [ ] **Step 3: Read conversations and members from rows**

Replace `crates/push-store/src/rows.rs` with:

```rust
//! Reading domain records from result rows.

use push_domain::{Conversation, ConversationId, InviteRecord, Member, UserId};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::codes::decode;

/// The columns [`conversation`] reads, from a table aliased `c`.
macro_rules! conversation_columns {
    () => {
        "c.conversation_id, c.kind, c.direct_key, c.title, c.created_by, c.last_seq, \
         c.created_at, c.updated_at"
    };
}
pub(crate) use conversation_columns;

/// The columns [`member`] reads, from a table aliased `m`.
macro_rules! member_columns {
    () => {
        "m.conversation_id, m.user_id, m.role, m.visible_from_seq, m.left_at_seq, m.read_seq"
    };
}
pub(crate) use member_columns;

fn decode_error(message: String) -> sqlx::Error {
    sqlx::Error::Decode(message.into())
}

pub(crate) fn conversation(row: &PgRow) -> Result<Conversation, sqlx::Error> {
    let direct_key: Option<Vec<u8>> = row.try_get("direct_key")?;
    let direct_key = direct_key
        .map(|key| {
            <[u8; 32]>::try_from(key)
                .map_err(|key| decode_error(format!("direct_key has {} bytes", key.len())))
        })
        .transpose()?;
    Ok(Conversation {
        id: ConversationId(row.try_get("conversation_id")?),
        kind: decode(row.try_get("kind")?)?,
        direct_key,
        title: row.try_get("title")?,
        created_by: UserId(row.try_get("created_by")?),
        last_seq: row.try_get("last_seq")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

pub(crate) fn member(row: &PgRow) -> Result<Member, sqlx::Error> {
    Ok(Member {
        conversation_id: ConversationId(row.try_get("conversation_id")?),
        user_id: UserId(row.try_get("user_id")?),
        role: decode(row.try_get("role")?)?,
        visible_from_seq: row.try_get("visible_from_seq")?,
        left_at_seq: row.try_get("left_at_seq")?,
        read_seq: row.try_get("read_seq")?,
    })
}

pub(crate) fn invite(row: &PgRow) -> Result<InviteRecord, sqlx::Error> {
    let count = |column: &str| -> Result<u32, sqlx::Error> {
        let value: i32 = row.try_get(column)?;
        u32::try_from(value).map_err(|_| decode_error(format!("invite.{column} is {value}")))
    };
    Ok(InviteRecord {
        code: row.try_get("code")?,
        user_id: UserId(row.try_get("user_id")?),
        expires_at: row.try_get("expires_at")?,
        max_uses: count("max_uses")?,
        uses: count("uses")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}
```

- [ ] **Step 4: Implement the methods**

In `crates/push-store/src/store.rs`, replace the imports with:

```rust
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use push_domain::{
    ClientMsgId, ContactState, Conversation, ConversationId, ConversationOverview, DeviceId,
    DomainError, DomainResult, Entry, GroupPositions, InsertOutcome, InviteRecord, Member,
    MessageId, Store, Tx, UserId,
};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::codes::{decode, encode};
use crate::error::store_error;
use crate::locks;
use crate::rows::{self, conversation_columns, member_columns};
```

add this helper after `to_integer`:

```rust
/// Reads every row with `read`, failing like a query.
fn read_all<T>(
    rows: Vec<PgRow>,
    read: fn(&PgRow) -> Result<T, sqlx::Error>,
) -> DomainResult<Vec<T>> {
    rows.iter()
        .map(|row| read(row).map_err(store_error))
        .collect()
}
```

and replace the stubs of `find_direct`, `insert_conversation`, `get_conversation`, `next_seq`, `set_title`, `insert_member`, `get_member`, `members`, `memberships`, `raise_read_seq` and `lock_conversation_positions` with:

```rust
    async fn find_direct(&mut self, key: [u8; 32]) -> DomainResult<Option<ConversationId>> {
        let id: Option<Uuid> = sqlx::query_scalar(
            "SELECT conversation_id FROM messaging.conversation WHERE direct_key = $1",
        )
        .bind(&key[..])
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        Ok(id.map(ConversationId))
    }

    async fn insert_conversation(
        &mut self,
        conversation: Conversation,
    ) -> DomainResult<InsertOutcome> {
        // Only a direct key conflict is a duplicate: a conversation id that exists still
        // fails with a primary key violation.
        let inserted = sqlx::query(
            "INSERT INTO messaging.conversation \
             (conversation_id, kind, direct_key, title, created_by, last_seq, created_at, \
              updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (direct_key) DO NOTHING",
        )
        .bind(conversation.id.0)
        .bind(encode(conversation.kind))
        .bind(conversation.direct_key.map(|key| key.to_vec()))
        .bind(&conversation.title)
        .bind(conversation.created_by.0)
        .bind(conversation.last_seq)
        .bind(conversation.created_at)
        .bind(conversation.updated_at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(if inserted == 1 {
            InsertOutcome::Inserted
        } else {
            InsertOutcome::Duplicate
        })
    }

    async fn get_conversation(&mut self, id: ConversationId) -> DomainResult<Option<Conversation>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            conversation_columns!(),
            " FROM messaging.conversation c WHERE c.conversation_id = $1"
        ))
        .bind(id.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::conversation(&row).map_err(store_error))
            .transpose()
    }

    async fn next_seq(&mut self, id: ConversationId, at: DateTime<Utc>) -> DomainResult<i64> {
        // The row lock this takes is held until the transaction ends: one appender at a
        // time per conversation.
        let seq: Option<i64> = sqlx::query_scalar(
            "UPDATE messaging.conversation \
             SET last_seq = last_seq + 1, updated_at = greatest(updated_at, $2) \
             WHERE conversation_id = $1 RETURNING last_seq",
        )
        .bind(id.0)
        .bind(at)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        seq.ok_or(DomainError::ConversationNotFound)
    }

    async fn set_title(&mut self, id: ConversationId, title: String) -> DomainResult<()> {
        let updated =
            sqlx::query("UPDATE messaging.conversation SET title = $2 WHERE conversation_id = $1")
                .bind(id.0)
                .bind(title)
                .execute(self.conn())
                .await
                .map_err(store_error)?
                .rows_affected();
        if updated == 0 {
            return Err(DomainError::ConversationNotFound);
        }
        Ok(())
    }

    async fn insert_member(&mut self, member: Member) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.conversation_member \
             (conversation_id, user_id, role, visible_from_seq, left_at_seq, read_seq) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (conversation_id, user_id) DO UPDATE SET \
             role = excluded.role, visible_from_seq = excluded.visible_from_seq, \
             left_at_seq = excluded.left_at_seq, read_seq = excluded.read_seq",
        )
        .bind(member.conversation_id.0)
        .bind(member.user_id.0)
        .bind(encode(member.role))
        .bind(member.visible_from_seq)
        .bind(member.left_at_seq)
        .bind(member.read_seq)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn get_member(
        &mut self,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<Option<Member>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            member_columns!(),
            " FROM messaging.conversation_member m \
             WHERE m.conversation_id = $1 AND m.user_id = $2"
        ))
        .bind(conversation.0)
        .bind(user.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::member(&row).map_err(store_error))
            .transpose()
    }

    async fn members(&mut self, conversation: ConversationId) -> DomainResult<Vec<Member>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            member_columns!(),
            " FROM messaging.conversation_member m WHERE m.conversation_id = $1"
        ))
        .bind(conversation.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, rows::member)
    }

    async fn memberships(&mut self, user: UserId) -> DomainResult<Vec<(Conversation, Member)>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            conversation_columns!(),
            ", ",
            member_columns!(),
            " FROM messaging.conversation_member m \
             JOIN messaging.conversation c ON c.conversation_id = m.conversation_id \
             WHERE m.user_id = $1"
        ))
        .bind(user.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, |row| {
            Ok((rows::conversation(row)?, rows::member(row)?))
        })
    }

    async fn raise_read_seq(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        seq: i64,
    ) -> DomainResult<bool> {
        let moved = sqlx::query(
            "UPDATE messaging.conversation_member SET read_seq = $3 \
             WHERE conversation_id = $1 AND user_id = $2 AND read_seq < $3",
        )
        .bind(conversation.0)
        .bind(user.0)
        .bind(seq)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        if moved == 1 {
            return Ok(true);
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM messaging.conversation_member \
             WHERE conversation_id = $1 AND user_id = $2)",
        )
        .bind(conversation.0)
        .bind(user.0)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        if exists {
            Ok(false)
        } else {
            Err(DomainError::NotMember)
        }
    }

    async fn lock_conversation_positions(
        &mut self,
        conversation: ConversationId,
    ) -> DomainResult<()> {
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(locks::conversation_positions(conversation))
            .execute(self.conn())
            .await
            .map_err(store_error)?;
        Ok(())
    }
```

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 306 tests pass (`push-domain` 22, `push-store` 7 unit and 27 `postgres`, `in_memory` 250), no warnings, no diff, the 1.85 build succeeds.

```powershell
git add crates/push-store crates/push-domain
git commit -m "feat(store): conversations and members on Postgres"
```

---

### Task 4: The message log

**Files:**
- Modify: `crates/push-store/src/rows.rs`, `store.rs`
- Modify: `crates/push-store/tests/postgres.rs`
- Modify: `crates/push-domain/src/store.rs` (a doc comment)

- [ ] **Step 1: Switch on the entries cases and watch them fail**

In `crates/push-store/tests/postgres.rs`, replace the `READY` list with:

```rust
    const READY: &[&str] = &[
        "contract::transactions::",
        "contract::users::",
        "contract::contacts::",
        "contract::conversations::",
        "contract::entries::",
    ];
```

```powershell
cargo test -p push-store --test postgres
```

Expected: the 14 `contract::entries::…` cases fail with `not implemented: plan 3B task 4`.

- [ ] **Step 2: Read entries from rows**

Replace `crates/push-store/src/rows.rs` with:

```rust
//! Reading domain records from result rows.

use push_domain::{
    ClientMsgId, Conversation, ConversationId, DeviceId, Entry, InviteRecord, Member, MessageId,
    UserId,
};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::codes::decode;

/// The columns [`conversation`] reads, from a table aliased `c`.
macro_rules! conversation_columns {
    () => {
        "c.conversation_id, c.kind, c.direct_key, c.title, c.created_by, c.last_seq, \
         c.created_at, c.updated_at"
    };
}
pub(crate) use conversation_columns;

/// The columns [`member`] reads, from a table aliased `m`.
macro_rules! member_columns {
    () => {
        "m.conversation_id, m.user_id, m.role, m.visible_from_seq, m.left_at_seq, m.read_seq"
    };
}
pub(crate) use member_columns;

/// The columns [`entry`] reads, from a table aliased `e`.
macro_rules! entry_columns {
    () => {
        "e.conversation_id, e.seq, e.message_id, e.client_msg_id, e.kind, e.target_seq, \
         e.sender_id, e.sender_device_id, e.content_type, e.body, e.client_ts, e.server_ts, \
         e.edited_at, e.edit_seq, e.deleted_at"
    };
}
pub(crate) use entry_columns;

fn decode_error(message: String) -> sqlx::Error {
    sqlx::Error::Decode(message.into())
}

pub(crate) fn conversation(row: &PgRow) -> Result<Conversation, sqlx::Error> {
    let direct_key: Option<Vec<u8>> = row.try_get("direct_key")?;
    let direct_key = direct_key
        .map(|key| {
            <[u8; 32]>::try_from(key)
                .map_err(|key| decode_error(format!("direct_key has {} bytes", key.len())))
        })
        .transpose()?;
    Ok(Conversation {
        id: ConversationId(row.try_get("conversation_id")?),
        kind: decode(row.try_get("kind")?)?,
        direct_key,
        title: row.try_get("title")?,
        created_by: UserId(row.try_get("created_by")?),
        last_seq: row.try_get("last_seq")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

pub(crate) fn member(row: &PgRow) -> Result<Member, sqlx::Error> {
    Ok(Member {
        conversation_id: ConversationId(row.try_get("conversation_id")?),
        user_id: UserId(row.try_get("user_id")?),
        role: decode(row.try_get("role")?)?,
        visible_from_seq: row.try_get("visible_from_seq")?,
        left_at_seq: row.try_get("left_at_seq")?,
        read_seq: row.try_get("read_seq")?,
    })
}

pub(crate) fn entry(row: &PgRow) -> Result<Entry, sqlx::Error> {
    let sender_device_id: Option<uuid::Uuid> = row.try_get("sender_device_id")?;
    Ok(Entry {
        conversation_id: ConversationId(row.try_get("conversation_id")?),
        seq: row.try_get("seq")?,
        message_id: MessageId(row.try_get("message_id")?),
        client_msg_id: ClientMsgId(row.try_get("client_msg_id")?),
        kind: decode(row.try_get("kind")?)?,
        target_seq: row.try_get("target_seq")?,
        sender_id: UserId(row.try_get("sender_id")?),
        sender_device_id: sender_device_id.map(DeviceId),
        content_type: decode(row.try_get("content_type")?)?,
        body: row.try_get("body")?,
        client_ts: row.try_get("client_ts")?,
        server_ts: row.try_get("server_ts")?,
        edited_at: row.try_get("edited_at")?,
        edit_seq: row.try_get("edit_seq")?,
        deleted_at: row.try_get("deleted_at")?,
    })
}

pub(crate) fn invite(row: &PgRow) -> Result<InviteRecord, sqlx::Error> {
    let count = |column: &str| -> Result<u32, sqlx::Error> {
        let value: i32 = row.try_get(column)?;
        u32::try_from(value).map_err(|_| decode_error(format!("invite.{column} is {value}")))
    };
    Ok(InviteRecord {
        code: row.try_get("code")?,
        user_id: UserId(row.try_get("user_id")?),
        expires_at: row.try_get("expires_at")?,
        max_uses: count("max_uses")?,
        uses: count("uses")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}
```

- [ ] **Step 3: Implement the methods**

In `crates/push-store/src/store.rs`, replace the imports with:

```rust
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use push_domain::{
    ClientMsgId, ContactState, Conversation, ConversationId, ConversationOverview, DeviceId,
    DomainError, DomainResult, Entry, EntryKind, GroupPositions, InsertOutcome, InviteRecord,
    Member, MessageId, Store, Tx, UserId,
};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::codes::{decode, encode};
use crate::error::store_error;
use crate::locks;
use crate::rows::{self, conversation_columns, entry_columns, member_columns};
```

and replace the stubs of `insert_entry`, `entry_by_client_id`, `entry_by_message_id`, `update_entry`, `erase_edit_bodies`, `entries_after`, `count_incoming_messages`, `first_live_message` and `messages_before` with:

```rust
    async fn insert_entry(&mut self, entry: Entry) -> DomainResult<InsertOutcome> {
        // Only a client_msg_id conflict is a duplicate: a seq or message id that exists
        // still fails with a unique violation.
        let inserted = sqlx::query(
            "INSERT INTO messaging.message \
             (conversation_id, seq, message_id, client_msg_id, kind, target_seq, sender_id, \
              sender_device_id, content_type, body, client_ts, server_ts, edited_at, edit_seq, \
              deleted_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
             ON CONFLICT (conversation_id, client_msg_id) DO NOTHING",
        )
        .bind(entry.conversation_id.0)
        .bind(entry.seq)
        .bind(entry.message_id.0)
        .bind(entry.client_msg_id.0)
        .bind(encode(entry.kind))
        .bind(entry.target_seq)
        .bind(entry.sender_id.0)
        .bind(entry.sender_device_id.map(|device| device.0))
        .bind(encode(entry.content_type))
        .bind(entry.body)
        .bind(entry.client_ts)
        .bind(entry.server_ts)
        .bind(entry.edited_at)
        .bind(entry.edit_seq)
        .bind(entry.deleted_at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(if inserted == 1 {
            InsertOutcome::Inserted
        } else {
            InsertOutcome::Duplicate
        })
    }

    async fn entry_by_client_id(
        &mut self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
    ) -> DomainResult<Option<Entry>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            entry_columns!(),
            " FROM messaging.message e WHERE e.conversation_id = $1 AND e.client_msg_id = $2"
        ))
        .bind(conversation.0)
        .bind(client_msg_id.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::entry(&row).map_err(store_error))
            .transpose()
    }

    async fn entry_by_message_id(
        &mut self,
        conversation: ConversationId,
        message_id: MessageId,
    ) -> DomainResult<Option<Entry>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            entry_columns!(),
            " FROM messaging.message e WHERE e.conversation_id = $1 AND e.message_id = $2"
        ))
        .bind(conversation.0)
        .bind(message_id.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::entry(&row).map_err(store_error))
            .transpose()
    }

    async fn update_entry(&mut self, entry: Entry) -> DomainResult<()> {
        let updated = sqlx::query(
            "UPDATE messaging.message \
             SET body = $3, edited_at = $4, edit_seq = $5, deleted_at = $6 \
             WHERE conversation_id = $1 AND seq = $2",
        )
        .bind(entry.conversation_id.0)
        .bind(entry.seq)
        .bind(entry.body)
        .bind(entry.edited_at)
        .bind(entry.edit_seq)
        .bind(entry.deleted_at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        if updated == 0 {
            return Err(DomainError::Store(
                "update of an entry that does not exist".into(),
            ));
        }
        Ok(())
    }

    async fn erase_edit_bodies(
        &mut self,
        conversation: ConversationId,
        target_seq: i64,
    ) -> DomainResult<()> {
        sqlx::query(
            "UPDATE messaging.message SET body = NULL \
             WHERE conversation_id = $1 AND target_seq = $2 AND kind = $3 \
             AND body IS NOT NULL",
        )
        .bind(conversation.0)
        .bind(target_seq)
        .bind(encode(EntryKind::Edit))
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn entries_after(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        targets_from: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            entry_columns!(),
            " FROM messaging.message e \
             WHERE e.conversation_id = $1 AND e.seq > $2 AND e.seq <= $3 \
             AND (e.target_seq IS NULL OR e.target_seq >= $4) \
             ORDER BY e.seq LIMIT $5"
        ))
        .bind(conversation.0)
        .bind(after)
        .bind(up_to)
        .bind(targets_from)
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, rows::entry)
    }

    async fn count_incoming_messages(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        after: i64,
        up_to: i64,
        cap: u32,
    ) -> DomainResult<u32> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ( \
                SELECT 1 FROM messaging.message \
                WHERE conversation_id = $1 AND seq > $2 AND seq <= $3 \
                AND kind = 1 AND deleted_at IS NULL AND sender_id <> $4 \
                LIMIT $5 \
             ) incoming",
        )
        .bind(conversation.0)
        .bind(after)
        .bind(up_to)
        .bind(user.0)
        .bind(i64::from(cap))
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        u32::try_from(count).map_err(|_| DomainError::Store(format!("count {count} overflows")))
    }

    async fn first_live_message(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        not_from: Option<UserId>,
    ) -> DomainResult<Option<i64>> {
        sqlx::query_scalar(
            "SELECT seq FROM messaging.message \
             WHERE conversation_id = $1 AND seq > $2 AND seq <= $3 \
             AND kind = 1 AND deleted_at IS NULL \
             AND ($4::uuid IS NULL OR sender_id <> $4) \
             ORDER BY seq LIMIT 1",
        )
        .bind(conversation.0)
        .bind(after)
        .bind(up_to)
        .bind(not_from.map(|user| user.0))
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)
    }

    async fn messages_before(
        &mut self,
        conversation: ConversationId,
        from: i64,
        before: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        let rows = sqlx::query(concat!(
            "SELECT * FROM ( \
                SELECT ",
            entry_columns!(),
            " FROM messaging.message e \
                WHERE e.conversation_id = $1 AND e.seq >= $2 AND e.seq < $3 \
                AND e.kind IN (1, 4) \
                ORDER BY e.seq DESC LIMIT $4 \
             ) page ORDER BY seq"
        ))
        .bind(conversation.0)
        .bind(from)
        .bind(before)
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, rows::entry)
    }
```

`count_incoming_messages` counts inside a `LIMIT`ed subquery, so it stops at the cap; `first_live_message` and `messages_before` are single range scans of the primary key and the history index.

- [ ] **Step 4: Document the errors of `insert_entry`**

The contract says when `insert_entry` returns `Duplicate` but not when it fails. In
`crates/push-domain/src/store.rs`, replace the doc comment of `insert_entry`:

```rust
    /// `Duplicate` if the conversation already has an entry with this `client_msg_id`.
```

with:

```rust
    /// `Duplicate`, writing nothing, if the conversation already has an entry with this
    /// `client_msg_id`. Otherwise fails with `DomainError::Store` if an entry with this
    /// `message_id` exists (in any conversation) or the conversation already has an entry
    /// at this `seq`.
```

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 320 tests pass (`push-domain` 22, `push-store` 7 unit and 41 `postgres`, `in_memory` 250), no warnings, no diff, the 1.85 build succeeds.

```powershell
git add crates/push-store crates/push-domain
git commit -m "feat(store): the message log on Postgres"
```

---

### Task 5: Delivery positions and group positions

**Files:**
- Modify: `crates/push-store/src/store.rs`
- Modify: `crates/push-store/tests/postgres.rs`
- Modify: `crates/push-domain/src/store.rs` (a doc comment)

- [ ] **Step 1: Switch on the positions cases and watch them fail**

In `crates/push-store/tests/postgres.rs`, replace the `READY` list with:

```rust
    const READY: &[&str] = &[
        "contract::transactions::",
        "contract::users::",
        "contract::contacts::",
        "contract::conversations::",
        "contract::entries::",
        "contract::positions::",
    ];
```

```powershell
cargo test -p push-store --test postgres
```

Expected: the 9 `contract::positions::…` cases fail with `not implemented: plan 3B task 5`.

- [ ] **Step 2: Implement the methods**

In `crates/push-store/src/store.rs`, replace the imports with:

```rust
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use push_domain::{
    ClientMsgId, ContactState, Conversation, ConversationId, ConversationOverview, DeviceId,
    DomainError, DomainResult, Entry, EntryKind, GroupPositions, InsertOutcome, InviteRecord,
    Member, MessageId, Store, Tx, UserId,
};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{PgConnection, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::codes::{decode, encode};
use crate::error::store_error;
use crate::locks;
use crate::rows::{self, conversation_columns, entry_columns, member_columns};
```

and replace the stubs of `get_cursor`, `create_cursor_if_absent`, `raise_cursor`, `delivered_seq`, `member_positions`, `sync_positions` and `group_positions` with:

```rust
    async fn get_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
    ) -> DomainResult<Option<i64>> {
        sqlx::query_scalar(
            "SELECT delivered_seq FROM messaging.device_cursor \
             WHERE device_id = $1 AND conversation_id = $2",
        )
        .bind(device.0)
        .bind(conversation.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)
    }

    async fn create_cursor_if_absent(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.device_cursor (device_id, conversation_id, delivered_seq) \
             VALUES ($1, $2, $3) ON CONFLICT (device_id, conversation_id) DO NOTHING",
        )
        .bind(device.0)
        .bind(conversation.0)
        .bind(seq)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn raise_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<bool> {
        let moved = sqlx::query(
            "INSERT INTO messaging.device_cursor (device_id, conversation_id, delivered_seq) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (device_id, conversation_id) \
             DO UPDATE SET delivered_seq = excluded.delivered_seq \
             WHERE device_cursor.delivered_seq < excluded.delivered_seq",
        )
        .bind(device.0)
        .bind(conversation.0)
        .bind(seq)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(moved == 1)
    }

    async fn delivered_seq(
        &mut self,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<i64> {
        sqlx::query_scalar("SELECT messaging.delivered_seq($1, $2)")
            .bind(conversation.0)
            .bind(user.0)
            .fetch_one(self.conn())
            .await
            .map_err(store_error)
    }

    async fn member_positions(
        &mut self,
        conversation: ConversationId,
    ) -> DomainResult<Vec<(Member, i64)>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            member_columns!(),
            ", messaging.delivered_seq(m.conversation_id, m.user_id) AS delivered_seq \
             FROM messaging.conversation_member m WHERE m.conversation_id = $1"
        ))
        .bind(conversation.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, |row| {
            Ok((rows::member(row)?, row.try_get("delivered_seq")?))
        })
    }

    async fn sync_positions(
        &mut self,
        user: UserId,
        device: DeviceId,
    ) -> DomainResult<Vec<(Conversation, Member, i64)>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            conversation_columns!(),
            ", ",
            member_columns!(),
            ", position.seq AS position \
             FROM messaging.conversation_member m \
             JOIN messaging.conversation c ON c.conversation_id = m.conversation_id \
             LEFT JOIN messaging.device_cursor d \
                ON d.device_id = $2 AND d.conversation_id = m.conversation_id \
             CROSS JOIN LATERAL ( \
                SELECT greatest(coalesce(d.delivered_seq, 0), m.visible_from_seq - 1) AS seq \
             ) position \
             WHERE m.user_id = $1 AND position.seq < coalesce(m.left_at_seq, c.last_seq)"
        ))
        .bind(user.0)
        .bind(device.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, |row| {
            Ok((
                rows::conversation(row)?,
                rows::member(row)?,
                row.try_get("position")?,
            ))
        })
    }

    async fn group_positions(
        &mut self,
        conversation: ConversationId,
        last_seq: i64,
    ) -> DomainResult<Option<GroupPositions>> {
        let positions: Option<(i64, i64)> =
            sqlx::query_as("SELECT delivered, read FROM messaging.group_positions($1, $2)")
                .bind(conversation.0)
                .bind(last_seq)
                .fetch_optional(self.conn())
                .await
                .map_err(store_error)?;
        Ok(positions.map(|(delivered, read)| GroupPositions { delivered, read }))
    }
```

`raise_cursor` is one upsert that updates only upwards: `rows_affected` is 1 when the position was created or moved. `sync_positions` starts from the memberships and left-joins the device's positions, so a missing position counts as the floor. `group_positions` is the SQL function of the migration: one statement however many members the group has.

- [ ] **Step 3: Document `get_cursor`**

In `crates/push-domain/src/store.rs`, replace

```rust
    // --- Delivery positions ------------------------------------------------------------

    async fn get_cursor(
```

with:

```rust
    // --- Delivery positions ------------------------------------------------------------

    /// The device's delivery position in the conversation; `None` if it has none there.
    /// Revoked devices keep their positions.
    async fn get_cursor(
```

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 329 tests pass (`push-domain` 22, `push-store` 7 unit and 50 `postgres`, `in_memory` 250), no warnings, no diff, the 1.85 build succeeds.

```powershell
git add crates/push-store crates/push-domain
git commit -m "feat(store): delivery positions and group positions on Postgres"
```

---

### Task 6: Conversation overviews, and the whole suite on Postgres

**Files:**
- Modify: `crates/push-store/src/store.rs`
- Modify: `crates/push-store/tests/postgres.rs`
- Modify: `crates/push-testkit/src/scenarios/send.rs` (a doc comment)

- [ ] **Step 1: Run every case and watch the rest fail**

In `crates/push-store/tests/postgres.rs`, replace everything from the comment `// The parts of the suite the store implements so far; task 6 runs all of it.` through `let conclusion = push_testkit::run(backend, cases);` with:

```rust
    let conclusion = push_testkit::run(backend, push_testkit::cases());
```

```powershell
cargo test -p push-store --test postgres
```

Expected: 250 cases run and 14 fail with `not implemented: plan 3B task 6`: `contract::overviews::overviews_hold_what_a_listing_needs_for_every_membership`, the 11 `listings::…` scenarios, `groups::messages_from_before_joining_are_neither_unread_nor_changed_for_the_new_member` and `membership::a_former_member_lists_the_group_as_it_was_when_they_left`. The other 236 pass: every domain operation except listing already works on Postgres.

- [ ] **Step 2: Implement the overviews**

In `crates/push-store/src/store.rs`, replace the imports with:

```rust
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use push_domain::{
    ClientMsgId, ContactState, Conversation, ConversationId, ConversationOverview, DeviceId,
    DomainError, DomainResult, Entry, EntryKind, GroupPositions, InsertOutcome, InviteRecord,
    Member, MessageId, Store, Tx, UserId,
};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{PgConnection, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::codes::{decode, encode};
use crate::error::store_error;
use crate::locks;
use crate::rows::{self, conversation_columns, entry_columns, member_columns};
```

replace the stub of `conversation_overviews` with:

```rust
    async fn conversation_overviews(
        &mut self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
        unread_cap: u32,
    ) -> DomainResult<Vec<ConversationOverview>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            conversation_columns!(),
            ", ",
            member_columns!(),
            ", others.members, others.admins, \
             peer.user_id AS peer_user_id, peer.role AS peer_role, \
             peer.visible_from_seq AS peer_visible_from_seq, \
             peer.left_at_seq AS peer_left_at_seq, peer.read_seq AS peer_read_seq, \
             peer.delivered_seq AS peer_delivered_seq, \
             unread.count AS unread, \
             gp.delivered AS group_delivered, gp.read AS group_read, \
             left_entry.server_ts AS left_at \
             FROM messaging.conversation_member m \
             JOIN messaging.conversation c ON c.conversation_id = m.conversation_id \
             CROSS JOIN LATERAL ( \
                SELECT \
                    coalesce(array_agg(o.user_id ORDER BY o.user_id) \
                        FILTER (WHERE o.left_at_seq IS NULL), '{}') AS members, \
                    coalesce(array_agg(o.user_id ORDER BY o.user_id) \
                        FILTER (WHERE o.left_at_seq IS NULL AND o.role = 1), '{}') AS admins \
                FROM messaging.conversation_member o \
                WHERE o.conversation_id = m.conversation_id \
             ) others \
             LEFT JOIN LATERAL ( \
                SELECT p.user_id, p.role, p.visible_from_seq, p.left_at_seq, p.read_seq, \
                    messaging.delivered_seq(p.conversation_id, p.user_id) AS delivered_seq \
                FROM messaging.conversation_member p \
                WHERE c.kind = 1 AND p.conversation_id = m.conversation_id \
                    AND p.user_id <> m.user_id \
                LIMIT 1 \
             ) peer ON true \
             CROSS JOIN LATERAL ( \
                SELECT count(*) AS count FROM ( \
                    SELECT 1 FROM messaging.message e \
                    WHERE e.conversation_id = m.conversation_id \
                        AND e.seq > greatest(m.read_seq, m.visible_from_seq - 1) \
                        AND e.seq <= coalesce(m.left_at_seq, c.last_seq) \
                        AND e.kind = 1 AND e.deleted_at IS NULL AND e.sender_id <> m.user_id \
                    LIMIT $3 \
                ) incoming \
             ) unread \
             LEFT JOIN LATERAL ( \
                SELECT g.delivered, g.read \
                FROM messaging.group_positions(c.conversation_id, c.last_seq) g \
                WHERE c.kind = 2 \
             ) gp ON true \
             LEFT JOIN messaging.message left_entry \
                ON left_entry.conversation_id = m.conversation_id \
                AND left_entry.seq = m.left_at_seq \
             WHERE m.user_id = $1 AND ($2::timestamptz IS NULL OR c.updated_at > $2)"
        ))
        .bind(user.0)
        .bind(changed_since)
        .bind(i64::from(unread_cap))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, overview)
    }
```

and add this function at the end of the file:

```rust
/// One row of the `conversation_overviews` query.
fn overview(row: &PgRow) -> Result<ConversationOverview, sqlx::Error> {
    let conversation = rows::conversation(row)?;
    let users = |column: &str| -> Result<Vec<UserId>, sqlx::Error> {
        let ids: Vec<Uuid> = row.try_get(column)?;
        Ok(ids.into_iter().map(UserId).collect())
    };
    let peer_user_id: Option<Uuid> = row.try_get("peer_user_id")?;
    let peer = match peer_user_id {
        Some(peer_user_id) => Some((
            Member {
                conversation_id: conversation.id,
                user_id: UserId(peer_user_id),
                role: decode(row.try_get("peer_role")?)?,
                visible_from_seq: row.try_get("peer_visible_from_seq")?,
                left_at_seq: row.try_get("peer_left_at_seq")?,
                read_seq: row.try_get("peer_read_seq")?,
            },
            row.try_get("peer_delivered_seq")?,
        )),
        None => None,
    };
    let delivered: Option<i64> = row.try_get("group_delivered")?;
    let read: Option<i64> = row.try_get("group_read")?;
    let unread: i64 = row.try_get("unread")?;
    Ok(ConversationOverview {
        member: rows::member(row)?,
        members: users("members")?,
        admins: users("admins")?,
        peer,
        unread: u32::try_from(unread)
            .map_err(|_| sqlx::Error::Decode(format!("unread count {unread}").into()))?,
        group: delivered
            .zip(read)
            .map(|(delivered, read)| GroupPositions { delivered, read }),
        left_at: row.try_get("left_at")?,
        conversation,
    })
}
```

One statement for every membership of the user: the members and admins are aggregated in sorted arrays, the direct peer and its delivery position come from a `LATERAL` subquery that only direct conversations run, the unread count stops at the cap inside a `LIMIT`ed subquery, the group positions come from `messaging.group_positions` only for groups, and a former member's departure time is a primary-key lookup of the entry at `left_at_seq`.

- [ ] **Step 3: Say what the commit probe proves on Postgres**

`send::events_are_published_only_after_the_send_commits` now runs on Postgres too, where
`begin` never waits for the sender. In `crates/push-testkit/src/scenarios/send.rs`,
replace the last line of the doc comment of `CommitProbe`:

```rust
/// the new message is already visible.
```

with:

```rust
/// the new message is already visible. On Postgres an early publish is caught because an
/// independent READ COMMITTED transaction would still see `last_seq == 0`.
```

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 529 tests pass (`push-domain` 22, `push-store` 7 unit and 250 `postgres`, `in_memory` 250), no warnings, no diff, the 1.85 build succeeds. The `postgres` target takes about 10 seconds.

```powershell
git add crates/push-store crates/push-testkit
git commit -m "feat(store): conversation overviews; run the whole suite on Postgres"
```

---

### Task 7: Concurrency scenarios

**Files:**
- Create: `crates/push-testkit/src/concurrency.rs`
- Modify: `crates/push-testkit/src/lib.rs`, `crates/push-testkit/Cargo.toml`
- Modify: `crates/push-store/tests/postgres.rs`

The in-memory store serialises every transaction, so these scenarios mean something only
on Postgres (decision 9). They are written against the public API and pass as soon as
they run, because Plan 3A and Tasks 1–6 put the locks in place; Step 4 removes each lock
(and the owner re-check of `insert_device`) in turn to watch the scenario that depends on
it fail.

- [ ] **Step 1: Write the scenarios**

Create `crates/push-testkit/src/concurrency.rs`:

```rust
//! Scenarios that run operations at the same time. They need a store whose transactions
//! really run concurrently: the in-memory store serialises every transaction, and its
//! store-wide lock would deadlock the scenarios that hold two transactions open. Only
//! `push-store` runs them.

use std::sync::Arc;

use chrono::TimeDelta;
use push_domain::{
    ContactState, DeviceId, DomainError, Event, ReceiptUpdate, SendReceipt, Store, Tx, UserId,
};
use tokio::task::JoinSet;

use crate::{Backend, Case, World, case_list, seqs, text};

/// Runs every future at the same time, each on its own task, and returns their results
/// in the order given.
async fn all<T: Send + 'static>(futures: Vec<impl Future<Output = T> + Send + 'static>) -> Vec<T> {
    let mut set = JoinSet::new();
    for (index, future) in futures.into_iter().enumerate() {
        set.spawn(async move { (index, future.await) });
    }
    let mut results: Vec<(usize, T)> = set.join_all().await;
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

async fn concurrent_sends_stay_gapless_and_ordered<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    let sends = (0..20)
        .map(|index| {
            let world = Arc::clone(&world);
            let from = if index % 2 == 0 { alice } else { bob };
            async move {
                world
                    .svc
                    .send_text(text(&from, chat, &index.to_string()))
                    .await
            }
        })
        .collect();
    let receipts: Vec<SendReceipt> = all(sends).await.into_iter().map(Result::unwrap).collect();

    let mut stored: Vec<i64> = receipts.iter().map(|receipt| receipt.seq).collect();
    stored.sort();
    assert_eq!(stored, (1..=20).collect::<Vec<_>>());
    let synced = world.svc.sync(bob.device, 100).await.unwrap().entries;
    assert_eq!(seqs(&synced), (1..=20).collect::<Vec<_>>());
    for entry in synced {
        let receipt = receipts
            .iter()
            .find(|receipt| receipt.message_id == entry.message_id)
            .unwrap();
        assert_eq!(receipt.seq, entry.seq);
    }
}

async fn concurrent_retries_store_one_entry<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let command = text(&alice, chat, "once");

    let retries = (0..8)
        .map(|_| {
            let (world, command) = (Arc::clone(&world), command.clone());
            async move { world.svc.send_text(command).await }
        })
        .collect();
    let receipts = all(retries).await;

    let first = receipts[0].clone().unwrap();
    assert!(receipts.iter().all(|receipt| *receipt == Ok(first.clone())));
    assert_eq!(first.seq, 1);
    let synced = world.svc.sync(bob.device, 100).await.unwrap().entries;
    assert_eq!(seqs(&synced), [1]);
    assert_eq!(world.send(&bob, chat, "next").await.seq, 2);
}

async fn concurrent_open_direct_calls_create_one_conversation<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;

    let opens = (0..8)
        .map(|index| {
            let world = Arc::clone(&world);
            let (me, peer) = if index % 2 == 0 {
                (alice.user, bob.user)
            } else {
                (bob.user, alice.user)
            };
            async move { world.svc.open_direct(me, peer).await }
        })
        .collect();
    let ids = all(opens).await;

    let first = ids[0].clone().unwrap();
    assert!(ids.iter().all(|id| *id == Ok(first)));
    let listed = world
        .svc
        .list_conversations(alice.user, None)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
}

async fn a_concurrent_accept_never_undoes_a_block<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    for _ in 0..10 {
        let (alice, bob) = (world.person().await, world.person().await);
        world
            .svc
            .contact_request(alice.user, bob.user)
            .await
            .unwrap();

        let accept = {
            let world = Arc::clone(&world);
            async move { world.svc.contact_accept(bob.user, alice.user).await }
        };
        let block = {
            let world = Arc::clone(&world);
            async move { world.svc.contact_block(alice.user, bob.user).await }
        };
        let (accepted, blocked) = tokio::join!(tokio::spawn(accept), tokio::spawn(block));
        assert!(matches!(
            accepted.unwrap(),
            Ok(()) | Err(DomainError::NoPendingRequest)
        ));
        blocked.unwrap().unwrap();

        assert_eq!(
            world.svc.contact_state(alice.user, bob.user).await,
            Ok(Some(ContactState::Blocked))
        );
        assert_eq!(
            world.svc.contact_state(bob.user, alice.user).await,
            Ok(None)
        );
    }
}

async fn concurrent_redeems_never_exceed_the_use_limit<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let creator = world.person().await;
    let invite = world
        .svc
        .create_invite(creator.user, TimeDelta::days(1), 3)
        .await
        .unwrap();
    let mut redeemers = Vec::new();
    for _ in 0..8 {
        redeemers.push(world.person().await);
    }

    let redeems = redeemers
        .iter()
        .map(|redeemer| {
            let (world, code, user) = (Arc::clone(&world), invite.code.clone(), redeemer.user);
            async move { world.svc.redeem_invite(user, &code).await }
        })
        .collect();
    let results = all(redeems).await;

    let redeemed = results.iter().filter(|result| result.is_ok()).count();
    assert_eq!(redeemed, 3);
    assert!(
        results
            .iter()
            .all(|result| matches!(result, Ok(_) | Err(DomainError::InviteNotFound)))
    );
    let contacts = world.svc.list_contacts(creator.user).await.unwrap();
    assert_eq!(contacts.len(), 3);
}

async fn concurrent_acks_by_different_members_produce_the_group_receipt<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;

    for _ in 0..10 {
        let seq = world.send(&alice, group, "news").await.seq;
        world.notifier.take();

        let acks = [bob, carol]
            .into_iter()
            .map(|member| {
                let world = Arc::clone(&world);
                async move { world.svc.ack(member.device, group, seq).await }
            })
            .collect();
        assert!(all(acks).await.into_iter().all(|moved| moved == Ok(true)));

        let delivered = world.notifier.take().into_iter().any(|event| {
            matches!(
                event,
                Event::Receipt {
                    receipt: ReceiptUpdate::Group {
                        all_delivered_seq,
                        ..
                    },
                    ..
                } if all_delivered_seq == seq
            )
        });
        assert!(delivered, "no group receipt reached seq {seq}");
    }
}

async fn concurrent_registrations_of_one_device_give_it_one_owner<B: Backend>(backend: &B) {
    let world = Arc::new(World::new(backend).await);
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let users = [alice.user, bob.user];
    let chats = [
        world.direct_chat(&alice, &carol).await,
        world.direct_chat(&bob, &carol).await,
    ];

    for _ in 0..10 {
        let device = DeviceId::new();
        let registrations = users
            .into_iter()
            .map(|user| {
                let world = Arc::clone(&world);
                async move { world.svc.register_device(device, user).await }
            })
            .collect();
        let results = all(registrations).await;

        let winner = results
            .iter()
            .position(Result::is_ok)
            .expect("one registration succeeds");
        let loser = 1 - winner;
        assert_eq!(results[loser], Err(DomainError::UnknownDevice));
        assert_eq!(world.svc.identify(device, users[winner]).await, Ok(()));
        assert_eq!(
            world.svc.identify(device, users[loser]).await,
            Err(DomainError::UnknownDevice)
        );
        assert!(world.position(device, chats[winner]).await.is_some());
        assert_eq!(world.position(device, chats[loser]).await, None);
    }
}

async fn a_deadlock_is_reported_as_a_retryable_conflict<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let [a, b, c, d] = [(); 4].map(|()| UserId::new());
    let mut first = store.begin().await.unwrap();
    let mut second = store.begin().await.unwrap();
    first.lock_contact_pair(a, b).await.unwrap();
    second.lock_contact_pair(c, d).await.unwrap();

    // Each waits for the other's lock. The database aborts one of them, which gives its
    // locks back when it rolls back, so the other then gets its lock.
    let take = |mut tx: <B::Store as Store>::Tx, x: UserId, y: UserId| async move {
        match tx.lock_contact_pair(x, y).await {
            Ok(()) => Ok(tx),
            Err(error) => {
                tx.rollback().await.unwrap();
                Err(error)
            }
        }
    };
    let (first, second) = tokio::join!(take(first, c, d), take(second, a, b));

    let errors: Vec<DomainError> = [&first, &second]
        .into_iter()
        .filter_map(|result| result.as_ref().err().cloned())
        .collect();
    assert_eq!(errors, [DomainError::Conflict]);
    assert!(errors[0].is_retryable());
    for tx in [first, second].into_iter().flatten() {
        tx.rollback().await.unwrap();
    }
}

/// The scenarios that need a store with concurrent transactions.
pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        concurrent_sends_stay_gapless_and_ordered,
        concurrent_retries_store_one_entry,
        concurrent_open_direct_calls_create_one_conversation,
        a_concurrent_accept_never_undoes_a_block,
        concurrent_redeems_never_exceed_the_use_limit,
        concurrent_acks_by_different_members_produce_the_group_receipt,
        concurrent_registrations_of_one_device_give_it_one_owner,
        a_deadlock_is_reported_as_a_retryable_conflict,
    ]
    .into_iter()
    .map(|case| case.within("concurrency"))
    .collect()
}
```

In `crates/push-testkit/Cargo.toml`, replace the `tokio` line with:

```toml
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "time"] }
```

In `crates/push-testkit/src/lib.rs`, replace `mod contract;` with `mod concurrency;` followed by `mod contract;`, and add after the function `cases`:

```rust
/// The cases that run operations at the same time. They need a store whose transactions
/// run concurrently; the in-memory store cannot run them.
pub fn concurrency_cases<B: Backend>() -> Vec<Case<B>> {
    concurrency::cases()
}
```

- [ ] **Step 2: Run them on Postgres**

In `crates/push-store/tests/postgres.rs`, replace `let conclusion = push_testkit::run(backend, push_testkit::cases());` with:

```rust
    let mut cases = push_testkit::cases();
    cases.extend(push_testkit::concurrency_cases());
    let conclusion = push_testkit::run(backend, cases);
```

```powershell
cargo test -p push-store --test postgres -- concurrency::
```

Expected: 8 cases, all `ok`.

- [ ] **Step 3: Check that they do not flake**

```powershell
foreach ($run in 1..5) { cargo test -p push-store --test postgres -- concurrency:: }
```

Expected: five runs, each `8 passed`.

- [ ] **Step 4: Watch each scenario catch the bug it is for**

Make each of these changes in `crates/push-store/src/store.rs` on its own, run `cargo test -p push-store --test postgres -- concurrency::` (twice if the first run passes), check the named scenario fails, and undo the change with `git checkout -- crates/push-store/src/store.rs`:

| Change | Fails |
|---|---|
| In `lock_conversation_positions`, bind `locks::conversation_positions(ConversationId(Uuid::new_v4()))` instead of `locks::conversation_positions(conversation)` | `concurrent_acks_by_different_members_produce_the_group_receipt` |
| In `lock_contact_pair`, bind `locks::contact_pair(UserId::new(), UserId::new())` instead of `locks::contact_pair(a, b)` | `a_concurrent_accept_never_undoes_a_block`, `a_deadlock_is_reported_as_a_retryable_conflict` |
| In `lock_invite`, delete ` FOR UPDATE` from the query | `concurrent_redeems_never_exceed_the_use_limit` |
| In `insert_device`, replace the second statement (the `SELECT user_id = $2 …` re-check) with `Ok(true)` | `concurrent_registrations_of_one_device_give_it_one_owner` |
| Replace the whole body of `insert_device` with the single statement below, which re-checks inside the insert's own statement | `concurrent_registrations_of_one_device_give_it_one_owner` |

```rust
    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool> {
        sqlx::query_scalar(
            "WITH inserted AS ( \
                INSERT INTO messaging.member_device (device_id, user_id) VALUES ($1, $2) \
                ON CONFLICT (device_id) DO NOTHING RETURNING user_id \
             ) \
             SELECT coalesce((SELECT true FROM inserted), \
                (SELECT user_id = $2 AND revoked_at IS NULL \
                 FROM messaging.member_device WHERE device_id = $1))",
        )
        .bind(device.0)
        .bind(user.0)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)
    }
```

The single statement passes every contract case and scenario, but under READ COMMITTED its
snapshot is taken before the concurrent registration commits: the loser's `ON CONFLICT`
waits and does nothing, its re-check sees no row, and the registration fails with
`Store("… unexpected null …")` instead of `UnknownDevice`. That is why `insert_device`
re-checks in a second statement, which sees the winner's committed row.

Finish with `git diff --stat crates/push-store` printing nothing.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 537 tests pass (`push-domain` 22, `push-store` 7 unit and 258 `postgres`, `in_memory` 250), no warnings, no diff, the 1.85 build succeeds.

```powershell
git add Cargo.lock crates/push-testkit crates/push-store
git commit -m "test: concurrency scenarios on Postgres"
```

---

### Task 8: CI, spec, roadmap and pull request

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-10-11-push-messenger-design.md`
- Modify: `docs/superpowers/plans/2026-10-11-roadmap.md`

- [ ] **Step 1: Run the Postgres tests in CI**

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
      # push-store's tests start this Postgres image through the runner's Docker.
      - run: docker pull postgres:17-alpine
      - run: cargo test --workspace --locked

  msrv:
    runs-on: ubuntu-latest
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.85
      - uses: Swatinem/rust-cache@v2
      - run: docker pull postgres:17-alpine
      - run: cargo test --workspace --locked
```

- [ ] **Step 2: Tell developers about Docker**

In `README.md`, replace the `## Build and test` section with:

```markdown
## Build and test

    cargo test --workspace

Docker must be running: the `push-store` tests start a PostgreSQL container
(`postgres:17-alpine`) and give every test a database of its own. Building needs no
database.
```

- [ ] **Step 3: Record the schema decisions in the spec**

In §3.2 of the spec, add after the closing ```` ``` ```` of the `messaging` schema block (before `### 3.3 worker schema`):

```markdown
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
```

- [ ] **Step 4: Mark plan 3B in review**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the empty status cell at the end of the row for plan 3B (`| 3B | Postgres store | … | 3A | |`) with `In review`, so the row ends `| 3A | In review |`.

- [ ] **Step 5: Run the workspace gates**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 test --workspace --locked
```

Expected: no formatting diff, no warnings, 537 tests pass on both toolchains.

- [ ] **Step 6: Commit and push**

```powershell
git add .github README.md docs
git commit -m "docs: record plan 3B in the spec, roadmap and CI"
git push -u origin feature/plan-03b-postgres-store
```

- [ ] **Step 7: Open the pull request and wait for CI**

```powershell
gh pr create --base main --title "Plan 3B: Postgres store" --body "Implements docs/superpowers/plans/2026-10-12-plan-03b-postgres-store.md: push-store with the messaging schema as an embedded sqlx migration, every Tx method on PostgreSQL (runtime-checked queries, READ COMMITTED, row and advisory locks, ON CONFLICT DO NOTHING for duplicates, 40001/40P01 mapped to Conflict), set-based statements for group positions, listings, sync and conversation state, the whole push-testkit suite on testcontainers (postgres:17-alpine, one database per case) and eight concurrency scenarios."
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust` and `msrv` jobs, each running the Postgres tests). Do not merge; the controller merges after the final review.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §2 one writer per conversation: a row lock on the conversation for every append | 3 (`next_seq`), 7 |
| §3.2 every table, key and index of the messaging schema, with `invite.revoked_at` and the indexes the queries need | 1, 8 |
| §4.2 sending in one transaction; a retry rolls back and returns the original; `ON CONFLICT (conversation_id, client_msg_id) DO NOTHING` as the backstop | 3, 4, 7 |
| §4.3 delivery positions: created eagerly, raised with `GREATEST`, missing positions read as the floor | 5 |
| §4.4/§4.5 receipts, group-wide minimums, listings with unread counts — one statement each | 5, 6 |
| §4.6 edit and delete erasure (`erase_edit_bodies`, `update_entry`) | 4 |
| §4.7 `seq` gapless; positions only move forward | 3, 5, 7 |
| §5.5 contacts serialised per pair; invites with `FOR UPDATE` and a use limit that holds under concurrency | 2, 7 |
| Plan 3A `insert_device` contract: two concurrent registrations of one device id cannot both succeed for different users | 1, 7 |
| §12 integration layer on real Postgres via testcontainers; adverse test 2 (two identical sends at once: one row, no gap) | 1–7 |
| Plan 3 decisions: crate, sqlx runtime queries, embedded migrations, error mapping, `ON CONFLICT DO NOTHING`, `next_seq` statement, pair and positions advisory locks, `lock_invite` `FOR UPDATE`, conformance on both stores, concurrency tests, CI | 1–8 |

## Deferred to later plans

- **Production wiring** (Plan 4): connection URL and pool sizes from configuration, TLS
  to the database if it ever leaves the cluster, running `PgStore::migrate` at start-up or
  as a job, and the in-cluster PostgreSQL version (the tests use 17).
- **Retrying `Conflict`**: the domain reports deadlocks and serialization failures as the
  retryable `Conflict`; message-svc (Plan 7) decides how often to retry.
- **Set-based position creation**: `register_device` still inserts one position per
  membership, and `join` one per device of each new member (see Plan 3A).
- **Partitioning** `message` by `conversation_id` (spec §2) once a single table is too big.
- **`auth` and `worker` schemas** with their outboxes (Plans 5 and 6).
- Plan 2B deferrals that remain open: a former member sees the current title;
  `create_group` has no idempotency key; group changes carry no device; redeem rate
  limiting (Plan 8); the wire mapping (Plans 4, 7, 8).
