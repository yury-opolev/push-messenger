# Microsoft Entra ID Sign-In

Date: 2026-10-19

Implementation status: **13A** planned ([plan](../plans/2026-10-19-plan-13a-entra-sign-in.md)),
**13B** planned ([plan](../plans/2026-10-19-plan-13b-entra-sign-in-chart.md)).

This answers the downstream change request "opt-in Microsoft Entra ID sign-in" (based on
`d1e1bf2`, PR #23). A client signs in with Microsoft Entra ID (MSAL, authorization code
with PKCE), presents the Entra access token to auth-svc and receives the registration
token that `POST /v1/auth/login` gives today. Device registration, access and refresh
tokens, the gateway's key set and everything after them stay as they are. The default
configuration is today's email and password sign-in, unchanged.

The ownership of the
[portable deployment-readiness design](2026-10-16-portable-deployment-readiness-design.md)
still holds: this repository supplies the mechanism, chart values and a guide; the
consumer owns the tenant, the app registrations, Conditional Access, egress and the
acceptance run. Nothing here provisions Entra ID or Azure.

Related: the [Entra ID datastore design](2026-10-18-azure-entra-datastores-design.md)
(Plans 12A and 12B) is a different feature: workload identities for PostgreSQL and Redis.
Sign-in does not depend on it, except that the chart work (13B) builds on 12B's values
and egress policies.

## 1. Requirements

From the request, with its acceptance criteria:

