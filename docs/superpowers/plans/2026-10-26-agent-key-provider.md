# Agent-key provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the opt-in `agent-key` authenticator. An owned agent signs in with a
single-use ES256 JWS made with its own P-256 key, which it binds by redeeming the
owner's enrollment code.

**Architecture:** A pure verifier (`agent_key/jws.rs`) checks the token against strict
rules and computes the RFC 7638 thumbprint. An `Authenticator` (`agent_key/mod.rs`) adds
a PostgreSQL replay cache. Accounts reuse the provider-generic external sign-in and a
generalized `enroll_agent`. Config, server wiring, audit, the Helm chart and the docs
follow the `entra-app` pattern.

**Tech Stack:** Rust 2024 (MSRV 1.85); ring 0.17 (`ECDSA_P256_SHA256_FIXED`), which is
already a dependency; base64 0.22; sha2; sqlx/PostgreSQL 17; axum; Helm.

Spec: `docs/superpowers/specs/2026-10-26-agent-key-provider-design.md` (D1–D12).

## Conventions every task must follow

- Repo: `C:\Users\yurio\Documents\github\push-messenger`, branch
  `feat/agent-key-provider`. Never commit `.github/copilot/`.
- Windows PowerShell. Tests need Docker (testcontainers postgres:17-alpine).
- After adding or changing a migration, run `cargo clean -p push-auth-core` before
  testing: sqlx embeds migrations at compile time and nothing forces a rebuild.
- New SQL migration files must have LF line endings, or the SQLx checksum differs.
  Write them with LF only, then check with `git ls-files --eol`.
- Never edit migrations 0001–0008.
- Focused tests: `cargo test -p push-auth-core --locked -j 4 -- <filter>`.
- Gates before each commit: `cargo fmt --all`, then
  `cargo clippy -p push-auth-core --all-targets --locked -j 4 -- -D warnings`.
- Conventional commit messages, each ending with the trailer
  `Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>`.
- Comments only where needed. Match the crate's doc-comment style.

## File map

- Create `crates/push-auth-core/src/agent_key/jws.rs`: the pure verifier and thumbprint.
- Create `crates/push-auth-core/src/agent_key/mod.rs`: `AgentKeySettings`,
  `AgentKeySignIn`, the replay cache, and a `#[cfg(test)] pub(crate) mod testing` that
  holds the test signing key.
- Create `crates/push-auth-core/migrations/0009_agent_key.sql`.
- Create `crates/push-auth-core/src/tests/agent_key.rs`: the integration tests.
- Modify `identity/mod.rs` (constants), `lib.rs` (module and exports), `service.rs`
  (`with_agent_key`, `authenticators`, `agent_key_offered`), `external.rs` (no account
  on first sign-in), `agents.rs` (generic enrollment, nullable tenant, owner, listing),
  `entra/settings.rs` (`Providers.agent_key`), `config.rs`, `server.rs`, `audit.rs`,
  and `tests/mod.rs`.
- Chart: `deploy/helm/push-messenger/values.yaml`, `templates/_helpers.tpl`,
  `deploy/helm/test-chart.ps1`.
- Docs: `docs/agent-key-sign-in.md` (new), `docs/entra-sign-in.md`,
  `docs/deployment-configuration.md`, `CHANGELOG.md`, `README.md` if it lists providers.

---

### Task 1: The pure verifier and thumbprint

**Files:**
- Create: `crates/push-auth-core/src/agent_key/jws.rs`
- Create: `crates/push-auth-core/src/agent_key/mod.rs` (only `mod jws;` and the
  `testing` module for now)
- Modify: `crates/push-auth-core/src/lib.rs` (`mod agent_key;`)

- [ ] **Step 1: Write `jws.rs` with the tests first, the body stubbed with `todo!()`**

The verifier is exactly this (spec D1, D3):

