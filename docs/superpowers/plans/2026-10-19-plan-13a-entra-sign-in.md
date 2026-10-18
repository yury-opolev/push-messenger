# Plan 13A: Microsoft Entra ID Sign-In (services, SDK, tests) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Work sequentially on one branch.

**Goal:** Let users sign in with Microsoft Entra ID beside (or instead of) email and password: auth-svc checks an Entra access token for its API, finds or creates the user by `(tid, oid)` and hands out today's registration token; Entra-backed devices have a maximum session age; the SDK and its bindings accept an Entra token the app obtained; the default stays exactly today's behaviour.

**Architecture:** A new module `crates\push-auth\src\entra\` holds the settings (`PUSH_AUTH_PROVIDERS`, `PUSH_ENTRA_*`), the claim rules, a per-tenant key cache (OpenID metadata and JWKS over rustls, refreshed on unknown `kid` with a cooldown, stale-if-error, fail closed) and the verifier `EntraSignIn` built on the `jsonwebtoken` 9 (ring) the workspace already uses. `crates\push-auth\src\external.rs` maps identities to accounts in a new `auth.external_identity` table and binds registration tokens to the identity (`xid`, `sx`) instead of the password (`pw`). `auth.device.session_expires_at` ends refresh after the maximum age. The router serves `POST /v1/auth/entra` and drops the email/password routes when `password` is not a provider. The worker gains `PUSH_EMAIL_SENDER=none`. The SDK gains `Client::sign_in_with_entra`. Tests run against a stub OpenID issuer with fixed RSA test keys.

**Tech Stack:** Rust 2024 / MSRV 1.85, Tokio, axum 0.8, sqlx 0.8.6 (PostgreSQL), jsonwebtoken 9.3.1 (RS256 on ring), reqwest 0.12 (`rustls-tls-native-roots`, ring), testcontainers; no new crate.

---

## Authority and boundaries

Read first: `docs\superpowers\specs\2026-10-19-entra-sign-in-design.md` (decisions D1 to
D14 and the configuration tables are binding). Plan 13B (chart, minikube rehearsal) follows
this plan and depends on Plan 12B; nothing here touches `deploy\`.

This plan provisions nothing in Entra ID or Azure and never calls a real Entra endpoint:
every test uses the stub issuer of Task 3. The default configuration must stay byte-for-byte
compatible: the existing tests pass unchanged except where this plan names the edit (a
`LinkClaims` literal gains two fields, `OwnProfile::email` becomes optional, the schema test
lists one more table).

Start from clean, current `main`:

```powershell
git switch main
git pull --ff-only
git switch -c feature/plan-13a-entra-sign-in
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
| The workspace pins `jsonwebtoken = { version = "9", default-features = false }`, locked at 9.3.1 (`rust-version = 1.73`); its crypto is always ring (`src\crypto\rsa.rs` uses `ring::signature`) | `Cargo.toml`, `Cargo.lock`, the crate source | RS256 needs no new crate or feature; no aws-lc-rs, no OpenSSL |
| `EncodingKey::from_rsa_der` takes PKCS#1 DER (`RsaKeyPair::from_der`), `DecodingKey::from_rsa_components(n, e)` takes base64url JWK members; an HS256 token checked with an RSA key fails `InvalidAlgorithm` | `jsonwebtoken-9.3.1\src\encoding.rs`, `decoding.rs`; checked with a scratch program | Test keys are PKCS#1 DER; JWKS keys load from `n`/`e` |
| jsonwebtoken 9.3.1 reports a required claim of the wrong type as `MissingRequiredClaim` | `jsonwebtoken-9.3.1\src\validation.rs:255-270` | `exp`, `nbf`, `iss`, `aud` are required, so a string `exp` is refused |
| ring cannot generate RSA keys; the `rsa` crate carries RUSTSEC-2023-0071 (cargo-deny passes today only because nothing builds `rsa`) | `deny.toml` comment, RustSec | Two fixed RSA-2048 test keys, generated once with .NET, live in `entra\testing.rs` |
| reqwest 0.12.28 reads `HTTP(S)_PROXY`/`NO_PROXY` by default (`auto_sys_proxy`), `rustls-tls-native-roots` brings ring | `reqwest-0.12.28\src\async_impl\client.rs:309,419`, `Cargo.toml` | Entra fetches work through a corporate proxy; push-credentials already uses the same feature |
| `auth.user_account.email` and `password_hash` are `NOT NULL`; `email` is `citext UNIQUE` | `crates\push-auth\migrations\0001_auth.sql` | Migration 0004 makes both nullable under a CHECK |
| `auth.delete_unverified_accounts` deletes accounts with `email_verified_at IS NULL` and no device | `migrations\0002_worker.sql` | Entra accounts are active from creation; the function also gains a `password_hash IS NOT NULL` guard |
| Registration tokens are `LinkClaims { sub, purpose: Register, exp, email: None, pw: Some(stamp) }`; `register_device` refuses a token whose `pw` does not match the current hash | `src\service.rs:381-397`, `src\devices.rs:117-145` | The Entra equivalent binds `xid` (identity row) and `sx` (session end) |
| `refresh` locks the device row first, then the token rows | `src\devices.rs:225-320` | The session check reads `session_expires_at` from the same locked row |
| `/v1/auth/login {email, password, device_id}` signs a known device in again; the SDK uses it first and registers only on `device_not_found` | `src\api.rs:263-290`, `crates\push-sdk\src\client.rs:407-480` | `/v1/auth/entra` mirrors it, so the SDK flow is reused |
| `credentials::is_refused` treats `unauthorized`, `token_expired`, `http_401` as a sign-out | `crates\push-sdk\src\credentials.rs:241-244` | Add `session_expired` |
| push-smoke's `common::lookup` returns the first match | `crates\push-smoke\tests\common\mod.rs:30-37` | Make later entries win, so a test can override what `Stack` sets |
| A package cannot depend on itself to enable its own feature | Plan 12A Task 2 | Entra tests that need the stub live in `src\tests\` (compiled with `cfg(test)`), like push-credentials |

## Files

| File | Change |
|---|---|
| `crates\push-auth\Cargo.toml` | feature `testing`; `reqwest` with `rustls-tls-native-roots` |
| `crates\push-auth\src\entra\mod.rs` | `Reason`, `EntraError`, `EntraIdentity`, `EntraSignIn` |
| `crates\push-auth\src\entra\settings.rs` | `Providers`, `EntraSettings` |
| `crates\push-auth\src\entra\claims.rs` | `EntraClaims`, `check` |
| `crates\push-auth\src\entra\keys.rs` | `KeyTiming`, `KeySets` |
| `crates\push-auth\src\entra\testing.rs` | `StubOpenId`, `KEY_A`, `KEY_B`, token helpers (feature `testing`) |
| `crates\push-auth\migrations\0004_entra.sql` | nullable email/password with a CHECK, `auth.external_identity`, `auth.device.session_expires_at`, cleanup guard |
| `crates\push-auth\src\external.rs` | Entra accounts: find, create, registration, device sign-in |
| `crates\push-auth\src\{config,service,devices,links,profiles,error,api,limits,audit,server,lib}.rs` | providers, sign-in options, bindings, session age, endpoint, routes, limits, events, wiring |
| `crates\push-auth\src\tests\{mod,entra_accounts,entra_api,entra_server}.rs` | integration tests with Postgres, Redis and the stub |
| `crates\push-auth\tests\{schema,accounts,devices}.rs` | the new table, accounts without a password, optional email |
| `crates\push-worker\src\{config,jobs,server}.rs` | `PUSH_EMAIL_SENDER=none` |
| `crates\push-sdk\src\{auth,client,credentials,testing}.rs` | `sign_in_with_entra`, `session_expired`, the fake endpoint |
| `bindings\node\{src\client.rs,index.js,index.d.ts,test\client.test.js,test\types.ts}` | `signInWithEntra` |
| `bindings\c\{src\client.rs,src\lib.rs,include\push_sdk.h}` | `push_client_sign_in_with_entra` |
| `crates\push-smoke\Cargo.toml`, `tests\common\mod.rs`, `tests\local.rs`, `tests\entra_sign_in.rs` | shared password datastores, later variables win, the Entra-only whole stack |
| `docs\entra-sign-in.md` | new guide |
| `README.md`, `docs\sdk-bindings.md`, roadmap, spec | configuration and status |

---

### Task 1: Providers and Entra settings

**Files:**
- Modify: `crates\push-auth\Cargo.toml`, `crates\push-auth\src\lib.rs`, `crates\push-auth\src\config.rs`
- Create: `crates\push-auth\src\entra\mod.rs`, `crates\push-auth\src\entra\settings.rs`

- [ ] **Step 1: Manifest.** In `crates\push-auth\Cargo.toml` add, after `[package]`'s fields
and before `[dependencies]`:

```toml
[features]
# The stub OpenID issuer and RSA test keys of Microsoft Entra ID sign-in, for tests in
# other crates (push-smoke). Never enabled by a service binary.
testing = []
```

and in `[dependencies]`, after `redis.workspace = true`:

```toml
reqwest = { workspace = true, features = ["rustls-tls-native-roots"] }
```

- [ ] **Step 2: Module skeleton.** Create `crates\push-auth\src\entra\mod.rs`:

```rust
//! Microsoft Entra ID sign-in (spec 2026-10-19): auth-svc checks an Entra ID access token
//! issued for its API and finds the user by the token's immutable `(tid, oid)`. It never
//! redeems authorization codes and holds no client secret. Tokens, object ids, names and
//! email addresses are never logged.

mod settings;

pub use settings::{DEFAULT_AUTHORITY_HOST, DEFAULT_REQUIRED_SCOPE, EntraSettings, Providers};
```

and in `crates\push-auth\src\lib.rs` add `pub mod entra;` after `mod directory;`.

- [ ] **Step 3: Write the failing tests.** Create `crates\push-auth\src\entra\settings.rs`
with only the tests module (the code comes in Step 5):

```rust
#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const TENANT: &str = "33333333-0000-4000-8000-000000000003";
    const API: &str = "11111111-0000-4000-8000-000000000001";

    fn settings(variables: &[(&str, &str)]) -> Result<EntraSettings, ConfigError> {
        let mut all: HashMap<String, String> = [
            ("PUSH_ENTRA_ALLOWED_TENANTS", TENANT),
            ("PUSH_ENTRA_CLIENT_ID", API),
        ]
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
        for (name, value) in variables {
            all.insert(name.to_string(), value.to_string());
        }
        EntraSettings::from_lookup(&|name: &str| all.get(name).cloned())
    }

    fn refused(name: &str, value: &str) -> bool {
        matches!(
            settings(&[(name, value)]),
            Err(ConfigError::Invalid { name: refused, .. }) if refused == name
        )
    }

    #[test]
    fn providers_default_to_password_and_take_each_name_once() {
        assert_eq!(
            Providers::parse(None),
            Ok(Providers {
                password: true,
                entra: false
            })
        );
        assert_eq!(Providers::parse(Some(" ")), Ok(Providers::default()));
        assert_eq!(
            Providers::parse(Some("entra")),
            Ok(Providers {
                password: false,
                entra: true
            })
        );
        assert_eq!(
            Providers::parse(Some("entra, password")),
            Ok(Providers {
                password: true,
                entra: true
            })
        );
        for bad in ["Entra", "oidc", "password,password", "password,", ","] {
            assert!(Providers::parse(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_defaults_name_the_global_cloud_a_day_and_access_as_user() {
        let settings = settings(&[]).unwrap();
        assert_eq!(settings.authority_host, DEFAULT_AUTHORITY_HOST);
        assert_eq!(settings.tenants, [Uuid::parse_str(TENANT).unwrap()]);
        assert_eq!(settings.audiences, [API]);
        assert_eq!(settings.required_scope.as_deref(), Some("access_as_user"));
        assert_eq!(settings.required_role, None);
        assert!(settings.client_apps.is_empty());
        assert_eq!(settings.max_session, Duration::from_secs(24 * 3600));
        let tenant = settings.tenants[0];
        assert_eq!(
            settings.issuer(tenant),
            format!("https://login.microsoftonline.com/{TENANT}/v2.0")
        );
        assert_eq!(
            settings.metadata_url(tenant),
            format!(
                "https://login.microsoftonline.com/{TENANT}/v2.0/.well-known/openid-configuration"
            )
        );
    }

    #[test]
    fn tenants_are_listed_ids_never_wildcards_or_personal_accounts() {
        let two = settings(&[(
            "PUSH_ENTRA_ALLOWED_TENANTS",
            "33333333-0000-4000-8000-000000000003, 44444444-0000-4000-8000-000000000004",
        )])
        .unwrap();
        assert_eq!(two.tenants.len(), 2);
        let with_common = format!("{TENANT},common");
        for bad in [
            "common",
            "Organizations",
            "consumers",
            "contoso.onmicrosoft.com",
            "9188040d-6c67-4c5b-b112-36a304b66dad",
            "00000000-0000-0000-0000-000000000000",
            with_common.as_str(),
        ] {
            assert!(refused("PUSH_ENTRA_ALLOWED_TENANTS", bad), "{bad}");
        }
        assert_eq!(
            settings(&[("PUSH_ENTRA_ALLOWED_TENANTS", "")]),
            Err(ConfigError::Missing("PUSH_ENTRA_ALLOWED_TENANTS"))
        );
    }

    #[test]
    fn audiences_are_the_application_id_and_optionally_its_uri() {
        let both = settings(&[(
            "PUSH_ENTRA_CLIENT_ID",
            "api://push-api, 11111111-0000-4000-8000-000000000001",
        )])
        .unwrap();
        assert_eq!(both.audiences, ["api://push-api", API]);
        for bad in [
            "api://push-api",
            "https://push.example.com",
            "push",
            "api://",
            "api://a b",
        ] {
            assert!(refused("PUSH_ENTRA_CLIENT_ID", bad), "{bad}");
        }
        assert_eq!(
            settings(&[("PUSH_ENTRA_CLIENT_ID", " ")]),
            Err(ConfigError::Missing("PUSH_ENTRA_CLIENT_ID"))
        );
    }

    #[test]
    fn the_authority_is_https_or_loopback_http_and_nothing_else() {
        assert_eq!(
            settings(&[(
                "PUSH_ENTRA_AUTHORITY_HOST",
                "https://login.microsoftonline.us"
            )])
            .unwrap()
            .authority_host,
            "https://login.microsoftonline.us/"
        );
        assert_eq!(
            settings(&[("PUSH_ENTRA_AUTHORITY_HOST", "http://127.0.0.1:8123/")])
                .unwrap()
                .authority_host,
            "http://127.0.0.1:8123/"
        );
        for bad in [
            "http://login.microsoftonline.com/",
            "https://login.microsoftonline.com/common",
            "https://user@login.microsoftonline.com/",
            "https://login.microsoftonline.com/?x=1",
            "login.microsoftonline.com",
        ] {
            assert!(refused("PUSH_ENTRA_AUTHORITY_HOST", bad), "{bad}");
        }
    }

    #[test]
    fn scope_role_client_apps_and_session_age_can_be_set() {
        let set = settings(&[
            ("PUSH_ENTRA_REQUIRED_SCOPE", "Chat.Use"),
            ("PUSH_ENTRA_REQUIRED_ROLE", "Messenger.User"),
            (
                "PUSH_ENTRA_ALLOWED_CLIENT_APPS",
                "22222222-0000-4000-8000-000000000002",
            ),
            ("PUSH_ENTRA_MAX_SESSION_HOURS", "8"),
        ])
        .unwrap();
        assert_eq!(set.required_scope.as_deref(), Some("Chat.Use"));
        assert_eq!(set.required_role.as_deref(), Some("Messenger.User"));
        assert_eq!(set.client_apps.len(), 1);
        assert_eq!(set.max_session, Duration::from_secs(8 * 3600));
        assert_eq!(
            settings(&[("PUSH_ENTRA_REQUIRED_SCOPE", "*")])
                .unwrap()
                .required_scope,
            None
        );
        for (name, value) in [
            ("PUSH_ENTRA_REQUIRED_SCOPE", "two scopes"),
            ("PUSH_ENTRA_REQUIRED_ROLE", "a\u{7f}"),
            ("PUSH_ENTRA_ALLOWED_CLIENT_APPS", "spa"),
            ("PUSH_ENTRA_MAX_SESSION_HOURS", "0"),
            ("PUSH_ENTRA_MAX_SESSION_HOURS", "721"),
            ("PUSH_ENTRA_MAX_SESSION_HOURS", "1.5"),
        ] {
            assert!(refused(name, value), "{name}={value}");
        }
    }
}
```

- [ ] **Step 4: Run them to see them fail.**

Run: `cargo test -p push-auth --lib entra::settings`
Expected: compile errors (`Providers`, `EntraSettings` not found).

- [ ] **Step 5: Implement.** Put this above the tests module in `settings.rs`:

```rust
//! Which ways to sign in auth-svc offers (`PUSH_AUTH_PROVIDERS`) and how it checks
//! Microsoft Entra ID access tokens (`PUSH_ENTRA_*`, spec 2026-10-19 §3).

use std::time::Duration;

use reqwest::Url;
use uuid::Uuid;

use crate::config::ConfigError;

/// Microsoft Entra ID in the global Azure cloud.
pub const DEFAULT_AUTHORITY_HOST: &str = "https://login.microsoftonline.com/";
/// The delegated scope required by default: the one the guide tells the API registration
/// to expose.
pub const DEFAULT_REQUIRED_SCOPE: &str = "access_as_user";
/// The tenant of personal Microsoft accounts: never a workforce tenant.
const CONSUMERS_TENANT: Uuid = Uuid::from_u128(0x9188040d_6c67_4c5b_b112_36a304b66dad);
const MAX_TENANTS: usize = 20;
const MAX_AUDIENCES: usize = 4;
const MAX_SESSION_HOURS: u64 = 720;

/// The ways to sign in: email and password, Microsoft Entra ID, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Providers {
    pub password: bool,
    pub entra: bool,
}

impl Default for Providers {
    /// Email and password only: today's behaviour.
    fn default() -> Self {
        Self {
            password: true,
            entra: false,
        }
    }
}

impl Providers {
    /// A `PUSH_AUTH_PROVIDERS` value: `password`, `entra` or both, comma-separated, each
    /// once; unset or empty is `password`.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(Self::default());
        };
        let mut providers = Self {
            password: false,
            entra: false,
        };
        for name in value.split(',').map(str::trim) {
            match name {
                "password" if !providers.password => providers.password = true,
                "entra" if !providers.entra => providers.entra = true,
                _ => return Err("expected password, entra or password,entra".to_owned()),
            }
        }
        Ok(providers)
    }
}

/// How Entra ID access tokens are checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntraSettings {
    /// Scheme and host, ending in `/`: `https://login.microsoftonline.com/`.
    pub authority_host: String,
    /// The tenants whose users may sign in.
    pub tenants: Vec<Uuid>,
    /// The accepted `aud` values: the API's application id (what v2.0 tokens carry) and
    /// optionally its `api://` identifier URI.
    pub audiences: Vec<String>,
    /// The delegated scope `scp` must contain; `None` accepts any scope of this API.
    pub required_scope: Option<String>,
    /// The app role `roles` must contain, if any.
    pub required_role: Option<String>,
    /// The client apps (`azp`) allowed to present tokens; empty allows any.
    pub client_apps: Vec<Uuid>,
    /// How long an Entra-backed device stays signed in without a new Entra sign-in.
    pub max_session: Duration,
}

impl EntraSettings {
    /// Reads the `PUSH_ENTRA_*` variables. Their values are configuration, not secrets, so
    /// errors show them.
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let invalid = |name: &'static str, value: &str, reason: &str| ConfigError::Invalid {
            name,
            value: value.to_owned(),
            reason: reason.to_owned(),
        };

        let authority = get("PUSH_ENTRA_AUTHORITY_HOST")
            .unwrap_or_else(|| DEFAULT_AUTHORITY_HOST.to_owned());
        let authority_host = authority_host(&authority)
            .map_err(|reason| invalid("PUSH_ENTRA_AUTHORITY_HOST", &authority, reason))?;

        let tenants_value = get("PUSH_ENTRA_ALLOWED_TENANTS")
            .ok_or(ConfigError::Missing("PUSH_ENTRA_ALLOWED_TENANTS"))?;
        let tenants = tenants(&tenants_value)
            .map_err(|reason| invalid("PUSH_ENTRA_ALLOWED_TENANTS", &tenants_value, reason))?;

        let audiences_value =
            get("PUSH_ENTRA_CLIENT_ID").ok_or(ConfigError::Missing("PUSH_ENTRA_CLIENT_ID"))?;
        let audiences = audiences(&audiences_value)
            .map_err(|reason| invalid("PUSH_ENTRA_CLIENT_ID", &audiences_value, reason))?;

        let scope_value = get("PUSH_ENTRA_REQUIRED_SCOPE")
            .unwrap_or_else(|| DEFAULT_REQUIRED_SCOPE.to_owned());
        let required_scope = match scope_value.as_str() {
            "*" => None,
            scope if is_claim_value(scope) => Some(scope.to_owned()),
            _ => {
                return Err(invalid(
                    "PUSH_ENTRA_REQUIRED_SCOPE",
                    &scope_value,
                    "use one scope name (1 to 128 visible ASCII characters) or *",
                ));
            }
        };
        let required_role = match get("PUSH_ENTRA_REQUIRED_ROLE") {
            None => None,
            Some(role) if is_claim_value(&role) => Some(role),
            Some(role) => {
                return Err(invalid(
                    "PUSH_ENTRA_REQUIRED_ROLE",
                    &role,
                    "use one app role value (1 to 128 visible ASCII characters)",
                ));
            }
        };
        let client_apps = match get("PUSH_ENTRA_ALLOWED_CLIENT_APPS") {
            None => Vec::new(),
            Some(value) => guids(&value, MAX_TENANTS)
                .map_err(|reason| invalid("PUSH_ENTRA_ALLOWED_CLIENT_APPS", &value, reason))?,
        };
        let hours_value =
            get("PUSH_ENTRA_MAX_SESSION_HOURS").unwrap_or_else(|| "24".to_owned());
        let hours: u64 = hours_value
            .parse()
            .ok()
            .filter(|hours| (1..=MAX_SESSION_HOURS).contains(hours))
            .ok_or_else(|| {
                invalid(
                    "PUSH_ENTRA_MAX_SESSION_HOURS",
                    &hours_value,
                    "use a whole number of hours from 1 to 720",
                )
            })?;
        Ok(Self {
            authority_host,
            tenants,
            audiences,
            required_scope,
            required_role,
            client_apps,
            max_session: Duration::from_secs(hours * 3600),
        })
    }

    /// The `iss` of `tenant`'s v2.0 tokens.
    pub fn issuer(&self, tenant: Uuid) -> String {
        format!("{}{tenant}/v2.0", self.authority_host)
    }

    /// `tenant`'s OpenID Connect metadata document.
    pub fn metadata_url(&self, tenant: Uuid) -> String {
        format!(
            "{}{tenant}/v2.0/.well-known/openid-configuration",
            self.authority_host
        )
    }
}

/// `https://host/` (a sovereign cloud's too), or `http://` to a loopback host for tests.
fn authority_host(value: &str) -> Result<String, &'static str> {
    let url = Url::parse(value).map_err(|_| "not a URL")?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err("must be an https:// URL (http:// only for a loopback host)"),
    }
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("must be a scheme and host only, like https://login.microsoftonline.com/");
    }
    Ok(url.to_string())
}

/// Tenant ids; never the multi-tenant names, personal accounts or the nil id (spec D3).
fn tenants(value: &str) -> Result<Vec<Uuid>, &'static str> {
    for name in value.split(',').map(str::trim) {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "common" | "organizations" | "consumers"
        ) {
            return Err("multi-tenant authorities are not supported: list tenant ids");
        }
    }
    let tenants = guids(value, MAX_TENANTS)?;
    if tenants
        .iter()
        .any(|tenant| tenant.is_nil() || *tenant == CONSUMERS_TENANT)
    {
        return Err("personal Microsoft accounts and the nil tenant are not supported");
    }
    Ok(tenants)
}

/// 1 to `most` GUIDs, comma-separated; duplicates count once.
fn guids(value: &str, most: usize) -> Result<Vec<Uuid>, &'static str> {
    let mut ids = Vec::new();
    for item in value.split(',').map(str::trim) {
        let id = Uuid::try_parse(item).map_err(|_| "expected ids (GUIDs) separated by commas")?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.len() > most {
        return Err("too many ids");
    }
    Ok(ids)
}

/// The API's application id, optionally with its `api://` identifier URI.
fn audiences(value: &str) -> Result<Vec<String>, &'static str> {
    let mut audiences = Vec::new();
    for item in value.split(',').map(str::trim) {
        let audience = if let Ok(id) = Uuid::try_parse(item) {
            id.to_string()
        } else if item.len() > "api://".len()
            && item.len() <= 256
            && item.starts_with("api://")
            && item.bytes().all(|byte| byte.is_ascii_graphic())
        {
            item.to_owned()
        } else {
            return Err(
                "expected the API's application id, optionally with its api:// identifier URI",
            );
        };
        if !audiences.contains(&audience) {
            audiences.push(audience);
        }
    }
    if audiences.len() > MAX_AUDIENCES {
        return Err("at most four audiences");
    }
    if audiences.iter().all(|audience| audience.starts_with("api://")) {
        return Err("include the API's application id: v2.0 access tokens carry it as `aud`");
    }
    Ok(audiences)
}

