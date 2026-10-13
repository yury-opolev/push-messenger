# Plan 5A — Auth Service: the Service Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `push-auth`, the auth service of spec §5, as a tested binary: the `auth` schema in a database of its own (`push_auth`); signup with the same answer whether or not the email exists; stateless, HMAC-signed email links for verification and password reset (no token table); verification as the gate to every other API; login that returns a registration token; explicit device registration (at most 10 active devices); EdDSA access tokens published as a JWKS; refresh tokens stored as hashes and rotated with reuse detection; device listing and revocation; handles, the own profile and handle lookup; Argon2id at the OWASP minimum; rate limits in Redis; and an outbox that records every email and every user and device event in the same transaction as the change. Everything is tested against Postgres and Redis containers. Plan 5B adds auth-svc to the image and the chart and runs the smoke test through it on minikube.

**Architecture:** `push-auth` is a library plus a binary, like `push-message-svc`. An axum router (`api`) serves `/v1/auth/*`, `/v1/devices/*` and `/v1/users/*` as JSON, plus two small HTML pages that email links open. Handlers call `Auth` (`service`, `devices`, `profiles`), which runs SQL with sqlx on a pool made by `push_store::connect_pool` (the request-sized session timeouts of Plan 4A) and writes outbox rows in the same transaction as each change. Credentials live in small pure modules: `password` (Argon2id on blocking threads, bounded by a semaphore), `links` (HMAC-SHA256 signed claims with a purpose), `tokens` (Ed25519 JWTs with `jsonwebtoken`, the JWKS, refresh tokens). `limits` counts requests per IP and per account in Redis, keyed by hashes. `server` migrates with `push_store::run_migrations` on a dedicated connection, retries Postgres and Redis until they answer (interruptible by shutdown), serves `/healthz` and `/readyz`, and shuts down gracefully. message-svc and the gateway are not changed: auth-svc only writes `user.verified`, `device.registered`, `device.revoked` and `user.password_reset` to its outbox, which the worker relays to message-svc in Plan 6. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §2, §3.1, §5.1–§5.6, §7.6, §9, §12.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85). New: `argon2` 0.5 (0.5.3, feature `std`), `jsonwebtoken` 9 (9.3.1, default features off: the `ring` backend), `ring` 0.17, `hmac` 0.12, `sha2` 0.10, `base64` 0.22, `rand` 0.9, and the `json` feature of `sqlx` 0.8; for tests `tower` 0.5 (`util`) and `http-body-util` 0.1. Already in the workspace: `axum` 0.8, `redis` 1 (1.2.x), `tokio`, `tokio-util`, `tracing`, `testcontainers-modules` 0.13.

---

## Before you start

- **Plan 4B is merged** (PR #7). This plan adds the `push-auth` crate and two functions to
  `push-store`; it changes nothing else.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-05a-auth-service
```

- Fix a stale PATH for the current PowerShell session (each new shell needs it again):

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
```

- **Docker must be running.** The tests start `postgres:17-alpine` and `redis:7-alpine`
  containers, one or two per test, in parallel. A run killed half-way can leave
  containers behind (`docker ps -a`, then `docker rm -f <id>` for the `postgres` and
  `redis` ones only — never touch other containers on this machine).
- **minikube is not needed for this plan.** Plan 5B does the image, the chart and the
  minikube run.
- **Edit files with the edit tool, and keep them LF** (`.gitattributes`:
  `* text=auto eol=lf`). The file-creation tool may write CRLF; git normalises it on
  commit, which is harmless.
- **Do not pipe `cargo fmt`.** Run `cargo fmt --all -- --check` on its own and read the
  exit code. The code in this plan is formatted, so a diff means a copying mistake.
- Baseline (after Plan 4B): `cargo test --workspace --locked` passes 616 tests.
- **Cargo.lock.** Tasks that add dependencies first run `cargo test -p push-auth` without
  `--locked`, which records the new packages; the gates then run with `--locked`. Resolver 3
  with `rust-version = "1.85"` picks versions that build on 1.85 (here: `argon2` 0.5.3, not
  0.6, `jsonwebtoken` 9.3.1, `reqwest` comes in Plan 5B). Never run `cargo update`.
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
  tries IPv6 first and every connect costs about 2 s).
- **Scope rule:** no changes to `push-domain`, `push-testkit`, `push-proto`,
  `push-message-svc`, `push-gateway` or `push-smoke`; no Dockerfile, chart or scripts
  (Plan 5B); no email sending (Plan 6); no JWT checks in the gateway (Plan 8).

## Key rules the code must preserve

- **auth-svc owns one database and reads no other** (spec §2). Other services learn about
  users and devices only through its outbox.
- **Every change and its outbox event commit in one transaction.** Nothing is announced
  that did not happen, and nothing happens unannounced.
- **Verification is the gate** (spec §5.2): login and device registration refuse an
  unverified account with `email_not_verified`; an unverified account can only ask for the
  email again. Opening a verification link never signs anyone in.
- **No verification or reset token is stored.** Links are signed claims; a reset link
  binds a fingerprint of the password hash it replaces, so it works once.
- **Nothing secret is stored, logged or echoed in clear:** passwords only as Argon2id PHC
  strings, refresh tokens only as SHA-256, keys and database URLs only in the
  environment; `Config`, `LinkSigner` and `AccessTokens` have redacting `Debug`;
  configuration errors never show a URL or key; Redis keys hold hashes, never an email or
  an IP; logs never contain a token, a password or an email.
- **No answer reveals whether an email has an account:** signup, resend and forgot answer
  `202 {"status":"check_email"}` either way, and login answers `invalid_credentials` for an
  unknown email and a wrong password alike, after the same Argon2id work.
- **Links are built from the configured public URL**, never from a `Host` header; the
  pages they open send `Referrer-Policy: no-referrer` and change nothing on `GET`.
- **DEV ONLY switches are strict:** `PUSH_DEV_MAILBOX` accepts exactly `1` (on) or
  `0`/empty/unset (off), refuses anything else, and logs a warning at start-up when on.

## Decisions made for this plan

These were settled while writing the plan. Do not reopen them during execution.

1. **Plan 5 is split.** 5A (this plan) delivers the service, tested against containers;
   5B adds it to the image and the chart, extends the smoke test and verifies it on
   minikube. Each is its own pull request.
2. **A database of its own.** auth-svc migrates `push_auth` as its owner, with
   `push_store::run_migrations(url, &push_auth::MIGRATOR)` — `migrate_database` generalised
   to any migrator — on a one-connection pool, and serves requests from
   `push_store::connect_pool`, which `PgStore::connect` now uses too, so both services get
   the same session timeouts. The migration creates `citext`, which PostgreSQL trusts
   database owners with; Task 1 proves it as a plain owner role, the way the chart runs it.
3. **Signed tokens without state.** Email links and the registration token are
   `base64url(JSON claims).base64url(HMAC-SHA256)`, with domain separation
   (`push-link-v1.`) and a `purpose` (`verify`, `reset`, `register`) that is always
   checked. Verify links bind the email (spec §5.2), reset links `pw`, the first 16 bytes
   of SHA-256 of the password hash (spec §5.4). The registration token is such a token,
   not a JWT: the gateway never has the HMAC key, so it can never accept one.
4. **Access tokens are EdDSA JWTs** made with `jsonwebtoken` 9.3 on `ring` (already in the
   lock through sqlx). Version 10 makes you choose `aws-lc-rs` (needs cmake and a C
   toolchain on Windows) or `rust_crypto` (pulls RSA and elliptic-curve crates we do not
   use). The key is a 32-byte Ed25519 seed from the environment; the `kid` is its RFC 7638
   thumbprint; `GET /v1/auth/jwks` publishes it. Claims are exactly spec §5.1's, `aud`
   being the gateway's URL. Verification inside auth-svc has no leeway.
5. **Refresh tokens** are 32 random bytes (base64url), stored as SHA-256. A refresh
   marks the old row `rotated_to` the new hash; presenting a rotated token again revokes
   every refresh token of that device (reuse detection), so the device must sign in again.
   `login` with `device_id` revokes the device's earlier refresh tokens: a new sign-in
   replaces the old session.
6. **Signup answers alike for new and existing emails.** The password is hashed first,
   always. A new email gets an account and a `verify_email` email; an existing unverified
   one a fresh `verify_email` (its owner may have lost the first); an existing verified
   one an `account_exists` email. Nothing is changed for an existing email. A taken handle
   is reported (`handle_taken`): handles are public through lookup anyway.
7. **Link pages.** `GET /v1/auth/verify?token=…` and `GET /v1/auth/password/reset?token=…`
   return a minimal page whose button `POST`s the token as a form; a `GET` changes nothing,
   so a mail scanner that opens links does not act for the user. The same `POST` endpoints
   accept JSON for API clients (and the SDK). Pages carry `Referrer-Policy: no-referrer`,
   `Cache-Control: no-store`, `X-Content-Type-Options: nosniff` and a CSP of
   `default-src 'none'; form-action 'self'; frame-ancestors 'none'`.
8. **Password reset is in this plan** (roadmap: "stateless email links"). It revokes
   every refresh token of the account and writes `user.password_reset {user_id}`, a fifth
   outbox kind: spec §5.4 kicks every device without revoking them, which none of the four
   listed kinds says. The worker relays it in Plan 6.
9. **Devices reach message-svc only through the outbox** (spec §2): auth-svc writes
   `user.verified`, `device.registered` and `device.revoked`; Plan 6's worker relays them
   to a new internal message-svc API. message-svc and the gateway are not touched; the
   gateway's DEV ONLY identity stays until Plan 8, and the smoke test (Plan 5B) mirrors
   the enrolled user and device into message-svc through `DevSetup` until the worker does.
10. **Profiles.** `GET|PATCH /v1/users/me` and `GET /v1/users/lookup?handle=…` (verified
    users only; with or without `@`, any case). `GET /v1/users?ids=…` is deferred to
    Plan 7: it must answer only for users who share a contact or conversation with the
    caller, which only message-svc knows, and auth-svc never reads its database.
11. **Inputs.** Handles: 3–30 of `a-z`, `0-9`, `_`, starting with a letter, stored lower
    case. Display and device names: 1–64 characters after trimming, no control characters.
    Passwords: 8–128 characters, used as typed. Emails: one `@`, a dotted domain without
    empty or hyphen-edged labels, at most 254 bytes; the domain is lower-cased. Platforms:
    `ios`, `android`, `web`, `desktop`, `other`.
12. **Rate limits** (spec §5.6) are fixed windows: `INCR`, then `EXPIRE … NX`, in one
    `MULTI`, keyed `rl:{action}:{scope}:{first 16 bytes of SHA-256, hex}`. Per IP and per
    account: signup 20/h and 5/h, login 60 and 10 per 15 min, resend 20/h and 3/h, forgot
    20/h and 3/h, opening a link 60/h per IP, handle lookup 120/h per IP and 20/h per user
    (the spec's number). When Redis does not answer within 500 ms (or shutdown began) the
    request goes ahead and a warning is logged: Redis is a doorbell (spec §2), and
    Argon2id keeps guessing slow. The client IP is the configured header (`x-real-ip`,
    which ingress-nginx overwrites), else the peer address. Refresh is not limited.
13. **Argon2id** `m=19456 KiB, t=2, p=1` as a PHC string; a hash with other parameters
    still verifies and is replaced on the next login. Hashing runs on `spawn_blocking`,
    at most `PUSH_AUTH_HASH_CONCURRENCY` (default 2) at once, since each takes 19 MiB.
    Login checks a dummy hash for an unknown email. The workspace builds `argon2` and
    `blake2` with `opt-level = 3` in the dev profile, so tests hash in milliseconds.
14. **The device limit** (10 active) is counted under `SELECT … FOR UPDATE` on the account
    row, so concurrent registrations cannot pass it together (Task 5 proves it with 13 at
    once).
15. **Errors** are `{"error": {"code", "message", "retryable"}}` with codes `invalid`,
    `handle_taken`, `invalid_credentials`, `email_not_verified`, `link_invalid`,
    `link_expired`, `unauthorized`, `token_expired`, `too_many_devices`,
    `device_not_found`, `user_not_found`, `rate_limited` (429 with `Retry-After`) and
    `unavailable` (503). 401s carry `WWW-Authenticate: Bearer`. Every answer is
    `Cache-Control: no-store` except the JWKS (`public, max-age=300`). Bodies are at most
    16 KiB.
16. **The DEV ONLY mailbox.** With `PUSH_DEV_MAILBOX=1`, `GET /dev/mailbox?to=…` returns
    the `email.send` payloads waiting in the outbox for one address, newest first. It is
    how a local cluster and the smoke test open verification links before the worker sends
    email (Plan 6). Its path is outside `/v1`, so the ingress of Plan 5B never routes it,
    and the chart's dev-mode guard (Plan 5B) decides whether it is on.
17. **Ids are UUIDv7** (time-ordered), like the domain's: users, devices, outbox events,
    access-token `jti`s.
18. **`citext` comparisons cast the parameter** (`email = $1::citext`). sqlx binds strings
    as `text`, and `citext = text` resolves to the case-sensitive `text = text`; found while
    writing this plan (a second signup in other case failed to find the account).

## File structure

```
Cargo.toml                                  members, dependencies, argon2/blake2 opt-level
crates/push-store/src/lib.rs                + run_migrations, connect_pool
crates/push-store/src/store.rs              PgStore::connect uses connect_pool
crates/push-auth/                           NEW
├── Cargo.toml
├── migrations/0001_auth.sql                the auth schema (spec §3.1)
├── src/
│   ├── lib.rs                              modules and exports
│   ├── main.rs                             the binary
│   ├── db.rs                               MIGRATOR, migrate_database
│   ├── password.rs                         Argon2id
│   ├── rules.rs                            emails, handles, names, passwords, platforms
│   ├── links.rs                            HMAC-signed links and registration tokens
│   ├── tokens.rs                           EdDSA access tokens, JWKS, refresh tokens
│   ├── error.rs                            AuthError: code, status, retryable
│   ├── outbox.rs                           outbox events and their payloads
│   ├── service.rs                          Auth: signup, verify, resend, login, reset
│   ├── devices.rs                          register, sign in again, refresh, list, revoke
│   ├── profiles.rs                         own profile, display name, lookup
│   ├── limits.rs                           rate limits: Redis and in-memory counters
│   ├── api.rs                              the HTTP API and the link pages
│   ├── config.rs                           Config from the environment
│   └── server.rs                           serve, health, logging, signals
└── tests/
    ├── schema.rs                           the migration as a plain owner, constraints
    ├── accounts.rs                         signup, verification, login, reset
    ├── devices.rs                          devices, tokens, profiles
    ├── limits.rs                           Redis counters
    ├── api.rs                              the HTTP API through the router
    └── server.rs                           start-up, readiness, shutdown
README.md                                   push-auth configuration
docs/superpowers/specs/2026-10-11-push-messenger-design.md   §3.1 and §5 notes
docs/superpowers/plans/2026-10-11-roadmap.md                 5A in review
```

---

### Task 1: The `push-auth` crate and the `auth` schema

**Files:**
- Modify: `Cargo.toml`, `crates/push-store/src/lib.rs`, `crates/push-store/src/store.rs`
- Create: `crates/push-auth/Cargo.toml`, `migrations/0001_auth.sql`, `src/lib.rs`,
  `src/db.rs`, `tests/schema.rs`

- [ ] **Step 1: Let any service migrate its own database and share the request pool**

In `crates/push-store/src/lib.rs`, `migrate_database` becomes a call of the new
`run_migrations`, and `connect_pool` takes over the pool-building code of
`PgStore::connect`. Replace the file with:

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

/// Checks that `url` parses as a PostgreSQL connection URL, without connecting. The error
/// never contains the URL, which carries the password.
pub fn check_database_url(url: &str) -> Result<(), String> {
    // sqlx parses any scheme as PostgreSQL options, so the scheme is checked here.
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return Err("not a valid PostgreSQL connection URL".to_owned());
    }
    url.parse::<sqlx::postgres::PgConnectOptions>()
        .map(drop)
        .map_err(|_| "not a valid PostgreSQL connection URL".to_owned())
}

/// Applies the migrations the database at `url` does not have yet, on a connection of its
/// own: a migration must not inherit the pool's statement and lock timeouts, which are
/// sized for requests. Run it as the owner of the database.
///
/// This is the entry point services use: at start-up they call
/// `push_store::migrate_database(url)` on a dedicated connection, not `PgStore::migrate`
/// on the request pool.
pub async fn migrate_database(url: &str) -> Result<(), sqlx::migrate::MigrateError> {
    run_migrations(url, &MIGRATOR).await
}

/// Applies `migrator`'s migrations that the database at `url` does not have yet, the way
/// [`migrate_database`] does: on a one-connection pool of its own, without the session
/// timeouts of [`connect_pool`]. Every service migrates its own database with it.
pub async fn run_migrations(
    url: &str,
    migrator: &sqlx::migrate::Migrator,
) -> Result<(), sqlx::migrate::MigrateError> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(url)
        .await?;
    let result = migrator.run(&pool).await;
    pool.close().await;
    result
}

/// Connects a request pool to `url` as `options` say: every connection the pool opens
/// first sets the lock, statement and idle-in-transaction timeouts. Fails if the options
/// do not pass [`StoreOptions::validate`] or the first connection cannot be made.
pub async fn connect_pool(url: &str, options: &StoreOptions) -> Result<sqlx::PgPool, sqlx::Error> {
    use sqlx::Executor;

    options
        .validate()
        .map_err(|message| sqlx::Error::Configuration(message.into()))?;
    let settings = options.session_settings();
    sqlx::postgres::PgPoolOptions::new()
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
        .await
}
```

In `crates/push-store/src/store.rs`, replace the two `sqlx` imports (lines 10–11) with:

```rust
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{PgConnection, Postgres, Row, Transaction};
```

and replace the body of `PgStore::connect`, keeping its doc comment, so that it reads:

```rust
    /// Connects a pool to `url` as `options` say. Every connection the pool opens first
    /// sets the lock, statement and idle-in-transaction timeouts. Fails if the options do
    /// not pass [`StoreOptions::validate`] or the first connection cannot be made.
    pub async fn connect(url: &str, options: &StoreOptions) -> Result<Self, sqlx::Error> {
        Ok(Self::new(crate::connect_pool(url, options).await?))
    }
```

```powershell
cargo test -p push-store --test connect
```

Expected: `test result: ok. 4 passed` — the pool behaves as before.

- [ ] **Step 2: Add the crate**

```powershell
New-Item -ItemType Directory crates\push-auth\src, crates\push-auth\migrations, crates\push-auth\tests -Force
```

Replace `Cargo.toml` with:

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-auth",
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
push-auth = { path = "crates/push-auth" }
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

Create `crates/push-auth/Cargo.toml`:

```toml
[package]
name = "push-auth"
description = "The push-messenger auth service: accounts, email links, devices and tokens."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
push-store.workspace = true
sqlx.workspace = true

[dev-dependencies]
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
uuid.workspace = true

[lints]
workspace = true
```

- [ ] **Step 3: Write the failing schema tests**

The tests create a plain role `push_auth` that owns a database `push_auth` — what Plan 5B's
chart does — and migrate as that role, so they also prove that a database owner may create
`citext`. Create `crates/push-auth/tests/schema.rs`:

```rust
//! The `auth` schema on a real PostgreSQL (Docker must be running): the migrations run as
//! the database's owner, a plain role like the chart's, and the constraints hold.

use push_auth::{MIGRATOR, migrate_database};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

/// A running Postgres with a database `push_auth` owned by the plain role `push_auth`,
/// and the URL to connect as that role.
async fn auth_database() -> (ContainerAsync<Postgres>, String) {
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
    let admin = PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
    ))
    .await
    .expect("connect as the superuser");
    for statement in [
        "CREATE ROLE push_auth LOGIN PASSWORD 'auth-secret'",
        "CREATE DATABASE push_auth OWNER push_auth",
    ] {
        sqlx::query(statement)
            .execute(&admin)
            .await
            .expect(statement);
    }
    admin.close().await;
    let url = format!("postgres://push_auth:auth-secret@127.0.0.1:{port}/push_auth");
    (container, url)
}

async fn migrated(url: &str) -> PgPool {
    migrate_database(url).await.expect("migrate");
    PgPool::connect(url).await.expect("connect")
}

async fn insert_user(pool: &PgPool, email: &str, handle: &str) -> Result<Uuid, sqlx::Error> {
    let user = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.user_account (user_id, email, handle, display_name, password_hash) \
         VALUES ($1, $2, $3, 'Someone', 'hash')",
    )
    .bind(user)
    .bind(email)
    .bind(handle)
    .execute(pool)
    .await?;
    Ok(user)
}

fn constraint(error: sqlx::Error) -> String {
    error
        .as_database_error()
        .and_then(|database| database.constraint())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn the_owner_migrates_and_a_second_run_changes_nothing() {
    let (_container, url) = auth_database().await;
    migrate_database(&url).await.expect("first run");
    let pool = migrated(&url).await;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .expect("count the applied migrations");
    assert_eq!(applied, MIGRATOR.iter().count() as i64);
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables \
         WHERE table_schema = 'auth' ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await
    .expect("list the tables");
    assert_eq!(
        tables,
        ["device", "outbox", "refresh_token", "user_account"]
    );
}

#[tokio::test]
async fn emails_are_unique_ignoring_case_and_handles_unique() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    insert_user(&pool, "Alice@Example.com", "alice")
        .await
        .expect("the first account");
    let error = insert_user(&pool, "alice@example.COM", "alice2")
        .await
        .expect_err("the same email in other case");
    assert_eq!(constraint(error), "user_account_email_key");
    let error = insert_user(&pool, "bob@example.com", "alice")
        .await
        .expect_err("a taken handle");
    assert_eq!(constraint(error), "user_account_handle_key");
}

#[tokio::test]
async fn malformed_handles_and_platforms_are_refused() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    for handle in [
        "al",
        "9lives",
        "has-dash",
        "Alice",
        "a_very_long_handle_over_thirty_c",
    ] {
        let error = insert_user(&pool, &format!("{}@example.com", Uuid::now_v7()), handle)
            .await
            .expect_err(handle);
        assert_eq!(constraint(error), "user_account_handle_check", "{handle}");
    }
    let user = insert_user(&pool, "carol@example.com", "carol")
        .await
        .expect("a valid account");
    let error = sqlx::query(
        "INSERT INTO auth.device (device_id, user_id, name, platform) \
         VALUES ($1, $2, 'Phone', 'toaster')",
    )
    .bind(Uuid::now_v7())
    .bind(user)
    .execute(&pool)
    .await
    .expect_err("an unknown platform");
    assert_eq!(constraint(error), "device_platform_check");
}
```

Create `crates/push-auth/src/lib.rs` with only its doc comment for now:

```rust
//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.
```

```powershell
cargo test -p push-auth --test schema
```

Expected: a compile error — `unresolved imports push_auth::MIGRATOR, push_auth::migrate_database`.

- [ ] **Step 4: The migration and the migrator**

Create `crates/push-auth/migrations/0001_auth.sql`:

```sql
-- The auth schema (spec §3.1), owned by auth-svc in its own database, push_auth.
--
-- Emails and handles are citext, so uniqueness ignores case. Handles are stored in lower
-- case; the CHECK compares the text value, since a citext regex match ignores case.
-- Verification links, password-reset links and registration tokens are signed and never
-- stored (spec §5.1); refresh tokens are stored as SHA-256 hashes only.

CREATE EXTENSION IF NOT EXISTS citext;

CREATE SCHEMA auth;