```rust
//! The agent-key assertion (spec 2026-10-26 agent-key provider, D1 and D3): a compact
//! ES256 JWS whose protected header carries the agent's public P-256 key. Pure: the
//! caller gives the time and keeps the replay cache.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The longest assertion accepted, in bytes.
pub(crate) const MAX_TOKEN_BYTES: usize = 4096;
/// The longest lifetime, `exp - iat`, in seconds.
pub(crate) const MAX_LIFETIME_SECONDS: i64 = 300;
/// The clock skew allowed on `iat` and `exp`, in seconds.
pub(crate) const CLOCK_SKEW_SECONDS: i64 = 60;
/// The required `typ` header.
pub(crate) const TOKEN_TYPE: &str = "push-agent+jwt";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    alg: String,
    typ: String,
    jwk: Jwk,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Jwk {
    kty: String,
    crv: String,
    x: String,
    y: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claims {
    aud: String,
    iat: i64,
    exp: i64,
    jti: String,
}

/// A verified assertion: whose key signed it and its single-use id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assertion {
    /// The RFC 7638 SHA-256 thumbprint of the signing key, base64url (43 characters).
    pub thumbprint: String,
    pub jti: String,
    pub exp: i64,
    pub claims: Map<String, Value>,
}

/// Checks `token` at `now` (seconds since the epoch) for `audience`; the reason of a
/// refusal is a short constant for the security log.
pub(crate) fn verify(token: &str, audience: &str, now: i64) -> Result<Assertion, &'static str> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err("size");
    }
    let mut parts = token.split('.');
    let (Some(header_part), Some(payload_part), Some(signature_part), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("format");
    };
    let header: Header =
        serde_json::from_slice(&decode(header_part).ok_or("format")?).map_err(|_| "header")?;
    if header.alg != "ES256" {
        return Err("algorithm");
    }
    if header.typ != TOKEN_TYPE {
        return Err("type");
    }
    let Jwk { kty, crv, x, y } = header.jwk;
    if kty != "EC" || crv != "P-256" {
        return Err("key");
    }
    let coordinate = |value: &str| decode(value).filter(|bytes| bytes.len() == 32);
    let (x, y) = (coordinate(&x).ok_or("key")?, coordinate(&y).ok_or("key")?);
    let signature = decode(signature_part)
        .filter(|bytes| bytes.len() == 64)
        .ok_or("signature")?;
    let mut point = Vec::with_capacity(65);
    point.push(0x04);
    point.extend_from_slice(&x);
    point.extend_from_slice(&y);
    // ring refuses a point off the curve or at infinity, and r or s outside 1..n.
    let signed = &token[..header_part.len() + 1 + payload_part.len()];
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &point)
        .verify(signed.as_bytes(), &signature)
        .map_err(|_| "signature")?;
    let payload = decode(payload_part).ok_or("format")?;
    // The struct refuses duplicate and unknown members; the map keeps them for admission.
    let claims: Claims = serde_json::from_slice(&payload).map_err(|_| "claims")?;
    let map: Map<String, Value> = serde_json::from_slice(&payload).map_err(|_| "claims")?;
    if !is_jti(&claims.jti) {
        return Err("claims");
    }
    if claims.aud != audience {
        return Err("audience");
    }
    if claims
        .exp
        .checked_sub(claims.iat)
        .is_none_or(|lifetime| lifetime <= 0 || lifetime > MAX_LIFETIME_SECONDS)
    {
        return Err("lifetime");
    }
    if claims.iat > now.saturating_add(CLOCK_SKEW_SECONDS) {
        return Err("not_yet_valid");
    }
    if claims.exp <= now.saturating_sub(CLOCK_SKEW_SECONDS) {
        return Err("expired");
    }
    Ok(Assertion {
        thumbprint: thumbprint(&x, &y),
        jti: claims.jti,
        exp: claims.exp,
        claims: map,
    })
}

/// The RFC 7638 thumbprint of the P-256 key `(x, y)`: SHA-256 over the required members
/// in lexicographic order, without white space, base64url without padding.
pub(crate) fn thumbprint(x: &[u8], y: &[u8]) -> String {
    let canonical = format!(
        r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
        URL_SAFE_NO_PAD.encode(x),
        URL_SAFE_NO_PAD.encode(y)
    );
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

/// Strict base64url: no padding, no other alphabet, canonical trailing bits.
fn decode(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(value).ok()
}

/// 16 to 128 characters of `A-Z a-z 0-9 - _ .`.
fn is_jti(jti: &str) -> bool {
    (16..=128).contains(&jti.len())
        && jti
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
```

The test helper goes in `agent_key/mod.rs`:

```rust
mod jws;

#[cfg(test)]
pub(crate) mod testing {
    //! A P-256 signing key for tests and the assertions it makes.

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    use serde_json::{Value, json};

    pub(crate) struct TestKey {
        pair: EcdsaKeyPair,
    }

    impl TestKey {
        pub(crate) fn generate() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            let pair =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            Self { pair }
        }

        /// The public JWK `{kty, crv, x, y}`.
        pub(crate) fn jwk(&self) -> Value {
            let point = self.pair.public_key().as_ref();
            json!({"kty": "EC", "crv": "P-256",
                "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
                "y": URL_SAFE_NO_PAD.encode(&point[33..65])})
        }

        pub(crate) fn thumbprint(&self) -> String {
            let point = self.pair.public_key().as_ref();
            super::jws::thumbprint(&point[1..33], &point[33..65])
        }

        /// A valid assertion for `audience`, issued now, valid for two minutes.
        pub(crate) fn assertion(&self, audience: &str) -> String {
            let now = crate::service::unix_now();
            self.sign(
                &self.header(),
                &json!({"aud": audience, "iat": now, "exp": now + 120,
                    "jti": uuid::Uuid::new_v4().simple().to_string()}),
            )
        }

        /// The standard header: ES256, push-agent+jwt and this key's JWK.
        pub(crate) fn header(&self) -> Value {
            json!({"alg": "ES256", "typ": "push-agent+jwt", "jwk": self.jwk()})
        }

        /// `header` and `claims` as JSON, signed with this key.
        pub(crate) fn sign(&self, header: &Value, claims: &Value) -> String {
            self.sign_raw(&header.to_string(), &claims.to_string())
        }

        /// Raw header and claims texts (for duplicate members), signed with this key.
        pub(crate) fn sign_raw(&self, header: &str, claims: &str) -> String {
            let signed = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(header),
                URL_SAFE_NO_PAD.encode(claims)
            );
            let signature = self
                .pair
                .sign(&SystemRandom::new(), signed.as_bytes())
                .unwrap();
            format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
        }
    }
}
```