| AC | Criterion |
|---|---|
| AC1 | Default configuration: behaviour, tests and chart output unchanged |
| AC2 | Entra-only: the email/password routes answer 404, no verification or reset email is ever produced, and a user signs in, registers a device, sends and receives messages |
| AC3 | Tokens are refused for a wrong audience, a wrong or non-allowed tenant, expiry, a bad signature, an unsupported algorithm, a missing required scope, app-only tokens and ID tokens |
| AC4 | The same `(tid, oid)` always maps to the same user; a changed UPN or email creates no new user; a different user with the same email is not linked |
| AC5 | After the maximum session age, refresh fails and a fresh Entra sign-in restores access |
| AC6 | A JWKS rotation (unknown `kid`) is handled without a restart; a metadata outage fails closed with a clear error |
| AC7 | Unit tests for claim validation and identity mapping; integration tests with a local JWKS and token stub |

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| D1 | auth-svc validates Entra tokens itself with the `jsonwebtoken` 9.3.1 it already uses (RS256 on ring) and fetches metadata with the workspace's `reqwest` 0.12 over rustls (ring). No MSAL, no Azure SDK, **no new crate**. | `jsonwebtoken` 9 has no crypto backend features: it always uses ring, so no aws-lc-rs and no OpenSSL. Verified: `EncodingKey::from_rsa_der` (PKCS#1) and `DecodingKey::from_rsa_components` sign and verify RS256, and an HS256 token presented to an RSA key fails with `InvalidAlgorithm`. MSRV 1.85 is unaffected. |
| D2 | Only **v2.0 access tokens issued for this API** are accepted: header `alg` exactly `RS256` and a `kid`; `iss` exactly `{authority}{tid}/v2.0` for an allowed `tid`; `aud` one of the configured audiences; `exp` and `nbf` with 60 s leeway; `ver` `2.0`; a non-empty `scp` (delegated) containing the required scope; `idtyp` not `app`; an `oid` that is a GUID. Optional: a required app role in `roles`, and an allow-list of client apps (`azp`). | An ID token's audience is the client app and it has no `scp`; a Graph token's audience is Graph; an app-only token has no `scp` (and `idtyp=app` when that optional claim is on). Microsoft: "In v2.0 tokens, `aud` is always the client ID of the API". The API registration must set `requestedAccessTokenVersion: 2` (the guide says how), otherwise Entra issues v1 tokens with the `sts.windows.net` issuer, which are refused. |
| D3 | Tenants are an explicit list of tenant GUIDs. `common`, `organizations`, `consumers`, domain names, the nil GUID and the personal-account tenant `9188040d-6c67-4c5b-b112-36a304b66dad` are refused at start-up. Multi-tenant wildcards are **not supported** (YAGNI): adding a tenant is a configuration change. | The request is single-tenant; a list form leaves room. A wildcard needs per-tenant issuer handling and onboarding policy that nobody asked for. |
| D4 | Keys per tenant come from `{authority}{tid}/v2.0/.well-known/openid-configuration` (whose `issuer` must equal the expected issuer) and its `jwks_uri` (which must be on the authority host, so egress stays one host). Lazily fetched, warmed at start-up without blocking readiness, cached 24 h; a token with an unknown `kid` refetches at most once per 5 min per tenant; requests time out after 10 s (connect 5 s), answers are read up to 256 KiB, redirects are not followed, `HTTPS_PROXY`/`NO_PROXY` are honoured. A failed refresh keeps using keys up to 24 h past their age (stale-if-error); with no usable keys every Entra sign-in fails closed with 503 `entra_unavailable`, retried at most every 10 s. The unverified `tid` and `iss` are checked against the allow-list **before** any fetch. | Microsoft rotates keys without notice and recommends refreshing about daily. The cooldown and the pre-check stop forged tokens from turning auth-svc into a fetch amplifier. Readiness must not depend on Entra: refresh and password sign-in keep working during an Entra outage. |
| D5 | Accounts: a new table `auth.external_identity (identity_id, provider, tenant_id, subject, user_id, email, created_at, last_sign_in_at)`, unique on `(provider, tenant_id, subject)` and on `user_id`. `subject` is the `oid`. An Entra account in `auth.user_account` has `email` and `password_hash` NULL and `email_verified_at` set at the first sign-in (it is active); a CHECK enforces "email and password, or neither and active". **No automatic linking** of an email/password account and an Entra identity, whatever their addresses. | `oid` is immutable and the same for every app in the tenant; `sub` is pairwise per app. A NULL email cannot collide with the unique email of a password account. "Active" via `email_verified_at` keeps lookup, profiles and the worker's cleanup correct without touching them; the cleanup also gets an explicit `password_hash IS NOT NULL` guard. |
| D6 | First sign-in provisions just-in-time, in one transaction with `user.verified` in the outbox. The handle is the request's optional `handle` (validated like signup, `409 handle_taken` if held) or a generated `u` plus 10 random letters and digits. The display name is the token's `name` claim when it passes the display-name rules, else the handle. Later sign-ins change neither; they update `last_sign_in_at` and the informational `email` (`email`, else `preferred_username`), which only the owner sees in `GET /v1/users/me`. | Handles are public and stable; deriving them from the UPN would publish it and change nothing for a user who can pick one. |
| D7 | The Entra registration token is the same signed token with purpose `register`, but bound to the identity: claims `xid` (the `external_identity` row) and `sx` (the session end) instead of `pw`. `POST /v1/devices` accepts exactly one binding: `pw` (the password hash must match, as today) or `xid`+`sx` (the identity must still belong to the account); and only while that provider is enabled. | The Entra equivalent of the `pw` stamp: a registration token can only register a device for the identity that signed in, and nothing once the identity is gone or the provider is switched off. |
| D8 | Session age: `auth.device.session_expires_at` (Unix seconds, NULL for password devices) is set to sign-in time plus `PUSH_ENTRA_MAX_SESSION_HOURS` (default 24, 1 to 720) on registration and on every Entra sign-in for that device. Refresh after it fails with 401 `session_expired` (the device stays registered); refresh tokens never outlive it. `POST /v1/auth/entra` with `device_id` renews the session for the same device. Access tokens keep their 30 min lifetime, so access ends at most 30 min after the session. Revocation is unchanged. | A user disabled in Entra cannot get a new Entra token, so access ends within the maximum age. Capping access tokens to the session end would make the SDK (which refreshes tokens with under 90 s left) refresh in a loop near the end, for little gain. |
| D9 | `PUSH_AUTH_PROVIDERS` = `password` (default), `entra` or `password,entra`. Without `password` the routes signup, verify (page and POST), verify/resend, login, password/forgot and password/reset are not routed (404) and nothing can write an `email.send` event. Existing password accounts keep their devices until refresh fails or they are revoked; they cannot sign in again. The worker gains `PUSH_EMAIL_SENDER=none`: no sender, and an `email.send` job (which Entra-only never produces) is buried without being sent; the webhook is refused with it. | AC2 and "no email provider is needed". `PUSH_AUTH_PUBLIC_URL` stays required (it names the public origin; the chart always sets it). |
| D10 | One new endpoint, `POST /v1/auth/entra {token, handle?, device_id?}`. Without `device_id`: `{registration_token, expires_in_s}` exactly as login. With `device_id` (an active device of that user): new tokens, earlier refresh tokens revoked, exactly as login with a device. Errors: 401 `entra_token_invalid` (one code for every refusal; the reason goes to the security log only), 503 `entra_unavailable` (retryable, `Retry-After: 5`), 409 `handle_taken`, 404 `device_not_found`, 429. Rate limits: per IP 60 per 15 min before validation, per identity 20 per 15 min after. | Mirrors `/v1/auth/login`, so the SDK's device-keeping logic applies unchanged. One refusal code gives no validation oracle. |
| D11 | Logging: security events `auth.entra.success` / `auth.entra.failure` (route, status, code, IP, user and device ids), `auth.entra.rejected` with the reason name, `auth.entra.provisioned` with the new user id and tenant id. Never the token, `oid`, UPN, name or email. | The request: only non-sensitive identifiers. Tests capture the log and search it for each of these values. |
| D12 | SDK: `Client::sign_in_with_entra(access_token, handle, device_name, platform)` (Node `signInWithEntra`, C `push_client_sign_in_with_entra`) runs the same flow as `sign_in`: the device registered before signs in again, otherwise a device is registered with a persisted `registration_id`. `session_expired` counts as a refusal (the SDK signs out and reports `SignedOut`). The app acquires the token with MSAL; the SDK never stores or logs it. | The request: acquisition stays in the app. |
| D13 | Tests use a local stub OpenID issuer (metadata and JWKS over loopback HTTP) with two fixed RSA-2048 test keys (PKCS#1 DER, base64 in the source, generated once with .NET's `RSA.Create(2048)`), behind push-auth's `testing` feature. `PUSH_ENTRA_AUTHORITY_HOST` accepts `http://` only for loopback hosts. | No RSA key generation is available without new crates: ring cannot generate RSA keys, and the `rsa` crate carries RUSTSEC-2023-0071, which cargo-deny would reject. Fixed keys also keep tests fast and deterministic. |
| D14 | Chart work is a separate plan (13B) on top of 12B: values for the providers and Entra settings, the worker's `none` sender and no email Secret keys in Entra-only mode, no link-page Ingress then, an auth-only HTTPS egress list and optional proxy for Entra, and a minikube rehearsal. | 12B owns the chart's egress model; 13A must be implementable before 12B lands. |

## 3. Configuration

### auth-svc (new variables; everything else unchanged)

| Variable | Meaning | Default |
|---|---|---|
| `PUSH_AUTH_PROVIDERS` | `password`, `entra` or `password,entra` | `password` |
| `PUSH_ENTRA_CLIENT_ID` | accepted audiences, comma-separated: the API registration's application (client) id, optionally its `api://…` identifier URI too (v2.0 tokens carry the GUID) | required with `entra` |
| `PUSH_ENTRA_ALLOWED_TENANTS` | tenant ids (GUIDs), comma-separated; `common`, `organizations`, `consumers` refused | required with `entra` |
| `PUSH_ENTRA_AUTHORITY_HOST` | `https://login.microsoftonline.com/` or a sovereign cloud's host; `http://` only for loopback (tests) | `https://login.microsoftonline.com/` |
| `PUSH_ENTRA_REQUIRED_SCOPE` | the delegated scope `scp` must contain; `*` accepts any scope of this API | `access_as_user` |
| `PUSH_ENTRA_REQUIRED_ROLE` | an app role `roles` must contain | none |
| `PUSH_ENTRA_ALLOWED_CLIENT_APPS` | client app ids allowed in `azp`, comma-separated | any |
| `PUSH_ENTRA_MAX_SESSION_HOURS` | maximum session age of an Entra-backed device, 1 to 720 | `24` |
| `HTTPS_PROXY`, `NO_PROXY` | standard proxy variables, honoured for the Entra requests only | none |

The `PUSH_ENTRA_*` variables are read only when `entra` is a provider.

### worker

| Variable | Meaning | Default |
|---|---|---|
| `PUSH_EMAIL_SENDER` | `log`, `resend` or **`none`** (no email at all: an `email.send` job is buried unsent; `PUSH_RESEND_WEBHOOK_SECRET` and the DEV ONLY mailbox are refused) | `log` |

### Egress

auth-svc reaches `https://{authority host}` (`login.microsoftonline.com` in the global
cloud) on 443 for `/{tid}/v2.0/.well-known/openid-configuration` and
`/{tid}/discovery/v2.0/keys`. Nothing else, and only when `entra` is enabled.

## 4. Sign-in flow

```text
browser / app                      Entra ID                         auth-svc
  MSAL: auth code + PKCE  ───────▶  /authorize, /token
  ◀──── access token (aud = API, scp = access_as_user)
  POST /v1/auth/entra {token} ─────────────────────────────────────▶ validate (D2, D4)
                                                                     find or create (D5, D6)
  ◀──── {registration_token, expires_in_s} ─────────────────────────
  POST /v1/devices (Bearer registration_token) ────────────────────▶ bound to xid (D7)
  ◀──── {access_token, refresh_token, …} (session_expires_at set, D8)
  … refresh until the session ends, then 401 session_expired …
  POST /v1/auth/entra {token, device_id} ──────────────────────────▶ same device, new session
```

Browser apps use MSAL.js (`@azure/msal-browser`) with a SPA platform redirect URI and
request `api://<api-client-id>/access_as_user`; native apps use MSAL for their platform
with a public-client redirect URI. Neither has a client secret. The guide
(`docs/entra-sign-in.md`) describes both registrations, `requestedAccessTokenVersion: 2`,
tenant restriction, user assignment and Conditional Access.

## 5. Data model

```sql
ALTER TABLE auth.user_account ALTER COLUMN email DROP NOT NULL;
ALTER TABLE auth.user_account ALTER COLUMN password_hash DROP NOT NULL;
ALTER TABLE auth.user_account ADD CONSTRAINT user_account_kind CHECK (
    (email IS NOT NULL AND password_hash IS NOT NULL)
    OR (email IS NULL AND password_hash IS NULL AND email_verified_at IS NOT NULL));
CREATE TABLE auth.external_identity (…, UNIQUE (provider, tenant_id, subject), user_id UNIQUE);
ALTER TABLE auth.device ADD COLUMN session_expires_at bigint;
```

`auth.delete_unverified_accounts` is replaced with the same body plus
`AND account.password_hash IS NOT NULL`. The worker's grants are unchanged (it never
reads accounts). Message-svc is unchanged: it learns the new user from `user.verified`.