CREATE TABLE auth.user_account (
    user_id           uuid PRIMARY KEY,
    email             citext NOT NULL UNIQUE CHECK (length(email) BETWEEN 3 AND 254),
    -- Null until the verification link is opened: no API access before that.
    email_verified_at timestamptz,
    handle            citext NOT NULL UNIQUE CHECK (handle::text ~ '^[a-z][a-z0-9_]{2,29}$'),
    display_name      text NOT NULL CHECK (length(display_name) BETWEEN 1 AND 64),
    -- Argon2id PHC string.
    password_hash     text NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE auth.device (
    device_id    uuid PRIMARY KEY,
    user_id      uuid NOT NULL REFERENCES auth.user_account,
    name         text NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    platform     text NOT NULL CHECK (platform IN ('ios', 'android', 'web', 'desktop', 'other')),
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz,
    revoked_at   timestamptz
);
-- The device limit counts a user's active devices.
CREATE INDEX device_active_user ON auth.device (user_id) WHERE revoked_at IS NULL;

CREATE TABLE auth.refresh_token (
    -- SHA-256 of the raw token, which is never stored.
    token_hash bytea PRIMARY KEY CHECK (length(token_hash) = 32),
    device_id  uuid NOT NULL REFERENCES auth.device,
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    -- The token that replaced this one; presenting a rotated token again is reuse.
    rotated_to bytea
);
CREATE INDEX refresh_token_device ON auth.refresh_token (device_id);

-- Events for other services, written in the same transaction as the change. The worker
-- (Plan 6) reads and deletes them; nothing else reads this table.
CREATE TABLE auth.outbox (
    event_id   uuid PRIMARY KEY,
    kind       text NOT NULL CHECK (kind IN (
                   'email.send', 'user.verified', 'user.password_reset',
                   'device.registered', 'device.revoked')),
    payload    jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX outbox_created ON auth.outbox (created_at);
```

Create `crates/push-auth/src/db.rs`:

```rust
//! The `auth` schema's migrations.

/// The migrations of the `auth` schema, embedded at build time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Applies the migrations the `push_auth` database at `url` does not have yet, on a
/// connection of its own (see [`push_store::run_migrations`]). Run it as the owner of the
/// database: it creates the `citext` extension, which PostgreSQL trusts database owners
/// with.
pub async fn migrate_database(url: &str) -> Result<(), sqlx::migrate::MigrateError> {
    push_store::run_migrations(url, &MIGRATOR).await
}
```

Replace `crates/push-auth/src/lib.rs` with:

```rust
//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.

mod db;

pub use db::{MIGRATOR, migrate_database};
```

- [ ] **Step 5: Run the tests**

```powershell
cargo test -p push-auth
```

Expected: cargo records `push-auth` in `Cargo.lock` on the first run, then
`test result: ok. 3 passed` for `tests\schema.rs`.

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.toml Cargo.lock crates/push-store crates/push-auth
git commit -m "feat(auth): push-auth crate with the auth schema"
```

Expected: 619 tests pass, no warnings, no diff.

---

### Task 2: Argon2id passwords and input rules

**Files:**
- Modify: `Cargo.toml`, `crates/push-auth/Cargo.toml`, `crates/push-auth/src/lib.rs`
- Create: `crates/push-auth/src/password.rs`, `src/rules.rs`

- [ ] **Step 1: Dependencies**

Replace `Cargo.toml` with (adds `argon2` and the `opt-level` overrides that keep Argon2id
fast in tests):

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-auth",
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
argon2 = { version = "0.5", features = ["std"] }
async-trait = "0.1"
axum = "0.8"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
futures-util = { version = "0.3", default-features = false, features = ["sink"] }
libtest-mimic = "0.8"
prost = "0.14"
protoc-bin-vendored = "3"
push-auth = { path = "crates/push-auth" }
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

# Argon2id is far too slow unoptimised, and the auth tests hash many passwords.
[profile.dev.package.argon2]
opt-level = 3

[profile.dev.package.blake2]
opt-level = 3

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

Replace `crates/push-auth/Cargo.toml` with:

```toml
[package]
name = "push-auth"
description = "The push-messenger auth service: accounts, email links, devices and tokens."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
argon2.workspace = true
push-store.workspace = true
serde.workspace = true
sqlx.workspace = true
tracing.workspace = true

[dev-dependencies]
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
uuid.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Passwords, with their tests**

Create `crates/push-auth/src/password.rs`:

```rust
//! Password hashing with Argon2id (spec §5.6).
//!
//! Hashes are PHC strings, so they carry their parameters: a hash made with weaker
//! parameters than the current ones still verifies, and [`Checked::Right`] says when to
//! store a new hash. Hashing takes about as long as it is configured to (tens to hundreds
//! of milliseconds) and 19 MiB of memory at the OWASP minimum: callers run it on a
//! blocking thread and bound how many run at once.

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};

/// The outcome of checking a password against a stored hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checked {
    /// The password does not match, or the stored hash is unreadable.
    Wrong,
    /// The password matches. `rehash` is true when the stored hash was made with other
    /// parameters than the current ones and should be replaced.
    Right { rehash: bool },
}

/// Hashes and checks passwords with one set of Argon2id parameters.
#[derive(Clone)]
pub struct Passwords {
    params: Params,
    /// A hash of a random password, checked when an account does not exist, so that an
    /// unknown email costs as much time as a wrong password.
    dummy: String,
}

impl std::fmt::Debug for Passwords {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Passwords")
            .field("memory_kib", &self.params.m_cost())
            .field("iterations", &self.params.t_cost())
            .field("parallelism", &self.params.p_cost())
            .finish()
    }
}

impl Passwords {
    /// Memory cost of the OWASP minimum, in KiB.
    pub const OWASP_MEMORY_KIB: u32 = 19_456;
    /// Iterations of the OWASP minimum.
    pub const OWASP_ITERATIONS: u32 = 2;
    /// Parallelism of the OWASP minimum.
    pub const OWASP_PARALLELISM: u32 = 1;

    /// Argon2id with `memory_kib` KiB, `iterations` passes and `parallelism` lanes.
    pub fn new(memory_kib: u32, iterations: u32, parallelism: u32) -> Result<Self, String> {
        let params = Params::new(memory_kib, iterations, parallelism, None)
            .map_err(|error| format!("invalid Argon2id parameters: {error}"))?;
        let mut passwords = Self {
            params,
            dummy: String::new(),
        };
        passwords.dummy = passwords.hash(&random_password());
        Ok(passwords)
    }

    /// The OWASP minimum, `m=19456 KiB, t=2, p=1`, which the spec requires.
    pub fn owasp() -> Self {
        Self::new(
            Self::OWASP_MEMORY_KIB,
            Self::OWASP_ITERATIONS,
            Self::OWASP_PARALLELISM,
        )
        .expect("the OWASP parameters are valid")
    }

    fn argon2(&self) -> Argon2<'_> {
        Argon2::new(Algorithm::Argon2id, Version::V0x13, self.params.clone())
    }

    /// A PHC string for `password` with a new random salt.
    pub fn hash(&self, password: &str) -> String {
        let salt = SaltString::generate(&mut OsRng);
        self.argon2()
            .hash_password(password.as_bytes(), &salt)
            .expect("Argon2id hashing with valid parameters cannot fail")
            .to_string()
    }

    /// Checks `password` against the PHC string `stored`.
    pub fn check(&self, password: &str, stored: &str) -> Checked {
        let Ok(parsed) = PasswordHash::new(stored) else {
            tracing::error!("a stored password hash is not a PHC string");
            return Checked::Wrong;
        };
        if self
            .argon2()
            .verify_password(password.as_bytes(), &parsed)
            .is_err()
        {
            return Checked::Wrong;
        }
        let current = parsed.algorithm.as_str() == "argon2id"
            && parsed.version == Some(Version::V0x13.into())
            && Params::try_from(&parsed).is_ok_and(|params| {
                params.m_cost() == self.params.m_cost()
                    && params.t_cost() == self.params.t_cost()
                    && params.p_cost() == self.params.p_cost()
            });
        Checked::Right { rehash: !current }
    }

    /// Spends the time of one check without an account, so that "no such account" and
    /// "wrong password" take equally long.
    pub fn check_nothing(&self, password: &str) {
        let _ = self.check(password, &self.dummy);
    }
}

fn random_password() -> String {
    SaltString::generate(&mut OsRng).as_str().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap parameters, so the tests stay fast.
    fn cheap() -> Passwords {
        Passwords::new(64, 1, 1).unwrap()
    }

    #[test]
    fn a_hash_is_an_argon2id_phc_string_with_the_given_parameters() {
        let hash = cheap().hash("correct horse");
        assert!(hash.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "{hash}");
        assert_ne!(
            hash,
            cheap().hash("correct horse"),
            "every hash has its own salt"
        );
    }

    #[test]
    fn the_right_password_matches_and_a_wrong_one_does_not() {
        let passwords = cheap();
        let hash = passwords.hash("correct horse");
        assert_eq!(
            passwords.check("correct horse", &hash),
            Checked::Right { rehash: false }
        );
        assert_eq!(passwords.check("correct horsE", &hash), Checked::Wrong);
        assert_eq!(
            passwords.check("correct horse", "not a hash"),
            Checked::Wrong
        );
    }

    #[test]
    fn a_hash_with_other_parameters_matches_and_asks_for_a_rehash() {
        let old = cheap().hash("correct horse");
        let stronger = Passwords::new(128, 2, 1).unwrap();
        assert_eq!(
            stronger.check("correct horse", &old),
            Checked::Right { rehash: true }
        );
    }

    #[test]
    fn the_owasp_parameters_are_the_spec_minimum() {
        let hash = Passwords::owasp().hash("correct horse");
        assert!(
            hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "{hash}"
        );
    }

    #[test]
    fn invalid_parameters_are_refused() {
        assert!(Passwords::new(0, 1, 1).is_err());
        assert!(Passwords::new(64, 0, 1).is_err());
    }

    #[test]
    fn debug_output_shows_the_parameters_only() {
        let shown = format!("{:?}", cheap());
        assert_eq!(
            shown,
            "Passwords { memory_kib: 64, iterations: 1, parallelism: 1 }"
        );
    }
}
```

- [ ] **Step 3: Input rules, with their tests**

Create `crates/push-auth/src/rules.rs`:

```rust
//! What the API accepts: emails, handles, display names, passwords and devices.
//!
//! Each function trims and normalises its input and returns what is stored, or the
//! reason it is refused.

use serde::{Deserialize, Serialize};

/// An input the API refuses: which field and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid {
    pub field: &'static str,
    pub reason: &'static str,
}

fn invalid(field: &'static str, reason: &'static str) -> Invalid {
    Invalid { field, reason }
}

/// Most characters of a password.
pub const MAX_PASSWORD_CHARS: usize = 128;
/// Fewest characters of a password.
pub const MIN_PASSWORD_CHARS: usize = 8;
/// Most characters of a display name or device name.
pub const MAX_NAME_CHARS: usize = 64;

/// An email address: trimmed, with the domain in lower case. Matching ignores case
/// anyway (the column is citext); the local part keeps the case the user typed.
pub fn email(raw: &str) -> Result<String, Invalid> {
    let raw = raw.trim();
    let refuse = || invalid("email", "not a valid email address");
    if raw.len() > 254 || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(refuse());
    }
    let (local, domain) = raw.split_once('@').ok_or_else(refuse)?;
    let labels_ok = domain
        .split('.')
        .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'));
    if local.is_empty() || local.len() > 64 || domain.contains('@') || !domain.contains('.') {
        return Err(refuse());
    }
    if !labels_ok {
        return Err(refuse());
    }
    Ok(format!("{local}@{}", domain.to_lowercase()))
}

/// A public handle, without its `@`: 3 to 30 lower-case letters, digits and underscores,
/// starting with a letter. Input may carry the `@` and any case.
pub fn handle(raw: &str) -> Result<String, Invalid> {
    let raw = raw.trim();
    let handle = raw.strip_prefix('@').unwrap_or(raw).to_lowercase();
    let mut chars = handle.chars();
    let starts_with_letter = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !(3..=30).contains(&handle.len()) || !starts_with_letter || !rest_ok {
        return Err(invalid(
            "handle",
            "use 3 to 30 letters, digits or underscores, starting with a letter",
        ));
    }
    Ok(handle)
}

/// A display name: trimmed, 1 to 64 characters, no control characters.
pub fn display_name(raw: &str) -> Result<String, Invalid> {
    name(raw, "display_name")
}

/// A device's name, like a display name.
pub fn device_name(raw: &str) -> Result<String, Invalid> {
    name(raw, "name")
}

fn name(raw: &str, field: &'static str) -> Result<String, Invalid> {
    let name = raw.trim();
    let count = name.chars().count();
    if count == 0 || count > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(invalid(
            field,
            "use 1 to 64 characters without control characters",
        ));
    }
    Ok(name.to_owned())
}

/// A new password: 8 to 128 characters. Used as typed: never trimmed.
pub fn password(raw: &str) -> Result<(), Invalid> {
    let count = raw.chars().count();
    if !(MIN_PASSWORD_CHARS..=MAX_PASSWORD_CHARS).contains(&count) {
        return Err(invalid("password", "use 8 to 128 characters"));
    }
    Ok(())
}

/// What kind of device a client runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Ios,
    Android,
    Web,
    Desktop,
    Other,
}