Tests in `jws.rs` (`#[cfg(test)] mod tests`), with
`const AUD: &str = "https://api.example.com";` and `now = 1_800_000_000`. Build claims as
`json!({"aud":AUD,"iat":now,"exp":now+120,"jti":"abcdefghijklmnop0123"})`:

1. `thumbprint_matches_an_independent_vector`: the P-256 generator point
   x=`6B17D1F2…C296`, y=`4FE342E2…51F5` (hex, decode with a small helper) gives
   `xx0BcA-wMohw8atYDJOe6peGModklG2wRHBlXHMvl0M`. The vector was computed with Python
   hashlib over `{"crv":"P-256","kty":"EC","x":"axfR8uEsQkf4vOblY6RA8ncDfYEt6zOg9KE5RdiYwpY","y":"T-NC4v4af5uO5-tKfA-eFivOM1drMV7Oy7ZAaDe_UfU"}`.
   Simpler: decode those two base64url strings and call `thumbprint`.
2. `a_valid_assertion_names_its_key`: verify gives `thumbprint == key.thumbprint()`,
   which is 43 characters, plus `jti` and `exp`, and `claims["aud"] == AUD`. The edges
   pass too: `exp - iat == 300`; `iat == now + 60`; `exp == now - 59`.
3. `refusals_have_their_reasons`: a table of `(token, reason)`:
   - aud `"https://evil.example"` gives `audience`; aud `[AUD]` gives `claims`
   - `exp = iat + 301` gives `lifetime`; `exp == iat` gives `lifetime`;
     `iat = i64::MIN, exp = i64::MAX` gives `lifetime` (no overflow panic)
   - `iat = now-200, exp = now-60` gives `expired`; `iat = now+61, exp = now+120` gives
     `not_yet_valid`
   - header alg `none` gives `algorithm`; `HS256` gives `algorithm`; `ES384` gives
     `algorithm`. For `alg: none` also test the empty-signature form
     `"<h>.<p>."`: it is refused (reason `algorithm`).
   - typ `JWT` gives `type`
   - header without `jwk` gives `header`; header with extra `kid`, `crit` (`["exp"]`),
     `jku` or `x5u` gives `header`
   - jwk with `d` (any 43-character string) gives `header`; jwk with an extra `kid`
     gives `header`; `crv: "P-384"` gives `key`; `kty: "OKP"` gives `key`
   - x of 31 bytes (base64url of 31 zero bytes) gives `key`
   - an off-curve point (x = 32 bytes of 0x01, y = 32 bytes of 0x02) gives
     `signature`
   - a DER signature: replace the signature part with base64url of 70 bytes. Gives
     `signature`.
   - signed by another key while the header carries `key`'s JWK gives `signature`
   - tampered payload (re-encode other claims, keep the signature) gives `signature`
   - extra claim `"sub":"x"` gives `claims`; duplicate claim via `sign_raw`
     (`{"aud":…,"aud":…,…}`) gives `claims`; duplicate header member via `sign_raw`
     gives `header`
   - jti `"short"` gives `claims`; a jti with a space gives `claims`; `iat` as `1.5`
     gives `claims`
   - 4097-byte token gives `size`; two parts gives `format`; four parts gives
     `format`; a header part ending in `=` gives `format`

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p push-auth-core --locked -j 4 -- agent_key::jws`
Expected: the tests panic at `todo!()`.

- [ ] **Step 3: Replace `todo!()` with the body above**

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p push-auth-core --locked -j 4 -- agent_key::jws`
Expected: all pass. Fix any reason mismatch in the test table only if the spec's order
of checks explains it (the header is parsed before the signature is checked; the
signature before the claims).

- [ ] **Step 5: fmt, clippy, commit**
`feat(auth): verify agent-key ES256 assertions and their key thumbprint`

---

### Task 2: The authenticator, the replay cache and migration 0009

**Files:**
- Create: `crates/push-auth-core/migrations/0009_agent_key.sql` (LF line endings)
- Modify: `agent_key/mod.rs`, `identity/mod.rs`, `service.rs`, `lib.rs`,
  `tests/mod.rs`
- Create: `crates/push-auth-core/src/tests/agent_key.rs`

- [ ] **Step 1: The migration**

```sql
-- Agent-key provider (REQUEST PUSH feature 6, spec 2026-10-26 agent-key provider).
-- A reservation made while only agent-key can redeem it has no tenant (D5); entra-app
-- refuses such a reservation.
ALTER TABLE auth.agent_enrollment ALTER COLUMN tenant_id DROP NOT NULL;

-- Single-use agent-key assertions (D2): each (thumbprint, jti) once, kept until its
-- expiry plus the clock skew, then deleted by later sign-ins.
CREATE TABLE auth.agent_key_assertion (
    thumbprint text NOT NULL CHECK (length(thumbprint) = 43),
    jti        text NOT NULL CHECK (length(jti) BETWEEN 16 AND 128),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (thumbprint, jti)
);
CREATE INDEX agent_key_assertion_expiry ON auth.agent_key_assertion (expires_at);
```