## 6. Verification

Local proof without Entra ID (Plan 13A), with the stub issuer:

| AC | Local evidence |
|---|---|
| AC1 | Unchanged existing tests; config defaults; `/v1/auth/entra` answers 404 by default; chart default render unchanged (13B) |
| AC2 | API test: every password route 404 in Entra-only mode, no `email.send` in the outbox; whole-stack test: two Entra users sign in with the SDK, register, become contacts and chat; the worker runs with `PUSH_EMAIL_SENDER=none` |
| AC3 | Unit tests of the claim rules; API test with real signatures for every refusal listed, no account created |
| AC4 | Service and API tests: same `(tid, oid)` with a changed UPN, name and email; a password account and another Entra user with the same address; the same `oid` in another allowed tenant |
| AC5 | API test: session moved into the past, refresh 401 `session_expired`, Entra sign-in with `device_id` restores the same device |
| AC6 | Key-set tests: rotation without restart, cooldown for unknown kids, outage before the first fetch (503), stale keys during an outage, foreign `jwks_uri`, wrong metadata issuer, a slow authority timing out |
| AC7 | All of the above |

Plan 13B adds the chart checks and a minikube rehearsal (Entra-only, the DEV ONLY in-cluster
issuer of 12B extended with OpenID metadata and user tokens). The consumer's acceptance
on a real tenant follows the guide's checklist.

