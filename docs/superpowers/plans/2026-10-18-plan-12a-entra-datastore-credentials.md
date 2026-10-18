# Plan 12A: Entra ID Credentials for PostgreSQL and Redis Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Work sequentially on one branch.

**Goal:** Let auth-svc, message-svc, the worker and the gateway connect to managed PostgreSQL and Redis with short-lived Microsoft Entra ID tokens from Azure workload identity (or managed identity) instead of passwords, opt-in and with password mode unchanged.

**Architecture:** A new crate `crates\push-credentials` fetches tokens with two plain OAuth 2.0 calls over rustls (no Azure SDK, no OpenSSL), refreshes them at 75% of their lifetime and exposes them as a `tokio::sync::watch` stream. `push-store` turns a `DatabaseTarget` into a `Connector` whose pools swap their connect options on every new token; `push-credentials` turns a `RedisTarget` into a `RedisConnector` whose managed connections re-`AUTH` through redis-rs's `StreamingCredentialsProvider` and whose pub/sub subscriber the gateway hub replaces make-before-break. Each database service gains a `migrate` subcommand and `PUSH_RUN_MIGRATIONS`.

**Tech Stack:** Rust 2024 / MSRV 1.85, Tokio, sqlx 0.8.6 (`tls-rustls-ring-native-roots`), redis 1.2.2 (`token-based-authentication`, `tokio-rustls-comp`), reqwest 0.12 (`rustls-tls-native-roots`), rustls 0.23 with ring; tests with testcontainers, rcgen 0.13 and a stub token issuer.

---

## Authority and boundaries

Read first: `docs\superpowers\specs\2026-10-18-azure-entra-datastores-design.md` (the
decisions D1 to D10 and the configuration tables are binding). Plan 12B (chart, guide,
minikube rehearsal) follows this plan; nothing here touches `deploy\`.

This plan provisions nothing on Azure and never calls a real Entra endpoint: every test
uses the stub issuer of Task 3. Password mode must stay byte-for-byte compatible: the
existing tests must pass unchanged except where a `Config` struct literal gains the new
fields.

Start from clean, current `main`:

```powershell
git switch main
git pull --ff-only
git switch -c feature/plan-12a-entra-credentials
$env:CARGO_INCREMENTAL = '0'
cargo test --workspace --locked
```

Expected: everything passes (Docker must be running).

Every commit ends with:

```text
Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>
```

## Facts the design depends on (verified)

| Fact | Source | Consequence |
|---|---|---|
| redis 1.2.2 is the last release with `rust-version = 1.85`; 1.0.4+ have `entra-id` and `token-based-authentication` | crates.io API | Stay on the locked 1.2.2; MSRV unchanged |
| `entra-id` pulls `azure_identity` 0.31 with `client_certificate` (OpenSSL) and `azure_core` default `reqwest_native_tls` | crates.io dependency metadata of redis 1.2.2 / azure_core 0.31 | Do not enable `entra-id`; implement the provider ourselves (D1) |
| `token-based-authentication` exposes `redis::auth::{BasicAuth, StreamingCredentialsProvider}` and `set_credentials_provider` on `ConnectionManagerConfig` / `AsyncConnectionConfig` | `redis-1.2.2\src\auth.rs:47`, `aio\connection_manager.rs:298` | Our provider plugs into every managed connection |
| A multiplexed connection takes the provider's first item to connect, then re-`AUTH`s on every later item | `redis-1.2.2\src\aio\multiplexed_connection.rs:615-700` | The stream must yield the current token at once, then each new one |
| `Client::get_async_pubsub()` takes no credentials provider | `redis-1.2.2\src\client.rs:603` | The hub builds a client with the current token per connection and rotates (D4) |
| `RedisConnectionInfo::set_username/set_password`, `ConnectionInfo::set_redis_settings`, `Client::build_with_tls(info, TlsCertificates { client_tls, root_cert })` | `redis-1.2.2\src\connection.rs:283-351`, `client.rs:161`, `tls.rs:25-29` | Per-connection credentials and a custom CA |
| redis's rustls is `default-features = false`; ring is the only rustls provider in the tree | `redis-1.2.2\Cargo.toml`; `cargo tree -i aws-lc-rs` finds nothing | Install the ring provider at start-up to be explicit |
| `Pool::set_connect_options` and `Pool::close_event` exist in sqlx 0.8.6 | `sqlx-core-0.8.6\src\pool\mod.rs:555` | Token rotation for new connections |
| sqlx 0.8.6 has `tls-rustls-ring-native-roots` | `sqlx-0.8.6\Cargo.toml:507` | TLS with system roots, ring |
| `citext` is created by the auth and worker migrations | `crates\push-auth\migrations\0001_auth.sql:8`, `crates\push-worker\migrations\0001_worker.sql:8` | Allow-list check before those migrations |
| auth-svc's `SECURITY DEFINER` cleanup functions run with their owner's rights | `crates\push-auth\migrations\0002_worker.sql` | With a migration identity they run as it; the worker's grants are unchanged |
| backon 1.6.0 (pulled by token auth) has `rust-version = 1.85`; rcgen 0.13 defaults to ring | crates.io | MSRV and OpenSSL-free kept |
| Every service has `Config::from_lookup` | `crates\push-*\src\config.rs` | Tests configure services through variables, exactly as pods do |

## Files

| File | Change |
|---|---|
| `Cargo.toml` | member `crates/push-credentials`; workspace deps `push-credentials`, `rustls`, `rcgen`; redis and sqlx features |
| `crates\push-credentials\Cargo.toml` | new crate; feature `testing` |
| `crates\push-credentials\src\lib.rs` | exports, `install_crypto_provider`, `ConfigProblem` |
| `crates\push-credentials\src\mode.rs` | `AuthMode` |
| `crates\push-credentials\src\token.rs` | `AccessToken`, `TokenError`, `TokenSource` |
| `crates\push-credentials\src\azure.rs` | `AzureConfig`, `WorkloadIdentity`, `ManagedIdentity` |
| `crates\push-credentials\src\tokens.rs` | `Tokens` (fetch, refresh, watch) |
| `crates\push-credentials\src\redis_connect.rs` | `RedisTarget`, `RedisConnector`, the credentials provider, `is_refused` |
| `crates\push-credentials\src\startup.rs` | `retry` shared by the services |
| `crates\push-credentials\src\testing.rs` | `StubIssuer`, `TestCa`, `tls_postgres`, `tls_redis` (feature `testing`) |
| `crates\push-store\src\connect.rs` | `DatabaseTarget`, `DatabaseVars`, `Connector`, grants, allow-list check, `is_principal_name`, `is_permanent` |
| `crates\push-store\src\lib.rs` | URL functions delegate to `Connector` |
| `crates\push-message-svc\src\{config,server,lib,main,notify}.rs` | targets, connectors, `migrate` |
| `crates\push-auth\src\{config,server,lib,main,db,limits}.rs` | targets, connectors, `migrate`, principal names |
| `crates\push-worker\src\{config,server,lib,main}.rs` | two database targets, `migrate` |
| `crates\push-gateway\src\{config,server,hub}.rs` | Redis target, connector, hub rotation |
| tests in each crate | struct literals gain the new fields; new Entra tests |
| `crates\push-smoke\tests\common\mod.rs`, `local.rs`, `entra.rs` | shared stack builder; Entra whole-stack and fail-fast tests |
| `docs\superpowers\plans\2026-10-11-roadmap.md`, the spec | status |

---

### Task 1: Workspace dependencies and the `push-credentials` core types

**Files:**
- Modify: `Cargo.toml`
- Create: `crates\push-credentials\Cargo.toml`, `crates\push-credentials\src\lib.rs`, `crates\push-credentials\src\mode.rs`, `crates\push-credentials\src\token.rs`

- [ ] **Step 1: Workspace entries.** In the root `Cargo.toml` add `"crates/push-credentials",` to `members` (alphabetically, after `crates/push-auth`), and in `[workspace.dependencies]`:

```toml
push-credentials = { path = "crates/push-credentials" }
rcgen = "0.13"
rustls = { version = "0.23", default-features = false, features = ["ring", "std"] }
```

Extend the existing `redis` and `sqlx` entries (keep every existing feature):

```toml
redis = { version = "1", default-features = false, features = [
    "connection-manager",
    "tokio-comp",
    "tokio-rustls-comp",
    "token-based-authentication",
] }
```

and add `"tls-rustls-ring-native-roots",` to the `sqlx` feature list.

- [ ] **Step 2: The crate manifest.** Create `crates\push-credentials\Cargo.toml`:

```toml
[package]
name = "push-credentials"
description = "Datastore credentials for push-messenger: password URLs or Microsoft Entra ID tokens."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[features]
# The stub token issuer and TLS containers for tests in other crates.
testing = ["dep:axum", "dep:rcgen", "dep:testcontainers-modules", "dep:uuid"]

[dependencies]
async-trait.workspace = true
axum = { workspace = true, optional = true }
base64.workspace = true
futures-util.workspace = true
rcgen = { workspace = true, optional = true }
redis.workspace = true
reqwest = { workspace = true, features = ["rustls-tls-native-roots"] }
rustls.workspace = true
serde.workspace = true
serde_json.workspace = true
testcontainers-modules = { workspace = true, optional = true }
thiserror.workspace = true
tokio = { workspace = true, features = ["fs", "macros", "rt", "sync", "time"] }
tokio-util.workspace = true
tracing.workspace = true
uuid = { workspace = true, optional = true }

[dev-dependencies]
tokio = { workspace = true, features = ["rt-multi-thread", "test-util"] }

[lints]
workspace = true
```

- [ ] **Step 3: `mode.rs`.**

```rust
//! Which credentials a datastore takes.

/// `password` (a URL with the password in it, the default) or `azure-entra` (Microsoft
/// Entra ID access tokens from Azure workload identity or managed identity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthMode {
    #[default]
    Password,
    AzureEntra,
}

impl AuthMode {
    /// Parses a `PUSH_POSTGRES_AUTH` / `PUSH_REDIS_AUTH` value; unset or empty is
    /// `password`.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("password") => Ok(Self::Password),
            Some("azure-entra") => Ok(Self::AzureEntra),
            Some(_) => Err("expected password or azure-entra".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_means_password_and_only_two_modes_exist() {
        assert_eq!(AuthMode::parse(None), Ok(AuthMode::Password));
        assert_eq!(AuthMode::parse(Some(" ")), Ok(AuthMode::Password));
        assert_eq!(AuthMode::parse(Some("password")), Ok(AuthMode::Password));
        assert_eq!(AuthMode::parse(Some("azure-entra")), Ok(AuthMode::AzureEntra));
        assert!(AuthMode::parse(Some("entra")).is_err());
        assert!(AuthMode::parse(Some("Azure-Entra")).is_err());
    }
}
```

- [ ] **Step 4: `token.rs`.**

```rust
//! An access token and where tokens come from.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use tokio::time::Instant;

/// A bearer token and when it stops working. Its `Debug` never shows the token.
#[derive(Clone)]
pub struct AccessToken {
    secret: Arc<str>,
    issued_at: Instant,
    expires_at: Instant,
}

impl AccessToken {
    /// A token received now that lives `lifetime`.
    pub fn new(secret: impl Into<Arc<str>>, lifetime: Duration) -> Self {
        let issued_at = Instant::now();
        Self {
            secret: secret.into(),
            issued_at,
            expires_at: issued_at + lifetime,
        }
    }

    /// The token itself: the password of the connection. Never log it.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    pub fn issued_at(&self) -> Instant {
        self.issued_at
    }

    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    /// The `oid` claim of a JWT access token (the principal's object id), read without
    /// checking the signature: the token came straight from the issuer, and the claim
    /// only names the Redis user it is presented for.
    pub fn object_id(&self) -> Option<String> {
        let payload = self.secret.split('.').nth(1)?;
        let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
        let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        claims.get("oid")?.as_str().map(str::to_owned)
    }
}

impl PartialEq for AccessToken {
    fn eq(&self, other: &Self) -> bool {
        self.secret == other.secret
    }
}

impl Eq for AccessToken {}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessToken")
            .field("secret", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Why no token could be had. Never carries a token, an assertion or the issuer's
/// free-text description (which may echo request data).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("the Azure identity is not configured: {0}")]
    Config(String),
    #[error("cannot read the federated token file: {0}")]
    File(String),
    #[error("the token endpoint could not be reached: {0}")]
    Unreachable(String),
    #[error("the token endpoint refused the request: HTTP {status}{}", code.as_deref().map(|code| format!(" ({code})")).unwrap_or_default())]
    Refused { status: u16, code: Option<String> },
    #[error("the token endpoint answered something that is not a token")]
    Malformed,
}