impl Platform {
    /// The stored name, as the `device.platform` CHECK lists it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Android => "android",
            Self::Web => "web",
            Self::Desktop => "desktop",
            Self::Other => "other",
        }
    }

    /// The platform with this stored name.
    pub fn parse(name: &str) -> Result<Self, Invalid> {
        Ok(match name {
            "ios" => Self::Ios,
            "android" => Self::Android,
            "web" => Self::Web,
            "desktop" => Self::Desktop,
            "other" => Self::Other,
            _ => {
                return Err(invalid(
                    "platform",
                    "use ios, android, web, desktop or other",
                ));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_trimmed_and_their_domain_lower_cased() {
        assert_eq!(
            email("  Alice.Smith@Example.COM "),
            Ok("Alice.Smith@example.com".to_owned())
        );
        for bad in [
            "",
            "alice",
            "@example.com",
            "alice@",
            "alice@localhost",
            "alice@@example.com",
            "al ice@example.com",
            "alice@example..com",
            "alice@-example.com",
        ] {
            assert_eq!(
                email(bad),
                Err(invalid("email", "not a valid email address")),
                "{bad:?}"
            );
        }
        let long_local = format!("{}@example.com", "a".repeat(65));
        assert!(email(&long_local).is_err());
    }

    #[test]
    fn handles_drop_the_at_sign_and_case() {
        assert_eq!(handle("@Alice_99"), Ok("alice_99".to_owned()));
        assert_eq!(handle("bob"), Ok("bob".to_owned()));
        for bad in ["al", "9lives", "has-dash", "_bob", "émile", &"a".repeat(31)] {
            assert!(handle(bad).is_err(), "{bad:?}");
        }
        assert!(handle(&"a".repeat(30)).is_ok());
    }

    #[test]
    fn names_are_trimmed_and_limited_to_64_characters() {
        assert_eq!(display_name("  Alice  "), Ok("Alice".to_owned()));
        assert_eq!(device_name("Yuri's iPhone"), Ok("Yuri's iPhone".to_owned()));
        assert!(display_name(&"é".repeat(64)).is_ok());
        assert!(display_name(&"é".repeat(65)).is_err());
        assert!(display_name("   ").is_err());
        assert!(display_name("tab\there").is_err());
        assert_eq!(device_name("").unwrap_err().field, "name");
    }

    #[test]
    fn passwords_have_8_to_128_characters() {
        assert!(password("1234567").is_err());
        assert!(password("12345678").is_ok());
        assert!(password(&"ü".repeat(128)).is_ok());
        assert!(password(&"ü".repeat(129)).is_err());
    }

    #[test]
    fn platforms_round_trip_through_their_names() {
        for platform in [
            Platform::Ios,
            Platform::Android,
            Platform::Web,
            Platform::Desktop,
            Platform::Other,
        ] {
            assert_eq!(Platform::parse(platform.as_str()), Ok(platform));
        }
        assert!(Platform::parse("toaster").is_err());
        assert!(Platform::parse("IOS").is_err());
    }
}
```

Replace `crates/push-auth/src/lib.rs` with:

```rust
//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.

mod db;
pub mod password;
pub mod rules;

pub use db::{MIGRATOR, migrate_database};
```

- [ ] **Step 4: Run the tests**

```powershell
cargo test -p push-auth --lib
```

Expected: `Adding argon2 v0.5.3 (available: v0.6.0)`, `blake2`, `password-hash` on the
first run; then `test result: ok. 11 passed` in well under a second —
`the_owasp_parameters_are_the_spec_minimum` hashes with the real parameters, which takes
seconds without the `opt-level` override.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.toml Cargo.lock crates/push-auth
git commit -m "feat(auth): Argon2id passwords and input rules"
```

Expected: 630 tests pass.

---

### Task 3: Signed links, access tokens with JWKS, refresh tokens

**Files:**
- Modify: `Cargo.toml`, `crates/push-auth/Cargo.toml`, `crates/push-auth/src/lib.rs`
- Create: `crates/push-auth/src/links.rs`, `src/tokens.rs`

- [ ] **Step 1: Dependencies**

Replace `Cargo.toml` with:

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-auth",
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
argon2 = { version = "0.5", features = ["std"] }
async-trait = "0.1"
axum = "0.8"
base64 = "0.22"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
futures-util = { version = "0.3", default-features = false, features = ["sink"] }
hmac = "0.12"
jsonwebtoken = { version = "9", default-features = false }
libtest-mimic = "0.8"
prost = "0.14"
protoc-bin-vendored = "3"
push-auth = { path = "crates/push-auth" }
push-domain = { path = "crates/push-domain" }
push-gateway = { path = "crates/push-gateway" }
push-message-svc = { path = "crates/push-message-svc" }
push-proto = { path = "crates/push-proto" }
push-store = { path = "crates/push-store" }
push-testkit = { path = "crates/push-testkit" }
rand = "0.9"
redis = { version = "1", default-features = false, features = [
    "connection-manager",
    "tokio-comp",
] }
ring = "0.17"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
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

# Argon2id is far too slow unoptimised, and the auth tests hash many passwords.
[profile.dev.package.argon2]
opt-level = 3

[profile.dev.package.blake2]
opt-level = 3

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

Replace `crates/push-auth/Cargo.toml` with:

```toml
[package]
name = "push-auth"
description = "The push-messenger auth service: accounts, email links, devices and tokens."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
argon2.workspace = true
base64.workspace = true
hmac.workspace = true
jsonwebtoken.workspace = true
push-store.workspace = true
rand.workspace = true
ring.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
sqlx.workspace = true
thiserror.workspace = true
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }

[lints]
workspace = true
```

- [ ] **Step 2: Signed links and registration tokens, with their tests**

Create `crates/push-auth/src/links.rs`:

```rust
//! Signed, stateless tokens: the email links and the registration token (spec §5.1).
//!
//! A token is `payload.signature`: the payload is base64url JSON claims, the signature
//! an HMAC-SHA256 over the payload under the service's link key. Nothing is stored:
//! a verification link binds the email it was sent to, a reset link a fingerprint of the
//! password hash it replaces (so using it once invalidates it), and every token has a
//! purpose, so one kind can never stand in for another.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// What a signed token allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    /// Verify the email address of an account (spec §5.2).
    Verify,
    /// Set a new password (spec §5.4).
    Reset,
    /// Register one device for an account (spec §5.3).
    Register,
}

/// The claims of a signed token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkClaims {
    /// The account.
    pub sub: Uuid,
    pub purpose: Purpose,
    /// Expiry, in seconds since the Unix epoch.
    pub exp: i64,
    /// Verify links: the address the link was sent to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Reset links: [`password_stamp`] of the password hash being replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pw: Option<String>,
}

/// Why a token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("the token is malformed")]
    Malformed,
    #[error("the token's signature does not match")]
    Forged,
    #[error("the token is for another purpose")]
    WrongPurpose,
    #[error("the token has expired")]
    Expired,
}

/// Domain separation: the MAC covers this prefix and the payload.
const CONTEXT: &[u8] = b"push-link-v1.";

/// Signs and checks tokens with one HMAC key.
#[derive(Clone)]
pub struct LinkSigner {
    key: Vec<u8>,
}

impl std::fmt::Debug for LinkSigner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LinkSigner(<redacted>)")
    }
}

impl LinkSigner {
    /// Fewest bytes of key: as long as the SHA-256 output.
    pub const MIN_KEY_BYTES: usize = 32;

    /// A signer with `key`, which must have at least [`Self::MIN_KEY_BYTES`] bytes.
    pub fn new(key: Vec<u8>) -> Result<Self, String> {
        if key.len() < Self::MIN_KEY_BYTES {
            return Err(format!(
                "the link key must have at least {} bytes",
                Self::MIN_KEY_BYTES
            ));
        }
        Ok(Self { key })
    }

    fn mac(&self, payload: &str) -> Hmac<Sha256> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts keys of any length");
        mac.update(CONTEXT);
        mac.update(payload.as_bytes());
        mac
    }

    /// The token for `claims`.
    pub fn sign(&self, claims: &LinkClaims) -> String {
        let json = serde_json::to_vec(claims).expect("claims serialise");
        let payload = URL_SAFE_NO_PAD.encode(json);
        let signature = URL_SAFE_NO_PAD.encode(self.mac(&payload).finalize().into_bytes());
        format!("{payload}.{signature}")
    }

    /// The claims of `token` if its signature matches, it is for `purpose` and it has not
    /// expired at `now` (seconds since the Unix epoch). The signature is checked first,
    /// in constant time, so a forged token reveals nothing about its claims.
    pub fn verify(&self, token: &str, purpose: Purpose, now: i64) -> Result<LinkClaims, LinkError> {
        let (payload, signature) = token.split_once('.').ok_or(LinkError::Malformed)?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| LinkError::Malformed)?;
        self.mac(payload)
            .verify_slice(&signature)
            .map_err(|_| LinkError::Forged)?;
        let json = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| LinkError::Malformed)?;
        let claims: LinkClaims = serde_json::from_slice(&json).map_err(|_| LinkError::Malformed)?;
        if claims.purpose != purpose {
            return Err(LinkError::WrongPurpose);
        }
        if now >= claims.exp {
            return Err(LinkError::Expired);
        }
        Ok(claims)
    }
}

/// A short fingerprint of a password hash, for reset links: it changes whenever the
/// password does, which makes a used link worthless.
pub fn password_stamp(password_hash: &str) -> String {
    let digest = Sha256::digest(password_hash.as_bytes());
    URL_SAFE_NO_PAD.encode(&digest[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> LinkSigner {
        LinkSigner::new(vec![7; 32]).unwrap()
    }

    fn verify_claims() -> LinkClaims {
        LinkClaims {
            sub: Uuid::now_v7(),
            purpose: Purpose::Verify,
            exp: 1_000,
            email: Some("alice@example.com".to_owned()),
            pw: None,
        }
    }

    #[test]
    fn a_signed_token_verifies_until_it_expires() {
        let claims = verify_claims();
        let token = signer().sign(&claims);
        assert_eq!(signer().verify(&token, Purpose::Verify, 999), Ok(claims));
        assert_eq!(
            signer().verify(&token, Purpose::Verify, 1_000),
            Err(LinkError::Expired)
        );
    }

    #[test]
    fn a_token_for_one_purpose_is_refused_for_another() {
        let token = signer().sign(&verify_claims());
        assert_eq!(
            signer().verify(&token, Purpose::Register, 0),
            Err(LinkError::WrongPurpose)
        );
        assert_eq!(
            signer().verify(&token, Purpose::Reset, 0),
            Err(LinkError::WrongPurpose)
        );
    }

    #[test]
    fn a_changed_payload_or_another_key_is_a_forgery() {
        let token = signer().sign(&verify_claims());
        let (_, signature) = token.split_once('.').unwrap();
        let other = LinkClaims {
            email: Some("mallory@example.com".to_owned()),
            ..verify_claims()
        };
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&other).unwrap());
        assert_eq!(
            signer().verify(&format!("{payload}.{signature}"), Purpose::Verify, 0),
            Err(LinkError::Forged)
        );
        let stranger = LinkSigner::new(vec![8; 32]).unwrap();
        assert_eq!(
            stranger.verify(&token, Purpose::Verify, 0),
            Err(LinkError::Forged)
        );
    }

    #[test]
    fn garbage_is_malformed() {
        for token in ["", "abc", "abc.***", "a.b.c"] {
            assert_eq!(
                signer().verify(token, Purpose::Verify, 0),
                Err(LinkError::Malformed),
                "{token:?}"
            );
        }
    }

    #[test]
    fn short_keys_are_refused_and_never_shown() {
        assert!(LinkSigner::new(vec![1; 31]).is_err());
        assert_eq!(format!("{:?}", signer()), "LinkSigner(<redacted>)");
    }

    #[test]
    fn the_password_stamp_changes_with_the_hash() {
        assert_eq!(password_stamp("hash-1"), password_stamp("hash-1"));
        assert_ne!(password_stamp("hash-1"), password_stamp("hash-2"));
        assert_eq!(password_stamp("hash-1").len(), 22);
    }
}
```

- [ ] **Step 3: Access tokens, the JWKS and refresh tokens, with their tests**

The last test builds a key from the published JWK with `DecodingKey::from_jwk` and checks a
token with it, exactly as the gateway will in Plan 8. Create
`crates/push-auth/src/tokens.rs`:

```rust
//! Sign-in credentials (spec §5.1): access tokens, signed JWTs (EdDSA) published as a
//! JWKS so the gateway can check them without calling auth-svc, and refresh tokens,
//! random strings stored only as SHA-256 hashes.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// How long an access token is valid.
pub const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);
/// How long a refresh token is valid.
pub const REFRESH_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The claims of an access token: "user `sub` on device `device_id`, email verified".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessClaims {
    pub sub: Uuid,
    pub device_id: Uuid,
    pub email_verified: bool,
    /// Expiry and issue time, in seconds since the Unix epoch.
    pub exp: i64,
    pub iat: i64,
    pub jti: Uuid,
    /// The gateway's URL: the only service that accepts these tokens.
    pub aud: String,
}

/// Why an access token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("the access token has expired")]
    Expired,
    #[error("the access token is not valid")]
    Invalid,
}

/// The PKCS#8 v1 prefix of an Ed25519 private key; the 32-byte seed follows it.
const PKCS8_ED25519_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// Issues and checks access tokens with one Ed25519 key.
#[derive(Clone)]
pub struct AccessTokens {
    encoding: EncodingKey,
    decoding: DecodingKey,
    /// The public key, base64url: the JWK's `x`.
    public_key: String,
    /// The RFC 7638 thumbprint of the public key, base64url.
    kid: String,
    audience: String,
}

impl std::fmt::Debug for AccessTokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessTokens")
            .field("kid", &self.kid)
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

impl AccessTokens {
    /// Tokens signed with the Ed25519 key whose 32-byte private seed is `seed`, for the
    /// audience `audience` (the gateway's URL).
    pub fn from_seed(seed: &[u8], audience: impl Into<String>) -> Result<Self, String> {
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| "the signing key must be a 32-byte Ed25519 seed".to_owned())?;
        let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed)
            .map_err(|_| "the signing key is not a valid Ed25519 seed".to_owned())?;
        let public_key = URL_SAFE_NO_PAD.encode(ring::signature::KeyPair::public_key(&pair));
        let mut der = PKCS8_ED25519_PREFIX.to_vec();
        der.extend_from_slice(&seed);
        let decoding = DecodingKey::from_ed_components(&public_key)
            .map_err(|_| "the public key cannot be loaded".to_owned())?;
        // RFC 7638: SHA-256 over the required members in lexicographic order.
        let thumbprint = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{public_key}"}}"#);
        let kid = URL_SAFE_NO_PAD.encode(Sha256::digest(thumbprint.as_bytes()));
        Ok(Self {
            encoding: EncodingKey::from_ed_der(&der),
            decoding,
            public_key,
            kid,
            audience: audience.into(),
        })
    }

    /// The key id in every token's header and in the JWKS.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// A new access token for `user` on `device`, issued at `now` (seconds since the Unix
    /// epoch), and its claims.
    pub fn issue(&self, user: Uuid, device: Uuid, now: i64) -> (String, AccessClaims) {
        let claims = AccessClaims {
            sub: user,
            device_id: device,
            email_verified: true,
            exp: now + ACCESS_TOKEN_TTL.as_secs() as i64,
            iat: now,
            jti: Uuid::now_v7(),
            aud: self.audience.clone(),
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        let token = jsonwebtoken::encode(&header, &claims, &self.encoding)
            .expect("signing with a valid Ed25519 key cannot fail");
        (token, claims)
    }

    /// The claims of `token` if it is one of ours, for our audience, and not expired
    /// (no leeway).
    pub fn verify(&self, token: &str) -> Result<AccessClaims, TokenError> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "sub", "aud"]);
        validation.leeway = 0;
        jsonwebtoken::decode::<AccessClaims>(token, &self.decoding, &validation)
            .map(|data| data.claims)
            .map_err(|error| match error.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => TokenError::Expired,
                _ => TokenError::Invalid,
            })
    }

    /// The JSON Web Key Set with the public key, served at `GET /v1/auth/jwks`.
    pub fn jwks(&self) -> serde_json::Value {
        serde_json::json!({
            "keys": [{
                "kty": "OKP",
                "crv": "Ed25519",
                "alg": "EdDSA",
                "use": "sig",
                "kid": self.kid,
                "x": self.public_key,
            }]
        })
    }
}

/// A new refresh token: 32 random bytes, base64url. Only [`refresh_hash`] of it is
/// stored.
pub fn new_refresh_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The SHA-256 hash under which a refresh token is stored.
pub fn refresh_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUDIENCE: &str = "wss://api.example.com/v1/socket";

    fn tokens() -> AccessTokens {
        AccessTokens::from_seed(&[9; 32], AUDIENCE).unwrap()
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[test]
    fn an_issued_token_verifies_with_its_claims() {
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        let (token, claims) = tokens().issue(user, device, now());
        assert_eq!(claims.sub, user);
        assert_eq!(claims.device_id, device);
        assert!(claims.email_verified);
        assert_eq!(claims.exp - claims.iat, 900);
        assert_eq!(claims.aud, AUDIENCE);
        assert_eq!(tokens().verify(&token), Ok(claims));
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!(header.alg, Algorithm::EdDSA);
        assert_eq!(header.kid.as_deref(), Some(tokens().kid()));
    }

    #[test]
    fn expired_foreign_and_other_audience_tokens_are_refused() {
        let user = Uuid::now_v7();
        let (expired, _) = tokens().issue(user, user, now() - 901);
        assert_eq!(tokens().verify(&expired), Err(TokenError::Expired));
        let stranger = AccessTokens::from_seed(&[10; 32], AUDIENCE).unwrap();
        let (foreign, _) = stranger.issue(user, user, now());
        assert_eq!(tokens().verify(&foreign), Err(TokenError::Invalid));
        let elsewhere = AccessTokens::from_seed(&[9; 32], "wss://other/v1/socket").unwrap();
        let (other_audience, _) = elsewhere.issue(user, user, now());
        assert_eq!(tokens().verify(&other_audience), Err(TokenError::Invalid));
        assert_eq!(tokens().verify("not.a.token"), Err(TokenError::Invalid));
    }

    #[test]
    fn the_jwks_publishes_the_public_key_that_verifies_tokens() {
        let jwks = tokens().jwks();
        let key = &jwks["keys"][0];
        assert_eq!(key["kty"], "OKP");
        assert_eq!(key["crv"], "Ed25519");
        assert_eq!(key["alg"], "EdDSA");
        assert_eq!(key["kid"], tokens().kid());
        // What the gateway will do: build the key from the JWK and check a token.
        let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(key.clone()).unwrap();
        let decoding = DecodingKey::from_jwk(&jwk).unwrap();
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[AUDIENCE]);
        let (token, claims) = tokens().issue(Uuid::now_v7(), Uuid::now_v7(), now());
        let decoded = jsonwebtoken::decode::<AccessClaims>(&token, &decoding, &validation);
        assert_eq!(decoded.unwrap().claims, claims);
    }

    #[test]
    fn a_seed_must_have_32_bytes_and_debug_hides_the_key() {
        assert!(AccessTokens::from_seed(&[1; 31], AUDIENCE).is_err());
        let shown = format!("{:?}", tokens());
        assert!(shown.starts_with("AccessTokens { kid: "), "{shown}");
        assert!(!shown.contains("encoding"), "{shown}");
    }

    #[test]
    fn refresh_tokens_are_random_and_hashed_with_sha256() {
        let (first, second) = (new_refresh_token(), new_refresh_token());
        assert_eq!(first.len(), 43);
        assert_ne!(first, second);
        assert_eq!(refresh_hash(&first), refresh_hash(&first));
        assert_ne!(refresh_hash(&first), refresh_hash(&second));
    }
}
```

Replace `crates/push-auth/src/lib.rs` with:

```rust
//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.

mod db;
pub mod links;
pub mod password;
pub mod rules;
pub mod tokens;

pub use db::{MIGRATOR, migrate_database};
```

- [ ] **Step 4: Run the tests**

```powershell
cargo test -p push-auth --lib
```

Expected: `Adding jsonwebtoken v9.3.1 (available: v10.3.0)` on the first run, then
`test result: ok. 22 passed`.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.toml Cargo.lock crates/push-auth
git commit -m "feat(auth): signed links, EdDSA access tokens with JWKS, refresh tokens"
```

Expected: 641 tests pass.

---

### Task 4: Signup, email verification, login and password reset, with the outbox

**Files:**
- Modify: `Cargo.toml` (the `json` feature of `sqlx`), `crates/push-auth/Cargo.toml`,
  `crates/push-auth/src/lib.rs`
- Create: `crates/push-auth/src/error.rs`, `src/outbox.rs`, `src/service.rs`,
  `tests/accounts.rs`

- [ ] **Step 1: Dependencies**

Replace `Cargo.toml` with (`sqlx` gains `json`, for the outbox payloads):

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-auth",
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
argon2 = { version = "0.5", features = ["std"] }
async-trait = "0.1"
axum = "0.8"
base64 = "0.22"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
futures-util = { version = "0.3", default-features = false, features = ["sink"] }
hmac = "0.12"
jsonwebtoken = { version = "9", default-features = false }
libtest-mimic = "0.8"
prost = "0.14"
protoc-bin-vendored = "3"
push-auth = { path = "crates/push-auth" }
push-domain = { path = "crates/push-domain" }
push-gateway = { path = "crates/push-gateway" }
push-message-svc = { path = "crates/push-message-svc" }
push-proto = { path = "crates/push-proto" }
push-store = { path = "crates/push-store" }
push-testkit = { path = "crates/push-testkit" }
rand = "0.9"
redis = { version = "1", default-features = false, features = [
    "connection-manager",
    "tokio-comp",
] }
ring = "0.17"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
sqlx = { version = "0.8", default-features = false, features = [
    "chrono",
    "json",
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

# Argon2id is far too slow unoptimised, and the auth tests hash many passwords.
[profile.dev.package.argon2]
opt-level = 3

[profile.dev.package.blake2]
opt-level = 3

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

Replace `crates/push-auth/Cargo.toml` with:

```toml
[package]
name = "push-auth"
description = "The push-messenger auth service: accounts, email links, devices and tokens."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
argon2.workspace = true
base64.workspace = true
hmac.workspace = true
jsonwebtoken.workspace = true
push-store.workspace = true
rand.workspace = true
ring.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
sqlx.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["rt"] }
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing tests**

Create `crates/push-auth/tests/accounts.rs`:

```rust
//! Signup, email verification, login and password reset on a real PostgreSQL (Docker
//! must be running). Each test starts a container of its own; they run in parallel.

use push_auth::links::{LinkClaims, LinkSigner, Purpose};
use push_auth::password::Passwords;
use push_auth::tokens::AccessTokens;
use push_auth::{Auth, AuthError, Settings, Signup, migrate_database};
use serde_json::Value;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

const LINK_KEY: [u8; 32] = [3; 32];

struct World {
    _container: ContainerAsync<Postgres>,
    auth: Auth,
    pool: PgPool,
}

async fn world() -> World {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    // Docker publishes ports on IPv4; "localhost" would try IPv6 first on every connect.
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    migrate_database(&url).await.expect("migrate");
    let pool = PgPool::connect(&url).await.expect("connect");
    let auth = Auth::new(pool.clone(), settings(Passwords::new(64, 1, 1).unwrap()));
    World {
        _container: container,
        auth,
        pool,
    }
}

fn settings(passwords: Passwords) -> Settings {
    Settings {
        public_url: "https://api.example.com/".to_owned(),
        passwords,
        links: LinkSigner::new(LINK_KEY.to_vec()).unwrap(),
        tokens: AccessTokens::from_seed(&[5; 32], "wss://api.example.com/v1/socket").unwrap(),
        hash_concurrency: 4,
    }
}

fn signup(email: &str, handle: &str) -> Signup {
    Signup {
        email: email.to_owned(),
        password: "correct horse".to_owned(),
        handle: handle.to_owned(),
        display_name: "Alice".to_owned(),
    }
}

/// The outbox, oldest first: `(kind, payload)`.
async fn outbox(pool: &PgPool) -> Vec<(String, Value)> {
    sqlx::query_as("SELECT kind, payload FROM auth.outbox ORDER BY created_at, event_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// The token of the newest email link sent to `to`.
async fn last_link_token(pool: &PgPool, to: &str) -> String {
    let link = outbox(pool)
        .await
        .into_iter()
        .rev()
        .find(|(kind, payload)| kind == "email.send" && payload["to"] == to)
        .map(|(_, payload)| payload["params"]["link"].as_str().unwrap().to_owned())
        .expect("an email with a link");
    link.split_once("?token=").unwrap().1.to_owned()
}

#[tokio::test]
async fn signup_writes_a_verification_email_and_the_account_waits_for_it() {
    let world = world().await;
    world
        .auth
        .signup(signup(" alice@Example.com ", "@Alice"))
        .await
        .unwrap();

    let events = outbox(&world.pool).await;
    assert_eq!(events.len(), 1);
    let (kind, payload) = &events[0];
    assert_eq!(kind, "email.send");
    assert_eq!(payload["to"], "alice@example.com");
    assert_eq!(payload["template"], "verify_email");
    assert_eq!(payload["params"]["handle"], "alice");
    assert_eq!(payload["params"]["expires_in_minutes"], 30);
    let link = payload["params"]["link"].as_str().unwrap();
    assert!(
        link.starts_with("https://api.example.com/v1/auth/verify?token="),
        "{link}"
    );
    assert_eq!(
        world.auth.login("alice@example.com", "correct horse").await,
        Err(AuthError::EmailNotVerified)
    );
}

#[tokio::test]
async fn the_link_verifies_once_announces_the_user_and_opens_login() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();
    world.auth.verify_email(&token).await.unwrap();

    let events = outbox(&world.pool).await;
    let verified: Vec<_> = events
        .iter()
        .filter(|(kind, _)| kind == "user.verified")
        .collect();
    assert_eq!(verified.len(), 1, "announced once: {events:?}");
    let user_id = verified[0].1["user_id"].as_str().unwrap().to_owned();

    let registration = world
        .auth
        .login("ALICE@example.com", "correct horse")
        .await
        .unwrap();
    assert_eq!(registration.expires_in_s, 300);
    let signer = LinkSigner::new(LINK_KEY.to_vec()).unwrap();
    let claims = signer
        .verify(&registration.token, Purpose::Register, 0)
        .unwrap();
    assert_eq!(claims.sub.to_string(), user_id);
}

#[tokio::test]
async fn signing_up_again_changes_nothing_and_says_nothing() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    // Unverified: the same answer, and another verification email.
    world
        .auth
        .signup(signup("Alice@example.com", "alice2"))
        .await
        .unwrap();
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();
    // Verified: the same answer, and an "account exists" email.
    world
        .auth
        .signup(signup("alice@example.com", "alice3"))
        .await
        .unwrap();

    let templates: Vec<String> = outbox(&world.pool)
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "email.send")
        .map(|(_, payload)| payload["template"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        templates,
        ["verify_email", "verify_email", "account_exists"]
    );
    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.user_account")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(accounts, 1);
    let handle: String = sqlx::query_scalar("SELECT handle::text FROM auth.user_account")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(handle, "alice");
}

#[tokio::test]
async fn a_taken_handle_and_invalid_input_are_reported() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    assert_eq!(
        world.auth.signup(signup("bob@example.com", "@ALICE")).await,
        Err(AuthError::HandleTaken)
    );
    let short_password = Signup {
        password: "short".to_owned(),
        ..signup("bob@example.com", "bob")
    };
    let error = world.auth.signup(short_password).await.unwrap_err();
    assert_eq!(error.code(), "invalid");
    assert_eq!(error.status(), 400);
    assert!(
        world
            .auth
            .signup(signup("not-an-email", "bob"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn forged_expired_and_mismatched_links_are_refused() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let token = last_link_token(&world.pool, "alice@example.com").await;
    let user_id = LinkSigner::new(LINK_KEY.to_vec())
        .unwrap()
        .verify(&token, Purpose::Verify, 0)
        .unwrap()
        .sub;
    let signer = LinkSigner::new(LINK_KEY.to_vec()).unwrap();
    let claims = |exp: i64, email: &str, purpose: Purpose| LinkClaims {
        sub: user_id,
        purpose,
        exp,
        email: Some(email.to_owned()),
        pw: None,
    };
    let far = i64::MAX;
    let cases = [
        (
            signer.sign(&claims(1, "alice@example.com", Purpose::Verify)),
            AuthError::LinkExpired,
        ),
        (
            signer.sign(&claims(far, "old@example.com", Purpose::Verify)),
            AuthError::LinkInvalid,
        ),
        (
            signer.sign(&claims(far, "alice@example.com", Purpose::Register)),
            AuthError::LinkInvalid,
        ),
        (
            LinkSigner::new(vec![4; 32]).unwrap().sign(&claims(
                far,
                "alice@example.com",
                Purpose::Verify,
            )),
            AuthError::LinkInvalid,
        ),
        ("garbage".to_owned(), AuthError::LinkInvalid),
    ];
    for (token, expected) in cases {
        assert_eq!(world.auth.verify_email(&token).await, Err(expected));
    }
    let unverified: bool = sqlx::query_scalar(
        "SELECT email_verified_at IS NULL FROM auth.user_account WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert!(unverified);
}

#[tokio::test]
async fn an_unknown_email_and_a_wrong_password_are_the_same_error() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();
    for (email, password) in [
        ("alice@example.com", "wrong horse"),
        ("nobody@example.com", "correct horse"),
        ("not an email", "correct horse"),
    ] {
        assert_eq!(
            world.auth.login(email, password).await,
            Err(AuthError::InvalidCredentials),
            "{email}"
        );
    }
}

#[tokio::test]
async fn resending_writes_a_new_email_only_for_an_unverified_account() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    world
        .auth
        .resend_verification("alice@example.com")
        .await
        .unwrap();
    world
        .auth
        .resend_verification("nobody@example.com")
        .await
        .unwrap();
    assert_eq!(outbox(&world.pool).await.len(), 2);
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();
    world
        .auth
        .resend_verification("alice@example.com")
        .await
        .unwrap();
    let kinds: Vec<String> = outbox(&world.pool)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, ["email.send", "email.send", "user.verified"]);
}

#[tokio::test]
async fn a_reset_link_sets_the_password_once_and_announces_it() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();

    world
        .auth
        .forgot_password("nobody@example.com")
        .await
        .unwrap();
    world
        .auth
        .forgot_password("alice@example.com")
        .await
        .unwrap();
    let (kind, payload) = outbox(&world.pool).await.pop().unwrap();
    assert_eq!(kind, "email.send");
    assert_eq!(payload["template"], "reset_password");
    let link = payload["params"]["link"].as_str().unwrap();
    assert!(
        link.starts_with("https://api.example.com/v1/auth/password/reset?token="),
        "{link}"
    );
    let reset = last_link_token(&world.pool, "alice@example.com").await;

    assert!(matches!(
        world.auth.reset_password(&reset, "short").await,
        Err(AuthError::Invalid(_))
    ));
    world
        .auth
        .reset_password(&reset, "battery staple")
        .await
        .unwrap();
    assert_eq!(
        world.auth.reset_password(&reset, "another password").await,
        Err(AuthError::LinkInvalid),
        "a used link"
    );
    assert_eq!(
        world.auth.login("alice@example.com", "correct horse").await,
        Err(AuthError::InvalidCredentials)
    );
    world
        .auth
        .login("alice@example.com", "battery staple")
        .await
        .unwrap();
    let (kind, _) = outbox(&world.pool).await.pop().unwrap();
    assert_eq!(kind, "user.password_reset");
}

#[tokio::test]
async fn login_replaces_a_hash_made_with_outdated_parameters() {
    let world = world().await;
    let old = Auth::new(
        world.pool.clone(),
        settings(Passwords::new(32, 1, 1).unwrap()),
    );
    old.signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();
    let stored = || async {
        sqlx::query_scalar::<_, String>("SELECT password_hash FROM auth.user_account")
            .fetch_one(&world.pool)
            .await
            .unwrap()
    };
    assert!(stored().await.starts_with("$argon2id$v=19$m=32,t=1,p=1$"));
    world
        .auth
        .login("alice@example.com", "correct horse")
        .await
        .unwrap();
    assert!(stored().await.starts_with("$argon2id$v=19$m=64,t=1,p=1$"));
}

#[tokio::test]
async fn new_ids_are_time_ordered() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let user_id: Uuid = sqlx::query_scalar("SELECT user_id FROM auth.user_account")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(user_id.get_version_num(), 7);
}
```

```powershell
cargo test -p push-auth --test accounts
```

Expected: a compile error — `unresolved imports push_auth::Auth, push_auth::AuthError,
push_auth::Settings, push_auth::Signup`.

- [ ] **Step 3: Errors**

Create `crates/push-auth/src/error.rs`:

```rust
//! The errors of the auth service, each with the HTTP status and code the API answers.

use crate::rules::Invalid;

/// Why an auth operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("{}: {}", .0.field, .0.reason)]
    Invalid(Invalid),
    #[error("this handle is taken")]
    HandleTaken,
    #[error("wrong email or password")]
    InvalidCredentials,
    #[error("the email address is not verified yet")]
    EmailNotVerified,
    #[error("this link is not valid")]
    LinkInvalid,
    #[error("this link has expired")]
    LinkExpired,
    #[error("sign in again")]
    Unauthorized,
    #[error("the access token has expired")]
    TokenExpired,
    #[error("the account already has the most active devices allowed")]
    TooManyDevices,
    #[error("no such device")]
    DeviceNotFound,
    #[error("no such user")]
    UserNotFound,
    #[error("too many requests; try again in {retry_after_s} s")]
    RateLimited { retry_after_s: u64 },
    #[error("the service is unavailable; try again")]
    Unavailable,
}

impl AuthError {
    /// The stable, machine-readable code clients switch on.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::HandleTaken => "handle_taken",
            Self::InvalidCredentials => "invalid_credentials",
            Self::EmailNotVerified => "email_not_verified",
            Self::LinkInvalid => "link_invalid",
            Self::LinkExpired => "link_expired",
            Self::Unauthorized => "unauthorized",
            Self::TokenExpired => "token_expired",
            Self::TooManyDevices => "too_many_devices",
            Self::DeviceNotFound => "device_not_found",
            Self::UserNotFound => "user_not_found",
            Self::RateLimited { .. } => "rate_limited",
            Self::Unavailable => "unavailable",
        }
    }

    /// The HTTP status of the answer.
    pub fn status(&self) -> u16 {
        match self {
            Self::Invalid(_) | Self::LinkInvalid | Self::LinkExpired => 400,
            Self::InvalidCredentials | Self::Unauthorized | Self::TokenExpired => 401,
            Self::EmailNotVerified => 403,
            Self::DeviceNotFound | Self::UserNotFound => 404,
            Self::HandleTaken | Self::TooManyDevices => 409,
            Self::RateLimited { .. } => 429,
            Self::Unavailable => 503,
        }
    }

    /// Whether the same request may succeed later without changes.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Unavailable)
    }
}

impl From<Invalid> for AuthError {
    fn from(invalid: Invalid) -> Self {
        Self::Invalid(invalid)
    }
}

impl From<sqlx::Error> for AuthError {
    /// A database failure, logged here without parameters (they may hold an email).
    fn from(error: sqlx::Error) -> Self {
        let code = error
            .as_database_error()
            .and_then(|database| database.code())
            .map(|code| code.into_owned());
        tracing::error!(%error, code = ?code, "database error");
        Self::Unavailable
    }
}
```

- [ ] **Step 4: The outbox**

Create `crates/push-auth/src/outbox.rs`. Its doc comment is the payload contract the
worker (Plan 6) reads:

```rust
//! Events for other services, written to `auth.outbox` in the same transaction as the
//! change they announce (spec §2). The worker (Plan 6) reads and deletes them: it sends
//! the emails and relays the user and device events to message-svc.
//!
//! Payloads (JSON):
//!
//! | kind | payload |
//! |---|---|
//! | `email.send` | `{user_id, to, template, params}` |
//! | `user.verified` | `{user_id}` |
//! | `user.password_reset` | `{user_id}`: every device of the user is to be kicked |
//! | `device.registered` | `{user_id, device_id}` |
//! | `device.revoked` | `{user_id, device_id}` |

use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

/// An email to send, by template. The worker renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Email {
    /// "Verify your address": `params.link` verifies it.
    VerifyEmail { link: String, handle: String },
    /// Someone signed up with an address that has a verified account already.
    AccountExists { handle: String },
    /// "Reset your password": `params.link` opens the reset page.
    ResetPassword { link: String },
}

/// How many minutes an email link stays valid; the templates say so.
pub const LINK_MINUTES: u64 = 30;

impl Email {
    fn template(&self) -> &'static str {
        match self {
            Self::VerifyEmail { .. } => "verify_email",
            Self::AccountExists { .. } => "account_exists",
            Self::ResetPassword { .. } => "reset_password",
        }
    }

    fn params(&self) -> Value {
        match self {
            Self::VerifyEmail { link, handle } => {
                json!({"link": link, "handle": handle, "expires_in_minutes": LINK_MINUTES})
            }
            Self::AccountExists { handle } => json!({"handle": handle}),
            Self::ResetPassword { link } => {
                json!({"link": link, "expires_in_minutes": LINK_MINUTES})
            }
        }
    }
}

/// One outbox event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    EmailSend {
        user_id: Uuid,
        to: String,
        email: Email,
    },
    UserVerified {
        user_id: Uuid,
    },
    UserPasswordReset {
        user_id: Uuid,
    },
    DeviceRegistered {
        user_id: Uuid,
        device_id: Uuid,
    },
    DeviceRevoked {
        user_id: Uuid,
        device_id: Uuid,
    },
}

impl Event {
    /// The `kind` column.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::EmailSend { .. } => "email.send",
            Self::UserVerified { .. } => "user.verified",
            Self::UserPasswordReset { .. } => "user.password_reset",
            Self::DeviceRegistered { .. } => "device.registered",
            Self::DeviceRevoked { .. } => "device.revoked",
        }
    }

    /// The `payload` column.
    pub fn payload(&self) -> Value {
        match self {
            Self::EmailSend { user_id, to, email } => json!({
                "user_id": user_id,
                "to": to,
                "template": email.template(),
                "params": email.params(),
            }),
            Self::UserVerified { user_id } | Self::UserPasswordReset { user_id } => {
                json!({"user_id": user_id})
            }
            Self::DeviceRegistered { user_id, device_id }
            | Self::DeviceRevoked { user_id, device_id } => {
                json!({"user_id": user_id, "device_id": device_id})
            }
        }
    }
}

/// Writes `event` in the caller's transaction.
pub(crate) async fn write(connection: &mut PgConnection, event: &Event) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO auth.outbox (event_id, kind, payload) VALUES ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(event.kind())
        .bind(event.payload())
        .execute(connection)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_events_carry_the_template_and_its_params() {
        let user_id = Uuid::now_v7();
        let event = Event::EmailSend {
            user_id,
            to: "alice@example.com".to_owned(),
            email: Email::VerifyEmail {
                link: "https://api.example.com/v1/auth/verify?token=t".to_owned(),
                handle: "alice".to_owned(),
            },
        };
        assert_eq!(event.kind(), "email.send");
        assert_eq!(
            event.payload(),
            json!({
                "user_id": user_id,
                "to": "alice@example.com",
                "template": "verify_email",
                "params": {
                    "link": "https://api.example.com/v1/auth/verify?token=t",
                    "handle": "alice",
                    "expires_in_minutes": 30,
                },
            })
        );
    }

    #[test]
    fn user_and_device_events_carry_their_ids() {
        let (user_id, device_id) = (Uuid::now_v7(), Uuid::now_v7());
        let cases = [
            (Event::UserVerified { user_id }, "user.verified"),
            (Event::UserPasswordReset { user_id }, "user.password_reset"),
            (
                Event::DeviceRegistered { user_id, device_id },
                "device.registered",
            ),
            (
                Event::DeviceRevoked { user_id, device_id },
                "device.revoked",
            ),
        ];
        for (event, kind) in cases {
            assert_eq!(event.kind(), kind);
            assert_eq!(event.payload()["user_id"], json!(user_id));
        }
        assert_eq!(
            Event::DeviceRevoked { user_id, device_id }.payload()["device_id"],
            json!(device_id)
        );
    }
}
```

- [ ] **Step 5: The account operations**

Create `crates/push-auth/src/service.rs`. Note the `::citext` casts (decision 18) and that
`signup` hashes before it looks at the email (decision 6):

```rust
//! The auth operations (spec §5.1–§5.4), over the `auth` schema.
//!
//! Every change and the outbox event announcing it commit in one transaction. Password
//! hashing runs on blocking threads, at most `hash_concurrency` at a time, because each
//! hash takes tens of milliseconds and 19 MiB of memory.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::{PgPool, Row};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::error::AuthError;
use crate::links::{LinkClaims, LinkError, LinkSigner, Purpose, password_stamp};
use crate::outbox::{self, Email, Event, LINK_MINUTES};
use crate::password::{Checked, Passwords};
use crate::rules;
use crate::tokens::AccessTokens;

/// How long a registration token is valid, in seconds (spec §5.1: 5 minutes).
pub const REGISTRATION_TOKEN_SECONDS: i64 = 5 * 60;
/// How long an email link is valid, in seconds (spec §5.1: 30 minutes).
pub const LINK_SECONDS: i64 = LINK_MINUTES as i64 * 60;

/// What the service is built from.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The public base URL that email links start with, such as
    /// `https://api.example.com`; never taken from a request's `Host` header.
    pub public_url: String,
    pub passwords: Passwords,
    pub links: LinkSigner,
    pub tokens: AccessTokens,
    /// Most password hashes computed at once.
    pub hash_concurrency: usize,
}

/// A new account.
#[derive(Debug, Clone)]
pub struct Signup {
    pub email: String,
    pub password: String,
    pub handle: String,
    pub display_name: String,
}

/// Proof that an account may register one device (spec §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationToken {
    pub token: String,
    pub expires_in_s: i64,
}