Check whether `db.rs` or `grant_worker_access_on` lists tables explicitly. The runtime
role gets schema-wide grants through `push_store::grant_runtime_access`; confirm this
by reading it. If grants are per table, add the new table.

- [ ] **Step 2: Constants in `identity/mod.rs`**

```rust
/// The opt-in, non-provisioning agent key-pair provider (spec 2026-10-26).
pub const AGENT_KEY_PROVIDER: &str = "agent-key";
/// The providers whose identities only an owner's enrollment binds: never a human, and
/// never created on a first sign-in.
pub const AGENT_PROVIDERS: &[&str] = &[ENTRA_APP_PROVIDER, AGENT_KEY_PROVIDER];
```

Also update the `Authenticator` trait docs, which list the identity types, to mention
`agent-key`.

- [ ] **Step 3: `AgentKeySettings` and `AgentKeySignIn` in `agent_key/mod.rs`**

```rust
//! The opt-in `agent-key` provider (spec 2026-10-26 agent-key provider): an owned agent
//! proves possession of the P-256 key it bound at enrollment with a single-use ES256
//! assertion. Push-messenger keeps only the key's thumbprint.

use std::time::Duration;

use sqlx::PgPool;

use crate::identity::{AGENT_KEY_PROVIDER, AuthenticationError, Authenticator, Principal};

mod jws;

/// Policy of the explicitly enabled `agent-key` provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentKeySettings {
    /// The `aud` every assertion must carry, e.g. the public API URL.
    pub audience: String,
    pub max_session: Duration,
}

/// The `agent-key` authenticator; its replay cache is the `auth` database.
#[derive(Debug, Clone)]
pub struct AgentKeySignIn {
    settings: AgentKeySettings,
    pool: PgPool,
}

impl AgentKeySignIn {
    /// Refuses an audience that is not 1 to 256 visible ASCII characters and a session
    /// outside 1 to 720 hours.
    pub fn new(settings: AgentKeySettings, pool: PgPool) -> Result<Self, String> {
        if !is_audience(&settings.audience) {
            return Err("the agent-key audience must be 1 to 256 visible ASCII characters".to_owned());
        }
        if settings.max_session < Duration::from_secs(3600)
            || settings.max_session > Duration::from_secs(720 * 3600)
        {
            return Err("the agent-key maximum session must be 1 to 720 hours".to_owned());
        }
        Ok(Self { settings, pool })
    }

    /// Records the assertion's first use; `false` for a replay.
    async fn first_use(&self, assertion: &jws::Assertion) -> Result<bool, sqlx::Error> {
        sqlx::query("DELETE FROM auth.agent_key_assertion WHERE expires_at < now()")
            .execute(&self.pool)
            .await?;
        let inserted = sqlx::query(
            "INSERT INTO auth.agent_key_assertion (thumbprint, jti, expires_at) \
             VALUES ($1, $2, to_timestamp($3)) ON CONFLICT DO NOTHING RETURNING 1",
        )
        .bind(&assertion.thumbprint)
        .bind(&assertion.jti)
        .bind((assertion.exp + jws::CLOCK_SKEW_SECONDS) as f64)
        .fetch_optional(&self.pool)
        .await?;
        Ok(inserted.is_some())
    }
}

pub(crate) fn is_audience(value: &str) -> bool {
    (1..=256).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_graphic())
}

#[async_trait::async_trait]
impl Authenticator for AgentKeySignIn {
    fn provider(&self) -> &str {
        AGENT_KEY_PROVIDER
    }

    fn max_session(&self) -> Duration {
        self.settings.max_session
    }

    async fn authenticate(&self, token: &str) -> Result<Principal, AuthenticationError> {
        let assertion = jws::verify(token, &self.settings.audience, crate::service::unix_now())
            .map_err(AuthenticationError::Rejected)?;
        match self.first_use(&assertion).await {
            Ok(true) => {}
            Ok(false) => return Err(AuthenticationError::Rejected("replay")),
            Err(error) => {
                tracing::warn!(%error, "an agent-key sign-in failed: the replay cache is unreachable");
                return Err(AuthenticationError::Unavailable(
                    "the agent-key replay cache is unreachable".to_owned(),
                ));
            }
        }
        Ok(Principal {
            provider: AGENT_KEY_PROVIDER.to_owned(),
            issuer: AGENT_KEY_PROVIDER.to_owned(),
            subject: assertion.thumbprint,
            name: None,
            email: None,
            claims: assertion.claims,
        })
    }
}
```

`to_timestamp(double precision)` is a PostgreSQL function. If the f64 cast is a
problem, bind an `i64` and use `to_timestamp($3::bigint)`; either is fine. `exp` is at
most `now + 360`, because `iat <= now + 60` and the lifetime is at most 300 s, so the
cast loses nothing.

`lib.rs`: add `pub mod agent_key;`. Export `AgentKeySettings` and `AgentKeySignIn`
through `pub use agent_key::{AgentKeySettings, AgentKeySignIn};`. Add
`AGENT_KEY_PROVIDER` to the `identity` re-export line.