/// A scope or role value: 1 to 128 visible ASCII characters.
fn is_claim_value(value: &str) -> bool {
    (1..=128).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_graphic())
}
```

- [ ] **Step 6: Run.**

Run: `cargo test -p push-auth --lib entra::settings`
Expected: 6 tests pass.

- [ ] **Step 7: The service configuration.** In `crates\push-auth\src\config.rs`:

Add `use crate::entra::{EntraSettings, Providers};` after `use crate::db::is_role_name;`.

Add rows to the doc table, after `PUSH_DIRECTORY_TOKEN`'s row:

```rust
/// | `PUSH_AUTH_PROVIDERS` | `password`, `entra` or `password,entra` (spec 2026-10-19) | `password` |
/// | `PUSH_ENTRA_CLIENT_ID` | accepted audiences: the API registration's application id, optionally with its `api://` URI | required with `entra` |
/// | `PUSH_ENTRA_ALLOWED_TENANTS` | tenant ids (GUIDs), comma-separated; `common`, `organizations`, `consumers` refused | required with `entra` |
/// | `PUSH_ENTRA_AUTHORITY_HOST` | the Entra ID host; `http://` only for loopback (tests) | `https://login.microsoftonline.com/` |
/// | `PUSH_ENTRA_REQUIRED_SCOPE` | the delegated scope `scp` must contain; `*` for any | `access_as_user` |
/// | `PUSH_ENTRA_REQUIRED_ROLE` | an app role `roles` must contain | none |
/// | `PUSH_ENTRA_ALLOWED_CLIENT_APPS` | client app ids allowed in `azp` | any |
/// | `PUSH_ENTRA_MAX_SESSION_HOURS` | maximum session age of an Entra-backed device, 1 to 720 | `24` |
```

Add two fields at the end of `pub struct Config`:

```rust
    /// The ways to sign in.
    pub providers: Providers,
    /// How Entra ID tokens are checked, when `entra` is a provider.
    pub entra: Option<EntraSettings>,
```

In the `Debug` impl add, before `.finish()`:

```rust
            .field("providers", &self.providers)
            .field("entra", &self.entra)
```

In `Config::from_lookup`, after the `directory_token` checks and before `Ok(Self {`:

```rust
        let providers_value = lookup("PUSH_AUTH_PROVIDERS");
        let providers = Providers::parse(providers_value.as_deref()).map_err(|reason| {
            ConfigError::Invalid {
                name: "PUSH_AUTH_PROVIDERS",
                value: providers_value.clone().unwrap_or_default(),
                reason,
            }
        })?;
        // The PUSH_ENTRA_* variables are read only with the provider.
        let entra = if providers.entra {
            Some(EntraSettings::from_lookup(lookup_dyn)?)
        } else {
            None
        };
```

and add `providers,` and `entra,` to the `Ok(Self { … })` literal (after `directory_token,`).

Add to the config tests module:

```rust
    #[test]
    fn sign_in_is_email_and_password_unless_entra_is_chosen() {
        let default = config(&REQUIRED).unwrap();
        assert_eq!(
            default.providers,
            Providers {
                password: true,
                entra: false
            }
        );
        assert_eq!(default.entra, None);
        // The Entra variables are not read without the provider.
        assert!(config(&with("PUSH_ENTRA_ALLOWED_TENANTS", "common")).is_ok());
        let mut entra_only = REQUIRED.to_vec();
        entra_only.extend([
            ("PUSH_AUTH_PROVIDERS", "entra"),
            (
                "PUSH_ENTRA_ALLOWED_TENANTS",
                "33333333-0000-4000-8000-000000000003",
            ),
            ("PUSH_ENTRA_CLIENT_ID", "11111111-0000-4000-8000-000000000001"),
        ]);
        let entra = config(&entra_only).unwrap();
        assert_eq!(
            entra.providers,
            Providers {
                password: false,
                entra: true
            }
        );
        assert_eq!(
            entra.entra.as_ref().map(|settings| settings.tenants.len()),
            Some(1)
        );
        entra_only.retain(|(name, _)| *name != "PUSH_ENTRA_CLIENT_ID");
        assert_eq!(
            config(&entra_only),
            Err(ConfigError::Missing("PUSH_ENTRA_CLIENT_ID"))
        );
        assert!(matches!(
            config(&with("PUSH_AUTH_PROVIDERS", "ldap")),
            Err(ConfigError::Invalid {
                name: "PUSH_AUTH_PROVIDERS",
                ..
            })
        ));
    }
```

- [ ] **Step 8: Run.**

Run: `cargo test -p push-auth --lib` (without `--locked` this once: `Cargo.lock` gains
`reqwest` in push-auth's dependency list and nothing else, since reqwest 0.12.28 with
`rustls-tls-native-roots` is already locked for push-credentials; check with
`git diff Cargo.lock`)
Expected: every lib test passes, the new config test included.

- [ ] **Step 9: Commit.**

```powershell
git add -A
git commit -m "feat(auth): read the sign-in providers and the Entra ID settings"
```

### Task 2: The claim rules

**Files:**
- Modify: `crates\push-auth\src\entra\mod.rs`
- Create: `crates\push-auth\src\entra\claims.rs`

- [ ] **Step 1: Types.** Replace `crates\push-auth\src\entra\mod.rs` with:

```rust
//! Microsoft Entra ID sign-in (spec 2026-10-19): auth-svc checks an Entra ID access token
//! issued for its API and finds the user by the token's immutable `(tid, oid)`. It never
//! redeems authorization codes and holds no client secret. Tokens, object ids, names and
//! email addresses are never logged.

mod claims;
mod settings;

use uuid::Uuid;

pub use settings::{DEFAULT_AUTHORITY_HOST, DEFAULT_REQUIRED_SCOPE, EntraSettings, Providers};

/// Why a token was refused. Logged by name; the client only ever hears
/// `entra_token_invalid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Malformed,
    Algorithm,
    Tenant,
    Issuer,
    Signature,
    Audience,
    Expired,
    NotYetValid,
    Version,
    AppOnly,
    Scope,
    Role,
    ClientApp,
    Subject,
}

impl Reason {
    /// The name in the security log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::Algorithm => "algorithm",
            Self::Tenant => "tenant",
            Self::Issuer => "issuer",
            Self::Signature => "signature",
            Self::Audience => "audience",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::Version => "version",
            Self::AppOnly => "app_only",
            Self::Scope => "scope",
            Self::Role => "role",
            Self::ClientApp => "client_app",
            Self::Subject => "subject",
        }
    }
}

/// Why no identity came out of a token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntraError {
    #[error("the Entra ID token is refused ({})", .0.as_str())]
    Rejected(Reason),
    /// No usable signing keys: the authority could not be reached or answered nonsense.
    #[error("Microsoft Entra ID's signing keys are not available: {0}")]
    Unavailable(String),
}

/// The user an accepted token names. `Debug` hides everything but the tenant.
#[derive(Clone, PartialEq, Eq)]
pub struct EntraIdentity {
    pub tenant_id: Uuid,
    /// The user's object id: immutable, the same for every app of the tenant.
    pub object_id: Uuid,
    /// The `name` claim: a display-name suggestion for a new account only.
    pub name: Option<String>,
    /// `email`, else `preferred_username`: informational, never used to find an account.
    pub email: Option<String>,
}

impl std::fmt::Debug for EntraIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EntraIdentity")
            .field("tenant_id", &self.tenant_id)
            .field("object_id", &"<redacted>")
            .field("name", &self.name.as_ref().map(|_| "<redacted>"))
            .field("email", &self.email.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}
```

- [ ] **Step 2: Write the failing tests.** Create `crates\push-auth\src\entra\claims.rs`
with the tests module only:

```rust
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const TENANT: Uuid = Uuid::from_u128(0x33333333_0000_4000_8000_000000000003);
    const USER: &str = "55555555-0000-4000-8000-000000000005";
    const SPA: &str = "22222222-0000-4000-8000-000000000002";

    fn settings() -> EntraSettings {
        EntraSettings {
            authority_host: "https://login.microsoftonline.com/".to_owned(),
            tenants: vec![TENANT],
            audiences: vec!["11111111-0000-4000-8000-000000000001".to_owned()],
            required_scope: Some("access_as_user".to_owned()),
            required_role: None,
            client_apps: Vec::new(),
            max_session: Duration::from_secs(3600),
        }
    }

    fn delegated() -> EntraClaims {
        EntraClaims {
            ver: Some("2.0".to_owned()),
            tid: Some(TENANT.to_string()),
            oid: Some(USER.to_owned()),
            scp: Some("access_as_user".to_owned()),
            azp: Some(SPA.to_owned()),
            name: Some(" Alice Smith ".to_owned()),
            preferred_username: Some("alice@contoso.test".to_owned()),
            ..EntraClaims::default()
        }
    }

    #[test]
    fn a_delegated_v2_token_names_its_user() {
        let identity = check(&delegated(), TENANT, &settings()).unwrap();
        assert_eq!(identity.tenant_id, TENANT);
        assert_eq!(identity.object_id.to_string(), USER);
        assert_eq!(identity.name.as_deref(), Some("Alice Smith"));
        assert_eq!(identity.email.as_deref(), Some("alice@contoso.test"));
        let with_email = EntraClaims {
            email: Some("a.smith@contoso.test".to_owned()),
            ..delegated()
        };
        assert_eq!(
            check(&with_email, TENANT, &settings())
                .unwrap()
                .email
                .as_deref(),
            Some("a.smith@contoso.test")
        );
        let bare = EntraClaims {
            name: None,
            preferred_username: Some("\u{7}".to_owned()),
            ..delegated()
        };
        let identity = check(&bare, TENANT, &settings()).unwrap();
        assert_eq!((identity.name, identity.email), (None, None));
    }

    #[test]
    fn other_versions_tenants_and_subjects_are_refused() {
        let cases = [
            (
                EntraClaims {
                    ver: Some("1.0".to_owned()),
                    ..delegated()
                },
                Reason::Version,
            ),
            (
                EntraClaims {
                    ver: None,
                    ..delegated()
                },
                Reason::Version,
            ),
            (
                EntraClaims {
                    tid: Some("44444444-0000-4000-8000-000000000004".to_owned()),
                    ..delegated()
                },
                Reason::Tenant,
            ),
            (
                EntraClaims {
                    tid: None,
                    ..delegated()
                },
                Reason::Tenant,
            ),
            (
                EntraClaims {
                    oid: None,
                    ..delegated()
                },
                Reason::Subject,
            ),
            (
                EntraClaims {
                    oid: Some("not-a-guid".to_owned()),
                    ..delegated()
                },
                Reason::Subject,
            ),
        ];
        for (claims, reason) in cases {
            assert_eq!(check(&claims, TENANT, &settings()).err(), Some(reason));
        }
    }

    #[test]
    fn app_only_and_id_tokens_lack_a_delegated_scope() {
        // App-only (client credentials): roles, no scp; and with the optional idtyp claim.
        let app = EntraClaims {
            scp: None,
            roles: vec!["Messages.Send".to_owned()],
            ..delegated()
        };
        assert_eq!(check(&app, TENANT, &settings()).err(), Some(Reason::AppOnly));
        let typed = EntraClaims {
            idtyp: Some("app".to_owned()),
            ..delegated()
        };
        assert_eq!(
            check(&typed, TENANT, &settings()).err(),
            Some(Reason::AppOnly)
        );
        // An ID token has no scp either (its audience, the client app, fails first).
        let id_token = EntraClaims {
            scp: None,
            ..delegated()
        };
        assert_eq!(
            check(&id_token, TENANT, &settings()).err(),
            Some(Reason::AppOnly)
        );
        let blank = EntraClaims {
            scp: Some("  ".to_owned()),
            ..delegated()
        };
        assert_eq!(
            check(&blank, TENANT, &settings()).err(),
            Some(Reason::AppOnly)
        );
        let user = EntraClaims {
            idtyp: Some("user".to_owned()),
            ..delegated()
        };
        assert!(check(&user, TENANT, &settings()).is_ok());
    }

    #[test]
    fn the_required_scope_role_and_client_app_hold() {
        let other = EntraClaims {
            scp: Some("User.Read".to_owned()),
            ..delegated()
        };
        assert_eq!(check(&other, TENANT, &settings()).err(), Some(Reason::Scope));
        let several = EntraClaims {
            scp: Some("User.Read access_as_user".to_owned()),
            ..delegated()
        };
        assert!(check(&several, TENANT, &settings()).is_ok());
        let any = EntraSettings {
            required_scope: None,
            ..settings()
        };
        assert!(check(&other, TENANT, &any).is_ok());

        let role = EntraSettings {
            required_role: Some("Messenger.User".to_owned()),
            ..settings()
        };
        assert_eq!(check(&delegated(), TENANT, &role).err(), Some(Reason::Role));
        let member = EntraClaims {
            roles: vec!["Messenger.User".to_owned()],
            ..delegated()
        };
        assert!(check(&member, TENANT, &role).is_ok());

        let apps = EntraSettings {
            client_apps: vec![Uuid::parse_str(SPA).unwrap()],
            ..settings()
        };
        assert!(check(&delegated(), TENANT, &apps).is_ok());
        for azp in [None, Some("99999999-0000-4000-8000-000000000009".to_owned())] {
            let elsewhere = EntraClaims {
                azp,
                ..delegated()
            };
            assert_eq!(
                check(&elsewhere, TENANT, &apps).err(),
                Some(Reason::ClientApp)
            );
        }
    }

    #[test]
    fn the_identity_never_shows_its_object_id_name_or_email() {
        let shown = format!("{:?}", check(&delegated(), TENANT, &settings()).unwrap());
        for secret in [USER, "Alice", "alice@contoso.test"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(shown.contains(&TENANT.to_string()), "{shown}");
    }
}
```

and add `mod claims;`'s use: nothing more in `mod.rs` (it already declares `mod claims;`).

- [ ] **Step 3: Run them to see them fail.**

Run: `cargo test -p push-auth --lib entra::claims`
Expected: compile errors (`EntraClaims`, `check` not found).

- [ ] **Step 4: Implement.** Put above the tests in `claims.rs`:

```rust
//! The claims of an Entra ID access token beyond those `jsonwebtoken` checks (signature,
//! `iss`, `aud`, `exp`, `nbf`): a v2.0 delegated token of the tenant its issuer named,
//! with the required scope and, when configured, the role and client app (spec D2).

use serde::Deserialize;
use uuid::Uuid;

use super::{EntraIdentity, EntraSettings, Reason};

/// The claims read from an access token. A claim of the wrong JSON type makes the whole
/// token malformed. No `Debug`: it holds the name and the address.
#[derive(Clone, Default, Deserialize)]
pub(crate) struct EntraClaims {
    #[serde(default)]
    pub ver: Option<String>,
    #[serde(default)]
    pub tid: Option<String>,
    #[serde(default)]
    pub oid: Option<String>,
    #[serde(default)]
    pub scp: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub idtyp: Option<String>,
    #[serde(default)]
    pub azp: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// The identity in `claims`, of a token whose signature and issuer named `tenant`.
pub(crate) fn check(
    claims: &EntraClaims,
    tenant: Uuid,
    settings: &EntraSettings,
) -> Result<EntraIdentity, Reason> {
    if claims.ver.as_deref() != Some("2.0") {
        return Err(Reason::Version);
    }
    if claims.tid.as_deref().and_then(guid) != Some(tenant) {
        return Err(Reason::Tenant);
    }
    let scopes: Vec<&str> = claims
        .scp
        .as_deref()
        .unwrap_or_default()
        .split(' ')
        .filter(|scope| !scope.is_empty())
        .collect();
    // Delegated tokens only: an app-only token has no scp (and `idtyp: app` when that
    // optional claim is configured); an ID token has none either.
    if claims.idtyp.as_deref() == Some("app") || scopes.is_empty() {
        return Err(Reason::AppOnly);
    }
    if let Some(required) = &settings.required_scope {
        if !scopes.contains(&required.as_str()) {
            return Err(Reason::Scope);
        }
    }
    if let Some(role) = &settings.required_role {
        if !claims.roles.iter().any(|held| held == role) {
            return Err(Reason::Role);
        }
    }
    if !settings.client_apps.is_empty()
        && !claims
            .azp
            .as_deref()
            .and_then(guid)
            .is_some_and(|app| settings.client_apps.contains(&app))
    {
        return Err(Reason::ClientApp);
    }
    let object_id = claims.oid.as_deref().and_then(guid).ok_or(Reason::Subject)?;
    Ok(EntraIdentity {
        tenant_id: tenant,
        object_id,
        name: informational(claims.name.as_deref()),
        email: informational(claims.email.as_deref())
            .or_else(|| informational(claims.preferred_username.as_deref())),
    })
}

fn guid(value: &str) -> Option<Uuid> {
    Uuid::try_parse(value).ok()
}

/// A claim kept for display only: trimmed, 1 to 254 characters, no control characters.
fn informational(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    let fits = !value.is_empty()
        && value.chars().count() <= 254
        && !value.chars().any(char::is_control);
    fits.then(|| value.to_owned())
}
```

- [ ] **Step 5: Run.**

Run: `cargo test -p push-auth --lib entra::claims`
Expected: 5 tests pass. (`cargo clippy -p push-auth --all-targets -- -D warnings` may warn that
`check` is unused outside tests until Task 5; add nothing for that: Task 5 uses it.)

- [ ] **Step 6: Commit.**

```powershell
git add -A
git commit -m "feat(auth): check the claims of an Entra ID access token"
```

### Task 3: The stub OpenID issuer and the test keys (feature `testing`)

Every Entra test gets its metadata, keys and tokens from this stub. It answers for any
tenant (the service decides which it trusts), can rotate keys, fail, be slow, name a
foreign key set or another issuer, and counts its fetches.

**Files:**
- Create: `crates\push-auth\src\entra\testing.rs`
- Modify: `crates\push-auth\src\entra\mod.rs`

- [ ] **Step 1: Declare the module.** In `entra\mod.rs`, after `mod settings;`:

```rust
#[cfg(any(test, feature = "testing"))]
pub mod testing;
```

- [ ] **Step 2: Write `testing.rs`.** The two keys were generated once with
`[System.Security.Cryptography.RSA]::Create(2048)` (`ExportRSAPrivateKey()` for the PKCS#1
DER, `ExportParameters($false)` for `n` and `e`, base64url without padding). They are
public test material: nothing may ever trust them.

```rust
//! A stub of Microsoft Entra ID for tests (feature `testing`): the OpenID metadata and key
//! set of any tenant over HTTP on 127.0.0.1, two fixed RSA-2048 test keys and a token
//! minter. TEST ONLY: the private keys below are public, so nothing may ever trust them.

use std::future::IntoFuture;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use uuid::Uuid;

use super::EntraSettings;

/// The tenant the stub's tokens come from by default.
pub const TENANT: &str = "33333333-0000-4000-8000-000000000003";
/// A second tenant the stub's settings allow.
pub const OTHER_TENANT: &str = "44444444-0000-4000-8000-000000000004";
/// The API's application id: the audience of the stub's tokens.
pub const API_CLIENT_ID: &str = "11111111-0000-4000-8000-000000000001";
/// The client app (a SPA) that requests them: their `azp`.
pub const CLIENT_APP: &str = "22222222-0000-4000-8000-000000000002";

/// An RSA-2048 test key: the private key as base64 PKCS#1 DER, and the public modulus
/// and exponent as a JWK publishes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestKey {
    pub kid: &'static str,
    der: &'static str,
    n: &'static str,
    e: &'static str,
}

pub const KEY_A: TestKey = TestKey {
    kid: "test-key-a",
    der: "MIIEowIBAAKCAQEAwdzXVqnWLXr9hp64LltVmiOkJymuLAfiwVV7HK4QbmkByq/m7BP8xYqw4ycFxcr7RfjUl8gtCt4XQdOgD1a2/Os8T/1gDvA5W/S+Oec4Q2+x4uIWVzAjzvKO6/+d+LbGTMghaPNZG7rD88t1kCwxl7hHgO/7o5QpDQD9YPk+Yk2P+EPq/GWMBpzwsxEJTSJ43D1AsC0gsd2F0IjiJCZbEvJLRh9aAc457dOsMRgDIDeleSQy3vz7i3yTFNvm+W8CSs8aAetjkylnG4XcNWViXiq0l44VLnlQ5vwW4ecci7FwN07qWgBFM2A1lq0Sd3Xi8O5kNVaArlDLJ6YnkP0GTQIDAQABAoIBACC11JfI3FycLf7y9PzNaGozrVT2UW8YfYhFlhpGvHsEuMqsV3hp/vKN7j7FrNGs6CzsSWK64e69fosa3Bz6crHGN0z/4Z9jpSoA57/5ZaLrS8aOg1p+wuv+PWNQpCT9IgR+mDSPKu/k2X9fSxzieBOzgk0dMQZuz2ZkTlx4cbBAPk/pWT58XR0T2u/k+LJCf1nSFINBtlw+EnTe1MULNzceBQathT7Q4oTRS67KgfM/t9em8zrL9MKPYmIVR4LB6apLKNF5GyT3ckzE7mVT7lySJOn3L95nEK6F+W8R0d/IkOe+adP7Tr9oAOOYOSLZuJq7gaTFe99Qy6zUaiz6CLkCgYEAxJCaqlfjymYhf6afBmYw1JKWAoBOnIX3GlLoSfrpVjB6x6Zv/PW6HMrD0VLm5m96jETImqbUtYZtMH4vmqs+EHQujZ4c8G+N54yk7zZ0NsDF14f5kNUkUnbT5AscAXV00HYF4DGF2R6xBq4orRDCQt9Bd5JedP19eBM+4SCtcqsCgYEA/HsRulOI8TzRvzYGq9O/XhZpMQGnwC3VsF2CvQUTzg5g0ryQe3WYpMKquKY1ijLaYcX1BErhxFFcgFOLP5JcLioLBASQ/l2QIAiTh39T/A0/DBm3wZGc3+9gu8YwQJmhOIrN7zOcTEFZAnfi+Qf+4KYdbYh1XlHwSUXCMAr/qucCgYEAigcQ1HULG+FDvKOAH7o6p8qXVAJNMVRRBEcbnnd2ynMYhIG3OeZDXlxzzNVuFnpJj53tT8u+qglPOzIgs3d0CZHL7t4MbDmpZ+M3HlMaoCgVxRdShAVnkBCpEgVLESstcASwc5HS73fBz3MUVRLjlpm3GwPJGFW4eUzqZqPPqCkCgYBNUseYH9M1xmGaRMqAPyZBg22N06rzYVYrYEgU6YQqPB8NCzDYjqhD9VZhnI2TGIXnIcArI1/HRv+kUXG483MFtFyRS3C4ZiEonlfITIeFDu17IDk1sAmyT1Z5y3WloydL8L6m/lBkzjKbXeoGSJuXY2hpzjvbug5z8khJfGTWiwKBgCEhdl546X2oRQtgvRZVeJoy/Gdcdjk6lOje+VehJahx5HconR91TkxP82QGFfxqzvE+aGnc+rYluVOYPOxrTlmh+IZFMEby7GTECNp1KEYZ805ha95XW+dBYGNAn6RQo5rL6WYnTecDZuZ0/pts3pNPlW71T4CdUwKZAH3YBCrF",
    n: "wdzXVqnWLXr9hp64LltVmiOkJymuLAfiwVV7HK4QbmkByq_m7BP8xYqw4ycFxcr7RfjUl8gtCt4XQdOgD1a2_Os8T_1gDvA5W_S-Oec4Q2-x4uIWVzAjzvKO6_-d-LbGTMghaPNZG7rD88t1kCwxl7hHgO_7o5QpDQD9YPk-Yk2P-EPq_GWMBpzwsxEJTSJ43D1AsC0gsd2F0IjiJCZbEvJLRh9aAc457dOsMRgDIDeleSQy3vz7i3yTFNvm-W8CSs8aAetjkylnG4XcNWViXiq0l44VLnlQ5vwW4ecci7FwN07qWgBFM2A1lq0Sd3Xi8O5kNVaArlDLJ6YnkP0GTQ",
    e: "AQAB",
};

pub const KEY_B: TestKey = TestKey {
    kid: "test-key-b",
    der: "MIIEowIBAAKCAQEAv6lpHKf48AAhfMTU2uCwB+t3gXryGfX0SbcAIFypvmLcWCUg1VNFTkJkQP44DUnp3vik/8XvgTLNce//HTPDG3LJkU64ArTk/Z91dxA9n/rh0opMSXGZ67Gc9J98Zo3LZViiDjNbUGyRwXxD/ybpY4Sdl7mcr26d16aB9rHVULo42ttRCKb4Wwscnh2bXg363stt0nYH8SyBahXCo7z6hFDKgpoBj7cY5oiS9R+r4pHgRW2UhdIijAVXwS+89JUY6bjL/tq2Y7e9hE759GagH8+Yw1Zu9LscjjhYOwJI83TcjsvFAb8Z3lHK/nXkgmh23fdF1qk7e+AMrHus0Az3cQIDAQABAoIBAAfmJoPc1GCn0LmArzkaG4coCDPY6lNxlJDZLvMmfoRvNJ+ZYCIKnYedKt6W256Ilk/vNXWpJbbQbjEs7/m3IWpnL7GCagkKhYmCYef3TmLihx4TPxOjz/Erz8kxLYJYg7J4la06JqBI6TTtNywz4ViNpaUULEX6rBywKxkMuTrrb4LIlsPTTq388GqGDPvLQy3479oYlv68gh9ioY+blHAD3r0qIKSLrq/Cvr6TvvXGmmHuNq56bS9ksGzZ1gNCBR0UQsOuCty4Md3G3p9GnqehKxbMzPqWxVXPMLuc2ccw9rQ/naMFmA+BhGlakh77olwFICPGGKyrainXcLXBYOUCgYEA9eJDw3gvz2PAcHJ8tBcXzx+LsYq1vkJnUW/v4CbnKEkEsiCnnhyH3TYs0K626ioR928wij2YXY1llabnZ68/55RZhD7kCaWsEgC+KTGYozaLqkuQLf4e3qLZIdstpssB8OB/kbV9JWojSv9w17OUG7pnsQyOaHnmMAWidDOKH7MCgYEAx4wPTXQ22fyKa8VpaMLQvEOM5x4Z2JRfsRPDCNnNAJ4HTvjxjcCu+WVYxdibmj4uMcREwQvvfn7fd6Y/V4lhqx2zdaD9KceEpeyzj9K0Z2uCawLr5vr+cNsdYkaonlzi9c1a5vCVcrDhc6/hXivU5Gmm4cKYRSUJO2g8l8BsmksCgYANwwAv7IYggWC/6mh4W+KFQSq8YRsS7vyBXlZ6oTLG2m9c2N8IxmgERYf0lhK+wGQJCTwaQKBKfCN3YMQK1ZAqEe+Aq1RwBcm/qDL8cz4rxF3Exj3nlRV9OHgWoQHJGAufw7DgItxCP9KB3BIgwHhFpQUwr5c42bm8blRNVKUiaQKBgCB50PvAEDVF/pkvlcow+pvJOqNaqMYwz6oYF0PXfp+9GF9n8JgrZc69CceZSym9OPNsh+wLLz0dFO16JxDuEPEyckSQoOMtz47ZiRe9KRMz0hPsc8wxyTtAzfVynnq3j1CFzWeMfCYgV3DmjP0wkQNjln9jljiaoHgErOfdr08lAoGBAIC3LNKSQHkcxrFvkq99p+GI3rN/VBvny56j5DUEHcb/2rXTIZaZDQz7b6s+ZYf1EXLGYEBbFyDdVAiLrjLVy4c/i3qcb9xZlapdnorm5u6TjzSHwip04wjHpNhDmumeKXGXvb8VBoXGhQAO2G1C+jhWHTrIX/RH64KYTBFdUWsW",
    n: "v6lpHKf48AAhfMTU2uCwB-t3gXryGfX0SbcAIFypvmLcWCUg1VNFTkJkQP44DUnp3vik_8XvgTLNce__HTPDG3LJkU64ArTk_Z91dxA9n_rh0opMSXGZ67Gc9J98Zo3LZViiDjNbUGyRwXxD_ybpY4Sdl7mcr26d16aB9rHVULo42ttRCKb4Wwscnh2bXg363stt0nYH8SyBahXCo7z6hFDKgpoBj7cY5oiS9R-r4pHgRW2UhdIijAVXwS-89JUY6bjL_tq2Y7e9hE759GagH8-Yw1Zu9LscjjhYOwJI83TcjsvFAb8Z3lHK_nXkgmh23fdF1qk7e-AMrHus0Az3cQ",
    e: "AQAB",
};