/// The auth service.
#[derive(Debug, Clone)]
pub struct Auth {
    pub(crate) pool: PgPool,
    passwords: Arc<Passwords>,
    hashing: Arc<Semaphore>,
    pub(crate) links: LinkSigner,
    pub(crate) tokens: AccessTokens,
    public_url: String,
}

/// Seconds since the Unix epoch.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64
}

fn link_error(error: LinkError) -> AuthError {
    match error {
        LinkError::Expired => AuthError::LinkExpired,
        LinkError::Malformed | LinkError::Forged | LinkError::WrongPurpose => {
            AuthError::LinkInvalid
        }
    }
}

impl Auth {
    /// The service on `pool`, a pool of the migrated `push_auth` database.
    pub fn new(pool: PgPool, settings: Settings) -> Self {
        Self {
            pool,
            passwords: Arc::new(settings.passwords),
            hashing: Arc::new(Semaphore::new(settings.hash_concurrency.max(1))),
            links: settings.links,
            tokens: settings.tokens,
            public_url: settings.public_url.trim_end_matches('/').to_owned(),
        }
    }

    /// The access-token issuer, for the JWKS.
    pub fn tokens(&self) -> &AccessTokens {
        &self.tokens
    }

    /// The database pool, for readiness checks.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    async fn hash(&self, password: String) -> Result<String, AuthError> {
        let _permit = self
            .hashing
            .acquire()
            .await
            .map_err(|_| AuthError::Unavailable)?;
        let passwords = Arc::clone(&self.passwords);
        tokio::task::spawn_blocking(move || passwords.hash(&password))
            .await
            .map_err(|_| AuthError::Unavailable)
    }

    /// Checks `password` against `stored`, or spends the same time when there is no
    /// account (`stored` is `None`), so both answers take equally long.
    pub(crate) async fn check_password(
        &self,
        password: String,
        stored: Option<String>,
    ) -> Result<Checked, AuthError> {
        let _permit = self
            .hashing
            .acquire()
            .await
            .map_err(|_| AuthError::Unavailable)?;
        let passwords = Arc::clone(&self.passwords);
        tokio::task::spawn_blocking(move || match stored {
            Some(stored) => passwords.check(&password, &stored),
            None => {
                passwords.check_nothing(&password);
                Checked::Wrong
            }
        })
        .await
        .map_err(|_| AuthError::Unavailable)
    }

    fn verify_link(&self, user_id: Uuid, email: &str) -> String {
        let token = self.links.sign(&LinkClaims {
            sub: user_id,
            purpose: Purpose::Verify,
            exp: unix_now() + LINK_SECONDS,
            email: Some(email.to_owned()),
            pw: None,
        });
        format!("{}/v1/auth/verify?token={token}", self.public_url)
    }

    /// Creates an unverified account and writes its verification email to the outbox
    /// (spec §5.2). The answer is the same, in the same time, whether or not the email
    /// has an account: an existing unverified account gets a new verification email, a
    /// verified one an "account exists" email, and neither changes. Only a taken handle,
    /// which is public anyway, is reported.
    pub async fn signup(&self, signup: Signup) -> Result<(), AuthError> {
        let email = rules::email(&signup.email)?;
        let handle = rules::handle(&signup.handle)?;
        let display_name = rules::display_name(&signup.display_name)?;
        rules::password(&signup.password)?;
        // Hashed before anything else, whether or not the account will be created.
        let password_hash = self.hash(signup.password).await?;

        let mut tx = self.pool.begin().await?;
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM auth.user_account WHERE handle = $1::citext)",
        )
        .bind(&handle)
        .fetch_one(&mut *tx)
        .await?;
        if taken {
            return Err(AuthError::HandleTaken);
        }
        let user_id = Uuid::now_v7();
        let inserted = sqlx::query(
            "INSERT INTO auth.user_account (user_id, email, handle, display_name, password_hash) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (email) DO NOTHING RETURNING user_id",
        )
        .bind(user_id)
        .bind(&email)
        .bind(&handle)
        .bind(&display_name)
        .bind(&password_hash)
        .fetch_optional(&mut *tx)
        .await;
        let inserted = match inserted {
            Ok(row) => row.is_some(),
            Err(error) if is_violation(&error, "user_account_handle_key") => {
                return Err(AuthError::HandleTaken);
            }
            Err(error) => return Err(error.into()),
        };
        let event = if inserted {
            Event::EmailSend {
                user_id,
                to: email.clone(),
                email: Email::VerifyEmail {
                    link: self.verify_link(user_id, &email),
                    handle,
                },
            }
        } else {
            let existing = sqlx::query(
                "SELECT user_id, email::text, handle::text, email_verified_at IS NOT NULL \
                 FROM auth.user_account WHERE email = $1::citext",
            )
            .bind(&email)
            .fetch_one(&mut *tx)
            .await?;
            let user_id: Uuid = existing.get(0);
            let to: String = existing.get(1);
            let handle: String = existing.get(2);
            let email = if existing.get::<bool, _>(3) {
                Email::AccountExists { handle }
            } else {
                Email::VerifyEmail {
                    link: self.verify_link(user_id, &to),
                    handle,
                }
            };
            Event::EmailSend { user_id, to, email }
        };
        outbox::write(&mut tx, &event).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Opens a verification link: marks the email verified and writes `user.verified`
    /// (spec §5.2). Opening it again changes nothing. It never signs the user in.
    pub async fn verify_email(&self, token: &str) -> Result<(), AuthError> {
        let claims = self
            .links
            .verify(token, Purpose::Verify, unix_now())
            .map_err(link_error)?;
        let email = claims.email.ok_or(AuthError::LinkInvalid)?;
        let mut tx = self.pool.begin().await?;
        // The link binds the address it was sent to: a link for an old address of the
        // account (after a future email change) verifies nothing.
        let verified = sqlx::query(
            "UPDATE auth.user_account SET email_verified_at = now() \
             WHERE user_id = $1 AND email = $2::citext AND email_verified_at IS NULL RETURNING user_id",
        )
        .bind(claims.sub)
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
        if verified.is_some() {
            outbox::write(
                &mut tx,
                &Event::UserVerified {
                    user_id: claims.sub,
                },
            )
            .await?;
            tx.commit().await?;
            return Ok(());
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM auth.user_account WHERE user_id = $1 AND email = $2::citext)",
        )
        .bind(claims.sub)
        .bind(&email)
        .fetch_one(&mut *tx)
        .await?;
        if exists {
            Ok(())
        } else {
            Err(AuthError::LinkInvalid)
        }
    }

    /// Sends the verification email again if `email` has an unverified account. The
    /// answer is the same either way.
    pub async fn resend_verification(&self, email: &str) -> Result<(), AuthError> {
        let email = rules::email(email)?;
        let mut tx = self.pool.begin().await?;
        let account = sqlx::query(
            "SELECT user_id, email::text, handle::text FROM auth.user_account \
             WHERE email = $1::citext AND email_verified_at IS NULL",
        )
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(account) = account {
            let user_id: Uuid = account.get(0);
            let to: String = account.get(1);
            let event = Event::EmailSend {
                user_id,
                email: Email::VerifyEmail {
                    link: self.verify_link(user_id, &to),
                    handle: account.get(2),
                },
                to,
            };
            outbox::write(&mut tx, &event).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Checks an email and password; the account's id if they match and the email is
    /// verified. An unknown email and a wrong password are the same error and take the
    /// same time. A hash made with outdated parameters is replaced.
    pub(crate) async fn sign_in(&self, email: &str, password: &str) -> Result<Uuid, AuthError> {
        let Ok(email) = rules::email(email) else {
            return Err(AuthError::InvalidCredentials);
        };
        let account = sqlx::query(
            "SELECT user_id, password_hash, email_verified_at IS NOT NULL \
             FROM auth.user_account WHERE email = $1::citext",
        )
        .bind(&email)
        .fetch_optional(&self.pool)
        .await?;
        let stored: Option<String> = account.as_ref().map(|row| row.get(1));
        let checked = self
            .check_password(password.to_owned(), stored.clone())
            .await?;
        let (Some(account), Some(stored), Checked::Right { rehash }) = (account, stored, checked)
        else {
            return Err(AuthError::InvalidCredentials);
        };
        let user_id: Uuid = account.get(0);
        if rehash {
            let new_hash = self.hash(password.to_owned()).await?;
            // Only if the password did not change meanwhile.
            sqlx::query(
                "UPDATE auth.user_account SET password_hash = $3 \
                 WHERE user_id = $1 AND password_hash = $2",
            )
            .bind(user_id)
            .bind(&stored)
            .bind(&new_hash)
            .execute(&self.pool)
            .await?;
        }
        if !account.get::<bool, _>(2) {
            return Err(AuthError::EmailNotVerified);
        }
        Ok(user_id)
    }

    /// `POST /v1/auth/login {email, password}`: a registration token, which can only
    /// register a device (spec §5.3).
    pub async fn login(&self, email: &str, password: &str) -> Result<RegistrationToken, AuthError> {
        let user_id = self.sign_in(email, password).await?;
        let token = self.links.sign(&LinkClaims {
            sub: user_id,
            purpose: Purpose::Register,
            exp: unix_now() + REGISTRATION_TOKEN_SECONDS,
            email: None,
            pw: None,
        });
        Ok(RegistrationToken {
            token,
            expires_in_s: REGISTRATION_TOKEN_SECONDS,
        })
    }

    /// Writes a password-reset email if `email` has an account (spec §5.4). The link
    /// binds a fingerprint of the current password hash. The answer is the same either
    /// way.
    pub async fn forgot_password(&self, email: &str) -> Result<(), AuthError> {
        let email = rules::email(email)?;
        let mut tx = self.pool.begin().await?;
        let account = sqlx::query(
            "SELECT user_id, email::text, password_hash FROM auth.user_account WHERE email = $1::citext",
        )
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(account) = account {
            let user_id: Uuid = account.get(0);
            let token = self.links.sign(&LinkClaims {
                sub: user_id,
                purpose: Purpose::Reset,
                exp: unix_now() + LINK_SECONDS,
                email: None,
                pw: Some(password_stamp(account.get(2))),
            });
            let link = format!("{}/v1/auth/password/reset?token={token}", self.public_url);
            let event = Event::EmailSend {
                user_id,
                to: account.get(1),
                email: Email::ResetPassword { link },
            };
            outbox::write(&mut tx, &event).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Opens a reset link: sets the new password, revokes every refresh token of the
    /// account and writes `user.password_reset`, on which every device is kicked
    /// (spec §5.4). Setting the password changes the stamp, so the link works once.
    pub async fn reset_password(&self, token: &str, password: &str) -> Result<(), AuthError> {
        let claims = self
            .links
            .verify(token, Purpose::Reset, unix_now())
            .map_err(link_error)?;
        rules::password(password)?;
        let new_hash = self.hash(password.to_owned()).await?;
        let mut tx = self.pool.begin().await?;
        let current: Option<String> = sqlx::query_scalar(
            "SELECT password_hash FROM auth.user_account WHERE user_id = $1 FOR UPDATE",
        )
        .bind(claims.sub)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(current) = current else {
            return Err(AuthError::LinkInvalid);
        };
        if claims.pw.as_deref() != Some(password_stamp(&current).as_str()) {
            return Err(AuthError::LinkInvalid);
        }
        sqlx::query("UPDATE auth.user_account SET password_hash = $2 WHERE user_id = $1")
            .bind(claims.sub)
            .bind(&new_hash)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE auth.refresh_token SET revoked_at = now() WHERE revoked_at IS NULL \
             AND device_id IN (SELECT device_id FROM auth.device WHERE user_id = $1)",
        )
        .bind(claims.sub)
        .execute(&mut *tx)
        .await?;
        outbox::write(
            &mut tx,
            &Event::UserPasswordReset {
                user_id: claims.sub,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Whether `error` violates the constraint `name`.
pub(crate) fn is_violation(error: &sqlx::Error, name: &str) -> bool {
    error
        .as_database_error()
        .and_then(|database| database.constraint())
        == Some(name)
}
```

Replace `crates/push-auth/src/lib.rs` with:

```rust
//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.

mod db;
mod error;
pub mod links;
pub mod outbox;
pub mod password;
pub mod rules;
mod service;
pub mod tokens;

pub use db::{MIGRATOR, migrate_database};
pub use error::AuthError;
pub use service::{
    Auth, LINK_SECONDS, REGISTRATION_TOKEN_SECONDS, RegistrationToken, Settings, Signup,
};
```

- [ ] **Step 6: Run the tests**

```powershell
cargo test -p push-auth
```

Expected: `test result: ok. 24 passed` (lib) and `test result: ok. 10 passed` for
`tests\accounts.rs`, plus the 3 schema tests.

- [ ] **Step 7: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.toml Cargo.lock crates/push-auth
git commit -m "feat(auth): signup, email verification, login and password reset with the outbox"
```

Expected: 653 tests pass.

---

### Task 5: Devices, token rotation, revocation and profiles

**Files:**
- Modify: `crates/push-auth/Cargo.toml`, `crates/push-auth/src/lib.rs`
- Create: `crates/push-auth/src/devices.rs`, `src/profiles.rs`, `tests/devices.rs`

- [ ] **Step 1: Dependencies**

Replace `crates/push-auth/Cargo.toml` with (adds `chrono` for device timestamps):

```toml
[package]
name = "push-auth"
description = "The push-messenger auth service: accounts, email links, devices and tokens."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
argon2.workspace = true
base64.workspace = true
chrono.workspace = true
hmac.workspace = true
jsonwebtoken.workspace = true
push-store.workspace = true
rand.workspace = true
ring.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
sqlx.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["rt"] }
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing tests**

`the_device_limit_holds_under_concurrent_registrations` starts 13 registrations at once
and expects exactly 10 to succeed. Create `crates/push-auth/tests/devices.rs`:

```rust
//! Devices, tokens and profiles on a real PostgreSQL (Docker must be running). Each test
//! starts a container of its own; they run in parallel.

use std::sync::Arc;

use push_auth::links::{LinkClaims, LinkSigner, Purpose};
use push_auth::password::Passwords;
use push_auth::tokens::{AccessTokens, refresh_hash};
use push_auth::{Auth, AuthError, Caller, MAX_ACTIVE_DEVICES, Settings, Signup, migrate_database};
use serde_json::Value;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

const LINK_KEY: [u8; 32] = [3; 32];
const AUDIENCE: &str = "wss://api.example.com/v1/socket";

struct World {
    _container: ContainerAsync<Postgres>,
    auth: Auth,
    pool: PgPool,
}

async fn world() -> World {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    // Docker publishes ports on IPv4; "localhost" would try IPv6 first on every connect.
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    migrate_database(&url).await.expect("migrate");
    let pool = PgPool::connect(&url).await.expect("connect");
    let auth = Auth::new(
        pool.clone(),
        Settings {
            public_url: "https://api.example.com".to_owned(),
            passwords: Passwords::new(64, 1, 1).unwrap(),
            links: LinkSigner::new(LINK_KEY.to_vec()).unwrap(),
            tokens: AccessTokens::from_seed(&[5; 32], AUDIENCE).unwrap(),
            hash_concurrency: 4,
        },
    );
    World {
        _container: container,
        auth,
        pool,
    }
}

async fn outbox(pool: &PgPool) -> Vec<(String, Value)> {
    sqlx::query_as("SELECT kind, payload FROM auth.outbox ORDER BY created_at, event_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// A verified account `name@example.com` with the handle `name`; returns a registration
/// token.
async fn verified(world: &World, name: &str) -> String {
    let email = format!("{name}@example.com");
    world
        .auth
        .signup(Signup {
            email: email.clone(),
            password: "correct horse".to_owned(),
            handle: name.to_owned(),
            display_name: name.to_uppercase(),
        })
        .await
        .unwrap();
    let link = outbox(&world.pool)
        .await
        .into_iter()
        .rev()
        .find(|(_, payload)| payload["to"] == email.as_str())
        .map(|(_, payload)| payload["params"]["link"].as_str().unwrap().to_owned())
        .unwrap();
    let token = link.split_once("?token=").unwrap().1;
    world.auth.verify_email(token).await.unwrap();
    world
        .auth
        .login(&email, "correct horse")
        .await
        .unwrap()
        .token
}

fn caller(tokens: &push_auth::Tokens) -> Caller {
    Caller {
        user_id: tokens.user_id,
        device_id: tokens.device_id,
    }
}

#[tokio::test]
async fn a_registered_device_gets_tokens_and_is_announced() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    let tokens = world
        .auth
        .register_device(&registration, "Alice's phone", "ios")
        .await
        .unwrap();
    assert_eq!(tokens.access_expires_in_s, 900);
    assert_eq!(tokens.refresh_expires_in_s, 30 * 24 * 3600);
    assert_eq!(tokens.device_id.get_version_num(), 7);

    let claims = AccessTokens::from_seed(&[5; 32], AUDIENCE)
        .unwrap()
        .verify(&tokens.access_token)
        .unwrap();
    assert_eq!(claims.sub, tokens.user_id);
    assert_eq!(claims.device_id, tokens.device_id);
    assert!(claims.email_verified);
    assert_eq!(
        world.auth.authenticate(&tokens.access_token).await,
        Ok(caller(&tokens))
    );

    let (kind, payload) = outbox(&world.pool).await.pop().unwrap();
    assert_eq!(kind, "device.registered");
    assert_eq!(payload["user_id"], tokens.user_id.to_string());
    assert_eq!(payload["device_id"], tokens.device_id.to_string());

    // Only the hash is stored.
    let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT token_hash FROM auth.refresh_token")
        .fetch_all(&world.pool)
        .await
        .unwrap();
    assert_eq!(stored, [refresh_hash(&tokens.refresh_token).to_vec()]);

    let devices = world.auth.list_devices(caller(&tokens)).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_id, tokens.device_id);
    assert_eq!(devices[0].name, "Alice's phone");
    assert_eq!(devices[0].platform.as_str(), "ios");
    assert!(devices[0].last_seen_at.is_some());
}

#[tokio::test]
async fn each_token_does_only_its_own_job() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    let tokens = world
        .auth
        .register_device(&registration, "Phone", "android")
        .await
        .unwrap();
    let signer = LinkSigner::new(LINK_KEY.to_vec()).unwrap();
    let expired = signer.sign(&LinkClaims {
        sub: tokens.user_id,
        purpose: Purpose::Register,
        exp: 1,
        email: None,
        pw: None,
    });
    let verify_link = signer.sign(&LinkClaims {
        sub: tokens.user_id,
        purpose: Purpose::Verify,
        exp: i64::MAX,
        email: Some("alice@example.com".to_owned()),
        pw: None,
    });
    for token in [&tokens.access_token, &expired, &verify_link] {
        assert_eq!(
            world.auth.register_device(token, "Phone", "web").await,
            Err(AuthError::Unauthorized)
        );
    }
    for token in [&registration, &tokens.refresh_token] {
        assert_eq!(
            world.auth.authenticate(token).await,
            Err(AuthError::Unauthorized)
        );
    }
    let stale = AccessTokens::from_seed(&[5; 32], AUDIENCE)
        .unwrap()
        .issue(tokens.user_id, tokens.device_id, 1)
        .0;
    assert_eq!(
        world.auth.authenticate(&stale).await,
        Err(AuthError::TokenExpired)
    );
    assert!(matches!(
        world
            .auth
            .register_device(&registration, "Phone", "toaster")
            .await,
        Err(AuthError::Invalid(_))
    ));
}

#[tokio::test]
async fn the_device_limit_holds_under_concurrent_registrations() {
    let world = world().await;
    let registration = Arc::new(verified(&world, "alice").await);
    let auth = Arc::new(world.auth.clone());
    let attempts = (0..MAX_ACTIVE_DEVICES + 3).map(|index| {
        let (auth, registration) = (Arc::clone(&auth), Arc::clone(&registration));
        tokio::spawn(async move {
            auth.register_device(&registration, &format!("Device {index}"), "other")
                .await
        })
    });
    let mut registered = Vec::new();
    let mut refused = 0;
    for attempt in attempts.collect::<Vec<_>>() {
        match attempt.await.unwrap() {
            Ok(tokens) => registered.push(tokens),
            Err(AuthError::TooManyDevices) => refused += 1,
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(registered.len() as i64, MAX_ACTIVE_DEVICES);
    assert_eq!(refused, 3);

    // Revoking one makes room for one.
    let first = &registered[0];
    world
        .auth
        .revoke_device(caller(first), first.device_id)
        .await
        .unwrap();
    world
        .auth
        .register_device(&registration, "Replacement", "web")
        .await
        .unwrap();
    assert_eq!(
        world
            .auth
            .register_device(&registration, "One too many", "web")
            .await,
        Err(AuthError::TooManyDevices)
    );
}

#[tokio::test]
async fn refreshing_rotates_and_reuse_revokes_the_device_tokens() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    let first = world
        .auth
        .register_device(&registration, "Phone", "ios")
        .await
        .unwrap();
    let second = world.auth.refresh(&first.refresh_token).await.unwrap();
    assert_eq!(second.device_id, first.device_id);
    assert_ne!(second.refresh_token, first.refresh_token);
    assert_ne!(second.access_token, first.access_token);
    world.auth.authenticate(&second.access_token).await.unwrap();

    // The first token was rotated: using it again revokes the whole device session.
    assert_eq!(
        world.auth.refresh(&first.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        world.auth.refresh(&second.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        world.auth.refresh("never-issued").await,
        Err(AuthError::Unauthorized)
    );
    // Signing in again on the device starts a new session.
    let third = world
        .auth
        .login_device("alice@example.com", "correct horse", first.device_id)
        .await
        .unwrap();
    world.auth.refresh(&third.refresh_token).await.unwrap();
}

#[tokio::test]
async fn signing_in_on_a_device_replaces_its_session_and_needs_an_active_own_device() {
    let world = world().await;
    let alice = verified(&world, "alice").await;
    let bob = verified(&world, "bob").await;
    let phone = world
        .auth
        .register_device(&alice, "Phone", "ios")
        .await
        .unwrap();
    let bobs = world
        .auth
        .register_device(&bob, "Laptop", "desktop")
        .await
        .unwrap();

    let again = world
        .auth
        .login_device("alice@example.com", "correct horse", phone.device_id)
        .await
        .unwrap();
    assert_eq!(again.device_id, phone.device_id);
    assert_eq!(
        world.auth.refresh(&phone.refresh_token).await,
        Err(AuthError::Unauthorized),
        "the earlier session is revoked"
    );
    assert_eq!(
        world
            .auth
            .login_device("alice@example.com", "correct horse", bobs.device_id)
            .await,
        Err(AuthError::DeviceNotFound)
    );
    assert_eq!(
        world
            .auth
            .login_device("alice@example.com", "wrong horse", phone.device_id)
            .await,
        Err(AuthError::InvalidCredentials)
    );
    world
        .auth
        .revoke_device(caller(&again), phone.device_id)
        .await
        .unwrap();
    assert_eq!(
        world
            .auth
            .login_device("alice@example.com", "correct horse", phone.device_id)
            .await,
        Err(AuthError::DeviceNotFound)
    );
}

#[tokio::test]
async fn revoking_a_device_ends_its_tokens_and_is_announced() {
    let world = world().await;
    let alice = verified(&world, "alice").await;
    let bob = verified(&world, "bob").await;
    let phone = world
        .auth
        .register_device(&alice, "Phone", "ios")
        .await
        .unwrap();
    let laptop = world
        .auth
        .register_device(&alice, "Laptop", "desktop")
        .await
        .unwrap();
    let bobs = world
        .auth
        .register_device(&bob, "Phone", "android")
        .await
        .unwrap();

    assert_eq!(
        world
            .auth
            .revoke_device(caller(&laptop), bobs.device_id)
            .await,
        Err(AuthError::DeviceNotFound),
        "not Alice's device"
    );
    world
        .auth
        .revoke_device(caller(&laptop), phone.device_id)
        .await
        .unwrap();
    let (kind, payload) = outbox(&world.pool).await.pop().unwrap();
    assert_eq!(kind, "device.revoked");
    assert_eq!(payload["device_id"], phone.device_id.to_string());
    assert_eq!(
        world.auth.authenticate(&phone.access_token).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        world.auth.refresh(&phone.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        world
            .auth
            .revoke_device(caller(&laptop), phone.device_id)
            .await,
        Err(AuthError::DeviceNotFound),
        "already revoked"
    );
    let listed: Vec<Uuid> = world
        .auth
        .list_devices(caller(&laptop))
        .await
        .unwrap()
        .into_iter()
        .map(|device| device.device_id)
        .collect();
    assert_eq!(listed, [laptop.device_id]);
}

#[tokio::test]
async fn a_password_reset_revokes_every_refresh_token() {
    let world = world().await;
    let alice = verified(&world, "alice").await;
    let phone = world
        .auth
        .register_device(&alice, "Phone", "ios")
        .await
        .unwrap();
    world
        .auth
        .forgot_password("alice@example.com")
        .await
        .unwrap();
    let (_, payload) = outbox(&world.pool).await.pop().unwrap();
    let link = payload["params"]["link"].as_str().unwrap();
    let token = link.split_once("?token=").unwrap().1;
    world
        .auth
        .reset_password(token, "battery staple")
        .await
        .unwrap();
    assert_eq!(
        world.auth.refresh(&phone.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
}

#[tokio::test]
async fn profiles_show_handles_and_only_the_owner_sees_the_email() {
    let world = world().await;
    let alice = verified(&world, "alice").await;
    let phone = world
        .auth
        .register_device(&alice, "Phone", "ios")
        .await
        .unwrap();
    let own = world.auth.own_profile(caller(&phone)).await.unwrap();
    assert_eq!(own.handle, "alice");
    assert_eq!(own.display_name, "ALICE");
    assert_eq!(own.email, "alice@example.com");
    let renamed = world
        .auth
        .set_display_name(caller(&phone), "  Alice A. ")
        .await
        .unwrap();
    assert_eq!(renamed.display_name, "Alice A.");
    assert!(matches!(
        world.auth.set_display_name(caller(&phone), "").await,
        Err(AuthError::Invalid(_))
    ));

    let found = world.auth.lookup("@ALICE").await.unwrap();
    assert_eq!(found.user_id, phone.user_id);
    assert_eq!(found.handle, "alice");
    assert_eq!(found.display_name, "Alice A.");
    assert_eq!(
        world.auth.lookup("nobody").await,
        Err(AuthError::UserNotFound)
    );
    // Unverified accounts are not users yet.
    world
        .auth
        .signup(Signup {
            email: "carol@example.com".to_owned(),
            password: "correct horse".to_owned(),
            handle: "carol".to_owned(),
            display_name: "Carol".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(
        world.auth.lookup("carol").await,
        Err(AuthError::UserNotFound)
    );
    assert!(matches!(
        world.auth.lookup("no").await,
        Err(AuthError::Invalid(_))
    ));
}
```

```powershell
cargo test -p push-auth --test devices
```

Expected: a compile error — `unresolved imports push_auth::Caller,
push_auth::MAX_ACTIVE_DEVICES`.

- [ ] **Step 3: Devices and tokens**

Create `crates/push-auth/src/devices.rs`:

```rust
//! Devices and sign-in tokens (spec §5.1, §5.3): registering a device with a registration
//! token, signing in again on a known device, refreshing with rotation and reuse
//! detection, listing and revoking devices, and checking access tokens.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::error::AuthError;
use crate::links::Purpose;
use crate::outbox::{self, Event};
use crate::rules::{self, Platform};
use crate::service::{Auth, unix_now};
use crate::tokens::{
    ACCESS_TOKEN_TTL, REFRESH_TOKEN_TTL, TokenError, new_refresh_token, refresh_hash,
};

/// Most active devices per account (spec §5.3).
pub const MAX_ACTIVE_DEVICES: i64 = 10;

/// The sign-in credentials of one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub user_id: Uuid,
    pub device_id: Uuid,
    pub access_token: String,
    pub access_expires_in_s: i64,
    pub refresh_token: String,
    pub refresh_expires_in_s: i64,
}

/// The signed-in user and device behind a valid access token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    pub user_id: Uuid,
    pub device_id: Uuid,
}

/// An active device, as `GET /v1/devices` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device_id: Uuid,
    pub name: String,
    pub platform: Platform,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl Auth {
    /// A new refresh token for `device`, stored as its hash, and a new access token.
    async fn issue(
        &self,
        connection: &mut PgConnection,
        user_id: Uuid,
        device_id: Uuid,
    ) -> Result<Tokens, AuthError> {
        let refresh_token = new_refresh_token();
        sqlx::query(
            "INSERT INTO auth.refresh_token (token_hash, device_id, expires_at) \
             VALUES ($1, $2, now() + make_interval(secs => $3))",
        )
        .bind(refresh_hash(&refresh_token).as_slice())
        .bind(device_id)
        .bind(REFRESH_TOKEN_TTL.as_secs() as f64)
        .execute(&mut *connection)
        .await?;
        sqlx::query("UPDATE auth.device SET last_seen_at = now() WHERE device_id = $1")
            .bind(device_id)
            .execute(&mut *connection)
            .await?;
        let (access_token, _) = self.tokens.issue(user_id, device_id, unix_now());
        Ok(Tokens {
            user_id,
            device_id,
            access_token,
            access_expires_in_s: ACCESS_TOKEN_TTL.as_secs() as i64,
            refresh_token,
            refresh_expires_in_s: REFRESH_TOKEN_TTL.as_secs() as i64,
        })
    }

    /// `POST /v1/devices {name, platform}` with a registration token: creates the device,
    /// writes `device.registered` and returns its tokens. Refused when the account has
    /// [`MAX_ACTIVE_DEVICES`] active devices; the account row is locked, so concurrent
    /// registrations cannot pass the limit together.
    pub async fn register_device(
        &self,
        registration_token: &str,
        name: &str,
        platform: &str,
    ) -> Result<Tokens, AuthError> {
        let claims = self
            .links
            .verify(registration_token, Purpose::Register, unix_now())
            .map_err(|_| AuthError::Unauthorized)?;
        let name = rules::device_name(name)?;
        let platform = Platform::parse(platform)?;
        let mut tx = self.pool.begin().await?;
        let verified: Option<bool> = sqlx::query_scalar(
            "SELECT email_verified_at IS NOT NULL FROM auth.user_account \
             WHERE user_id = $1 FOR UPDATE",
        )
        .bind(claims.sub)
        .fetch_optional(&mut *tx)
        .await?;
        match verified {
            None => return Err(AuthError::Unauthorized),
            Some(false) => return Err(AuthError::EmailNotVerified),
            Some(true) => {}
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM auth.device WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(claims.sub)
        .fetch_one(&mut *tx)
        .await?;
        if active >= MAX_ACTIVE_DEVICES {
            return Err(AuthError::TooManyDevices);
        }
        let device_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO auth.device (device_id, user_id, name, platform) VALUES ($1, $2, $3, $4)",
        )
        .bind(device_id)
        .bind(claims.sub)
        .bind(&name)
        .bind(platform.as_str())
        .execute(&mut *tx)
        .await?;
        let tokens = self.issue(&mut tx, claims.sub, device_id).await?;
        let event = Event::DeviceRegistered {
            user_id: claims.sub,
            device_id,
        };
        outbox::write(&mut tx, &event).await?;
        tx.commit().await?;
        Ok(tokens)
    }

    /// `POST /v1/auth/login {email, password, device_id}` for an active device of the
    /// account: new tokens for it. Its earlier refresh tokens are revoked, since this
    /// sign-in replaces them.
    pub async fn login_device(
        &self,
        email: &str,
        password: &str,
        device_id: Uuid,
    ) -> Result<Tokens, AuthError> {
        let user_id = self.sign_in(email, password).await?;
        let mut tx = self.pool.begin().await?;
        let active = sqlx::query(
            "SELECT 1 FROM auth.device WHERE device_id = $1 AND user_id = $2 \
             AND revoked_at IS NULL FOR UPDATE",
        )
        .bind(device_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if active.is_none() {
            return Err(AuthError::DeviceNotFound);
        }
        revoke_refresh_tokens(&mut tx, device_id).await?;
        let tokens = self.issue(&mut tx, user_id, device_id).await?;
        tx.commit().await?;
        Ok(tokens)
    }

    /// `POST /v1/auth/refresh`: exchanges a refresh token for new tokens and retires it
    /// (rotation). Presenting a retired token again means it was copied: every refresh
    /// token of that device is revoked and the device must sign in again.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, AuthError> {
        let hash = refresh_hash(refresh_token);
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT t.device_id, d.user_id, t.rotated_to IS NOT NULL, \
             t.revoked_at IS NULL AND t.expires_at > now() AND d.revoked_at IS NULL \
             FROM auth.refresh_token t JOIN auth.device d USING (device_id) \
             WHERE t.token_hash = $1 FOR UPDATE OF t",
        )
        .bind(hash.as_slice())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(AuthError::Unauthorized);
        };
        let device_id: Uuid = row.get(0);
        let user_id: Uuid = row.get(1);
        if row.get::<bool, _>(2) {
            tracing::warn!(%device_id, "a rotated refresh token was used again; revoking the device's tokens");
            revoke_refresh_tokens(&mut tx, device_id).await?;
            tx.commit().await?;
            return Err(AuthError::Unauthorized);
        }
        if !row.get::<bool, _>(3) {
            return Err(AuthError::Unauthorized);
        }
        let tokens = self.issue(&mut tx, user_id, device_id).await?;
        sqlx::query("UPDATE auth.refresh_token SET rotated_to = $2 WHERE token_hash = $1")
            .bind(hash.as_slice())
            .bind(refresh_hash(&tokens.refresh_token).as_slice())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(tokens)
    }

    /// The caller behind an access token: valid, and its device still active.
    pub async fn authenticate(&self, access_token: &str) -> Result<Caller, AuthError> {
        let claims = self
            .tokens
            .verify(access_token)
            .map_err(|error| match error {
                TokenError::Expired => AuthError::TokenExpired,
                TokenError::Invalid => AuthError::Unauthorized,
            })?;
        let active = sqlx::query(
            "SELECT 1 FROM auth.device WHERE device_id = $1 AND user_id = $2 \
             AND revoked_at IS NULL",
        )
        .bind(claims.device_id)
        .bind(claims.sub)
        .fetch_optional(&self.pool)
        .await?;
        if active.is_none() {
            return Err(AuthError::Unauthorized);
        }
        Ok(Caller {
            user_id: claims.sub,
            device_id: claims.device_id,
        })
    }

    /// `GET /v1/devices`: the caller's active devices, oldest first.
    pub async fn list_devices(&self, caller: Caller) -> Result<Vec<DeviceInfo>, AuthError> {
        let rows = sqlx::query(
            "SELECT device_id, name, platform, created_at, last_seen_at FROM auth.device \
             WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at, device_id",
        )
        .bind(caller.user_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(DeviceInfo {
                    device_id: row.get(0),
                    name: row.get(1),
                    platform: Platform::parse(row.get(2)).map_err(|_| AuthError::Unavailable)?,
                    created_at: row.get(3),
                    last_seen_at: row.get(4),
                })
            })
            .collect()
    }

    /// `DELETE /v1/devices/{id}`: revokes one of the caller's devices (the caller's own
    /// included) and its refresh tokens, and writes `device.revoked`, on which
    /// message-svc kicks it (spec §5.3).
    pub async fn revoke_device(&self, caller: Caller, device_id: Uuid) -> Result<(), AuthError> {
        let mut tx = self.pool.begin().await?;
        let revoked = sqlx::query(
            "UPDATE auth.device SET revoked_at = now() WHERE device_id = $1 AND user_id = $2 \
             AND revoked_at IS NULL RETURNING device_id",
        )
        .bind(device_id)
        .bind(caller.user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if revoked.is_none() {
            return Err(AuthError::DeviceNotFound);
        }
        revoke_refresh_tokens(&mut tx, device_id).await?;
        let event = Event::DeviceRevoked {
            user_id: caller.user_id,
            device_id,
        };
        outbox::write(&mut tx, &event).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn revoke_refresh_tokens(
    connection: &mut PgConnection,
    device_id: Uuid,
) -> Result<(), AuthError> {
    sqlx::query(
        "UPDATE auth.refresh_token SET revoked_at = now() \
         WHERE device_id = $1 AND revoked_at IS NULL",
    )
    .bind(device_id)
    .execute(connection)
    .await?;
    Ok(())
}
```

- [ ] **Step 4: Profiles**

Create `crates/push-auth/src/profiles.rs`:

```rust
//! Profiles and handles (spec §5.5): the caller's own profile, and finding a user by
//! @handle. Emails are never shown to anyone but their owner.

use sqlx::Row;
use uuid::Uuid;

use crate::devices::Caller;
use crate::error::AuthError;
use crate::rules;
use crate::service::Auth;

/// What anyone may see of a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicProfile {
    pub user_id: Uuid,
    pub handle: String,
    pub display_name: String,
}

/// The caller's own profile, with their email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnProfile {
    pub user_id: Uuid,
    pub handle: String,
    pub display_name: String,
    pub email: String,
}

impl Auth {
    /// `GET /v1/users/me`.
    pub async fn own_profile(&self, caller: Caller) -> Result<OwnProfile, AuthError> {
        let row = sqlx::query(
            "SELECT handle::text, display_name, email::text FROM auth.user_account \
             WHERE user_id = $1",
        )
        .bind(caller.user_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AuthError::Unauthorized)?;
        Ok(OwnProfile {
            user_id: caller.user_id,
            handle: row.get(0),
            display_name: row.get(1),
            email: row.get(2),
        })
    }

    /// `PATCH /v1/users/me {display_name}`.
    pub async fn set_display_name(
        &self,
        caller: Caller,
        display_name: &str,
    ) -> Result<OwnProfile, AuthError> {
        let display_name = rules::display_name(display_name)?;
        sqlx::query("UPDATE auth.user_account SET display_name = $2 WHERE user_id = $1")
            .bind(caller.user_id)
            .bind(&display_name)
            .execute(&self.pool)
            .await?;
        self.own_profile(caller).await
    }

    /// `GET /v1/users/lookup?handle=…`: the verified user with this handle (with or
    /// without the `@`, in any case). Unverified accounts are not users yet.
    pub async fn lookup(&self, handle: &str) -> Result<PublicProfile, AuthError> {
        let handle = rules::handle(handle)?;
        let row = sqlx::query(
            "SELECT user_id, handle::text, display_name FROM auth.user_account \
             WHERE handle = $1::citext AND email_verified_at IS NOT NULL",
        )
        .bind(&handle)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AuthError::UserNotFound)?;
        Ok(PublicProfile {
            user_id: row.get(0),
            handle: row.get(1),
            display_name: row.get(2),
        })
    }
}
```

Replace `crates/push-auth/src/lib.rs` with:

```rust
//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.

mod db;
mod devices;
mod error;
pub mod links;
pub mod outbox;
pub mod password;
mod profiles;
pub mod rules;
mod service;
pub mod tokens;

pub use db::{MIGRATOR, migrate_database};
pub use devices::{Caller, DeviceInfo, MAX_ACTIVE_DEVICES, Tokens};
pub use error::AuthError;
pub use profiles::{OwnProfile, PublicProfile};
pub use service::{
    Auth, LINK_SECONDS, REGISTRATION_TOKEN_SECONDS, RegistrationToken, Settings, Signup,
};
```

- [ ] **Step 5: Run the tests**

```powershell
cargo test -p push-auth --test devices
```

Expected: `test result: ok. 8 passed`.

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock crates/push-auth
git commit -m "feat(auth): device registration, token rotation, revocation and profiles"
```

Expected: 661 tests pass.

---

### Task 6: Rate limits in Redis

**Files:**
- Modify: `crates/push-auth/Cargo.toml`, `crates/push-auth/src/lib.rs`
- Create: `crates/push-auth/src/limits.rs`, `tests/limits.rs`

- [ ] **Step 1: Dependencies**

Replace `crates/push-auth/Cargo.toml` with:

```toml
[package]
name = "push-auth"
description = "The push-messenger auth service: accounts, email links, devices and tokens."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
argon2.workspace = true
async-trait.workspace = true
base64.workspace = true
chrono.workspace = true
hmac.workspace = true
jsonwebtoken.workspace = true
push-store.workspace = true
rand.workspace = true
redis.workspace = true
ring.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
sqlx.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "rt", "time"] }
tokio-util.workspace = true
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing Redis tests**

Create `crates/push-auth/tests/limits.rs`:

```rust
//! The Redis rate-limit counters against a real Redis (Docker must be running).

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use push_auth::limits::{Action, Counter, Limit, Limits, Outcome, RedisCounter};
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio_util::sync::CancellationToken;

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
async fn a_counter_allows_up_to_its_limit_and_then_names_the_wait() {
    let (_container, url) = redis().await;
    let counter = RedisCounter::connect(&url, CancellationToken::new())
        .await
        .unwrap();
    let limit = Limit {
        max: 2,
        window: Duration::from_secs(60),
    };
    assert_eq!(counter.hit("rl:test", limit).await, Ok(Outcome::Allowed));
    assert_eq!(counter.hit("rl:test", limit).await, Ok(Outcome::Allowed));
    match counter.hit("rl:test", limit).await {
        Ok(Outcome::Limited { retry_after_s }) => {
            assert!((1..=60).contains(&retry_after_s), "{retry_after_s}")
        }
        other => panic!("expected Limited, got {other:?}"),
    }
    let mut connection = redis::Client::open(url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let ttl: i64 = redis::cmd("TTL")
        .arg("rl:test")
        .query_async(&mut connection)
        .await
        .unwrap();
    assert!((1..=60).contains(&ttl), "the window expires: {ttl}");
}

#[tokio::test]
async fn redis_never_sees_an_email_or_an_ip() {
    let (_container, url) = redis().await;
    let counter = RedisCounter::connect(&url, CancellationToken::new())
        .await
        .unwrap();
    let limits = Limits::new(Arc::new(counter));
    let ip: IpAddr = "203.0.113.7".parse().unwrap();
    limits
        .check(Action::Login, Some(ip), Some("alice@example.com"))
        .await
        .unwrap();
    let mut connection = redis::Client::open(url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let mut keys: Vec<String> = redis::cmd("KEYS")
        .arg("*")
        .query_async(&mut connection)
        .await
        .unwrap();
    keys.sort();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys[0].starts_with("rl:login:account:"), "{keys:?}");
    assert!(keys[1].starts_with("rl:login:ip:"), "{keys:?}");
    for key in keys {
        assert!(!key.contains("alice") && !key.contains("203.0.113.7"));
    }
}

#[tokio::test]
async fn without_redis_requests_go_ahead_quickly() {
    let (container, url) = redis().await;
    let counter = RedisCounter::connect(&url, CancellationToken::new())
        .await
        .unwrap();
    container.stop().await.expect("stop Redis");
    let limits = Limits::new(Arc::new(counter));
    let started = Instant::now();
    limits
        .check(Action::Login, None, Some("alice@example.com"))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}
```

```powershell
cargo test -p push-auth --test limits
```

Expected: a compile error — `unresolved import push_auth::limits`.

- [ ] **Step 3: The limits, with their unit tests**

Create `crates/push-auth/src/limits.rs`:

```rust
//! Rate limits (spec §5.6): per account and per IP, on signup, login, verification resend,
//! password reset, opening email links and handle lookup. Accounts are never locked; a
//! request over a limit is refused with `rate_limited` and a retry delay.
//!
//! Counters are fixed windows in Redis (`INCR`, then `EXPIRE … NX`), keyed by a hash of
//! the subject, so no email address or IP is stored in Redis. When Redis does not answer
//! within 500 ms the request is let through and a warning logged: Redis is a doorbell,
//! never a reason to refuse service, and Argon2id still makes guessing slow.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::error::AuthError;

/// What is being limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Signup,
    Login,
    ResendVerification,
    ForgotPassword,
    /// Opening a verification or reset link.
    OpenLink,
    HandleLookup,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Signup => "signup",
            Self::Login => "login",
            Self::ResendVerification => "resend",
            Self::ForgotPassword => "forgot",
            Self::OpenLink => "link",
            Self::HandleLookup => "lookup",
        }
    }

    /// The limits per IP and per account: `(max requests, window)`.
    pub fn limits(self) -> (Limit, Option<Limit>) {
        const HOUR: Duration = Duration::from_secs(3600);
        const QUARTER: Duration = Duration::from_secs(900);
        let limit = |max, window| Limit { max, window };
        match self {
            Self::Signup => (limit(20, HOUR), Some(limit(5, HOUR))),
            Self::Login => (limit(60, QUARTER), Some(limit(10, QUARTER))),
            Self::ResendVerification => (limit(20, HOUR), Some(limit(3, HOUR))),
            Self::ForgotPassword => (limit(20, HOUR), Some(limit(3, HOUR))),
            Self::OpenLink => (limit(60, HOUR), None),
            // Spec §5.5: 20 lookups per hour per user.
            Self::HandleLookup => (limit(120, HOUR), Some(limit(20, HOUR))),
        }
    }
}

/// At most `max` requests per `window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max: u32,
    pub window: Duration,
}

/// The answer of a counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allowed,
    Limited { retry_after_s: u64 },
}

/// A store of fixed-window counters.
#[async_trait]
pub trait Counter: Send + Sync {
    /// Counts one request against `key` and says whether it is within `limit`.
    async fn hit(&self, key: &str, limit: Limit) -> Result<Outcome, String>;
}

/// The rate limits of the API over one counter store.
#[derive(Clone)]
pub struct Limits {
    counter: Arc<dyn Counter>,
}

impl std::fmt::Debug for Limits {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Limits")
    }
}

impl Limits {
    pub fn new(counter: Arc<dyn Counter>) -> Self {
        Self { counter }
    }

    /// Counts a request for `action` from `ip` about `account` (an email address or a
    /// user id); `RateLimited` if either is over its limit. A failing counter lets the
    /// request through.
    pub async fn check(
        &self,
        action: Action,
        ip: Option<IpAddr>,
        account: Option<&str>,
    ) -> Result<(), AuthError> {
        let (per_ip, per_account) = action.limits();
        let mut checks = Vec::new();
        if let Some(ip) = ip {
            checks.push((key(action, "ip", &ip.to_string()), per_ip));
        }
        if let (Some(account), Some(limit)) = (account, per_account) {
            let account = account.trim().to_lowercase();
            checks.push((key(action, "account", &account), limit));
        }
        for (key, limit) in checks {
            match self.counter.hit(&key, limit).await {
                Ok(Outcome::Allowed) => {}
                Ok(Outcome::Limited { retry_after_s }) => {
                    return Err(AuthError::RateLimited { retry_after_s });
                }
                Err(error) => {
                    tracing::warn!(%error, action = action.name(), "rate limit not checked");
                }
            }
        }
        Ok(())
    }
}

/// `rl:{action}:{scope}:{hash}`: the subject is hashed, never stored.
fn key(action: Action, scope: &str, subject: &str) -> String {
    let digest = Sha256::digest(subject.as_bytes());
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("rl:{}:{scope}:{hex}", action.name())
}

/// How long a Redis call may take before the request goes ahead without it.
pub const REDIS_TIMEOUT: Duration = Duration::from_millis(500);

/// Counters in Redis.
#[derive(Clone)]
pub struct RedisCounter {
    connection: ConnectionManager,
    shutdown: CancellationToken,
}

impl RedisCounter {
    /// Connects to Redis at `url`; the connection reconnects by itself. Calls give up
    /// when `shutdown` is cancelled.
    pub async fn connect(url: &str, shutdown: CancellationToken) -> redis::RedisResult<Self> {
        let client = redis::Client::open(url)?;
        let connection = client.get_connection_manager().await?;
        Ok(Self {
            connection,
            shutdown,
        })
    }
}

#[async_trait]
impl Counter for RedisCounter {
    async fn hit(&self, key: &str, limit: Limit) -> Result<Outcome, String> {
        let window = limit.window.as_secs().max(1);
        let mut pipeline = redis::pipe();
        pipeline
            .atomic()
            .incr(key, 1)
            .cmd("EXPIRE")
            .arg(key)
            .arg(window)
            .arg("NX")
            .ignore()
            .ttl(key);
        let mut connection = self.connection.clone();
        let call = pipeline.query_async::<(u64, i64)>(&mut connection);
        let (count, ttl) = tokio::select! {
            () = self.shutdown.cancelled() => return Err("shutting down".to_owned()),
            result = tokio::time::timeout(REDIS_TIMEOUT, call) => result
                .map_err(|_| "redis did not answer in time".to_owned())?
                .map_err(|error| error.to_string())?,
        };
        if count <= u64::from(limit.max) {
            Ok(Outcome::Allowed)
        } else {
            Ok(Outcome::Limited {
                retry_after_s: ttl.max(1) as u64,
            })
        }
    }
}

/// Counters in this process, for tests and single-instance local runs.
#[derive(Debug, Default)]
pub struct MemoryCounter {
    counts: Mutex<HashMap<String, (u32, Instant)>>,
}

#[async_trait]
impl Counter for MemoryCounter {
    async fn hit(&self, key: &str, limit: Limit) -> Result<Outcome, String> {
        let now = Instant::now();
        let mut counts = self.counts.lock().expect("not poisoned");
        let (count, ends) = counts
            .entry(key.to_owned())
            .or_insert((0, now + limit.window));
        if now >= *ends {
            *count = 0;
            *ends = now + limit.window;
        }
        *count += 1;
        if *count <= limit.max {
            Ok(Outcome::Allowed)
        } else {
            Ok(Outcome::Limited {
                retry_after_s: ends.saturating_duration_since(now).as_secs().max(1),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Broken;

    #[async_trait]
    impl Counter for Broken {
        async fn hit(&self, _key: &str, _limit: Limit) -> Result<Outcome, String> {
            Err("connection refused".to_owned())
        }
    }

    fn ip(last: u8) -> Option<IpAddr> {
        Some(IpAddr::from([10, 0, 0, last]))
    }

    #[tokio::test]
    async fn an_account_is_limited_across_ips_and_an_ip_across_accounts() {
        let limits = Limits::new(Arc::new(MemoryCounter::default()));
        for attempt in 0..3 {
            limits
                .check(
                    Action::ResendVerification,
                    ip(attempt),
                    Some("a@example.com"),
                )
                .await
                .unwrap();
        }
        let refused = limits
            .check(Action::ResendVerification, ip(9), Some("A@Example.com "))
            .await;
        assert!(
            matches!(refused, Err(AuthError::RateLimited { retry_after_s }) if retry_after_s > 3000),
            "{refused:?}"
        );
        // Other accounts from one IP: 20 per hour.
        for attempt in 0..20 {
            limits
                .check(
                    Action::Signup,
                    ip(1),
                    Some(&format!("{attempt}@example.com")),
                )
                .await
                .unwrap();
        }
        assert!(
            limits
                .check(Action::Signup, ip(1), Some("new@example.com"))
                .await
                .is_err()
        );
        assert!(
            limits
                .check(Action::Signup, ip(2), Some("new@example.com"))
                .await
                .is_ok()
        );
    }

    #[test]
    fn handle_lookups_follow_the_spec() {
        assert_eq!(
            Action::HandleLookup.limits().1,
            Some(Limit {
                max: 20,
                window: Duration::from_secs(3600)
            })
        );
    }

    #[tokio::test]
    async fn a_failing_counter_lets_requests_through() {
        let limits = Limits::new(Arc::new(Broken));
        for _ in 0..50 {
            limits
                .check(Action::Login, ip(1), Some("a@example.com"))
                .await
                .unwrap();
        }
    }

    #[test]
    fn keys_hash_the_subject() {
        let key = key(Action::Login, "account", "alice@example.com");
        assert!(key.starts_with("rl:login:account:"), "{key}");
        assert!(!key.contains("alice"), "{key}");
        assert_eq!(key.len(), "rl:login:account:".len() + 32);
    }
}
```

Replace `crates/push-auth/src/lib.rs` with:

```rust
//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.

mod db;
mod devices;
mod error;
pub mod limits;
pub mod links;
pub mod outbox;
pub mod password;
mod profiles;
pub mod rules;
mod service;
pub mod tokens;

pub use db::{MIGRATOR, migrate_database};
pub use devices::{Caller, DeviceInfo, MAX_ACTIVE_DEVICES, Tokens};
pub use error::AuthError;
pub use profiles::{OwnProfile, PublicProfile};
pub use service::{
    Auth, LINK_SECONDS, REGISTRATION_TOKEN_SECONDS, RegistrationToken, Settings, Signup,
};
```

- [ ] **Step 4: Run the tests**

```powershell
cargo test -p push-auth --lib --test limits
```

Expected: `test result: ok. 28 passed` (lib) and `test result: ok. 3 passed` (limits).
`without_redis_requests_go_ahead_quickly` stops Redis and still lets the request through
within the 500 ms timeout.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock crates/push-auth
git commit -m "feat(auth): rate limits in Redis"
```

Expected: 668 tests pass.

---

### Task 7: The HTTP API and the link pages

**Files:**
- Modify: `Cargo.toml`, `crates/push-auth/Cargo.toml`, `crates/push-auth/src/lib.rs`,
  `crates/push-auth/src/service.rs` (the DEV ONLY mailbox query)
- Create: `crates/push-auth/src/api.rs`, `tests/api.rs`

- [ ] **Step 1: Dependencies**

Replace `Cargo.toml` with (adds `http-body-util` and `tower` for the router tests):

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-auth",
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
argon2 = { version = "0.5", features = ["std"] }
async-trait = "0.1"
axum = "0.8"
base64 = "0.22"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
futures-util = { version = "0.3", default-features = false, features = ["sink"] }
hmac = "0.12"
http-body-util = "0.1"
jsonwebtoken = { version = "9", default-features = false }
libtest-mimic = "0.8"
prost = "0.14"
protoc-bin-vendored = "3"
push-auth = { path = "crates/push-auth" }
push-domain = { path = "crates/push-domain" }
push-gateway = { path = "crates/push-gateway" }
push-message-svc = { path = "crates/push-message-svc" }
push-proto = { path = "crates/push-proto" }
push-store = { path = "crates/push-store" }
push-testkit = { path = "crates/push-testkit" }
rand = "0.9"
redis = { version = "1", default-features = false, features = [
    "connection-manager",
    "tokio-comp",
] }
ring = "0.17"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
sqlx = { version = "0.8", default-features = false, features = [
    "chrono",
    "json",
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
tower = { version = "0.5", features = ["util"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
uuid = { version = "1", features = ["serde", "v4", "v7"] }

# Argon2id is far too slow unoptimised, and the auth tests hash many passwords.
[profile.dev.package.argon2]
opt-level = 3

[profile.dev.package.blake2]
opt-level = 3

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

Replace `crates/push-auth/Cargo.toml` with:

```toml
[package]
name = "push-auth"
description = "The push-messenger auth service: accounts, email links, devices and tokens."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
argon2.workspace = true
async-trait.workspace = true
axum.workspace = true
base64.workspace = true
chrono.workspace = true
hmac.workspace = true
jsonwebtoken.workspace = true
push-store.workspace = true
rand.workspace = true
redis.workspace = true
ring.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
sqlx.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "rt", "time"] }
tokio-util.workspace = true
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
http-body-util.workspace = true
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
tower.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing API tests**

The tests drive the router with `tower::ServiceExt::oneshot`, with the client IP in
`x-real-ip` as the ingress sends it and the DEV ONLY mailbox on (it is how they read
links). Create `crates/push-auth/tests/api.rs`:

```rust
//! The HTTP API end to end over a real PostgreSQL (Docker must be running): requests go
//! through the router as the ingress would send them, with the client IP in `x-real-ip`.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, Method, Request, StatusCode};
use http_body_util::BodyExt;
use push_auth::api::{Api, router};
use push_auth::limits::{Limits, MemoryCounter};
use push_auth::links::LinkSigner;
use push_auth::password::Passwords;
use push_auth::tokens::AccessTokens;
use push_auth::{Auth, Settings, migrate_database};
use serde_json::{Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tower::ServiceExt;

struct World {
    _container: ContainerAsync<Postgres>,
    router: Router,
}

async fn world(dev_mailbox: bool) -> World {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    // Docker publishes ports on IPv4; "localhost" would try IPv6 first on every connect.
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    migrate_database(&url).await.expect("migrate");
    let pool = PgPool::connect(&url).await.expect("connect");
    let auth = Auth::new(
        pool,
        Settings {
            public_url: "http://push.local".to_owned(),
            passwords: Passwords::new(64, 1, 1).unwrap(),
            links: LinkSigner::new(vec![3; 32]).unwrap(),
            tokens: AccessTokens::from_seed(&[5; 32], "ws://push.local/v1/socket").unwrap(),
            hash_concurrency: 4,
        },
    );
    let router = router(Api {
        auth,
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
        dev_mailbox,
    });
    World {
        _container: container,
        router,
    }
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|_| panic!("not JSON: {}", self.text))
    }
}

/// One request; `body` is sent as JSON unless it is a string, which is sent as a form.
async fn call(
    world: &World,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-real-ip", "198.51.100.7");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        None => request.body(Body::empty()),
        Some(Value::String(form)) => request
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form)),
        Some(json) => request
            .header("content-type", "application/json")
            .body(Body::from(json.to_string())),
    }
    .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        text: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

/// Signs up `name@example.com` and returns the token of its verification link, read from
/// the DEV ONLY mailbox.
async fn sign_up(world: &World, name: &str) -> String {
    let answer = call(
        world,
        Method::POST,
        "/v1/auth/signup",
        None,
        Some(json!({
            "email": format!("{name}@example.com"),
            "password": "correct horse",
            "handle": name,
            "display_name": name,
        })),
    )
    .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{}", answer.text);
    assert_eq!(answer.json(), json!({"status": "check_email"}));
    let mailbox = call(
        world,
        Method::GET,
        &format!("/dev/mailbox?to={name}@example.com"),
        None,
        None,
    )
    .await
    .json();
    let link = mailbox["emails"][0]["params"]["link"].as_str().unwrap();
    assert!(
        link.starts_with("http://push.local/v1/auth/verify?token="),
        "{link}"
    );
    link.split_once("?token=").unwrap().1.to_owned()
}

/// A verified account with one registered device: its tokens.
async fn with_device(world: &World, name: &str) -> Value {
    let token = sign_up(world, name).await;
    let verified = call(
        world,
        Method::POST,
        "/v1/auth/verify",
        None,
        Some(json!({"token": token})),
    )
    .await;
    assert_eq!(verified.json(), json!({"status": "verified"}));
    let login = call(
        world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": format!("{name}@example.com"), "password": "correct horse"})),
    )
    .await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.text);
    let registration = login.json()["registration_token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(login.json()["expires_in_s"], 300);
    let device = call(
        world,
        Method::POST,
        "/v1/devices",
        Some(&registration),
        Some(json!({"name": "Phone", "platform": "ios"})),
    )
    .await;
    assert_eq!(device.status, StatusCode::CREATED, "{}", device.text);
    device.json()
}

#[tokio::test]
async fn sign_up_verify_register_and_use_a_device() {
    let world = world(true).await;
    let tokens = with_device(&world, "alice").await;
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in_s"], 900);
    let access = tokens["access_token"].as_str().unwrap();

    let devices = call(&world, Method::GET, "/v1/devices", Some(access), None).await;
    assert_eq!(devices.status, StatusCode::OK);
    assert_eq!(devices.headers["cache-control"], "no-store");
    let listed = &devices.json()["devices"][0];
    assert_eq!(listed["device_id"], tokens["device_id"]);
    assert_eq!(listed["platform"], "ios");
    assert_eq!(listed["current"], true);

    let me = call(&world, Method::GET, "/v1/users/me", Some(access), None).await;
    assert_eq!(me.json()["email"], "alice@example.com");
    assert_eq!(me.json()["handle"], "alice");
    let renamed = call(
        &world,
        Method::PATCH,
        "/v1/users/me",
        Some(access),
        Some(json!({"display_name": "Alice A."})),
    )
    .await;
    assert_eq!(renamed.json()["display_name"], "Alice A.");
    let found = call(
        &world,
        Method::GET,
        "/v1/users/lookup?handle=%40Alice",
        Some(access),
        None,
    )
    .await;
    assert_eq!(
        found.json(),
        json!({"user_id": tokens["user_id"], "handle": "alice", "display_name": "Alice A."})
    );
    assert!(found.json().get("email").is_none());

    let refreshed = call(
        &world,
        Method::POST,
        "/v1/auth/refresh",
        None,
        Some(json!({"refresh_token": tokens["refresh_token"]})),
    )
    .await;
    assert_eq!(refreshed.status, StatusCode::OK, "{}", refreshed.text);
    let again = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({
            "email": "alice@example.com",
            "password": "correct horse",
            "device_id": tokens["device_id"],
        })),
    )
    .await;
    assert_eq!(again.json()["device_id"], tokens["device_id"]);
    let access = again.json()["access_token"].as_str().unwrap().to_owned();

    let revoked = call(
        &world,
        Method::DELETE,
        &format!("/v1/devices/{}", tokens["device_id"].as_str().unwrap()),
        Some(&access),
        None,
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    let after = call(&world, Method::GET, "/v1/devices", Some(&access), None).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    assert_eq!(after.json()["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn link_pages_change_nothing_and_leak_nothing() {
    let world = world(true).await;
    let token = sign_up(&world, "alice").await;
    let page = call(
        &world,
        Method::GET,
        &format!("/v1/auth/verify?token={token}"),
        None,
        None,
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.headers["referrer-policy"], "no-referrer");
    assert_eq!(page.headers["cache-control"], "no-store");
    assert!(
        page.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'none'")
    );
    assert!(
        page.text.contains(&format!(r#"value="{token}""#)),
        "{}",
        page.text
    );
    let hostile = call(
        &world,
        Method::GET,
        "/v1/auth/verify?token=%22%3E%3Cscript%3E",
        None,
        None,
    )
    .await;
    assert!(!hostile.text.contains("<script>"), "{}", hostile.text);

    // Opening the page did not verify: login still says so.
    let login = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": "alice@example.com", "password": "correct horse"})),
    )
    .await;
    assert_eq!(login.status, StatusCode::FORBIDDEN);
    assert_eq!(login.json()["error"]["code"], "email_not_verified");

    // The page's button posts a form and gets a page back.
    let done = call(
        &world,
        Method::POST,
        "/v1/auth/verify",
        None,
        Some(Value::String(format!("token={token}"))),
    )
    .await;
    assert_eq!(done.status, StatusCode::OK);
    assert!(
        done.text.contains("Your email address is verified"),
        "{}",
        done.text
    );
    let bad = call(
        &world,
        Method::POST,
        "/v1/auth/verify",
        None,
        Some(Value::String("token=forged.token".to_owned())),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(bad.text.contains("not valid"), "{}", bad.text);
}

#[tokio::test]
async fn a_password_reset_works_through_its_page() {
    let world = world(true).await;
    with_device(&world, "alice").await;
    let forgot = call(
        &world,
        Method::POST,
        "/v1/auth/password/forgot",
        None,
        Some(json!({"email": "alice@example.com"})),
    )
    .await;
    assert_eq!(forgot.status, StatusCode::ACCEPTED);
    let mailbox = call(
        &world,
        Method::GET,
        "/dev/mailbox?to=ALICE@example.com",
        None,
        None,
    )
    .await
    .json();
    assert_eq!(mailbox["emails"][0]["template"], "reset_password");
    let link = mailbox["emails"][0]["params"]["link"].as_str().unwrap();
    let path = link.strip_prefix("http://push.local").unwrap();
    let page = call(&world, Method::GET, path, None, None).await;
    assert_eq!(page.headers["referrer-policy"], "no-referrer");
    assert!(page.text.contains(r#"type="password""#), "{}", page.text);

    let token = link.split_once("?token=").unwrap().1;
    let changed = call(
        &world,
        Method::POST,
        "/v1/auth/password/reset",
        None,
        Some(Value::String(format!(
            "token={token}&password=battery+staple"
        ))),
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.text);
    let login = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": "alice@example.com", "password": "battery staple"})),
    )
    .await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.text);
}

#[tokio::test]
async fn errors_have_one_format() {
    let world = world(true).await;
    let garbage = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": 1})),
    )
    .await;
    assert_eq!(garbage.status, StatusCode::BAD_REQUEST);
    assert_eq!(garbage.json()["error"]["code"], "invalid");
    assert_eq!(garbage.json()["error"]["retryable"], false);

    let wrong = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": "nobody@example.com", "password": "correct horse"})),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.json()["error"]["code"], "invalid_credentials");

    let anonymous = call(&world, Method::GET, "/v1/users/me", None, None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.headers["www-authenticate"], "Bearer");

    let jwks = call(&world, Method::GET, "/v1/auth/jwks", None, None).await;
    assert_eq!(jwks.json()["keys"][0]["crv"], "Ed25519");
    assert_eq!(jwks.headers["cache-control"], "public, max-age=300");
}

#[tokio::test]
async fn login_is_rate_limited_per_account_without_locking_it() {
    let world = world(true).await;
    with_device(&world, "alice").await;
    // with_device logged in once; 9 more wrong attempts fit in the window.
    for _ in 0..9 {
        let answer = call(
            &world,
            Method::POST,
            "/v1/auth/login",
            None,
            Some(json!({"email": "alice@example.com", "password": "wrong horse"})),
        )
        .await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    }
    let limited = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": "alice@example.com", "password": "correct horse"})),
    )
    .await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.json()["error"]["code"], "rate_limited");
    assert_eq!(limited.json()["error"]["retryable"], true);
    let wait: u64 = limited.headers["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=900).contains(&wait), "{wait}");
}

#[tokio::test]
async fn the_dev_mailbox_exists_only_when_enabled() {
    let world = world(false).await;
    let answer = call(
        &world,
        Method::GET,
        "/dev/mailbox?to=a@example.com",
        None,
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
}
```

```powershell
cargo test -p push-auth --test api
```

Expected: a compile error — `unresolved import push_auth::api`.

- [ ] **Step 3: The DEV ONLY mailbox query**

In `crates/push-auth/src/service.rs`, add this method at the end of `impl Auth`, after
`reset_password`:

```rust
    /// DEV ONLY: the `email.send` payloads waiting in the outbox for the address `to`,
    /// newest first (at most 20). Until the worker sends email (Plan 6), this is how a
    /// local cluster and the smoke test read their verification links.
    pub async fn dev_mailbox(&self, to: &str) -> Result<Vec<serde_json::Value>, AuthError> {
        let emails = sqlx::query_scalar(
            "SELECT payload FROM auth.outbox WHERE kind = 'email.send' \
             AND lower(payload->>'to') = lower($1) \
             ORDER BY created_at DESC, event_id DESC LIMIT 20",
        )
        .bind(to.trim())
        .fetch_all(&self.pool)
        .await?;
        Ok(emails)
    }
```

- [ ] **Step 4: The router, with its unit tests**

Create `crates/push-auth/src/api.rs`:

```rust
//! The public HTTP API (spec §2, §5): `/v1/auth/*`, `/v1/devices/*` and `/v1/users/*`,
//! JSON in and out, plus the two small pages that email links open.
//!
//! Errors are `{"error": {"code", "message", "retryable"}}` with the status of
//! [`AuthError::status`]; 429 answers carry `Retry-After`, 401 answers
//! `WWW-Authenticate: Bearer`. No answer may be cached (`Cache-Control: no-store`) except
//! the JWKS. Pages send `Referrer-Policy: no-referrer`, so the token in their URL never
//! leaks to another site (spec §5.6), and change nothing on `GET`: the change is the
//! `POST` their button sends, so a mail scanner that opens links does nothing.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
    REFERRER_POLICY, RETRY_AFTER, WWW_AUTHENTICATE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::request::Parts;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Form, Json, Router};
use chrono::SecondsFormat;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::devices::{Caller, Tokens};
use crate::error::AuthError;
use crate::limits::{Action, Limits};
use crate::rules::Invalid;
use crate::service::{Auth, Signup};

/// Largest request body accepted.
const MAX_BODY_BYTES: usize = 16 * 1024;

/// What the handlers share.
#[derive(Debug, Clone)]
pub struct Api {
    pub auth: Auth,
    pub limits: Limits,
    /// The header that carries the client's IP, set by the ingress controller
    /// (`x-real-ip`); without it, the peer address. Only trust a header the proxy in front
    /// always overwrites.
    pub client_ip_header: Option<HeaderName>,
    /// DEV ONLY: serve `GET /dev/mailbox`, the outbox's emails to one address.
    pub dev_mailbox: bool,
}

/// The router of the public API (and, when enabled, the DEV ONLY mailbox).
pub fn router(api: Api) -> Router {
    let mut router = Router::new()
        .route("/v1/auth/signup", post(signup))
        .route("/v1/auth/verify", get(verify_page).post(verify))
        .route("/v1/auth/verify/resend", post(resend))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/password/forgot", post(forgot))
        .route("/v1/auth/password/reset", get(reset_page).post(reset))
        .route("/v1/auth/jwks", get(jwks))
        .route("/v1/devices", post(register_device).get(list_devices))
        .route("/v1/devices/{device_id}", delete(revoke_device))
        .route("/v1/users/me", get(own_profile).patch(set_display_name))
        .route("/v1/users/lookup", get(lookup));
    if api.dev_mailbox {
        router = router.route("/dev/mailbox", get(dev_mailbox));
    }
    router
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::map_response(no_store_by_default))
        .with_state(api)
}

async fn no_store_by_default(mut response: Response) -> Response {
    response
        .headers_mut()
        .entry(CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status()).expect("a valid status");
        let body = Json(json!({
            "error": {
                "code": self.code(),
                "message": self.to_string(),
                "retryable": self.retryable(),
            }
        }));
        let mut response = (status, body).into_response();
        let headers = response.headers_mut();
        match self {
            Self::RateLimited { retry_after_s } => {
                headers.insert(RETRY_AFTER, HeaderValue::from(retry_after_s));
            }
            Self::Unauthorized | Self::TokenExpired | Self::InvalidCredentials => {
                headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            }
            _ => {}
        }
        response
    }
}

/// A JSON body; anything else is an `invalid` error in the API's format.
struct JsonBody<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = AuthError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|_| bad_body())
    }
}

fn bad_body() -> AuthError {
    AuthError::Invalid(Invalid {
        field: "body",
        reason: "expected a JSON object with the documented fields",
    })
}

/// A body from an API client (JSON) or from one of the pages (a form).
enum Submitted<T> {
    Json(T),
    Form(T),
}

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Submitted<T> {
    type Rejection = AuthError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_form = request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"));
        if is_form {
            Form::<T>::from_request(request, state)
                .await
                .map(|Form(value)| Self::Form(value))
                .map_err(|_| bad_body())
        } else {
            JsonBody::<T>::from_request(request, state)
                .await
                .map(|JsonBody(value)| Self::Json(value))
        }
    }
}

/// The client's IP: from the configured header, else the peer address.
struct ClientIp(Option<IpAddr>);

impl FromRequestParts<Api> for ClientIp {
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, api: &Api) -> Result<Self, Self::Rejection> {
        let from_header = api.client_ip_header.as_ref().and_then(|name| {
            let value = parts.headers.get(name)?.to_str().ok()?;
            // X-Forwarded-For lists the client first.
            value.split(',').next()?.trim().parse().ok()
        });
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| address.ip());
        Ok(Self(from_header.or(peer)))
    }
}

fn bearer(headers: &HeaderMap) -> Result<&str, AuthError> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(AuthError::Unauthorized)
}