- [ ] **Step 4: `service.rs`**

Add the field `agent_key: Option<crate::agent_key::AgentKeySignIn>` to `Identity`. Then:

```rust
/// Offers agent key-pair sign-in for enrolled agents; never provisions an account.
pub fn with_agent_key(mut self, authenticator: crate::agent_key::AgentKeySignIn) -> Self {
    self.identity.agent_key = Some(authenticator);
    self
}

/// Whether `agent-key` is offered.
pub(crate) fn agent_key_offered(&self) -> bool {
    self.identity.agent_key.is_some()
}
```

In `authenticators()`, push the agent key after `entra_app` and before `external`. Fix
the doc comment, which says "Entra ID first, then the external one", to list the order.

- [ ] **Step 5: Integration tests, file `tests/agent_key.rs`**

Register `mod agent_key;` in `tests/mod.rs`. Add a setup helper:

```rust
pub(super) const KEY_AUDIENCE: &str = "http://push.local";

/// A world offering password sign-in and agent-key only (no entra-app), with its router.
pub(super) async fn key_world() -> (World, Auth, Router) {
    let world = world(Offer::Password).await;
    let auth = world.auth.clone().with_agent_key(
        AgentKeySignIn::new(
            AgentKeySettings { audience: KEY_AUDIENCE.to_owned(), max_session: Duration::from_secs(3600) },
            world.pool.clone(),
        )
        .unwrap(),
    );
    let router = router(Api { auth: auth.clone(), limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: None, directory: Arc::new(NoDirectory) });
    (world, auth, router)
}
```

Tests in this task, at the authenticator level:
- `a_jti_is_single_use_per_key`: `auth.authenticator("agent-key").unwrap().authenticate(&t)`
  gives a principal with provider and issuer `agent-key`, and the subject
  `key.thumbprint()`. The same token again gives `Rejected("replay")`. Another key with
  the same jti (craft both with `sign` and an explicit jti) is accepted, because the key
  is `(thumbprint, jti)`. Expired rows are deleted: insert a row with
  `expires_at = now() - interval '1 second'`, authenticate any new token, and the row
  is gone.
- `agent_key_settings_are_checked`: `AgentKeySignIn::new` refuses audience `""`,
  `"a b"`, a 257-character string, and max sessions of 59 min and 721 h.

- [ ] **Step 6: Run the tests**

Run `cargo clean -p push-auth-core`, then
`cargo test -p push-auth-core --locked -j 4 -- agent_key`.
Expected: pass.

- [ ] **Step 7: fmt, clippy, commit**
`feat(auth): add the agent-key authenticator with a durable replay cache`

---

### Task 3: Accounts: no account on first sign-in, generic enrollment, owners, listing

**Files:** `external.rs`, `agents.rs`, `tests/agent_key.rs`, and `tests/agents.rs`
only where existing assertions change.

- [ ] **Step 1: Write the failing integration tests in `tests/agent_key.rs`**

Use the helpers `password_user`, `call` and a `caller()` function. Copy `call` from
`tests/agents.rs`, or make it `pub(super)` there and reuse it (preferred; DRY). The
tests are:

1. `a_key_enrolls_signs_in_and_another_key_is_refused` (acceptance 1). Owner reserves
   `key_agent` (`auth.reserve_agent`). HTTP `POST /v1/auth/agent-key`
   `{token: K.assertion(KEY_AUDIENCE), enrollment_code}` gives 200 with `user_id` ==
   the reservation, and `owned_agents` shows `state: "active"`,
   `credential: "agent-key"`, `key_thumbprint: K.thumbprint()`, and
   `client_app_id: null`. Then `{token: K.assertion}` gives 200 and a
   registration_token; `register_device` with it works. Then
   `{token, device_id}` with a fresh assertion gives 200 and tokens for that user.
   `{token: K2.assertion}` gives 401 with code `token_invalid`, and the account count
   is unchanged. A token whose header has K's JWK but is signed by K2 (build with
   `K2.sign(&K.header(), claims)`) gives 401.
2. `assertion_refusals_answer_401` (acceptance 2, over HTTP): an aud mismatch,
   `exp-iat = 301`, expired, a reused jti (send the same token twice: the first is 200
   with the code, or use an enrolled key; the second is 401), `alg: none`,
   `alg: HS256`, no `jwk`, a `jwk` with `d`. Each gives 401 `token_invalid`.