impl TestKey {
    fn encoding(self) -> EncodingKey {
        EncodingKey::from_rsa_der(&STANDARD.decode(self.der).expect("the test key is base64"))
    }

    /// The public key as a JWKS entry.
    pub fn jwk(self) -> Value {
        json!({"kty": "RSA", "use": "sig", "kid": self.kid, "n": self.n, "e": self.e})
    }
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64
}

/// `claims` signed with `key` (RS256), naming `key`'s id.
pub fn sign(key: TestKey, claims: &Value) -> String {
    sign_as(key, key.kid, claims)
}

/// `claims` signed with `key` but naming the key `kid`: with another key's id, a token
/// whose signature does not match.
pub fn sign_as(key: TestKey, kid: &str, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_owned());
    jsonwebtoken::encode(&header, claims, &key.encoding()).expect("signing with a test key")
}

/// A token whose header names `alg` and `kid` and whose last part is `signature`: for
/// the algorithms the service must refuse (`none`, `HS256`, `RS384`, …).
pub fn with_algorithm(alg: &str, kid: &str, claims: &Value, signature: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(json!({"alg": alg, "kid": kid, "typ": "JWT"}).to_string());
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{header}.{payload}.{signature}")
}

struct Stub {
    authority: String,
    published: Mutex<Vec<TestKey>>,
    failing: AtomicBool,
    delay_ms: AtomicU64,
    foreign_jwks: AtomicBool,
    wrong_issuer: AtomicBool,
    metadata_fetches: AtomicUsize,
    jwks_fetches: AtomicUsize,
}

/// The stub authority. It publishes [`KEY_A`] until told otherwise.
pub struct StubOpenId {
    state: Arc<Stub>,
}

impl StubOpenId {
    /// The stub on 127.0.0.1 over plain HTTP.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the stub issuer");
        let authority = format!("http://{}/", listener.local_addr().expect("its address"));
        let (stub, router) = Self::with_authority(&authority);
        tokio::spawn(axum::serve(listener, router).into_future());
        stub
    }

    /// The stub and its routes, for a server of the caller's (an HTTPS one, for example)
    /// reachable at `authority` (scheme and host, ending in `/`).
    pub fn with_authority(authority: &str) -> (Self, Router) {
        let state = Arc::new(Stub {
            authority: authority.to_owned(),
            published: Mutex::new(vec![KEY_A]),
            failing: AtomicBool::new(false),
            delay_ms: AtomicU64::new(0),
            foreign_jwks: AtomicBool::new(false),
            wrong_issuer: AtomicBool::new(false),
            metadata_fetches: AtomicUsize::new(0),
            jwks_fetches: AtomicUsize::new(0),
        });
        let router = Router::new()
            .route(
                "/{tenant}/v2.0/.well-known/openid-configuration",
                get(metadata),
            )
            .route("/{tenant}/discovery/v2.0/keys", get(keys))
            .with_state(Arc::clone(&state));
        (Self { state }, router)
    }

    /// The value for `PUSH_ENTRA_AUTHORITY_HOST`, ending in `/`.
    pub fn authority(&self) -> &str {
        &self.state.authority
    }

    /// The `iss` of `tenant`'s tokens.
    pub fn issuer(&self, tenant: &str) -> String {
        format!("{}{tenant}/v2.0", self.state.authority)
    }

    /// Publishes exactly `keys` from now on: a rotation.
    pub fn publish(&self, keys: &[TestKey]) {
        *self.state.published.lock().unwrap() = keys.to_vec();
    }

    /// While set, every request answers 503.
    pub fn set_failing(&self, failing: bool) {
        self.state.failing.store(failing, Ordering::SeqCst);
    }

    /// Every answer waits this long first.
    pub fn set_delay(&self, delay: Duration) {
        self.state
            .delay_ms
            .store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// While set, the metadata names a key set on another host.
    pub fn set_foreign_jwks(&self, foreign: bool) {
        self.state.foreign_jwks.store(foreign, Ordering::SeqCst);
    }

    /// While set, the metadata names another issuer.
    pub fn set_wrong_issuer(&self, wrong: bool) {
        self.state.wrong_issuer.store(wrong, Ordering::SeqCst);
    }

    /// Metadata documents served so far (failed ones included).
    pub fn metadata_fetches(&self) -> usize {
        self.state.metadata_fetches.load(Ordering::SeqCst)
    }

    /// Key sets served so far (failed ones included).
    pub fn jwks_fetches(&self) -> usize {
        self.state.jwks_fetches.load(Ordering::SeqCst)
    }

    /// Settings that trust this stub: [`TENANT`] and [`OTHER_TENANT`], [`API_CLIENT_ID`],
    /// `access_as_user`, 24 hours.
    pub fn settings(&self) -> EntraSettings {
        EntraSettings {
            authority_host: self.state.authority.clone(),
            tenants: vec![
                Uuid::parse_str(TENANT).expect("a GUID"),
                Uuid::parse_str(OTHER_TENANT).expect("a GUID"),
            ],
            audiences: vec![API_CLIENT_ID.to_owned()],
            required_scope: Some("access_as_user".to_owned()),
            required_role: None,
            client_apps: Vec::new(),
            max_session: Duration::from_secs(24 * 3600),
        }
    }

    /// auth-svc's variables for Entra-only sign-in trusting this stub.
    pub fn variables(&self) -> Vec<(String, String)> {
        vec![
            ("PUSH_AUTH_PROVIDERS".to_owned(), "entra".to_owned()),
            (
                "PUSH_ENTRA_AUTHORITY_HOST".to_owned(),
                self.state.authority.clone(),
            ),
            (
                "PUSH_ENTRA_ALLOWED_TENANTS".to_owned(),
                format!("{TENANT},{OTHER_TENANT}"),
            ),
            ("PUSH_ENTRA_CLIENT_ID".to_owned(), API_CLIENT_ID.to_owned()),
        ]
    }

    /// The claims of a delegated v2.0 access token for [`API_CLIENT_ID`] from `tenant`,
    /// for the user `object_id`, valid for an hour.
    pub fn user_claims(&self, tenant: &str, object_id: &str) -> Value {
        let now = unix_now();
        json!({
            "aud": API_CLIENT_ID,
            "iss": self.issuer(tenant),
            "iat": now,
            "nbf": now,
            "exp": now + 3600,
            "ver": "2.0",
            "tid": tenant,
            "oid": object_id,
            "sub": format!("pairwise-{object_id}"),
            "scp": "access_as_user",
            "azp": CLIENT_APP,
            "azpacr": "0",
            "name": "Test User",
            "preferred_username": "test.user@contoso.test",
        })
    }

    /// A valid token of [`TENANT`]'s user `object_id`, signed with [`KEY_A`].
    pub fn token(&self, object_id: &str) -> String {
        sign(KEY_A, &self.user_claims(TENANT, object_id))
    }
}

/// Waits the configured delay; `Some(503)` while failing.
async fn pause(stub: &Stub) -> Option<Response> {
    let delay = stub.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    stub.failing
        .load(Ordering::SeqCst)
        .then(|| StatusCode::SERVICE_UNAVAILABLE.into_response())
}

async fn metadata(State(stub): State<Arc<Stub>>, Path(tenant): Path<String>) -> Response {
    stub.metadata_fetches.fetch_add(1, Ordering::SeqCst);
    if let Some(refused) = pause(&stub).await {
        return refused;
    }
    let issuer = if stub.wrong_issuer.load(Ordering::SeqCst) {
        format!("https://sts.example.test/{tenant}/v2.0")
    } else {
        format!("{}{tenant}/v2.0", stub.authority)
    };
    let jwks_uri = if stub.foreign_jwks.load(Ordering::SeqCst) {
        "http://203.0.113.9/keys".to_owned()
    } else {
        format!("{}{tenant}/discovery/v2.0/keys", stub.authority)
    };
    Json(json!({
        "issuer": issuer,
        "jwks_uri": jwks_uri,
        "id_token_signing_alg_values_supported": ["RS256"],
    }))
    .into_response()
}

async fn keys(State(stub): State<Arc<Stub>>) -> Response {
    stub.jwks_fetches.fetch_add(1, Ordering::SeqCst);
    if let Some(refused) = pause(&stub).await {
        return refused;
    }
    let keys: Vec<Value> = stub
        .published
        .lock()
        .unwrap()
        .iter()
        .map(|key| key.jwk())
        .collect();
    Json(json!({ "keys": keys })).into_response()
}

#[cfg(test)]
mod tests {
    use jsonwebtoken::{DecodingKey, Validation};

    use super::*;

