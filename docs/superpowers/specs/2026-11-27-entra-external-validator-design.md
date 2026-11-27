# Pluggable external token validator for the Entra ID providers — design

Request: "REQUEST PUSH seam" (2026-11-27). Component: push-auth (`crates/push-auth-core`),
Helm chart `deploy/helm/push-messenger`. Opt-in; with `PUSH_ENTRA_VALIDATOR_URL` unset no
code path, log line or chart output changes.

## Decisions

| # | Decision | Why |
|---|---|---|
| V1 | `EntraSettings` gains `pub validator: Option<ValidatorSettings>` (`url`, `timeout`, `secret`), parsed by `EntraSettings::from_lookup`, so wrappers using `EntraSettings::from_env` get it too. `ValidatorSettings` has a hand-written `Debug` that redacts the secret. | One place reads every `PUSH_ENTRA_*`; config.rs already reads it only with `entra`/`entra-app`. |
| V2 | `PUSH_ENTRA_VALIDATOR_URL`: trimmed, empty = unset. Absolute URL; `https://` anywhere, `http://` only for `127.0.0.1`, `[::1]`, `localhost` (as `PUSH_ENTRA_AUTHORITY_HOST`); no user info, no fragment; a path and query are allowed. | Spec §2.2. User info could hold a password, so it is refused (and then not echoed). |
| V3 | `PUSH_ENTRA_VALIDATOR_TIMEOUT_MS`: whole number 100–10000, default 2000. `PUSH_ENTRA_VALIDATOR_SECRET_FILE`: read once at start-up; trimmed content must be 1–1024 visible ASCII characters (a valid header value); errors name the variable and the path, never the content. Both are refused (`ConfigError::Invalid`, "set only with PUSH_ENTRA_VALIDATOR_URL") when the URL is unset. | Spec §2.2; a secret or timeout without a URL is a misconfiguration that would otherwise be silently ignored. The variables are new, so no existing deployment is affected. |
| V4 | `EntraSignIn` holds `keys: Option<Arc<KeySets>>` and `validator: Option<Arc<Validator>>`, exactly one set. With a validator no `KeySets` is built and `warm()` does nothing. | Spec §2.1: the validator owns key discovery. |
| V5 | `verified(token, ProviderKind)`; `ProviderKind::{User, App}` (`"entra"`, `"entra-app"`), crate-private. Pre-checks in both modes: size, three segments, header JSON with `alg` RS256 and a `kid`, unverified `tid` admitted, unverified `iss` = the tenant's v2.0 issuer. Then either JWKS + `jsonwebtoken::decode` (unchanged) or the validator. | The header check is local, cheap and only narrows (a validator can never make push-auth accept `alg: none`). |
| V6 | Request: `POST url`, `Authorization: Bearer <token>`, `Content-Type`/`Accept: application/json`, `X-Push-Validator-Secret` only when configured, body `{"provider":"entra"\|"entra-app"}`. No `X-Request-Id`: push-auth has no request id (optional in the spec). reqwest client: `redirect::Policy::none()`, `timeout` = `connect_timeout` = the configured timeout, `no_proxy()` for loopback URLs (a pod's `HTTPS_PROXY` must not catch the sidecar), env proxies otherwise. No retries. | Spec §2.3. |
| V7 | Response handling. Every body is read by chunks up to 16 KiB (a larger `Content-Length` or body is `Unavailable`). **200**: not JSON → `Unavailable`; JSON not of the schema (no `claims` object, a required claim missing, a claim of the wrong JSON type) → `Rejected(Malformed)`. **401**: JSON `{"reason": <name>}` with a known `Reason` name → `Rejected(reason)`; anything else (no body, other JSON, unknown name) → `Rejected(Validator)`. **Any other status** (3xx included), timeout, connection error → `Unavailable` with only "HTTP <code>", "timed out", "could not be reached" or "failed". The 200 `Content-Type` is not checked. | Spec §2.4. |
| V8 | Defensive re-checks of returned claims, in order: schema (V7) → `tid`/`oid` equal to the token's unverified `tid`/`oid` (else `malformed`) → `tid` admitted (`tenant`) → `iss` = the tenant's v2.0 issuer (`issuer`) → `aud` (string or array) contains a configured audience (`audience`) → `exp < now - 60` (`expired`) → `nbf > now + 60` when present (`not_yet_valid`). `nbf` is optional, as in the spec's schema. Then the same `EntraClaims` deserialization, `claims::common` and provider policy as today; the returned claims object is the principal's verified claims. | Spec §2.4 1–4; a validator can only narrow who signs in. |
| V9 | `Reason::Validator` (`"validator"`), and `Reason::from_name` for the 401 mapping. | Spec §2.4. |
| V10 | Security log: `auth.entra.rejected` (entra) and `auth.external.rejected` (entra-app) carry `validator = true` only in validator mode: the field is an `Option<bool>`, so built-in lines are byte-for-byte unchanged. The middleware learns the mode from `IdentityState.validator`, set in `api::router` from `Auth::external_validation(provider)` (true only for the built-in `entra`/`entra-app` authenticators with a validator). `EntraSignIn::verify` logs the same way. Unavailability in validator mode is logged as "the token validator is unavailable" with the error kind/status only. | Spec §2.4 error semantics. |
| V11 | Test stub `StubValidator` in `entra/testing.rs` (feature `testing`): axum on 127.0.0.1; by default echoes the bearer token's unverified payload as `claims` (a validator copies claims); can answer a fixed status/body, fixed claims, or wait; records each request's method, authorization, secret header and body. | Spec §5. |
| V12 | Helm: `auth.signIn.entra.validator.{url,timeoutMs,secretFile}` → the variables (only with an Entra provider and a URL); `auth.extraContainers`, `auth.extraVolumes`, `auth.extraVolumeMounts` rendered verbatim (`toYaml`) into the auth Deployment; `validate.yaml` fails on a validator URL that is `http://` to a non-loopback host or not http(s), a `timeoutMs` outside 100–10000, a relative `secretFile`, and extras that are not lists. Defaults render byte-for-byte as before. Network policy unchanged. | Spec §2.5. |
| V13 | No caching of validator results or failures; one validator call per sign-in; never a JWKS fallback. | Spec §2.4. |

## Testing

Unit tests in `entra/mod.rs`, `entra/app.rs`, `entra/settings.rs`, `config.rs`; the router
and log tests in `tests/entra_api.rs` (Docker); chart checks in
`deploy/helm/test-chart.ps1`. Cases follow spec §5 items 1–10.

## Documentation

`config.rs` table, `docs/entra-sign-in.md` (a section with the whole contract), chart
`values.yaml` comments, `CHANGELOG.md`.
