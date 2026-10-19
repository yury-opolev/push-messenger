# Plan 14: Pluggable Identity (push-auth-core and a thin push-auth) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Work sequentially on one branch.

**Goal:** Let a consumer of the repository substitute or restrict Microsoft Entra ID sign-in without forking: everything auth-svc is today moves into a library, `push-auth-core`, behind an `Authenticator` (who vouches for a user) and an `Admission` (who may sign in); a thin `push-auth` binary wires the defaults; a consumer's own binary passes its own implementations to `serve_with`. The default configuration, the image, the chart and every answer stay as they are.

**Architecture:** `crates\push-auth` is renamed (with history) to `crates\push-auth-core` (package `push-auth-core`, lib `push_auth_core`, no binary); a new `crates\push-auth` holds only `src\main.rs` and the example wrapper `examples\custom_identity.rs`. The core gains `src\identity\` (`Principal`, `Authenticator`, `AuthenticationError`, `Admission`, `AdmissionDecision`, the provider-id rules and the security-event helpers) and `src\identity\middleware.rs`, an axum middleware on `POST /v1/auth/{provider}` that rate-limits per IP, takes the token from `Authorization: Bearer` or the JSON body, authenticates, admits, puts the `Principal` into the request and rate-limits per identity. `EntraSignIn` implements `Authenticator` (provider `entra`). Accounts are found by `(provider, issuer, subject)` after migration `0005_external_provider.sql` turns `tenant_id uuid` into `issuer text`. `PUSH_AUTH_PROVIDERS` gains `external`; `serve_with(.., Overrides { authenticator, admission })` checks that the overrides and the providers agree before anything starts. The admin API gains an `external` selector.

**Tech Stack:** Rust 2024 / MSRV 1.85, Tokio, axum 0.8, async-trait 0.1, serde_json, sqlx 0.8.6 (PostgreSQL), testcontainers; no new crate.

---

## Authority and boundaries

Read first: `docs\superpowers\specs\2026-10-19-pluggable-identity-design.md` (decisions D1
to D10 and acceptance criteria AC1 to AC8 are binding). Not in scope (spec): the SDK and the
bindings for providers other than `entra`, splitting message-svc, the gateway or the worker,
chart values, replacing account provisioning.

The default configuration must stay compatible: the existing tests pass unchanged except
where this plan names the edit (the `push_auth::` paths become `push_auth_core::`, the
`Providers` literals gain `external: false`, the schema test names the new column and
unique key of migration 0005).

Work in the worktree `C:\Users\yurio\Documents\github\push-messenger-14` on
`feature/plan-14-replaceable-entra-check`. Every PowerShell call is a fresh process:

```powershell
$env:CARGO_INCREMENTAL = '0'
cargo test --workspace --locked -j 4
```

Expected before Task 1: everything passes (Docker must be running). Use `-j 4` for every
cargo build or test on this machine.

Every commit ends with:

```text
Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>
```

## Facts the design depends on (verified)

| Fact | Source | Consequence |
|---|---|---|
| `deploy\docker\Dockerfile` builds `-p push-auth` and copies `target/release/push-auth`; the second build (`-p push-smoke -p push-load`) is separate so push-auth's `testing` feature never reaches a service | `deploy\docker\Dockerfile:22-33` | The binary package keeps the name `push-auth`; the `testing` feature moves to the core; only the comment changes |
| Dependents: push-gateway (dev, `push_auth::tokens`), push-worker (`migrate_database`, `grant_worker_access`, `outbox`), push-smoke (`Config`, `serve`, `migrate`, `MigrateConfig`, `entra::testing`, feature `testing`) | `git grep push_auth::` | They switch to `push-auth-core`; nothing depends on the binary package |
| `POST /v1/auth/entra` today: `JsonBody<EntraBody { token, handle, device_id }>`, then the `EntraSignIn` per-IP limit, `verify`, the `EntraAccount` limit on `entra:{tid}:{oid}` | `src\api.rs:378-437` | The middleware keeps that order and the same limit keys for `entra` |
| `EntraSignIn::verify` logs `auth.entra.rejected` (`event`, `reason`) and a warn with the key error on an outage | `src\entra\mod.rs:183-199` | The middleware logs `auth.entra.rejected` with the same fields; `verify` keeps logging for direct callers |
| `jsonwebtoken::decode::<T>` deserialises the claims into any `DeserializeOwned`; required claims are checked on a separate pass | jsonwebtoken 9.3.1 `decoding.rs` | Decoding into `serde_json::Map` then into `EntraClaims` keeps the reasons and gives the full payload |
| `auth.external_identity` has `provider CHECK (provider IN ('entra'))`, `tenant_id uuid`, `UNIQUE (provider, tenant_id, subject)` (`external_identity_provider_tenant_id_subject_key`) | `migrations\0004_entra.sql`, `tests\schema.rs:380-410` | Migration 0005 rewrites them; the schema test names the new key |
| sqlx runs each migration in a transaction unless it starts with `-- no-transaction` | sqlx-core 0.8.6 `migrate` | 0005 is transactional; a failure leaves 0004's schema |
| sqlx's `Migrator` fields are public (`#[doc(hidden)]`, semver-exempt) and `MigrationSource` exists only for paths | sqlx-core 0.8.6 `migrator.rs`, `source.rs` | The upgrade test builds a migrator of versions 1 to 4 from `MIGRATOR.iter()` (pinned by `Cargo.lock`) |
| The audit middleware classifies answers by `MatchedPath`; `/v1/auth/entra` gives `auth.entra.success`/`auth.entra.failure` | `src\audit.rs:83-110` | An external route gets `auth.external.success`/`auth.external.failure`; Entra's stay |
| `Limits::check` lower-cases the account key | `src\limits.rs:183-188` | Per-identity keys may share a bucket for subjects differing in case only: stricter, never looser |
| The SDK posts `/v1/auth/entra` with a JSON body and no `Authorization` header | `crates\push-sdk\src\auth.rs:257-285` | Existing clients never hit the header/body conflict |
| Routes `/v1/auth/{signup,verify,login,password,refresh,jwks}` exist | `src\api.rs:58-80` | Those provider ids are refused at start-up (axum would panic on the overlap) |
| async-trait is a dependency of push-auth already | `crates\push-auth\Cargo.toml` | `Authenticator` and `Admission` are object-safe with `#[async_trait]` on Rust 1.85 |

## Files