    #[test]
    fn each_test_key_verifies_its_own_signatures_only() {
        let claims = json!({"aud": API_CLIENT_ID, "exp": unix_now() + 60});
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[API_CLIENT_ID]);
        for (key, other) in [(KEY_A, KEY_B), (KEY_B, KEY_A)] {
            let decoding = DecodingKey::from_rsa_components(key.n, key.e).unwrap();
            let own = jsonwebtoken::decode::<Value>(&sign(key, &claims), &decoding, &validation);
            assert!(own.is_ok(), "{own:?}");
            let foreign = jsonwebtoken::decode::<Value>(&sign(other, &claims), &decoding, &validation);
            assert!(foreign.is_err());
        }
    }

    #[tokio::test]
    async fn the_stub_publishes_metadata_and_keys_for_any_tenant() {
        let stub = StubOpenId::start().await;
        let base = stub.authority().to_owned();
        let metadata: Value =
            reqwest::get(format!("{base}{TENANT}/v2.0/.well-known/openid-configuration"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        assert_eq!(metadata["issuer"], stub.issuer(TENANT));
        let keys: Value = reqwest::get(metadata["jwks_uri"].as_str().unwrap())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(keys["keys"][0]["kid"], KEY_A.kid);
        stub.set_failing(true);
        let refused = reqwest::get(format!("{base}{TENANT}/discovery/v2.0/keys"))
            .await
            .unwrap();
        assert_eq!(refused.status(), 503);
        assert_eq!((stub.metadata_fetches(), stub.jwks_fetches()), (1, 2));
    }
}
```

- [ ] **Step 3: Run.**

Run: `cargo test -p push-auth --lib entra::testing`
Expected: 2 tests pass. If `the_stub_publishes…` fails with a proxy error, the shell has
`HTTP_PROXY` set: run with `$env:NO_PROXY = '127.0.0.1,localhost'` (the services honour the
same variables, D4).

- [ ] **Step 4: The feature builds alone.**

Run: `cargo build -p push-auth --features testing`
Expected: builds (the stub uses only regular dependencies).

- [ ] **Step 5: Commit.**

```powershell
git add -A
git commit -m "test(auth): add a stub Entra ID issuer with fixed RSA test keys"
```

### Task 4: Each tenant's signing keys

**Files:**
- Create: `crates\push-auth\src\entra\keys.rs`
- Modify: `crates\push-auth\src\entra\mod.rs`

- [ ] **Step 1: Declare.** In `entra\mod.rs` add `mod keys;` after `mod claims;` and
`pub use keys::KeyTiming;` after the `pub use settings::…` line.

- [ ] **Step 2: Write the failing tests.** Create `crates\push-auth\src\entra\keys.rs` with
the tests module:

```rust
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::entra::testing::{KEY_A, KEY_B, StubOpenId, TENANT};

    fn quick() -> KeyTiming {
        KeyTiming {
            max_age: Duration::from_secs(3600),
            stale_for: Duration::from_secs(3600),
            unknown_kid_cooldown: Duration::ZERO,
            retry_after_failure: Duration::ZERO,
            fetch_timeout: Duration::from_secs(2),
        }
    }

    fn tenant() -> Uuid {
        Uuid::parse_str(TENANT).unwrap()
    }

    #[tokio::test]
    async fn keys_are_fetched_once_and_cached() {
        let stub = StubOpenId::start().await;
        let keys = KeySets::new(&stub.settings(), quick()).unwrap();
        assert!(keys.key(tenant(), KEY_A.kid).await.unwrap().is_some());
        assert!(keys.key(tenant(), KEY_A.kid).await.unwrap().is_some());
        assert_eq!((stub.metadata_fetches(), stub.jwks_fetches()), (1, 1));
        // A tenant the settings do not list is never fetched.
        assert!(keys.key(Uuid::from_u128(7), KEY_A.kid).await.unwrap().is_none());
        assert_eq!(stub.metadata_fetches(), 1);
    }

    #[tokio::test]
    async fn an_unknown_kid_fetches_again_but_not_within_the_cooldown() {
        let stub = StubOpenId::start().await;
        let keys = KeySets::new(&stub.settings(), quick()).unwrap();
        keys.key(tenant(), KEY_A.kid).await.unwrap();
        stub.publish(&[KEY_A, KEY_B]);
        assert!(
            keys.key(tenant(), KEY_B.kid).await.unwrap().is_some(),
            "the rotated key is found without a restart"
        );
        assert_eq!(stub.jwks_fetches(), 2);

        let patient = KeySets::new(
            &stub.settings(),
            KeyTiming {
                unknown_kid_cooldown: Duration::from_secs(3600),
                ..quick()
            },
        )
        .unwrap();
        patient.key(tenant(), KEY_A.kid).await.unwrap();
        for kid in ["forged-1", "forged-2", "forged-3"] {
            assert!(patient.key(tenant(), kid).await.unwrap().is_none());
        }
        assert_eq!(
            stub.jwks_fetches(),
            3,
            "unknown kids within the cooldown fetch nothing"
        );
    }

    #[tokio::test]
    async fn an_outage_before_the_first_fetch_fails_closed_and_recovers() {
        let stub = StubOpenId::start().await;
        let keys = KeySets::new(&stub.settings(), quick()).unwrap();
        stub.set_failing(true);
        let error = keys.key(tenant(), KEY_A.kid).await.unwrap_err();
        assert!(
            matches!(&error, EntraError::Unavailable(reason) if reason.contains("HTTP 503")),
            "{error}"
        );
        stub.set_failing(false);
        assert!(keys.key(tenant(), KEY_A.kid).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn cached_keys_outlive_an_outage_until_they_are_too_old() {
        let stub = StubOpenId::start().await;
        let lenient = KeySets::new(
            &stub.settings(),
            KeyTiming {
                max_age: Duration::ZERO,
                ..quick()
            },
        )
        .unwrap();
        let strict = KeySets::new(
            &stub.settings(),
            KeyTiming {
                max_age: Duration::ZERO,
                stale_for: Duration::ZERO,
                ..quick()
            },
        )
        .unwrap();
        lenient.key(tenant(), KEY_A.kid).await.unwrap();
        strict.key(tenant(), KEY_A.kid).await.unwrap();
        stub.set_failing(true);
        assert!(
            lenient.key(tenant(), KEY_A.kid).await.unwrap().is_some(),
            "stale keys while the authority is down"
        );
        assert!(matches!(
            strict.key(tenant(), KEY_A.kid).await,
            Err(EntraError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_wrong_issuer_a_foreign_key_set_or_a_slow_authority_give_no_keys() {
        let stub = StubOpenId::start().await;
        stub.set_wrong_issuer(true);
        let error = KeySets::new(&stub.settings(), quick())
            .unwrap()
            .key(tenant(), KEY_A.kid)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("another issuer"), "{error}");
        stub.set_wrong_issuer(false);

        stub.set_foreign_jwks(true);
        let error = KeySets::new(&stub.settings(), quick())
            .unwrap()
            .key(tenant(), KEY_A.kid)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("authority host"), "{error}");
        assert_eq!(stub.jwks_fetches(), 0);
        stub.set_foreign_jwks(false);

        stub.set_delay(Duration::from_secs(5));
        let slow = KeySets::new(
            &stub.settings(),
            KeyTiming {
                fetch_timeout: Duration::from_millis(300),
                ..quick()
            },
        )
        .unwrap();
        let started = Instant::now();
        let error = slow.key(tenant(), KEY_A.kid).await.unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
```

- [ ] **Step 3: Run them to see them fail.**

Run: `cargo test -p push-auth --lib entra::keys`
Expected: compile errors (`KeySets`, `KeyTiming` not found).

- [ ] **Step 4: Implement.** Put above the tests in `keys.rs`:

```rust
//! Each allowed tenant's signing keys (spec D4). The tenant's OpenID metadata names its
//! issuer, which must be the expected one, and its key set, which must be on the authority
//! host. Keys are fetched on first use (and warmed at start-up), kept for `max_age`, fetched
//! again for a token naming an unknown key at most every `unknown_kid_cooldown`, and kept
//! up to `stale_for` longer while the authority is down. Without usable keys every Entra
//! sign-in fails closed. A tenant's fetches are serialised by its lock, so a burst of
//! sign-ins fetches once.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::DecodingKey;
use reqwest::Url;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use super::{EntraError, EntraSettings};

/// Largest metadata or key set read.
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
/// Most keys taken from one key set.
const MAX_KEYS: usize = 64;
/// Longest wait for a connection.
const MAX_CONNECT: Duration = Duration::from_secs(5);

/// How keys are kept and fetched. Tests shorten these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyTiming {
    /// Keys older than this are fetched again before use.
    pub max_age: Duration,
    /// While fetching fails, keys up to `max_age + stale_for` old are still used.
    pub stale_for: Duration,
    /// A token naming an unknown key fetches the keys again at most this often.
    pub unknown_kid_cooldown: Duration,
    /// After a failed fetch, the next one waits this long; sign-ins fail meanwhile unless
    /// cached keys are still usable.
    pub retry_after_failure: Duration,
    /// Each request's limit, the answer included.
    pub fetch_timeout: Duration,
}

impl Default for KeyTiming {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(24 * 3600),
            stale_for: Duration::from_secs(24 * 3600),
            unknown_kid_cooldown: Duration::from_secs(300),
            retry_after_failure: Duration::from_secs(10),
            fetch_timeout: Duration::from_secs(10),
        }
    }
}

/// The keys of every allowed tenant.
pub(crate) struct KeySets {
    http: reqwest::Client,
    authority: Url,
    timing: KeyTiming,
    tenants: HashMap<Uuid, Tenant>,
}

struct Tenant {
    issuer: String,
    metadata_url: String,
    cache: tokio::sync::Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
    tried_at: Option<Instant>,
    last_error: Option<String>,
}

#[derive(Deserialize)]
struct Metadata {
    issuer: String,
    jwks_uri: String,
}

#[derive(Deserialize)]
struct KeySet {
    #[serde(default)]
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    #[serde(default)]
    kty: String,
    #[serde(default, rename = "use")]
    usage: Option<String>,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
}

impl KeySets {
    pub(crate) fn new(settings: &EntraSettings, timing: KeyTiming) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(timing.fetch_timeout.min(MAX_CONNECT))
            .timeout(timing.fetch_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "cannot build the HTTP client for Entra ID".to_owned())?;
        let authority = Url::parse(&settings.authority_host)
            .map_err(|_| "the Entra ID authority host is not a URL".to_owned())?;
        let tenants = settings
            .tenants
            .iter()
            .map(|&tenant| {
                let entry = Tenant {
                    issuer: settings.issuer(tenant),
                    metadata_url: settings.metadata_url(tenant),
                    cache: tokio::sync::Mutex::new(Cache::default()),
                };
                (tenant, entry)
            })
            .collect();
        Ok(Self {
            http,
            authority,
            timing,
            tenants,
        })
    }

    /// Fetches every tenant's keys that are not cached yet. Failures are logged, not
    /// fatal: readiness never depends on Entra ID.
    pub(crate) async fn warm(&self) {
        for (tenant, entry) in &self.tenants {
            let mut cache = entry.cache.lock().await;
            if cache.fetched_at.is_some() {
                continue;
            }
            cache.tried_at = Some(Instant::now());
            match self.refresh(entry).await {
                Ok(keys) => {
                    cache.keys = keys;
                    cache.fetched_at = Some(Instant::now());
                    tracing::info!(%tenant, "fetched the Entra ID signing keys");
                }
                Err(error) => {
                    tracing::warn!(%tenant, %error, "cannot fetch the Entra ID signing keys yet; sign-ins try again");
                    cache.last_error = Some(error);
                }
            }
        }
    }

    /// The key `kid` of `tenant`; `None` if the tenant is not allowed or does not publish
    /// that key. `Unavailable` when no usable keys can be had.
    pub(crate) async fn key(
        &self,
        tenant: Uuid,
        kid: &str,
    ) -> Result<Option<DecodingKey>, EntraError> {
        let Some(entry) = self.tenants.get(&tenant) else {
            return Ok(None);
        };
        let mut cache = entry.cache.lock().await;
        let now = Instant::now();
        let fresh = cache
            .fetched_at
            .is_some_and(|at| now.duration_since(at) < self.timing.max_age);
        let wait = if fresh {
            self.timing.unknown_kid_cooldown
        } else {
            self.timing.retry_after_failure
        };
        let due = cache
            .tried_at
            .is_none_or(|at| now.duration_since(at) >= wait);
        if due && (!fresh || !cache.keys.contains_key(kid)) {
            cache.tried_at = Some(now);
            match self.refresh(entry).await {
                Ok(keys) => {
                    cache.keys = keys;
                    cache.fetched_at = Some(Instant::now());
                    cache.last_error = None;
                    return Ok(cache.keys.get(kid).cloned());
                }
                Err(error) => {
                    tracing::warn!(%tenant, %error, "cannot fetch the Entra ID signing keys");
                    cache.last_error = Some(error);
                }
            }
        }
        let usable = cache.fetched_at.is_some_and(|at| {
            now.duration_since(at) < self.timing.max_age + self.timing.stale_for
        });
        if !usable {
            let reason = cache
                .last_error
                .clone()
                .unwrap_or_else(|| "no current signing keys".to_owned());
            return Err(EntraError::Unavailable(reason));
        }
        Ok(cache.keys.get(kid).cloned())
    }

    /// The tenant's RSA signing keys, by key id.
    async fn refresh(&self, tenant: &Tenant) -> Result<HashMap<String, DecodingKey>, String> {
        let metadata: Metadata = self.document(&tenant.metadata_url).await?;
        if metadata.issuer != tenant.issuer {
            return Err("the metadata names another issuer".to_owned());
        }
        let jwks_url = self.on_authority(&metadata.jwks_uri)?;
        let set: KeySet = self.document(jwks_url.as_str()).await?;
        let keys: HashMap<String, DecodingKey> = set
            .keys
            .into_iter()
            .take(MAX_KEYS)
            .filter(|key| key.kty == "RSA" && key.usage.as_deref().is_none_or(|usage| usage == "sig"))
            .filter_map(|key| {
                let decoding =
                    DecodingKey::from_rsa_components(key.n.as_deref()?, key.e.as_deref()?).ok()?;
                Some((key.kid?, decoding))
            })
            .collect();
        if keys.is_empty() {
            return Err("the key set has no RSA signing key".to_owned());
        }
        Ok(keys)
    }

    /// `url` if it is on the authority's scheme, host and port: egress stays one host.
    fn on_authority(&self, url: &str) -> Result<Url, String> {
        let url =
            Url::parse(url).map_err(|_| "the metadata's jwks_uri is not a URL".to_owned())?;
        let same = url.scheme() == self.authority.scheme()
            && url.host_str() == self.authority.host_str()
            && url.port_or_known_default() == self.authority.port_or_known_default()
            && url.username().is_empty()
            && url.password().is_none();
        if !same {
            return Err("the metadata's jwks_uri is not on the authority host".to_owned());
        }
        Ok(url)
    }

    /// A JSON document, read up to [`MAX_DOCUMENT_BYTES`].
    async fn document<T: DeserializeOwned>(&self, url: &str) -> Result<T, String> {
        let mut response = self.http.get(url).send().await.map_err(describe)?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(describe)? {
            if body.len() + chunk.len() > MAX_DOCUMENT_BYTES {
                return Err("the answer is larger than 256 KiB".to_owned());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| "the answer is not the expected JSON".to_owned())
    }
}

fn describe(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "the request timed out".to_owned()
    } else {
        "the authority could not be reached".to_owned()
    }
}
```

- [ ] **Step 5: Run.**

Run: `cargo test -p push-auth --lib entra::keys`
Expected: 5 tests pass.

- [ ] **Step 6: Commit.**

```powershell
git add -A
git commit -m "feat(auth): cache each tenant's Entra ID signing keys and fail closed"
```

### Task 5: The verifier

**Files:**
- Modify: `crates\push-auth\src\entra\mod.rs`

- [ ] **Step 1: Write the failing tests.** Append to `entra\mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::testing::{
        CLIENT_APP, KEY_A, KEY_B, OTHER_TENANT, StubOpenId, TENANT, sign, sign_as, unix_now,
        with_algorithm,
    };
    use super::*;

    const USER: &str = "55555555-0000-4000-8000-000000000005";

    fn quick() -> KeyTiming {
        KeyTiming {
            unknown_kid_cooldown: Duration::ZERO,
            retry_after_failure: Duration::ZERO,
            fetch_timeout: Duration::from_secs(2),
            ..KeyTiming::default()
        }
    }

    async fn verifier() -> (StubOpenId, EntraSignIn) {
        let stub = StubOpenId::start().await;
        let entra = EntraSignIn::with_timing(stub.settings(), quick()).unwrap();
        (stub, entra)
    }

    fn changed(mut claims: Value, name: &str, value: Value) -> Value {
        claims[name] = value;
        claims
    }

    fn without(mut claims: Value, name: &str) -> Value {
        claims.as_object_mut().unwrap().remove(name);
        claims
    }

    #[tokio::test]
    async fn a_delegated_token_for_this_api_is_accepted() {
        let (stub, entra) = verifier().await;
        let identity = entra.verify(&stub.token(USER)).await.unwrap();
        assert_eq!(identity.tenant_id.to_string(), TENANT);
        assert_eq!(identity.object_id.to_string(), USER);
        assert_eq!(identity.name.as_deref(), Some("Test User"));
        assert_eq!(identity.email.as_deref(), Some("test.user@contoso.test"));
        // Clock skew within the 60 s leeway.
        let early = changed(stub.user_claims(TENANT, USER), "nbf", json!(unix_now() + 50));
        assert!(entra.verify(&sign(KEY_A, &early)).await.is_ok());
        let other = stub.user_claims(OTHER_TENANT, USER);
        let identity = entra.verify(&sign(KEY_A, &other)).await.unwrap();
        assert_eq!(identity.tenant_id.to_string(), OTHER_TENANT);
    }

    #[tokio::test]
    async fn every_kind_of_wrong_token_is_refused_with_its_reason() {
        let (stub, entra) = verifier().await;
        let good = stub.user_claims(TENANT, USER);
        let now = unix_now();
        let elsewhere = "66666666-0000-4000-8000-000000000006";
        let id_token = json!({
            "aud": CLIENT_APP, "iss": stub.issuer(TENANT), "iat": now, "nbf": now,
            "exp": now + 3600, "ver": "2.0", "tid": TENANT, "oid": USER, "sub": "pairwise",
            "nonce": "n-0S6_WzA2Mj", "name": "Test User",
            "preferred_username": "test.user@contoso.test",
        });
        let app_only = without(changed(good.clone(), "roles", json!(["Messages.Send"])), "scp");
        let cases: Vec<(&str, String, Reason)> = vec![
            (
                "a Graph token",
                sign(KEY_A, &changed(good.clone(), "aud", json!("00000003-0000-0000-c000-000000000000"))),
                Reason::Audience,
            ),
            (
                "an audience that is not configured",
                sign(KEY_A, &changed(good.clone(), "aud", json!("api://push-api"))),
                Reason::Audience,
            ),
            (
                "a tenant that is not allowed",
                sign(
                    KEY_A,
                    &changed(
                        changed(good.clone(), "tid", json!(elsewhere)),
                        "iss",
                        json!(stub.issuer(elsewhere)),
                    ),
                ),
                Reason::Tenant,
            ),
            (
                "tid and iss disagree",
                sign(KEY_A, &changed(good.clone(), "tid", json!(OTHER_TENANT))),
                Reason::Issuer,
            ),
            (
                "the v1.0 issuer",
                sign(
                    KEY_A,
                    &changed(good.clone(), "iss", json!(format!("https://sts.windows.net/{TENANT}/"))),
                ),
                Reason::Issuer,
            ),
            (
                "expired",
                sign(KEY_A, &changed(good.clone(), "exp", json!(now - 120))),
                Reason::Expired,
            ),
            (
                "not yet valid",
                sign(KEY_A, &changed(good.clone(), "nbf", json!(now + 300))),
                Reason::NotYetValid,
            ),
            ("a bad signature", sign_as(KEY_B, KEY_A.kid, &good), Reason::Signature),
            ("an unpublished key", sign(KEY_B, &good), Reason::Signature),
            ("alg none", with_algorithm("none", KEY_A.kid, &good, ""), Reason::Algorithm),
            (
                "alg HS256",
                with_algorithm("HS256", KEY_A.kid, &good, "c2lnbmF0dXJl"),
                Reason::Algorithm,
            ),
            (
                "alg RS384",
                with_algorithm("RS384", KEY_A.kid, &good, "c2lnbmF0dXJl"),
                Reason::Algorithm,
            ),
            (
                "version 1.0",
                sign(KEY_A, &changed(good.clone(), "ver", json!("1.0"))),
                Reason::Version,
            ),
            (
                "another scope",
                sign(KEY_A, &changed(good.clone(), "scp", json!("User.Read"))),
                Reason::Scope,
            ),
            ("app-only", sign(KEY_A, &app_only), Reason::AppOnly),
            (
                "app-only with idtyp",
                sign(KEY_A, &changed(good.clone(), "idtyp", json!("app"))),
                Reason::AppOnly,
            ),
            ("an ID token", sign(KEY_A, &id_token), Reason::Audience),
            ("no oid", sign(KEY_A, &without(good.clone(), "oid")), Reason::Subject),
            ("no exp", sign(KEY_A, &without(good.clone(), "exp")), Reason::Malformed),
            (
                "a string exp",
                sign(KEY_A, &changed(good.clone(), "exp", json!("tomorrow"))),
                Reason::Malformed,
            ),
            ("not a JWT", "not-a-token".to_owned(), Reason::Malformed),
            ("too long", "a.b.".repeat(3000), Reason::Malformed),
        ];
        for (case, token, reason) in cases {
            assert_eq!(
                entra.verify(&token).await,
                Err(EntraError::Rejected(reason)),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn a_rotated_key_works_without_a_restart_and_an_outage_fails_closed() {
        let (stub, entra) = verifier().await;
        assert!(entra.verify(&stub.token(USER)).await.is_ok());
        stub.publish(&[KEY_B]);
        let rotated = sign(KEY_B, &stub.user_claims(TENANT, USER));
        assert!(entra.verify(&rotated).await.is_ok());

        let (down, cold) = verifier().await;
        down.set_failing(true);
        assert!(matches!(
            cold.verify(&down.token(USER)).await,
            Err(EntraError::Unavailable(_))
        ));
        // A token of a tenant that is not allowed never reaches the authority.
        let fetched = down.metadata_fetches();
        let foreign = "66666666-0000-4000-8000-000000000006";
        let claims = changed(
            changed(down.user_claims(TENANT, USER), "tid", json!(foreign)),
            "iss",
            json!(down.issuer(foreign)),
        );
        assert_eq!(
            cold.verify(&sign(KEY_A, &claims)).await,
            Err(EntraError::Rejected(Reason::Tenant))
        );
        assert_eq!(down.metadata_fetches(), fetched);
    }
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p push-auth --lib entra::tests`
Expected: compile errors (`EntraSignIn` not found).

- [ ] **Step 3: Implement.** In `entra\mod.rs`, replace the `use uuid::Uuid;` line with:

```rust
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, Validation};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;
```

and add, after the `EntraIdentity` `Debug` impl and before the tests:

```rust
/// The longest token accepted, in bytes.
const MAX_TOKEN_BYTES: usize = 8 * 1024;
/// Clock skew allowed on `exp` and `nbf`.
pub const LEEWAY_SECONDS: u64 = 60;

/// Checks Entra ID access tokens for this API (spec D2): RS256 with a published key of
/// an allowed tenant, that tenant's v2.0 issuer, a configured audience, `exp`/`nbf`
/// within 60 s, then the claim rules.
#[derive(Clone)]
pub struct EntraSignIn {
    settings: Arc<EntraSettings>,
    keys: Arc<keys::KeySets>,
}

impl std::fmt::Debug for EntraSignIn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EntraSignIn")
            .field("tenants", &self.settings.tenants)
            .field("audiences", &self.settings.audiences)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokenHeader {
    #[serde(default)]
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

/// What is read before the signature is checked, only to choose the tenant and to
/// refuse tokens of other tenants without fetching anything.
#[derive(Deserialize)]
struct Unverified {
    #[serde(default)]
    tid: Option<String>,
    #[serde(default)]
    iss: Option<String>,
}

impl EntraSignIn {
    pub fn new(settings: EntraSettings) -> Result<Self, String> {
        Self::with_timing(settings, KeyTiming::default())
    }

    /// As [`EntraSignIn::new`] with other key timings (tests).
    pub fn with_timing(settings: EntraSettings, timing: KeyTiming) -> Result<Self, String> {
        let keys = keys::KeySets::new(&settings, timing)?;
        Ok(Self {
            settings: Arc::new(settings),
            keys: Arc::new(keys),
        })
    }

    pub fn settings(&self) -> &EntraSettings {
        &self.settings
    }

    /// How long an Entra-backed device stays signed in (spec D8).
    pub fn max_session(&self) -> Duration {
        self.settings.max_session
    }

    /// Fetches every allowed tenant's keys once; failures are logged.
    pub async fn warm(&self) {
        self.keys.warm().await;
    }

    /// The user `token` names, if it is an acceptable access token for this API. A
    /// refusal is logged with its reason, never with the token or its claims.
    pub async fn verify(&self, token: &str) -> Result<EntraIdentity, EntraError> {
        let checked = self.check(token).await;
        match &checked {
            Err(EntraError::Rejected(reason)) => tracing::warn!(
                target: "security",
                event = "auth.entra.rejected",
                reason = reason.as_str(),
                "an Entra ID access token was refused"
            ),
            Err(EntraError::Unavailable(error)) => {
                tracing::warn!(%error, "an Entra ID sign-in failed: no usable signing keys");
            }
            Ok(_) => {}
        }
        checked
    }

    async fn check(&self, token: &str) -> Result<EntraIdentity, EntraError> {
        let malformed = EntraError::Rejected(Reason::Malformed);
        let parts: Vec<&str> = token.split('.').collect();
        if token.len() > MAX_TOKEN_BYTES || parts.len() != 3 {
            return Err(malformed);
        }
        let header: TokenHeader = decode_part(parts[0])?;
        if header.alg != "RS256" {
            return Err(EntraError::Rejected(Reason::Algorithm));
        }
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty() && kid.len() <= 128)
            .ok_or(malformed)?;
        let unverified: Unverified = decode_part(parts[1])?;
        let tenant = unverified
            .tid
            .as_deref()
            .and_then(|tid| Uuid::try_parse(tid).ok())
            .filter(|tenant| self.settings.tenants.contains(tenant))
            .ok_or(EntraError::Rejected(Reason::Tenant))?;
        let issuer = self.settings.issuer(tenant);
        if unverified.iss.as_deref() != Some(issuer.as_str()) {
            return Err(EntraError::Rejected(Reason::Issuer));
        }
        let key = self
            .keys
            .key(tenant, &kid)
            .await?
            .ok_or(EntraError::Rejected(Reason::Signature))?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[issuer.as_str()]);
        validation.set_audience(&self.settings.audiences);
        validation.set_required_spec_claims(&["exp", "nbf", "iss", "aud"]);
        validation.validate_nbf = true;
        validation.leeway = LEEWAY_SECONDS;
        let claims = jsonwebtoken::decode::<claims::EntraClaims>(token, &key, &validation)
            .map_err(|error| EntraError::Rejected(reason_of(&error)))?
            .claims;
        claims::check(&claims, tenant, &self.settings).map_err(EntraError::Rejected)
    }
}

fn decode_part<T: DeserializeOwned>(part: &str) -> Result<T, EntraError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| EntraError::Rejected(Reason::Malformed))?;
    serde_json::from_slice(&bytes).map_err(|_| EntraError::Rejected(Reason::Malformed))
}

fn reason_of(error: &jsonwebtoken::errors::Error) -> Reason {
    use jsonwebtoken::errors::ErrorKind;
    match error.kind() {
        ErrorKind::ExpiredSignature => Reason::Expired,
        ErrorKind::ImmatureSignature => Reason::NotYetValid,
        ErrorKind::InvalidAudience => Reason::Audience,
        ErrorKind::InvalidIssuer => Reason::Issuer,
        ErrorKind::InvalidSignature => Reason::Signature,
        ErrorKind::InvalidAlgorithm => Reason::Algorithm,
        _ => Reason::Malformed,
    }
}
```

- [ ] **Step 4: Run.**

Run: `cargo test -p push-auth --lib entra`
Expected: every `entra` test passes (settings 6, claims 5, testing 2, keys 5, verifier 3).

- [ ] **Step 5: Commit.**

```powershell
git add -A
git commit -m "feat(auth): verify Entra ID access tokens for this API"
```

### Task 6: The schema

**Files:**
- Create: `crates\push-auth\migrations\0004_entra.sql`
- Modify: `crates\push-auth\tests\schema.rs`

- [ ] **Step 1: Write the failing test.** In `crates\push-auth\tests\schema.rs`, change the
expected table list in `the_owner_migrates_and_a_second_run_changes_nothing` to

```rust
        ["device", "external_identity", "outbox", "refresh_token", "user_account"]
```

and append:

```rust
#[tokio::test]
async fn an_external_account_has_no_email_or_password_and_the_cleanup_keeps_it() {
    let (_container, url) = auth_database().await;
    let pool = migrated(&url).await;
    grant_worker_access(&url, "push_worker")
        .await
        .expect("grant");
    // Every account was created 8 days ago.
    let account = |handle: &'static str,
                   email: Option<&'static str>,
                   password: Option<&'static str>,
                   active: bool| {
        let pool = pool.clone();
        async move {
            let user = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO auth.user_account \
                 (user_id, email, handle, display_name, password_hash, email_verified_at, created_at) \
                 VALUES ($1, $2, $3, 'Someone', $4, CASE WHEN $5 THEN now() END, \
                 now() - interval '8 days')",
            )
            .bind(user)
            .bind(email)
            .bind(handle)
            .bind(password)
            .bind(active)
            .execute(&pool)
            .await
            .map(|_| user)
        }
    };
    let erin = account("erin", None, None, true)
        .await
        .expect("an active external account");
    for (handle, email, password, active) in [
        ("fay", None, None, false),
        ("gus", None, Some("hash"), true),
        ("hal", Some("hal@example.com"), None, true),
    ] {
        let error = account(handle, email, password, active)
            .await
            .expect_err(handle);
        assert_eq!(constraint(error), "user_account_kind", "{handle}");
    }

    let tenant = Uuid::now_v7();
    let identity = |user: Uuid, subject: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO auth.external_identity \
                 (identity_id, provider, tenant_id, subject, user_id) \
                 VALUES ($1, 'entra', $2, $3, $4)",
            )
            .bind(Uuid::now_v7())
            .bind(tenant)
            .bind(subject)
            .bind(user)
            .execute(&pool)
            .await
        }
    };
    identity(erin, "oid-1").await.expect("an identity");
    let ivy = account("ivy", None, None, true).await.expect("ivy");
    assert_eq!(
        constraint(identity(ivy, "oid-1").await.expect_err("the same subject")),
        "external_identity_provider_tenant_id_subject_key"
    );
    assert_eq!(
        constraint(identity(erin, "oid-2").await.expect_err("a second identity")),
        "external_identity_user_id_key"
    );

    // An email/password account unverified for 8 days is deleted; external ones are kept.
    account("joe", Some("joe@example.com"), Some("hash"), false)
        .await
        .expect("joe");
    let worker = PgPool::connect(&worker_url(&url))
        .await
        .expect("connect as the worker");
    let deleted: i32 =
        sqlx::query_scalar("SELECT auth.delete_unverified_accounts(interval '7 days', 100)")
            .fetch_one(&worker)
            .await
            .expect("the cleanup");
    assert_eq!(deleted, 1);
    let left: Vec<String> =
        sqlx::query_scalar("SELECT handle::text FROM auth.user_account ORDER BY handle")
            .fetch_all(&pool)
            .await
            .expect("the accounts");
    assert_eq!(left, ["erin", "ivy"]);
}
```

- [ ] **Step 2: Run it to see it fail.**

Run: `cargo test -p push-auth --test schema`
Expected: both changed tests FAIL (no `external_identity`; the insert of `erin` violates the
`NOT NULL` of `email`).

- [ ] **Step 3: The migration.** Create `crates\push-auth\migrations\0004_entra.sql`:

```sql
-- Microsoft Entra ID sign-in (spec 2026-10-19). An Entra account has no email address and
-- no password here: the user is found by the immutable (tenant, object id) of the token,
-- never by an address, and is active from the first sign-in (email_verified_at is the time
-- of that sign-in, so lookup, profiles and the cleanup treat it like a verified account).
-- Email/password accounts are unchanged; the two are never linked.

ALTER TABLE auth.user_account ALTER COLUMN email DROP NOT NULL;
ALTER TABLE auth.user_account ALTER COLUMN password_hash DROP NOT NULL;
-- An email/password account, or an external one, which is always active.
ALTER TABLE auth.user_account ADD CONSTRAINT user_account_kind CHECK (
    (email IS NOT NULL AND password_hash IS NOT NULL)
    OR (email IS NULL AND password_hash IS NULL AND email_verified_at IS NOT NULL));

CREATE TABLE auth.external_identity (
    identity_id     uuid PRIMARY KEY,
    provider        text NOT NULL CHECK (provider IN ('entra')),
    -- Entra ID: the tenant (`tid`) and the user's object id (`oid`), both immutable.
    tenant_id       uuid NOT NULL,
    subject         text NOT NULL CHECK (length(subject) BETWEEN 1 AND 128),
    -- One identity per account: no linking (spec D5).
    user_id         uuid NOT NULL UNIQUE REFERENCES auth.user_account,
    -- The latest `email` or `preferred_username`: shown to its owner only, never used to
    -- find an account.
    email           text CHECK (length(email) <= 254),
    created_at      timestamptz NOT NULL DEFAULT now(),
    last_sign_in_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (provider, tenant_id, subject)
);

-- When an Entra-backed device's session ends, in seconds since the Unix epoch: refresh is
-- refused from then on until the user signs in with Entra ID again (spec D8). NULL: no
-- limit (email/password devices).
ALTER TABLE auth.device ADD COLUMN session_expires_at bigint;

-- As in 0002, and never an account without a password (defence in depth: external
-- accounts are active anyway). CREATE OR REPLACE keeps the worker's EXECUTE grant.
CREATE OR REPLACE FUNCTION auth.delete_unverified_accounts(older_than interval, batch integer)
RETURNS integer
LANGUAGE sql STRICT SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
    WITH gone AS (
        DELETE FROM auth.user_account
        WHERE user_id IN (
            SELECT account.user_id FROM auth.user_account AS account
            WHERE account.email_verified_at IS NULL
              AND account.password_hash IS NOT NULL
              AND account.created_at < statement_timestamp() - greatest(older_than, interval '1 day')
              AND NOT EXISTS (
                  SELECT 1 FROM auth.device WHERE device.user_id = account.user_id)
            LIMIT least(batch, 10000)
            FOR UPDATE SKIP LOCKED)
        RETURNING 1)
    SELECT count(*)::integer FROM gone
$$;
```

- [ ] **Step 4: Run.**

Run: `cargo test -p push-auth --test schema`
Expected: every schema test passes. Then `cargo test -p push-auth` : the other suites still
pass (no code reads a NULL yet; Task 7 makes that safe before anything writes one).

- [ ] **Step 5: Commit.**

```powershell
git add -A
git commit -m "feat(auth): add external identities, accounts without a password, and device session ends"
```

### Task 7: Accounts without a password; registration tokens that can bind an identity

No behaviour changes for email/password accounts; every place that reads a password hash
or an email copes with an account that has neither, and the token and error types gain
what Tasks 8 and 9 use.

**Files:**
- Modify: `crates\push-auth\src\links.rs`, `src\service.rs`, `src\devices.rs`, `src\profiles.rs`, `src\error.rs`, `src\api.rs`
- Modify: `crates\push-auth\tests\accounts.rs`, `tests\devices.rs`

- [ ] **Step 1: Write the failing test.** Append to `crates\push-auth\tests\accounts.rs`:

```rust
#[tokio::test]
async fn an_account_without_a_password_takes_no_password_link_or_token() {
    let world = world().await;
    let user = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth.user_account (user_id, handle, display_name, email_verified_at) \
         VALUES ($1, 'erin', 'Erin', now())",
    )
    .bind(user)
    .execute(&world.pool)
    .await
    .unwrap();
    let signer = LinkSigner::new(LINK_KEY.to_vec()).unwrap();
    let claims = |purpose| LinkClaims {
        sub: user,
        purpose,
        exp: i64::MAX,
        email: None,
        pw: Some(password_stamp("")),
        xid: None,
        sx: None,
    };
    assert_eq!(
        world
            .auth
            .reset_password(&signer.sign(&claims(Purpose::Reset)), "a new password")
            .await,
        Err(AuthError::LinkInvalid)
    );
    assert_eq!(
        world
            .auth
            .register_device(&signer.sign(&claims(Purpose::Register)), Uuid::now_v7(), "Phone", "ios")
            .await,
        Err(AuthError::Unauthorized)
    );
}
```

- [ ] **Step 2: Run it to see it fail.**

Run: `cargo test -p push-auth --test accounts an_account_without_a_password`
Expected: compile error (`LinkClaims` has no `xid`/`sx`).

- [ ] **Step 3: Token claims.** In `crates\push-auth\src\links.rs`:

Replace the module doc's sentence "and every token binds a fingerprint of the account's
password hash when it was made" with "and every token binds a fingerprint of the account's
password hash when it was made (or, for an Entra ID registration token, the identity that
signed in, spec 2026-10-19 D7)".

Replace the `pw` field's doc and add two fields after it:

```rust
    /// [`password_stamp`] of the account's password hash when the token was made. Every
    /// email link and every email/password registration token carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pw: Option<String>,
    /// Entra ID registration tokens: the `auth.external_identity` row that signed in. A
    /// registration token carries either `pw` or `xid` with `sx`, never both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xid: Option<Uuid>,
    /// Entra ID registration tokens: when the device's session ends, in seconds since the
    /// Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sx: Option<i64>,
```

In `links.rs`'s test `verify_claims()`, add `xid: None, sx: None,` after `pw: None,`.

Add `xid: None, sx: None,` after the `pw: …` line of every other `LinkClaims { … }` literal:
`src\service.rs` (`verify_link`, `login`, `forgot_password`), `tests\accounts.rs` (the
`claims` closure of `forged_expired_and_mismatched_links_are_refused`; the two
`LinkClaims { pw: …, ..claims(…) }` literals need nothing), `tests\devices.rs` (the three
literals in `each_token_does_only_its_own_job`).

- [ ] **Step 4: Errors.** In `crates\push-auth\src\error.rs` add variants before
`RateLimited`:

```rust
    #[error("the Microsoft Entra ID token is not accepted")]
    EntraTokenInvalid,
    #[error("Microsoft Entra ID cannot be reached to check the token; try again")]
    EntraUnavailable,
    #[error("the session has reached its maximum age; sign in again")]
    SessionExpired,
```

In `code()`:

```rust
            Self::EntraTokenInvalid => "entra_token_invalid",
            Self::EntraUnavailable => "entra_unavailable",
            Self::SessionExpired => "session_expired",
```

In `status()` change the 401 arm to
`Self::InvalidCredentials | Self::Unauthorized | Self::TokenExpired | Self::EntraTokenInvalid | Self::SessionExpired => 401,`
and the 503 arm to `Self::Unavailable | Self::EntraUnavailable => 503,`.
In `retryable()`: `matches!(self, Self::RateLimited { .. } | Self::Unavailable | Self::EntraUnavailable)`.

In `crates\push-auth\src\api.rs`, `impl IntoResponse for AuthError`, add an arm and extend
the 401 arm:

```rust
            Self::EntraUnavailable => {
                headers.insert(RETRY_AFTER, HeaderValue::from_static("5"));
            }
            Self::Unauthorized
            | Self::TokenExpired
            | Self::InvalidCredentials
            | Self::EntraTokenInvalid
            | Self::SessionExpired => {
                headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            }
```

and add to its unit test `errors_answer_with_their_status_code_and_headers`:

```rust
        let response = AuthError::EntraUnavailable.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "5");
        let response = AuthError::SessionExpired.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
```

- [ ] **Step 5: Password reads that cope with NULL.** In `crates\push-auth\src\service.rs`:

In `sign_in`, replace
`let stored: Option<String> = account.as_ref().map(|row| row.get(1));` with

```rust
        // An account without a password (Entra ID) has no address either, so the email
        // never finds it; NULL is read safely all the same.
        let stored: Option<String> = account
            .as_ref()
            .and_then(|row| row.get::<Option<String>, _>(1));
```

In `reset_password`, replace

```rust
        let current: String = account.get(0);
        if claims.pw.as_deref() != Some(password_stamp(&current).as_str()) {
            return Err(AuthError::LinkInvalid);
        }
```

with

```rust
        let current: Option<String> = account.get(0);
        let matches = match (&claims.pw, &current) {
            (Some(stamp), Some(current)) => *stamp == password_stamp(current),
            _ => false,
        };
        if !matches {
            return Err(AuthError::LinkInvalid);
        }
```

In `crates\push-auth\src\devices.rs`, `register_device`, replace

```rust
        let password_hash: String = account.get(0);
        if password_stamp(&password_hash) != stamp {
            return Err(AuthError::Unauthorized);
        }
```

with

```rust
        let password_hash: Option<String> = account.get(0);
        if !password_hash.is_some_and(|hash| password_stamp(&hash) == stamp) {
            return Err(AuthError::Unauthorized);
        }