async fn caller(api: &Api, headers: &HeaderMap) -> Result<Caller, AuthError> {
    api.auth.authenticate(bearer(headers)?).await
}

fn tokens_json(tokens: &Tokens) -> Value {
    json!({
        "user_id": tokens.user_id,
        "device_id": tokens.device_id,
        "token_type": "Bearer",
        "access_token": tokens.access_token,
        "expires_in_s": tokens.access_expires_in_s,
        "refresh_token": tokens.refresh_token,
        "refresh_expires_in_s": tokens.refresh_expires_in_s,
    })
}

fn check_email() -> Response {
    (StatusCode::ACCEPTED, Json(json!({"status": "check_email"}))).into_response()
}

#[derive(Deserialize)]
struct SignupBody {
    email: String,
    password: String,
    handle: String,
    display_name: String,
}

async fn signup(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    JsonBody(body): JsonBody<SignupBody>,
) -> Result<Response, AuthError> {
    api.limits
        .check(Action::Signup, ip, Some(&body.email))
        .await?;
    api.auth
        .signup(Signup {
            email: body.email,
            password: body.password,
            handle: body.handle,
            display_name: body.display_name,
        })
        .await?;
    Ok(check_email())
}

#[derive(Deserialize)]
struct TokenQuery {
    #[serde(default)]
    token: String,
}