/// Something that issues access tokens for a scope.
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn fetch(&self, scope: &str) -> Result<AccessToken, TokenError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: &str) -> String {
        format!(
            "{}.{}.signature",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#),
            URL_SAFE_NO_PAD.encode(claims)
        )
    }

    #[test]
    fn the_object_id_comes_from_the_payload_and_the_debug_output_hides_the_token() {
        let token = AccessToken::new(jwt(r#"{"oid":"5f1c","tid":"t"}"#), Duration::from_secs(60));
        assert_eq!(token.object_id().as_deref(), Some("5f1c"));
        assert!(!format!("{token:?}").contains(token.secret()));
        assert_eq!(AccessToken::new("opaque", Duration::from_secs(1)).object_id(), None);
        assert_eq!(AccessToken::new(jwt("{}"), Duration::from_secs(1)).object_id(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_expires_after_its_lifetime() {
        let token = AccessToken::new("t", Duration::from_secs(10));
        assert!(!token.is_expired());
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(token.is_expired());
    }

    #[test]
    fn errors_name_the_status_and_code_only() {
        let refused = TokenError::Refused {
            status: 401,
            code: Some("invalid_client".to_owned()),
        };
        assert_eq!(
            refused.to_string(),
            "the token endpoint refused the request: HTTP 401 (invalid_client)"
        );
    }
}
```

- [ ] **Step 5: `lib.rs`.**

```rust
//! Datastore credentials for push-messenger (spec 2026-10-18): a password URL, or
//! Microsoft Entra ID access tokens from Azure workload identity or managed identity,
//! refreshed before they expire and handed to PostgreSQL (by `push-store`) and Redis (by
//! [`RedisConnector`]). Tokens are fetched with plain OAuth 2.0 requests over rustls: no
//! Azure SDK and no OpenSSL. Tokens and assertions are never logged.

mod azure;
mod mode;
mod redis_connect;
pub mod startup;
mod token;
mod tokens;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod tests;

pub use azure::{AzureConfig, DEFAULT_AUTHORITY_HOST, IMDS_ENDPOINT, ManagedIdentity, WorkloadIdentity};
pub use mode::AuthMode;
pub use redis_connect::{REDIS_SCOPE, RedisConnector, RedisTarget, is_refused};
pub use token::{AccessToken, TokenError, TokenSource};
pub use tokens::{Tokens, refresh_at};

/// A configuration variable that is missing or not valid. Services turn it into their
/// own error, redacting the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigProblem {
    Missing(&'static str),
    Invalid { name: &'static str, reason: String },
}

/// Makes ring the process's rustls provider, before any TLS connection: redis, sqlx and
/// reqwest then agree on it. Idempotent.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
```

Create empty placeholders so the crate builds until their tasks: `src\azure.rs`,
`src\redis_connect.rs`, `src\startup.rs` and `src\tokens.rs` each containing only
`//! Filled in by a later task of Plan 12A.`, `src\testing.rs` with
`//! Test doubles (Task 2).` and `src\tests\mod.rs` with
`//! The crate's integration-style tests against the stub issuer and TLS containers.`, and
comment out the matching `pub use` lines with `// Task N:` notes. Each later task restores its line. (Every task ends with
a building workspace.)

- [ ] **Step 6: Build and test.**

Run: `cargo test -p push-credentials --locked` (first run updates `Cargo.lock`: use
`cargo test -p push-credentials` without `--locked` once, then `git diff Cargo.lock` must
show only the new crate, `rustls` features and the new redis/sqlx dependencies such as
`backon`, `tokio-rustls`, `rustls-native-certs`).
Expected: 4 tests pass. Then `cargo tree -i aws-lc-rs --workspace` prints "did not match
any packages" and `cargo tree -i openssl-sys --workspace` too.

- [ ] **Step 7: Commit.**

```powershell
git add -A
git commit -m "feat(credentials): add the push-credentials crate with tokens and auth modes"
```

### Task 2: The stub token issuer (feature `testing`)

Every later test gets its tokens from this stub: it speaks the two Azure endpoints the
real sources call, issues JWT-shaped tokens carrying an `oid`, and runs a hook before it
answers, which the datastore tests use to make each token the principal's password.

**Files:**
- Create: `crates\push-credentials\src\testing.rs`
- Modify: `crates\push-credentials\Cargo.toml` (dev-dependency on itself)

- [ ] **Step 1: Let the crate's own tests use the module.** A package cannot depend on
itself, so the module compiles for the crate's own tests and for the `testing` feature:
in `lib.rs` change its declaration to

```rust
#[cfg(any(test, feature = "testing"))]
pub mod testing;
```

and in `crates\push-credentials\Cargo.toml` repeat the optional dependencies as
dev-dependencies (Cargo allows a crate in both tables):

```toml
[dev-dependencies]
axum.workspace = true
rcgen.workspace = true
testcontainers-modules.workspace = true
uuid.workspace = true
```

The crate's tests are therefore unit-test modules (`src\tests\*.rs`, declared in
`lib.rs` as `#[cfg(test)] mod tests;` with a `src\tests\mod.rs` listing them), not
`tests\*.rs` integration tests: those would not see `cfg(test)`. Where later tasks say
"`crates\push-credentials\tests\X.rs`", create `src\tests\X.rs` instead, declare it in
`src\tests\mod.rs`, write `use crate::...` for the crate's items (`push_credentials::`
becomes `crate::`), and run it with `cargo test -p push-credentials --lib tests::X`.

- [ ] **Step 2: Write `testing.rs`** (the TLS helpers join it in Task 5):

```rust
//! Test doubles for Entra ID credentials (feature `testing`): a stub of the Microsoft
//! Entra token endpoint and of the managed-identity endpoint (IMDS), and, below,
//! a test certificate authority with PostgreSQL and Redis containers that require TLS.
//! Never used by a service binary.

use std::collections::HashMap;
use std::future::IntoFuture;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::future::BoxFuture;
use serde_json::json;

const JWT_BEARER: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

/// A principal the stub issues tokens for: an identity's client id and object id.
#[derive(Debug, Clone)]
pub struct StubPrincipal {
    pub client_id: String,
    pub object_id: String,
}

/// One token the stub issued.
#[derive(Debug, Clone)]
pub struct Issued {
    pub client_id: String,
    pub object_id: String,
    pub scope: String,
    pub token: String,
}

/// Runs before the stub answers with a token, e.g. to make it the datastore password.
pub type IssueHook = Arc<dyn Fn(Issued) -> BoxFuture<'static, ()> + Send + Sync>;

struct Stub {
    tenant: String,
    assertion: String,
    principals: Vec<StubPrincipal>,
    lifetime_s: AtomicU64,
    failing: AtomicBool,
    serial: AtomicU64,
    issued: Mutex<Vec<Issued>>,
    hook: Option<IssueHook>,
}

/// The stub issuer, listening on 127.0.0.1.
pub struct StubIssuer {
    authority: String,
    state: Arc<Stub>,
}

impl StubIssuer {
    /// Issues tokens of `lifetime` (whole seconds, at least 1) to `principals` of
    /// `tenant` that present `assertion` as their federated token.
    pub async fn start(
        tenant: &str,
        assertion: &str,
        principals: Vec<StubPrincipal>,
        lifetime: Duration,
        hook: Option<IssueHook>,
    ) -> Self {
        let state = Arc::new(Stub {
            tenant: tenant.to_owned(),
            assertion: assertion.to_owned(),
            principals,
            lifetime_s: AtomicU64::new(lifetime.as_secs().max(1)),
            failing: AtomicBool::new(false),
            serial: AtomicU64::new(0),
            issued: Mutex::new(Vec::new()),
            hook,
        });
        let router = Router::new()
            .route("/{tenant}/oauth2/v2.0/token", post(token))
            .route("/metadata/identity/oauth2/token", get(imds))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, router).into_future());
        Self { authority, state }
    }

    /// The value for `AZURE_AUTHORITY_HOST`, with a trailing `/`.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// The managed-identity endpoint, for `ManagedIdentity::new`.
    pub fn imds_endpoint(&self) -> String {
        format!("{}metadata/identity/oauth2/token", self.authority)
    }

    /// Makes every request fail with 503 until called again with `false`.
    pub fn set_failing(&self, failing: bool) {
        self.state.failing.store(failing, Ordering::SeqCst);
    }

    /// Every token issued so far, in order.
    pub fn issued(&self) -> Vec<Issued> {
        self.state.issued.lock().unwrap().clone()
    }
}

async fn token(
    State(stub): State<Arc<Stub>>,
    Path(tenant): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if stub.failing.load(Ordering::SeqCst) {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    let field = |name: &str| form.get(name).map(String::as_str).unwrap_or_default();
    if tenant != stub.tenant
        || field("grant_type") != "client_credentials"
        || field("client_assertion_type") != JWT_BEARER
        || field("client_assertion") != stub.assertion
    {
        return refuse(StatusCode::UNAUTHORIZED, "invalid_client");
    }
    issue(&stub, field("client_id"), field("scope"), false).await
}

async fn imds(
    State(stub): State<Arc<Stub>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if stub.failing.load(Ordering::SeqCst) {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    if headers.get("metadata").and_then(|value| value.to_str().ok()) != Some("true") {
        return refuse(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let client_id = match query.get("client_id") {
        Some(client_id) => client_id.clone(),
        None => stub.principals[0].client_id.clone(),
    };
    let scope = format!("{}/.default", query.get("resource").cloned().unwrap_or_default());
    issue(&stub, &client_id, &scope, true).await
}

async fn issue(stub: &Stub, client_id: &str, scope: &str, lifetime_as_text: bool) -> Response {
    let Some(principal) = stub.principals.iter().find(|p| p.client_id == client_id) else {
        return refuse(StatusCode::BAD_REQUEST, "unauthorized_client");
    };
    if !scope.ends_with("/.default") || scope.len() <= "/.default".len() {
        return refuse(StatusCode::BAD_REQUEST, "invalid_scope");
    }
    let serial = stub.serial.fetch_add(1, Ordering::SeqCst);
    let claims = json!({"oid": principal.object_id, "appid": client_id, "n": serial});
    let token = format!(
        "{}.{}.stub",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let issued = Issued {
        client_id: client_id.to_owned(),
        object_id: principal.object_id.clone(),
        scope: scope.to_owned(),
        token: token.clone(),
    };
    if let Some(hook) = &stub.hook {
        hook(issued.clone()).await;
    }
    stub.issued.lock().unwrap().push(issued);
    let lifetime = stub.lifetime_s.load(Ordering::SeqCst);
    let expires_in = if lifetime_as_text {
        json!(lifetime.to_string())
    } else {
        json!(lifetime)
    };
    Json(json!({"token_type": "Bearer", "expires_in": expires_in, "access_token": token}))
        .into_response()
}

/// An OAuth error answer whose description echoes request data, as real ones may: the
/// sources must never pass it on.
fn refuse(status: StatusCode, code: &str) -> Response {
    let body = json!({
        "error": code,
        "error_description": "AADSTS700016: client_assertion=leaked-assertion",
    });
    (status, Json(body)).into_response()
}
```

- [ ] **Step 3: Build.**

Run: `cargo build -p push-credentials --features testing`
Expected: builds without warnings.

- [ ] **Step 4: Commit.**

```powershell
git add -A
git commit -m "test(credentials): add a stub Entra token issuer"
```

### Task 3: Workload identity and managed identity sources

**Files:**
- Modify: `crates\push-credentials\src\azure.rs`, `crates\push-credentials\src\lib.rs`
- Test: `crates\push-credentials\tests\azure.rs`

- [ ] **Step 1: Write the failing tests** in `crates\push-credentials\tests\azure.rs`:

```rust
//! The Azure token sources against the stub issuer.

use std::time::Duration;

use push_credentials::testing::{StubIssuer, StubPrincipal};
use push_credentials::{AzureConfig, ConfigProblem, ManagedIdentity, TokenError, WorkloadIdentity};

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const SCOPE: &str = "https://ossrdbms-aad.database.windows.net/.default";

fn principal() -> Vec<StubPrincipal> {
    vec![StubPrincipal {
        client_id: "client-a".to_owned(),
        object_id: "oid-a".to_owned(),
    }]
}

fn assertion_file(contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("push-credentials-{}", std::process::id()))
        .join(format!("{}.jwt", contents.len()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("{contents}\n")).unwrap();
    path
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

#[tokio::test]
async fn workload_identity_exchanges_the_federated_token_for_an_access_token() {
    let stub = StubIssuer::start(TENANT, "federated", principal(), Duration::from_secs(3600), None).await;
    let source = WorkloadIdentity::new(http(), stub.authority(), TENANT, "client-a", &assertion_file("federated")).unwrap();
    let token = push_credentials::TokenSource::fetch(&source, SCOPE).await.unwrap();
    assert_eq!(token.object_id().as_deref(), Some("oid-a"));
    let issued = stub.issued();
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].scope, SCOPE);
    assert_eq!(issued[0].token, token.secret());
}

#[tokio::test]
async fn a_refusal_reports_the_status_and_code_but_never_the_description() {
    let stub = StubIssuer::start(TENANT, "federated", principal(), Duration::from_secs(3600), None).await;
    let source = WorkloadIdentity::new(http(), stub.authority(), TENANT, "client-a", &assertion_file("wrong-assertion")).unwrap();
    let error = push_credentials::TokenSource::fetch(&source, SCOPE).await.unwrap_err();
    assert_eq!(error, TokenError::Refused { status: 401, code: Some("invalid_client".to_owned()) });
    let text = error.to_string();
    assert!(!text.contains("AADSTS") && !text.contains("leaked") && !text.contains("wrong-assertion"), "{text}");
}

#[tokio::test]
async fn managed_identity_asks_imds_with_the_resource_and_reads_a_text_lifetime() {
    let stub = StubIssuer::start(TENANT, "federated", principal(), Duration::from_secs(3600), None).await;
    let source = ManagedIdentity::new(http(), &stub.imds_endpoint(), Some("client-a".to_owned())).unwrap();
    let token = push_credentials::TokenSource::fetch(&source, SCOPE).await.unwrap();
    assert_eq!(token.object_id().as_deref(), Some("oid-a"));
    assert_eq!(stub.issued()[0].scope, SCOPE);
}

#[tokio::test]
async fn an_unreachable_endpoint_and_a_missing_file_are_reported_without_details() {
    let closed = WorkloadIdentity::new(http(), "http://127.0.0.1:9/", TENANT, "client-a", &assertion_file("x")).unwrap();
    assert!(matches!(push_credentials::TokenSource::fetch(&closed, SCOPE).await, Err(TokenError::Unreachable(_))));
    let stub = StubIssuer::start(TENANT, "federated", principal(), Duration::from_secs(3600), None).await;
    let missing = WorkloadIdentity::new(http(), stub.authority(), TENANT, "client-a", std::path::Path::new("Z:\\no\\such\\file")).unwrap();
    assert!(matches!(push_credentials::TokenSource::fetch(&missing, SCOPE).await, Err(TokenError::File(_))));
}

#[test]
fn the_configuration_requires_https_except_on_this_machine() {
    let lookup = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
    };
    let workload = [
        ("AZURE_CLIENT_ID", "client-a"),
        ("AZURE_TENANT_ID", TENANT),
        ("AZURE_FEDERATED_TOKEN_FILE", "/var/run/secrets/azure/tokens/azure-identity-token"),
    ];
    let config = AzureConfig::from_lookup(&lookup(&workload)).unwrap();
    assert_eq!(config.authority_host, "https://login.microsoftonline.com/");
    assert_eq!(config.client_id.as_deref(), Some("client-a"));
    let local = AzureConfig::from_lookup(&lookup(&[("AZURE_AUTHORITY_HOST", "http://127.0.0.1:8080")])).unwrap();
    assert_eq!(local.authority_host, "http://127.0.0.1:8080/");
    assert!(local.federated_token_file.is_none(), "managed identity");
    for bad in ["http://login.example.com/", "ftp://x/", "https://x/?q=1", "not a url"] {
        let pairs: &'static [(&'static str, &'static str)] = Box::leak(Box::new([("AZURE_AUTHORITY_HOST", bad)]));
        assert!(matches!(AzureConfig::from_lookup(&lookup(pairs)), Err(ConfigProblem::Invalid { name: "AZURE_AUTHORITY_HOST", .. })), "{bad}");
    }
    assert_eq!(
        AzureConfig::from_lookup(&lookup(&[("AZURE_FEDERATED_TOKEN_FILE", "/t")])),
        Err(ConfigProblem::Missing("AZURE_CLIENT_ID"))
    );
    assert_eq!(
        AzureConfig::from_lookup(&lookup(&[("AZURE_FEDERATED_TOKEN_FILE", "/t"), ("AZURE_CLIENT_ID", "c")])),
        Err(ConfigProblem::Missing("AZURE_TENANT_ID"))
    );
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p push-credentials --test azure`
Expected: compile errors: `AzureConfig`, `WorkloadIdentity`, `ManagedIdentity` not found.

- [ ] **Step 3: Implement `azure.rs`.**

```rust
//! Access tokens from Azure: workload identity (a federated Kubernetes service-account
//! token exchanged at Microsoft Entra ID) or managed identity (IMDS). Plain OAuth 2.0 over
//! rustls; requests time out after 10 s; answers are read up to 64 KiB.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde::Deserialize;

use crate::ConfigProblem;
use crate::token::{AccessToken, TokenError, TokenSource};

/// Microsoft Entra ID in the global Azure cloud.
pub const DEFAULT_AUTHORITY_HOST: &str = "https://login.microsoftonline.com/";
/// The Azure Instance Metadata Service's token endpoint.
pub const IMDS_ENDPOINT: &str = "http://169.254.169.254/metadata/identity/oauth2/token";
const JWT_BEARER: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ANSWER_BYTES: u64 = 64 * 1024;

/// The identity variables (set by the AKS workload-identity webhook, or by the chart).
/// With `AZURE_FEDERATED_TOKEN_FILE` the source is workload identity, which needs
/// `AZURE_CLIENT_ID` and `AZURE_TENANT_ID`; without it, managed identity, where
/// `AZURE_CLIENT_ID` selects a user-assigned identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureConfig {
    pub client_id: Option<String>,
    pub tenant_id: Option<String>,
    pub federated_token_file: Option<PathBuf>,
    /// `https://`, ending in `/`; plain `http://` only to this machine (tests).
    pub authority_host: String,
}

impl AzureConfig {
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigProblem> {
        let get = |name: &str| lookup(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let authority = get("AZURE_AUTHORITY_HOST").unwrap_or_else(|| DEFAULT_AUTHORITY_HOST.to_owned());
        let authority_host = check_authority(&authority)
            .map_err(|reason| ConfigProblem::Invalid { name: "AZURE_AUTHORITY_HOST", reason })?
            .to_string();
        let federated_token_file = get("AZURE_FEDERATED_TOKEN_FILE").map(PathBuf::from);
        let client_id = get("AZURE_CLIENT_ID");
        let tenant_id = get("AZURE_TENANT_ID");
        if federated_token_file.is_some() {
            if client_id.is_none() {
                return Err(ConfigProblem::Missing("AZURE_CLIENT_ID"));
            }
            match &tenant_id {
                None => return Err(ConfigProblem::Missing("AZURE_TENANT_ID")),
                Some(tenant) if !is_tenant(tenant) => {
                    return Err(ConfigProblem::Invalid {
                        name: "AZURE_TENANT_ID",
                        reason: "expected a tenant id or domain".to_owned(),
                    });
                }
                Some(_) => {}
            }
        }
        Ok(Self { client_id, tenant_id, federated_token_file, authority_host })
    }

    /// The token source these variables describe.
    pub fn source(&self) -> Result<Arc<dyn TokenSource>, TokenError> {
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| TokenError::Config("cannot build the HTTP client".to_owned()))?;
        match &self.federated_token_file {
            Some(file) => Ok(Arc::new(WorkloadIdentity::new(
                http,
                &self.authority_host,
                self.tenant_id.as_deref().unwrap_or_default(),
                self.client_id.as_deref().unwrap_or_default(),
                file,
            )?)),
            None => Ok(Arc::new(ManagedIdentity::new(http, IMDS_ENDPOINT, self.client_id.clone())?)),
        }
    }
}

fn check_authority(value: &str) -> Result<Url, String> {
    let mut url = Url::parse(value).map_err(|_| "not a URL".to_owned())?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err("must be an https:// URL".to_owned()),
    }
    if url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err("must be a plain base URL".to_owned());
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn is_tenant(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// Workload identity: the projected service-account token at `token_file` (re-read on
/// every request; the kubelet rotates it) is exchanged for an access token.
pub struct WorkloadIdentity {
    http: reqwest::Client,
    token_url: Url,
    client_id: String,
    token_file: PathBuf,
}

impl WorkloadIdentity {
    pub fn new(
        http: reqwest::Client,
        authority_host: &str,
        tenant_id: &str,
        client_id: &str,
        token_file: &Path,
    ) -> Result<Self, TokenError> {
        let authority = check_authority(authority_host).map_err(TokenError::Config)?;
        if !is_tenant(tenant_id) || client_id.is_empty() {
            return Err(TokenError::Config("a client id and a tenant id are required".to_owned()));
        }
        let token_url = authority
            .join(&format!("{tenant_id}/oauth2/v2.0/token"))
            .map_err(|_| TokenError::Config("cannot build the token URL".to_owned()))?;
        Ok(Self { http, token_url, client_id: client_id.to_owned(), token_file: token_file.to_owned() })
    }
}

#[async_trait]
impl TokenSource for WorkloadIdentity {
    async fn fetch(&self, scope: &str) -> Result<AccessToken, TokenError> {
        let assertion = tokio::fs::read_to_string(&self.token_file)
            .await
            .map_err(|error| TokenError::File(error.kind().to_string()))?;
        let form = [
            ("client_id", self.client_id.as_str()),
            ("scope", scope),
            ("grant_type", "client_credentials"),
            ("client_assertion_type", JWT_BEARER),
            ("client_assertion", assertion.trim()),
        ];
        answer(self.http.post(self.token_url.clone()).form(&form).send().await).await
    }
}

/// Managed identity through IMDS; `client_id` selects a user-assigned identity.
pub struct ManagedIdentity {
    http: reqwest::Client,
    endpoint: Url,
    client_id: Option<String>,
}

impl ManagedIdentity {
    pub fn new(http: reqwest::Client, endpoint: &str, client_id: Option<String>) -> Result<Self, TokenError> {
        let endpoint = Url::parse(endpoint)
            .map_err(|_| TokenError::Config("the managed-identity endpoint is not a URL".to_owned()))?;
        Ok(Self { http, endpoint, client_id })
    }
}

#[async_trait]
impl TokenSource for ManagedIdentity {
    async fn fetch(&self, scope: &str) -> Result<AccessToken, TokenError> {
        let resource = scope.strip_suffix("/.default").unwrap_or(scope);
        let mut query = vec![("api-version", "2018-02-01"), ("resource", resource)];
        if let Some(client_id) = &self.client_id {
            query.push(("client_id", client_id));
        }
        let request = self.http.get(self.endpoint.clone()).query(&query).header("Metadata", "true");
        answer(request.send().await).await
    }
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    expires_in: Lifetime,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Lifetime {
    Seconds(u64),
    Text(String),
}

#[derive(Deserialize)]
struct ErrorAnswer {
    error: Option<String>,
}

async fn answer(response: Result<reqwest::Response, reqwest::Error>) -> Result<AccessToken, TokenError> {
    let response = response.map_err(|error| unreachable(&error))?;
    if response.content_length().is_some_and(|length| length > MAX_ANSWER_BYTES) {
        return Err(TokenError::Malformed);
    }
    let status = response.status();
    let body = response.bytes().await.map_err(|error| unreachable(&error))?;
    if body.len() as u64 > MAX_ANSWER_BYTES {
        return Err(TokenError::Malformed);
    }
    if !status.is_success() {
        // Only a short machine code passes; the description may echo request data.
        let code = serde_json::from_slice::<ErrorAnswer>(&body)
            .ok()
            .and_then(|answer| answer.error)
            .filter(|code| code.len() <= 64 && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        return Err(TokenError::Refused { status: status.as_u16(), code });
    }
    let answer: TokenAnswer = serde_json::from_slice(&body).map_err(|_| TokenError::Malformed)?;
    let seconds = match answer.expires_in {
        Lifetime::Seconds(seconds) => seconds,
        Lifetime::Text(text) => text.parse().map_err(|_| TokenError::Malformed)?,
    };
    if answer.access_token.is_empty() || seconds == 0 {
        return Err(TokenError::Malformed);
    }
    Ok(AccessToken::new(answer.access_token, Duration::from_secs(seconds)))
}

fn unreachable(error: &reqwest::Error) -> TokenError {
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "connection failed"
    } else {
        "request failed"
    };
    TokenError::Unreachable(kind.to_owned())
}
```

Restore `pub use azure::{...};` in `lib.rs`.

- [ ] **Step 4: Run the tests.**

Run: `cargo test -p push-credentials --test azure`
Expected: 5 tests pass. (The "missing file" path `Z:\no\such\file` also fails on Linux CI:
any missing path works.)

- [ ] **Step 5: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "feat(credentials): fetch tokens with workload identity or managed identity"
```

### Task 4: `Tokens`: fetch at start, refresh at 75%, back off on failure

**Files:**
- Modify: `crates\push-credentials\src\tokens.rs`, `crates\push-credentials\src\lib.rs`

- [ ] **Step 1: Write the failing unit tests** at the end of `tokens.rs` (the file holds
only its doc comment so far):

```rust
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;

    /// Answers fetches from a script; counts them.
    struct Script {
        answers: Mutex<VecDeque<Result<(&'static str, u64), TokenError>>>,
        fetches: AtomicUsize,
    }

    impl Script {
        fn new(answers: Vec<Result<(&'static str, u64), TokenError>>) -> Arc<Self> {
            Arc::new(Self { answers: Mutex::new(answers.into()), fetches: AtomicUsize::new(0) })
        }
        fn fetches(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl TokenSource for Script {
        async fn fetch(&self, _scope: &str) -> Result<AccessToken, TokenError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let next = self.answers.lock().unwrap().pop_front().expect("a scripted answer");
            next.map(|(secret, seconds)| AccessToken::new(secret, Duration::from_secs(seconds)))
        }
    }

    fn down() -> TokenError {
        TokenError::Unreachable("connection failed".to_owned())
    }

    /// Lets the refresher run until it waits on time again.
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn start_fails_when_the_first_token_cannot_be_had() {
        let script = Script::new(vec![Err(down())]);
        let started = Tokens::start(script, "scope/.default", CancellationToken::new()).await;
        assert_eq!(started.err(), Some(down()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_is_refreshed_at_three_quarters_of_its_lifetime() {
        let script = Script::new(vec![Ok(("first", 100)), Ok(("second", 100))]);
        let tokens = Tokens::start(script.clone(), "scope/.default", CancellationToken::new()).await.unwrap();
        let mut watch = tokens.watch();
        assert_eq!(tokens.current().secret(), "first");
        tokio::time::advance(Duration::from_secs(74)).await;
        settle().await;
        assert_eq!(script.fetches(), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        watch.changed().await.unwrap();
        assert_eq!(tokens.current().secret(), "second");
        assert_eq!(script.fetches(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_keep_the_old_token_and_back_off_up_to_30_s() {
        let mut answers = vec![Ok(("first", 100))];
        answers.extend((0..7).map(|_| Err(down())));
        answers.push(Ok(("second", 100)));
        let script = Script::new(answers);
        let tokens = Tokens::start(script.clone(), "scope/.default", CancellationToken::new()).await.unwrap();
        tokio::time::advance(Duration::from_secs(75)).await;
        settle().await;
        assert_eq!(script.fetches(), 2, "the first refresh failed");
        // Retries after 1, 2, 4, 8, 16, 30 and 30 s.
        for (delay, fetches) in [(1, 3), (2, 4), (4, 5), (8, 6), (16, 7), (30, 8)] {
            tokio::time::advance(Duration::from_secs(delay)).await;
            settle().await;
            assert_eq!(script.fetches(), fetches, "after waiting {delay} s");
            assert_eq!(tokens.current().secret(), "first");
        }
        tokio::time::advance(Duration::from_secs(30)).await;
        settle().await;
        assert_eq!(tokens.current().secret(), "second");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_refreshing() {
        let script = Script::new(vec![Ok(("first", 100))]);
        let shutdown = CancellationToken::new();
        let _tokens = Tokens::start(script.clone(), "scope/.default", shutdown.clone()).await.unwrap();
        shutdown.cancel();
        tokio::time::advance(Duration::from_secs(1000)).await;
        settle().await;
        assert_eq!(script.fetches(), 1);
    }

    #[test]
    fn refresh_comes_at_75_percent() {
        let token = AccessToken::new("t", Duration::from_secs(3600));
        assert_eq!(refresh_at(&token) - token.issued_at(), Duration::from_secs(2700));
    }
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p push-credentials --lib tokens`
Expected: compile errors (`Tokens`, `refresh_at` missing).

- [ ] **Step 3: Implement** (above the tests):

```rust
//! A scope's current access token: fetched at start-up (failing start-up when it cannot
//! be had), refreshed at 75% of its lifetime, and published on a `watch` channel. A
//! failed refresh keeps the old token and is retried after 1 s, doubling to 30 s.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::token::{AccessToken, TokenError, TokenSource};

const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);

/// The current token of one scope. Cheap to clone.
#[derive(Clone)]
pub struct Tokens {
    receiver: watch::Receiver<AccessToken>,
}

impl Tokens {
    /// Fetches the first token, then refreshes in the background until `shutdown` or
    /// until every clone is dropped.
    pub async fn start(
        source: Arc<dyn TokenSource>,
        scope: &str,
        shutdown: CancellationToken,
    ) -> Result<Self, TokenError> {
        let first = source.fetch(scope).await?;
        let (sender, receiver) = watch::channel(first);
        tokio::spawn(refresh(source, scope.to_owned(), sender, shutdown));
        Ok(Self { receiver })
    }

    pub fn current(&self) -> AccessToken {
        self.receiver.borrow().clone()
    }

    /// A receiver that sees every later token (the current one counts as seen).
    pub fn watch(&self) -> watch::Receiver<AccessToken> {
        self.receiver.clone()
    }
}

/// When `token` is refreshed: after 75% of its lifetime.
pub fn refresh_at(token: &AccessToken) -> Instant {
    token.issued_at() + (token.expires_at() - token.issued_at()) * 3 / 4
}

async fn refresh(
    source: Arc<dyn TokenSource>,
    scope: String,
    sender: watch::Sender<AccessToken>,
    shutdown: CancellationToken,
) {
    loop {
        let due = refresh_at(&sender.borrow());
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = sender.closed() => return,
            () = tokio::time::sleep_until(due) => {}
        }
        let mut retry = FIRST_RETRY;
        loop {
            let fetched = tokio::select! {
                () = shutdown.cancelled() => return,
                fetched = source.fetch(&scope) => fetched,
            };
            match fetched {
                Ok(token) => {
                    sender.send_replace(token);
                    tracing::debug!(%scope, "refreshed an access token");
                    break;
                }
                Err(error) => {
                    if sender.borrow().is_expired() {
                        tracing::error!(%scope, %error, retry_in = ?retry, "the access token expired and cannot be refreshed");
                    } else {
                        tracing::warn!(%scope, %error, retry_in = ?retry, "cannot refresh the access token yet");
                    }
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        () = tokio::time::sleep(retry) => {}
                    }
                    retry = (retry * 2).min(MAX_RETRY);
                }
            }
        }
    }
}
```

Restore `pub use tokens::{Tokens, refresh_at};` in `lib.rs`.

- [ ] **Step 4: Run the tests.**

Run: `cargo test -p push-credentials --lib`
Expected: all unit tests pass (mode, token, tokens).

- [ ] **Step 5: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "feat(credentials): refresh access tokens before they expire"
```

### Task 5: A test CA and TLS-only PostgreSQL and Redis containers

Entra mode always verifies certificates (D6), so the integration tests need servers with
real TLS: a CA made with rcgen, PostgreSQL 17 with `ssl=on`, and Redis 7 listening only
on a TLS port. The datastore helpers apply each issued token as the principal's password,
the way Azure accepts its tokens.

**Files:**
- Modify: `crates\push-credentials\Cargo.toml`, `crates\push-credentials\src\testing.rs`
- Test: `crates\push-credentials\tests\tls.rs`

- [ ] **Step 1: Feature dependencies.** In `crates\push-credentials\Cargo.toml` add
`sqlx = { workspace = true, optional = true }` to `[dependencies]` and extend the feature:

```toml
testing = ["dep:axum", "dep:rcgen", "dep:sqlx", "dep:testcontainers-modules", "dep:uuid"]
```

- [ ] **Step 2: Write the failing test** `crates\push-credentials\tests\tls.rs`:

```rust
//! The TLS test servers accept clients that trust the test CA, and nobody else.

use push_credentials::testing::{TestCa, tls_postgres, tls_redis};

#[tokio::test]
async fn the_tls_servers_trust_only_the_test_ca() {
    push_credentials::install_crypto_provider();
    let ca = TestCa::new();
    let postgres = tls_postgres(&ca).await;
    let admin = postgres.admin(&ca).await;
    let one: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&admin).await.unwrap();
    assert_eq!(one, 1);
    let other = TestCa::new();
    assert!(postgres.try_admin(&other).await.is_err(), "an untrusted CA is refused");

    let redis = tls_redis(&ca).await;
    let mut connection = redis.admin(&ca).await;
    let pong: String = redis::cmd("PING").query_async(&mut connection).await.unwrap();
    assert_eq!(pong, "PONG");
}
```

Add `sqlx.workspace = true` and `redis.workspace = true` to the crate's
`[dev-dependencies]`.

- [ ] **Step 3: Run it to see it fail.**

Run: `cargo test -p push-credentials --test tls`
Expected: compile errors (`TestCa`, `tls_postgres`, `tls_redis` missing).

- [ ] **Step 4: Append the helpers to `testing.rs`.**

```rust
// ---- TLS test servers ----

use std::path::{Path as FilePath, PathBuf};

use rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use testcontainers_modules::testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, GenericImage, ImageExt};

/// A throwaway certificate authority; its certificate is also written to a file, as
/// `PUSH_DATABASE_CA_FILE` and `PUSH_REDIS_CA_FILE` expect.
pub struct TestCa {
    cert: Certificate,
    key: KeyPair,
    file: PathBuf,
}

impl TestCa {
    pub fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, "push-messenger test CA");
        let cert = params.self_signed(&key).unwrap();
        let file = std::env::temp_dir().join(format!("push-test-ca-{}.pem", uuid::Uuid::now_v7()));
        std::fs::write(&file, cert.pem()).unwrap();
        Self { cert, key, file }
    }

    pub fn pem(&self) -> String {
        self.cert.pem()
    }

    pub fn file(&self) -> &FilePath {
        &self.file
    }

    /// A server certificate for `127.0.0.1` and `localhost`: (certificate, key), PEM.
    pub fn server(&self) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        // rcgen turns "127.0.0.1" into an IP address name.
        let params = CertificateParams::new(vec!["127.0.0.1".to_owned(), "localhost".to_owned()]).unwrap();
        let cert = params.signed_by(&key, &self.cert, &self.key).unwrap();
        (cert.pem(), key.serialize_pem())
    }
}