```

(Task 8 replaces this block with the binding check.)

- [ ] **Step 6: The own profile's email is optional.** In `crates\push-auth\src\profiles.rs`
change `pub email: String,` in `OwnProfile` to

```rust
    /// The account's address; for an Entra ID account the latest address its token
    /// carried (informational), if any.
    pub email: Option<String>,
```

and `own_profile`'s query to

```rust
        let row = sqlx::query(
            "SELECT account.handle::text, account.display_name, \
             coalesce(account.email::text, identity.email) \
             FROM auth.user_account AS account \
             LEFT JOIN auth.external_identity AS identity ON identity.user_id = account.user_id \
             WHERE account.user_id = $1",
        )
```

(`own_profile_json` in `api.rs` needs no change: `None` becomes `null`.) In
`tests\devices.rs`, `profiles_show_handles_and_only_the_owner_sees_the_email`, change
`assert_eq!(own.email, "alice@example.com");` to
`assert_eq!(own.email.as_deref(), Some("alice@example.com"));`.

- [ ] **Step 7: Run.**

Run: `cargo test -p push-auth`
Expected: every test passes, the new one included.

- [ ] **Step 8: Commit.**

```powershell
git add -A
git commit -m "feat(auth): cope with accounts without a password; prepare identity-bound tokens"
```

### Task 8: Entra ID accounts, identity-bound registration and the session's maximum age

**Files:**
- Create: `crates\push-auth\src\external.rs`, `crates\push-auth\src\tests\mod.rs`, `crates\push-auth\src\tests\entra_accounts.rs`
- Modify: `crates\push-auth\src\service.rs`, `src\devices.rs`, `src\lib.rs`

- [ ] **Step 1: Sign-in options on the service.** In `crates\push-auth\src\service.rs`:

Add `use crate::entra::EntraSignIn;` to the imports. After `RegistrationToken` add:

```rust
/// Which ways to sign in the service offers (spec 2026-10-19 D9).
#[derive(Debug, Clone)]
pub struct SignIn {
    /// Email and password: signup, links, login.
    pub password: bool,
    /// Microsoft Entra ID, with its token checker.
    pub entra: Option<EntraSignIn>,
}

impl Default for SignIn {
    /// Email and password only: today's behaviour.
    fn default() -> Self {
        Self {
            password: true,
            entra: None,
        }
    }
}
```

Add a field `sign_in: SignIn,` at the end of `pub struct Auth`, `sign_in: SignIn::default(),`
at the end of the literal in `Auth::new`, and these methods after `pool()`:

```rust
    /// The service offering `sign_in` instead of email and password only.
    pub fn with_sign_in(mut self, sign_in: SignIn) -> Self {
        self.sign_in = sign_in;
        self
    }

    /// Whether email and password sign-in is offered.
    pub fn password_enabled(&self) -> bool {
        self.sign_in.password
    }

    /// The Entra ID token checker, when Entra ID sign-in is offered.
    pub fn entra(&self) -> Option<&EntraSignIn> {
        self.sign_in.entra.as_ref()
    }
```

- [ ] **Step 2: The test harness.** Create `crates\push-auth\src\tests\mod.rs`:

```rust
//! Entra ID sign-in against a real PostgreSQL (Docker must be running) and the stub
//! issuer: the service (`entra_accounts`), the HTTP API (`entra_api`) and the whole
//! service on sockets (`entra_server`). Here rather than under `tests\`: the stub is behind
//! the crate's `testing` feature, which the crate's own integration tests cannot enable.

mod entra_accounts;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::HeaderName;
use serde_json::Value;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

use crate::api::{Api, router};
use crate::db::migrate_database;
use crate::directory::NoDirectory;
use crate::entra::testing::StubOpenId;
use crate::entra::{EntraSignIn, KeyTiming};
use crate::limits::{Limits, MemoryCounter};
use crate::links::LinkSigner;
use crate::password::Passwords;
use crate::service::{Auth, Settings, SignIn};
use crate::tokens::AccessTokens;

pub(crate) const AUDIENCE: &str = "ws://push.local/v1/socket";
/// The link key of every world.
pub(crate) const LINK_KEY: [u8; 32] = [3; 32];

/// Which ways to sign in a world offers.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Offer {
    Password,
    Entra,
    Both,
}

pub(crate) struct World {
    _container: ContainerAsync<Postgres>,
    pub pool: PgPool,
    pub auth: Auth,
    pub router: Router,
    pub stub: StubOpenId,
}

/// Key timings without waits, so tests see every fetch at once.
pub(crate) fn quick() -> KeyTiming {
    KeyTiming {
        unknown_kid_cooldown: Duration::ZERO,
        retry_after_failure: Duration::ZERO,
        fetch_timeout: Duration::from_secs(2),
        ..KeyTiming::default()
    }
}

/// A migrated database, the stub issuer, the service offering `offer` and its router
/// (in-memory rate limits, the client IP in `x-real-ip`).
pub(crate) async fn world(offer: Offer) -> World {
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
    let stub = StubOpenId::start().await;
    let entra = EntraSignIn::with_timing(stub.settings(), quick()).unwrap();
    let sign_in = match offer {
        Offer::Password => SignIn::default(),
        Offer::Entra => SignIn {
            password: false,
            entra: Some(entra),
        },
        Offer::Both => SignIn {
            password: true,
            entra: Some(entra),
        },
    };
    let auth = Auth::new(
        pool.clone(),
        Settings {
            public_url: "http://push.local".to_owned(),
            passwords: Passwords::new(64, 1, 1).unwrap(),
            links: LinkSigner::new(LINK_KEY.to_vec()).unwrap(),
            tokens: AccessTokens::from_seed(&[5; 32], AUDIENCE).unwrap(),
            hash_concurrency: 4,
            hash_queue: 64,
            hash_wait: Duration::from_secs(5),
        },
    )
    .with_sign_in(sign_in);
    let router = router(Api {
        auth: auth.clone(),
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
        directory: Arc::new(NoDirectory),
    });
    World {
        _container: container,
        pool,
        auth,
        router,
        stub,
    }
}

impl World {
    /// The outbox, oldest first: `(kind, payload)`.
    pub async fn outbox(&self) -> Vec<(String, Value)> {
        sqlx::query_as("SELECT kind, payload FROM auth.outbox ORDER BY created_at, event_id")
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }

    /// How many accounts exist.
    pub async fn accounts(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM auth.user_account")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}
```

In `crates\push-auth\src\lib.rs` add `mod external;` after `mod error;`, and at the end:

```rust
#[cfg(test)]
mod tests;
```

Extend the `pub use service::{…}` line with `SignIn`, and add
`pub use external::EntraRegistration;`.

- [ ] **Step 3: Write the failing tests.** Create
`crates\push-auth\src\tests\entra_accounts.rs`:

```rust
//! Entra ID accounts in the service (spec AC4, AC5): found by `(tid, oid)`, created once,
//! never linked by address; registration tokens bound to the identity; the session's
//! maximum age.

use uuid::Uuid;

use super::{LINK_KEY, Offer, World, world};
use crate::devices::{Caller, Tokens};
use crate::entra::EntraIdentity;
use crate::error::AuthError;
use crate::links::{LinkClaims, LinkSigner, Purpose, password_stamp};
use crate::service::{SignIn, Signup, unix_now};

const TENANT: Uuid = Uuid::from_u128(0x33333333_0000_4000_8000_000000000003);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x44444444_0000_4000_8000_000000000004);

fn person(object: u128, email: &str) -> EntraIdentity {
    EntraIdentity {
        tenant_id: TENANT,
        object_id: Uuid::from_u128(object),
        name: Some("Erin Example".to_owned()),
        email: Some(email.to_owned()),
    }
}

async fn register(world: &World, identity: &EntraIdentity) -> Tokens {
    let signed_in = world.auth.sign_in_entra(identity, None).await.unwrap();
    world
        .auth
        .register_device(&signed_in.registration.token, Uuid::now_v7(), "Laptop", "web")
        .await
        .unwrap()
}

#[tokio::test]
async fn the_first_sign_in_creates_an_active_account_without_email_or_password() {
    let world = world(Offer::Entra).await;
    let signed_in = world
        .auth
        .sign_in_entra(&person(1, "erin@contoso.test"), None)
        .await
        .unwrap();
    assert_eq!(signed_in.registration.expires_in_s, 300);
    let (email, password, active, handle, name): (
        Option<String>,
        Option<String>,
        bool,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT email::text, password_hash, email_verified_at IS NOT NULL, handle::text, \
         display_name FROM auth.user_account WHERE user_id = $1",
    )
    .bind(signed_in.user_id)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert_eq!((email, password, active), (None, None, true));
    assert!(
        handle.len() == 11 && handle.starts_with('u'),
        "a generated handle: {handle}"
    );
    assert_eq!(name, "Erin Example");
    let kinds: Vec<String> = world.outbox().await.into_iter().map(|(kind, _)| kind).collect();
    assert_eq!(kinds, ["user.verified"]);

    let tokens = world
        .auth
        .register_device(&signed_in.registration.token, Uuid::now_v7(), "Laptop", "web")
        .await
        .unwrap();
    assert_eq!(tokens.user_id, signed_in.user_id);
    let own = world
        .auth
        .own_profile(Caller {
            user_id: tokens.user_id,
            device_id: tokens.device_id,
        })
        .await
        .unwrap();
    assert_eq!(own.email.as_deref(), Some("erin@contoso.test"));
    assert_eq!(
        world.auth.lookup(&own.handle).await.unwrap().user_id,
        tokens.user_id,
        "found by handle like any user"
    );
}

#[tokio::test]
async fn the_same_identity_is_always_the_same_user_and_addresses_never_link() {
    let world = world(Offer::Both).await;
    let first = world
        .auth
        .sign_in_entra(&person(1, "erin@contoso.test"), None)
        .await
        .unwrap()
        .user_id;
    // A new UPN, name and address: the same user, whose display name stays.
    let renamed = EntraIdentity {
        name: Some("Erin Married".to_owned()),
        email: Some("erin.married@contoso.test".to_owned()),
        ..person(1, "")
    };
    assert_eq!(
        world.auth.sign_in_entra(&renamed, None).await.unwrap().user_id,
        first
    );
    let (name, email): (String, Option<String>) = sqlx::query_as(
        "SELECT account.display_name, identity.email FROM auth.user_account AS account \
         JOIN auth.external_identity AS identity USING (user_id) WHERE account.user_id = $1",
    )
    .bind(first)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert_eq!(
        (name.as_str(), email.as_deref()),
        ("Erin Example", Some("erin.married@contoso.test"))
    );
    // Another Entra ID user with the first one's address: another account.
    let namesake = world
        .auth
        .sign_in_entra(&person(2, "erin@contoso.test"), None)
        .await
        .unwrap()
        .user_id;
    assert_ne!(namesake, first);
    // The same object id in another allowed tenant: another person.
    let elsewhere = EntraIdentity {
        tenant_id: OTHER_TENANT,
        ..person(1, "erin@contoso.test")
    };
    let foreigner = world.auth.sign_in_entra(&elsewhere, None).await.unwrap().user_id;
    assert!(![first, namesake].contains(&foreigner));
    // An email/password account with that address: its own account, never linked.
    world
        .auth
        .signup(Signup {
            email: "erin@contoso.test".to_owned(),
            password: "correct horse".to_owned(),
            handle: "erin".to_owned(),
            display_name: "Erin".to_owned(),
        })
        .await
        .unwrap();
    let password_user: Uuid =
        sqlx::query_scalar("SELECT user_id FROM auth.user_account WHERE email = 'erin@contoso.test'")
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert!(![first, namesake, foreigner].contains(&password_user));
    let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.external_identity")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(identities, 3);
    assert_eq!(
        world
            .auth
            .sign_in_entra(&person(1, "x@contoso.test"), None)
            .await
            .unwrap()
            .user_id,
        first
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requested_handle_is_used_once_and_concurrent_first_sign_ins_make_one_account() {
    let world = world(Offer::Entra).await;
    let erin = world
        .auth
        .sign_in_entra(&person(1, "e@contoso.test"), Some("@Erin"))
        .await
        .unwrap();
    let handle: String =
        sqlx::query_scalar("SELECT handle::text FROM auth.user_account WHERE user_id = $1")
            .bind(erin.user_id)
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!(handle, "erin");
    assert_eq!(
        world
            .auth
            .sign_in_entra(&person(2, "f@contoso.test"), Some("erin"))
            .await
            .unwrap_err(),
        AuthError::HandleTaken
    );
    assert!(matches!(
        world
            .auth
            .sign_in_entra(&person(2, "f@contoso.test"), Some("x"))
            .await,
        Err(AuthError::Invalid(_))
    ));
    // Later sign-ins ignore the handle, even an invalid one.
    assert_eq!(
        world
            .auth
            .sign_in_entra(&person(1, "e@contoso.test"), Some("x"))
            .await
            .unwrap()
            .user_id,
        erin.user_id
    );

    let gus = person(3, "g@contoso.test");
    let attempts: Vec<_> = (0..8)
        .map(|_| {
            let (auth, gus) = (world.auth.clone(), gus.clone());
            tokio::spawn(async move { auth.sign_in_entra(&gus, None).await.unwrap().user_id })
        })
        .collect();
    let mut users = Vec::new();
    for attempt in attempts {
        users.push(attempt.await.unwrap());
    }
    users.sort();
    users.dedup();
    assert_eq!(users.len(), 1, "one account for concurrent first sign-ins");
    let accounts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM auth.external_identity WHERE subject = $1")
            .bind(gus.object_id.to_string())
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!(accounts, 1);
}

#[tokio::test]
async fn an_entra_device_session_ends_at_its_maximum_age_and_a_new_sign_in_renews_it() {
    let world = world(Offer::Entra).await;
    let erin = person(1, "e@contoso.test");
    let tokens = register(&world, &erin).await;
    let session: Option<i64> =
        sqlx::query_scalar("SELECT session_expires_at FROM auth.device WHERE device_id = $1")
            .bind(tokens.device_id)
            .fetch_one(&world.pool)
            .await
            .unwrap();
    let left = session.expect("an Entra device has a session end") - unix_now();
    assert!((24 * 3600 - 10..=24 * 3600).contains(&left), "{left}");
    assert!(tokens.refresh_expires_in_s <= 24 * 3600);
    let refreshed = world.auth.refresh(&tokens.refresh_token).await.unwrap();

    sqlx::query("UPDATE auth.device SET session_expires_at = $2 WHERE device_id = $1")
        .bind(tokens.device_id)
        .bind(unix_now() - 1)
        .execute(&world.pool)
        .await
        .unwrap();
    assert_eq!(
        world.auth.refresh(&refreshed.refresh_token).await,
        Err(AuthError::SessionExpired)
    );
    // The device is still the user's: an Entra ID sign-in for it renews the session.
    let renewed = world
        .auth
        .sign_in_entra_device(&erin, None, tokens.device_id)
        .await
        .unwrap();
    assert_eq!(
        (renewed.user_id, renewed.device_id),
        (tokens.user_id, tokens.device_id)
    );
    assert!(world.auth.refresh(&renewed.refresh_token).await.is_ok());
    // The new sign-in ended the earlier refresh tokens.
    assert_eq!(
        world.auth.refresh(&refreshed.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    // Another user's device is not found.
    let other = register(&world, &person(2, "f@contoso.test")).await;
    assert_eq!(
        world
            .auth
            .sign_in_entra_device(&erin, None, other.device_id)
            .await,
        Err(AuthError::DeviceNotFound)
    );
}

#[tokio::test]
async fn an_entra_registration_token_registers_only_for_its_identity_and_provider() {
    let world = world(Offer::Both).await;
    let signed_in = world
        .auth
        .sign_in_entra(&person(1, "e@contoso.test"), None)
        .await
        .unwrap();
    let identity: Uuid = sqlx::query_scalar("SELECT identity_id FROM auth.external_identity")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    let signer = LinkSigner::new(LINK_KEY.to_vec()).unwrap();
    let token = |pw: Option<String>, xid: Option<Uuid>, sx: Option<i64>| {
        signer.sign(&LinkClaims {
            sub: signed_in.user_id,
            purpose: Purpose::Register,
            exp: i64::MAX,
            email: None,
            pw,
            xid,
            sx,
        })
    };
    let later = unix_now() + 3600;
    for refused in [
        token(None, None, Some(later)),
        token(None, Some(identity), None),
        token(None, Some(Uuid::now_v7()), Some(later)),
        token(Some(password_stamp("")), Some(identity), Some(later)),
        token(Some(password_stamp("")), None, None),
    ] {
        assert_eq!(
            world
                .auth
                .register_device(&refused, Uuid::now_v7(), "Laptop", "web")
                .await,
            Err(AuthError::Unauthorized)
        );
    }
    assert!(
        world
            .auth
            .register_device(&token(None, Some(identity), Some(later)), Uuid::now_v7(), "Laptop", "web")
            .await
            .is_ok()
    );
    // The same token on a service that no longer offers Entra ID sign-in.
    let password_only = world.auth.clone().with_sign_in(SignIn::default());
    assert_eq!(
        password_only
            .register_device(&signed_in.registration.token, Uuid::now_v7(), "Laptop", "web")
            .await,
        Err(AuthError::Unauthorized)
    );
}
```

- [ ] **Step 4: Run them to see them fail.**

Run: `cargo test -p push-auth --lib tests::entra_accounts`
Expected: compile errors (`sign_in_entra`, `sign_in_entra_device`, `EntraRegistration` not
found).

- [ ] **Step 5: Devices: binding, session, shared helpers.** In
`crates\push-auth\src\devices.rs`:

Add `use crate::links::LinkClaims;` (extend the existing `use crate::links::{Purpose,
password_stamp};` to `use crate::links::{LinkClaims, Purpose, password_stamp};`).

Make `issue` `pub(crate)` and give it the session end:

```rust
    pub(crate) async fn issue(
        &self,
        connection: &mut PgConnection,
        user_id: Uuid,
        device_id: Uuid,
        session_expires_at: Option<i64>,
    ) -> Result<Tokens, AuthError> {
        let refresh_token = new_refresh_token();
        let full = REFRESH_TOKEN_TTL.as_secs() as i64;
        // A refresh token never outlives the device's session (spec D8).
        let refresh_seconds =
            session_expires_at.map_or(full, |end| (end - unix_now()).clamp(0, full));
        sqlx::query(
            "INSERT INTO auth.refresh_token (token_hash, device_id, expires_at) \
             VALUES ($1, $2, now() + make_interval(secs => $3))",
        )
        .bind(refresh_hash(&refresh_token).as_slice())
        .bind(device_id)
        .bind(refresh_seconds as f64)
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
            refresh_expires_in_s: refresh_seconds,
        })
    }
```

Replace `register_device`'s body from `let claims = …` to the account check with:

```rust
        let claims = self
            .links
            .verify(registration_token, Purpose::Register, unix_now())
            .map_err(|_| AuthError::Unauthorized)?;
        let name = rules::device_name(name)?;
        let platform = Platform::parse(platform)?;
        let mut tx = self.pool.begin().await?;
        let account = sqlx::query(
            "SELECT password_hash, email_verified_at IS NOT NULL FROM auth.user_account \
             WHERE user_id = $1 FOR UPDATE",
        )
        .bind(claims.sub)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(account) = account else {
            return Err(AuthError::Unauthorized);
        };
        let session = self
            .registration_binding(&mut tx, &claims, account.get(0))
            .await?;
        if !account.get::<bool, _>(1) {
            return Err(AuthError::EmailNotVerified);
        }
```

In the retry branch (`if let Some(earlier) = earlier {`), before
`revoke_refresh_tokens(&mut tx, device_id).await?;` add
`set_session(&mut tx, device_id, session).await?;`, and change its `self.issue(…)` call to
`self.issue(&mut tx, claims.sub, device_id, session).await?`.

Change the new device's insert and issue to:

```rust
        sqlx::query(
            "INSERT INTO auth.device \
             (device_id, user_id, name, platform, registration_id, session_expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(device_id)
        .bind(claims.sub)
        .bind(&name)
        .bind(platform.as_str())
        .bind(registration_id)
        .bind(session)
        .execute(&mut *tx)
        .await?;
        let tokens = self.issue(&mut tx, claims.sub, device_id, session).await?;
```

Update the doc comment's last paragraph of `register_device` to: "The token binds the
password hash of the sign-in that made it (after a password reset it registers nothing), or
the Entra ID identity that signed in, whose session end the device takes (spec D7, D8)."

In `login_device` change the call to `self.issue(&mut tx, user_id, device_id, None).await?`.

In `refresh`, replace the device query and its checks with:

```rust
        let device = sqlx::query(
            "SELECT user_id, revoked_at IS NULL, session_expires_at FROM auth.device \
             WHERE device_id = $1 FOR UPDATE",
        )
        .bind(device_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(device) = device else {
            return Err(AuthError::Unauthorized);
        };
        let user_id: Uuid = device.get(0);
        if !device.get::<bool, _>(1) {
            return Err(AuthError::Unauthorized);
        }
        // An Entra-backed device past its maximum age signs in with Entra ID again; the
        // device and its tokens stay as they are (spec D8).
        let session: Option<i64> = device.get(2);
        if session.is_some_and(|end| end <= unix_now()) {
            return Err(AuthError::SessionExpired);
        }
```

and change both `self.issue(&mut tx, user_id, device_id)` calls in `refresh` to
`self.issue(&mut tx, user_id, device_id, session)`.

Add to the `impl Auth` block:

```rust
    /// What a registration token is bound to (spec D7): the password hash of its sign-in
    /// (`None`: no session end), or the Entra ID identity that signed in (`Some`: the
    /// session end it carries). A token bound to neither or to both, to a changed password,
    /// to another account's identity, or to a way of signing in the service no longer
    /// offers registers nothing.
    async fn registration_binding(
        &self,
        connection: &mut PgConnection,
        claims: &LinkClaims,
        password_hash: Option<String>,
    ) -> Result<Option<i64>, AuthError> {
        match (&claims.pw, claims.xid, claims.sx) {
            (Some(stamp), None, None) if self.password_enabled() => {
                if password_hash.is_some_and(|hash| password_stamp(&hash) == *stamp) {
                    Ok(None)
                } else {
                    Err(AuthError::Unauthorized)
                }
            }
            (None, Some(identity), Some(session)) if self.entra().is_some() => {
                let bound = sqlx::query(
                    "SELECT 1 FROM auth.external_identity WHERE identity_id = $1 AND user_id = $2",
                )
                .bind(identity)
                .bind(claims.sub)
                .fetch_optional(&mut *connection)
                .await?;
                if bound.is_some() {
                    Ok(Some(session))
                } else {
                    Err(AuthError::Unauthorized)
                }
            }
            _ => Err(AuthError::Unauthorized),
        }
    }
```

and below `revoke_refresh_tokens` (which becomes `pub(crate) async fn`):

```rust
/// Starts a new session for an Entra-backed device; nothing for a password device.
pub(crate) async fn set_session(
    connection: &mut PgConnection,
    device_id: Uuid,
    session: Option<i64>,
) -> Result<(), AuthError> {
    if let Some(session) = session {
        sqlx::query("UPDATE auth.device SET session_expires_at = $2 WHERE device_id = $1")
            .bind(device_id)
            .bind(session)
            .execute(connection)
            .await?;
    }
    Ok(())
}
```

- [ ] **Step 6: The accounts.** Create `crates\push-auth\src\external.rs`:

```rust
//! Entra ID accounts (spec 2026-10-19 D5 to D8): found by `(tenant, object id)`, created
//! on the first sign-in with `user.verified` in the same transaction, never linked to an
//! email/password account; registration tokens bound to the identity; device sign-ins that
//! renew the session.

use rand::RngCore;
use sqlx::Row;
use uuid::Uuid;

use crate::devices::{Tokens, revoke_refresh_tokens, set_session};
use crate::entra::EntraIdentity;
use crate::error::AuthError;
use crate::links::{LinkClaims, Purpose};
use crate::outbox::{self, Event};
use crate::rules;
use crate::service::{Auth, REGISTRATION_TOKEN_SECONDS, RegistrationToken, unix_now};

/// What an Entra ID sign-in without a device gives: the account and a registration token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntraRegistration {
    pub user_id: Uuid,
    pub registration: RegistrationToken,
}

/// An Entra ID account: the user and the identity row that names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EntraAccount {
    user_id: Uuid,
    identity_id: Uuid,
}

/// Attempts at a free generated handle.
const HANDLE_ATTEMPTS: usize = 5;

impl Auth {
    /// `POST /v1/auth/entra {token}`: the account of `identity`, created on its first
    /// sign-in, and a registration token bound to the identity and to the end of the
    /// session it starts. `handle` names a new account (it is ignored afterwards); without
    /// it a random `u…` handle is chosen.
    pub async fn sign_in_entra(
        &self,
        identity: &EntraIdentity,
        handle: Option<&str>,
    ) -> Result<EntraRegistration, AuthError> {
        let max_session = self.max_session()?;
        let account = self.entra_account(identity, handle).await?;
        let now = unix_now();
        let token = self.links.sign(&LinkClaims {
            sub: account.user_id,
            purpose: Purpose::Register,
            exp: now + REGISTRATION_TOKEN_SECONDS,
            email: None,
            pw: None,
            xid: Some(account.identity_id),
            sx: Some(now + max_session),
        });
        Ok(EntraRegistration {
            user_id: account.user_id,
            registration: RegistrationToken {
                token,
                expires_in_s: REGISTRATION_TOKEN_SECONDS,
            },
        })
    }

    /// `POST /v1/auth/entra {token, device_id}` for an active device of the account: a new
    /// session for it and new tokens; its earlier refresh tokens are revoked.
    pub async fn sign_in_entra_device(
        &self,
        identity: &EntraIdentity,
        handle: Option<&str>,
        device_id: Uuid,
    ) -> Result<Tokens, AuthError> {
        let max_session = self.max_session()?;
        let account = self.entra_account(identity, handle).await?;
        let session = Some(unix_now() + max_session);
        let mut tx = self.pool.begin().await?;
        // Locks the device row first, like every change to a device's tokens.
        let active = sqlx::query(
            "SELECT 1 FROM auth.device WHERE device_id = $1 AND user_id = $2 \
             AND revoked_at IS NULL FOR UPDATE",
        )
        .bind(device_id)
        .bind(account.user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if active.is_none() {
            return Err(AuthError::DeviceNotFound);
        }
        set_session(&mut tx, device_id, session).await?;
        revoke_refresh_tokens(&mut tx, device_id).await?;
        let tokens = self.issue(&mut tx, account.user_id, device_id, session).await?;
        tx.commit().await?;
        Ok(tokens)
    }

    /// The configured maximum session age in seconds; `Unauthorized` when Entra ID
    /// sign-in is not offered.
    fn max_session(&self) -> Result<i64, AuthError> {
        self.entra()
            .map(|entra| entra.max_session().as_secs() as i64)
            .ok_or(AuthError::Unauthorized)
    }

    /// The account of `identity`, created if it has none. A concurrent first sign-in of
    /// the same identity may win the race once; then its account is the one.
    async fn entra_account(
        &self,
        identity: &EntraIdentity,
        handle: Option<&str>,
    ) -> Result<EntraAccount, AuthError> {
        for _ in 0..2 {
            if let Some(found) = self.find_entra(identity).await? {
                return Ok(found);
            }
            if let Some(created) = self.create_entra(identity, handle).await? {
                return Ok(created);
            }
        }
        Err(AuthError::Unavailable)
    }

    /// The identity's account, recording this sign-in and the address the token carried.
    async fn find_entra(&self, identity: &EntraIdentity) -> Result<Option<EntraAccount>, AuthError> {
        let row = sqlx::query(
            "UPDATE auth.external_identity SET last_sign_in_at = now(), email = $3 \
             WHERE provider = 'entra' AND tenant_id = $1 AND subject = $2 \
             RETURNING identity_id, user_id",
        )
        .bind(identity.tenant_id)
        .bind(identity.object_id.to_string())
        .bind(identity.email.as_deref())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| EntraAccount {
            identity_id: row.get(0),
            user_id: row.get(1),
        }))
    }

    /// A new account for `identity`; `None` if another sign-in created one meanwhile
    /// (this transaction is then rolled back, the account it made included).
    async fn create_entra(
        &self,
        identity: &EntraIdentity,
        handle: Option<&str>,
    ) -> Result<Option<EntraAccount>, AuthError> {
        let requested = handle.map(rules::handle).transpose()?;
        let user_id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        let mut created = false;
        for _ in 0..HANDLE_ATTEMPTS {
            let candidate = requested.clone().unwrap_or_else(generated_handle);
            let display_name = identity
                .name
                .as_deref()
                .and_then(|name| rules::display_name(name).ok())
                .unwrap_or_else(|| candidate.clone());
            // Active from the start: email_verified_at is the first sign-in (spec D5).
            let inserted = sqlx::query(
                "INSERT INTO auth.user_account \
                 (user_id, email, handle, display_name, password_hash, email_verified_at) \
                 VALUES ($1, NULL, $2, $3, NULL, now()) \
                 ON CONFLICT (handle) DO NOTHING RETURNING 1",
            )
            .bind(user_id)
            .bind(&candidate)
            .bind(&display_name)
            .fetch_optional(&mut *tx)
            .await?;
            if inserted.is_some() {
                created = true;
                break;
            }
            if requested.is_some() {
                return Err(AuthError::HandleTaken);
            }
        }
        if !created {
            return Err(AuthError::Unavailable);
        }
        let identity_id = Uuid::now_v7();
        let linked = sqlx::query(
            "INSERT INTO auth.external_identity \
             (identity_id, provider, tenant_id, subject, user_id, email) \
             VALUES ($1, 'entra', $2, $3, $4, $5) \
             ON CONFLICT (provider, tenant_id, subject) DO NOTHING RETURNING 1",
        )
        .bind(identity_id)
        .bind(identity.tenant_id)
        .bind(identity.object_id.to_string())
        .bind(user_id)
        .bind(identity.email.as_deref())
        .fetch_optional(&mut *tx)
        .await?;
        if linked.is_none() {
            tx.rollback().await?;
            return Ok(None);
        }
        outbox::write(&mut tx, &Event::UserVerified { user_id }).await?;
        tx.commit().await?;
        tracing::info!(
            target: "security",
            event = "auth.entra.provisioned",
            %user_id,
            tenant = %identity.tenant_id,
            "an account was created for an Entra ID user"
        );
        Ok(Some(EntraAccount {
            user_id,
            identity_id,
        }))
    }
}

/// `u` and 10 random lower-case letters and digits: a valid handle nobody chose.
fn generated_handle() -> String {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 10];
    rand::rng().fill_bytes(&mut bytes);
    let mut handle = String::from("u");
    handle.extend(
        bytes
            .iter()
            .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()])),
    );
    handle
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn generated_handles_are_valid_and_differ() {
        let (first, second) = (generated_handle(), generated_handle());
        assert_eq!(rules::handle(&first).as_deref(), Ok(first.as_str()));
        assert_eq!(first.len(), 11);
        assert_ne!(first, second);
    }
}
```

- [ ] **Step 7: Run.**

Run: `cargo test -p push-auth`
Expected: every test passes: the five new `tests::entra_accounts` tests, the handle unit
test, and every existing test (password registration, refresh and retry paths are
unchanged: their session is `None`).

- [ ] **Step 8: Commit.**

```powershell
git add -A
git commit -m "feat(auth): Entra ID accounts, identity-bound registration and a maximum session age"
```

### Task 9: The endpoint, the routes per provider, limits and security events

**Files:**
- Modify: `crates\push-auth\src\api.rs`, `src\limits.rs`, `src\audit.rs`, `src\tests\mod.rs`
- Create: `crates\push-auth\src\tests\entra_api.rs`

- [ ] **Step 1: Write the failing tests.** In `src\tests\mod.rs` add `mod entra_api;` after
`mod entra_accounts;`. Create `crates\push-auth\src\tests\entra_api.rs`:

```rust
//! The Entra ID sign-in API through the router (spec AC1 to AC6): the routes of each
//! offer, the endpoint, refusals, the session's end, key rotation and outages, rate limits
//! and the security log.

