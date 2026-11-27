# Entra external token validator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Let `entra`/`entra-app` delegate signature/protocol validation to an opt-in external HTTP validator while push-auth keeps its pre-checks, defensive re-checks, policy, accounts and logging.

**Architecture:** `EntraSettings.validator: Option<ValidatorSettings>` selects the mode. `EntraSignIn` holds either `KeySets` (built-in) or a `Validator` (new `entra/validator.rs`). `verified(token, ProviderKind)` branches after the shared pre-checks; the validator path re-checks returned claims then reuses `EntraClaims` and the policies. Logs gain `validator = true` (an `Option<bool>` field) in validator mode only.

**Tech Stack:** Rust 1.85 (MSRV), reqwest (already a dependency), axum (stub), tracing, Helm 3, PowerShell chart tests.

Design: `docs/superpowers/specs/2026-11-27-entra-external-validator-design.md` (decisions V1–V13). Request: the email saved in the session files.

---

## File map

| File | Responsibility |
|---|---|
| `crates/push-auth-core/src/entra/validator.rs` (new) | `ValidatorSettings` (+ parsing helper `from_lookup`), `ProviderKind`, `Validator` HTTP client (`check(token, kind) -> Result<Map<String, Value>, EntraError>`), `recheck` of returned claims |
| `crates/push-auth-core/src/entra/settings.rs` | `EntraSettings.validator` field, read in `from_lookup` |
| `crates/push-auth-core/src/entra/mod.rs` | `Reason::Validator`, `Reason::from_name`, `EntraSignIn { keys: Option<..>, validator: Option<..> }`, `verified(token, kind)`, logging, unit tests |
| `crates/push-auth-core/src/entra/app.rs` | call `verified(token, ProviderKind::App)`, unavailable log text, tests |
| `crates/push-auth-core/src/entra/testing.rs` | `StubValidator`, `StubOpenId::settings()` sets `validator: None` |
| `crates/push-auth-core/src/entra/claims.rs` | test `settings()` gets `validator: None` |
| `crates/push-auth-core/src/identity/{mod,middleware}.rs`, `api.rs`, `service.rs` | `validator` flag on rejection logs |
| `crates/push-auth-core/src/config.rs` | table rows; test that entra-app-only reads the validator variables |
| `crates/push-auth-core/src/tests/entra_api.rs` | router + log tests in validator mode (Docker) |
| `deploy/helm/push-messenger/{values.yaml,templates/_helpers.tpl,templates/auth.yaml,templates/validate.yaml}`, `deploy/helm/test-chart.ps1` | chart |
| `docs/entra-sign-in.md`, `CHANGELOG.md` | docs |

## Task 1: Settings (V1–V3)

- [x] Write failing tests in `entra/settings.rs` tests: default `validator == None`; `PUSH_ENTRA_VALIDATOR_URL=http://127.0.0.1:8099/validate` → Some with timeout 2000 ms, no secret; `https://validator.example/v1/check` ok; `http://[::1]:9/x`, `http://localhost:9/` ok; refused (ConfigError::Invalid naming the variable): `http://validator.example/`, `ftp://127.0.0.1/`, `https://u:p@h/`, `https://h/#f`, `not a url`; timeout `99`, `10001`, `1.5`, `x` refused, `100` and `10000` ok; secret file: content `"  s3cret \n"` → `Some("s3cret")`; empty/whitespace file refused; missing file refused; error Display never contains the secret; timeout or secret file without URL refused; `Debug` of settings never contains the secret.
- [x] Run `cargo test -p push-auth-core --lib entra::settings` → fails to compile/fails.
- [x] Implement `ValidatorSettings { pub url: String, pub timeout: Duration, pub secret: Option<String> }` in `validator.rs` with `pub(super) fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<Option<Self>, ConfigError>`; add `pub validator: Option<ValidatorSettings>` to `EntraSettings`; fix struct literals (`claims.rs` tests, `testing.rs`). Re-export `ValidatorSettings` from `entra`.
- [x] Tests pass; commit `feat(auth): PUSH_ENTRA_VALIDATOR_* settings`.

## Task 2: Validator client, stub and verified integration (V4–V9, V11, V13)

- [x] `StubValidator` in `testing.rs`: `start().await`, `url()`, `set_answer(Answer)` with `Answer::{Echo, Claims(Value), Status(u16, String), Raw(u16, String), Delay(Duration)}` (`Echo` default: `{"claims": <token payload>}`), `requests() -> Vec<SeenRequest { method, authorization, secret, content_type, accept, body }>`, `settings_for(&StubOpenId) -> EntraSettings` (stub settings + validator to this stub, 1000 ms timeout).
- [x] Failing tests in `entra/mod.rs` (validator mode): delegated token accepted, `Principal` identical to built-in path's; delegated token to app → `app_only` and app token to entra → `app_only`; returned claims with wrong aud / foreign tid (admitted pre-check but claims tid differs → malformed) / tid of another admitted tenant with matching token? (covered by malformed) / wrong iss / expired exp / future nbf → matching reasons; claims oid differs → malformed; claims missing `exp`, `aud` number, `exp` string, `claims` absent → malformed; 401 `{"reason":"signature"}` → signature, 401 empty → validator, 401 `{"reason":"bogus"}` → validator; 403, 500, 302, delay > timeout, connection refused, 17 KiB 200, non-JSON 200 → Unavailable, and `StubOpenId` saw zero metadata/JWKS fetches; request shape (POST, `Bearer <token>`, body `{"provider":"entra"}`/`"entra-app"`, JSON content type and accept, secret header only when configured); `warm()` fetches nothing; header pre-checks still refuse `alg: none` without a validator call; each sign-in calls the validator once (no caching).
- [x] Implement per design: `Reason::Validator`, `Reason::from_name`, `ProviderKind`, `Validator::new(&ValidatorSettings)`, `Validator::check`, claims re-check function, `EntraSignIn { keys: Option<Arc<KeySets>>, validator: Option<Arc<Validator>> }`, `verified(token, kind)`, app.rs caller, logs with `validator` `Option<bool>` and validator-specific unavailable message.
- [x] `cargo test -p push-auth-core --lib entra` passes; commit.

## Task 3: Rejection log flag through the router (V10)

- [x] `Auth::external_validation(provider) -> bool`; `IdentityState.validator`; `identity::rejected_by(provider, reason, validator)` (and `rejected` delegating with `false`).
- [x] Docker test in `tests/entra_api.rs`: world with validator-mode Entra; sign-in 200; refused token → 401 `entra_token_invalid`, security event `auth.entra.rejected` with `validator == true`; validator 500 → 503 `entra_unavailable`; token text never in captured logs; built-in world rejection has no `validator` field.
- [x] Commit.

## Task 4: Helm chart (V12)

- [x] Add values, env rendering, extras, validate.yaml guard; add checks to `deploy/helm/test-chart.ps1` (env vars rendered, secret file var, timeout, sidecar/volumes/mounts verbatim, non-loopback http fails, bad timeout fails, default and plain Entra output contain no `PUSH_ENTRA_VALIDATOR`); run `pwsh deploy/helm/test-chart.ps1`; `helm lint`. Commit.

## Task 5: Docs

- [x] `config.rs` table rows; `docs/entra-sign-in.md` section "External token validator" with the full request/response contract, error mapping, security notes, Helm example; `values.yaml` comments; `CHANGELOG.md` entry. Commit.

## Verification

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test -p push-auth-core`, `cargo test --workspace --locked`, `pwsh deploy/helm/test-chart.ps1`.
