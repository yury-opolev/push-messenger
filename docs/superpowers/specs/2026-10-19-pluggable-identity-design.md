# Pluggable identity: push-auth-core and a thin push-auth

Date: 2026-10-19

Implementation status: **14** in progress ([plan](../plans/2026-10-19-plan-14-pluggable-identity.md)).

The owner asked that the Entra ID authentication be substitutable by the repository's
consumers, the way safeexchange splits `SafeExchange.Core` (the logic, behind
interfaces) from `SafeExchange.Functions` (a thin host that wires the defaults), so a
consumer writes their own host. Their answers: two projects; a generic principal
(provider, issuer, subject, name, email, claims) so another OpenID Connect provider can be
plugged in later; the token read from `Authorization: Bearer` or, as today, the JSON body;
only push-auth is split now.

## Shape

```text
crates/push-auth-core   library: everything push-auth is today (accounts, devices,
                        tokens, API, admin API, Entra ID check, migrations), plus the
                        identity middleware and its traits. Lib name push_auth_core.
crates/push-auth        thin binary: reads Config from the environment, builds the
                        defaults, calls push_auth_core::serve_with. Binary name push-auth
                        (images, chart and Dockerfile unchanged).
```

A consumer's own wrapper is the same ~40 lines with their own `Authenticator` and/or
`Admission` passed to `serve_with`.

## Decisions