#[derive(Deserialize)]
struct TokenBody {
    token: String,
}

async fn verify_page(Query(query): Query<TokenQuery>) -> Response {
    page(
        StatusCode::OK,
        "Verify your email address",
        &format!(
            r#"<form method="post" action="/v1/auth/verify"><input type="hidden" name="token" value="{}"><button type="submit">Verify my email address</button></form>"#,
            escape(&query.token)
        ),
    )
}

async fn verify(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    body: Submitted<TokenBody>,
) -> Result<Response, AuthError> {
    api.limits.check(Action::OpenLink, ip, None).await?;
    match body {
        Submitted::Json(body) => {
            api.auth.verify_email(&body.token).await?;
            Ok(Json(json!({"status": "verified"})).into_response())
        }
        Submitted::Form(body) => Ok(match api.auth.verify_email(&body.token).await {
            Ok(()) => page(
                StatusCode::OK,
                "Email address verified",
                "<p>Your email address is verified. You can sign in now.</p>",
            ),
            Err(error) => error_page(&error),
        }),
    }
}

#[derive(Deserialize)]
struct EmailBody {
    email: String,
}

async fn resend(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    JsonBody(body): JsonBody<EmailBody>,
) -> Result<Response, AuthError> {
    api.limits
        .check(Action::ResendVerification, ip, Some(&body.email))
        .await?;
    api.auth.resend_verification(&body.email).await?;
    Ok(check_email())
}

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
    #[serde(default)]
    device_id: Option<Uuid>,
}