## 7. Delivery

| Plan | Delivers |
|---|---|
| 13A | auth-svc: configuration, validation, key cache, accounts, endpoint, session age; worker `none` sender; SDK and bindings; stub issuer; tests; README and `docs/entra-sign-in.md` |
| 13B | Chart values, validation, env, Ingress and Secret changes for Entra-only, auth egress and proxy, guide section for the chart, minikube rehearsal (depends on 12B) |

## 8. Open questions

| # | Question | Default taken |
|---|---|---|
| Q1 | Should the handle be derived from the UPN's local part when the client sends none? | No: random `u…` handle; the app should ask the user on first sign-in |
| Q2 | Should a profile endpoint let Entra users change their handle later? | Out of scope (no handle change exists for anyone today) |
| Q3 | Should auth-svc offer an admin way to remove an external identity (offboarding before the session ends)? | No: device revocation and the maximum session age cover it; SQL is documented |
| Q4 | Apps with custom signing keys (claims mapping) need `jwks_uri?appid=`. Support it? | No: refused (the guide says not to configure custom keys on the API) |
| Q5 | Should switching an installation to Entra-only revoke existing password devices at once? | No: they expire with their refresh tokens or are revoked by the operator |

Out of scope: multi-tenant wildcards, account linking, SCIM or Graph lookups, group
claims, v1.0 tokens, sign-in for any provider other than Entra ID, and provisioning Entra
or Azure.