| File | Change |
|---|---|
| `Cargo.toml` | member and dependency `push-auth-core`; `push-auth` stays a member |
| `crates\push-auth-core\` (moved from `crates\push-auth\`) | package `push-auth-core`, lib only, feature `testing` |
| `crates\push-auth\Cargo.toml`, `src\main.rs` | the thin binary |
| `crates\push-auth\examples\custom_identity.rs` | the example wrapper with its tests |
| `crates\push-auth-core\src\identity\mod.rs` | `Principal`, `Authenticator`, `AuthenticationError`, `Admission`, `AdmissionDecision`, provider ids, security events |
| `crates\push-auth-core\src\identity\middleware.rs` | the identity middleware |
| `crates\push-auth-core\src\entra\{mod,claims,settings}.rs` | `impl Authenticator for EntraSignIn`, the verified payload, `Providers::external`, `EntraSettings::from_env` |
| `crates\push-auth-core\migrations\0005_external_provider.sql` | generic provider, `issuer text`, new unique key |
| `crates\push-auth-core\src\{external,service,devices,api,audit,error,admin,config,server,lib}.rs` | accounts by `(provider, issuer, subject)`, the authenticators of a service, the routes, events, codes, the admin selector, `serve_with` |
| `crates\push-auth-core\src\tests\{mod,identity,identity_accounts,identity_api,identity_server,admin}.rs` | the fake authenticator and admission, the new tests |
| `crates\push-auth-core\tests\schema.rs` | the new column and key; the upgrade from 0004 |
| `crates\push-gateway`, `crates\push-worker`, `crates\push-smoke` | `push_auth_core` |
| `deploy\docker\Dockerfile`, `deploy\minikube\*` | comments only |
| `docs\entra-sign-in.md`, `README.md`, roadmap, spec | "Your own wrapper", configuration, status |

---

### Task 1: Split the crate without changing behaviour

**Files:**
- Move: `crates\push-auth\` → `crates\push-auth-core\` (`git mv`)
- Move: `crates\push-auth-core\src\main.rs` → `crates\push-auth\src\main.rs`
- Create: `crates\push-auth\Cargo.toml`
- Modify: `Cargo.toml`, `crates\push-auth-core\Cargo.toml`, `crates\push-gateway\Cargo.toml`, `crates\push-worker\Cargo.toml`, `crates\push-smoke\Cargo.toml`, every `push_auth::` path, `deploy\docker\Dockerfile` (comment), `deploy\minikube\*` (comments)

- [ ] **Step 1: Move with history.**

```powershell
git mv crates/push-auth crates/push-auth-core
New-Item -ItemType Directory crates/push-auth/src | Out-Null
git mv crates/push-auth-core/src/main.rs crates/push-auth/src/main.rs
```

- [ ] **Step 2: The core's manifest.** In `crates\push-auth-core\Cargo.toml` change the
package name and description:

```toml
[package]
name = "push-auth-core"
description = "The push-messenger auth service as a library: accounts, email links, devices, tokens and pluggable sign-in."
```

and the feature comment to "The stub OpenID issuer and RSA test keys of Microsoft Entra ID
sign-in, for tests in other crates (push-smoke, push-auth's example). Never enabled by a
service binary."

- [ ] **Step 3: The thin binary.** Create `crates\push-auth\Cargo.toml`:

```toml
[package]
name = "push-auth"
description = "The push-messenger auth service: push-auth-core with the built-in sign-in."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
anyhow.workspace = true
push-auth-core.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread"] }
tokio-util.workspace = true

[lints]
workspace = true
```

and in `crates\push-auth\src\main.rs` replace every `push_auth::` by `push_auth_core::`.

- [ ] **Step 4: Workspace and dependents.** In `Cargo.toml` add the member
`"crates/push-auth-core",` after `"crates/push-auth",` and replace the dependency line
`push-auth = { path = "crates/push-auth" }` by
`push-auth-core = { path = "crates/push-auth-core" }`. In `crates\push-gateway\Cargo.toml`
and `crates\push-worker\Cargo.toml` replace `push-auth.workspace = true` by
`push-auth-core.workspace = true`; in `crates\push-smoke\Cargo.toml`
`push-auth = { workspace = true, features = ["testing"] }` by
`push-auth-core = { workspace = true, features = ["testing"] }`. Then:

```powershell
$files = git grep -l "push_auth::" -- crates deploy
foreach ($file in $files) {
    $text = [IO.File]::ReadAllText($file)
    [IO.File]::WriteAllText($file, $text.Replace('push_auth::', 'push_auth_core::'))
}
```

In `deploy\docker\Dockerfile` change the comment "push-auth's `testing` feature" to
"push-auth-core's `testing` feature"; nothing else in the Dockerfile changes.

- [ ] **Step 5: Verify nothing changed.**

```powershell
$env:CARGO_INCREMENTAL = '0'
cargo fmt --all
cargo build -p push-auth -j 4
cargo test --workspace --locked -j 4
cargo tree -p push-auth --depth 1
```

Expected: `cargo build` updates `Cargo.lock` (a new `push-auth-core` package, `push-auth`
depending on it); then the same test counts as before the move; `push-auth` depends on
`push-auth-core` without its `testing` feature (`cargo tree -p push-auth -e features -i
push-auth-core` lists no `testing`).

- [ ] **Step 6: Commit.**

```powershell
git add -A
git commit -m "refactor: split push-auth into push-auth-core and a thin push-auth binary" -m "No behaviour change: the binary, image and chart keep the name push-auth (spec D1)." -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 2: The principal and the authenticator; Entra ID is one

**Files:**
- Create: `crates\push-auth-core\src\identity\mod.rs`
- Modify: `crates\push-auth-core\src\lib.rs`, `src\entra\mod.rs`, `src\entra\claims.rs`

- [ ] **Step 1: Write the failing test** in `src\entra\mod.rs`'s tests module:

```rust
    #[tokio::test]
    async fn as_an_authenticator_it_names_the_tenant_and_object_id_with_every_claim() {
        let (stub, entra) = verifier().await;
        let checker: &dyn crate::identity::Authenticator = &entra;
        assert_eq!(checker.provider(), "entra");
        assert_eq!(checker.max_session(), Duration::from_secs(24 * 3600));
        let principal = checker.authenticate(&stub.token(USER)).await.unwrap();
        assert_eq!(principal.provider, "entra");
        assert_eq!(principal.issuer, TENANT);
        assert_eq!(principal.subject, USER);
        assert_eq!(principal.name.as_deref(), Some("Test User"));
        assert_eq!(principal.email.as_deref(), Some("test.user@contoso.test"));
        assert_eq!(principal.claims["scp"], "access_as_user");
        assert_eq!(principal.claims["azp"], CLIENT_APP);
        let shown = format!("{principal:?}");
        assert!(shown.contains(TENANT) && !shown.contains(USER), "{shown}");
        assert!(!shown.contains("Test User"), "{shown}");

        let mut claims = stub.user_claims(TENANT, USER);
        claims["aud"] = json!(CLIENT_APP);
        assert_eq!(
            checker.authenticate(&sign(KEY_A, &claims)).await,
            Err(crate::identity::AuthenticationError::Rejected("audience"))
        );
        let (down, cold) = verifier().await;
        down.set_failing(true);
        assert!(matches!(
            cold.authenticate(&down.token(USER)).await,
            Err(crate::identity::AuthenticationError::Unavailable(_))
        ));
    }
```

Run: `cargo test -p push-auth-core --lib -j 4 entra::tests::as_an_authenticator`.
Expected: does not compile (`crate::identity` does not exist).

- [ ] **Step 2: The identity module.** Create `src\identity\mod.rs`:

```rust
//! Pluggable identity (spec 2026-10-19 pluggable identity, D2 to D5): who vouches for a
//! user ([`Authenticator`]) and who may sign in ([`Admission`]). The built-in Microsoft
//! Entra ID check ([`crate::entra::EntraSignIn`]) is one authenticator; a binary of one's
//! own passes others to [`crate::serve_with`]. Tokens, subjects, names and email
//! addresses are never logged.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};

/// The provider id of the built-in Microsoft Entra ID check.
pub const ENTRA_PROVIDER: &str = "entra";

/// The longest session an authenticator may grant: 30 days, as for Entra ID.
pub const MAX_SESSION_LIMIT: Duration = Duration::from_secs(720 * 3600);

/// The longest issuer and subject the account store keeps, in characters.
pub const MAX_ISSUER_CHARS: usize = 256;
pub const MAX_SUBJECT_CHARS: usize = 128;

/// A user an [`Authenticator`] vouches for. Accounts are found by `(provider, issuer,
/// subject)`, never by the address. `Debug` shows the provider and the issuer only.
#[derive(Clone, PartialEq)]
pub struct Principal {
    /// The authenticator's provider id ([`is_provider_id`]), e.g. `entra`.
    pub provider: String,
    /// The stable key of the issuer (Entra ID: the tenant id, a lower-case GUID); 1 to 256
    /// characters.
    pub issuer: String,
    /// The user's immutable id at that issuer (Entra ID: `oid`); 1 to 128 characters.
    pub subject: String,
    /// A display-name suggestion for a new account; informational.
    pub name: Option<String>,
    /// The user's address; informational, shown to its owner only.
    pub email: Option<String>,
    /// Every verified claim of the token, for an [`Admission`] to decide on.
    pub claims: Map<String, Value>,
}

impl fmt::Debug for Principal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Principal")
            .field("provider", &self.provider)
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

/// Why an authenticator gave no principal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthenticationError {
    /// The token is not acceptable; the reason goes to the security log only.
    #[error("the token is refused ({0})")]
    Rejected(&'static str),
    /// The provider cannot check tokens now (its keys cannot be fetched, …). The text
    /// must not hold the token or its claims.
    #[error("the identity provider is unavailable: {0}")]
    Unavailable(String),
}

/// Checks a token and names its user (spec D3). An implementation must check the
/// signature and algorithm, the issuer, the audience (this API), the lifetime, that it is
/// a delegated user token, and give a stable, immutable subject; it must never log the
/// token or its claims.
#[async_trait]
pub trait Authenticator: Send + Sync + fmt::Debug {
    /// The provider id: the route is `/v1/auth/{provider}`.
    fn provider(&self) -> &str;
    /// How long a device signed in through it stays signed in (1 s to 720 h).
    fn max_session(&self) -> Duration;
    /// The user `token` names, if it is acceptable.
    async fn authenticate(&self, token: &str) -> Result<Principal, AuthenticationError>;
    /// Prepares at start-up (fetches keys); failures are logged, never fatal.
    async fn warm(&self) {}
}
```

and in `src\lib.rs` add `pub mod identity;` after `mod external;` and
`pub use identity::{AuthenticationError, Authenticator, ENTRA_PROVIDER, Principal};` plus
`pub use async_trait::async_trait;` (so a wrapper needs no version of its own).

- [ ] **Step 3: Entra ID as an authenticator.** In `src\entra\mod.rs`:
  - `check` decodes `jsonwebtoken::decode::<serde_json::Map<String, Value>>`, then
    `serde_json::from_value::<claims::EntraClaims>(Value::Object(payload.clone()))`
    (a wrong type is `Reason::Malformed`, as before) and returns
    `(EntraIdentity, Map<String, Value>)`; `verify` returns `.map(|(identity, _)| identity)`
    and keeps its logging.
  - `impl EntraIdentity { pub fn principal(&self, claims: Map<String, Value>) -> Principal }`:
    provider `entra`, issuer `tenant_id.to_string()`, subject `object_id.to_string()`.
  - The implementation:

```rust
#[async_trait]
impl Authenticator for EntraSignIn {
    fn provider(&self) -> &str {
        ENTRA_PROVIDER
    }

    fn max_session(&self) -> Duration {
        self.settings.max_session
    }

    /// As [`EntraSignIn::verify`], but the refusal is logged by the identity middleware.
    async fn authenticate(&self, token: &str) -> Result<Principal, AuthenticationError> {
        match self.check(token).await {
            Ok((identity, claims)) => Ok(identity.principal(claims)),
            Err(EntraError::Rejected(reason)) => Err(AuthenticationError::Rejected(reason.as_str())),
            Err(EntraError::Unavailable(error)) => {
                tracing::warn!(%error, "an Entra ID sign-in failed: no usable signing keys");
                Err(AuthenticationError::Unavailable(error))
            }
        }
    }

    async fn warm(&self) {
        self.keys.warm().await;
    }
}
```

  - Move `informational` from `claims.rs` to `identity` as `pub(crate) fn informational`
    and import it in `claims.rs`.

- [ ] **Step 4: Run** `cargo test -p push-auth-core --lib -j 4 entra::`. Expected: all pass.

- [ ] **Step 5: Commit** `feat(auth): Principal and Authenticator; Entra ID sign-in implements it`.

---

### Task 3: Admission, and the authenticators a service offers

**Files:**
- Modify: `src\identity\mod.rs`, `src\service.rs`, `src\lib.rs`
- Create: `src\tests\identity.rs`; modify `src\tests\mod.rs`

- [ ] **Step 1: The fakes.** In `src\tests\mod.rs` add `mod identity;` and:

```rust
/// A test authenticator: `ok|issuer|subject|email|group,group` names that user (name
/// `Acme User`, claims `iss`, `sub`, `groups`), `foreign|…` names one of another provider,
/// `down` is unavailable, anything else is refused (`signature`). It counts its calls.
#[derive(Debug)]
pub(crate) struct FakeIdentity {
    pub provider: String,
    pub calls: AtomicUsize,
}

/// An admission requiring `group` in the `groups` claim; `Unavailable` while `down`.
/// It keeps the claims it saw.
#[derive(Debug)]
pub(crate) struct RequireGroup {
    pub group: String,
    pub down: AtomicBool,
    pub seen: Mutex<Vec<Map<String, Value>>>,
}
```

(with `FakeIdentity::new(provider) -> Arc<Self>`, `RequireGroup::new(group) -> Arc<Self>`,
`fake_token(issuer, subject, email, groups) -> String`, and the two trait implementations;
`max_session` is two hours).

- [ ] **Step 2: Write the failing test** `src\tests\identity.rs`:

```rust
#[tokio::test]
async fn a_service_offers_entra_and_the_external_authenticator_by_their_ids() {
    let pool = PgPool::connect_lazy("postgres://127.0.0.1:1/none").unwrap();
    let stub = StubOpenId::start().await;
    let entra = EntraSignIn::with_timing(stub.settings(), quick()).unwrap();
    let plain = service(pool.clone(), SignIn::default());
    assert!(plain.authenticators().is_empty());
    assert!(plain.authenticator("entra").is_none());
    let both = service(pool, SignIn { password: true, entra: Some(entra) })
        .with_external(FakeIdentity::new("acme"))
        .with_admission(RequireGroup::new("staff"));
    let ids: Vec<String> = both.authenticators().iter().map(|a| a.provider().to_owned()).collect();
    assert_eq!(ids, ["entra", "acme"]);
    assert_eq!(both.authenticator("acme").unwrap().max_session(), Duration::from_secs(7200));
    assert!(both.authenticator("other").is_none());
    assert!(both.admission().is_some());
}
```

Expected: does not compile.

- [ ] **Step 3: Admission.** In `src\identity\mod.rs`:

```rust
/// Whether a principal may sign in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    Admit,
    /// 403 `sign_in_not_permitted`; no account is created.
    Refuse,
    /// The decision cannot be made now: 503, retryable.
    Unavailable,
}