async fn login(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    JsonBody(body): JsonBody<LoginBody>,
) -> Result<Response, AuthError> {
    api.limits
        .check(Action::Login, ip, Some(&body.email))
        .await?;
    Ok(match body.device_id {
        None => {
            let registration = api.auth.login(&body.email, &body.password).await?;
            Json(json!({
                "registration_token": registration.token,
                "expires_in_s": registration.expires_in_s,
            }))
            .into_response()
        }
        Some(device_id) => {
            let tokens = api
                .auth
                .login_device(&body.email, &body.password, device_id)
                .await?;
            Json(tokens_json(&tokens)).into_response()
        }
    })
}

#[derive(Deserialize)]
struct RefreshBody {
    refresh_token: String,
}

async fn refresh(
    State(api): State<Api>,
    JsonBody(body): JsonBody<RefreshBody>,
) -> Result<Response, AuthError> {
    let tokens = api.auth.refresh(&body.refresh_token).await?;
    Ok(Json(tokens_json(&tokens)).into_response())
}

async fn forgot(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    JsonBody(body): JsonBody<EmailBody>,
) -> Result<Response, AuthError> {
    api.limits
        .check(Action::ForgotPassword, ip, Some(&body.email))
        .await?;
    api.auth.forgot_password(&body.email).await?;
    Ok(check_email())
}

async fn reset_page(Query(query): Query<TokenQuery>) -> Response {
    page(
        StatusCode::OK,
        "Choose a new password",
        &format!(
            r#"<form method="post" action="/v1/auth/password/reset"><input type="hidden" name="token" value="{}"><label>New password <input type="password" name="password" minlength="8" maxlength="128" autocomplete="new-password" required></label> <button type="submit">Set password</button></form>"#,
            escape(&query.token)
        ),
    )
}

#[derive(Deserialize)]
struct ResetBody {
    token: String,
    password: String,
}

async fn reset(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    body: Submitted<ResetBody>,
) -> Result<Response, AuthError> {
    api.limits.check(Action::OpenLink, ip, None).await?;
    match body {
        Submitted::Json(body) => {
            api.auth.reset_password(&body.token, &body.password).await?;
            Ok(Json(json!({"status": "password_changed"})).into_response())
        }
        Submitted::Form(body) => Ok(
            match api.auth.reset_password(&body.token, &body.password).await {
                Ok(()) => page(
                    StatusCode::OK,
                    "Password changed",
                    "<p>Your password is changed and every device is signed out. Sign in again with the new password.</p>",
                ),
                Err(error) => error_page(&error),
            },
        ),
    }
}

async fn jwks(State(api): State<Api>) -> Response {
    let mut response = Json(api.auth.tokens().jwks()).into_response();
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    response
}

#[derive(Deserialize)]
struct DeviceBody {
    name: String,
    platform: String,
}

async fn register_device(
    State(api): State<Api>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<DeviceBody>,
) -> Result<Response, AuthError> {
    let tokens = api
        .auth
        .register_device(bearer(&headers)?, &body.name, &body.platform)
        .await?;
    Ok((StatusCode::CREATED, Json(tokens_json(&tokens))).into_response())
}

async fn list_devices(State(api): State<Api>, headers: HeaderMap) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let devices: Vec<Value> = api
        .auth
        .list_devices(caller)
        .await?
        .into_iter()
        .map(|device| {
            json!({
                "device_id": device.device_id,
                "name": device.name,
                "platform": device.platform.as_str(),
                "created_at": device.created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
                "last_seen_at": device
                    .last_seen_at
                    .map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true)),
                "current": device.device_id == caller.device_id,
            })
        })
        .collect();
    Ok(Json(json!({"devices": devices})).into_response())
}

async fn revoke_device(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let device_id: Uuid = device_id.parse().map_err(|_| AuthError::DeviceNotFound)?;
    api.auth.revoke_device(caller, device_id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn own_profile_json(profile: &crate::profiles::OwnProfile) -> Value {
    json!({
        "user_id": profile.user_id,
        "handle": profile.handle,
        "display_name": profile.display_name,
        "email": profile.email,
    })
}

async fn own_profile(State(api): State<Api>, headers: HeaderMap) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let profile = api.auth.own_profile(caller).await?;
    Ok(Json(own_profile_json(&profile)).into_response())
}

#[derive(Deserialize)]
struct DisplayNameBody {
    display_name: String,
}

async fn set_display_name(
    State(api): State<Api>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<DisplayNameBody>,
) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let profile = api
        .auth
        .set_display_name(caller, &body.display_name)
        .await?;
    Ok(Json(own_profile_json(&profile)).into_response())
}

#[derive(Deserialize)]
struct HandleQuery {
    #[serde(default)]
    handle: String,
}

async fn lookup(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Query(query): Query<HandleQuery>,
) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let user = caller.user_id.to_string();
    api.limits
        .check(Action::HandleLookup, ip, Some(&user))
        .await?;
    let profile = api.auth.lookup(&query.handle).await?;
    Ok(Json(json!({
        "user_id": profile.user_id,
        "handle": profile.handle,
        "display_name": profile.display_name,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct MailboxQuery {
    #[serde(default)]
    to: String,
}

/// DEV ONLY: the emails the outbox holds for one address, newest first.
async fn dev_mailbox(
    State(api): State<Api>,
    Query(query): Query<MailboxQuery>,
) -> Result<Response, AuthError> {
    let emails = api.auth.dev_mailbox(&query.to).await?;
    Ok(Json(json!({"emails": emails})).into_response())
}

/// A small HTML page with the headers every link page needs.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{title}</title></head><body><main><h1>{title}</h1>{body}</main></body></html>"#,
        title = escape(title),
    );
    let mut response = (status, Html(html)).into_response();
    let headers = response.headers_mut();
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; form-action 'self'; frame-ancestors 'none'"),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

fn error_page(error: &AuthError) -> Response {
    let status = StatusCode::from_u16(error.status()).expect("a valid status");
    let message = match error {
        AuthError::LinkExpired => "This link has expired. Ask for a new one.".to_owned(),
        AuthError::LinkInvalid => "This link is not valid, or it was used already.".to_owned(),
        other => other.to_string(),
    };
    page(
        status,
        "Something went wrong",
        &format!("<p>{}</p>", escape(&message)),
    )
}

/// Escapes text for HTML content and attribute values.
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
    use super::*;

    #[test]
    fn escaping_covers_markup_and_quotes() {
        assert_eq!(
            escape(r#"<a href="x">'&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn errors_answer_with_their_status_code_and_headers() {
        let response = AuthError::RateLimited { retry_after_s: 42 }.into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[RETRY_AFTER], "42");
        let response = AuthError::Unauthorized.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
    }
}
```

Replace `crates/push-auth/src/lib.rs` with:

```rust
//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.

pub mod api;
mod db;
mod devices;
mod error;
pub mod limits;
pub mod links;
pub mod outbox;
pub mod password;
mod profiles;
pub mod rules;
mod service;
pub mod tokens;

pub use db::{MIGRATOR, migrate_database};
pub use devices::{Caller, DeviceInfo, MAX_ACTIVE_DEVICES, Tokens};
pub use error::AuthError;
pub use profiles::{OwnProfile, PublicProfile};
pub use service::{
    Auth, LINK_SECONDS, REGISTRATION_TOKEN_SECONDS, RegistrationToken, Settings, Signup,
};
```

- [ ] **Step 5: Run the tests**

```powershell
cargo test -p push-auth --lib --test api
```

Expected: `test result: ok. 30 passed` (lib) and `test result: ok. 6 passed` (api).

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.toml Cargo.lock crates/push-auth
git commit -m "feat(auth): the HTTP API and the link pages"
```

Expected: 676 tests pass.

---

### Task 8: Configuration, serving, health and graceful shutdown

**Files:**
- Modify: `crates/push-auth/Cargo.toml`, `crates/push-auth/src/lib.rs`
- Create: `crates/push-auth/src/config.rs`, `src/server.rs`, `src/main.rs`,
  `tests/server.rs`

- [ ] **Step 1: Dependencies**

Replace `crates/push-auth/Cargo.toml` with:

```toml
[package]
name = "push-auth"
description = "The push-messenger auth service: accounts, email links, devices and tokens."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
anyhow.workspace = true
argon2.workspace = true
async-trait.workspace = true
axum.workspace = true
base64.workspace = true
chrono.workspace = true
hmac.workspace = true
jsonwebtoken.workspace = true
push-store.workspace = true
rand.workspace = true
redis.workspace = true
ring.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
sqlx.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "time"] }
tokio-util.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
uuid.workspace = true

[dev-dependencies]
http-body-util.workspace = true
testcontainers-modules.workspace = true
tokio = { workspace = true, features = ["io-util"] }
tower.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Write the failing service tests**

Create `crates/push-auth/tests/server.rs`:

```rust
//! The service as a whole: start-up, readiness, the API on a real socket, and shutdown
//! (Docker must be running).

use std::net::SocketAddr;
use std::time::Duration;

use push_auth::{Config, serve};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// base64 of 32 bytes of 0x07 and of 0x09.
const SIGNING_KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";
const LINK_KEY: &str = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=";

fn config(database_url: &str, redis_url: &str) -> Config {
    let variables = [
        ("PUSH_DATABASE_URL", database_url),
        ("PUSH_REDIS_URL", redis_url),
        ("PUSH_AUTH_PUBLIC_URL", "http://push.local"),
        ("PUSH_AUTH_TOKEN_AUDIENCE", "ws://push.local/v1/socket"),
        ("PUSH_AUTH_SIGNING_KEY", SIGNING_KEY),
        ("PUSH_AUTH_LINK_KEY", LINK_KEY),
    ];
    Config::from_lookup(|name| {
        variables
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    })
    .unwrap()
}

/// `GET path` over a plain socket: the status code and the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return (0, String::new());
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return (0, String::new());
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).await.is_err() {
        return (0, String::new());
    }
    let status = response
        .get(9..12)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
}

async fn listeners() -> (TcpListener, TcpListener, SocketAddr, SocketAddr) {
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (http_addr, health_addr) = (http.local_addr().unwrap(), health.local_addr().unwrap());
    (http, health, http_addr, health_addr)
}

#[tokio::test]
async fn it_becomes_ready_serves_the_jwks_and_stops_cleanly() {
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
    let (http, health, http_addr, health_addr) = listeners().await;
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        config(&database_url, &redis_url),
        http,
        health,
        shutdown.clone(),
    ));

    let mut ready = false;
    for _ in 0..240 {
        assert!(
            !server.is_finished(),
            "the service stopped before it became ready"
        );
        if get(health_addr, "/readyz").await.0 == 200 {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(ready, "not ready within 60 s");
    assert_eq!(get(health_addr, "/healthz").await, (200, "ok".to_owned()));
    let (status, body) = get(http_addr, "/v1/auth/jwks").await;
    assert_eq!(status, 200);
    assert!(body.contains(r#""crv":"Ed25519""#), "{body}");
    assert_eq!(get(http_addr, "/dev/mailbox?to=a@example.com").await.0, 404);

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("stops within 10 s")
        .unwrap()
        .expect("stops cleanly");
}

#[tokio::test]
async fn without_a_database_it_is_not_ready_and_still_stops_when_asked() {
    // Nothing listens on this port.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let (http, health, _http_addr, health_addr) = listeners().await;
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        config(
            &format!("postgres://push_auth:secret@127.0.0.1:{port}/push_auth"),
            "redis://127.0.0.1:1",
        ),
        http,
        health,
        shutdown.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        get(health_addr, "/readyz").await,
        (503, "starting".to_owned())
    );
    assert_eq!(get(health_addr, "/healthz").await.0, 200);
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the start-up retries stop on shutdown")
        .unwrap()
        .expect("stops cleanly");
}
```

```powershell
cargo test -p push-auth --test server
```

Expected: a compile error — `unresolved imports push_auth::Config, push_auth::serve`.

- [ ] **Step 3: Configuration, with its tests**

Create `crates/push-auth/src/config.rs`:

```rust
//! Configuration from environment variables.

use std::net::SocketAddr;

use axum::http::HeaderName;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use push_store::{StoreOptions, check_database_url};

/// How the service runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_DATABASE_URL` | Postgres URL of the `push_auth` database, as its owner | required |
/// | `PUSH_REDIS_URL` | Redis URL for rate limits, `redis://:password@host:6379` | required |
/// | `PUSH_AUTH_PUBLIC_URL` | base URL of email links, `https://api.example.com` | required |
/// | `PUSH_AUTH_TOKEN_AUDIENCE` | the gateway's URL, the access tokens' `aud` | required |
/// | `PUSH_AUTH_SIGNING_KEY` | base64 of the 32-byte Ed25519 seed that signs access tokens | required |
/// | `PUSH_AUTH_LINK_KEY` | base64 of at least 32 bytes, the HMAC key of links and registration tokens | required |
/// | `PUSH_AUTH_CLIENT_IP_HEADER` | header with the client IP, set by the ingress (`x-real-ip`) | the peer address |
/// | `PUSH_AUTH_HASH_CONCURRENCY` | most password hashes at once (19 MiB each) | `2` |
/// | `PUSH_HTTP_ADDR` | address of the API | `0.0.0.0:8080` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_DEV_MAILBOX` | `1` serves the DEV ONLY `GET /dev/mailbox` | off |
///
/// `http://` public URLs and `ws://` audiences are accepted only for local hosts
/// (`localhost`, `127.0.0.1`, `*.local`, `*.localhost`, `*.test`).
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub database_url: String,
    pub redis_url: String,
    pub public_url: String,
    pub token_audience: String,
    pub signing_key: Vec<u8>,
    pub link_key: Vec<u8>,
    pub client_ip_header: Option<HeaderName>,
    pub hash_concurrency: usize,
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
            .field("redis_url", &"<redacted>")
            .field("public_url", &self.public_url)
            .field("token_audience", &self.token_audience)
            .field("signing_key", &"<redacted>")
            .field("link_key", &"<redacted>")
            .field("client_ip_header", &self.client_ip_header)
            .field("hash_concurrency", &self.hash_concurrency)
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
        store.validate().map_err(|reason| ConfigError::Invalid {
            name: "PUSH_DB_MAX_CONNECTIONS",
            value: store.max_connections.to_string(),
            reason,
        })?;
        let database_url = required("PUSH_DATABASE_URL")?;
        check_database_url(&database_url)
            .map_err(|reason| redacted("PUSH_DATABASE_URL", reason))?;
        let redis_url = required("PUSH_REDIS_URL")?;
        redis::Client::open(redis_url.as_str()).map_err(|_| {
            redacted(
                "PUSH_REDIS_URL",
                "not a valid Redis URL, expected redis://[:password@]host:6379".to_owned(),
            )
        })?;
        let public_url = required("PUSH_AUTH_PUBLIC_URL")?;
        check_url(&public_url, "https://", "http://").map_err(|reason| ConfigError::Invalid {
            name: "PUSH_AUTH_PUBLIC_URL",
            value: public_url.clone(),
            reason,
        })?;
        let token_audience = required("PUSH_AUTH_TOKEN_AUDIENCE")?;
        check_url(&token_audience, "wss://", "ws://").map_err(|reason| ConfigError::Invalid {
            name: "PUSH_AUTH_TOKEN_AUDIENCE",
            value: token_audience.clone(),
            reason,
        })?;
        let signing_key = key(&required("PUSH_AUTH_SIGNING_KEY")?, "PUSH_AUTH_SIGNING_KEY")?;
        if signing_key.len() != 32 {
            return Err(redacted(
                "PUSH_AUTH_SIGNING_KEY",
                "must be base64 of a 32-byte Ed25519 seed".to_owned(),
            ));
        }
        let link_key = key(&required("PUSH_AUTH_LINK_KEY")?, "PUSH_AUTH_LINK_KEY")?;
        if link_key.len() < 32 {
            return Err(redacted(
                "PUSH_AUTH_LINK_KEY",
                "must be base64 of at least 32 bytes".to_owned(),
            ));
        }
        let client_ip_header =
            match lookup("PUSH_AUTH_CLIENT_IP_HEADER").filter(|v| !v.is_empty()) {
                None => None,
                Some(value) => Some(HeaderName::try_from(value.as_str()).map_err(|_| {
                    ConfigError::Invalid {
                        name: "PUSH_AUTH_CLIENT_IP_HEADER",
                        value: value.clone(),
                        reason: "not a valid header name".to_owned(),
                    }
                })?),
            };
        let hash_concurrency: usize = parsed(&lookup, "PUSH_AUTH_HASH_CONCURRENCY", 2)?;
        if hash_concurrency == 0 {
            return Err(ConfigError::Invalid {
                name: "PUSH_AUTH_HASH_CONCURRENCY",
                value: "0".to_owned(),
                reason: "must be at least 1".to_owned(),
            });
        }
        Ok(Self {
            database_url,
            redis_url,
            public_url,
            token_audience,
            signing_key,
            link_key,
            client_ip_header,
            hash_concurrency,
            http_addr: parsed(&lookup, "PUSH_HTTP_ADDR", ([0, 0, 0, 0], 8080).into())?,
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            store,
            dev_mailbox: dev_flag(&lookup, "PUSH_DEV_MAILBOX")?,
        })
    }
}