impl Default for TestCa {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestCa {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

/// The superuser password of [`tls_postgres`].
pub const POSTGRES_ADMIN_PASSWORD: &str = "admin-password";

/// PostgreSQL 17 that requires nothing but offers TLS with a certificate of `ca`.
pub struct TlsPostgres {
    _container: ContainerAsync<GenericImage>,
    pub port: u16,
}

/// Starts it. The key must belong to `postgres` with mode 0600, so the entrypoint copies
/// the files into place before handing over to the image's own entrypoint.
pub async fn tls_postgres(ca: &TestCa) -> TlsPostgres {
    let (cert, key) = ca.server();
    let script = "install -o postgres -m 0600 /tls/server.key /var/lib/postgresql/server.key \
        && install -o postgres -m 0644 /tls/server.crt /var/lib/postgresql/server.crt \
        && exec docker-entrypoint.sh postgres -c ssl=on \
        -c ssl_cert_file=/var/lib/postgresql/server.crt \
        -c ssl_key_file=/var/lib/postgresql/server.key";
    let container = GenericImage::new("postgres", "17-alpine")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr("database system is ready to accept connections"))
        .with_entrypoint("sh")
        .with_env_var("POSTGRES_PASSWORD", POSTGRES_ADMIN_PASSWORD)
        .with_copy_to("/tls/server.crt", cert.into_bytes())
        .with_copy_to("/tls/server.key", key.into_bytes())
        .with_cmd(["-c", script])
        .start()
        .await
        .expect("start PostgreSQL with TLS (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    TlsPostgres { _container: container, port }
}

impl TlsPostgres {
    /// Connect options for `user` with full verification against `ca`.
    pub fn options(&self, ca: &TestCa, user: &str, database: &str) -> PgConnectOptions {
        PgConnectOptions::new_without_pgpass()
            .host("127.0.0.1")
            .port(self.port)
            .username(user)
            .database(database)
            .ssl_mode(PgSslMode::VerifyFull)
            .ssl_root_cert(ca.file())
    }

    /// A superuser pool, once the server accepts connections (it restarts once after
    /// initialising, so the first minute is retried).
    pub async fn admin(&self, ca: &TestCa) -> sqlx::PgPool {
        for _ in 0..120 {
            if let Ok(pool) = self.try_admin(ca).await {
                return pool;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!("PostgreSQL with TLS did not accept connections within 60 s");
    }

    pub async fn try_admin(&self, ca: &TestCa) -> Result<sqlx::PgPool, sqlx::Error> {
        let options = self.options(ca, "postgres", "postgres").password(POSTGRES_ADMIN_PASSWORD);
        PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await
    }
}

/// Makes `token` the password of `role`, as Azure accepts its tokens.
pub async fn set_postgres_password(admin: &sqlx::PgPool, role: &str, token: &str) {
    sqlx::raw_sql(&format!("ALTER ROLE \"{role}\" WITH LOGIN PASSWORD '{token}'"))
        .execute(admin)
        .await
        .unwrap();
}

/// Redis 7 listening only on TLS (no plain port), with a certificate of `ca`.
pub struct TlsRedis {
    _container: ContainerAsync<GenericImage>,
    pub port: u16,
}

pub async fn tls_redis(ca: &TestCa) -> TlsRedis {
    let (cert, key) = ca.server();
    let container = GenericImage::new("redis", "7-alpine")
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_copy_to("/tls/server.crt", cert.into_bytes())
        .with_copy_to("/tls/server.key", key.into_bytes())
        .with_cmd([
            "redis-server", "--port", "0", "--tls-port", "6379",
            "--tls-cert-file", "/tls/server.crt", "--tls-key-file", "/tls/server.key",
            "--tls-auth-clients", "no",
        ])
        .start()
        .await
        .expect("start Redis with TLS (is Docker running?)");
    let port = container.get_host_port_ipv4(6379).await.unwrap();
    TlsRedis { _container: container, port }
}

impl TlsRedis {
    /// `rediss://127.0.0.1:{port}`, without credentials.
    pub fn url(&self) -> String {
        format!("rediss://127.0.0.1:{}", self.port)
    }

    /// The `default` (password-less) user, trusting `ca`.
    pub async fn admin(&self, ca: &TestCa) -> redis::aio::MultiplexedConnection {
        let certificates = redis::TlsCertificates { client_tls: None, root_cert: Some(ca.pem().into_bytes()) };
        redis::Client::build_with_tls(self.url(), certificates)
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap()
    }
}

/// Makes `token` the only password of ACL user `user` (all keys, channels, commands):
/// connections opened with an older token stay, new ones need this one.
pub async fn set_redis_password(admin: &mut redis::aio::MultiplexedConnection, user: &str, token: &str) {
    redis::cmd("ACL")
        .arg("SETUSER").arg(user).arg("on").arg("resetpass").arg(format!(">{token}"))
        .arg("~*").arg("&*").arg("+@all")
        .exec_async(admin)
        .await
        .unwrap();
}
```

If `redis::TlsCertificates` is not re-exported at the crate root in 1.2.2, import it as
`redis::tls::TlsCertificates` (check `redis-1.2.2\src\lib.rs`); keep the same in later
tasks. If the first `WaitFor` matches before the server restarts, `admin` absorbs it.

- [ ] **Step 5: Run the test.**

Run: `cargo test -p push-credentials --test tls`
Expected: 1 test passes. If PostgreSQL refuses the key ("private key file has group or
world access"), `docker logs` shows it: the `install` line must leave mode 0600.

- [ ] **Step 6: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "test(credentials): add a test CA and TLS-only PostgreSQL and Redis"
```

### Task 6: `RedisTarget` and `RedisConnector`

**Files:**
- Modify: `crates\push-credentials\src\redis_connect.rs`, `crates\push-credentials\src\lib.rs`
- Test: `crates\push-credentials\tests\redis.rs`

- [ ] **Step 1: Write the failing tests** in `crates\push-credentials\tests\redis.rs`:

```rust
//! Redis with Entra tokens: TLS, a user from the token, re-authentication after
//! rotations, reconnection with a fresh token, and refusals.

use std::sync::Arc;
use std::time::Duration;

use push_credentials::testing::{IssueHook, StubIssuer, StubPrincipal, TestCa, set_redis_password, tls_redis};
use push_credentials::{AzureConfig, ConfigProblem, RedisConnector, RedisTarget, is_refused};
use redis::aio::ConnectionManagerConfig;
use tokio_util::sync::CancellationToken;

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const OID: &str = "0a1b2c3d-0000-4000-8000-000000000001";

fn lookup(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
    move |name| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| value.clone())
}

#[test]
fn targets_parse_and_entra_needs_tls_without_credentials() {
    let password = RedisTarget::from_lookup(&lookup(vec![("PUSH_REDIS_URL", "redis://:hunter2@redis:6379".into())])).unwrap();
    assert!(!format!("{password:?}").contains("hunter2"));
    assert_eq!(
        RedisTarget::from_lookup(&lookup(vec![])),
        Err(ConfigProblem::Missing("PUSH_REDIS_URL"))
    );
    let entra = |url: &str| RedisTarget::from_lookup(&lookup(vec![
        ("PUSH_REDIS_AUTH", "azure-entra".into()),
        ("PUSH_REDIS_URL", url.into()),
    ]));
    assert!(matches!(entra("rediss://cache.example.net:10000"), Ok(RedisTarget::Entra { .. })));
    for bad in ["redis://cache:10000", "rediss://user:pw@cache:10000", "rediss://:pw@cache:10000"] {
        assert!(matches!(entra(bad), Err(ConfigProblem::Invalid { name: "PUSH_REDIS_URL", .. })), "{bad}");
    }
}

struct Setup {
    _ca: TestCa,
    _server: push_credentials::testing::TlsRedis,
    stub: StubIssuer,
    connector: RedisConnector,
    admin: redis::aio::MultiplexedConnection,
}

async fn setup(lifetime: Duration, register: bool) -> Setup {
    push_credentials::install_crypto_provider();
    let ca = TestCa::new();
    let server = tls_redis(&ca).await;
    let admin = server.admin(&ca).await;
    let hook: Option<IssueHook> = register.then(|| {
        let admin = admin.clone();
        Arc::new(move |issued: push_credentials::testing::Issued| {
            let mut admin = admin.clone();
            Box::pin(async move { set_redis_password(&mut admin, &issued.object_id, &issued.token).await })
                as futures_util::future::BoxFuture<'static, ()>
        }) as IssueHook
    });
    let principals = vec![StubPrincipal { client_id: "gateway".into(), object_id: OID.into() }];
    let stub = StubIssuer::start(TENANT, "federated", principals, lifetime, hook).await;
    let file = std::env::temp_dir().join(format!("push-redis-{}.jwt", uuid::Uuid::now_v7()));
    std::fs::write(&file, "federated").unwrap();
    let azure = AzureConfig::from_lookup(&lookup(vec![
        ("AZURE_CLIENT_ID", "gateway".into()),
        ("AZURE_TENANT_ID", TENANT.into()),
        ("AZURE_FEDERATED_TOKEN_FILE", file.display().to_string()),
        ("AZURE_AUTHORITY_HOST", stub.authority().into()),
    ]))
    .unwrap();
    let target = RedisTarget::from_lookup(&lookup(vec![
        ("PUSH_REDIS_AUTH", "azure-entra".into()),
        ("PUSH_REDIS_URL", server.url()),
        ("PUSH_REDIS_CA_FILE", ca.file().display().to_string()),
    ]))
    .unwrap();
    let source = azure.source().unwrap();
    let connector = RedisConnector::open(&target, Some(&source), CancellationToken::new()).await.unwrap();
    Setup { _ca: ca, _server: server, stub, connector, admin }
}

#[tokio::test]
async fn managed_connections_and_subscribers_survive_rotations_and_kills() {
    let mut setup = setup(Duration::from_secs(2), true).await;
    let mut manager = setup.connector.connection_manager(ConnectionManagerConfig::new()).await.unwrap();
    let _: () = redis::cmd("SET").arg("k").arg("v").query_async(&mut manager).await.unwrap();
    // Three rotations: each token became the only password.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(setup.stub.issued().len() >= 3, "{}", setup.stub.issued().len());
    let value: String = redis::cmd("GET").arg("k").query_async(&mut manager).await.unwrap();
    assert_eq!(value, "v");
    // Azure closes connections whose token expired; the manager reconnects with the
    // current token.
    let _: () = redis::cmd("CLIENT").arg("KILL").arg("USER").arg(OID).query_async(&mut setup.admin).await.unwrap();
    let mut answered = None;
    for _ in 0..20 {
        if let Ok(value) = redis::cmd("GET").arg("k").query_async::<String>(&mut manager).await {
            answered = Some(value);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(answered.as_deref(), Some("v"));
    let mut pubsub = setup.connector.pubsub().await.unwrap();
    pubsub.subscribe("ch").await.unwrap();
    let _: () = redis::cmd("PUBLISH").arg("ch").arg("hi").query_async(&mut manager).await.unwrap();
    let message = futures_util::StreamExt::next(&mut pubsub.on_message()).await.unwrap();
    assert_eq!(message.get_payload::<String>().unwrap(), "hi");
    assert!(setup.connector.rotations().is_some());
}

#[tokio::test]
async fn a_principal_without_a_redis_user_is_refused() {
    let setup = setup(Duration::from_secs(3600), false).await;
    let error = setup.connector.connection_manager(ConnectionManagerConfig::new()).await.err().expect("refused");
    assert!(is_refused(&error), "{error:?}");
}
```

Add `futures-util.workspace = true`, `tokio-util.workspace = true` and
`uuid.workspace = true` to the crate's `[dev-dependencies]`.

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p push-credentials --test redis`
Expected: compile errors (`RedisTarget`, `RedisConnector`, `is_refused` missing).

- [ ] **Step 3: Implement `redis_connect.rs`.**

```rust
//! Redis with a password URL, or with Entra ID tokens (spec 2026-10-18 §6): TLS
//! (`rediss://`), the token's `oid` (or `PUSH_REDIS_USER`) as the ACL user, managed
//! connections that re-`AUTH` whenever the token changes, and subscribers opened with the
//! current token (the gateway hub replaces them when the token changes).

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::Stream;
use redis::aio::{ConnectionManager, ConnectionManagerConfig, PubSub};
use redis::auth::{BasicAuth, StreamingCredentialsProvider};
use redis::{ErrorKind, RedisError, RedisResult};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{AccessToken, AuthMode, ConfigProblem, TokenError, TokenSource, Tokens};

/// The scope of Azure Managed Redis (and Azure Cache for Redis) tokens.
pub const REDIS_SCOPE: &str = "https://redis.azure.com/.default";

/// Where Redis is and how to sign in. `Debug` never shows a password URL.
#[derive(Clone, PartialEq, Eq)]
pub enum RedisTarget {
    /// `redis://:password@host:6379` (or `rediss://`), as before.
    Password { url: String },
    /// `rediss://host:port` without credentials; the user is `user` or the token's `oid`.
    Entra { url: String, user: Option<String>, ca_file: Option<PathBuf> },
}

impl std::fmt::Debug for RedisTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password { .. } => formatter.write_str("Password { url: <redacted> }"),
            Self::Entra { url, user, ca_file } => formatter
                .debug_struct("Entra")
                .field("url", url)
                .field("user", user)
                .field("ca_file", ca_file)
                .finish(),
        }
    }
}

impl RedisTarget {
    /// Reads `PUSH_REDIS_AUTH`, `PUSH_REDIS_URL`, `PUSH_REDIS_USER`, `PUSH_REDIS_CA_FILE`.
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigProblem> {
        let get = |name: &str| lookup(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let mode = AuthMode::parse(lookup("PUSH_REDIS_AUTH").as_deref())
            .map_err(|reason| ConfigProblem::Invalid { name: "PUSH_REDIS_AUTH", reason })?;
        let url = get("PUSH_REDIS_URL").ok_or(ConfigProblem::Missing("PUSH_REDIS_URL"))?;
        let invalid = |reason: &str| ConfigProblem::Invalid { name: "PUSH_REDIS_URL", reason: reason.to_owned() };
        match mode {
            AuthMode::Password => {
                redis::Client::open(url.as_str())
                    .map_err(|_| invalid("not a valid Redis URL, expected redis://[:password@]host:6379"))?;
                Ok(Self::Password { url })
            }
            AuthMode::AzureEntra => {
                let parsed = reqwest::Url::parse(&url).map_err(|_| invalid("not a valid URL"))?;
                if parsed.scheme() != "rediss" {
                    return Err(invalid("azure-entra needs a rediss:// URL (TLS)"));
                }
                if !parsed.username().is_empty() || parsed.password().is_some() || parsed.host_str().is_none() {
                    return Err(invalid("azure-entra takes rediss://host:port without credentials"));
                }
                redis::Client::open(url.as_str()).map_err(|_| invalid("not a valid Redis URL"))?;
                Ok(Self::Entra {
                    url,
                    user: get("PUSH_REDIS_USER"),
                    ca_file: get("PUSH_REDIS_CA_FILE").map(PathBuf::from),
                })
            }
        }
    }

    pub fn mode(&self) -> AuthMode {
        match self {
            Self::Password { .. } => AuthMode::Password,
            Self::Entra { .. } => AuthMode::AzureEntra,
        }
    }
}

/// Why a connector could not be opened. Never retried: it is configuration or identity.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("{0}")]
    Config(String),
    #[error("cannot get an access token for Redis: {0}")]
    Token(#[from] TokenError),
}

/// Opens Redis connections with the target's credentials. Cheap to clone.
#[derive(Clone)]
pub struct RedisConnector {
    client: redis::Client,
    credentials: Option<Credentials>,
}

impl RedisConnector {
    /// Password mode from a URL.
    pub fn password(url: &str) -> RedisResult<Self> {
        Ok(Self { client: redis::Client::open(url)?, credentials: None })
    }