/// Decides, after authentication and before any account is found or created, whether a
/// principal may sign in (spec D4): an Entra group, an app role, an allow-list.
#[async_trait]
pub trait Admission: Send + Sync + fmt::Debug {
    async fn admit(&self, principal: &Principal) -> AdmissionDecision;
}
```

- [ ] **Step 4: The service holds them.** In `src\service.rs` add to `Auth` a field
`identity: Identity` (`#[derive(Debug, Clone, Default)] struct Identity { external:
Option<Arc<dyn Authenticator>>, admission: Option<Arc<dyn Admission>> }`) and:

```rust
    /// The service also offering `authenticator`'s sign-in (spec D6, D7).
    pub fn with_external(mut self, authenticator: Arc<dyn Authenticator>) -> Self { … }
    /// The service asking `admission` before any account is found or created (spec D4).
    pub fn with_admission(mut self, admission: Arc<dyn Admission>) -> Self { … }
    /// Every authenticator offered: Entra ID first, then the external one.
    pub fn authenticators(&self) -> Vec<Arc<dyn Authenticator>> { … }
    /// The authenticator of `provider`, if offered.
    pub fn authenticator(&self, provider: &str) -> Option<Arc<dyn Authenticator>> { … }
    pub fn admission(&self) -> Option<&Arc<dyn Admission>> { … }