/// A key variable, base64-decoded; the value is never shown.
fn key(value: &str, name: &'static str) -> Result<Vec<u8>, ConfigError> {
    STANDARD
        .decode(value.trim())
        .map_err(|_| redacted(name, "not valid base64".to_owned()))
}

/// Checks a public URL: `secure` scheme, or `local` only for a local host.
fn check_url(url: &str, secure: &str, local: &str) -> Result<(), String> {
    let rest = if let Some(rest) = url.strip_prefix(secure) {
        rest
    } else if let Some(rest) = url.strip_prefix(local) {
        if !is_local_host(host_of(rest)) {
            return Err(format!(
                "{local} is only for local hosts (localhost, *.local, *.localhost, *.test); use {secure}"
            ));
        }
        rest
    } else {
        return Err(format!("must start with {secure}"));
    };
    let host = host_of(rest);
    if host.is_empty() || url.contains(['?', '#', ' ']) {
        return Err("must be a plain URL with a host, without query or fragment".to_owned());
    }
    Ok(())
}

/// The host of `rest`, the URL after its scheme: up to the port or path.
fn host_of(rest: &str) -> &str {
    let authority = rest.split('/').next().unwrap_or_default();
    match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => authority,
    }
}

fn is_local_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
        || [".local", ".localhost", ".test"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
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

    /// base64 of 32 bytes of 0x07 and of 0x09.
    const SIGNING_KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";
    const LINK_KEY: &str = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=";

    const REQUIRED: [(&str, &str); 6] = [
        (
            "PUSH_DATABASE_URL",
            "postgres://push_auth:hunter2@db/push_auth",
        ),
        ("PUSH_REDIS_URL", "redis://:hunter2@redis:6379"),
        ("PUSH_AUTH_PUBLIC_URL", "https://api.example.com"),
        (
            "PUSH_AUTH_TOKEN_AUDIENCE",
            "wss://api.example.com/v1/socket",
        ),
        ("PUSH_AUTH_SIGNING_KEY", SIGNING_KEY),
        ("PUSH_AUTH_LINK_KEY", LINK_KEY),
    ];

    fn with(name: &'static str, value: &'static str) -> Vec<(&'static str, &'static str)> {
        let mut variables = REQUIRED.to_vec();
        variables.retain(|(key, _)| *key != name);
        variables.push((name, value));
        variables
    }

    #[test]
    fn defaults_apply_when_only_the_required_variables_are_set() {
        let config = config(&REQUIRED).unwrap();
        assert_eq!(config.signing_key, [7; 32]);
        assert_eq!(config.link_key, [9; 32]);
        assert_eq!(config.http_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.hash_concurrency, 2);
        assert_eq!(config.client_ip_header, None);
        assert_eq!(config.store, StoreOptions::default());
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
    fn optional_settings_can_be_set() {
        let mut variables = REQUIRED.to_vec();
        variables.extend([
            ("PUSH_AUTH_CLIENT_IP_HEADER", "X-Real-IP"),
            ("PUSH_AUTH_HASH_CONCURRENCY", "4"),
            ("PUSH_HTTP_ADDR", "127.0.0.1:7000"),
            ("PUSH_DB_MAX_CONNECTIONS", "3"),
            ("PUSH_DEV_MAILBOX", "1"),
        ]);
        let config = config(&variables).unwrap();
        assert_eq!(
            config.client_ip_header,
            Some(HeaderName::from_static("x-real-ip"))
        );
        assert_eq!(config.hash_concurrency, 4);
        assert_eq!(config.http_addr, "127.0.0.1:7000".parse().unwrap());
        assert_eq!(config.store.max_connections, 3);
        assert!(config.dev_mailbox);
    }

    #[test]
    fn plain_http_and_ws_are_for_local_hosts_only() {
        let mut variables = with("PUSH_AUTH_PUBLIC_URL", "http://push.local");
        variables.retain(|(name, _)| *name != "PUSH_AUTH_TOKEN_AUDIENCE");
        variables.push(("PUSH_AUTH_TOKEN_AUDIENCE", "ws://push.local:8080/v1/socket"));
        let local = config(&variables);
        assert!(local.is_ok(), "{local:?}");
        assert!(config(&with("PUSH_AUTH_PUBLIC_URL", "http://localhost:8080")).is_ok());
        for (name, value) in [
            ("PUSH_AUTH_PUBLIC_URL", "http://api.example.com"),
            ("PUSH_AUTH_PUBLIC_URL", "ftp://api.example.com"),
            ("PUSH_AUTH_PUBLIC_URL", "https://api.example.com/?x=1"),
            ("PUSH_AUTH_PUBLIC_URL", "https://"),
            ("PUSH_AUTH_TOKEN_AUDIENCE", "ws://api.example.com/v1/socket"),
            (
                "PUSH_AUTH_TOKEN_AUDIENCE",
                "https://api.example.com/v1/socket",
            ),
        ] {
            assert!(
                matches!(config(&with(name, value)), Err(ConfigError::Invalid { name: n, .. }) if n == name),
                "{name}={value}"
            );
        }
    }

    #[test]
    fn bad_keys_fail_start_up_without_showing_them() {
        for (name, value) in [
            ("PUSH_AUTH_SIGNING_KEY", "not base64!"),
            ("PUSH_AUTH_SIGNING_KEY", "AAAA"),
            (
                "PUSH_AUTH_LINK_KEY",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            ),
        ] {
            let message = config(&with(name, value)).unwrap_err().to_string();
            assert!(message.contains(name), "{message}");
            assert!(!message.contains(value), "{message}");
        }
    }

    #[test]
    fn dev_mailbox_is_on_only_for_exactly_1() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            assert_eq!(
                config(&with("PUSH_DEV_MAILBOX", value))
                    .unwrap()
                    .dev_mailbox,
                expected
            );
        }
        for value in ["true", "yes", " 1"] {
            assert!(
                config(&with("PUSH_DEV_MAILBOX", value)).is_err(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn urls_and_keys_never_show_in_errors_or_debug_output() {
        let message = config(&with("PUSH_DATABASE_URL", "mysql://u:hunter2@db/x"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("PUSH_DATABASE_URL") && !message.contains("hunter2"));
        let message = config(&with("PUSH_REDIS_URL", "http://:hunter2@redis:6379"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("PUSH_REDIS_URL") && !message.contains("hunter2"));
        let shown = format!("{:?}", config(&REQUIRED).unwrap());
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("[7, 7"), "{shown}");
        assert!(shown.contains("https://api.example.com"), "{shown}");
    }

    #[test]
    fn zero_hash_concurrency_is_refused() {
        assert!(config(&with("PUSH_AUTH_HASH_CONCURRENCY", "0")).is_err());
    }
}
```

- [ ] **Step 4: Serving, health, logging and signals**

Create `crates/push-auth/src/server.rs`:

```rust
//! Running the service: the HTTP API, the health endpoints and graceful shutdown.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::api::{Api, router};
use crate::config::Config;
use crate::db::migrate_database;
use crate::limits::{Limits, RedisCounter};
use crate::links::LinkSigner;
use crate::password::Passwords;
use crate::service::{Auth, Settings};
use crate::tokens::AccessTokens;

/// How long to wait before trying an unreachable dependency again.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Runs the service until `shutdown` is cancelled, then finishes the requests in
/// progress and closes the database pool.
///
/// The health endpoints answer at once: `/healthz` always, `/readyz` only once the
/// database is migrated and answers a ping, and never after shutdown began. Postgres and
/// Redis are retried until they answer, so the service may start before them.
pub async fn serve(
    config: Config,
    http: TcpListener,
    health: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let tokens = AccessTokens::from_seed(&config.signing_key, config.token_audience.clone())
        .map_err(anyhow::Error::msg)?;
    let links = LinkSigner::new(config.link_key.clone()).map_err(anyhow::Error::msg)?;
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
            "DEV ONLY: PUSH_DEV_MAILBOX=1 serves GET /dev/mailbox, which shows anyone the \
             email links waiting in the outbox, and with them every account. Never enable it \
             outside a local cluster."
        );
    }
    let Some(pool) = retry("postgres", &shutdown, || async {
        // Migrations run on a dedicated connection, not on the request pool, whose
        // statement and lock timeouts are sized for requests.
        migrate_database(&config.database_url).await?;
        let pool = push_store::connect_pool(&config.database_url, &config.store).await?;
        anyhow::Ok(pool)
    })
    .await
    else {
        return finish(health_server).await;
    };
    let Some(counter) = retry("redis", &shutdown, || {
        RedisCounter::connect(&config.redis_url, shutdown.clone())
    })
    .await
    else {
        pool.close().await;
        return finish(health_server).await;
    };
    let _ = ready.pool.set(pool.clone());

    let auth = Auth::new(
        pool.clone(),
        Settings {
            public_url: config.public_url.clone(),
            passwords: Passwords::owasp(),
            links,
            tokens,
            hash_concurrency: config.hash_concurrency,
        },
    );
    let api = router(Api {
        auth,
        limits: Limits::new(Arc::new(counter)),
        client_ip_header: config.client_ip_header.clone(),
        dev_mailbox: config.dev_mailbox,
    });
    tracing::info!(addr = %config.http_addr, "serving HTTP");
    let result = axum::serve(
        http,
        api.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned())
    .await;
    pool.close().await;
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

/// Ready once the database is migrated and answers. Redis is not checked: without it the
/// rate limits let requests through (see `limits`).
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

Create `crates/push-auth/src/main.rs`:

```rust
use push_auth::{Config, init_logging, serve, terminated};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

// `serve` runs the migrations itself, with `push_auth::migrate_database(url)` on a
// dedicated connection at start-up (not on the request pool).
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

Replace `crates/push-auth/src/lib.rs` with:

```rust
//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.

pub mod api;
mod config;
mod db;
mod devices;
mod error;
pub mod limits;
pub mod links;
pub mod outbox;
pub mod password;
mod profiles;
pub mod rules;
mod server;
mod service;
pub mod tokens;

pub use config::{Config, ConfigError};
pub use db::{MIGRATOR, migrate_database};
pub use devices::{Caller, DeviceInfo, MAX_ACTIVE_DEVICES, Tokens};
pub use error::AuthError;
pub use profiles::{OwnProfile, PublicProfile};
pub use server::{init_logging, serve, terminated};
pub use service::{
    Auth, LINK_SECONDS, REGISTRATION_TOKEN_SECONDS, RegistrationToken, Settings, Signup,
};
```

- [ ] **Step 5: Run the tests**

```powershell
cargo test -p push-auth
```

Expected: lib `38 passed`, `accounts` 10, `api` 6, `devices` 8, `limits` 3, `schema` 3,
`server` 2. `without_a_database_it_is_not_ready_and_still_stops_when_asked` shows that the
start-up retries stop on shutdown.

- [ ] **Step 6: Run it by hand**

```powershell
cargo run -p push-auth
```

Expected: exit code 1 with `Error: PUSH_DATABASE_URL must be set` — configuration errors
stop it before it listens.

- [ ] **Step 7: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock crates/push-auth
git commit -m "feat(auth): configuration, serving, health and graceful shutdown"
```

Expected: 686 tests pass.

---

### Task 9: Spec, README, roadmap and pull request

**Files:**
- Modify: `README.md`, `docs/superpowers/specs/2026-10-11-push-messenger-design.md`,
  `docs/superpowers/plans/2026-10-11-roadmap.md`

CI needs no change: the `rust` and `msrv` jobs run the whole workspace, and already pull
`postgres:17-alpine` and `redis:7-alpine`.

- [ ] **Step 1: Document the configuration**

In `README.md`, after the `push-gateway` table (its last row is
`` | `PUSH_DEV_AUTH` | `1` accepts DEV ONLY identities without a token | off | ``), add a
blank line and:

```markdown
`push-auth`:

| Variable | Meaning | Default |
|----------|---------|---------|
| `PUSH_DATABASE_URL` | Postgres URL of the `push_auth` database, as its owner | required |
| `PUSH_REDIS_URL` | Redis URL for rate limits, `redis://:password@host:6379` | required |
| `PUSH_AUTH_PUBLIC_URL` | base URL of email links, `https://api.example.com` (`http` only for local hosts) | required |
| `PUSH_AUTH_TOKEN_AUDIENCE` | the gateway's URL, the access tokens' `aud` (`wss://…/v1/socket`) | required |
| `PUSH_AUTH_SIGNING_KEY` | base64 of the 32-byte Ed25519 seed that signs access tokens | required |
| `PUSH_AUTH_LINK_KEY` | base64 of at least 32 bytes, the HMAC key of email links and registration tokens | required |
| `PUSH_AUTH_CLIENT_IP_HEADER` | header with the client's IP, set by the ingress (`x-real-ip`) | the peer address |
| `PUSH_AUTH_HASH_CONCURRENCY` | most password hashes at once (19 MiB each) | `2` |
| `PUSH_HTTP_ADDR` | address of the API | `0.0.0.0:8080` |
| `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
| `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
| `PUSH_DEV_MAILBOX` | `1` serves the DEV ONLY `GET /dev/mailbox` | off |
```

- [ ] **Step 2: Record the implementation in the spec**

In §3.1, the `outbox` block lists the event kinds in a comment. Replace its two comment
lines

```text
  kind       text not null,                 -- email.send | user.verified |
                                            -- device.registered | device.revoked
```

with

```text
  kind       text not null,                 -- email.send | user.verified |
                                            -- user.password_reset |
                                            -- device.registered | device.revoked
```

Then, after the closing fence of that block (before `### 3.2 messaging schema`), add a
blank line and:

```markdown
Implementation notes (push-auth, migration `0001_auth.sql`, Plan 5A):

- auth-svc has its own database, `push_auth`, owned by its own role. The migration creates
  the `citext` extension (trusted, so the database owner may) and the `auth` schema.
- Handles are stored in lower case: 3–30 letters, digits and underscores, starting with a
  letter (a `CHECK`). Display and device names have 1–64 characters; `platform` is checked
  against the five values. A partial index on `device (user_id) where revoked_at is null`
  serves the device limit.
- Comparisons with a bound email or handle cast the parameter to `citext`
  (`email = $1::citext`): sqlx binds `text`, and `citext = text` compares as text, which
  is case-sensitive.
- Outbox payloads are JSON: `email.send {user_id, to, template, params}` (templates
  `verify_email {link, handle, expires_in_minutes}`, `account_exists {handle}`,
  `reset_password {link, expires_in_minutes}`); `user.verified {user_id}`;
  `user.password_reset {user_id}`, on which every device of the user is kicked (§5.4);
  `device.registered` and `device.revoked {user_id, device_id}`. New ids are UUIDv7.
```

In §5.6, after the last bullet (`- Links are built from a configured base URL, never from
the `Host` header. Pages they open send `Referrer-Policy: no-referrer`.`), add a blank line
and:

```markdown
Implementation notes (push-auth, Plan 5A):

- **Endpoints.** `POST /v1/auth/signup` (202), `GET|POST /v1/auth/verify`,
  `POST /v1/auth/verify/resend` (202), `POST /v1/auth/login`, `POST /v1/auth/refresh`,
  `POST /v1/auth/password/forgot` (202), `GET|POST /v1/auth/password/reset`,
  `GET /v1/auth/jwks`, `POST|GET /v1/devices`, `DELETE /v1/devices/{id}`,
  `GET|PATCH /v1/users/me` and `GET /v1/users/lookup?handle=…`. Errors are
  `{"error": {"code", "message", "retryable"}}`; 429 carries `Retry-After`. No answer may
  be cached except the JWKS.
- **Link pages.** A link opens a small page whose button posts the token: a `GET` changes
  nothing, so mail scanners that open links do nothing. Pages send
  `Referrer-Policy: no-referrer`, `Cache-Control: no-store` and a CSP of
  `default-src 'none'`. API clients post the token as JSON instead.
- **Signed tokens.** Email links and the registration token are
  `base64url(JSON claims).base64url(HMAC-SHA256)` with `{sub, purpose, exp}` plus `email`
  (verify) or `pw` (reset: 16 bytes of SHA-256 of the password hash). The purposes
  `verify`, `reset` and `register` are checked, so no token stands in for another; the
  gateway does not have the HMAC key, so a registration token can never open a socket.
- **Access tokens** are EdDSA (Ed25519) JWTs with a `kid` (the RFC 7638 thumbprint) and
  the claims of §5.1; `GET /v1/auth/jwks` publishes the key. The signing seed and the HMAC
  key come from the Secret.
- **Refresh tokens** are 32 random bytes, base64url, stored as SHA-256. Refreshing marks
  the old row `rotated_to`; presenting a rotated token again revokes every refresh token of
  the device. Signing in again with `device_id` revokes the device's earlier refresh
  tokens. A client must not refresh twice concurrently with one token.
- **Device limit**: 10 active devices, checked under a row lock on the account, so
  concurrent registrations cannot pass it together.
- **Timing.** Signup hashes the password before looking at the email; login checks a
  dummy hash when the email is unknown. Password hashing runs on blocking threads, at most
  `PUSH_AUTH_HASH_CONCURRENCY` (default 2) at once, 19 MiB each.
- **Rate limits** (fixed windows in Redis, keyed by a SHA-256 of the subject):

  | Action | Per IP | Per account |
  |---|---|---|
  | signup | 20 / hour | 5 / hour (email) |
  | login | 60 / 15 min | 10 / 15 min (email) |
  | verification resend | 20 / hour | 3 / hour (email) |
  | password forgot | 20 / hour | 3 / hour (email) |
  | opening a link (verify, reset) | 60 / hour | — |
  | handle lookup | 120 / hour | 20 / hour (user) |

  The client IP comes from `X-Real-IP`, which ingress-nginx overwrites. When Redis does
  not answer within 500 ms the request goes ahead and a warning is logged.
- **Profiles.** `GET /v1/users/lookup` finds verified users only. `GET /v1/users?ids=…`
  needs to know who shares a contact or conversation with the caller, which only
  message-svc knows; it comes with Plan 7.
- **DEV ONLY mailbox.** auth-svc started with `PUSH_DEV_MAILBOX=1` serves
  `GET /dev/mailbox?to=…`, the `email.send` payloads in the outbox for one address, so a
  local cluster and the smoke test can open verification links before the worker sends
  email (Plan 6). Like the other DEV ONLY switches it accepts only `1` or `0` and logs a
  warning at start-up.
```

- [ ] **Step 3: Mark plan 5A in review**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the empty status cell at the
end of the row for plan 5A (`| 5A | Auth service: the service | … | 4B | |`) with
`In review`, so the row ends `| 4B | In review |`.

- [ ] **Step 4: Final gates**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 test --workspace --locked
```

Expected: no diff, no warnings, 686 tests on both toolchains.

- [ ] **Step 5: Commit and push**

```powershell
git add README.md docs
git commit -m "docs: record plan 5A in the spec, README and roadmap"
git push -u origin feature/plan-05a-auth-service
```

- [ ] **Step 6: Open the pull request and wait for CI**

```powershell
$body = @'
Implements docs/superpowers/plans/2026-10-13-plan-05a-auth-service.md: the push-auth crate (library and binary) with the auth schema in its own database; signup with uniform answers; stateless HMAC-signed links for email verification and password reset; verification as the gate; login with registration tokens; explicit device registration (10 active at most, race-free); EdDSA access tokens with a JWKS; refresh rotation with reuse detection; device listing and revocation; handles, own profile and lookup; Argon2id at the OWASP minimum; Redis rate limits; the outbox (email.send, user.verified, user.password_reset, device.registered, device.revoked); the DEV ONLY mailbox behind PUSH_DEV_MAILBOX=1. push-store gains run_migrations and connect_pool. 686 tests.

Plan 5B adds auth-svc to the image and the chart and verifies it on minikube.
'@
gh pr create --base main --title "Plan 5A: auth service" --body $body
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust`, `msrv`, `images`, `chart`). Do not merge;
the controller merges after the final review.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §2 auth-svc owns the `auth` schema in its own database; outbox written in the same transaction | 1, 4, 5 |
| §3.1 `user_account`, `device`, `refresh_token`, `outbox` (citext email and handle, Argon2id PHC, hashed refresh tokens, `rotated_to`) | 1 |
| §5.1 registration token 5 min (signed, unstored); access token EdDSA 15 min with `sub, device_id, email_verified, exp, iat, jti, aud`; refresh 30 days, hashed and rotated; email links 30 min, unstored | 3, 4, 5 |
| §5.2 uniform signup; `email.send` in the same transaction; link binds `{user_id, email, purpose, exp}`; opening sets `email_verified_at` and writes `user.verified`, never signs in; unverified accounts can only resend | 4, 7 |
| §5.3 login → registration token, or tokens for an existing active device; `POST /v1/devices` with the 10-device limit and `device.registered`; refresh the only way to new access tokens; list and revoke with `device.revoked` | 5, 7 |
| §5.4 reset link `{user_id, purpose: reset, exp, pw}`, single use without state; all refresh tokens revoked, devices kicked (`user.password_reset`) | 4, 5, 7 |
| §5.5 unique public handles, lookup without email, 20 per hour per user | 5, 6, 7 |
| §5.6 Argon2id `m=19456, t=2, p=1` as PHC, rehash on login; rate limits per account and per IP on signup, login, resend, reset, lookup; never locked; links from a configured base URL; pages with `Referrer-Policy: no-referrer` | 2, 6, 7, 8 |
| §7.6 strict DEV ONLY switches with a start-up warning; URLs never echoed; migrations on a dedicated connection; start-up retries interruptible | 8 |
| §9 any service sends email by writing `email.send {user_id or address, template, params}` | 4 |
| §12 integration tests against real Postgres and Redis | 1, 4–8 |

## Deferred to later plans

- **Image, chart, ingress, the smoke test through auth-svc, minikube** (Plan 5B).
- **Sending email and relaying events** (Plan 6): the worker reads and deletes the
  outbox, sends `email.send` (log sender locally, Resend later), and relays
  `user.verified`, `device.registered`, `device.revoked` and `user.password_reset` to a
  new internal message-svc API, which kicks devices. Once the worker deletes outbox rows,
  the DEV ONLY mailbox must read from the worker's log sender or `email_message` instead.
- **`GET /v1/users?ids=…`** (Plan 7): auth-svc asks message-svc which of the ids share a
  contact or conversation with the caller.
- **Access tokens in the gateway** (Plan 8): verifying with the JWKS (cached), `ReAuth`,
  4001/4002. An access token issued before a password reset stays valid until it expires
  (at most 15 minutes) unless the gateway also refuses tokens older than the reset.
- **Calibrating Argon2id** on the production VM (spec §13), raising the parameters there
  (hashes are upgraded on the next login).
- **Cleanup jobs** for expired and rotated refresh tokens and old revoked devices
  (Plan 6's worker).
- **Signing-key rotation** (several keys in the JWKS, `kid` selects); **email change**
  (out of scope in v1; links already bind the address).