    /// The connector for `target`; in Entra mode it fetches the first token from
    /// `azure` (failing when none can be had) and keeps it fresh until `shutdown`.
    pub async fn open(
        target: &RedisTarget,
        azure: Option<&Arc<dyn TokenSource>>,
        shutdown: CancellationToken,
    ) -> Result<Self, OpenError> {
        match target {
            RedisTarget::Password { url } => {
                Self::password(url).map_err(|_| OpenError::Config("PUSH_REDIS_URL is not a valid Redis URL".to_owned()))
            }
            RedisTarget::Entra { url, user, ca_file } => {
                let source = azure.ok_or_else(|| OpenError::Config("PUSH_REDIS_AUTH=azure-entra needs the AZURE_* identity variables".to_owned()))?;
                let root_cert = match ca_file {
                    Some(file) => Some(tokio::fs::read(file).await.map_err(|error| {
                        OpenError::Config(format!("cannot read PUSH_REDIS_CA_FILE: {}", error.kind()))
                    })?),
                    None => None,
                };
                let client = match root_cert {
                    Some(pem) => redis::Client::build_with_tls(
                        url.as_str(),
                        redis::TlsCertificates { client_tls: None, root_cert: Some(pem) },
                    ),
                    None => redis::Client::open(url.as_str()),
                }
                .map_err(|_| OpenError::Config("PUSH_REDIS_URL is not a valid Redis URL".to_owned()))?;
                let tokens = Tokens::start(Arc::clone(source), REDIS_SCOPE, shutdown).await?;
                Ok(Self {
                    client,
                    credentials: Some(Credentials { tokens, user: user.as_deref().map(Arc::from) }),
                })
            }
        }
    }

    fn with_credentials(&self, config: ConnectionManagerConfig) -> ConnectionManagerConfig {
        match &self.credentials {
            Some(credentials) => config.set_credentials_provider(credentials.clone()),
            None => config,
        }
    }

    /// A self-reconnecting connection; in Entra mode it re-authenticates on every token.
    pub async fn connection_manager(&self, config: ConnectionManagerConfig) -> RedisResult<ConnectionManager> {
        self.client.get_connection_manager_with_config(self.with_credentials(config)).await
    }

    /// As [`Self::connection_manager`], connecting on first use.
    pub fn connection_manager_lazy(&self, config: ConnectionManagerConfig) -> RedisResult<ConnectionManager> {
        self.client.get_connection_manager_lazy(self.with_credentials(config))
    }

    /// A pub/sub connection signed in with the current token. A subscribed connection
    /// cannot re-authenticate: replace it when [`Self::rotations`] fires.
    pub async fn pubsub(&self) -> RedisResult<PubSub> {
        let Some(credentials) = &self.credentials else {
            return self.client.get_async_pubsub().await;
        };
        let auth = credentials.basic_auth(&credentials.tokens.current())?;
        let info = self.client.get_connection_info().clone();
        let settings = info
            .redis_settings()
            .clone()
            .set_username(auth.username())
            .set_password(auth.password());
        // The address keeps its TLS parameters (the CA), so `open` reuses them.
        redis::Client::open(info.set_redis_settings(settings))?
            .get_async_pubsub()
            .await
    }

    /// Fires on every new token in Entra mode; `None` in password mode.
    pub fn rotations(&self) -> Option<watch::Receiver<AccessToken>> {
        self.credentials.as_ref().map(|credentials| credentials.tokens.watch())
    }
}

/// Whether Redis refused the credentials (a wrong or expired token, an unknown user):
/// configuration, not a passing outage.
pub fn is_refused(error: &RedisError) -> bool {
    error.kind() == ErrorKind::AuthenticationFailed
        || matches!(error.code(), Some("WRONGPASS" | "NOPERM" | "NOAUTH"))
}

/// A Redis error that waiting will not fix in Entra mode: refused credentials or a
/// rejected server certificate. Everything else (refused connections, timeouts, resets)
/// is an outage and retried.
pub fn is_permanent_redis(error: &RedisError) -> bool {
    is_refused(error) || crate::is_certificate_error(error)
}

#[derive(Clone)]
struct Credentials {
    tokens: Tokens,
    user: Option<Arc<str>>,
}

impl Credentials {
    fn basic_auth(&self, token: &AccessToken) -> RedisResult<BasicAuth> {
        let user = match &self.user {
            Some(user) => user.to_string(),
            None => token.object_id().ok_or_else(|| {
                RedisError::from((
                    ErrorKind::AuthenticationFailed,
                    "no Redis user: set PUSH_REDIS_USER (the token has no oid claim)",
                ))
            })?,
        };
        Ok(BasicAuth::new(user, token.secret().to_owned()))
    }
}

impl StreamingCredentialsProvider for Credentials {
    /// The current credentials at once, then every new token's.
    fn subscribe(&self) -> Pin<Box<dyn Stream<Item = RedisResult<BasicAuth>> + Send + 'static>> {
        let mut receiver = self.tokens.watch();
        receiver.mark_changed();
        let credentials = self.clone();
        Box::pin(futures_util::stream::unfold(receiver, move |mut receiver| {
            let credentials = credentials.clone();
            async move {
                receiver.changed().await.ok()?;
                let token = receiver.borrow_and_update().clone();
                Some((credentials.basic_auth(&token), receiver))
            }
        }))
    }
}
```

Restore in `lib.rs`: `pub use redis_connect::{OpenError, REDIS_SCOPE, RedisConnector, RedisTarget, is_permanent_redis, is_refused};`
and add to `lib.rs`:

```rust
/// Whether `error` or one of its causes is a server certificate the client rejected (an
/// unknown issuer, a wrong name, an expired certificate): configuration, not an outage.
/// Other TLS failures (a reset or closed connection during the handshake) are not.
pub fn is_certificate_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause = Some(error);
    while let Some(current) = cause {
        if let Some(tls) = current.downcast_ref::<rustls::Error>() {
            if matches!(tls, rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented) {
                return true;
            }
        }
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            if let Some(inner) = io.get_ref() {
                if is_certificate_error(inner) {
                    return true;
                }
            }
        }
        cause = current.source();
    }
    false
}
```

with a unit test in `src\tests\mod.rs`:

```rust
#[test]
fn only_rejected_certificates_count_as_certificate_errors() {
    use rustls::{CertificateError, Error};
    let rejected = std::io::Error::new(std::io::ErrorKind::InvalidData, Error::InvalidCertificate(CertificateError::UnknownIssuer));
    assert!(crate::is_certificate_error(&rejected));
    let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset during the handshake");
    assert!(!crate::is_certificate_error(&reset));
}
```

Add to the Redis tests (Step 1) a test that a connector trusting another CA fails with a
permanent error:

```rust
#[tokio::test]
async fn an_untrusted_server_certificate_is_permanent() {
    push_credentials::install_crypto_provider();
    let (ca, other) = (TestCa::new(), TestCa::new());
    let server = tls_redis(&ca).await;
    let stub = StubIssuer::start(TENANT, "federated", vec![StubPrincipal { client_id: "gateway".into(), object_id: OID.into() }], Duration::from_secs(3600), None).await;
    let file = std::env::temp_dir().join(format!("push-redis-{}.jwt", uuid::Uuid::now_v7()));
    std::fs::write(&file, "federated").unwrap();
    let azure = AzureConfig { client_id: Some("gateway".into()), tenant_id: Some(TENANT.into()), federated_token_file: Some(file), authority_host: stub.authority().into() };
    let target = RedisTarget::Entra { url: server.url(), user: None, ca_file: Some(other.file().to_owned()) };
    let connector = RedisConnector::open(&target, Some(&azure.source().unwrap()), CancellationToken::new()).await.unwrap();
    let error = connector.connection_manager(ConnectionManagerConfig::new()).await.err().expect("refused");
    assert!(push_credentials::is_permanent_redis(&error), "{error:?}");
}
```

If redis 1.2.2 does not expose the rustls error through `source()` (check how
`redis-1.2.2\src\aio\tokio.rs` wraps the handshake error), wrap the walk: also accept an
`io::Error` of kind `InvalidData` whose message names a certificate, and say so in the
function's doc. The test decides.

If a name differs in redis 1.2.2 (for example `TlsCertificates`'s path or
`RedisError::code`), adapt to the source in
`%USERPROFILE%\.cargo\registry\src\index.crates.io-*\redis-1.2.2\src` and note it in the
commit message; do not change the behaviour.

- [ ] **Step 4: Run the tests.**

Run: `cargo test -p push-credentials --test redis`
Expected: 4 tests pass. The rotation test takes about 6 s.

- [ ] **Step 5: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "feat(credentials): connect to Redis with Entra tokens over TLS"
```

### Task 7: PostgreSQL targets and the token-rotating `Connector`

**Files:**
- Create: `crates\push-store\src\connect.rs`
- Modify: `crates\push-store\src\lib.rs`, `crates\push-store\Cargo.toml`
- Test: `crates\push-store\tests\entra.rs`

- [ ] **Step 1: Dependencies.** In `crates\push-store\Cargo.toml` add
`push-credentials.workspace = true`, `tokio = { workspace = true, features = ["macros", "rt", "sync", "time"] }`
(merge with any existing `tokio` entry) and `tracing.workspace = true` if missing; under
`[dev-dependencies]` add `push-credentials = { workspace = true, features = ["testing"] }`,
`tokio-util.workspace = true`, `futures-util.workspace = true`, `uuid.workspace = true`.

- [ ] **Step 2: Write the failing integration tests** `crates\push-store\tests\entra.rs`:

```rust
//! PostgreSQL with Entra tokens over verified TLS: rotation for new connections, an
//! unknown principal and an untrusted CA refused, the citext allow-list, and the
//! messaging schema used by a non-owner runtime role.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use push_credentials::testing::{IssueHook, Issued, StubIssuer, StubPrincipal, TestCa, set_postgres_password, tls_postgres};
use push_credentials::{AzureConfig, Tokens};
use push_domain::{ClientMsgId, DeviceId, Messaging, NoopNotifier, SendText, SystemClock, UserId};
use push_store::{Connector, EntraDatabase, PgStore, StoreOptions, grant_runtime_access, is_permanent};
use tokio_util::sync::CancellationToken;

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const ROLE: &str = "push-store-id";

struct Setup {
    ca: TestCa,
    postgres: push_credentials::testing::TlsPostgres,
    admin: sqlx::PgPool,
    stub: StubIssuer,
}

async fn setup(lifetime: Duration, register: bool) -> Setup {
    push_credentials::install_crypto_provider();
    let ca = TestCa::new();
    let postgres = tls_postgres(&ca).await;
    let admin = postgres.admin(&ca).await;
    sqlx::raw_sql(&format!("CREATE ROLE \"{ROLE}\" LOGIN; CREATE DATABASE push_messaging OWNER \"{ROLE}\""))
        .execute(&admin).await.unwrap();
    let hook: Option<IssueHook> = register.then(|| {
        let admin = admin.clone();
        Arc::new(move |issued: Issued| {
            let admin = admin.clone();
            Box::pin(async move { set_postgres_password(&admin, ROLE, &issued.token).await }) as BoxFuture<'static, ()>
        }) as IssueHook
    });
    let principals = vec![StubPrincipal { client_id: "message-svc".into(), object_id: "oid-m".into() }];
    let stub = StubIssuer::start(TENANT, "federated", principals, lifetime, hook).await;
    Setup { ca, postgres, admin, stub }
}

async fn tokens(setup: &Setup) -> Tokens {
    let file = std::env::temp_dir().join(format!("push-store-{}.jwt", uuid::Uuid::now_v7()));
    std::fs::write(&file, "federated").unwrap();
    let azure = AzureConfig {
        client_id: Some("message-svc".into()),
        tenant_id: Some(TENANT.into()),
        federated_token_file: Some(file),
        authority_host: setup.stub.authority().into(),
    };
    Tokens::start(azure.source().unwrap(), push_store::POSTGRES_SCOPE, CancellationToken::new()).await.unwrap()
}

fn database(setup: &Setup, ca: &TestCa) -> EntraDatabase {
    EntraDatabase {
        host: "127.0.0.1".into(),
        port: setup.postgres.port,
        name: "push_messaging".into(),
        user: ROLE.into(),
        ca_file: Some(ca.file().to_owned()),
    }
}

#[tokio::test]
async fn new_connections_use_the_newest_token_and_old_ones_keep_working() {
    let setup = setup(Duration::from_secs(2), true).await;
    let connector = Connector::entra(&database(&setup, &setup.ca), tokens(&setup).await);
    connector.migrate(&push_store::MIGRATOR, &[]).await.unwrap();
    let pool = connector.pool(&StoreOptions { max_connections: 2, ..StoreOptions::default() }).await.unwrap();
    let mut held = pool.acquire().await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await; // at least two rotations
    assert!(setup.stub.issued().len() >= 3);
    let one: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&mut *held).await.unwrap();
    assert_eq!(one, 1, "an old connection keeps working");
    drop(held);
    sqlx::query(&format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = '{ROLE}'"))
        .execute(&setup.admin).await.unwrap();
    // Only the newest token is the password now: a new connection must use it.
    let mut fresh = None;
    for _ in 0..20 {
        if let Ok(value) = sqlx::query_scalar::<_, i32>("SELECT 2").fetch_one(&pool).await {
            fresh = Some(value);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(fresh, Some(2));
}

#[tokio::test]
async fn an_unknown_principal_and_an_untrusted_ca_are_permanent_errors() {
    let setup = setup(Duration::from_secs(3600), false).await;
    let connector = Connector::entra(&database(&setup, &setup.ca), tokens(&setup).await);
    let refused = connector.pool(&StoreOptions::default()).await.unwrap_err();
    assert!(is_permanent(&refused), "{refused:?}");
    let message = refused.to_string();
    assert!(!message.contains(setup.stub.issued()[0].token.as_str()), "{message}");

    let other = TestCa::new();
    let untrusted = Connector::entra(&database(&setup, &other), tokens(&setup).await);
    let error = untrusted.pool(&StoreOptions::default()).await.unwrap_err();
    assert!(is_permanent(&error), "{error:?}");
}

#[tokio::test]
async fn migrations_stop_when_citext_is_not_allow_listed() {
    let setup = setup(Duration::from_secs(3600), true).await;
    sqlx::raw_sql("ALTER DATABASE push_messaging SET azure.extensions = 'PG_TRGM,PGCRYPTO'")
        .execute(&setup.admin).await.unwrap();
    let connector = Connector::entra(&database(&setup, &setup.ca), tokens(&setup).await);
    let error = connector.migrate(&push_store::MIGRATOR, &["citext"]).await.unwrap_err().to_string();
    assert!(error.contains("azure.extensions") && error.contains("CITEXT"), "{error}");
    sqlx::raw_sql("ALTER DATABASE push_messaging SET azure.extensions = 'CITEXT,PG_TRGM'")
        .execute(&setup.admin).await.unwrap();
    connector.migrate(&push_store::MIGRATOR, &["citext"]).await.unwrap();
}

/// With a migration identity, the runtime role owns nothing: the domain still works.
#[tokio::test]
async fn the_messaging_domain_runs_as_a_granted_non_owner_role() {
    let setup = setup(Duration::from_secs(3600), true).await;
    let connector = Connector::entra(&database(&setup, &setup.ca), tokens(&setup).await);
    connector.migrate(&push_store::MIGRATOR, &[]).await.unwrap();
    sqlx::raw_sql("CREATE ROLE \"push-runtime\" LOGIN PASSWORD 'runtime-password'")
        .execute(&setup.admin).await.unwrap();
    let owner = connector.single().await.unwrap();
    grant_runtime_access(&owner, "messaging", "push-runtime").await.unwrap();
    owner.close().await;
    let runtime = Connector::from_options(
        setup.postgres.options(&setup.ca, "push-runtime", "push_messaging").password("runtime-password"),
    );
    let store = PgStore::new(runtime.pool(&StoreOptions::default()).await.unwrap());
    let messaging = Messaging::new(store, SystemClock, NoopNotifier);
    let (alice, bob) = (UserId::new(), UserId::new());
    let (phone, laptop) = (DeviceId::new(), DeviceId::new());
    messaging.device_registered(phone, alice).await.unwrap();
    messaging.device_registered(laptop, bob).await.unwrap();
    messaging.contact_request(alice, bob).await.unwrap();
    messaging.contact_accept(bob, alice).await.unwrap();
    let conversation = messaging.open_direct(alice, bob).await.unwrap();
    let sent = messaging
        .send_text(SendText {
            sender: alice,
            device: phone,
            conversation,
            client_msg_id: ClientMsgId::new(),
            body: b"hello".to_vec(),
            client_ts: None,
        })
        .await
        .unwrap();
    let batch = messaging.sync(laptop, 10).await.unwrap();
    assert!(batch.entries.iter().any(|entry| entry.seq == sent.seq));
    messaging.ack(laptop, conversation, sent.seq).await.unwrap();
}
```

Add `push-domain.workspace = true` to push-store's `[dev-dependencies]` if missing. If an
entry's field in `SyncBatch` is not `seq`, use the name in `crates\push-domain\src\sync.rs`.
The password URL keeps the runtime role on a plain password: only the owner path is Entra
here, which is what the separate-migration setup looks like.

- [ ] **Step 3: Run them to see them fail.**

Run: `cargo test -p push-store --test entra`
Expected: compile errors (`Connector`, `EntraDatabase`, `grant_runtime_access`,
`is_permanent`, `POSTGRES_SCOPE` missing).

- [ ] **Step 4: Implement `connect.rs`.**