```

`SignIn` keeps its two fields, so every existing `SignIn { password, entra }` stays valid.

- [ ] **Step 5: Run** `cargo test -p push-auth-core --lib -j 4 tests::identity`. Expected: pass.

- [ ] **Step 6: Commit** `feat(auth): Admission, and the authenticators a service offers`.

---

### Task 4: Migration 0005: any provider, the issuer as text

**Files:**
- Create: `crates\push-auth-core\migrations\0005_external_provider.sql`
- Modify: `crates\push-auth-core\tests\schema.rs`

- [ ] **Step 1: Write the failing upgrade test** in `tests\schema.rs`:

```rust
/// Plan 13A's Entra identities survive migration 0005 unchanged (spec AC6), and the new
/// rules hold: any provider id of the pattern, an issuer of 1 to 256 characters.
#[tokio::test]
async fn migration_0005_keeps_entra_identities_and_admits_other_providers() {
    let (_container, url) = database().await;
    // Versions 1 to 4 only: a database as Plan 13A left it.
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR.iter().filter(|m| m.version <= 4).cloned().collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    let pool = PgPool::connect(&url).await.unwrap();
    before.run(&pool).await.unwrap();
    // An Entra account, as external.rs wrote it.
    …insert user_account erin, external_identity (provider 'entra', tenant_id TENANT, subject OID)…
    migrate_database(&url).await.unwrap();
    let row: (String, String, String, Uuid, Option<String>) = sqlx::query_as(
        "SELECT provider, issuer, subject, user_id, email FROM auth.external_identity",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(row, ("entra".into(), TENANT.into(), OID.into(), erin, Some("erin@contoso.test".into())));
    // Another provider, a URL issuer, the same subject: a different identity.
    insert ("acme-id", "https://id.example.com/", OID) → ok;
    insert ("acme-id", "https://id.example.com/", OID) again → external_identity_provider_issuer_subject_key;
    insert ("Acme", …) → external_identity_provider_check;
    insert ("a".repeat(33)…) → external_identity_provider_check;
    insert issuer "" and issuer "x".repeat(257) → external_identity_issuer_check;
}
```

Run `cargo test -p push-auth-core --test schema -j 4`.
Expected: fails (no column `issuer`).

- [ ] **Step 2: The migration** `0005_external_provider.sql`:

```sql
-- Pluggable identity (spec 2026-10-19 pluggable identity, D8): an external identity is
-- (provider, issuer, subject) for any provider id, not only Entra ID's (tenant, object id).
-- Existing Entra rows keep their identity: the tenant id becomes its text form, the issuer.

ALTER TABLE auth.external_identity DROP CONSTRAINT external_identity_provider_check;
ALTER TABLE auth.external_identity ADD CONSTRAINT external_identity_provider_check
    CHECK (provider ~ '^[a-z][a-z0-9-]{0,31}$');
ALTER TABLE auth.external_identity
    DROP CONSTRAINT external_identity_provider_tenant_id_subject_key;
ALTER TABLE auth.external_identity ALTER COLUMN tenant_id TYPE text USING tenant_id::text;
ALTER TABLE auth.external_identity RENAME COLUMN tenant_id TO issuer;
-- Entra ID: the tenant id; another provider: its stable issuer key.
ALTER TABLE auth.external_identity ADD CONSTRAINT external_identity_issuer_check
    CHECK (length(issuer) BETWEEN 1 AND 256);
ALTER TABLE auth.external_identity ADD CONSTRAINT external_identity_provider_issuer_subject_key
    UNIQUE (provider, issuer, subject);
```

- [ ] **Step 3: The existing schema test.** In `tests\schema.rs`'s external-account test
the insert names `issuer` (bound as `tenant.to_string()`) and the expected constraint is
`external_identity_provider_issuer_subject_key`.

- [ ] **Step 4: The code that names the column** (`src\external.rs`, `src\admin.rs`) still
says `tenant_id`; Task 5 and Task 9 change it. Until then bind `tenant_id.to_string()` to
`issuer` in both so the crate's tests pass:
`cargo test -p push-auth-core -j 4`. Expected: all pass.

- [ ] **Step 5: Commit** `feat(auth): migration 0005, external identities of any provider`.

---

### Task 5: Accounts by (provider, issuer, subject)

**Files:**
- Modify: `src\external.rs`, `src\devices.rs`, `src\lib.rs`
- Create: `src\tests\identity_accounts.rs`

- [ ] **Step 1: Write the failing tests** (`src\tests\identity_accounts.rs`, a world with
password sign-in and `FakeIdentity::new("acme")`):
  - `an_external_principal_gets_one_account_whatever_its_name_or_address`: sign in with
    `principal(issuer, subject, "a@x")`, register a device; sign in again with a changed
    name and email: the same `user_id`; `auth.external_identity` holds one row with the
    new email; the outbox holds one `user.verified`.
  - `the_provider_and_the_issuer_are_part_of_the_identity`: the same subject at another
    issuer, and as provider `entra` (with Entra offered), are other accounts.
  - `a_registration_token_of_a_provider_no_longer_offered_registers_nothing`: sign in
    through `acme`, then `register_device` on a service without the external authenticator:
    `Unauthorized`.
  - `a_principal_that_breaks_the_rules_is_refused`: a subject of 129 characters, an empty
    issuer: `Unauthorized`, no account.

Expected: does not compile (`sign_in_external`).

- [ ] **Step 2: The code.** `src\external.rs`:

```rust
/// What an external sign-in without a device gives: the account and a registration token.
pub struct ExternalRegistration { pub user_id: Uuid, pub registration: RegistrationToken }
/// Plan 13A's name.
pub type EntraRegistration = ExternalRegistration;

impl Auth {
    pub async fn sign_in_external(&self, principal: &Principal, handle: Option<&str>)
        -> Result<ExternalRegistration, AuthError>;
    pub async fn sign_in_external_device(&self, principal: &Principal, handle: Option<&str>,
        device_id: Uuid) -> Result<Tokens, AuthError>;
    /// Plan 13A's entry points, kept: the principal of `identity`.
    pub async fn sign_in_entra(&self, identity: &EntraIdentity, handle: Option<&str>)
        -> Result<EntraRegistration, AuthError>;
    pub async fn sign_in_entra_device(&self, identity: &EntraIdentity, handle: Option<&str>,
        device_id: Uuid) -> Result<Tokens, AuthError>;
}
```

  The session's maximum age is the offered authenticator's (`self.authenticator(provider)`,
  `Unauthorized` if none). The principal is checked (`Principal::checked`: provider id, 1
  to 256 issuer characters, 1 to 128 subject characters, no control characters; name and
  email kept only if informational) before any query. Find:

```sql
UPDATE auth.external_identity SET last_sign_in_at = now(), email = $4
WHERE provider = $1 AND issuer = $2 AND subject = $3 RETURNING identity_id, user_id
```

  Create: `INSERT … (identity_id, provider, issuer, subject, user_id, email) … ON CONFLICT
  (provider, issuer, subject) DO NOTHING RETURNING 1`. Provisioning logs
  `auth.entra.provisioned` (`user_id`, `tenant`) for `entra` as before, else
  `auth.external.provisioned` (`user_id`, `provider`, `issuer`).

  `src\devices.rs` `registration_binding`: an identity-bound token registers only if the
  identity's provider is still offered:

```rust
(None, Some(identity), Some(session)) if !self.authenticators().is_empty() => {
    let provider: Option<String> = sqlx::query_scalar(
        "SELECT provider FROM auth.external_identity WHERE identity_id = $1 AND user_id = $2",
    ) …;
    match provider {
        Some(provider) if self.authenticator(&provider).is_some() => Ok(Some(session)),
        _ => Err(AuthError::Unauthorized),
    }
}
```

- [ ] **Step 3: Run** `cargo test -p push-auth-core -j 4 tests::`. Expected: the new tests
and every `entra_*` test pass.

- [ ] **Step 4: Commit** `feat(auth): external accounts by (provider, issuer, subject)`.

---

### Task 6: Error codes and security events

(Before the middleware, which answers with them.)

**Files:**
- Modify: `src\error.rs`, `src\api.rs`, `src\audit.rs`, `src\identity\mod.rs`

- [ ] **Step 1: Write the failing tests.** In `src\api.rs`'s tests:

```rust
    #[test]
    fn identity_refusals_answer_their_codes() {
        for (error, status, code) in [
            (AuthError::TokenInvalid, 401, "token_invalid"),
            (AuthError::IdentityUnavailable, 503, "identity_unavailable"),
            (AuthError::SignInNotPermitted, 403, "sign_in_not_permitted"),
        ] {
            assert_eq!((error.status(), error.code()), (status, code));
        }
        let response = AuthError::TokenInvalid.into_response();
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
        let response = AuthError::IdentityUnavailable.into_response();
        assert_eq!(response.headers()[RETRY_AFTER], "5");
        assert!(AuthError::IdentityUnavailable.retryable());
        assert!(!AuthError::SignInNotPermitted.retryable());
    }
```

and in `src\audit.rs`'s tests `external_sign_ins_have_events_of_their_own`:
`external_event(200, None) == Some("auth.external.success")`, `(401, token_invalid)` and
`(403, sign_in_not_permitted)` are `auth.external.failure`, 400 and 503 are `None`, 429 is
`auth.rate_limited`. Expected: does not compile.

- [ ] **Step 2: The codes** in `src\error.rs`: `TokenInvalid` ("the identity token is not
accepted", 401, `token_invalid`), `IdentityUnavailable` ("the identity provider cannot check
the token now; try again", 503, `identity_unavailable`, retryable), `SignInNotPermitted`
("this account may not sign in here", 403, `sign_in_not_permitted`); `api.rs` adds
`WWW-Authenticate: Bearer` to `TokenInvalid` and `Retry-After: 5` to `IdentityUnavailable`.

- [ ] **Step 3: The events.** In `src\identity\mod.rs`:

```rust
/// A refused token: logged (`auth.entra.rejected` for Entra ID, as before; else
/// `auth.external.rejected` with the provider) and answered (spec D9).
pub(crate) fn rejected(provider: &str, reason: &'static str) -> AuthError { … }
/// The provider could not check the token: `entra_unavailable` or `identity_unavailable`.
pub(crate) fn unavailable(provider: &str) -> AuthError { … }
/// An admission refusal: `auth.external.not_permitted` with the provider and the issuer.
pub(crate) fn not_permitted(principal: &Principal) -> AuthError { … }
```

  `src\audit.rs`: `pub(crate) fn external_event(status, code)` and, in `audit`, the route
  of the external authenticator (`Auth::external_route`, `None` when its id is `entra`)
  uses it instead of `event`.

- [ ] **Step 4: Run** `cargo test -p push-auth-core --lib -j 4`. Expected: pass.

- [ ] **Step 5: Commit** `feat(auth): error codes and security events of external sign-in`.

---

### Task 7: The identity middleware on /v1/auth/{provider}

**Files:**
- Create: `src\identity\middleware.rs`, `src\tests\identity_api.rs`
- Modify: `src\api.rs`, `src\identity\mod.rs`, `src\tests\mod.rs`

- [ ] **Step 1: Write the failing tests** (`src\tests\identity_api.rs`, through the router;
`world_with(offer, external, admission)` in `src\tests\mod.rs`):
  - `the_token_comes_from_the_header_or_the_body_and_conflicts_are_refused` (AC3): Entra:
    header only (body `{}`) → 200; body only → 200; both equal → 200; both different → 400
    `invalid`, no key fetched for it; a header that is not `Bearer …` → 400; two
    `Authorization` headers → 400; a header token over 16 KiB → 400; no token → 400.
  - `sign_ins_are_rate_limited_before_the_authenticator_is_asked` (AC3): 60 `bad` tokens on
    `/v1/auth/acme` → 401 `token_invalid`; the 61st → 429; `calls == 60`.
  - `an_external_provider_signs_in_registers_and_renews` (AC2): `/v1/auth/acme` → a
    registration token, a device, `device_id` renewal; `bad` → 401 `token_invalid` with
    `WWW-Authenticate: Bearer`; `down` → 503 `identity_unavailable`; `foreign|…` → 401;
    `/v1/auth/entra` → 404 without Entra.
  - `an_admission_refusal_creates_nothing_and_its_outage_is_retryable` (AC4): no group → 403
    `sign_in_not_permitted`, no account; with the group → 200; the device renewal of a user
    removed from the group → 403 and the device's refresh token still works only until its
    session (nothing new issued); `down` → 503 `identity_unavailable`; the admission saw
    the `groups` claim; the same for Entra (`groups` in the stub's claims).
  - `external_sign_ins_are_logged_without_tokens_subjects_names_or_addresses` (AC7):
    `auth.external.provisioned`, `auth.external.success` (route `/v1/auth/acme`),
    `auth.external.rejected` (`provider: acme`, `reason: signature`),
    `auth.external.not_permitted` (`provider`, `issuer`), `auth.external.failure`; the
    captured text holds no token, subject, email or `Acme User`.

Expected: do not compile (`world_with`).

- [ ] **Step 2: The middleware** `src\identity\middleware.rs`:

```rust
//! The identity middleware (spec D5) on `POST /v1/auth/{provider}`: (1) the per-IP
//! sign-in limit; (2) the token from `Authorization: Bearer`, else the JSON body's `token`
//! (the body, at most 16 KiB, is handed on unchanged; both present and different is
//! `invalid`); (3) the authenticator; (4) the admission; (5) the principal in the
//! request's extensions; (6) the per-identity limit.

#[derive(Clone)]
pub(crate) struct IdentityState {
    pub api: Api,
    pub authenticator: Arc<dyn Authenticator>,
}

pub(crate) async fn identify(
    State(state): State<IdentityState>,
    request: Request,
    next: Next,
) -> Result<Response, AuthError> {
    let IdentityState { api, authenticator } = state;
    let provider = authenticator.provider().to_owned();
    let ip = client_ip(&api, request.headers(), request.extensions());
    api.limits.check(Action::EntraSignIn, ip, None).await?;
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, MAX_BODY_BYTES).await.map_err(|_| bad_body())?;
    if !is_json(&parts.headers) {
        return Err(bad_body());
    }
    let field: TokenField = serde_json::from_slice(&bytes).map_err(|_| bad_body())?;
    let token = match (header_token(&parts.headers)?, field.token) {
        (Some(header), Some(body)) if header != body => {
            return Err(invalid("the Authorization header and the body carry different tokens"));
        }
        (Some(header), _) => header,
        (None, Some(body)) => body,
        (None, None) => return Err(bad_body()),
    };
    if token.len() > MAX_TOKEN_BYTES {
        return Err(invalid("the token is longer than 16 KiB"));
    }
    let principal = match authenticator.authenticate(&token).await {
        Ok(principal) => principal
            .checked(&provider)
            .ok_or_else(|| rejected(&provider, "principal"))?,
        Err(AuthenticationError::Rejected(reason)) => return Err(rejected(&provider, reason)),
        Err(AuthenticationError::Unavailable(_)) => return Err(unavailable(&provider)),
    };
    if let Some(admission) = api.auth.admission() {
        match admission.admit(&principal).await {
            AdmissionDecision::Admit => {}
            AdmissionDecision::Refuse => return Err(not_permitted(&principal)),
            AdmissionDecision::Unavailable => return Err(admission_unavailable(&provider)),
        }
    }
    let account = limit_key(&principal);
    let mut request = Request::from_parts(parts, Body::from(bytes));
    request.extensions_mut().insert(principal);
    api.limits.check(Action::EntraAccount, None, Some(&account)).await?;
    Ok(next.run(request).await)
}
```

  `limit_key` is `{provider}:{issuer}:{subject}` with `%` and `:` of the issuer escaped
  (`%25`, `%3A`), so Entra's keys stay `entra:{tid}:{oid}`. `header_token`: none without
  `Authorization`; `Some` for exactly one `Bearer <non-empty>`; else `invalid`.

- [ ] **Step 3: The routes and the handler** in `src\api.rs`:

```rust
    for authenticator in api.auth.authenticators() {
        let path = format!("/v1/auth/{}", authenticator.provider());
        let state = IdentityState { api: api.clone(), authenticator };
        routes = routes.route(
            &path,
            post(sign_in).route_layer(axum::middleware::from_fn_with_state(state, identify)),
        );
    }
```

  `sign_in(State(api), Extension(principal), JsonBody(body: SignInBody { handle,
  device_id }))` calls `sign_in_external` or `sign_in_external_device` as `entra` did.

- [ ] **Step 4: Run** `cargo test -p push-auth-core -j 4 tests::`. Expected: the new tests
and every `entra_api` test pass unchanged.

- [ ] **Step 5: Commit** `feat(auth): the identity middleware on /v1/auth/{provider}`.

---

### Task 8: PUSH_AUTH_PROVIDERS=external, Overrides and serve_with

**Files:**
- Modify: `src\entra\settings.rs`, `src\config.rs`, `src\server.rs`, `src\lib.rs`
- Create: `src\tests\identity_server.rs`

- [ ] **Step 1: Write the failing tests.**
  - `settings.rs`: `Providers::parse(Some("external"))` is `{password: false, entra: false,
    external: true}`; `"password,entra,external"` all three; `"external,external"` and
    `"oidc"` refused. Existing literals gain `external: false`.
  - `config.rs`: with `PUSH_AUTH_PROVIDERS=external` and no `PUSH_ENTRA_*`, the config is
    valid and `entra` is `None`.
  - `server.rs`: `check_overrides` refuses `external` without an authenticator, an
    authenticator without `external`, an id that is not a provider id (`Acme`, `a b`, 33
    characters), a reserved id (`login`, `refresh`, …), `entra` twice, a maximum session
    of 0 or over 720 h, and an admission without `entra` or `external`; accepts each valid
    pairing. Every message names `PUSH_AUTH_PROVIDERS` or `Overrides`.
  - `src\tests\identity_server.rs`: `serve_with` with `external` and no authenticator
    returns that error at once (no datastore is contacted: the database URL points to
    port 1); the whole service with Postgres and Redis, `PUSH_AUTH_PROVIDERS=external`
    and `FakeIdentity::new("acme")` signs a user in over a socket with the header and
    answers 404 on `/v1/auth/login` and `/v1/auth/entra`.

- [ ] **Step 2: The code.** `Providers { password, entra, external }` parses `external`
  (message "expected password, entra, external or a comma-separated combination"); the
  config table documents it. `EntraSettings::from_env()` for wrappers. In `src\server.rs`:

```rust
/// What a binary of one's own substitutes (spec D6): an authenticator, served when
/// `PUSH_AUTH_PROVIDERS` names `external`, and an admission for every sign-in by token.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub authenticator: Option<Arc<dyn Authenticator>>,
    pub admission: Option<Arc<dyn Admission>>,
}