| # | Decision | Why |
|---|---|---|
| D1 | Rename the crate directory `crates\push-auth` to `crates\push-auth-core` (package `push-auth-core`, lib `push_auth_core`, `[lib]` only, no binary); a new `crates\push-auth` holds only `src\main.rs` (package and binary `push-auth`). Workspace members, the workspace dependency table and the dependents (push-worker, push-gateway dev, push-smoke) move to `push-auth-core`; the `testing` feature moves with the core. | The core/wrapper split the owner asked for. The binary keeps its name, so `deploy\docker\Dockerfile` (`-p push-auth`, `target/release/push-auth`), images, the chart and runbooks are untouched. |
| D2 | `Principal { provider, issuer, subject, name, email, claims }`: `provider` a short id (`^[a-z][a-z0-9-]{0,31}$`, e.g. `entra`); `issuer` the stable issuer key (Entra: the tenant id, lower-case GUID); `subject` the immutable user id at that issuer (Entra: `oid`); `name`/`email` informational only; `claims` the full verified payload (JSON map). `Debug` shows provider and issuer only. Accounts are found by `(provider, issuer, subject)`, never by email. | Generic enough for any OpenID Connect provider; for Entra it is exactly Plan 13A's `(tid, oid)`. |
| D3 | `trait Authenticator: Send + Sync + Debug { fn provider(&self) -> &str; fn max_session(&self) -> Duration; async fn authenticate(&self, token: &str) -> Result<Principal, AuthenticationError>; async fn warm(&self) {} }`. `AuthenticationError::{Rejected(reason: &'static str), Unavailable(String)}`. The built-in `EntraSignIn` implements it (provider `entra`). | The substitutable token check (safeexchange's `ITokenValidationParametersProvider` + `ITokenHelper`). The session's maximum age belongs to whoever vouches for the user. |
| D4 | `trait Admission: Send + Sync + Debug { async fn admit(&self, principal: &Principal) -> AdmissionDecision }`, `AdmissionDecision::{Admit, Refuse, Unavailable}`. Optional; runs after authentication, before any account is found or created. | Restrictions (an Entra group, a role, an allow-list in the consumer's store) without re-implementing token validation. A refused principal never gets an account. |
| D5 | The **identity middleware** (an axum layer in the core) on `POST /v1/auth/{provider}`: (1) the per-IP sign-in rate limit; (2) the token from `Authorization: Bearer`, else the body's `token` (body buffered up to 16 KiB and handed on unchanged; both present and different → 400 `invalid`); (3) `authenticate`; (4) `admit`; (5) the `Principal` put into the request's extensions; then (6) the per-identity rate limit. The handler reads `Extension<Principal>` and the rest of the body (`handle`, `device_id`) as today. | "A middleware that handles the incoming request and outputs the principal with claims" (the owner), mirroring safeexchange's `TokenMiddlewareCore`; the header is the standard place, the body keeps existing clients working. |
| D6 | `serve_with(config, http, health, shutdown, Overrides { authenticator: Option<Arc<dyn Authenticator>>, admission: Option<Arc<dyn Admission>> })`; `serve` = `serve_with(.., Overrides::default())`. | Compile-time substitution: Rust has no DI container; a function argument is explicit and type-checked, with no unsafe plug-in loading. |
| D7 | `PUSH_AUTH_PROVIDERS` gains `external`: the authenticator the wrapper supplies. `entra` = the built-in Entra ID check (requires `PUSH_ENTRA_*`, as today); `external` requires `Overrides.authenticator` and does not read `PUSH_ENTRA_*` (a wrapper that customises Entra builds `EntraSignIn` from `EntraSettings::from_env()` itself). `entra` and `external` together are allowed only if the external provider id is not `entra`. An `Overrides.authenticator` without `external`, or `external` without one, stops start-up with a clear error. The route is `/v1/auth/{provider id}`, so a wrapper that keeps provider `entra` keeps `/v1/auth/entra` and the SDK's `sign_in_with_entra`. | One switch says which routes exist; no silent custom code behind a disabled route, no Entra settings demanded from a non-Entra provider. |
| D8 | Migration `0005_external_provider.sql`: `provider` CHECK becomes the D2 pattern; `tenant_id uuid` becomes `issuer text` (`USING tenant_id::text`, 1–256 characters); the unique key becomes `(provider, issuer, subject)`. Non-destructive, transactional; existing Entra rows keep their identity. The admin API keeps `{entra: {tenant_id, object_id}}` and gains `{external: {provider, issuer, subject}}`. | The account store must accept the generic principal, or "generic" would be a promise the database breaks. |
| D9 | Refusals: rejected token → 401 `entra_token_invalid` for provider `entra`, `token_invalid` for others; authenticator unavailable → 503 `entra_unavailable` / `identity_unavailable`; admission `Refuse` → **403 `sign_in_not_permitted`** (new); admission `Unavailable` → 503. Security events `auth.external.rejected` (provider, reason) and `auth.external.not_permitted` (provider, issuer); Entra keeps `auth.entra.rejected`. Logged by the middleware for every authenticator; never the token, subject, name or email. | Existing Entra clients see the same codes; custom code cannot weaken the log contract. |
| D10 | An example wrapper, `crates\push-auth\examples\custom_identity.rs`: the built-in Entra check wrapped by a custom `Authenticator`, plus an `Admission` requiring a group id (`EXAMPLE_REQUIRED_GROUP`) in the `groups` claim. `docs\entra-sign-in.md` gains "Your own wrapper": how to build an image from it and the duties of a custom authenticator (signature and algorithm, issuer, audience, lifetime, delegated user token, a stable immutable subject, no token in logs). | Consumers get a compiled, tested starting point; the workspace's Clippy `--all-targets` builds it on every change. |

Not in scope: the SDK and bindings for providers other than `entra` (a later plan);
splitting message-svc, the gateway or the worker; chart values (a consumer's image goes in
`images.auth` as today); account provisioning replacement.

## Acceptance criteria

- **AC1** Default configuration and the built-in Entra check: routes, answers, logs, chart
  render and every existing test unchanged; the `push-auth` image is built and named as before.
- **AC2** A wrapper passing an `Authenticator` with `PUSH_AUTH_PROVIDERS=external` serves
  `/v1/auth/{provider}`; its principal signs in and gets one account per
  `(provider, issuer, subject)`; its rejection answers 401; the same principal with a
  changed email or name maps to the same account.
- **AC3** The middleware accepts the token from the header or the body, refuses
  conflicting ones (400), and applies the sign-in rate limit before authenticating.
- **AC4** An admission refusing answers 403 `sign_in_not_permitted` and creates no account,
  for a new sign-in and a device renewal; `Unavailable` answers 503; it sees the verified
  claims (e.g. `groups`).
- **AC5** Mismatched overrides and providers stop start-up with a clear error.
- **AC6** Migration 0005 upgrades a database holding Plan 13A Entra identities without
  losing or changing them; the admin API revokes by `entra` and by `external` selectors.
- **AC7** No token, subject, name or email reaches the log on any path.
- **AC8** The example wrapper builds and is documented.