```rust
//! How a service reaches its PostgreSQL database (spec 2026-10-18 §5): a password URL, or
//! an Entra ID principal whose access token is the password. In Entra mode TLS is always
//! verified (`verify-full`, the system roots or `ca_file`), new connections take the
//! newest token, and pooled connections live at most 30 minutes.

use std::path::PathBuf;
use std::time::Duration;

use push_credentials::{AccessToken, AuthMode, ConfigProblem, Tokens};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use tokio::sync::watch;

use crate::StoreOptions;

/// The scope of Azure Database for PostgreSQL tokens.
pub const POSTGRES_SCOPE: &str = "https://ossrdbms-aad.database.windows.net/.default";
const ENTRA_MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// Where a database is and how to sign in. `Debug` never shows a password URL.
#[derive(Clone, PartialEq, Eq)]
pub enum DatabaseTarget {
    Url(String),
    Entra(EntraDatabase),
}

impl std::fmt::Debug for DatabaseTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Url(_) => formatter.write_str("Url(<redacted>)"),
            Self::Entra(database) => formatter.debug_tuple("Entra").field(database).finish(),
        }
    }
}

/// A database reached with an Entra principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntraDatabase {
    pub host: String,
    pub port: u16,
    pub name: String,
    /// The principal's PostgreSQL role ([`is_principal_name`]).
    pub user: String,
    pub ca_file: Option<PathBuf>,
}

/// The variable names of one database connection.
#[derive(Debug, Clone, Copy)]
pub struct DatabaseVars {
    pub url: &'static str,
    pub host: &'static str,
    pub port: &'static str,
    pub name: &'static str,
    pub user: &'static str,
    pub ca_file: &'static str,
    pub default_name: &'static str,
}

impl DatabaseVars {
    /// A service's own database: `PUSH_DATABASE_*`.
    pub const fn own(default_name: &'static str) -> Self {
        Self {
            url: "PUSH_DATABASE_URL",
            host: "PUSH_DATABASE_HOST",
            port: "PUSH_DATABASE_PORT",
            name: "PUSH_DATABASE_NAME",
            user: "PUSH_DATABASE_USER",
            ca_file: "PUSH_DATABASE_CA_FILE",
            default_name,
        }
    }

    /// The worker's connection to auth-svc's database: `PUSH_AUTH_DATABASE_*`.
    pub const AUTH_FOR_WORKER: Self = Self {
        url: "PUSH_AUTH_DATABASE_URL",
        host: "PUSH_AUTH_DATABASE_HOST",
        port: "PUSH_AUTH_DATABASE_PORT",
        name: "PUSH_AUTH_DATABASE_NAME",
        user: "PUSH_AUTH_DATABASE_USER",
        ca_file: "PUSH_AUTH_DATABASE_CA_FILE",
        default_name: "push_auth",
    };
}

/// `PUSH_POSTGRES_AUTH`.
pub fn postgres_auth(lookup: &dyn Fn(&str) -> Option<String>) -> Result<AuthMode, ConfigProblem> {
    AuthMode::parse(lookup("PUSH_POSTGRES_AUTH").as_deref())
        .map_err(|reason| ConfigProblem::Invalid { name: "PUSH_POSTGRES_AUTH", reason })
}

impl DatabaseTarget {
    /// Reads one database's variables. In Entra mode host, port, user and CA file default
    /// to `fallback`'s (the worker's own database, for its second connection).
    pub fn from_lookup(
        lookup: &dyn Fn(&str) -> Option<String>,
        mode: AuthMode,
        vars: &DatabaseVars,
        fallback: Option<&EntraDatabase>,
    ) -> Result<Self, ConfigProblem> {
        let get = |name: &str| lookup(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        if mode == AuthMode::Password {
            let url = get(vars.url).ok_or(ConfigProblem::Missing(vars.url))?;
            crate::check_database_url(&url).map_err(|reason| ConfigProblem::Invalid { name: vars.url, reason })?;
            return Ok(Self::Url(url));
        }
        if get(vars.url).is_some() {
            return Err(ConfigProblem::Invalid {
                name: vars.url,
                reason: "not used with PUSH_POSTGRES_AUTH=azure-entra: unset it and set the host, name and user".to_owned(),
            });
        }
        let host = get(vars.host)
            .or_else(|| fallback.map(|f| f.host.clone()))
            .ok_or(ConfigProblem::Missing(vars.host))?;
        let port = match get(vars.port) {
            Some(port) => port.parse::<u16>().ok().filter(|port| *port > 0).ok_or_else(|| {
                ConfigProblem::Invalid { name: vars.port, reason: "expected a port number".to_owned() }
            })?,
            None => fallback.map_or(5432, |f| f.port),
        };
        let name = get(vars.name).unwrap_or_else(|| vars.default_name.to_owned());
        if !is_principal_name(&name) {
            return Err(ConfigProblem::Invalid { name: vars.name, reason: "not a database name".to_owned() });
        }
        let user = get(vars.user)
            .or_else(|| fallback.map(|f| f.user.clone()))
            .ok_or(ConfigProblem::Missing(vars.user))?;
        if !is_principal_name(&user) {
            return Err(ConfigProblem::Invalid {
                name: vars.user,
                reason: "expected 1 to 63 ASCII letters, digits, _ - . or @".to_owned(),
            });
        }
        let ca_file = get(vars.ca_file).map(PathBuf::from).or_else(|| fallback.and_then(|f| f.ca_file.clone()));
        Ok(Self::Entra(EntraDatabase { host, port, name, user, ca_file }))
    }

    pub fn mode(&self) -> AuthMode {
        match self {
            Self::Url(_) => AuthMode::Password,
            Self::Entra(_) => AuthMode::AzureEntra,
        }
    }
}

/// Opens pools and migration connections for one database. Cheap to clone.
#[derive(Clone)]
pub struct Connector {
    base: PgConnectOptions,
    tokens: Option<Tokens>,
}

impl Connector {
    pub fn from_url(url: &str) -> Result<Self, sqlx::Error> {
        Ok(Self { base: url.parse()?, tokens: None })
    }

    /// Fixed options (a password role): tests and tools.
    pub fn from_options(options: PgConnectOptions) -> Self {
        Self { base: options, tokens: None }
    }

    /// `database` signed in with `tokens` (scope [`POSTGRES_SCOPE`]).
    pub fn entra(database: &EntraDatabase, tokens: Tokens) -> Self {
        let mut base = PgConnectOptions::new_without_pgpass()
            .host(&database.host)
            .port(database.port)
            .database(&database.name)
            .username(&database.user)
            .ssl_mode(PgSslMode::VerifyFull);
        if let Some(ca_file) = &database.ca_file {
            base = base.ssl_root_cert(ca_file);
        }
        Self { base, tokens: Some(tokens) }
    }

    /// The connector for `target`; Entra targets need `tokens`.
    pub fn for_target(target: &DatabaseTarget, tokens: Option<&Tokens>) -> Result<Self, sqlx::Error> {
        match (target, tokens) {
            (DatabaseTarget::Url(url), _) => Self::from_url(url),
            (DatabaseTarget::Entra(database), Some(tokens)) => Ok(Self::entra(database, tokens.clone())),
            (DatabaseTarget::Entra(_), None) => Err(sqlx::Error::Configuration(
                "PUSH_POSTGRES_AUTH=azure-entra needs an access token".into(),
            )),
        }
    }

    pub fn is_entra(&self) -> bool {
        self.tokens.is_some()
    }

    /// The options of a new connection: in Entra mode the newest token is the password.
    pub fn options(&self) -> PgConnectOptions {
        match &self.tokens {
            Some(tokens) => self.base.clone().password(tokens.current().secret()),
            None => self.base.clone(),
        }
    }

    /// A request pool as [`crate::connect_pool`] describes; in Entra mode its connect
    /// options follow every new token until the pool closes.
    pub async fn pool(&self, store: &StoreOptions) -> Result<PgPool, sqlx::Error> {
        use sqlx::Executor;

        store.validate().map_err(|message| sqlx::Error::Configuration(message.into()))?;
        let settings = store.session_settings();
        let mut options = PgPoolOptions::new()
            .max_connections(store.max_connections)
            .acquire_timeout(store.acquire_timeout)
            .after_connect(move |connection, _| {
                let settings = settings.clone();
                Box::pin(async move {
                    connection.execute(settings.as_str()).await?;
                    Ok(())
                })
            });
        if self.is_entra() {
            options = options.max_lifetime(ENTRA_MAX_LIFETIME);
        }
        let pool = options.connect_with(self.options()).await?;
        if let Some(tokens) = &self.tokens {
            tokio::spawn(rotate(pool.clone(), self.base.clone(), tokens.watch()));
        }
        Ok(pool)
    }

    /// One connection without the request timeouts, for migrations and grants.
    pub async fn single(&self) -> Result<PgPool, sqlx::Error> {
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(self.options())
            .await
    }

    /// Applies `migrator`'s pending migrations on a connection of its own, after checking
    /// that each of `extensions` is allow-listed where the server has an allow-list
    /// (Azure's `azure.extensions`).
    pub async fn migrate(
        &self,
        migrator: &sqlx::migrate::Migrator,
        extensions: &[&str],
    ) -> Result<(), sqlx::migrate::MigrateError> {
        let pool = self.single().await?;
        let result = async {
            for extension in extensions {
                check_allow_listed(&pool, extension).await?;
            }
            migrator.run(&pool).await
        }
        .await;
        pool.close().await;
        result
    }
}

async fn rotate(pool: PgPool, base: PgConnectOptions, mut tokens: watch::Receiver<AccessToken>) {
    loop {
        tokio::select! {
            () = pool.close_event() => return,
            changed = tokens.changed() => if changed.is_err() { return },
        }
        let token = tokens.borrow_and_update().clone();
        pool.set_connect_options(base.clone().password(token.secret()));
    }
}

/// Fails with a clear message when the server allow-lists extensions (Azure:
/// `azure.extensions`) and `extension` is not on the list.
pub async fn check_allow_listed(pool: &PgPool, extension: &str) -> Result<(), sqlx::Error> {
    let list: Option<String> = sqlx::query_scalar("SELECT current_setting('azure.extensions', true)")
        .fetch_one(pool)
        .await?;
    match list {
        Some(list) if !allow_listed(&list, extension) => Err(sqlx::Error::Configuration(
            format!(
                "the {extension} extension is not allow-listed: add {} to the server parameter azure.extensions",
                extension.to_ascii_uppercase()
            )
            .into(),
        )),
        _ => Ok(()),
    }
}

fn allow_listed(list: &str, extension: &str) -> bool {
    list.split(',').any(|entry| entry.trim().eq_ignore_ascii_case(extension))
}

/// Grants `role` the use of everything in `schema` (`USAGE`; `SELECT, INSERT, UPDATE,
/// DELETE` on its tables; `USAGE, SELECT, UPDATE` on its sequences; `EXECUTE` on its
/// functions), for a runtime identity that does not own the schema. Run it as the owner
/// after every migration. Idempotent.
pub async fn grant_runtime_access(pool: &PgPool, schema: &str, role: &str) -> Result<(), sqlx::Error> {
    if !is_principal_name(role) || !schema.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return Err(sqlx::Error::Configuration("not a valid role or schema name".into()));
    }
    let grants = format!(
        "GRANT USAGE ON SCHEMA {schema} TO \"{role}\"; \
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {schema} TO \"{role}\"; \
         GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA {schema} TO \"{role}\"; \
         GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA {schema} TO \"{role}\""
    );
    sqlx::raw_sql(&grants).execute(pool).await.map(drop)
}

/// A role name safe to quote into SQL: 1 to 63 bytes of ASCII letters, digits, `_`, `-`,
/// `.` or `@` (Entra principals: managed identity names, groups, UPNs), and none of
/// `public`, `none` or `pg_*`, which PostgreSQL reserves.
pub fn is_principal_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@'))
        && !matches!(lower.as_str(), "public" | "none")
        && !lower.starts_with("pg_")
}

/// Whether a connection or migration error is configuration rather than an outage: a
/// refused password or token (SQLSTATE 28P01, 28000), a rejected server certificate, or a
/// configuration error such as a missing allow-listed extension. Other TLS failures (a
/// handshake cut short) stay retryable. Services stop on these in Entra mode.
pub fn is_permanent(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Configuration(_) => true,
        sqlx::Error::Tls(inner) => push_credentials::is_certificate_error(inner.as_ref()),
        error => error
            .as_database_error()
            .and_then(|error| error.code())
            .is_some_and(|code| code == "28P01" || code == "28000"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
    }

    #[test]
    fn password_mode_reads_the_url_and_entra_mode_the_parts() {
        let own = DatabaseVars::own("push_messaging");
        let url = DatabaseTarget::from_lookup(&lookup(&[("PUSH_DATABASE_URL", "postgres://u:hunter2@db/x")]), AuthMode::Password, &own, None).unwrap();
        assert_eq!(format!("{url:?}"), "Url(<redacted>)");
        let entra = DatabaseTarget::from_lookup(
            &lookup(&[("PUSH_DATABASE_HOST", "pg.example.net"), ("PUSH_DATABASE_USER", "push-message-svc-id")]),
            AuthMode::AzureEntra, &own, None,
        )
        .unwrap();
        assert_eq!(entra, DatabaseTarget::Entra(EntraDatabase {
            host: "pg.example.net".into(), port: 5432, name: "push_messaging".into(),
            user: "push-message-svc-id".into(), ca_file: None,
        }));
        assert_eq!(
            DatabaseTarget::from_lookup(&lookup(&[("PUSH_DATABASE_HOST", "h")]), AuthMode::AzureEntra, &own, None),
            Err(ConfigProblem::Missing("PUSH_DATABASE_USER"))
        );
        assert!(matches!(
            DatabaseTarget::from_lookup(&lookup(&[("PUSH_DATABASE_URL", "postgres://u:hunter2@db/x")]), AuthMode::AzureEntra, &own, None),
            Err(ConfigProblem::Invalid { name: "PUSH_DATABASE_URL", .. })
        ));
    }

    #[test]
    fn the_workers_auth_database_defaults_to_its_own_server_and_principal() {
        let own = EntraDatabase { host: "pg".into(), port: 6432, name: "push_worker".into(), user: "push-worker-id".into(), ca_file: None };
        let auth = DatabaseTarget::from_lookup(&lookup(&[]), AuthMode::AzureEntra, &DatabaseVars::AUTH_FOR_WORKER, Some(&own)).unwrap();
        assert_eq!(auth, DatabaseTarget::Entra(EntraDatabase { name: "push_auth".into(), ..own }));
    }

    #[test]
    fn principal_names_allow_entra_characters_only() {
        for name in ["push_worker", "push-auth-id", "Push.App@contoso.com", "a"] {
            assert!(is_principal_name(name), "{name}");
        }
        for name in ["", "public", "PUBLIC", "none", "pg_monitor", "a\"b", "a b", "a;b", &"x".repeat(64)] {
            assert!(!is_principal_name(name), "{name}");
        }
    }

    #[test]
    fn the_allow_list_is_comma_separated_and_case_insensitive() {
        assert!(allow_listed("PG_TRGM, CITEXT", "citext"));
        assert!(!allow_listed("PG_TRGM", "citext"));
        assert!(!allow_listed("", "citext"));
    }
}
```

- [ ] **Step 5: Delegate the URL functions.** In `crates\push-store\src\lib.rs` add
`mod connect;` and

```rust
pub use connect::{
    Connector, DatabaseTarget, DatabaseVars, EntraDatabase, POSTGRES_SCOPE, check_allow_listed,
    grant_runtime_access, is_permanent, is_principal_name, postgres_auth,
};
```

and replace the bodies of `run_migrations` and `connect_pool` (keep their docs and
signatures):

```rust
pub async fn run_migrations(
    url: &str,
    migrator: &sqlx::migrate::Migrator,
) -> Result<(), sqlx::migrate::MigrateError> {
    Connector::from_url(url)?.migrate(migrator, &[]).await
}

pub async fn connect_pool(url: &str, options: &StoreOptions) -> Result<sqlx::PgPool, sqlx::Error> {
    Connector::from_url(url)?.pool(options).await
}
```

- [ ] **Step 6: Run the tests.**

Run: `cargo test -p push-store --locked`
Expected: the new unit tests, the 4 tests of `tests\entra.rs` and every existing
push-store test pass (the conformance suite still connects with plain URLs, now with TLS
compiled in and `sslmode=prefer` falling back to plaintext as before).

- [ ] **Step 7: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "feat(store): connect to PostgreSQL with rotating Entra tokens over verified TLS"
```

### Task 8: Start-up helpers: identity selection, token start, permanent errors, retry

The four services connect the same way; these helpers keep that in one place.

**Files:**
- Modify: `crates\push-credentials\src\startup.rs`, `crates\push-credentials\src\lib.rs`, `crates\push-credentials\Cargo.toml`
- Modify: `crates\push-store\src\connect.rs`, `crates\push-store\src\lib.rs`, `crates\push-store\Cargo.toml`

- [ ] **Step 1: Write the failing unit tests** at the end of `startup.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn retry_retries_outages_stops_on_permanent_errors_and_on_shutdown() {
        let calls = Arc::new(AtomicUsize::new(0));
        let attempts = {
            let calls = Arc::clone(&calls);
            move || {
                let calls = Arc::clone(&calls);
                async move {
                    match calls.fetch_add(1, Ordering::SeqCst) {
                        0 | 1 => Err(anyhow::anyhow!("outage")),
                        _ => Ok(7),
                    }
                }
            }
        };
        let shutdown = CancellationToken::new();
        let never = |_: &anyhow::Error| false;
        let value = retry("db", Duration::from_secs(2), &shutdown, &never, attempts).await.unwrap();
        assert_eq!((value, calls.load(Ordering::SeqCst)), (Some(7), 3));

        let always = |_: &anyhow::Error| true;
        let failed = retry("db", Duration::from_secs(2), &shutdown, &always, || async {
            Err::<(), _>(anyhow::anyhow!("refused"))
        })
        .await;
        assert_eq!(format!("{:#}", failed.unwrap_err()), "db: refused");

        shutdown.cancel();
        let stopped = retry("db", Duration::from_secs(2), &shutdown, &never, || async {
            Err::<(), _>(anyhow::anyhow!("outage"))
        })
        .await;
        assert!(stopped.unwrap().is_none());
    }

    #[test]
    fn the_identity_is_read_only_when_a_datastore_uses_entra() {
        let lookup = |_: &str| None;
        assert_eq!(azure_for(&lookup, &[AuthMode::Password, AuthMode::Password]), Ok(None));
        let config = azure_for(&lookup, &[AuthMode::Password, AuthMode::AzureEntra]).unwrap().unwrap();
        assert!(config.federated_token_file.is_none());
    }
}
```

Add `anyhow.workspace = true` to `crates\push-credentials\Cargo.toml`.

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p push-credentials --lib startup`
Expected: compile errors (`retry`, `azure_for` missing).

- [ ] **Step 3: Implement `startup.rs`** (above the tests):

```rust
//! Start-up helpers shared by the services.

use std::future::Future;
use std::time::Duration;

use ::redis::RedisError;
use tokio_util::sync::CancellationToken;

use crate::{AuthMode, AzureConfig, ConfigProblem, is_permanent_redis};

/// The identity variables, read only when one of `modes` is `azure-entra`.
pub fn azure_for(
    lookup: &dyn Fn(&str) -> Option<String>,
    modes: &[AuthMode],
) -> Result<Option<AzureConfig>, ConfigProblem> {
    if modes.contains(&AuthMode::AzureEntra) {
        AzureConfig::from_lookup(lookup).map(Some)
    } else {
        Ok(None)
    }
}

/// Whether `error`'s chain holds a Redis error that waiting will not fix
/// ([`crate::is_permanent_redis`]).
pub fn redis_permanent(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.downcast_ref::<RedisError>().is_some_and(is_permanent_redis))
}

/// Calls `connect` until it succeeds, logging each failure, and waits `delay` between
/// attempts. `Ok(None)` if `shutdown` came first; `Err` at once for an error `permanent`
/// recognises (a refused token, an untrusted certificate: configuration, not an outage).
pub async fn retry<T, F, Fut>(
    what: &str,
    delay: Duration,
    shutdown: &CancellationToken,
    permanent: &dyn Fn(&anyhow::Error) -> bool,
    connect: F,
) -> anyhow::Result<Option<T>>
where
    F: Fn() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    loop {
        let attempt = tokio::select! {
            () = shutdown.cancelled() => return Ok(None),
            attempt = connect() => attempt,
        };
        match attempt {
            Ok(value) => {
                tracing::info!(dependency = what, "connected");
                return Ok(Some(value));
            }
            Err(error) if permanent(&error) => {
                tracing::error!(dependency = what, error = %format!("{error:#}"), "refused; not retrying");
                return Err(error.context(what.to_owned()));
            }
            Err(error) => {
                tracing::warn!(dependency = what, error = %format!("{error:#}"), "not reachable yet; retrying");
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => return Ok(None),
            () = tokio::time::sleep(delay) => {}
        }
    }
}
```

Add `pub use startup::azure_for;` to `lib.rs`. (`{:#}` of the error reads `db: refused`:
the context comes first.)

- [ ] **Step 4: Token start and the PostgreSQL side of "permanent"** in
`crates\push-store\src\connect.rs` (add `anyhow.workspace = true`,
`tokio-util.workspace = true` to push-store's dependencies):

```rust
/// The PostgreSQL tokens a target needs: `None` for a password URL; in Entra mode the
/// first token from `azure` (failing when none can be had), refreshed until `shutdown`.
pub async fn tokens_for(
    target: &DatabaseTarget,
    azure: Option<&std::sync::Arc<dyn push_credentials::TokenSource>>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<Option<Tokens>, push_credentials::TokenError> {
    match target {
        DatabaseTarget::Url(_) => Ok(None),
        DatabaseTarget::Entra(_) => {
            let source = azure.ok_or_else(|| {
                push_credentials::TokenError::Config(
                    "PUSH_POSTGRES_AUTH=azure-entra needs the AZURE_* identity variables".to_owned(),
                )
            })?;
            Tokens::start(std::sync::Arc::clone(source), POSTGRES_SCOPE, shutdown).await.map(Some)
        }
    }
}

/// [`is_permanent`] anywhere in `error`'s chain, also inside a migration error; a
/// migration refused for lack of privilege (42501) is permanent too: the migrating
/// identity is not the database's owner.
pub fn is_permanent_startup(error: &anyhow::Error) -> bool {
    use sqlx::migrate::MigrateError;
    let in_migration = |error: &sqlx::Error| {
        is_permanent(error)
            || error.as_database_error().and_then(|error| error.code()).is_some_and(|code| code == "42501")
    };
    error.chain().any(|cause| {
        cause.downcast_ref::<sqlx::Error>().is_some_and(is_permanent)
            || match cause.downcast_ref::<MigrateError>() {
                Some(MigrateError::Execute(error)) | Some(MigrateError::ExecuteMigration(error, _)) => {
                    in_migration(error)
                }
                _ => false,
            }
    })
}
```

Export both from `lib.rs` (`tokens_for`, `is_permanent_startup`).

- [ ] **Step 5: Run.**

Run: `cargo test -p push-credentials --lib; cargo build -p push-store`
Expected: the startup tests pass; push-store builds.

- [ ] **Step 6: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "feat(credentials): share start-up retry and identity selection"
```

### Task 9: message-svc: targets, connectors, `PUSH_RUN_MIGRATIONS`, `migrate`

**Files:**
- Modify: `crates\push-message-svc\Cargo.toml`, `src\config.rs`, `src\server.rs`, `src\notify.rs`, `src\lib.rs`, `src\main.rs`
- Modify (struct literals): `crates\push-message-svc\tests\service.rs`, `crates\push-smoke\tests\local.rs`

- [ ] **Step 1: Write the failing config tests** in `src\config.rs`'s test module
(replace the assertions that read `config.database_url` / `config.redis_url`: they now
compare `config.database` with `DatabaseTarget::Url(...)` and `config.redis` with
`RedisTarget::Password { url: ... }`), and add:

```rust
#[test]
fn entra_mode_reads_the_parts_and_the_identity() {
    let variables = [
        ("PUSH_POSTGRES_AUTH", "azure-entra"),
        ("PUSH_DATABASE_HOST", "pg.example.net"),
        ("PUSH_DATABASE_USER", "push-message-svc-id"),
        ("PUSH_REDIS_AUTH", "azure-entra"),
        ("PUSH_REDIS_URL", "rediss://cache.example.net:10000"),
        ("AZURE_CLIENT_ID", "client"),
        ("AZURE_TENANT_ID", "tenant"),
        ("AZURE_FEDERATED_TOKEN_FILE", "/var/run/secrets/azure/tokens/azure-identity-token"),
        ("PUSH_RUN_MIGRATIONS", "false"),
    ];
    let config = Config::from_lookup(|name| {
        variables.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())
    })
    .unwrap();
    assert!(matches!(config.database, DatabaseTarget::Entra(ref database) if database.name == "push_messaging"));
    assert!(matches!(config.redis, RedisTarget::Entra { .. }));
    assert!(config.azure.is_some());
    assert!(!config.run_migrations);
    let defaults = config_with(&REQUIRED);
    assert!(defaults.run_migrations && defaults.azure.is_none());
}