3. `one_live_agent_per_key_and_codes_are_single_use` (acceptance 3): reserve A and B,
   enroll A with K, then enroll B with K: 409 `identity_in_use`, and B is still
   pending (the code was not spent: then enroll B with K3 gives 200). An expired code
   (UPDATE `expires_at` into the past) gives 400 `enrollment_invalid`, and so does a
   spent code (A's code again, with a new key K4).
4. `the_route_is_absent_unless_enabled` (acceptance 4): with the router of a plain
   `world(Offer::Password)` (no agent key), `POST /v1/auth/agent-key` gives 404, the
   same status as `POST /v1/auth/no-such-provider`.
5. `deleting_a_key_agent_releases_its_key`: delete the agent, and K gives 401 (no
   account is created; count accounts before and after). The released row exists in
   `auth.released_identity` with provider `agent-key`. A fresh reservation and code
   with K enrolls a new agent.
6. `a_key_agent_is_not_a_human_owner`: the enrolled agent's device caller calls
   `reserve_agent` and gets `EnrollmentNotPermitted`.
7. `reservations_without_entra_app_have_no_tenant`: in the `key_world`, the reservation
   row has `tenant_id IS NULL`. In a world with both providers (build `entra-app` as
   `tests/agents.rs::setup` does, plus `with_agent_key`), use an owner with several
   allowed tenants, or set the reservation's tenant to NULL by SQL: redeeming it with
   an `entra-app` principal gives `EnrollmentNotPermitted`, and redeeming with
   agent-key succeeds. Also check that with both providers on, a single-tenant
   password owner still gets the tenant recorded.

Run: `cargo test -p push-auth-core --locked -j 4 -- tests::agent_key`
Expected: failures. Enrollment refuses agent-key, and reservation needs entra-app.

- [ ] **Step 2: `external.rs::create_external`**

Replace the `ENTRA_APP_PROVIDER` refusal with:

```rust
// Agent identities must be bound by enrollment, never by a first sign-in. An unknown
// key is no vouched-for identity at all: an invalid credential.
if principal.provider == crate::identity::AGENT_KEY_PROVIDER {
    return Err(crate::identity::rejected(&principal.provider, "unenrolled_key"));
}
if crate::identity::AGENT_PROVIDERS.contains(&principal.provider.as_str()) {
    return Err(crate::identity::not_permitted(principal));
}
```

Update the doc of `sign_in_external`: "`entra-app` and `agent-key` are linked-only".

- [ ] **Step 3: `agents.rs`**

- `owner()`: in the SQL, use `AND e.provider IN ('entra-app', 'agent-key')`, with a
  comment pointing at `AGENT_PROVIDERS`.
- `reserve_agent`: replace the first statement with logic that lets `tenants` be
  `Option<&[Uuid]>` from `self.app_tenants()`, and `keys = self.agent_key_offered()`.
  If neither is offered, return `EnrollmentNotPermitted`. Compute
  `tenant: Option<Uuid>` as today when `tenants` is `Some`, otherwise `None`. Then
  `if tenant.is_none() && !keys { return Err(EnrollmentNotPermitted) }`. Bind `tenant`
  (an `Option<Uuid>`) in the INSERT. Run the delegated-issuer query only when `tenants`
  is `Some`.
- `enroll_agent`:
  ```rust
  let provider = principal.provider.as_str();
  if !AGENT_PROVIDERS.contains(&provider) || self.authenticator(provider).is_none() {
      return Err(AuthError::EnrollmentNotPermitted);
  }
  let principal = principal.checked(provider).ok_or(AuthError::Unauthorized)?;
  ```
  - lock key `format!("{}:{}:{}", principal.provider, principal.issuer, principal.subject)`;
  - the exists query binds the provider: `WHERE provider = $1 AND issuer = $2 AND subject = $3`;
  - `pending` gives `(Option<Uuid>, bool)`. After the validity check:
    ```rust
    if principal.provider == ENTRA_APP_PROVIDER
        && tenant.map(|t| t.to_string()).as_deref() != Some(principal.issuer.as_str())
    {
        return Err(AuthError::EnrollmentNotPermitted);
    }
    ```
  - the identity insert binds `&principal.provider` instead of the `'entra-app'` literal;
  - `azp` only `if principal.provider == ENTRA_APP_PROVIDER`, else `None`;
  - the log gains `provider = %principal.provider`, and `tenant_id = ?tenant`.
- `owned_agents`: extend the SELECT with
  `LEFT JOIN LATERAL (SELECT provider, subject FROM auth.external_identity x WHERE x.user_id = a.user_id AND x.provider IN ('entra-app','agent-key') ORDER BY x.created_at DESC LIMIT 1) c ON true`
  and add the JSON fields `"credential": provider` (an `Option<String>`) and
  `"key_thumbprint"`. The thumbprint is the subject when the provider is `agent-key`,
  otherwise null.
- Update the module doc ("releases the app identity" becomes "releases its identity").

- [ ] **Step 4: Run the new and the existing agent tests**

Run: `cargo test -p push-auth-core --locked -j 4 -- tests::agent tests::entra tests::admin`
Expected: all pass. If an existing test asserted the exact JSON key set of an agent,
update it to include the new fields.

- [ ] **Step 5: fmt, clippy, commit**
`feat(auth): enroll owned agents with a key and refuse unknown keys`

---

### Task 4: Config, server wiring, overrides and audit

**Files:** `entra/settings.rs`, `config.rs`, `agent_key/mod.rs`
(`AgentKeySettings::from_lookup`), `server.rs`, `audit.rs`, and the tests in those
files.

- [ ] **Step 1: Failing unit tests**

- `entra/settings.rs`: `Providers::parse(Some("password,agent-key"))` has
  `agent_key: true`; `"agent-key,agent-key"` is an error; the error text lists
  `agent-key`. Add `agent_key: false` to every existing struct literal.
- `config.rs` tests, following the existing `entra-app` test at about line 812:
  - with `PUSH_AUTH_PROVIDERS=password,agent-key`, `config.agent_key ==
    Some(AgentKeySettings { audience: <PUSH_AUTH_PUBLIC_URL without trailing '/'>,
    max_session: 24 h })`;
  - `PUSH_AUTH_AGENT_KEY_AUDIENCE=https://api.example.com/agents` and
    `PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS=720` are taken;
  - invalid values are errors naming the variable: `0`, `721`, `1.5`, an audience with
    a space, and a 257-character audience;
  - without `agent-key`, `agent_key == None` even when the variables are set.
- `server.rs::overrides_and_providers_must_agree`: refuse
  `("external", with("agent-key", hour), "built-in provider")`; accept `"agent-key"` with
  an admission only.
- `audit.rs`: `sign_in_event` (below) gives `auth.agent-key.success` for 200,
  `auth.agent-key.failure` for 401 `token_invalid`, 403, 409 `identity_in_use` and 400
  `enrollment_invalid`, `None` for 400 `invalid` and for 503, and `auth.rate_limited`
  for 429.

- [ ] **Step 2: Implement**

- `Providers`: add `pub agent_key: bool`, parse `"agent-key"`, and update the error
  string and the doc.
- `AgentKeySettings::from_lookup(lookup: &dyn Fn(&str) -> Option<String>, public_url: &str) -> Result<Self, ConfigError>`.
  The audience is the trimmed `PUSH_AUTH_AGENT_KEY_AUDIENCE` if set and non-empty,
  else `public_url.trim_end_matches('/')`; check it with `is_audience`. The hours are
  `PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS`, default 24, a whole number 1..=720. Use
  `ConfigError::Invalid { name, value, reason }` like the others.
- `Config`: add `pub agent_key: Option<AgentKeySettings>`, read only when
  `providers.agent_key`. Add it to `Debug` and to the doc table (two rows), and update
  the `PUSH_AUTH_PROVIDERS` row. Update the doc on `entra/settings.rs`.
- `server.rs::serve_with`: after the pool and before the `Auth` builder chain, if
  `config.agent_key` is `Some`, build `AgentKeySignIn::new(settings.clone(), pool.clone())`
  with `.map_err(anyhow::Error::msg)?`, and chain `.with_agent_key(..)`. Validate early:
  before the datastores, call `crate::agent_key::check(&settings)`, which returns
  `Result<(), String>` and holds the same checks as `new`; let `new` use it too.
  `check_overrides`: refuse an id in `AGENT_PROVIDERS` with the message
  `"{id} is a built-in provider: explicitly enable it in PUSH_AUTH_PROVIDERS, not an Overrides authenticator"`.
  The existing entra-app test matches "built-in provider", so keep that wording. The
  admission condition gains `&& !providers.agent_key`.
- `audit.rs`: generalize
  `external_event(status, code)` into
  `fn sign_in_event(status, code, success: &'static str, failure: &'static str) -> Option<&'static str>`,
  and keep `external_event` as a thin wrapper or replace its call sites. In `audit()`:
  ```rust
  let event = if route == "/v1/auth/agent-key" && api.auth.agent_key_offered() {
      sign_in_event(status, code, "auth.agent-key.success", "auth.agent-key.failure")
  } else if api.auth.is_external_route(&route) { … }
  ```
  Update the module doc.

- [ ] **Step 3: Integration test for audit (in `tests/agent_key.rs`)**

Call `capture_logs()`. A successful enrollment logs a security event with
`event == "auth.agent-key.success"`. A refused token logs `auth.agent-key.failure` and
`auth.external.rejected` with `provider == "agent-key"` and a `reason`. Filter on the
unique user id or route, because the logs are shared.

- [ ] **Step 4: Run**
`cargo test -p push-auth-core --locked -j 4 -- config server audit settings tests::agent_key`
Expected: pass.

- [ ] **Step 5: fmt, clippy (workspace:
`cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`, because
`Providers` is public and other crates may build it), commit**
`feat(auth): configure, serve and audit the agent-key provider`

---

### Task 5: Helm chart

**Files:** `deploy/helm/push-messenger/values.yaml`, `templates/_helpers.tpl`,
`deploy/helm/test-chart.ps1`.

- [ ] **Step 1: Failing chart checks in `test-chart.ps1`**, next to the entra-app
  block at about line 757. Add `Check 'sign-in: agent-key renders its audience and session'`:
  - `--set auth.signIn.providers=password\,agent-key --set auth.signIn.agentKey.audience=https://api.example.com --set auth.signIn.agentKey.maxSessionHours=8`.
    The auth deployment env has `PUSH_AUTH_PROVIDERS` `"password,agent-key"`,
    `PUSH_AUTH_AGENT_KEY_AUDIENCE` `"https://api.example.com"` and
    `PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS` `"8"`.
  - With no audience set, the audience variable is absent (the service defaults to the
    public URL).
  - Refusals (render fails): `maxSessionHours=0`, `721`, `1.5`; `audience=a b`;
    `providers=agent-key\,agent-key`.
  - The default render does not contain `PUSH_AUTH_AGENT_KEY_`.
  Use the default `$signInArgs`, or plain password args if agent-key needs no Entra
  settings. It does not: `agent-key` must not trigger the Entra requirement in
  `_helpers.tpl` line 327.
- [ ] **Step 2: Implement**
  - `values.yaml`: document `agent-key` in the providers comment. Add
    `agentKey: { audience: "", maxSessionHours: 24 }` under `auth.signIn`, with
    comments.
  - `_helpers.tpl`: add `"agent-key"` to the allowed list and to `$providerError`.
    Validate `agentKey.maxSessionHours` (a whole number 1–720, as for entraApp) and
    `agentKey.audience` (a string; if non-empty, matches `^[!-~]{1,256}$`). Render the
    env vars inside `{{- if has "agent-key" (splitList "," $providers) }}`. Check the
    email rule at about line 332/517: "password not in providers means no email" still
    holds for `agent-key` without password. Leave it, but confirm the
    `entra`-specific failure messages still make sense.
- [ ] **Step 3: Run** `pwsh deploy/helm/test-chart.ps1` (needs `helm`). If `helm` is
  missing, run `winget`/`choco` only if it is quick. Otherwise rely on CI's `chart` job
  and say so in the report.
- [ ] **Step 4: Commit** `feat(chart): offer the agent-key provider`

---

### Task 6: Documentation and release notes

**Files:** create `docs/agent-key-sign-in.md`; modify `docs/entra-sign-in.md`,
`docs/deployment-configuration.md`, `CHANGELOG.md` ("Unreleased"), and `README.md`
where it lists providers or docs.

- [ ] **Step 1: `docs/agent-key-sign-in.md`** has these sections:
  1. Why: local agents have no managed identity, and tenant policy forbids secrets.
  2. Enable: `PUSH_AUTH_PROVIDERS=…,agent-key`, `PUSH_AUTH_AGENT_KEY_AUDIENCE`
     (default: public URL), `PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS` (24, 1–720),
     `PUSH_AGENTS_PER_USER`; Helm `auth.signIn.providers` and
     `auth.signIn.agentKey.{audience,maxSessionHours}`. Reservations work with
     `agent-key` alone (NULL tenant; `entra-app` cannot redeem those).
  3. The assertion format: the exact header, claims, rules and limits from spec D1,
     the replay rule (D2), and the thumbprint (D3), with a worked canonical JSON
     example.
  4. Flow: reserve (`POST /v1/agents`), redeem
     (`POST /v1/auth/agent-key {token, enrollment_code}`), register a device, sign in
     again (`{token}` / `{token, device_id}`), errors (401 `token_invalid` including an
     unenrolled key, 409 `identity_in_use`, 400 `enrollment_invalid`, 403
     `enrollment_not_permitted`, 404 when disabled), listing fields `credential` and
     `key_thumbprint`, deletion releasing the key, and the events
     `auth.agent-key.success|failure`, `auth.external.rejected` (`provider=agent-key`,
     `reason`).
  5. Client example (PowerShell 7 / .NET 8 on Windows). Create or open a persisted,
     non-exportable CNG P-256 key in the `Microsoft Platform Crypto Provider` (TPM),
     falling back to `Microsoft Software Key Storage Provider`, with
     `CngKey.Create(CngAlgorithm.ECDsaP256, name, CngKeyCreationParameters{ Provider =
     new CngProvider(...), ExportPolicy = CngExportPolicies.None, KeyCreationOptions =
     CngKeyCreationOptions.None })`, and `CngKey.Open` on later runs. Build the JWK
     from `ECDsaCng.ExportParameters(false)`, which exports the public part only and
     works for non-exportable keys. Sign with
     `SignData(bytes, HashAlgorithmName.SHA256)`. On .NET the default
     `DSASignatureFormat.IeeeP1363FixedFieldConcatenation` is the 64-byte `r||s` that
     JWS needs. Include a base64url helper, the canonical thumbprint computation (so
     the agent can show its thumbprint to the owner), and an `Invoke-RestMethod` call.
     Mark it as an example and keep it correct.
  6. Security notes: strict parsing, key binding by thumbprint, replay in PostgreSQL,
     no JIT accounts, and key rotation not supported yet (delete and re-enroll).
- [ ] **Step 2: `docs/entra-sign-in.md`**: add the `credential` and `key_thumbprint`
  fields to the `GET /v1/agents` table row and a link to the new doc. The custom
  external authenticator paragraph should now say ids `entra-app` and `agent-key` are
  reserved.
- [ ] **Step 3: `docs/deployment-configuration.md`**: add the two variables next to
  the `PUSH_ENTRA_APP_*` rows. Grep for them.
- [ ] **Step 4: `CHANGELOG.md` Unreleased**: an "Added" entry for the agent-key
  provider, plus migration 0009 (makes `agent_enrollment.tenant_id` nullable and adds
  `auth.agent_key_assertion`), and the new listing fields. Follow the existing style.
- [ ] **Step 5: Commit** `docs: document the agent-key provider`

---

## Final verification (controller)

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`
- `cargo test -p push-auth-core --locked -j 4`
- `cargo build --workspace --locked -j 4` (push-auth, push-smoke compile)
- Then push the branch, open the PR, watch CI, and merge.