pub async fn serve(config, http, health, shutdown) -> anyhow::Result<()> {
    serve_with(config, http, health, shutdown, Overrides::default()).await
}

pub async fn serve_with(config, http, health, shutdown, overrides: Overrides)
    -> anyhow::Result<()>
```

  `check_overrides(&config.providers, &overrides)` runs first, before the health listener
  serves; every authenticator is warmed; `Auth` gets `with_external` and `with_admission`.

- [ ] **Step 3: Run** `cargo test -p push-auth-core -j 4`. Expected: pass.

- [ ] **Step 4: Commit** `feat(auth): PUSH_AUTH_PROVIDERS=external, Overrides and serve_with`.

---

### Task 9: The admin API's external selector

**Files:** Modify `src\admin.rs`, `src\tests\admin.rs`.

- [ ] **Step 1: Write the failing test** `an_operator_signs_out_an_external_identity`: an
  `acme` user with a device; `{external: {provider: "acme", issuer, subject}}` →
  `devices_revoked: 1` and the refresh token refused; the event's `selector` is
  `external`; `{entra: …}` still works after migration 0005 (the existing test); bad
  selectors → 400 (`provider: "Acme"`, a subject of 129 characters, an empty issuer, two
  selectors, an unknown field); an unknown identity → 404 `user_not_found`.

- [ ] **Step 2: The code.** `UserSelector::External { provider, issuer, subject }`;
  `RevokeBody.external: Option<ExternalSubject>` (`deny_unknown_fields`); the messages say
  "exactly one of user_id, handle, entra or external"; the Entra query is `provider =
  'entra' AND issuer = $1 AND subject = $2` with the tenant's text form.

- [ ] **Step 3: Run** `cargo test -p push-auth-core --lib -j 4 tests::admin`. Expected: pass.

- [ ] **Step 4: Commit** `feat(auth): the admin API revokes by an external identity`.

---

### Task 10: The example wrapper

**Files:**
- Create: `crates\push-auth\examples\custom_identity.rs`
- Modify: `crates\push-auth\Cargo.toml`

- [ ] **Step 1: The manifest.**

```toml
[dev-dependencies]
push-auth-core = { workspace = true, features = ["testing"] }
serde_json.workspace = true