#[test]
fn run_migrations_is_a_boolean() {
    let mut variables = REQUIRED.to_vec();
    variables.push(("PUSH_RUN_MIGRATIONS", "maybe"));
    assert!(matches!(config(&variables), Err(ConfigError::Invalid { name: "PUSH_RUN_MIGRATIONS", .. })));
}
```

Use the module's existing `REQUIRED` and `config` helpers; if there is no `config_with`,
call `config(&REQUIRED).unwrap()`.

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p push-message-svc --lib config`
Expected: compile errors.

- [ ] **Step 3: Dependencies.** Add `push-credentials.workspace = true` to
`crates\push-message-svc\Cargo.toml`.

- [ ] **Step 4: `config.rs`.** Document the new variables in the table:

```rust
/// | `PUSH_POSTGRES_AUTH` | `password` or `azure-entra` (spec 2026-10-18) | `password` |
/// | `PUSH_DATABASE_URL` | password mode: Postgres URL of the `push_messaging` database, as its owner | required |
/// | `PUSH_DATABASE_HOST`, `_PORT`, `_NAME`, `_USER`, `_CA_FILE` | Entra mode: the server, port (`5432`), database (`push_messaging`), the principal's role and an optional CA bundle | host and user required |
/// | `PUSH_REDIS_AUTH` | `password` or `azure-entra` | `password` |
/// | `PUSH_REDIS_URL` | password mode `redis://:password@host:6379`; Entra mode `rediss://host:port` | required |
/// | `PUSH_REDIS_USER`, `PUSH_REDIS_CA_FILE` | Entra mode: the ACL user (default: the token's `oid`) and an optional CA bundle | none |
/// | `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_FEDERATED_TOKEN_FILE`, `AZURE_AUTHORITY_HOST` | Entra mode: the identity (workload identity with the token file, else managed identity) | set by the AKS webhook |
/// | `PUSH_RUN_MIGRATIONS` | `true` or `false`: migrate at start-up (else a `migrate` job does) | `true` |
```

Replace the fields `database_url` and `redis_url` with:

```rust
    pub database: DatabaseTarget,
    pub redis: RedisTarget,
    /// The identity, when a datastore uses Entra ID.
    pub azure: Option<AzureConfig>,
    pub run_migrations: bool,
```

In `Debug`, replace the two redacted fields with
`.field("database", &self.database).field("redis", &self.redis).field("azure", &self.azure).field("run_migrations", &self.run_migrations)`
(both targets redact password URLs themselves).

In `from_lookup`, replace everything from `let database_url = ...` through the Redis URL
check with:

```rust
        let lookup_dyn: &dyn Fn(&str) -> Option<String> = &lookup;
        let mode = postgres_auth(lookup_dyn).map_err(problem)?;
        let database = DatabaseTarget::from_lookup(lookup_dyn, mode, &DatabaseVars::own("push_messaging"), None)
            .map_err(problem)?;
        let redis = RedisTarget::from_lookup(lookup_dyn).map_err(problem)?;
        let azure = azure_for(lookup_dyn, &[database.mode(), redis.mode()]).map_err(problem)?;
        let run_migrations = flag(&lookup, "PUSH_RUN_MIGRATIONS", true)?;
```

and build `Self { database, redis, azure, run_migrations, ... }`. Add the helpers next to
`redacted`:

```rust
/// A datastore or identity variable that is missing or invalid; values are never shown.
fn problem(problem: ConfigProblem) -> ConfigError {
    match problem {
        ConfigProblem::Missing(name) => ConfigError::Missing(name),
        ConfigProblem::Invalid { name, reason } => redacted(name, reason),
    }
}

/// `true` or `false`; `default` when unset or empty.
fn flag(lookup: &impl Fn(&str) -> Option<String>, name: &'static str, default: bool) -> Result<bool, ConfigError> {
    match lookup(name).as_deref().map(str::trim) {
        None | Some("") => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(other) => Err(ConfigError::Invalid { name, value: other.to_owned(), reason: "expected true or false".to_owned() }),
    }
}
```

Imports: `use push_credentials::{AzureConfig, ConfigProblem, RedisTarget, azure_for};` and
`use push_store::{DatabaseTarget, DatabaseVars, StoreOptions, postgres_auth};`
(`check_database_url` is no longer used here).

Add the `migrate` subcommand's configuration below `Config`:

```rust
/// What `push-message-svc migrate` reads: the database variables above, the identity,
/// and `PUSH_DATABASE_RUNTIME_ROLE`, a role granted the use of the `messaging` schema
/// when the migrating identity is not the one the service runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateConfig {
    pub database: DatabaseTarget,
    pub azure: Option<AzureConfig>,
    pub runtime_role: Option<String>,
}

impl MigrateConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let lookup_dyn: &dyn Fn(&str) -> Option<String> = &lookup;
        let mode = postgres_auth(lookup_dyn).map_err(problem)?;
        let database = DatabaseTarget::from_lookup(lookup_dyn, mode, &DatabaseVars::own("push_messaging"), None)
            .map_err(problem)?;
        let azure = azure_for(lookup_dyn, &[database.mode()]).map_err(problem)?;
        let runtime_role = runtime_role(&lookup)?;
        Ok(Self { database, azure, runtime_role })
    }
}

/// `PUSH_DATABASE_RUNTIME_ROLE`: a principal name, or unset.
fn runtime_role(lookup: &impl Fn(&str) -> Option<String>) -> Result<Option<String>, ConfigError> {
    match lookup("PUSH_DATABASE_RUNTIME_ROLE").filter(|role| !role.trim().is_empty()) {
        None => Ok(None),
        Some(role) if push_store::is_principal_name(role.trim()) => Ok(Some(role.trim().to_owned())),
        Some(role) => Err(ConfigError::Invalid {
            name: "PUSH_DATABASE_RUNTIME_ROLE",
            value: role,
            reason: "expected 1 to 63 ASCII letters, digits, _ - . or @".to_owned(),
        }),
    }
}
```

- [ ] **Step 5: `notify.rs`.** Add beside `connect`:

```rust
    /// Connects through `connector` (password or Entra ID); reconnects by itself.
    pub async fn with_connector(connector: &push_credentials::RedisConnector) -> redis::RedisResult<Self> {
        let connection = connector
            .connection_manager(redis::aio::ConnectionManagerConfig::new())
            .await?;
        Ok(Self { connection })
    }
```

and make `connect(url)` call `Self::with_connector(&push_credentials::RedisConnector::password(url)?).await`.

- [ ] **Step 6: `server.rs`.** Delete the local `retry` function and `use std::future::Future;`.
Add imports:

```rust
use push_credentials::startup::{redis_permanent, retry};
use push_credentials::{AuthMode, AzureConfig, RedisConnector};
use push_store::{Connector, PgStore, grant_runtime_access, is_permanent_startup, tokens_for};
use crate::config::MigrateConfig;
```

(drop `migrate_database` from the push_store import). Replace the Postgres and Redis
blocks of `serve` (from `let Some(store) = retry(` through the notifier's `else { ... };`)
with:

```rust
    push_credentials::install_crypto_provider();
    let azure = config.azure.as_ref().map(AzureConfig::source).transpose().context("the Azure identity")?;
    let pg_tokens = tokens_for(&config.database, azure.as_ref(), shutdown.clone())
        .await
        .context("PostgreSQL credentials")?;
    let connector = Connector::for_target(&config.database, pg_tokens.as_ref())?;
    let redis = RedisConnector::open(&config.redis, azure.as_ref(), shutdown.clone())
        .await
        .context("Redis credentials")?;
    // In Entra mode a refused token or certificate is configuration: stop, do not wait.
    // Per datastore: a refused token or certificate is configuration in Entra mode only,
    // so a password-mode datastore keeps retrying exactly as before.
    let pg_entra = config.database.mode() == AuthMode::AzureEntra;
    let postgres_fatal = move |error: &anyhow::Error| pg_entra && is_permanent_startup(error);
    let redis_entra = config.redis.mode() == AuthMode::AzureEntra;
    let redis_fatal = move |error: &anyhow::Error| redis_entra && redis_permanent(error);
    let Some(store) = retry("postgres", RETRY_DELAY, &shutdown, &postgres_fatal, || async {
        // Migrations run on a connection of their own, not on the request pool, whose
        // statement and lock timeouts are sized for requests.
        if config.run_migrations {
            connector.migrate(&push_store::MIGRATOR, &[]).await?;
        }
        anyhow::Ok(PgStore::new(connector.pool(&config.store).await?))
    })
    .await?
    else {
        return finish(health_server).await;
    };
    let Some(notifier) = retry("redis", RETRY_DELAY, &shutdown, &redis_fatal, || async {
        anyhow::Ok(RedisNotifier::with_connector(&redis).await?)
    })
    .await?
    else {
        store.close().await;
        return finish(health_server).await;
    };
```

Add the subcommand's body at the end of the file (before tests, if any):

```rust
/// `push-message-svc migrate`: applies the migrations of `push_messaging` and, with
/// `PUSH_DATABASE_RUNTIME_ROLE`, grants that role the use of the `messaging` schema, then
/// returns. For a migration job whose identity owns the database while the service runs
/// as another (`PUSH_RUN_MIGRATIONS=false`).
pub async fn migrate(config: MigrateConfig) -> anyhow::Result<()> {
    push_credentials::install_crypto_provider();
    let shutdown = CancellationToken::new();
    let azure = config.azure.as_ref().map(AzureConfig::source).transpose().context("the Azure identity")?;
    let tokens = tokens_for(&config.database, azure.as_ref(), shutdown.clone())
        .await
        .context("PostgreSQL credentials")?;
    let connector = Connector::for_target(&config.database, tokens.as_ref())?;
    connector.migrate(&push_store::MIGRATOR, &[]).await?;
    if let Some(role) = &config.runtime_role {
        let pool = connector.single().await?;
        let granted = grant_runtime_access(&pool, "messaging", role).await;
        pool.close().await;
        granted?;
    }
    shutdown.cancel();
    tracing::info!("push_messaging is migrated");
    Ok(())
}
```

- [ ] **Step 7: `lib.rs` and `main.rs`.** Export `pub use config::{Config, ConfigError, MigrateConfig};`
and `pub use server::{init_logging, migrate, serve, terminated};`. In `main.rs`, right
after `init_logging();`:

```rust
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        return push_message_svc::migrate(push_message_svc::MigrateConfig::from_env()?).await;
    }
```

and replace the comment above `main` with
`// `serve` migrates at start-up unless PUSH_RUN_MIGRATIONS=false; `push-message-svc migrate` only migrates.`

- [ ] **Step 8: Struct literals.** Where tests build `push_message_svc::Config { ... }`
(`crates\push-message-svc\tests\service.rs` and `crates\push-smoke\tests\local.rs`),
replace `database_url: x, redis_url: y,` with

```rust
                database: push_store::DatabaseTarget::Url(x),
                redis: push_credentials::RedisTarget::Password { url: y },
                azure: None,
                run_migrations: true,
```

adding `push-credentials.workspace = true` to those crates' `[dev-dependencies]`.

- [ ] **Step 9: Run.**

Run: `cargo test -p push-message-svc --locked; cargo test -p push-smoke --test local --locked`
Expected: everything passes (password mode is unchanged).

- [ ] **Step 10: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "feat(message-svc): connect with Entra ID tokens; add migrate and PUSH_RUN_MIGRATIONS"
```

### Task 10: auth-svc: targets, connectors, principal names, `migrate`

**Files:**
- Modify: `crates\push-auth\Cargo.toml`, `src\config.rs`, `src\server.rs`, `src\db.rs`, `src\limits.rs`, `src\lib.rs`, `src\main.rs`
- Modify (struct literals and lookups): `crates\push-auth\tests\server.rs`, any test reading `config.database_url`

- [ ] **Step 1: Failing tests.** In `src\config.rs`'s tests: change the reads of
`config.database_url` / `config.redis_url` to the targets as in Task 9; keep the test
that lists refused `PUSH_AUTH_WORKER_ROLE` values unchanged (password mode keeps
`is_role_name`); add:

```rust
#[test]
fn entra_mode_needs_the_parts_and_accepts_principal_names() {
    let mut variables = REQUIRED.to_vec();
    variables.retain(|(name, _)| *name != "PUSH_DATABASE_URL");
    variables.extend([
        ("PUSH_POSTGRES_AUTH", "azure-entra"),
        ("PUSH_DATABASE_HOST", "pg.example.net"),
        ("PUSH_DATABASE_USER", "push-auth-id"),
        ("PUSH_AUTH_WORKER_ROLE", "push-worker-id"),
    ]);
    let config = config(&variables).unwrap();
    assert!(matches!(config.database, DatabaseTarget::Entra(ref database) if database.name == "push_auth"));
    assert_eq!(config.worker_role.as_deref(), Some("push-worker-id"));
    assert!(config.azure.is_some(), "the identity is read (managed identity here)");
    // Password mode keeps the plain role names it always accepted.
    let mut password = REQUIRED.to_vec();
    password.push(("PUSH_AUTH_WORKER_ROLE", "push-worker-id"));
    assert!(matches!(config_result(&password), Err(ConfigError::Invalid { name: "PUSH_AUTH_WORKER_ROLE", .. })));
}
```

In `src\db.rs` tests, keep `only_plain_role_names_are_accepted` for `is_role_name` (it
stays exported) and add:

```rust
#[test]
fn worker_grants_refuse_names_that_are_not_principals() {
    assert!(!push_store::is_principal_name("w\"; DROP"));
    assert!(push_store::is_principal_name("push-worker-id"));
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p push-auth --lib`
Expected: compile errors.

- [ ] **Step 3: `config.rs`.** Same changes as Task 9 Step 4 with these differences: the
default database name is `push_auth`; the table documents the same new variables; the
worker role is checked by the PostgreSQL mode, so password mode is unchanged:

```rust
/// `PUSH_AUTH_WORKER_ROLE`: a plain role name in password mode (as before), an Entra
/// principal name in Entra mode; unset is `None`.
fn worker_role(lookup: &impl Fn(&str) -> Option<String>, mode: AuthMode) -> Result<Option<String>, ConfigError> {
    let Some(role) = lookup("PUSH_AUTH_WORKER_ROLE").filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let (valid, reason) = match mode {
        AuthMode::Password => (is_role_name(&role), "expected a plain role name: a-z, 0-9, _"),
        AuthMode::AzureEntra => (push_store::is_principal_name(&role), "expected 1 to 63 ASCII letters, digits, _ - . or @"),
    };
    if valid {
        Ok(Some(role))
    } else {
        Err(ConfigError::Invalid { name: "PUSH_AUTH_WORKER_ROLE", value: role, reason: reason.to_owned() })
    }
}
```

(use the existing error reason text for password mode if the current tests assert it;
`config_result` in the test above stands for the module's helper that returns the
`Result`, whatever it is called there).

(keep the variable's documentation: "role the worker connects as: granted the outbox and
the cleanups at start-up (or by `migrate`)"). Copy the `problem`, `flag` and
`runtime_role` helpers of Task 9 Step 4 into this file (each service owns its
`ConfigError`). Add `MigrateConfig` with one more field:

```rust
/// What `push-auth migrate` reads: the database variables, the identity,
/// `PUSH_DATABASE_RUNTIME_ROLE` (granted the use of the `auth` schema) and
/// `PUSH_AUTH_WORKER_ROLE` (granted the worker's narrow access, as at start-up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateConfig {
    pub database: DatabaseTarget,
    pub azure: Option<AzureConfig>,
    pub runtime_role: Option<String>,
    pub worker_role: Option<String>,
}
```

whose `from_lookup` reads the database as `Config` does (default name `push_auth`), the
identity with `azure_for(lookup_dyn, &[database.mode()])`, `runtime_role(&lookup)?` and
the worker role with `worker_role(&lookup, database.mode())?` (also called from
`Config::from_lookup`).

- [ ] **Step 4: `db.rs`.** Split the grant from its connection:

```rust
/// [`grant_worker_access_on`] on a connection of its own to `url`.
pub async fn grant_worker_access(url: &str, role: &str) -> Result<(), sqlx::Error> {
    let pool = PgPoolOptions::new().max_connections(1).connect(url).await?;
    let result = grant_worker_access_on(&pool, role).await;
    pool.close().await;
    result
}

/// Lets the worker's `role` drain the outbox and run the two cleanups, and nothing else
/// (the grants listed on [`grant_worker_access`]). Idempotent. Run it as the owner of the
/// `auth` schema, after the migrations. `role` must be a principal name
/// ([`push_store::is_principal_name`]) of an existing role.
pub async fn grant_worker_access_on(pool: &sqlx::PgPool, role: &str) -> Result<(), sqlx::Error> {
    if !push_store::is_principal_name(role) {
        return Err(sqlx::Error::Configuration("the worker's role name is not valid".into()));
    }
    let grants = format!(
        "GRANT USAGE ON SCHEMA auth TO \"{role}\"; \
         GRANT SELECT, DELETE, UPDATE (created_at) ON auth.outbox TO \"{role}\"; \
         GRANT EXECUTE ON FUNCTION auth.delete_expired_refresh_tokens(integer), \
         auth.delete_unverified_accounts(interval, integer) TO \"{role}\""
    );
    sqlx::raw_sql(&grants).execute(pool).await.map(drop)
}
```

Move the existing doc comment's list of grants onto `grant_worker_access`. Export
`grant_worker_access_on` from `lib.rs`.

- [ ] **Step 5: `limits.rs`.** Add beside `RedisCounter::connect`:

```rust
    /// Counters through `connector` (password or Entra ID).
    pub async fn with_connector(
        connector: &push_credentials::RedisConnector,
        shutdown: CancellationToken,
    ) -> redis::RedisResult<Self> {
        let connection = connector.connection_manager(redis::aio::ConnectionManagerConfig::new()).await?;
        Ok(Self { connection, shutdown })
    }
```

and make `connect(url, shutdown)` delegate to it with `RedisConnector::password(url)?`.

- [ ] **Step 6: `server.rs`.** Remove the local `retry`; import as in Task 9 Step 6 (plus
`crate::db::grant_worker_access_on` instead of `grant_worker_access, migrate_database`,
and `crate::config::MigrateConfig`). Replace the Postgres and Redis blocks of `serve`:

```rust
    push_credentials::install_crypto_provider();
    let azure = config.azure.as_ref().map(AzureConfig::source).transpose().context("the Azure identity")?;
    let pg_tokens = tokens_for(&config.database, azure.as_ref(), shutdown.clone())
        .await
        .context("PostgreSQL credentials")?;
    let connector = Connector::for_target(&config.database, pg_tokens.as_ref())?;
    let redis = RedisConnector::open(&config.redis, azure.as_ref(), shutdown.clone())
        .await
        .context("Redis credentials")?;
    // Per datastore: a refused token or certificate is configuration in Entra mode only,
    // so a password-mode datastore keeps retrying exactly as before.
    let pg_entra = config.database.mode() == AuthMode::AzureEntra;
    let postgres_fatal = move |error: &anyhow::Error| pg_entra && is_permanent_startup(error);
    let redis_entra = config.redis.mode() == AuthMode::AzureEntra;
    let redis_fatal = move |error: &anyhow::Error| redis_entra && redis_permanent(error);
    let Some(pool) = retry("postgres", RETRY_DELAY, &shutdown, &postgres_fatal, || async {
        if config.run_migrations {
            // On a connection of their own, not on the request pool.
            connector.migrate(&crate::db::MIGRATOR, &["citext"]).await?;
            if let Some(role) = &config.worker_role {
                let owner = connector.single().await?;
                let granted = grant_worker_access_on(&owner, role).await;
                owner.close().await;
                granted?;
            }
        }
        anyhow::Ok(connector.pool(&config.store).await?)
    })
    .await?
    else {
        return finish(health_server).await;
    };
    let Some(counter) = retry("redis", RETRY_DELAY, &shutdown, &redis_fatal, || async {
        anyhow::Ok(RedisCounter::with_connector(&redis, shutdown.clone()).await?)
    })
    .await?
    else {
        pool.close().await;
        return finish(health_server).await;
    };
```

Add the subcommand:

```rust
/// `push-auth migrate`: applies the `auth` migrations (checking that `citext` is
/// allow-listed), grants `PUSH_DATABASE_RUNTIME_ROLE` the use of the `auth` schema and
/// `PUSH_AUTH_WORKER_ROLE` the worker's narrow access, then returns.
pub async fn migrate(config: MigrateConfig) -> anyhow::Result<()> {
    push_credentials::install_crypto_provider();
    let shutdown = CancellationToken::new();
    let azure = config.azure.as_ref().map(AzureConfig::source).transpose().context("the Azure identity")?;
    let tokens = tokens_for(&config.database, azure.as_ref(), shutdown.clone())
        .await
        .context("PostgreSQL credentials")?;
    let connector = Connector::for_target(&config.database, tokens.as_ref())?;
    connector.migrate(&crate::db::MIGRATOR, &["citext"]).await?;
    let pool = connector.single().await?;
    let granted = async {
        if let Some(role) = &config.runtime_role {
            grant_runtime_access(&pool, "auth", role).await?;
        }
        if let Some(role) = &config.worker_role {
            grant_worker_access_on(&pool, role).await?;
        }
        anyhow::Ok(())
    }
    .await;
    pool.close().await;
    granted?;
    shutdown.cancel();
    tracing::info!("push_auth is migrated");
    Ok(())
}
```

- [ ] **Step 7: `lib.rs` and `main.rs`** as in Task 9 Step 7 (`push_auth::migrate`,
`push_auth::MigrateConfig`).

- [ ] **Step 8: Tests that build auth's config by struct** (if any; `push-smoke` uses
`from_lookup`): add the four fields as in Task 9 Step 8.

- [ ] **Step 9: Run.**

Run: `cargo test -p push-auth --locked`
Expected: all pass.

- [ ] **Step 10: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "feat(auth): connect with Entra ID tokens; add migrate and PUSH_RUN_MIGRATIONS"
```