use std::sync::Mutex;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use super::{Offer, World, world};
use crate::entra::testing::{CLIENT_APP, KEY_A, KEY_B, TENANT, sign};
use crate::service::unix_now;

const USER: &str = "55555555-0000-4000-8000-000000000005";

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    json: Value,
}

/// One request from the client 198.51.100.7, as the ingress reports it.
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
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

async fn entra(world: &World, body: Value) -> Answer {
    call(world, Method::POST, "/v1/auth/entra", None, Some(body)).await
}

/// `object_id` signs in with Entra ID and registers a device: its tokens.
async fn with_device(world: &World, object_id: &str) -> Value {
    let signed_in = entra(world, json!({"token": world.stub.token(object_id)})).await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.json);
    let registration = signed_in.json["registration_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let device = call(
        world,
        Method::POST,
        "/v1/devices",
        Some(&registration),
        Some(json!({"name": "Laptop", "platform": "web", "registration_id": Uuid::now_v7()})),
    )
    .await;
    assert_eq!(device.status, StatusCode::CREATED, "{}", device.json);
    device.json
}

#[tokio::test]
async fn each_offer_routes_only_its_own_sign_in() {
    let entra_only = world(Offer::Entra).await;
    for (method, uri) in [
        (Method::POST, "/v1/auth/signup"),
        (Method::GET, "/v1/auth/verify?token=x"),
        (Method::POST, "/v1/auth/verify"),
        (Method::POST, "/v1/auth/verify/resend"),
        (Method::POST, "/v1/auth/login"),
        (Method::POST, "/v1/auth/password/forgot"),
        (Method::GET, "/v1/auth/password/reset?token=x"),
        (Method::POST, "/v1/auth/password/reset"),
    ] {
        let answer = call(&entra_only, method.clone(), uri, None, Some(json!({}))).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
    for (method, uri) in [
        (Method::POST, "/v1/auth/entra"),
        (Method::POST, "/v1/auth/refresh"),
        (Method::GET, "/v1/auth/jwks"),
        (Method::GET, "/v1/users/me"),
    ] {
        let answer = call(&entra_only, method.clone(), uri, None, Some(json!({}))).await;
        assert_ne!(answer.status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
    // The default: email and password only, no Entra ID endpoint (AC1).
    let password_only = world(Offer::Password).await;
    let token = password_only.stub.token(USER);
    assert_eq!(
        entra(&password_only, json!({"token": token})).await.status,
        StatusCode::NOT_FOUND
    );
    let login = call(&password_only, Method::POST, "/v1/auth/login", None, Some(json!({}))).await;
    assert_ne!(login.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_entra_user_signs_in_registers_and_refreshes_and_no_email_is_written() {
    let world = world(Offer::Entra).await;
    let signed_in = entra(
        &world,
        json!({"token": world.stub.token(USER), "handle": "erin"}),
    )
    .await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.json);
    assert_eq!(signed_in.json["expires_in_s"], 300);
    let registration = signed_in.json["registration_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let device = call(
        &world,
        Method::POST,
        "/v1/devices",
        Some(&registration),
        Some(json!({"name": "Laptop", "platform": "web", "registration_id": Uuid::now_v7()})),
    )
    .await;
    assert_eq!(device.status, StatusCode::CREATED, "{}", device.json);
    assert!(device.json["refresh_expires_in_s"].as_i64().unwrap() <= 24 * 3600);
    let refreshed = call(
        &world,
        Method::POST,
        "/v1/auth/refresh",
        None,
        Some(json!({"refresh_token": device.json["refresh_token"]})),
    )
    .await;
    assert_eq!(refreshed.status, StatusCode::OK);
    let me = call(
        &world,
        Method::GET,
        "/v1/users/me",
        refreshed.json["access_token"].as_str(),
        None,
    )
    .await;
    assert_eq!(me.json["handle"], "erin");
    assert_eq!(me.json["display_name"], "Test User");
    assert_eq!(me.json["email"], "test.user@contoso.test");
    let kinds: Vec<String> = world.outbox().await.into_iter().map(|(kind, _)| kind).collect();
    assert_eq!(kinds, ["user.verified", "device.registered"]);
}

#[tokio::test]
async fn refused_tokens_answer_one_code_and_create_nobody() {
    let world = world(Offer::Entra).await;
    let good = world.stub.user_claims(TENANT, USER);
    let mut id_token_audience = good.clone();
    id_token_audience["aud"] = json!(CLIENT_APP);
    let mut expired = good.clone();
    expired["exp"] = json!(unix_now() - 120);
    let mut app_only = good.clone();
    app_only.as_object_mut().unwrap().remove("scp");
    for token in [
        sign(KEY_A, &id_token_audience),
        sign(KEY_A, &expired),
        sign(KEY_B, &good),
        sign(KEY_A, &app_only),
        "garbage".to_owned(),
    ] {
        let answer = entra(&world, json!({"token": token})).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert_eq!(answer.json["error"]["code"], "entra_token_invalid");
        assert_eq!(answer.json["error"]["retryable"], false);
        assert_eq!(answer.headers["www-authenticate"], "Bearer");
    }
    assert_eq!(world.accounts().await, 0);
    let no_token = entra(&world, json!({"handle": "erin"})).await;
    assert_eq!(no_token.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_session_ends_at_its_maximum_age_and_an_entra_sign_in_restores_the_device() {
    let world = world(Offer::Entra).await;
    let tokens = with_device(&world, USER).await;
    let device_id = tokens["device_id"].as_str().unwrap().to_owned();
    sqlx::query("UPDATE auth.device SET session_expires_at = $1")
        .bind(unix_now() - 1)
        .execute(&world.pool)
        .await
        .unwrap();
    let ended = call(
        &world,
        Method::POST,
        "/v1/auth/refresh",
        None,
        Some(json!({"refresh_token": tokens["refresh_token"]})),
    )
    .await;
    assert_eq!(ended.status, StatusCode::UNAUTHORIZED);
    assert_eq!(ended.json["error"]["code"], "session_expired");

    let again = entra(
        &world,
        json!({"token": world.stub.token(USER), "device_id": device_id}),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.json);
    assert_eq!(again.json["device_id"], device_id.as_str());
    let refreshed = call(
        &world,
        Method::POST,
        "/v1/auth/refresh",
        None,
        Some(json!({"refresh_token": again.json["refresh_token"]})),
    )
    .await;
    assert_eq!(refreshed.status, StatusCode::OK);
    let stranger = entra(
        &world,
        json!({"token": world.stub.token(USER), "device_id": Uuid::now_v7()}),
    )
    .await;
    assert_eq!(stranger.status, StatusCode::NOT_FOUND);
    assert_eq!(stranger.json["error"]["code"], "device_not_found");
}

#[tokio::test]
async fn an_outage_fails_closed_and_a_rotated_key_works_without_a_restart() {
    let world = world(Offer::Entra).await;
    world.stub.set_failing(true);
    let down = entra(&world, json!({"token": world.stub.token(USER)})).await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(down.json["error"]["code"], "entra_unavailable");
    assert_eq!(down.json["error"]["retryable"], true);
    assert_eq!(down.headers["retry-after"], "5");
    world.stub.set_failing(false);
    let up = entra(&world, json!({"token": world.stub.token(USER)})).await;
    assert_eq!(up.status, StatusCode::OK);

    world.stub.publish(&[KEY_B]);
    let rotated = sign(KEY_B, &world.stub.user_claims(TENANT, USER));
    assert_eq!(
        entra(&world, json!({"token": rotated})).await.status,
        StatusCode::OK
    );
    assert_eq!(world.accounts().await, 1);
}

#[tokio::test]
async fn sign_ins_are_rate_limited_per_ip_before_any_token_is_checked() {
    let world = world(Offer::Entra).await;
    for _ in 0..60 {
        let refused = entra(&world, json!({"token": "garbage"})).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    }
    let limited = entra(&world, json!({"token": world.stub.token(USER)})).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers.contains_key("retry-after"));
    assert_eq!(world.stub.metadata_fetches(), 0, "nothing was fetched for them");
}

struct Capture(&'static Mutex<Vec<u8>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The subscriber is this thread's (the test runs on one thread), so other tests'
/// lines never mix in.
#[tokio::test]
async fn entra_sign_ins_are_logged_without_tokens_object_ids_names_or_addresses() {
    static LINES: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(|| Capture(&LINES))
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let world = world(Offer::Entra).await;
    let accepted = world.stub.token(USER);
    assert_eq!(
        entra(&world, json!({"token": accepted})).await.status,
        StatusCode::OK
    );
    let mut claims = world.stub.user_claims(TENANT, USER);
    claims["aud"] = json!(CLIENT_APP);
    let refused = sign(KEY_A, &claims);
    assert_eq!(
        entra(&world, json!({"token": refused})).await.status,
        StatusCode::UNAUTHORIZED
    );
    let user: Uuid = sqlx::query_scalar("SELECT user_id FROM auth.external_identity")
        .fetch_one(&world.pool)
        .await
        .unwrap();

    let text = String::from_utf8(LINES.lock().unwrap().clone()).unwrap();
    let events: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["target"] == "security")
        .map(|line| line["fields"].clone())
        .collect();
    let named = |name: &str| {
        events
            .iter()
            .find(|event| event["event"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} in {events:?}"))
    };
    assert_eq!(named("auth.entra.provisioned")["tenant"], TENANT);
    assert_eq!(named("auth.entra.provisioned")["user_id"], user.to_string());
    assert_eq!(named("auth.entra.success")["user_id"], user.to_string());
    assert_eq!(named("auth.entra.success")["ip"], "198.51.100.7");
    assert_eq!(named("auth.entra.rejected")["reason"], "audience");
    assert_eq!(named("auth.entra.failure")["code"], "entra_token_invalid");
    for secret in [
        accepted.as_str(),
        refused.as_str(),
        USER,
        "Test User",
        "test.user@contoso.test",
    ] {
        assert!(!text.contains(secret), "the log holds {secret}");
    }
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p push-auth --lib tests::entra_api`
Expected: they fail: `/v1/auth/entra` answers 404 everywhere and the password routes exist in
every offer.

- [ ] **Step 3: Limits.** In `crates\push-auth\src\limits.rs`, extend the module doc's first
sentence with "and on Entra ID sign-in", and add to `enum Action` (after `HandleLookup`):

```rust
    /// `POST /v1/auth/entra`, counted per IP before the token is checked.
    EntraSignIn,
    /// `POST /v1/auth/entra`, counted per Entra ID identity once the token is accepted.
    EntraAccount,
```

In `name()`: `Self::EntraSignIn => "entra",` and `Self::EntraAccount => "entra_account",`.
In `limits()` add the constants and arms:

```rust
        // Like login per IP; per identity once the token is known to be good.
        const ENTRA: &[(Scope, Limit)] = &[(Scope::Ip, per(60, QUARTER))];
        const ENTRA_ACCOUNT: &[(Scope, Limit)] = &[(Scope::Account, per(20, QUARTER))];
```

```rust
            Self::EntraSignIn => ENTRA,
            Self::EntraAccount => ENTRA_ACCOUNT,
```

- [ ] **Step 4: The router and the handler.** In `crates\push-auth\src\api.rs`:

Add `use crate::entra::EntraError;` to the imports and update the module doc's first line to
"The public HTTP API (spec §2, §5; Entra ID sign-in: spec 2026-10-19 §4): …".

Replace the beginning of `router` up to `.route("/v1/auth/jwks", get(jwks))` with:

```rust
/// The router of the public API. Without email and password sign-in its routes do not
/// exist (404), and without Entra ID sign-in neither does `/v1/auth/entra` (spec D9).
pub fn router(api: Api) -> Router {
    let mut routes: Router<Api> = Router::new();
    if api.auth.password_enabled() {
        routes = routes
            .route("/v1/auth/signup", post(signup))
            .route("/v1/auth/verify", get(verify_page).post(verify))
            .route("/v1/auth/verify/resend", post(resend))
            .route("/v1/auth/login", post(login))
            .route("/v1/auth/password/forgot", post(forgot))
            .route("/v1/auth/password/reset", get(reset_page).post(reset));
    }
    if api.auth.entra().is_some() {
        routes = routes.route("/v1/auth/entra", post(entra));
    }
    routes
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/jwks", get(jwks))
```

(the rest of the chain, from `.route("/v1/devices", …)` to `.with_state(api)`, is
unchanged).

Add after the `login` handler:

```rust
#[derive(Deserialize)]
struct EntraBody {
    /// The Entra ID access token for this API, as MSAL returned it.
    token: String,
    /// The handle of a new account; ignored once the account exists.
    #[serde(default)]
    handle: Option<String>,
    #[serde(default)]
    device_id: Option<Uuid>,
}

/// `POST /v1/auth/entra {token, handle?, device_id?}` (spec D10): without `device_id` a
/// registration token, as `POST /v1/auth/login` answers; with it, new tokens for that
/// active device and a new session. Every refused token is `entra_token_invalid`; the
/// reason goes to the security log only.
async fn entra(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    JsonBody(body): JsonBody<EntraBody>,
) -> Result<Response, AuthError> {
    api.limits.check(Action::EntraSignIn, ip, None).await?;
    let checker = api.auth.entra().ok_or(AuthError::Unauthorized)?;
    let identity = checker
        .verify(&body.token)
        .await
        .map_err(|error| match error {
            EntraError::Rejected(_) => AuthError::EntraTokenInvalid,
            EntraError::Unavailable(_) => AuthError::EntraUnavailable,
        })?;
    let account = format!("entra:{}:{}", identity.tenant_id, identity.object_id);
    api.limits
        .check(Action::EntraAccount, None, Some(&account))
        .await?;
    let handle = body.handle.as_deref();
    Ok(match body.device_id {
        None => {
            let signed_in = api.auth.sign_in_entra(&identity, handle).await?;
            let mut response = Json(json!({
                "registration_token": signed_in.registration.token,
                "expires_in_s": signed_in.registration.expires_in_s,
            }))
            .into_response();
            response.extensions_mut().insert(Subject {
                user: Some(signed_in.user_id),
                device: None,
            });
            response
        }
        Some(device_id) => {
            let tokens = api
                .auth
                .sign_in_entra_device(&identity, handle, device_id)
                .await?;
            tokens_response(StatusCode::OK, &tokens)
        }
    })
}
```

- [ ] **Step 5: Security events.** In `crates\push-auth\src\audit.rs`, `event()`, add before
the `("POST", "/v1/auth/refresh") if ok` arm:

```rust
        ("POST", "/v1/auth/entra") if ok => "auth.entra.success",
        ("POST", "/v1/auth/entra") if status != StatusCode::BAD_REQUEST => "auth.entra.failure",
```

and add cases to `answers_are_classified_by_route_status_and_code`:

```rust
            (
                &post,
                "/v1/auth/entra",
                200,
                None,
                Some("auth.entra.success"),
            ),
            (
                &post,
                "/v1/auth/entra",
                401,
                Some("entra_token_invalid"),
                Some("auth.entra.failure"),
            ),
            (&post, "/v1/auth/entra", 400, Some("invalid"), None),
            (&post, "/v1/auth/entra", 503, Some("entra_unavailable"), None),
            (
                &post,
                "/v1/auth/refresh",
                401,
                Some("session_expired"),
                Some("auth.refresh.refused"),
            ),
```

Extend the module doc's list: "sign-ins (email and password, or Entra ID), refused tokens, …".

- [ ] **Step 6: Run.**

Run: `cargo test -p push-auth`
Expected: every test passes, the seven `tests::entra_api` tests and the audit cases included.

- [ ] **Step 7: Commit.**

```powershell
git add -A
git commit -m "feat(auth): serve POST /v1/auth/entra and route only the offered sign-ins"
```

### Task 10: Wire it into the service

**Files:**
- Modify: `crates\push-auth\src\server.rs`, `src\tests\mod.rs`, `README.md`
- Create: `crates\push-auth\src\tests\entra_server.rs`

- [ ] **Step 1: Write the failing test.** In `src\tests\mod.rs` add `mod entra_server;`.
Create `crates\push-auth\src\tests\entra_server.rs`:

```rust
//! The whole service from its variables, Entra ID only, on real sockets with PostgreSQL
//! and Redis (Docker must be running) and the stub issuer.

use std::net::SocketAddr;
use std::time::Duration;

use serde_json::json;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::Config;
use crate::entra::testing::StubOpenId;

async fn ready(client: &reqwest::Client, health: SocketAddr) {
    for _ in 0..240 {
        let status = client
            .get(format!("http://{health}/readyz"))
            .send()
            .await
            .map(|answer| answer.status().as_u16());
        if status == Ok(200) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("auth-svc did not become ready within 60 s");
}

#[tokio::test]
async fn the_service_starts_entra_only_and_signs_in_over_a_socket() {
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
    let stub = StubOpenId::start().await;
    let mut variables: Vec<(String, String)> = vec![
        (
            "PUSH_DATABASE_URL".to_owned(),
            format!(
                "postgres://postgres:postgres@127.0.0.1:{}/postgres",
                postgres.get_host_port_ipv4(5432).await.unwrap()
            ),
        ),
        (
            "PUSH_REDIS_URL".to_owned(),
            format!(
                "redis://127.0.0.1:{}",
                redis.get_host_port_ipv4(6379).await.unwrap()
            ),
        ),
        ("PUSH_AUTH_PUBLIC_URL".to_owned(), "http://push.local".to_owned()),
        (
            "PUSH_AUTH_TOKEN_AUDIENCE".to_owned(),
            "ws://push.local/v1/socket".to_owned(),
        ),
        // base64 of 32 bytes of 0x07 and of 0x09.
        (
            "PUSH_AUTH_SIGNING_KEY".to_owned(),
            "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=".to_owned(),
        ),
        (
            "PUSH_AUTH_LINK_KEY".to_owned(),
            "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=".to_owned(),
        ),
    ];
    variables.extend(stub.variables());
    let config = Config::from_lookup(|name| {
        variables
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (addr, health_addr) = (http.local_addr().unwrap(), health.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(crate::serve(config, http, health, shutdown.clone()));

    let client = reqwest::Client::new();
    ready(&client, health_addr).await;
    let signed_in = client
        .post(format!("http://{addr}/v1/auth/entra"))
        .json(&json!({"token": stub.token("55555555-0000-4000-8000-000000000005")}))
        .send()
        .await
        .unwrap();
    assert_eq!(signed_in.status(), 200);
    let login = client
        .post(format!("http://{addr}/v1/auth/login"))
        .json(&json!({"email": "a@example.com", "password": "correct horse"}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 404);
    assert!(stub.jwks_fetches() >= 1);

    shutdown.cancel();
    server.await.unwrap().unwrap();
}
```

- [ ] **Step 2: Run it to see it fail.**

Run: `cargo test -p push-auth --lib tests::entra_server`
Expected: FAIL: the entra sign-in answers 404 (the server ignores the providers).

- [ ] **Step 3: Wire.** In `crates\push-auth\src\server.rs`:

Add `use crate::entra::EntraSignIn;` and change `use crate::service::{Auth, Settings};` to
`use crate::service::{Auth, Settings, SignIn};`.

After `let links = LinkSigner::new(…)?;` add:

```rust
    // Built before the datastores, so a bad Entra ID setting stops start-up at once.
    let entra = match &config.entra {
        Some(settings) => {
            Some(EntraSignIn::new(settings.clone()).map_err(anyhow::Error::msg)?)
        }
        None => None,
    };
```

Replace `let auth = Auth::new(pool.clone(), Settings { … });` with the same call followed by
`.with_sign_in(…)`, and add the warm-up and a notice before it:

```rust
    if let Some(entra) = &entra {
        // Fetched now so the first sign-in is quick; a failure only warns: readiness never
        // depends on Entra ID (spec D4).
        let warming = entra.clone();
        tokio::spawn(async move { warming.warm().await });
    }
    if !config.providers.password {
        tracing::info!(
            "email and password sign-in is off: only Microsoft Entra ID sign-in is served"
        );
    }
    let auth = Auth::new(
        pool.clone(),
        Settings {
            public_url: config.public_url.clone(),
            passwords: Passwords::owasp(),
            links,
            tokens,
            hash_concurrency: config.hash_concurrency,
            hash_queue: config.hash_queue,
            hash_wait: HASH_WAIT,
        },
    )
    .with_sign_in(SignIn {
        password: config.providers.password,
        entra,
    });
```

- [ ] **Step 4: Run.**

Run: `cargo test -p push-auth`
Expected: every test passes.

- [ ] **Step 5: README.** In `README.md`'s `push-auth` variable table, after the
`PUSH_DIRECTORY_TOKEN` row, add:

```markdown
| `PUSH_AUTH_PROVIDERS` | `password`, `entra` or `password,entra`; without `password` the signup, verification, login and password-reset routes answer 404 ([Entra ID sign-in](docs/entra-sign-in.md)) | `password` |
| `PUSH_ENTRA_CLIENT_ID` | accepted audiences: the API registration's application id, optionally with its `api://` identifier URI | required with `entra` |
| `PUSH_ENTRA_ALLOWED_TENANTS` | tenant ids (GUIDs), comma-separated; `common`, `organizations` and `consumers` are refused | required with `entra` |
| `PUSH_ENTRA_AUTHORITY_HOST` | Microsoft Entra ID's host (a sovereign cloud's); `http://` only for loopback | `https://login.microsoftonline.com/` |
| `PUSH_ENTRA_REQUIRED_SCOPE` | the delegated scope tokens must carry; `*` for any scope of the API | `access_as_user` |
| `PUSH_ENTRA_REQUIRED_ROLE` | an app role tokens must carry | none |
| `PUSH_ENTRA_ALLOWED_CLIENT_APPS` | client app ids allowed to present tokens (`azp`), comma-separated | any |
| `PUSH_ENTRA_MAX_SESSION_HOURS` | maximum session age of an Entra-backed device, 1 to 720; refresh fails after it until the user signs in with Entra ID again | `24` |
```

- [ ] **Step 6: Commit.**

```powershell
git add -A
git commit -m "feat(auth): offer the configured sign-in providers"
```

### Task 11: The worker without email

**Files:**
- Modify: `crates\push-worker\src\config.rs`, `src\jobs.rs`, `src\server.rs`, `README.md`

- [ ] **Step 1: Write the failing tests.** In `crates\push-worker\src\config.rs`'s tests add:

```rust
    #[test]
    fn email_can_be_off_without_a_webhook_or_a_mailbox() {
        let off = config(&with(&[("PUSH_EMAIL_SENDER", "none")])).unwrap();
        assert_eq!(off.sender, Sender::Off);
        assert_eq!(format!("{:?}", off.sender), "Off");
        for extra in [
            ("PUSH_RESEND_WEBHOOK_SECRET", SECRET),
            ("PUSH_DEV_MAILBOX", "1"),
        ] {
            let result = config(&with(&[("PUSH_EMAIL_SENDER", "none"), extra]));
            assert!(
                matches!(result, Err(ConfigError::Invalid { name, .. }) if name == extra.0),
                "{extra:?}"
            );
        }
    }
```

and at the end of `crates\push-worker\src\jobs.rs`:

```rust
#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn without_email_an_email_job_is_buried_unsent() {
        let accounts = Accounts::new("http://127.0.0.1:1", "accounts-token-of-the-jobs-tests")
            .unwrap();
        let job = Job {
            id: Uuid::now_v7(),
            kind: "email.send".to_owned(),
            payload: json!({
                "to": "a@example.com",
                "template": "verify_email",
                "params": {"link": "https://push.example/v1/auth/verify?token=t"},
            }),
            attempts: 1,
            claimed_at: Utc::now(),
        };
        let outcome = Jobs::without_email(accounts).run(&job).await;
        assert!(
            matches!(&outcome, Outcome::Poison(reason) if reason.contains("PUSH_EMAIL_SENDER=none")),
            "{outcome:?}"
        );
    }
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p push-worker --lib`
Expected: compile errors (`Sender::Off`, `Jobs::without_email` not found).

- [ ] **Step 3: Configuration.** In `crates\push-worker\src\config.rs`:

Add a variant to `enum Sender` after `Log`:

```rust
    /// No email at all (`none`): Microsoft Entra ID-only installations write none. An
    /// `email.send` job is buried unsent.
    Off,
```

and to its `Debug`: `Self::Off => formatter.write_str("Off"),`.

Change the `PUSH_EMAIL_SENDER` row of the doc table to
``/// | `PUSH_EMAIL_SENDER` | `log` (logs instead of sending), `resend`, or `none` (no email: an `email.send` job is buried unsent; no webhook, no mailbox) | `log` |``.

In `from_lookup`, add the arm `Some("none") => Sender::Off,` after
`None | Some("log") => Sender::Log,`, change the unknown sender's reason to
`"use log, resend or none"`, and the `from` match to:

```rust
        let from = match sender {
            Sender::Log | Sender::Off => set("PUSH_EMAIL_FROM").unwrap_or_default(),
            Sender::Resend { .. } => required("PUSH_EMAIL_FROM")?,
        };
```

After the `webhook_secret` validation add:

```rust
        if webhook_secret.is_some() && sender == Sender::Off {
            return Err(redacted(
                "PUSH_RESEND_WEBHOOK_SECRET",
                "no email is sent with PUSH_EMAIL_SENDER=none, so there is no webhook".to_owned(),
            ));
        }
```

(The existing mailbox check already refuses `PUSH_DEV_MAILBOX=1` with any sender but `log`.)

- [ ] **Step 4: Jobs.** In `crates\push-worker\src\jobs.rs` change the field to
`mailer: Option<Mailer>,`, `new` to set `mailer: Some(mailer),`, and add:

```rust
    /// The jobs of a worker that sends no email (`PUSH_EMAIL_SENDER=none`).
    pub fn without_email(accounts: Accounts) -> Self {
        Self {
            accounts,
            mailer: None,
            cleanup: None,
        }
    }
```

In `run`, replace `return self.mailer.send(job).await;` with:

```rust
            return match &self.mailer {
                Some(mailer) => mailer.send(job).await,
                // Buried without its link, like any poison (queue.rs).
                None => Outcome::Poison(
                    "email.send: email is off (PUSH_EMAIL_SENDER=none); nothing sends it"
                        .to_owned(),
                ),
            };
```

- [ ] **Step 5: Server.** In `crates\push-worker\src\server.rs` replace the `sender` match
with:

```rust
    let sender: Option<Arc<dyn EmailSender>> = match (&config.sender, &mailbox) {
        (Sender::Off, _) => None,
        (Sender::Log, Some(mailbox)) => Some(Arc::new(MailboxSender::new(
            LogSender,
            mailbox.clone(),
        ))),
        (Sender::Log, None) => Some(Arc::new(LogSender)),
        (Sender::Resend { api_url, api_key }, _) => Some(Arc::new(
            ResendSender::new(api_url, api_key.clone(), config.from.clone())
                .map_err(anyhow::Error::msg)?,
        )),
    };
```

and the `mailer`/`jobs` lines with:

```rust
    let jobs = match sender {
        Some(sender) => Jobs::new(
            accounts,
            Mailer::new(
                pool.clone(),
                sender,
                Templates::new(config.product_name.clone()),
            ),
        ),
        None => {
            tracing::info!("email is off (PUSH_EMAIL_SENDER=none)");
            Jobs::without_email(accounts)
        }
    };
```

keeping `let cleanup = Cleanup::new(…);` before it and changing the next line to
`let jobs = jobs.with_cleanup(cleanup);`.

- [ ] **Step 6: Run.**

Run: `cargo test -p push-worker`
Expected: every test passes (Docker running), the two new ones included.

- [ ] **Step 7: README.** In `README.md`'s `push-worker` table change the
`PUSH_EMAIL_SENDER` row to
``| `PUSH_EMAIL_SENDER` | `log` (logs instead of sending), `resend`, or `none` (no email at all, for Microsoft Entra ID-only installations: an `email.send` job is buried unsent; refused with `PUSH_RESEND_WEBHOOK_SECRET` or the mailbox) | `log` |``.

- [ ] **Step 8: Commit.**

```powershell
git add -A
git commit -m "feat(worker): add PUSH_EMAIL_SENDER=none for installations without email"
```

### Task 12: The SDK signs in with an Entra ID access token

**Files:**
- Modify: `crates\push-sdk\src\auth.rs`, `src\client.rs`, `src\credentials.rs`, `src\testing.rs`

- [ ] **Step 1: The fake endpoint.** In `crates\push-sdk\src\testing.rs`:

Add after `WRONG_PASSWORD`:

```rust
/// The Entra ID token [`FakeAuth`] refuses with `entra_token_invalid`; it accepts any other.
pub const WRONG_ENTRA_TOKEN: &str = "refused-entra-token";
```

Add a field to `FakeAuth` after `signups`:

```rust
    /// Every `POST /v1/auth/entra` body, in order.
    pub entra_bodies: Arc<Mutex<Vec<Value>>>,
```

In `FakeAuth::start`, add `let entra_bodies = Arc::new(Mutex::new(Vec::new()));` and
`let bodies = entra_bodies.clone();` after `let signed_up = signups.clone();`, change the
token closures' split to

```rust
        let (login_tokens, device_tokens, entra_tokens, refresh_tokens) =
            (tokens.clone(), tokens.clone(), tokens.clone(), tokens);
```

add the route after the `/v1/auth/login` route:

```rust
            .route(
                "/v1/auth/entra",
                post(move |Json(body): Json<Value>| {
                    let (bodies, tokens) = (bodies.clone(), entra_tokens.clone());
                    async move {
                        bodies.lock().unwrap().push(body.clone());
                        if body["token"] == WRONG_ENTRA_TOKEN {
                            let error = json!({"error": {"code": "entra_token_invalid", "message": "the Microsoft Entra ID token is not accepted", "retryable": false}});
                            return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
                        }
                        if body.get("device_id").is_some() {
                            Json(tokens(200)).into_response()
                        } else {
                            Json(json!({"registration_token": "registration", "expires_in_s": 300}))
                                .into_response()
                        }
                    }
                }),
            )
```

and `entra_bodies,` to the `Self { … }` literal.

- [ ] **Step 2: Write the failing tests.** In `crates\push-sdk\src\client.rs`'s tests module
add:

```rust
    #[tokio::test]
    async fn an_entra_sign_in_registers_once_and_signs_the_same_device_in_again() {
        use serde_json::json;

        use crate::testing::WRONG_ENTRA_TOKEN;

        let (auth, gateway) = (FakeAuth::start().await, FakeGateway::start().await);
        let secrets = Arc::new(MemorySecretStore::new());
        let database = format!("file:client-{}?mode=memory&cache=shared", Uuid::now_v7());
        let (client, _events) = open(&auth, &gateway, &database, &secrets);
        let account = client
            .sign_in_with_entra("entra-access-token", Some("erin"), "laptop", Platform::Web)
            .await
            .unwrap();
        assert_eq!(
            (account.user_id, account.device_id),
            (auth.user, auth.device)
        );
        assert_eq!(auth.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(secrets.load(REGISTRATION_KEY).unwrap(), None);

        // Signed out by the server (the session's maximum age): the same device again.
        client.inner.credentials.sign_out().await.unwrap();
        client
            .sign_in_with_entra("a-newer-entra-token", None, "laptop", Platform::Web)
            .await
            .unwrap();
        assert_eq!(auth.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(
            auth.entra_bodies.lock().unwrap().clone(),
            vec![
                json!({"token": "entra-access-token", "handle": "erin"}),
                json!({"token": "a-newer-entra-token", "handle": null, "device_id": auth.device}),
            ]
        );
        let refused = client
            .sign_in_with_entra(WRONG_ENTRA_TOKEN, None, "laptop", Platform::Web)
            .await
            .unwrap_err();
        assert_eq!(refused.code(), "entra_token_invalid");
    }
```

and in `crates\push-sdk\src\credentials.rs`'s tests:

```rust
    #[test]
    fn an_ended_session_is_refused_like_a_revoked_token() {
        let ended = SdkError::Server {
            code: "session_expired".to_owned(),
            message: "the session has reached its maximum age; sign in again".to_owned(),
            retryable: false,
            retry_after_s: None,
        };
        assert!(is_refused(&ended));
    }
```

- [ ] **Step 3: Run them to see them fail.**

Run: `cargo test -p push-sdk --lib`
Expected: compile error (`sign_in_with_entra` not found); after Step 4's first edit, the
credentials test fails until `session_expired` is added.

- [ ] **Step 4: Implement.** In `crates\push-sdk\src\credentials.rs`, `is_refused`:

```rust
/// auth-svc refused the credentials for good: they were revoked, reused or expired, or
/// the Entra ID-backed session reached its maximum age.
pub(crate) fn is_refused(error: &SdkError) -> bool {
    matches!(error, SdkError::Server { code, .. }
        if matches!(code.as_str(), "unauthorized" | "token_expired" | "session_expired" | "http_401"))
}
```

In `crates\push-sdk\src\auth.rs`, after `login_device`:

```rust
    /// `POST /v1/auth/entra`: a registration token for the account of an Entra ID access
    /// token for auth-svc's API; `handle` names a new account only. Never repeated: it
    /// counts against rate limits.
    pub async fn entra(&self, access_token: &str, handle: Option<&str>) -> Result<String, SdkError> {
        let body = json!({ "token": access_token, "handle": handle });
        let answer = once(self.request(Method::POST, "/v1/auth/entra").json(&body)).await?;
        answer["registration_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| unreadable("the sign-in answer"))
    }

    /// `POST /v1/auth/entra` with an active device: its new tokens and a new session.
    pub async fn entra_device(
        &self,
        access_token: &str,
        handle: Option<&str>,
        device: DeviceId,
    ) -> Result<Tokens, SdkError> {
        let body = json!({ "token": access_token, "handle": handle, "device_id": device });
        let answer = once(self.request(Method::POST, "/v1/auth/entra").json(&body)).await?;
        tokens(answer)
    }
```

In `crates\push-sdk\src\client.rs`:

Add near the other private items (before `impl Client`):

```rust
/// How a sign-in proves who the user is.
enum Proof {
    Password { email: String, password: String },
    /// An Entra ID access token the app obtained (MSAL); `handle` names a new account.
    Entra {
        access_token: String,
        handle: Option<String>,
    },
}

impl Proof {
    /// New tokens for `device`, a device this client registered before.
    async fn device(&self, auth: &AuthApi, device: DeviceId) -> Result<Tokens, SdkError> {
        match self {
            Self::Password { email, password } => auth.login_device(email, password, device).await,
            Self::Entra {
                access_token,
                handle,
            } => auth.entra_device(access_token, handle.as_deref(), device).await,
        }
    }

    /// A registration token for a new device.
    async fn registration(&self, auth: &AuthApi) -> Result<String, SdkError> {
        match self {
            Self::Password { email, password } => auth.login(email, password).await,
            Self::Entra {
                access_token,
                handle,
            } => auth.entra(access_token, handle.as_deref()).await,
        }
    }
}
```

(add `use crate::auth::{AuthApi, Tokens};` to the imports if `client.rs` does not import them
yet; `DeviceId` is already imported).

Replace `sign_in` with the pair below and a private `sign_in_with` whose body is the old
`sign_in` body with the two auth calls going through the proof:

```rust
    /// Signs in on this device (spec §5.3). The device this client registered before is
    /// signed in again if it is still active; otherwise a device is registered, with a
    /// `registration_id` kept until the registration is answered and its tokens saved, so
    /// a retry, even after a restart, never creates a second device. Then call
    /// [`Client::connect`]. A connection that is up (or coming up) is stopped first, and
    /// this waits for it, so one for an earlier user never writes into the new user's
    /// data ([`SdkError::ConnectionBusy`] if it does not stop in time: the new session is
    /// saved then, and signing in again completes the sign-in). Connect again afterwards.
    pub async fn sign_in(
        &self,
        email: &str,
        password: &str,
        device_name: &str,
        platform: Platform,
    ) -> Result<Account, SdkError> {
        let proof = Proof::Password {
            email: email.to_owned(),
            password: password.to_owned(),
        };
        self.sign_in_with(proof, device_name, platform).await
    }

    /// Signs in on this device with a Microsoft Entra ID access token for auth-svc's API,
    /// which the app obtained itself with MSAL (scope `api://<api-client-id>/access_as_user`,
    /// docs/entra-sign-in.md). `handle` names the account on its first sign-in only. The
    /// device is kept exactly as by [`Client::sign_in`]. After the session's maximum age
    /// the SDK reports `SignedOut`: get a new token and call this again (same device). The
    /// token is sent once and never stored or logged.
    pub async fn sign_in_with_entra(
        &self,
        access_token: &str,
        handle: Option<&str>,
        device_name: &str,
        platform: Platform,
    ) -> Result<Account, SdkError> {
        let proof = Proof::Entra {
            access_token: access_token.to_owned(),
            handle: handle.map(str::to_owned),
        };
        self.sign_in_with(proof, device_name, platform).await
    }

    async fn sign_in_with(
        &self,
        proof: Proof,
        device_name: &str,
        platform: Platform,
    ) -> Result<Account, SdkError> {
        let inner = self.inner.clone();
        let device_name = device_name.to_owned();
        self.run(async move {
            let credentials = &inner.credentials;
            let mut tokens = None;
            if let Some(device) = credentials.last_device()? {
                match proof.device(&inner.auth, device).await {
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
                    let registration = proof.registration(&inner.auth).await?;
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
            let session = credentials.sign_in(tokens).await?;
            credentials.registration_done()?;
            let user = session.user_id;
            // A connection of the earlier user may be live (or handshaking): it stops
            // before the store changes hands, and it does not come back as the earlier
            // user, the session being the new one's now. The app connects again. Not
            // cut short once the session is saved: a store left with the earlier user's
            // data under the new session would let a connection write into it.
            let tail = inner.clone();
            shielded(async move {
                stop_connection(&tail).await?;
                blocking(&tail.shared, move |store| store.bind_owner(user)).await?;
                Ok(())
            })
            .await?;
            Ok(Account {
                user_id: session.user_id,
                device_id: session.device_id,
            })
        })
        .await
    }
```

This is the old body with `inner.auth.login_device(&email, &password, device)` and
`inner.auth.login(&email, &password)` replaced by the proof's calls. Also extend the
`Client` type's doc where it lists sign-in: "signs up and in through auth-svc (email and
password, or an Entra ID access token)".

- [ ] **Step 5: Run.**

Run: `cargo test -p push-sdk`
Expected: every test passes, the two new ones included.

- [ ] **Step 6: Commit.**

```powershell
git add -A
git commit -m "feat(sdk): sign in with a Microsoft Entra ID access token"
```

### Task 13: The bindings

**Files:**
- Modify: `bindings\node\src\client.rs`, `bindings\node\index.js`, `bindings\node\index.d.ts`, `bindings\node\test\client.test.js`, `bindings\node\test\types.ts`
- Modify: `bindings\c\src\client.rs`, `bindings\c\src\lib.rs`, `bindings\c\include\push_sdk.h`
- Modify: `docs\sdk-bindings.md`

- [ ] **Step 1: Write the failing tests.** In `bindings\node\test\client.test.js`, after the
"errors carry the SDK codes" test:

```js
test('signs in with an Entra ID access token the app obtained', async () => {
  const client = await Client.open(config(), memoryStore());
  const account = await client.signInWithEntra('entra-access-token', 'erin', 'node test', 'desktop');
  assert.equal(account.userId, fakes.userId);
  await assert.rejects(
    client.signInWithEntra('refused-entra-token', null, 'node test', 'desktop'),
    code('entra_token_invalid'),
  );
  await client.close();
});
```

In `bindings\node\test\types.ts`, after the `signIn` line add:

```ts
  const viaEntra: Account = await client.signInWithEntra('token', null, 'laptop', 'desktop');
  console.log(viaEntra.deviceId);
```

(if `Account` is not imported there yet, add it to the existing type import from `'..'`).

In `bindings\c\src\lib.rs`'s tests, after `fn sign_in`:

```rust
    #[test]
    fn an_entra_access_token_signs_in_and_a_refused_one_reports_its_code() {
        let servers = servers();
        let client = open(&servers, memory_store());
        let (mut account, mut error) = (ptr::null_mut(), ptr::null_mut());
        let status = unsafe {
            push_client_sign_in_with_entra(
                client,
                c"entra-access-token".as_ptr(),
                ptr::null(),
                c"c app".as_ptr(),
                c"desktop".as_ptr(),
                &mut account,
                &mut error,
            )
        };
        assert_eq!(status, PUSH_OK);
        let json = unsafe { CStr::from_ptr(account) }.to_str().unwrap().to_owned();
        assert!(json.contains("userId"), "{json}");
        unsafe { push_string_free(account) };
        let (mut account, mut error) = (ptr::null_mut(), ptr::null_mut());
        let status = unsafe {
            push_client_sign_in_with_entra(
                client,
                c"refused-entra-token".as_ptr(),
                c"erin".as_ptr(),
                c"c app".as_ptr(),
                c"desktop".as_ptr(),
                &mut account,
                &mut error,
            )
        };
        assert_eq!(status, PUSH_ERROR);
        assert!(account.is_null());
        assert_eq!(code(error), "entra_token_invalid");
        unsafe { push_client_destroy(client) };
    }
```

- [ ] **Step 2: Run them to see them fail.**

Run: `cargo test -p push-sdk-c --lib`
Expected: compile error (`push_client_sign_in_with_entra` not found).

- [ ] **Step 3: C.** In `bindings\c\src\client.rs` after `push_client_sign_in`:

```rust
/// Signs in on this device with a Microsoft Entra ID access token the app obtained (MSAL);
/// `handle_name` (may be NULL) names the account on its first sign-in only. `*account_json`
/// gets `{"userId", "deviceId"}`. Then connect. The token is never stored or logged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_sign_in_with_entra(
    client: *const PushClient,
    access_token: *const c_char,
    handle_name: *const c_char,
    device_name: *const c_char,
    platform: *const c_char,
    account_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(account_json, "account_json")?;
        let this = unsafe { handle(client) }?;
        let access_token = unsafe { text(access_token, "access_token") }?;
        let handle_name = if handle_name.is_null() {
            None
        } else {
            Some(unsafe { text(handle_name, "handle_name") }?)
        };
        let (device_name, platform) = unsafe {
            (
                text(device_name, "device_name")?,
                text(platform, "platform")?,
            )
        };
        let platform = json::platform(platform)?;
        let account = wait(this.client.sign_in_with_entra(
            access_token,
            handle_name,
            device_name,
            platform,
        ))?;
        put_json(out, &json::account(&account));
        Ok(())
    })
}
```

Regenerate the header:

```powershell
$env:PUSH_SDK_BLESS_HEADER = '1'
cargo test -p push-sdk-c --test header
Remove-Item Env:PUSH_SDK_BLESS_HEADER
cargo test -p push-sdk-c
```

Expected: the header gains `push_client_sign_in_with_entra`; every C binding test passes
(the C program included).

- [ ] **Step 4: Node.** In `bindings\node\src\client.rs` after `sign_in`:

```rust
    #[napi(catch_unwind)]
    pub fn sign_in_with_entra<'env>(
        &self,
        env: &'env Env,
        access_token: String,
        handle: Option<String>,
        device_name: String,
        platform: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let platform = json::platform(&platform)?;
            let account = client
                .sign_in_with_entra(&access_token, handle.as_deref(), &device_name, platform)
                .await?;
            Ok(json::account(&account))
        })
    }
```

In `bindings\node\index.js` after `signIn(…)`:

```js
  /**
   * Signs in with a Microsoft Entra ID access token the app obtained with MSAL; `handle`
   * (or null) names the account on its first sign-in only. The token is never stored.
   */
  signInWithEntra(accessToken, handle, deviceName, platform) {
    return settled(() => this.#native.signInWithEntra(accessToken, handle ?? null, deviceName, platform));
  }
```

In `bindings\node\index.d.ts` after the `signIn` line:

```ts
  /**
   * Signs in with a Microsoft Entra ID access token for auth-svc's API, obtained by the app
   * with MSAL (scope `api://<api-client-id>/access_as_user`). `handle` names the account on
   * its first sign-in only. Rejects with `entra_token_invalid` or `entra_unavailable`.
   */
  signInWithEntra(accessToken: string, handle: string | null, deviceName: string, platform: Platform): Promise<Account>;
```

Run:

```powershell
cd bindings/node
npm ci
npm run build
npm test
cd ../..
```

Expected: every Node test passes and `tsc` accepts `test/types.ts` (the `npm test` script runs
it, as in Plan 10).

- [ ] **Step 5: Docs.** In `docs\sdk-bindings.md`, add a short section after the error codes:

```markdown
### Signing in with Microsoft Entra ID

Where auth-svc offers Entra ID sign-in ([guide](entra-sign-in.md)), the app gets an access
token for auth-svc's API with MSAL and passes it on: `client.sign_in_with_entra(token,
handle, device_name, platform)` in Rust, `client.signInWithEntra(token, handle, deviceName,
platform)` in Node and `push_client_sign_in_with_entra(client, token, handle_name /* or NULL */,
device_name, platform, &account_json, &error)` in C. `handle` names the account on its
first sign-in only. The device is kept as with email and password. New codes:
`entra_token_invalid` (get a new token; it was for another API, tenant or app, expired, or
lacked the scope), `entra_unavailable` (retryable), and `session_expired`, after which the SDK
reports `SignedOut` and the app signs in with Entra ID again.
```

- [ ] **Step 6: Commit.**

```powershell
git add -A
git commit -m "feat(bindings): sign in with an Entra ID access token from Node and C"
```

### Task 14: The whole stack with Entra ID sign-in only

**Files:**
- Modify: `crates\push-smoke\Cargo.toml`, `tests\common\mod.rs`, `tests\local.rs`
- Create: `crates\push-smoke\tests\entra_sign_in.rs`

- [ ] **Step 1: Test plumbing.** In `crates\push-smoke\Cargo.toml` replace the dev-dependency
`push-auth.workspace = true` with:

```toml
push-auth = { workspace = true, features = ["testing"] }
```

(`reqwest`, `serde_json`, `sqlx`, `tracing` and `tracing-subscriber`, which the new test
uses, are already push-smoke dependencies.)

In `tests\common\mod.rs`, replace `lookup` with:

```rust
/// A variable lookup over `variables`, as each service's `Config::from_lookup` takes. A
/// later entry wins, so a test can override what `Stack` sets.
pub fn lookup(variables: Vec<(String, String)>) -> impl Fn(&str) -> Option<String> {
    move |name| {
        variables
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}
```

Move the datastore half of `local.rs`'s `start()` into `common\mod.rs`:

```rust
/// The containers the stack runs on; they stop when dropped.
pub type Containers = (
    testcontainers_modules::testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    testcontainers_modules::testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
);

/// Password-mode PostgreSQL and Redis in containers (Docker must be running), with
/// databases for auth-svc and the worker as the chart creates them.
pub async fn password_datastores() -> (Datastores, Containers) {
    use testcontainers_modules::postgres::Postgres;
    use testcontainers_modules::redis::Redis;
    use testcontainers_modules::testcontainers::ImageExt;
    use testcontainers_modules::testcontainers::runners::AsyncRunner;

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
    let port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let database_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let redis_url = format!(
        "redis://127.0.0.1:{}",
        redis.get_host_port_ipv4(6379).await.unwrap()
    );
    // auth-svc and the worker have databases of their own in the same server, as in the
    // chart; the worker's role may also drain auth-svc's outbox, which auth-svc grants.
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    for statement in [
        "CREATE DATABASE push_auth",
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement).execute(&admin).await.unwrap();
    }
    admin.close().await;
    let auth_database_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/push_auth");
    let worker_server = format!("postgres://push_worker:worker-secret@127.0.0.1:{port}");
    let datastores = Datastores {
        auth: owned(&[
            ("PUSH_DATABASE_URL", &auth_database_url),
            ("PUSH_REDIS_URL", &redis_url),
            ("PUSH_AUTH_WORKER_ROLE", "push_worker"),
        ]),
        message_svc: owned(&[
            ("PUSH_DATABASE_URL", &database_url),
            ("PUSH_REDIS_URL", &redis_url),
        ]),
        gateway: owned(&[("PUSH_REDIS_URL", &redis_url)]),
        worker: owned(&[
            ("PUSH_DATABASE_URL", &format!("{worker_server}/push_worker")),
            ("PUSH_AUTH_DATABASE_URL", &format!("{worker_server}/push_auth")),
        ]),
    };
    (datastores, (postgres, redis))
}
```

In `tests\local.rs` replace `start()`'s body with:

```rust
async fn start() -> (Stack, Containers) {
    let (datastores, containers) = common::password_datastores().await;
    (Stack::start(datastores).await, containers)
}
```

remove its `type Containers` and the now unused imports (`Postgres`, `Redis`, `AsyncRunner`,
`ContainerAsync`, `ImageExt`), and import `common::Containers`.

Run: `cargo test -p push-smoke --test local`
Expected: both local tests pass, as before.

- [ ] **Step 2: Write the test.** Create `crates\push-smoke\tests\entra_sign_in.rs`:

```rust
//! The whole stack with Microsoft Entra ID sign-in only (spec 2026-10-19 AC2): auth-svc
//! trusts the stub issuer and serves no email route, the worker sends no email, and two
//! users sign in with Entra ID tokens through push-sdk, become contacts and chat; signing
//! in again keeps the device. No token, object id, name or address reaches a log.
//!
//! One test per binary: it owns the global log capture.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::Stack;
use push_auth::entra::testing::StubOpenId;
use push_sdk::{
    Client, Config, ConnectionState, ContactState, Event, Events, MemorySecretStore,
    MessageStatus, Platform, UserId,
};

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

struct App {
    client: Client,
    events: Events,
    user: UserId,
}

impl App {
    /// Signs `name` in with an Entra ID token for `object_id`, registering a device, and
    /// connects (the gateway answers 4006 until the worker relays the device; the SDK
    /// reconnects by itself).
    async fn sign_in(
        stack: &Stack,
        stub: &StubOpenId,
        data_dir: &Path,
        name: &str,
        object_id: &str,
        tokens: &mut Vec<String>,
    ) -> Self {
        let database = data_dir.join(format!("{name}.db"));
        let mut config = Config::new(
            &stack.auth_url,
            &stack.socket_url,
            &database.to_string_lossy(),
        );
        // Plain HTTP to this machine's stack, as the local smoke test does.
        config.allow_insecure_transport = true;
        let (client, events) = Client::open(config, Arc::new(MemorySecretStore::new())).unwrap();
        let token = stub.token(object_id);
        tokens.push(token.clone());
        let account = client
            .sign_in_with_entra(&token, Some(name), &format!("{name}'s laptop"), Platform::Desktop)
            .await
            .expect("an Entra ID sign-in");
        client.connect().unwrap();
        let mut app = Self {
            client,
            events,
            user: account.user_id,
        };
        app.wait("Connected", |event| {
            matches!(
                event,
                Event::ConnectionStateChanged(ConnectionState::Connected)
            )
        })
        .await;
        app
    }

    async fn wait(&mut self, what: &str, wanted: impl Fn(&Event) -> bool) -> Event {
        let events = &mut self.events;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let event = events.next().await.expect("the client is open");
                if wanted(&event) {
                    return event;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {what} within 60 s"))
    }
}

#[tokio::test]
async fn entra_users_sign_in_and_chat_with_no_email_anywhere() {
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(|| Capture)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let stub = StubOpenId::start().await;
    let (mut datastores, _containers) = common::password_datastores().await;
    datastores.auth.extend(stub.variables());
    datastores.worker.extend([
        ("PUSH_EMAIL_SENDER".to_owned(), "none".to_owned()),
        ("PUSH_DEV_MAILBOX".to_owned(), "0".to_owned()),
    ]);
    let worker_database = datastores
        .worker
        .iter()
        .find(|(name, _)| name == "PUSH_DATABASE_URL")
        .map(|(_, value)| value.clone())
        .unwrap();
    let stack = Stack::start(datastores).await;

    let http = reqwest::Client::new();
    for path in [
        "/v1/auth/signup",
        "/v1/auth/login",
        "/v1/auth/password/forgot",
        "/v1/auth/verify/resend",
    ] {
        let status = http
            .post(format!("{}{path}", stack.auth_url))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 404, "{path}");
    }

    let data_dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("push-entra-sign-in-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let (erin_oid, finn_oid) = (uuid::Uuid::now_v7().to_string(), uuid::Uuid::now_v7().to_string());
    let mut tokens = Vec::new();
    let mut erin = App::sign_in(&stack, &stub, &data_dir, "erin", &erin_oid, &mut tokens).await;
    let mut finn = App::sign_in(&stack, &stub, &data_dir, "finn", &finn_oid, &mut tokens).await;

    let invite = erin.client.create_invite(600, 1).await.unwrap();
    assert_eq!(finn.client.redeem_invite(&invite.code).await.unwrap(), erin.user);
    let finn_user = finn.user;
    erin.wait("the new contact", |event| {
        matches!(event, Event::ContactChanged(contact)
            if contact.user_id == finn_user && contact.state == ContactState::Accepted)
    })
    .await;
    let chat = erin.client.open_direct(finn.user).await.unwrap().id;
    let text = format!("hello over Entra ID {}", uuid::Uuid::now_v7());
    let id = erin.client.send(chat, &text).unwrap().client_msg_id;
    let Event::MessageAdded(received) = finn
        .wait("the message", |event| {
            matches!(event, Event::MessageAdded(message) if message.client_msg_id == id)
        })
        .await
    else {
        unreachable!("the wait accepts only MessageAdded");
    };
    assert_eq!(received.body.as_deref(), Some(text.as_str()));
    assert_eq!(received.status, MessageStatus::Received);

    // Signing in again keeps the device.
    let device = erin.client.account().unwrap().unwrap().device_id;
    let again = stub.token(&erin_oid);
    tokens.push(again.clone());
    let account = erin
        .client
        .sign_in_with_entra(&again, None, "erin's laptop", Platform::Desktop)
        .await
        .unwrap();
    assert_eq!(account.device_id, device);

    // No email was ever queued, sent or buried.
    let worker = sqlx::PgPool::connect(&worker_database).await.unwrap();
    let emails: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM worker.job WHERE kind = 'email.send') \
         + (SELECT count(*) FROM worker.email_message)",
    )
    .fetch_one(&worker)
    .await
    .unwrap();
    assert_eq!(emails, 0);
    worker.close().await;

    for app in [erin, finn] {
        app.client.close().await.unwrap();
    }
    stack.stop().await;
    let log = String::from_utf8(LOG.lock().unwrap().clone()).unwrap();
    assert!(!log.is_empty(), "the log was captured");
    for secret in tokens
        .iter()
        .map(String::as_str)
        .chain([erin_oid.as_str(), finn_oid.as_str(), "Test User", "test.user@contoso.test"])
    {
        let leak = log
            .lines()
            .find(|line| line.contains(secret))
            .map(|line| line.replace(secret, "<SECRET>"));
        assert!(leak.is_none(), "a secret reached a log: {leak:?}");
    }
    let _ = std::fs::remove_dir_all(&data_dir);
}
```

- [ ] **Step 3: Run.**

Run: `cargo test -p push-smoke --test entra_sign_in`
Expected: 1 test passes in well under 2 minutes.

- [ ] **Step 4: Commit.**

```powershell
git add -A
git commit -m "test(smoke): run the whole stack with Entra ID sign-in only"
```

### Task 15: The guide, documentation and status

**Files:**
- Create: `docs\entra-sign-in.md`
- Modify: `README.md`, `docs\superpowers\plans\2026-10-11-roadmap.md`, `docs\superpowers\specs\2026-10-19-entra-sign-in-design.md`

- [ ] **Step 1: Write `docs\entra-sign-in.md`.**

````markdown
# Microsoft Entra ID sign-in

push-messenger can let users sign in with Microsoft Entra ID instead of (or beside) an
email address and password. The app signs the user in with MSAL (authorization code with
PKCE), gets an access token for push-messenger's API and gives it to auth-svc, which
answers with the same registration token as an email/password sign-in. Devices, access and
refresh tokens and the gateway work as before. Design: [spec 2026-10-19](superpowers/specs/2026-10-19-entra-sign-in-design.md).

auth-svc never sees a password or a client secret and never redeems a code: it only checks
access tokens. The tenant, app registrations, user assignment and Conditional Access are
yours.

## Modes

| `PUSH_AUTH_PROVIDERS` | What users can do |
|---|---|
| `password` (default) | Today's email and password sign-up and sign-in |
| `entra` | Entra ID only: the signup, verification, login and password-reset routes answer 404; no email is ever written, so the worker runs with `PUSH_EMAIL_SENDER=none` and no email provider is needed |
| `password,entra` | Both; the two kinds of accounts are never linked, even with the same address |

Switching an existing installation to `entra`: email/password devices keep working until
their refresh tokens expire (30 days) or you revoke them; those users cannot sign in again.

## 1. Register the API

In the Microsoft Entra admin center, **App registrations → New registration**:

1. Name: `push-messenger API`; supported account types: **Accounts in this organizational
   directory only** (single tenant). No redirect URI.
2. **Expose an API**: set the Application ID URI (`api://<application-id>` is fine) and add
   a scope `access_as_user` ("Access push-messenger as the signed-in user"), who can
   consent: admins only or admins and users, as your policy says.
3. **Manifest**: set `"requestedAccessTokenVersion": 2` (in the new manifest editor:
   `api.requestedAccessTokenVersion`). Without it Entra ID issues v1.0 tokens (issuer
   `https://sts.windows.net/{tid}/`), which auth-svc refuses.
4. Optional: **App roles** (for example `Messenger.User`, allowed member types Users/Groups)
   and in **Enterprise applications → push-messenger API → Properties**, *Assignment
   required* = Yes, then assign the users or groups who may use the messenger. Set
   `PUSH_ENTRA_REQUIRED_ROLE` to the role's value to enforce it in auth-svc too.
5. Do not configure custom signing keys or claims mapping on this registration (auth-svc
   reads the tenant's standard key set).
6. Note the **Application (client) ID** and the **Directory (tenant) ID**.

## 2. Register the client apps

For a browser app: **New registration** `push-messenger web`, single tenant, platform
**Single-page application** with your redirect URIs (for example
`https://chat.example.com/auth/callback`). No client secret, no certificate. **API
permissions → Add → My APIs → push-messenger API → Delegated → access_as_user**, then
**Grant admin consent** if users may not consent themselves.

For desktop and mobile apps: the same, with platform **Mobile and desktop applications**
and the redirect URI MSAL for that platform documents (for example
`msal<client-id>://auth`, or `http://localhost` for desktop). Public client, no secret.

Optional: list the client apps' ids in `PUSH_ENTRA_ALLOWED_CLIENT_APPS` so only they can use
the tokens (`azp`).

Conditional Access policies that target the client apps or the API apply to these sign-ins
as to any other: MFA, compliant devices, named locations.

## 3. Configure auth-svc

| Variable | Value |
|---|---|
| `PUSH_AUTH_PROVIDERS` | `entra` (or `password,entra`) |
| `PUSH_ENTRA_CLIENT_ID` | the API's application id; optionally also its `api://…` URI, comma-separated |
| `PUSH_ENTRA_ALLOWED_TENANTS` | your tenant id; a list for more tenants later. `common`, `organizations` and `consumers` are refused: multi-tenant wildcards are not supported |
| `PUSH_ENTRA_AUTHORITY_HOST` | `https://login.microsoftonline.com/` (default) or your sovereign cloud's host |
| `PUSH_ENTRA_REQUIRED_SCOPE` | `access_as_user` (default) |
| `PUSH_ENTRA_REQUIRED_ROLE` | optional app role value |
| `PUSH_ENTRA_ALLOWED_CLIENT_APPS` | optional client app ids |
| `PUSH_ENTRA_MAX_SESSION_HOURS` | `24` (default), 1 to 720 |

And for the worker in Entra-only mode: `PUSH_EMAIL_SENDER=none`.

## 4. Sign in from a browser (MSAL.js)

```js
import { PublicClientApplication } from '@azure/msal-browser';

const msal = new PublicClientApplication({
  auth: {
    clientId: '<web-client-id>',
    authority: 'https://login.microsoftonline.com/<tenant-id>',
    redirectUri: 'https://chat.example.com/auth/callback',
  },
  cache: { cacheLocation: 'sessionStorage' },
});
await msal.initialize();
const scopes = ['api://<api-client-id>/access_as_user'];

async function entraToken() {
  const result = await msal.handleRedirectPromise();
  const account = result?.account ?? msal.getAllAccounts()[0];
  if (!account) {
    await msal.loginRedirect({ scopes }); // authorization code + PKCE
    return null;
  }
  try {
    return (await msal.acquireTokenSilent({ scopes, account })).accessToken;
  } catch {
    await msal.acquireTokenRedirect({ scopes, account });
    return null;
  }
}

// First sign-in on this browser: a registration token, then a device.
const token = await entraToken();
const signedIn = await fetch('/v1/auth/entra', {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ token, handle: 'erin' }),
}).then((answer) => answer.json());
// POST /v1/devices with Authorization: Bearer <signedIn.registration_token>, as after
// POST /v1/auth/login. Later sign-ins of this device send {token, device_id}.
```

The access token goes to auth-svc only; do not store it beyond MSAL's own cache.

## 5. Sign in from the SDK

```rust
// `token` comes from MSAL for the platform (scope api://<api-client-id>/access_as_user).
let account = client
    .sign_in_with_entra(&token, Some("erin"), "Erin's laptop", Platform::Desktop)
    .await?;
client.connect()?;
```

Node: `await client.signInWithEntra(token, 'erin', 'Erin laptop', 'desktop')`. C:
`push_client_sign_in_with_entra(client, token, "erin", "Erin laptop", "desktop", &account,
&error)`. The device is kept like an email/password sign-in: the same device signs in
again, a new one is registered only when needed.

## 6. Sessions and offboarding

An Entra-backed device stays signed in for `PUSH_ENTRA_MAX_SESSION_HOURS` from its last
Entra ID sign-in. After that, refresh answers 401 `session_expired`, the SDK reports
`SignedOut`, and the app gets a new token from MSAL and signs in again (same device, same
data). A user disabled or deleted in Entra ID cannot get a new token, so their access ends
at most `PUSH_ENTRA_MAX_SESSION_HOURS` plus 30 minutes (the last access token's lifetime)
after their last sign-in. To end it at once, revoke their devices (`DELETE
/v1/devices/{id}` from one of their devices) or, as an operator, in `push_auth`:

```sql
UPDATE auth.device SET revoked_at = now()
WHERE user_id = (SELECT user_id FROM auth.external_identity
                 WHERE provider = 'entra' AND tenant_id = '<tid>' AND subject = '<oid>')
  AND revoked_at IS NULL;
```

and restart nothing: the relayed `device.revoked` closes their sockets.

Users are identified by tenant and object id, never by name or address: a renamed user, a
new UPN or a changed address is the same account; someone else with the same address is
another account.

## 7. Network

auth-svc fetches, per allowed tenant, `https://login.microsoftonline.com/<tid>/v2.0/.well-known/openid-configuration`
and the key set it names (`https://login.microsoftonline.com/<tid>/discovery/v2.0/keys`), at
start-up, about daily and when Microsoft rotates keys. Allow HTTPS (443) from auth-svc to
`login.microsoftonline.com` (FQDN rules in your firewall, or Microsoft's
`AzureActiveDirectory` service tag ranges), or set `HTTPS_PROXY` (and `NO_PROXY` for
in-cluster names) on auth-svc. Nothing else calls Entra ID. When Entra ID cannot be reached
and no keys are cached, Entra sign-ins answer 503 `entra_unavailable`; refresh, devices and
messaging keep working.

## 8. Troubleshooting

auth-svc logs every refused token as `auth.entra.rejected` with a reason (never the token):

| Reason | Usual cause |
|---|---|
| `issuer` | v1.0 tokens: set `requestedAccessTokenVersion` to 2 on the API registration |
| `audience` | the token is for another API (Graph), or an ID token was sent; request `api://<api-client-id>/access_as_user` |
| `tenant` | the user's tenant is not in `PUSH_ENTRA_ALLOWED_TENANTS` |
| `scope` | the token lacks `PUSH_ENTRA_REQUIRED_SCOPE` |
| `app_only` | a client-credentials (application) token: user sign-in is required |
| `role` / `client_app` | the user lacks the app role / the client app is not allowed |
| `expired`, `not_yet_valid` | clock skew over 60 s, or a stale token: get a new one |
| `signature` | a key auth-svc cannot find, or custom signing keys on the API registration |

## 9. Acceptance on your tenant

1. With `PUSH_AUTH_PROVIDERS=entra`: `POST /v1/auth/login` answers 404.
2. A user signs in from the browser app and the SDK, registers, chats with another user.
3. A Graph token, an ID token and a token from another tenant are refused (`entra_token_invalid`).
4. The same user from a second browser gets the same account; after a UPN change too.
5. With `PUSH_ENTRA_MAX_SESSION_HOURS=1`: after an hour without signing in, the app reports
   `SignedOut`; a new sign-in restores the same device.
6. Disable a test user in Entra ID: within the maximum session age plus 30 minutes their
   apps are signed out and cannot sign in again.
7. auth-svc's logs contain no access token, UPN or address.
````

- [ ] **Step 2: README.** In the SDK paragraph of `README.md` ("A `Client` signs up and in
through auth-svc"), add after the code block: "Where auth-svc offers Microsoft Entra ID
sign-in, `client.sign_in_with_entra(token, handle, device_name, platform)` takes an access
token the app got with MSAL instead ([guide](docs/entra-sign-in.md))." In "Deploying it" add:
"Microsoft Entra ID sign-in (opt-in, also Entra-only without email): [docs/entra-sign-in.md](docs/entra-sign-in.md)
(chart values arrive with Plan 13B)."

- [ ] **Step 3: Roadmap and spec status.** In the roadmap set the 13A row's status to
`In progress; [spec](../specs/2026-10-19-entra-sign-in-design.md), [plan](2026-10-19-plan-13a-entra-sign-in.md)`
(at merge time `Done (#N)`), and in the spec's status line: `**13A** in progress`.

- [ ] **Step 4: Commit.**

```powershell
git add -A
git commit -m "docs: add the Entra ID sign-in guide and record Plan 13A"
```

### Task 16: Verification and delivery

- [ ] **Step 1: The full checks.**

```powershell
$env:CARGO_INCREMENTAL = '0'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p push-auth --all-targets --features testing --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 build --workspace --locked
cargo tree -i openssl-sys --workspace
cargo tree -i aws-lc-rs --workspace
cargo tree -i rsa --workspace -e normal,dev
pwsh deploy/helm/test-chart.ps1
git diff main --stat -- Cargo.lock
```

Expected: no formatting change, no warning, every test passes (including
`push-auth`'s `tests::entra_*` and `push-smoke`'s `entra_sign_in`), the MSRV build succeeds,
the three `cargo tree` calls report no such package (no OpenSSL, no aws-lc-rs, and no `rsa`
built), the chart checks pass unchanged (AC1: nothing in `deploy\` changed), and
`Cargo.lock` is unchanged or changes only by push-auth's dependency entry for `reqwest`.

- [ ] **Step 2: The supply-chain check.** Download cargo-deny as in PR #23 (the release
binary from EmbarkStudios/cargo-deny, its `.sha256` verified) and run `cargo deny check`.
Expected: `advisories ok, bans ok, licenses ok, sources ok`.

- [ ] **Step 3: Bindings.** `cargo test -p push-sdk-c` and, in `bindings\node`, `npm ci`,
`npm run build`, `npm test`, `npm audit --omit=dev`. Expected: all pass, no advisories.

- [ ] **Step 4: Password mode on minikube.** On an owned profile (never the shared `push`
profile; restore the kubectl context to `push` afterwards):
`deploy/minikube/up.ps1 -Cluster push13a-local -Smoke`. Expected: the smoke test passes with
the default (password) configuration; existing data is kept. Stop the profile.

- [ ] **Step 5: Review.** Ask for a code review of `git diff main...HEAD` (the
`superpowers:code-reviewer` agent with this plan and the spec); fix what it finds with tests.

- [ ] **Step 6: Deliver** (the repository's standing workflow): set the roadmap row 13A to
`Done (#N)` with the PR number and the spec's status to `**13A** done, #N`, push, open the
PR, wait for CI, squash-merge with the subject `Microsoft Entra ID sign-in (Plan 13A) (#N)`,
switch to `main` and pull, delete the branch.

## Self-review against the spec

| Spec item | Task |
|---|---|
| D1 jsonwebtoken 9 (ring) and reqwest over rustls, no new crate | 1, 4, 5; 16 (`cargo tree`, lockfile) |
| D2 v2.0 access tokens for this API only: RS256 + kid, issuer per tenant, audiences, exp/nbf ±60 s, ver, scp, idtyp, oid; optional role and client apps | 2, 5 |
| D3 explicit tenant GUIDs; `common`/`organizations`/`consumers`/MSA/nil refused | 1 |
| D4 metadata + JWKS on the authority host; 24 h cache; unknown-kid refetch with cooldown; 10 s timeouts; 256 KiB; no redirects; stale-if-error; fail closed 503; pre-check before fetching; warm at start without blocking readiness; proxies honoured | 4, 5, 9, 10 |
| D5 `auth.external_identity`, `(provider, tenant_id, subject)` and `user_id` unique; accounts without email/password, active; no linking; cleanup guard | 6, 8 |
| D6 JIT provisioning with `user.verified`; optional handle or `u…`; display name from `name`; informational email | 8 |
| D7 registration tokens bound by `xid` + `sx`; exactly one binding; provider must be enabled | 7, 8 |
| D8 `session_expires_at`, `session_expired`, refresh tokens capped, renewal on the same device | 6, 8, 9 |
| D9 `PUSH_AUTH_PROVIDERS`, routes absent, worker `none` | 1, 9, 10, 11 |
| D10 `POST /v1/auth/entra`, error codes, rate limits | 7, 9 |
| D11 security events and log hygiene | 5, 8, 9, 14 |
| D12 SDK and bindings | 12, 13 |
| D13 stub issuer, fixed test keys, loopback-only `http://` | 1, 3 |
| D14 chart in Plan 13B | Plan 13B |
| AC1 default unchanged | 1 (config default), 9 (404 for `/v1/auth/entra`), existing tests throughout, 16 (chart checks, minikube password smoke) |
| AC2 Entra-only: 404 routes, no email, sign-in to chat | 9, 10, 11, 14 |
| AC3 refusals: audience, tenant, expiry, signature, algorithm, scope, app-only, ID token | 2, 5, 9 |
| AC4 same `(tid, oid)` same user; UPN/email changes; no linking by address | 2, 8 |
| AC5 maximum session age and renewal | 8, 9, 12 |
| AC6 rotation without restart; outage fails closed | 4, 5, 9 |
| AC7 unit and integration tests with a local stub | 1 to 14 |

Placeholder scan: no "TBD", "TODO" or "similar to" steps; every code step is complete. Type
consistency: `EntraSignIn::{new, with_timing, verify, warm, max_session}`, `EntraIdentity`,
`EntraError::{Rejected, Unavailable}`, `Reason`, `KeyTiming`, `SignIn { password, entra }`,
`Auth::{with_sign_in, password_enabled, entra, sign_in_entra, sign_in_entra_device}`,
`EntraRegistration { user_id, registration }`, `LinkClaims { xid, sx }`,
`AuthError::{EntraTokenInvalid, EntraUnavailable, SessionExpired}`,
`Action::{EntraSignIn, EntraAccount}`, `Sender::Off`, `Jobs::without_email`,
`Client::sign_in_with_entra`, `AuthApi::{entra, entra_device}`, `WRONG_ENTRA_TOKEN`,
`StubOpenId::{start, with_authority, settings, variables, user_claims, token}` are used with
the same names and signatures in every task that refers to them.