[[example]]
name = "custom_identity"
# Its tests run with `cargo test`; `cargo build -p push-auth` never builds it.
test = true
```

- [ ] **Step 2: Write the example with its failing tests first** (the test module at the
  bottom of the file): with the stub issuer, a member of the group is admitted, a
  non-member is refused by the admission, a guest (`acct: 1`) is rejected with `guest`,
  and the wrapper keeps the provider id `entra`.

- [ ] **Step 3: The wrapper**: `NoGuests { entra: EntraSignIn }` (provider `entra`, Entra's
  maximum session, `authenticate` delegates to the built-in check and refuses `acct == 1`,
  `warm` delegates) and `GroupMember { group }` (an `Admission` on the `groups` claim);
  `main` reads `Config::from_env()`, `EntraSettings::from_env()` and the required
  `EXAMPLE_REQUIRED_GROUP`, and calls `serve_with(.., Overrides { authenticator, admission })`.
  `push-auth migrate` is kept.

- [ ] **Step 4: Run**

```powershell
cargo test -p push-auth -j 4
cargo clippy -p push-auth --all-targets --locked -j 4 -- -D warnings
```

Expected: the example's tests pass; no warning.

- [ ] **Step 5: Commit** `docs(auth): an example push-auth with its own authenticator and admission`.

---

### Task 11: Documentation and status

**Files:** `docs\entra-sign-in.md`, `README.md`, `docs\superpowers\plans\2026-10-11-roadmap.md`,
`docs\superpowers\specs\2026-10-19-pluggable-identity-design.md`, `src\config.rs` (table).

- [ ] **Step 1:** `docs\entra-sign-in.md` gains "Your own wrapper": the two crates, the
  example, building an image from it (a Dockerfile stage that builds `--example
  custom_identity` or the consumer's own crate depending on `push-auth-core`), `images.auth`
  in the chart, `PUSH_AUTH_PROVIDERS=external`, the duties of a custom authenticator
  (signature and algorithm, issuer, audience, lifetime, delegated user token, a stable
  immutable subject, no token in logs), the admission's answers (403, 503), and the error
  codes and security events. The admin section names the `external` selector.
- [ ] **Step 2:** `README.md`: `PUSH_AUTH_PROVIDERS` lists `external`; a pointer to the
  wrapper section; the admin token row names the `external` selector.
- [ ] **Step 3:** the roadmap gains row 14 "In progress"; the spec's status line names the
  plan as in progress.
- [ ] **Step 4: Commit** `docs: your own push-auth wrapper; plan 14 in progress`.

---

### Task 12: Verification and delivery

- [ ] **Step 1: The full checks.**

```powershell
$env:CARGO_INCREMENTAL = '0'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings
cargo test --workspace --locked -j 4
cargo +1.85 build --workspace --locked -j 4
cargo tree -i openssl-sys --workspace
cargo tree -i aws-lc-rs --workspace
pwsh deploy/helm/test-chart.ps1
cargo test --locked -p push-sdk-c -p push-sdk-ffi -j 4
```

and in `bindings\node`: `npm ci`, `npm test`, `npm run typecheck`. Expected: no change of
format, no warning, every test passes, the MSRV build succeeds, no OpenSSL or aws-lc-rs,
the chart checks pass unchanged.

- [ ] **Step 2: The image.** `docker build -f deploy/docker/Dockerfile --target auth -t
  push-messenger/auth:plan14 .` Expected: builds; `/usr/local/bin/push-auth` exists.
  (Optional on a slow machine; the Dockerfile's commands are unchanged.)

- [ ] **Step 3: Review.** Ask for a code review of `git diff main...HEAD` against this plan
  and the spec; fix what it finds with tests.

- [ ] **Step 4: Deliver** (only when the owner asks): the roadmap row 14 `Done (#N)`, the
  spec's status, push, the PR, CI, squash-merge as `Pluggable identity (Plan 14) (#N)`.

## Self-review against the spec

| Spec item | Task |
|---|---|
| D1 crate split, binary keeps its name, `testing` with the core | 1 |
| D2 `Principal` (provider pattern, issuer, subject, informational name/email, claims; `Debug` provider and issuer) | 2, 5 (checked) |
| D3 `Authenticator`, `AuthenticationError`, `EntraSignIn` implements it | 2 |
| D4 `Admission`, `AdmissionDecision`, before any account | 3, 7 |
| D5 the middleware: per-IP limit, header or body ≤ 16 KiB, conflict 400, authenticate, admit, extension, per-identity limit | 7 |
| D6 `serve_with`, `Overrides`, `serve` = defaults | 8 |
| D7 `external`, `entra` + `external` only with another id, mismatches stop start-up, route by id | 8 |
| D8 migration 0005; admin `entra` and `external` selectors | 4, 9 |
| D9 codes (`entra_token_invalid`/`token_invalid`, `entra_unavailable`/`identity_unavailable`, `sign_in_not_permitted`), events (`auth.entra.rejected`, `auth.external.rejected`, `auth.external.not_permitted`) | 6, 7 |
| D10 example wrapper and guide | 10, 11 |
| AC1 default unchanged | 1 (no behaviour change), every task keeps the existing tests, 12 |
| AC2 external sign-in, one account per identity, 401, same account after a changed name or email | 5, 7, 8 |
| AC3 header or body, conflicts 400, rate limit before authenticating | 7 |
| AC4 admission 403 for a new sign-in and a renewal, 503, sees the claims | 7, 10 |
| AC5 mismatched overrides stop start-up | 8 |
| AC6 migration keeps Entra identities; admin by `entra` and `external` | 4, 9 |
| AC7 no token, subject, name or email in the log | 7 (capture), existing Entra capture tests |
| AC8 example builds (Clippy `--all-targets`) and is documented | 10, 11, 12 |

Decisions this plan adds where the spec is silent (the most conservative option):
provider ids that are routes of their own (`signup`, `verify`, `login`, `password`,
`refresh`, `jwks`) are refused at start-up; an `Admission` without `entra` or `external`
stops start-up; an authenticator's maximum session must be 1 s to 720 h; a principal that
breaks the rules (wrong provider, issuer or subject length, control characters) is a
rejected token (`principal`), never an account; a request whose body is not a JSON object
is `invalid` even with the header token (so the handler reads `handle` and `device_id` as
today); an `Authorization` header that is not exactly one `Bearer <token>` is `invalid`;
an admission's outage answers `identity_unavailable` for every provider; an authenticator's
`Unavailable` text is never logged by the middleware (the built-in Entra check logs its
own, as before).

Type consistency: `Principal { provider, issuer, subject, name, email, claims }`,
`Authenticator::{provider, max_session, authenticate, warm}`,
`AuthenticationError::{Rejected, Unavailable}`, `Admission::admit`,
`AdmissionDecision::{Admit, Refuse, Unavailable}`, `Overrides { authenticator, admission }`,
`serve_with`, `Auth::{with_external, with_admission, authenticators, authenticator,
admission, sign_in_external, sign_in_external_device}`, `ExternalRegistration`,
`UserSelector::External`, `Providers { password, entra, external }`,
`AuthError::{TokenInvalid, IdentityUnavailable, SignInNotPermitted}`, the test helpers
`FakeIdentity`, `RequireGroup`, `fake_token`, `world_with` are used with the same names in
every task.