### Task 11: The worker: two Entra databases, `migrate`

**Files:**
- Modify: `crates\push-worker\Cargo.toml`, `src\config.rs`, `src\server.rs`, `src\lib.rs`, `src\main.rs`
- Modify (struct literals): `crates\push-worker\tests\server.rs`, `crates\push-smoke\tests\local.rs`

- [ ] **Step 1: Failing config test** in `src\config.rs`'s tests (and update reads of
`database_url` / `auth_database_url` to the targets):

```rust
#[test]
fn in_entra_mode_the_auth_database_defaults_to_the_workers_server_and_principal() {
    let mut variables = REQUIRED.to_vec();
    variables.retain(|(name, _)| !name.ends_with("DATABASE_URL"));
    variables.extend([
        ("PUSH_POSTGRES_AUTH", "azure-entra"),
        ("PUSH_DATABASE_HOST", "pg.example.net"),
        ("PUSH_DATABASE_USER", "push-worker-id"),
    ]);
    let config = config(&variables).unwrap();
    let DatabaseTarget::Entra(own) = &config.database else { panic!("Entra") };
    let DatabaseTarget::Entra(auth) = &config.auth_database else { panic!("Entra") };
    assert_eq!((own.name.as_str(), auth.name.as_str()), ("push_worker", "push_auth"));
    assert_eq!((auth.host.as_str(), auth.user.as_str()), ("pg.example.net", "push-worker-id"));
}
```

- [ ] **Step 2: Run it to see it fail.**

Run: `cargo test -p push-worker --lib config`
Expected: compile errors.

- [ ] **Step 3: `config.rs`.** Replace `database_url` and `auth_database_url` with
`database: DatabaseTarget`, `auth_database: DatabaseTarget`, add `azure` and
`run_migrations`; document the variables (including `PUSH_AUTH_DATABASE_HOST`, `_PORT`,
`_NAME` (`push_auth`), `_USER`, `_CA_FILE`, defaulting to the worker's own); parse:

```rust
        let lookup_dyn: &dyn Fn(&str) -> Option<String> = &lookup;
        let mode = postgres_auth(lookup_dyn).map_err(problem)?;
        let database = DatabaseTarget::from_lookup(lookup_dyn, mode, &DatabaseVars::own("push_worker"), None)
            .map_err(problem)?;
        let own = match &database {
            DatabaseTarget::Entra(own) => Some(own.clone()),
            DatabaseTarget::Url(_) => None,
        };
        let auth_database = DatabaseTarget::from_lookup(lookup_dyn, mode, &DatabaseVars::AUTH_FOR_WORKER, own.as_ref())
            .map_err(problem)?;
        let azure = azure_for(lookup_dyn, &[mode]).map_err(problem)?;
        let run_migrations = flag(&lookup, "PUSH_RUN_MIGRATIONS", true)?;
```

with the `problem`, `flag` and `runtime_role` helpers (copied as in Task 10) and a
`MigrateConfig { database, azure, runtime_role }` (default name `push_worker`).

- [ ] **Step 4: `server.rs`.** Remove the local `retry`; with the same imports as Task 9,
replace the two pool blocks:

```rust
    push_credentials::install_crypto_provider();
    let azure = config.azure.as_ref().map(AzureConfig::source).transpose().context("the Azure identity")?;
    // One identity, one scope: both databases share the token.
    let pg_tokens = tokens_for(&config.database, azure.as_ref(), shutdown.clone())
        .await
        .context("PostgreSQL credentials")?;
    let own = Connector::for_target(&config.database, pg_tokens.as_ref())?;
    let auth_side = Connector::for_target(&config.auth_database, pg_tokens.as_ref())?;
    let entra = own.is_entra();
    let permanent = move |error: &anyhow::Error| entra && is_permanent_startup(error);
    let Some(pool) = retry("postgres", RETRY_DELAY, &shutdown, &permanent, || async {
        if config.run_migrations {
            own.migrate(&crate::db::MIGRATOR, &["citext"]).await?;
        }
        anyhow::Ok(own.pool(&config.store).await?)
    })
    .await?
    else {
        return finish(health_server).await;
    };
    let Some(auth) = retry("auth-svc's database", RETRY_DELAY, &shutdown, &permanent, || async {
        let auth = auth_side.pool(&config.store).await?;
        // Until auth-svc (or its migration job) has migrated and granted the worker's role
        // its access, this fails.
        if let Err(error) = sqlx::query("SELECT 1 FROM auth.outbox LIMIT 0").execute(&auth).await {
            auth.close().await;
            return Err(anyhow::Error::from(error));
        }
        anyhow::Ok(auth)
    })
    .await?
    else {
        pool.close().await;
        return finish(health_server).await;
    };
```

Note the missing-grant error (`42501`, permission denied) is not permanent: it is retried
until auth-svc grants it, as today. Add `migrate(config: MigrateConfig)` like message-svc's
with `&["citext"]` and schema `worker`.

- [ ] **Step 5: `lib.rs`, `main.rs`, struct literals** as in Task 9 Steps 7 and 8
(`database: DatabaseTarget::Url(...)`, `auth_database: DatabaseTarget::Url(...)`,
`azure: None`, `run_migrations: true`).

- [ ] **Step 6: Run.**

Run: `cargo test -p push-worker --locked; cargo test -p push-smoke --test local --locked`
Expected: all pass.

- [ ] **Step 7: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "feat(worker): connect both databases with Entra ID tokens; add migrate"
```

### Task 12: The gateway: Redis target, connector, and make-before-break hub rotation

**Files:**
- Modify: `crates\push-gateway\Cargo.toml`, `src\config.rs`, `src\server.rs`, `src\hub.rs`, `src\lib.rs` (if `Hub::start` is re-exported with its signature in docs)
- Modify: `crates\push-gateway\tests\hub.rs`, `crates\push-gateway\tests\gateway.rs`, `crates\push-smoke\tests\local.rs`
- Test: `crates\push-gateway\tests\hub_entra.rs`

- [ ] **Step 1: Write the failing tests** `crates\push-gateway\tests\hub_entra.rs`:

```rust
//! The hub on Redis with Entra tokens: rotations replace the subscriber without a gap,
//! and a killed subscriber comes back with a fresh token.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use prost::Message;
use push_credentials::testing::{IssueHook, Issued, StubIssuer, StubPrincipal, TestCa, TlsRedis, set_redis_password, tls_redis};
use push_credentials::{AzureConfig, RedisConnector, RedisTarget};
use push_gateway::{Hub, Signal};
use push_proto::internal::v1::{EntriesAvailable, Notification, notification::Kind};
use push_proto::notify_channel;
use redis::AsyncCommands;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const OID: &str = "0a1b2c3d-0000-4000-8000-0000000000aa";

struct Setup {
    _ca: TestCa,
    _server: TlsRedis,
    admin: redis::aio::MultiplexedConnection,
    stub: StubIssuer,
    connector: RedisConnector,
}

async fn setup() -> Setup {
    push_credentials::install_crypto_provider();
    let ca = TestCa::new();
    let server = tls_redis(&ca).await;
    let admin = server.admin(&ca).await;
    let hook: IssueHook = {
        let admin = admin.clone();
        Arc::new(move |issued: Issued| {
            let mut admin = admin.clone();
            Box::pin(async move { set_redis_password(&mut admin, &issued.object_id, &issued.token).await })
                as BoxFuture<'static, ()>
        })
    };
    let principals = vec![StubPrincipal { client_id: "gateway".into(), object_id: OID.into() }];
    let stub = StubIssuer::start(TENANT, "federated", principals, Duration::from_secs(2), Some(hook)).await;
    let file = std::env::temp_dir().join(format!("push-hub-{}.jwt", Uuid::now_v7()));
    std::fs::write(&file, "federated").unwrap();
    let azure = AzureConfig {
        client_id: Some("gateway".into()),
        tenant_id: Some(TENANT.into()),
        federated_token_file: Some(file),
        authority_host: stub.authority().into(),
    };
    let target = RedisTarget::Entra { url: server.url(), user: None, ca_file: Some(ca.file().to_owned()) };
    let connector = RedisConnector::open(&target, Some(&azure.source().unwrap()), CancellationToken::new()).await.unwrap();
    Setup { _ca: ca, _server: server, admin, stub, connector }
}

fn ring_payload() -> Vec<u8> {
    Notification { kind: Some(Kind::EntriesAvailable(EntriesAvailable {})) }.encode_to_vec()
}

async fn connected(hub: &Hub) {
    for _ in 0..100 {
        if hub.is_connected() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the hub did not connect within 5 s");
}

#[tokio::test]
async fn rotations_replace_the_subscriber_without_losing_a_notification() {
    let mut setup = setup().await;
    let shutdown = CancellationToken::new();
    let (hub, subscriber) = Hub::start(setup.connector.clone(), shutdown.clone());
    connected(&hub).await;
    // One session per kick: a kick is never coalesced, so each lost message shows.
    let mut sessions = Vec::new();
    for _ in 0..35 {
        let device = Uuid::now_v7();
        sessions.push((device, hub.watch_device(device, Uuid::now_v7(), 0).await));
    }
    let issued_before = setup.stub.issued().len();
    // A publisher at 20 a second, independent of the receiving side, for about 7 s:
    // three or more rotations happen while kicks keep coming.
    let publisher = {
        let mut admin = setup.admin.clone();
        let devices: Vec<Uuid> = sessions.iter().map(|(device, _)| *device).collect();
        tokio::spawn(async move {
            for device in devices {
                let kick = push_proto::internal::v1::Kick {
                    reason: push_proto::internal::v1::KickReason::DeviceRevoked.into(),
                    ..Default::default()
                };
                let _: i64 = admin.publish(push_proto::kick_channel(device), kick.encode_to_vec()).await.unwrap();
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
    };
    for (device, session) in &mut sessions {
        let signal = tokio::time::timeout(Duration::from_secs(10), session.next())
            .await
            .unwrap_or_else(|_| panic!("the kick of {device} was lost"));
        assert_eq!(signal, Signal::Kick(push_proto::internal::v1::KickReason::DeviceRevoked));
        assert!(hub.is_connected(), "a rotation must not disconnect the hub");
    }
    publisher.await.unwrap();
    assert!(setup.stub.issued().len() >= issued_before + 3);
    let counts: Vec<(String, i64)> = redis::cmd("PUBSUB").arg("NUMSUB").arg(push_proto::kick_channel(sessions[0].0))
        .query_async(&mut setup.admin).await.unwrap();
    assert_eq!(counts[0].1, 1, "the replaced subscribers are gone");
    shutdown.cancel();
    subscriber.await.unwrap();
}

#[tokio::test]
async fn a_killed_subscriber_returns_with_a_fresh_token_and_its_channels() {
    let mut setup = setup().await;
    let shutdown = CancellationToken::new();
    let (hub, subscriber) = Hub::start(setup.connector.clone(), shutdown.clone());
    connected(&hub).await;
    let user = Uuid::now_v7();
    let mut session = hub.watch_device(Uuid::now_v7(), Uuid::now_v7(), 0).await;
    session.join_user(user).await;
    // Let the token it connected with be replaced, then cut every connection of the user.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _: () = redis::cmd("CLIENT").arg("KILL").arg("USER").arg(OID).query_async(&mut setup.admin).await.unwrap();
    let ring = tokio::time::timeout(Duration::from_secs(10), session.next()).await.expect("the reconnect rings");
    assert_eq!(ring, Signal::Ring);
    connected(&hub).await;
    let _: i64 = setup.admin.publish(notify_channel(user), ring_payload()).await.unwrap();
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), session.next()).await.unwrap(), Signal::Ring);
    shutdown.cancel();
    subscriber.await.unwrap();
}
```

Add to `crates\push-gateway\Cargo.toml`: `push-credentials.workspace = true` under
`[dependencies]`, and `push-credentials = { workspace = true, features = ["testing"] }` plus
`futures-util.workspace = true` under `[dev-dependencies]`.

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p push-gateway --test hub_entra`
Expected: compile errors (`Hub::start` takes a `redis::Client`).

- [ ] **Step 3: The hub.** In `src\hub.rs`, `Hub::start` and `Hub::start_with` take
`redis: RedisConnector` instead of `client: redis::Client` (import
`push_credentials::RedisConnector`), and `subscribe` takes `redis: RedisConnector`.
Replace `connection = client.get_async_pubsub() => connection,` with
`connection = redis.pubsub() => connection,`.

The rotation must not stop serving the current subscriber while the next one connects
(spec D4): the replacement is a future polled by the same `select!` as the current
stream, and the swap happens only once the new subscriber holds every channel.

1. Before the outer loop: `let mut rotations = redis.rotations();`
2. Inside `if resubscribed {`, before the inner loop:
   `let mut replacement: Option<Pin<Box<dyn Future<Output = Option<Subscriber>> + Send>>> = None;`
3. Add two branches to the inner `tokio::select!` (after the ping branch):

```rust
                            () = rotated(&mut rotations), if replacement.is_none() => {
                                // The new token cannot reach a subscribed connection: prepare a
                                // new subscriber while this one keeps serving.
                                replacement = Some(Box::pin(prepare(redis.clone(), hub.clone(), shutdown.clone())));
                            }
                            prepared = async { replacement.as_mut().expect("guarded").await }, if replacement.is_some() => {
                                replacement = None;
                                match prepared {
                                    Some((mut new_sink, new_messages)) => {
                                        // Channels joined while it was prepared: make the new
                                        // subscriber match the hub now (subscribing twice is harmless).
                                        let mut complete = true;
                                        for channel in hub.channels() {
                                            match bounded(&shutdown, new_sink.subscribe(&channel)).await {
                                                None => return,
                                                Some(true) => {}
                                                Some(false) => {
                                                    complete = false;
                                                    break;
                                                }
                                            }
                                        }
                                        if complete {
                                            sink = new_sink;
                                            messages = new_messages;
                                            ping_at = tokio::time::Instant::now() + ping_interval;
                                            tracing::info!("replaced the Redis subscriber for a new token");
                                        } else {
                                            tracing::warn!("could not complete the new Redis subscriber; keeping the current one");
                                        }
                                    }
                                    None => tracing::warn!("could not prepare a new Redis subscriber; keeping the current one"),
                                }
                            }
```

Add below `bounded`:

```rust
/// Resolves when the Redis token changed; never in password mode.
async fn rotated(rotations: &mut Option<watch::Receiver<AccessToken>>) {
    match rotations {
        Some(receiver) => {
            if receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

type Subscriber = (redis::aio::PubSubSink, redis::aio::PubSubStream);

/// A new subscriber, signed in with the current token and subscribed to the hub's
/// channels at this moment; `None` when it failed or `shutdown` came. The caller
/// reconciles the channels again before swapping.
async fn prepare(redis: RedisConnector, hub: Hub, shutdown: CancellationToken) -> Option<Subscriber> {
    let connect = tokio::select! {
        () = shutdown.cancelled() => return None,
        connect = tokio::time::timeout(COMMAND_TIMEOUT, redis.pubsub()) => connect,
    };
    let (mut sink, messages) = connect.ok()?.ok()?.split();
    for channel in hub.channels() {
        if bounded(&shutdown, sink.subscribe(&channel)).await != Some(true) {
            return None;
        }
    }
    Some((sink, messages))
}
```

with imports `use std::future::Future;`, `use std::pin::Pin;`,
`use push_credentials::AccessToken;` and `use tokio::sync::watch;`. The sink and stream
type names are those `PubSub::split` returns in redis 1.2.2 (check
`redis-1.2.2\src\aio\pubsub.rs`; use the owned split, as the existing code does).
`sink`, `messages` and `ping_at` in the inner loop must be `let mut`. While the
replacement is prepared, commands keep going to the current sink and messages keep being
dispatched from the current stream; the reconciliation catches channels joined meanwhile,
and a channel left meanwhile stays subscribed on the new connection until its next leave
(its messages find no member and are dropped). A failed preparation keeps the current
subscriber; when Azure closes it at token expiry, the existing reconnect path takes over
with the newest token.

In the module's own test (`hub.rs` around line 913) replace
`redis::Client::open(format!("redis://127.0.0.1:{port}")).unwrap()` with
`RedisConnector::password(&format!("redis://127.0.0.1:{port}")).unwrap()`; in
`tests\hub.rs`, call `Hub::start(RedisConnector::password(&url).unwrap(), ...)` where
`url` is the `redis://127.0.0.1:{port}` the helper already builds (keep the
`redis::Client` for publishing and `PUBSUB NUMSUB`).

- [ ] **Step 4: Config.** In `src\config.rs` replace `redis_url: String` with
`redis: RedisTarget` and `azure: Option<AzureConfig>`; parse with
`RedisTarget::from_lookup(lookup_dyn)` and `azure_for(lookup_dyn, &[redis.mode()])`; add
the `problem` and `redacted` helpers (the gateway has none yet: copy them from
message-svc's config); document `PUSH_REDIS_AUTH`, `PUSH_REDIS_USER`, `PUSH_REDIS_CA_FILE`
and the `AZURE_*` variables as in Task 9. Update its tests' reads of `config.redis_url`
(`RedisTarget::Password { url }`) and its Debug test (the target redacts the URL).

- [ ] **Step 5: Server.** In `src\server.rs` replace the `redis::Client::open` block with:

```rust
    push_credentials::install_crypto_provider();
    let azure = config.azure.as_ref().map(AzureConfig::source).transpose().context("the Azure identity")?;
    // The error is not passed on in password mode: the URL carries the Redis password.
    let redis = RedisConnector::open(&config.redis, azure.as_ref(), shutdown.clone())
        .await
        .context("Redis credentials")?;
    let commands = ConnectionManagerConfig::new()
        .set_connection_timeout(Some(REDIS_TIMEOUT))
        .set_response_timeout(Some(REDIS_TIMEOUT))
        .set_number_of_retries(1);
    let publisher = redis
        .connection_manager_lazy(commands)
        .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
    let (hub, subscriber) = Hub::start(redis, shutdown.clone());
```

In Entra mode the lazy publisher and the background subscriber would hide a refused
token or certificate forever, so before them `serve` proves one authenticated connection
(password mode keeps today's lazy start):

```rust
    if config.redis.mode() == push_credentials::AuthMode::AzureEntra {
        let fatal = |error: &anyhow::Error| redis_permanent(error);
        let check = retry("redis", Duration::from_secs(2), &shutdown, &fatal, || async {
            let mut connection = redis.connection_manager(ConnectionManagerConfig::new()).await?;
            redis::cmd("PING").query_async::<String>(&mut connection).await?;
            anyhow::Ok(())
        })
        .await?;
        if check.is_none() {
            return Ok(());
        }
    }
```

(placed right after `RedisConnector::open`, with `use push_credentials::startup::{redis_permanent, retry};`;
an unreachable Redis is retried, as for the other services; `return Ok(())` on shutdown
before anything was spawned mirrors the other early exits in `serve`).

(`OpenError` never contains the URL.) Struct literals of `push_gateway::Config` in
`tests\gateway.rs` and `push-smoke\tests\local.rs` replace `redis_url: x,` with
`redis: push_credentials::RedisTarget::Password { url: x }, azure: None,`.

- [ ] **Step 6: Run.**

Run: `cargo test -p push-gateway --locked`
Expected: all tests pass, including `hub_entra` (about 15 s).

- [ ] **Step 7: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "feat(gateway): use Entra ID tokens for Redis and rotate the hub's subscriber"
```

### Task 13: The whole stack on Entra tokens, through many rotations

The local proof of AC2 to AC7: the four services in Entra mode with a separate migration
identity (so every service runs as a granted non-owner), tokens that live 6 s, the smoke
test, a 45-second push-load run (seven or more rotations of every token), forced new
PostgreSQL connections, and no token in any log line. Plus the fail-fast cases.

**Files:**
- Create: `crates\push-smoke\tests\common\mod.rs`
- Modify: `crates\push-smoke\tests\local.rs`, `crates\push-smoke\Cargo.toml`
- Create: `crates\push-smoke\tests\entra.rs`

- [ ] **Step 1: Share the stack builder.** Move `Server`, `spawn`, `Stack` (without the
container fields), `route`, `wait_until_ready` and `readyz_status` from `tests\local.rs`
into `tests\common\mod.rs` (`#![allow(dead_code)]` at its top), changing `Stack::start`
into

```rust
/// What differs between the password stack and the Entra stack: each service's
/// datastore variables. `Stack` adds the rest (addresses, internal tokens, keys).
pub struct Datastores {
    pub auth: Vec<(String, String)>,
    pub message_svc: Vec<(String, String)>,
    pub gateway: Vec<(String, String)>,
    pub worker: Vec<(String, String)>,
}

impl Stack {
    pub async fn start(datastores: Datastores) -> Self { /* body below */ }
}
```

whose body is the old `start` from `let shutdown = CancellationToken::new();` onward,
minus the container start and the `CREATE DATABASE` / `CREATE ROLE` statements (the
caller prepares the databases), with every service configured through its
`Config::from_lookup` over its variables: the existing ones (addresses, tokens, keys,
`PUSH_AUTH_WORKER_ROLE` stays per caller in `datastores.auth`) followed by the
`Datastores` entries. A helper does the lookups:

```rust
pub fn lookup(variables: Vec<(String, String)>) -> impl Fn(&str) -> Option<String> {
    move |name| variables.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone())
}
```

For message-svc, the gateway and the worker build their variable lists from the values
the struct literals used (`PUSH_GRPC_ADDR`, `PUSH_ACCOUNTS_GRPC_ADDR`,
`PUSH_DIRECTORY_GRPC_ADDR`, `PUSH_HEALTH_ADDR`, `PUSH_ACCOUNTS_TOKEN`,
`PUSH_DIRECTORY_TOKEN`, `PUSH_MESSAGING_TOKEN`; `PUSH_MESSAGE_SVC_URL`,
`PUSH_MESSAGING_TOKEN`, `PUSH_AUTH_URL`, `PUSH_TOKEN_AUDIENCE`, `PUSH_LISTEN_ADDR`,
`PUSH_HEALTH_ADDR`; the worker's `PUSH_MESSAGE_SVC_URL`, `PUSH_ACCOUNTS_TOKEN`,
`PUSH_EMAIL_SENDER=log`, `PUSH_WORKER_POLL_MS=250`, `PUSH_HTTP_ADDR`, `PUSH_HEALTH_ADDR`,
`PUSH_DEV_MAILBOX=1`), using the names each service's `config.rs` documents; check each
name there, not here. `local.rs` keeps its containers and builds
`Datastores` with the URLs it used before (`PUSH_DATABASE_URL`, `PUSH_REDIS_URL`,
`PUSH_AUTH_DATABASE_URL`, `PUSH_AUTH_WORKER_ROLE=push_worker`), and declares `mod common;`.

Run: `cargo test -p push-smoke --test local --locked`
Expected: both tests still pass. Commit:

```powershell
cargo fmt --all
git add -A
git commit -m "test(smoke): share the whole-stack builder"
```

- [ ] **Step 2: Write the Entra tests** `crates\push-smoke\tests\entra.rs` (add
`push-credentials = { workspace = true, features = ["testing"] }`, `tracing`,
`tracing-subscriber`, `futures-util` and `sqlx` to push-smoke's `[dev-dependencies]` if
missing):

```rust
//! The whole stack with Entra ID tokens over TLS (spec 2026-10-18 AC2 to AC7): a
//! migration identity owns the databases and runs each service's `migrate`; the services
//! run as granted principals with tokens that live 6 s; the smoke test, a 45-second
//! push-load run and forced new database connections all pass; no token reaches a log.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Datastores, Stack, lookup};
use futures_util::future::BoxFuture;
use push_credentials::testing::{
    IssueHook, Issued, StubIssuer, StubPrincipal, TestCa, TlsPostgres, TlsRedis, set_postgres_password,
    set_redis_password, tls_postgres, tls_redis,
};

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const ASSERTION: &str = "federated-service-account-token";
/// (client id, PostgreSQL role, Redis object id) of each identity.
const PRINCIPALS: [(&str, &str, &str); 5] = [
    ("migrator", "push-migrator-id", "00000000-0000-4000-8000-000000000000"),
    ("auth", "push-auth-id", "00000000-0000-4000-8000-000000000001"),
    ("message-svc", "push-message-svc-id", "00000000-0000-4000-8000-000000000002"),
    ("worker", "push-worker-id", "00000000-0000-4000-8000-000000000003"),
    ("gateway", "push-gateway-id", "00000000-0000-4000-8000-000000000004"),
];

static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        LOG.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Azure {
    ca: TestCa,
    postgres: TlsPostgres,
    redis: TlsRedis,
    admin: sqlx::PgPool,
    stub: StubIssuer,
    token_file: std::path::PathBuf,
}

/// TLS datastores, the stub issuer (whose tokens become passwords), the roles and the
/// databases owned by the migration identity.
async fn azure(lifetime: Duration) -> Azure {
    push_credentials::install_crypto_provider();
    let ca = TestCa::new();
    let postgres = tls_postgres(&ca).await;
    let redis = tls_redis(&ca).await;
    let admin = postgres.admin(&ca).await;
    let mut statements = String::new();
    for (_, role, _) in PRINCIPALS {
        statements.push_str(&format!("CREATE ROLE \"{role}\" LOGIN; "));
    }
    for database in ["push_auth", "push_messaging", "push_worker"] {
        statements.push_str(&format!("CREATE DATABASE {database} OWNER \"push-migrator-id\"; "));
    }
    sqlx::raw_sql(&statements).execute(&admin).await.unwrap();
    let redis_admin = redis.admin(&ca).await;
    let hook: IssueHook = {
        let admin = admin.clone();
        Arc::new(move |issued: Issued| {
            let admin = admin.clone();
            let mut redis_admin = redis_admin.clone();
            Box::pin(async move {
                let role = PRINCIPALS.iter().find(|(client, _, _)| *client == issued.client_id).unwrap().1;
                if issued.scope.starts_with("https://ossrdbms-aad") {
                    set_postgres_password(&admin, role, &issued.token).await;
                } else {
                    set_redis_password(&mut redis_admin, &issued.object_id, &issued.token).await;
                }
            }) as BoxFuture<'static, ()>
        })
    };
    let principals = PRINCIPALS
        .iter()
        .map(|(client, _, oid)| StubPrincipal { client_id: (*client).into(), object_id: (*oid).into() })
        .collect();
    let stub = StubIssuer::start(TENANT, ASSERTION, principals, lifetime, Some(hook)).await;
    let token_file = std::env::temp_dir().join(format!("push-entra-{}.jwt", uuid::Uuid::now_v7()));
    std::fs::write(&token_file, ASSERTION).unwrap();
    Azure { ca, postgres, redis, admin, stub, token_file }
}

impl Azure {
    fn identity(&self, client: &str) -> Vec<(String, String)> {
        vec![
            ("AZURE_CLIENT_ID".into(), client.into()),
            ("AZURE_TENANT_ID".into(), TENANT.into()),
            ("AZURE_FEDERATED_TOKEN_FILE".into(), self.token_file.display().to_string()),
            ("AZURE_AUTHORITY_HOST".into(), self.stub.authority().into()),
        ]
    }

    fn postgres(&self, client: &str, role: &str) -> Vec<(String, String)> {
        let mut variables = self.identity(client);
        variables.extend([
            ("PUSH_POSTGRES_AUTH".into(), "azure-entra".into()),
            ("PUSH_DATABASE_HOST".into(), "127.0.0.1".into()),
            ("PUSH_DATABASE_PORT".into(), self.postgres.port.to_string()),
            ("PUSH_DATABASE_USER".into(), role.into()),
            ("PUSH_DATABASE_CA_FILE".into(), self.ca.file().display().to_string()),
            ("PUSH_RUN_MIGRATIONS".into(), "false".into()),
        ]);
        variables
    }

    fn redis(&self) -> Vec<(String, String)> {
        vec![
            ("PUSH_REDIS_AUTH".into(), "azure-entra".into()),
            ("PUSH_REDIS_URL".into(), self.redis.url()),
            ("PUSH_REDIS_CA_FILE".into(), self.ca.file().display().to_string()),
        ]
    }

    /// Each service's `migrate`, as the migration job runs them.
    async fn migrate(&self) {
        let migrator = |runtime: &str| {
            let mut variables = self.postgres("migrator", "push-migrator-id");
            variables.push(("PUSH_DATABASE_RUNTIME_ROLE".into(), runtime.into()));
            variables
        };
        let mut auth = migrator("push-auth-id");
        auth.push(("PUSH_AUTH_WORKER_ROLE".into(), "push-worker-id".into()));
        push_auth::migrate(push_auth::MigrateConfig::from_lookup(lookup(auth)).unwrap()).await.unwrap();
        push_message_svc::migrate(push_message_svc::MigrateConfig::from_lookup(lookup(migrator("push-message-svc-id"))).unwrap())
            .await
            .unwrap();
        push_worker::migrate(push_worker::MigrateConfig::from_lookup(lookup(migrator("push-worker-id"))).unwrap())
            .await
            .unwrap();
    }

    fn datastores(&self) -> Datastores {
        let mut auth = self.postgres("auth", "push-auth-id");
        auth.extend(self.redis());
        let mut message_svc = self.postgres("message-svc", "push-message-svc-id");
        message_svc.extend(self.redis());
        let mut gateway = self.identity("gateway");
        gateway.extend(self.redis());
        let mut worker = self.postgres("worker", "push-worker-id");
        worker.push(("PUSH_AUTH_DATABASE_NAME".into(), "push_auth".into()));
        Datastores { auth, message_svc, gateway, worker }
    }
}

#[tokio::test]
async fn the_whole_stack_runs_on_rotating_entra_tokens() {
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(|| Capture)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let azure = azure(Duration::from_secs(6)).await;
    azure.migrate().await;
    let stack = Stack::start(azure.datastores()).await;
    let data_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("push-entra-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let target = stack.smoke_target(data_dir.to_string_lossy().into_owned());
    tokio::time::timeout(Duration::from_secs(300), push_smoke::run(&target)).await.unwrap().expect("smoke passes");

    // 45 s of steady traffic: seven or more rotations of every token. No connection may
    // drop (reconnect off) and no message may be lost.
    let issued_before = azure.stub.issued().len();
    let report = common::load_run(&stack, Duration::from_secs(45)).await;
    assert_eq!(report.status, push_load::report::Status::Passed, "{:?}", report.failures);
    assert_eq!(report.connections.ready_once, 4);
    assert!(report.messages.lost == 0 && report.messages.delivered >= 10, "{:?}", report.messages);
    let rotations = azure.stub.issued().len() - issued_before;
    // Six token streams (auth-svc and message-svc: PostgreSQL and Redis; the gateway:
    // Redis; the worker: PostgreSQL for both databases), each refreshed every 4.5 s.
    assert!(rotations >= 6 * 7, "{rotations} tokens issued in 45 s");

    // New PostgreSQL connections after many rotations: only the newest token works now.
    sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename LIKE 'push-%-id'")
        .execute(&azure.admin).await.unwrap();
    tokio::time::timeout(Duration::from_secs(300), push_smoke::run(&target)).await.unwrap().expect("smoke passes again");
    stack.stop().await;

    let log = String::from_utf8(LOG.lock().unwrap().clone()).unwrap();
    assert!(!log.contains(ASSERTION), "the federated token reached a log");
    for issued in azure.stub.issued() {
        assert!(!log.contains(&issued.token), "an access token reached a log");
    }
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn start_up_stops_clearly_without_a_token_a_role_or_trust() {
    let azure = azure(Duration::from_secs(3600)).await;
    azure.migrate().await;
    let listeners = || async {
        let bind = || tokio::net::TcpListener::bind("127.0.0.1:0");
        push_message_svc::Listeners { messaging: bind().await.unwrap(), accounts: bind().await.unwrap(), directory: bind().await.unwrap(), health: bind().await.unwrap() }
    };
    let serve = |variables: Vec<(String, String)>| async {
        let config = push_message_svc::Config::from_lookup(lookup(variables)).unwrap();
        tokio::time::timeout(
            Duration::from_secs(20),
            push_message_svc::serve(config, listeners().await, tokio_util::sync::CancellationToken::new()),
        )
        .await
        .expect("start-up stops within 20 s instead of retrying")
        .expect_err("start-up fails")
    };
    let mut base = azure.postgres("message-svc", "push-message-svc-id");
    base.extend(azure.redis());

    azure.stub.set_failing(true);
    let message = format!("{:#}", serve(base.clone()).await);
    assert!(message.contains("credentials") && message.contains("503"), "{message}");
    azure.stub.set_failing(false);

    let mut no_role = base.clone();
    no_role.retain(|(name, _)| name != "PUSH_DATABASE_USER");
    no_role.push(("PUSH_DATABASE_USER".into(), "push-nobody-id".into()));
    let message = format!("{:#}", serve(no_role).await);
    assert!(message.contains("postgres"), "{message}");

    let other = TestCa::new();
    let mut untrusted = base.clone();
    untrusted.retain(|(name, _)| name != "PUSH_DATABASE_CA_FILE");
    untrusted.push(("PUSH_DATABASE_CA_FILE".into(), other.file().display().to_string()));
    let message = format!("{:#}", serve(untrusted).await);
    assert!(message.contains("postgres"), "{message}");

    sqlx::raw_sql("ALTER DATABASE push_auth SET azure.extensions = 'PG_TRGM'").execute(&azure.admin).await.unwrap();
    let mut auth = azure.postgres("migrator", "push-migrator-id");
    auth.push(("PUSH_DATABASE_NAME".into(), "push_auth".into()));
    let error = push_auth::migrate(push_auth::MigrateConfig::from_lookup(lookup(auth)).unwrap()).await.unwrap_err();
    assert!(format!("{error:#}").contains("azure.extensions"), "{error:#}");

    // Redis: a principal without an ACL user, and an untrusted certificate.
    let mut no_user = base.clone();
    no_user.push(("PUSH_REDIS_USER".into(), "nobody-oid".into()));
    let message = format!("{:#}", serve(no_user).await);
    assert!(message.contains("redis"), "{message}");
    let mut untrusted_redis = base.clone();
    untrusted_redis.retain(|(name, _)| name != "PUSH_REDIS_CA_FILE");
    untrusted_redis.push(("PUSH_REDIS_CA_FILE".into(), other.file().display().to_string()));
    let message = format!("{:#}", serve(untrusted_redis).await);
    assert!(message.contains("redis"), "{message}");

    // The gateway proves its Redis credentials before serving.
    let mut gateway = azure.identity("gateway");
    gateway.extend(azure.redis());
    gateway.retain(|(name, _)| name != "PUSH_REDIS_CA_FILE");
    gateway.push(("PUSH_REDIS_CA_FILE".into(), other.file().display().to_string()));
    gateway.extend(common::gateway_variables_for_a_failing_start());
    let config = push_gateway::Config::from_lookup(lookup(gateway)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        push_gateway::serve(config, listener, health, tokio_util::sync::CancellationToken::new()),
    )
    .await
    .expect("the gateway stops within 20 s");
    assert!(format!("{:#}", result.unwrap_err()).contains("redis"));

    // Mixed modes: Entra PostgreSQL with a password-mode Redis keeps today's behaviour for
    // Redis: a wrong password is retried, not fatal.
    let mut mixed = azure.postgres("message-svc", "push-message-svc-id");
    mixed.push(("PUSH_REDIS_URL".into(), "redis://:wrong@127.0.0.1:1".into()));
    let config = push_message_svc::Config::from_lookup(lookup(mixed)).unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let running = tokio::spawn(push_message_svc::serve(config, listeners().await, shutdown.clone()));
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(!running.is_finished(), "a password-mode Redis outage is retried");
    shutdown.cancel();
    running.await.unwrap().unwrap();
}
```

`common::load_run(&stack, duration)` is a helper to add to `common\mod.rs`: it creates
two verified accounts with `push_smoke::create_verified_accounts`, prepares them with
`push_load::prepare::prepare` (2 devices each, 1 sample pair) in a temporary directory
outside the repository, and runs `push_load::run::run` with 4 connections, ramp 4 per
second, `duration`, a 2 s message interval and `reconnect: false`, returning the report
(the code is the first half of `push_load_prepares_runs_and_reports_against_the_whole_stack`
in `local.rs`, moved into the helper and called from that test too).

The no-role case: the stub issues the token, but the role `push-nobody-id` does not exist
(PostgreSQL answers 28000/28P01): `retry` stops with "postgres: ...". The untrusted case
fails certificate verification. Neither waits for a retry. The Redis cases stop with
"redis: ..." for the same reasons. `common::gateway_variables_for_a_failing_start()`
returns the gateway's non-datastore variables (a message-svc URL and token, an auth URL,
the token audience, listen and health addresses on port 0) with the names its
`config.rs` documents; its start fails before it uses any of them.

- [ ] **Step 3: Run.**

Run: `cargo test -p push-smoke --test entra --locked -- --test-threads=1`
Expected: 2 tests pass in about two minutes. (One thread: the first test owns the global
log capture.) If a rotation breaks a connection, the push-load report names the closes;
fix the cause, never lengthen the token lifetime to hide it.

- [ ] **Step 4: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "test(smoke): run the whole stack on rotating Entra tokens"
```

### Task 14: Documentation and status

**Files:**
- Modify: `docs\superpowers\plans\2026-10-11-roadmap.md`, `docs\superpowers\specs\2026-10-18-azure-entra-datastores-design.md`, `README.md`

- [ ] **Step 1: Roadmap.** Add rows after `SR`:

```markdown
| 12A | Entra ID credentials for the datastores | `push-credentials` (workload identity and managed identity over rustls, refreshed at 75%), PostgreSQL with rotating tokens over verified TLS, Redis with re-authenticating connections and a make-before-break hub subscriber, `migrate` subcommands and `PUSH_RUN_MIGRATIONS`, proven locally against TLS datastores and a stub issuer | SR | In progress; [spec](../specs/2026-10-18-azure-entra-datastores-design.md), [plan](2026-10-18-plan-12a-entra-datastore-credentials.md) |
| 12B | Entra ID datastores in the chart | External datastores, per-service service accounts and token projection, a migration Job, egress NetworkPolicies, a CA bundle, the Azure guide and a minikube rehearsal | 12A | Planned |
| 13 | Entra ID sign-in | Opt-in Microsoft Entra ID sign-in beside email and password (the second downstream request) | 12B | Planned |
```

(At merge time the 12A status becomes `Done (#N)`.)

- [ ] **Step 2: Spec status.** Under the spec's title add
`Implementation status: **12A** in progress ([plan](../plans/2026-10-18-plan-12a-entra-datastore-credentials.md)).`

- [ ] **Step 3: README.** In the deployment paragraph that links
`docs/deployment-configuration.md`, add one sentence: "Managed PostgreSQL and Redis with
Microsoft Entra ID tokens instead of passwords: see the
[design](docs/superpowers/specs/2026-10-18-azure-entra-datastores-design.md) (the chart
values and guide arrive with Plan 12B)."

- [ ] **Step 4: Commit.**

```powershell
git add -A
git commit -m "docs: record Plan 12A"
```

### Task 15: Verification and delivery

- [ ] **Step 1: The full checks.**

```powershell
$env:CARGO_INCREMENTAL = '0'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 build --workspace --locked
cargo tree -i openssl-sys --workspace
cargo tree -i aws-lc-rs --workspace
pwsh deploy/helm/test-chart.ps1
```

Expected: no formatting change, no warning, every test passes, the MSRV build succeeds,
both `cargo tree` calls report no such package, the chart checks pass unchanged.

- [ ] **Step 2: The supply-chain check.** Download cargo-deny as in PR #23 (the release
binary from EmbarkStudios/cargo-deny, its `.sha256` verified) and run `cargo deny check`.
Expected: `advisories ok, bans ok, licenses ok, sources ok`. A new licence (for example
from `backon` or `rcgen`'s dependencies) is added to `deny.toml` only if it is permissive.

- [ ] **Step 3: Password mode on minikube.** On an owned profile (never the shared
`push` profile; restore the kubectl context to `push` afterwards):
`deploy/minikube/up.ps1 -Cluster push11b-local -Smoke`. Expected: the smoke test passes;
existing data is kept. Stop the profile.

- [ ] **Step 4: Review.** Ask for a code review of `git diff main...HEAD`; fix what it
finds with tests.

- [ ] **Step 5: Deliver** (the repository's standing workflow): set the roadmap row to
`Done (#N)` with the PR number, push, open the PR, wait for CI, squash-merge with the
subject `Entra ID credentials for PostgreSQL and Redis (Plan 12A) (#N)`, switch to `main`
and pull, delete the branch.

## Self-review against the spec

| Spec item | Task |
|---|---|
| D1 no Azure SDK, rustls only | 1, 3; Task 15 `cargo tree` |
| D2 always compiled | 1 |
| D3 MSRV 1.85, redis 1.2.2, sqlx TLS | 1, 15 |
| D4 Redis provider and hub make-before-break | 6, 12 |
| D5 refresh at 75%, back-off 1 to 30 s | 4 |
| D6 verify-full, rediss, CA files, no credentials in Entra URLs | 6, 7 |
| D7 permanent errors stop start-up; outages retried | 8 to 12, 13 |
| D8 `migrate` and `PUSH_RUN_MIGRATIONS`, runtime grants | 7, 9, 10, 11, 13 |
| D9 principal names | 7, 10 |
| D10 chart token projection | Plan 12B |
| AC1 password mode unchanged | 9 to 12 (existing tests), 15 |
| AC2 no secret in config or logs | 1 (redaction), 3 (errors), 13 (log capture) |
| AC3, AC4 serving on rotating tokens, new connections later | 13 |
| AC5 killed connections come back with their channels | 6, 12 |
| AC6 clear, secret-free start-up failures | 3, 8, 13 |
| AC7 TLS verification enforced | 5, 7, 13 |
| AC8 unit and integration tests | 3 to 13 |

