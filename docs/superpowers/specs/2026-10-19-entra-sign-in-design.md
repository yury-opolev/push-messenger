# Microsoft Entra ID Sign-In

Date: 2026-10-19

Implementation status: **13A** done, #37 ([plan](../plans/2026-10-19-plan-13a-entra-sign-in.md)),
**13B** in progress ([plan](../plans/2026-10-19-plan-13b-entra-sign-in-chart.md)). Design
approved with the owner's final decisions of 2026-10-18.

This answers the downstream change request "opt-in Microsoft Entra ID sign-in" (based on
`d1e1bf2`, PR #23). A client signs in with Microsoft Entra ID (MSAL, authorization code
with PKCE), presents the Entra access token to auth-svc and receives the registration
token that `POST /v1/auth/login` gives today. Device registration, access and refresh
tokens, the gateway's key set and everything after them stay as they are. The default
configuration is today's email and password sign-in, unchanged.

With it come three account operations the owner asked for: every user may change their
handle, an operator may sign a user out of all devices through an admin API, and switching
an installation to Entra ID only signs every password session out at once.

The ownership of the
[portable deployment-readiness design](2026-10-16-portable-deployment-readiness-design.md)
still holds: this repository supplies the mechanism, chart values and a guide; the
consumer owns the tenant, the app registrations, Conditional Access, egress, the admin
token's custody and the acceptance run. Nothing here provisions Entra ID or Azure.

Related: the [Entra ID datastore design](2026-10-18-azure-entra-datastores-design.md)
(Plans 12A and 12B) is a different feature. Sign-in does not depend on it, except that
the chart work (13B) builds on 12B's values and egress policies.

## 1. Requirements and acceptance criteria

| AC | Criterion |
|---|---|
| AC1 | Default configuration: behaviour, tests and chart output unchanged |
| AC2 | Entra-only: the email/password routes answer 404, no verification or reset email is ever produced, and a user signs in, registers a device, sends and receives messages |
| AC3 | Tokens are refused for a wrong audience, a wrong or non-allowed tenant, expiry, a bad signature, an unsupported algorithm, a missing required scope, app-only tokens and ID tokens |
| AC4 | The same `(tid, oid)` always maps to the same user; a changed UPN or email creates no new user; a different user with the same email is not linked |
| AC5 | After the maximum session age, refresh fails and a fresh Entra sign-in restores access |
| AC6 | A JWKS rotation (unknown `kid`) is handled without a restart; a metadata outage fails closed with a clear error |
| AC7 | Unit tests for claim validation and identity mapping; integration tests with a local JWKS and token stub |
| AC8 | `PUSH_ENTRA_ALLOWED_TENANTS` takes exactly one of `common`, `organizations`, `consumers` or a list of tenant ids; mixtures, duplicates, empty entries and other values stop start-up; each keyword admits exactly its tenants |
| AC9 | Any signed-in user (password or Entra) changes their handle; the old handle is free at once, a taken one answers 409 `handle_taken`, invalid ones 400, too many changes 429; lookup finds the new handle only |
| AC10 | With the admin token configured, an operator signs a user (by user id, handle, or Entra tenant and object id) out of every device: refresh fails, live connections close within seconds, nothing else changes; without the token the admin listener does not exist; a wrong token answers 401 |
| AC11 | Starting with `entra` alone signs out every device of every password account at once (refresh refused, connections closed), idempotently with several replicas; switching password back on later revives none of those devices |

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| D1 | auth-svc validates Entra tokens itself with the `jsonwebtoken` 9.3.1 it already uses (RS256 on ring) and fetches metadata with the workspace's `reqwest` 0.12 over rustls (ring). No MSAL, no Azure SDK, **no new crate**. | `jsonwebtoken` 9 has no crypto backend features: it always uses ring, so no aws-lc-rs and no OpenSSL. Verified: `EncodingKey::from_rsa_der` (PKCS#1) and `DecodingKey::from_rsa_components` sign and verify RS256, and an HS256 token presented to an RSA key fails with `InvalidAlgorithm`. MSRV 1.85 is unaffected. |
| D2 | Only **v2.0 access tokens issued for this API** are accepted: header `alg` exactly `RS256` and a `kid`; `iss` exactly `{authority}{tid}/v2.0` for the token's own `tid`, which the tenant setting must admit; `aud` one of the configured audiences; `exp` and `nbf` with 60 s leeway; `ver` `2.0`; a non-empty `scp` (delegated) containing the required scope; `idtyp` not `app`; an `oid` that is a GUID. Optional: a required app role in `roles`, and an allow-list of client apps (`azp`). | An ID token's audience is the client app and it has no `scp`; a Graph token's audience is Graph; an app-only token has no `scp` (and `idtyp=app` when that optional claim is on). Microsoft: "In v2.0 tokens, `aud` is always the client ID of the API". The API registration must set `requestedAccessTokenVersion: 2` (the guide says how), otherwise Entra issues v1 tokens with the `sts.windows.net` issuer, which are refused. |
| D3 | `PUSH_ENTRA_ALLOWED_TENANTS` (required with `entra`, no default) is **exactly one** of: `common` (admits any tenant, personal accounts included), `organizations` (any tenant except the personal-account tenant `9188040d-6c67-4c5b-b112-36a304b66dad`), `consumers` (that tenant only), or a comma-separated list of one or more tenant ids (exactly those; at most 20). Keywords are case-insensitive and stand alone: `common,<guid>`, `organizations,consumers`, duplicates, empty entries, the nil GUID and any other value stop start-up with a configuration error. The issuer is always checked against the token's own `tid`. | The owner's decision. The keywords are opt-in and explicit; they never come from a default. **Security implication:** with a keyword, anyone who has an account of that kind (any Microsoft Entra tenant, or any personal Microsoft account) and can obtain a token for the API can create an account here; the API registration's supported account types, user assignment and Conditional Access are then the only gate. The guide says so in bold. |
| D4 | Keys come from OpenID metadata: for a tenant list, per tenant from `{authority}{tid}/v2.0/.well-known/openid-configuration` (its `issuer` must equal the expected one); for a keyword, from the keyword's shared endpoint `{authority}{keyword}/v2.0/.well-known/openid-configuration` (whose issuer is a template, so only its key set's host is checked). The `jwks_uri` must be on the authority host (one egress host). Keys are fetched lazily, warmed at start-up without blocking readiness, cached 24 h; a token with an unknown `kid` refetches at most once per 5 min per key source; requests time out after 10 s (connect 5 s), answers are read up to 256 KiB, redirects are not followed, `HTTPS_PROXY`/`NO_PROXY` are honoured. A failed refresh keeps using keys up to 24 h past their age; with no usable keys every Entra sign-in fails closed with 503 `entra_unavailable`, retried at most every 10 s. The `tid` and `iss` are checked against the setting **before** any fetch. | One shared key source per keyword bounds the cache to one entry (or at most 20 for a list) whatever `tid`s attackers put into forged tokens, and an arbitrary `tid` never causes a fetch of its own. Microsoft signs all tenants' tokens with the keys its common endpoints publish, except for apps with custom signing keys, which are out of scope (D13). Readiness must not depend on Entra: refresh and password sign-in keep working during an outage. |
| D5 | Accounts: a new table `auth.external_identity (identity_id, provider, tenant_id, subject, user_id, email, created_at, last_sign_in_at)`, unique on `(provider, tenant_id, subject)` and on `user_id`. `subject` is the `oid`. An Entra account has `email` and `password_hash` NULL and `email_verified_at` set at its first sign-in (it is active); a CHECK enforces "email and password, or neither and active". **No automatic linking** of an email/password account and an Entra identity, whatever their addresses. | `oid` is immutable; `sub` is pairwise per app. A NULL email cannot collide with a password account's unique email. "Active" via `email_verified_at` keeps lookup, profiles and the worker's cleanup correct; the cleanup also gets an explicit `password_hash IS NOT NULL` guard. |
| D6 | First sign-in provisions just-in-time, in one transaction with `user.verified` in the outbox. The handle is the request's optional `handle` (validated like signup, 409 `handle_taken` if held) or a **random** `u` plus 10 letters and digits. The display name is the token's `name` claim when it passes the display-name rules, else the handle. Later sign-ins change neither; they update `last_sign_in_at` and the informational `email` (`email`, else `preferred_username`), which only the owner sees in `GET /v1/users/me`. | A random handle publishes nothing about the user; D14 lets them pick a better one. |
| D7 | The Entra registration token is the same signed token with purpose `register`, bound to the identity: claims `xid` (the `external_identity` row) and `sx` (the session end) instead of `pw`. `POST /v1/devices` accepts exactly one binding: `pw` (the password hash must match, as today) or `xid`+`sx` (the identity must still belong to the account); and only while that provider is enabled. | The Entra equivalent of the `pw` stamp. |
| D8 | Session age: `auth.device.session_expires_at` (Unix seconds, NULL for password devices) is set to sign-in time plus `PUSH_ENTRA_MAX_SESSION_HOURS` (default 24, 1 to 720) on registration and on every Entra sign-in for that device. Refresh after it fails with 401 `session_expired` (the device stays registered); refresh tokens never outlive it. `POST /v1/auth/entra` with `device_id` renews the session for the same device. Access tokens keep their 30 min lifetime. | A user disabled in Entra cannot get a new token, so access ends within the maximum age plus 30 min. Capping access tokens would make the SDK refresh in a loop near the end. |
| D9 | `PUSH_AUTH_PROVIDERS` = `password` (default: email and password with email verification, password reset by email, as today), `entra` (Entra ID only: no email verification, no password, no email provider; the signup, verify, verify/resend, login, password/forgot and password/reset routes are not routed, 404), or `password,entra` (both; never linked). The worker gains `PUSH_EMAIL_SENDER=none`: no sender, and an `email.send` job (which Entra-only never produces) is buried unsent; the webhook and the mailbox are refused with it. | AC2 and "no email provider is needed". `PUSH_AUTH_PROVIDERS` order and spaces do not matter; each name once. |
| D10 | One sign-in endpoint, `POST /v1/auth/entra {token, handle?, device_id?}`. Without `device_id`: `{registration_token, expires_in_s}` exactly as login. With `device_id` (an active device of that user): new tokens, earlier refresh tokens revoked. Errors: 401 `entra_token_invalid` (one code for every refusal; the reason goes to the security log only), 503 `entra_unavailable` (retryable, `Retry-After: 5`), 409 `handle_taken`, 404 `device_not_found`, 429. Rate limits: per IP 60 per 15 min before validation, per identity 20 per 15 min after. | Mirrors `/v1/auth/login`, so the SDK's device-keeping logic applies unchanged. One refusal code gives no validation oracle. |
| D11 | Logging: security events `auth.entra.success` / `auth.entra.failure`, `auth.entra.rejected` (reason name), `auth.entra.provisioned` (user id, tenant id), `auth.handle.changed` (user id), `auth.admin.sessions_revoked` (user id, device count, selector kind, IP), `auth.admin.token.refused` (IP), `auth.password_sessions.revoked` (count). Never a token, `oid`, UPN, name, email or handle. | The request: only non-sensitive identifiers. Tests capture the log and search it. |
| D12 | SDK: `Client::sign_in_with_entra(access_token, handle, device_name, platform)` (Node `signInWithEntra`, C `push_client_sign_in_with_entra`) runs the same flow as `sign_in`. `session_expired` counts as a refusal (`SignedOut`). `Client::update_profile(display_name, handle)` (Node `updateProfile`, C `push_client_update_profile`). The SDK never stores or logs the Entra token. | The request: acquisition stays in the app. |
| D13 | Tests use a local stub OpenID issuer (metadata and JWKS over loopback HTTP, for tenant ids and keywords) with two fixed RSA-2048 test keys (PKCS#1 DER, base64 in the source, generated once with .NET's `RSA.Create(2048)`), behind push-auth's `testing` feature. `PUSH_ENTRA_AUTHORITY_HOST` accepts `http://` only for loopback. Custom signing keys (`jwks_uri?appid=`) are **not supported**. | ring cannot generate RSA keys, and the `rsa` crate carries RUSTSEC-2023-0071, which cargo-deny would reject. |
| D14 | **Handle change for every user:** `PATCH /v1/users/me` (which already changes `display_name`) also takes `handle`; either or both, at least one. The handle follows the signup rules; the unique constraint decides (409 `handle_taken`); the old handle is free at once. Rate limit `handle_change`: 30 per hour per IP, 5 per day per account. **Re-authentication: none beyond a valid access token**, as for the display name and device revocation. | A handle is a public name, not a credential: contacts and conversations are bound to user ids, so a new handle cannot take over anything, and the security log records each change. Requiring a password would exclude Entra users; requiring a fresh Entra token would need a second Entra flow in every app for little gain. Nothing caches handles: lookup and profiles read `auth.user_account`, message-svc keeps only user ids (verified: no `handle` in message-svc, push-domain, the protocol or push-store), and the SDK stores no handles. |
| D15 | **Admin API, sign a user out everywhere:** `POST /v1/admin/users/revoke-sessions` with exactly one of `{"user_id"}`, `{"handle"}`, `{"entra": {"tenant_id", "object_id"}}`. It revokes every active device and its refresh tokens in one transaction and writes `device.revoked` per device, so the existing relay (worker → message-svc `DeviceRevoked` → `kick:{device}` → gateway close 4003) ends live connections within seconds and message-svc refuses the devices' access tokens. Answer `{"user_id", "devices_revoked"}`; unknown user 404. Served **only** on a separate listener (`PUSH_AUTH_ADMIN_ADDR`, default `0.0.0.0:8082`) that exists only when `PUSH_AUTH_ADMIN_TOKEN` (32 to 256 visible ASCII characters, from the consumer-managed Secret) is set; `Authorization: Bearer` compared in constant time over SHA-256 digests; per-IP rate limit 60 per 15 min for calls with the token, and a separate 20 per 15 min per IP for failed attempts, which therefore never lock the token's holder out (fail closed, like every limit). The chart never routes it through the Ingress and allows it only from `networkPolicy.adminPeers` (13B). **No CLI** and **no "remove the Entra link" option**. | The HTTP listener is the one path: it works with several replicas, needs no database credentials in an operator's hands (`kubectl port-forward` to the admin port is enough), and is testable like the rest of the API. Removing the link would only orphan the account (contacts, history) and make the next sign-in create a duplicate person, since `oid`s are never reused; blocking a person belongs in Entra ID (disable, unassign), after which this API ends their sessions. |
| D16 | **Switching to Entra-only signs password sessions out at once:** when `password` is not a provider, auth-svc revokes, before it reports ready, every active device of every account with a password, in batches of 500 (`FOR UPDATE SKIP LOCKED`, so replicas share the work), each batch with its refresh tokens and `device.revoked` events, as D15; then logs the count. Rows held by another transaction are retried until a non-skipping check finds no active password device, and the sweep runs again every minute while `password` is off (devices signed in by old replicas during a rolling update). Refresh by a password account's device while `password` is off revokes it the same way, and its access tokens are refused. Idempotent: a later start finds nothing. Switching password back on revives nothing: revoked devices stay revoked (their `registration_id`s spent), and those users sign in again as new devices. Entra sign-in never links to those accounts: the same people get new accounts. | The owner's decision: the switch is destructive for password sessions, and immediate rather than "within 30 minutes". Reusing the revocation relay keeps one kick path. |
| D17 | Chart work is a separate plan (13B) on top of 12B: values for the providers and Entra settings (tenant keywords or ids), the worker's `none` sender and no email Secret keys in Entra-only mode, no link-page Ingress then, an auth-only HTTPS egress list and optional proxy, a CA bundle, the admin listener (Secret key `auth-admin-token`, Service port, NetworkPolicy peers, never an Ingress), and a minikube rehearsal. | 12B owns the chart's egress model; 13A must be implementable before 12B lands. |

## 3. Configuration

### auth-svc (new variables; everything else unchanged)

| Variable | Meaning | Default |
|---|---|---|
| `PUSH_AUTH_PROVIDERS` | `password` (email and password, email verification), `entra` (Entra ID only, no email), or `password,entra` | `password` |
| `PUSH_ENTRA_CLIENT_ID` | accepted audiences, comma-separated: the API registration's application (client) id, optionally its `api://…` identifier URI too (v2.0 tokens carry the GUID) | required with `entra` |
| `PUSH_ENTRA_ALLOWED_TENANTS` | exactly one of `common`, `organizations`, `consumers`, or a comma-separated list of one or more tenant ids (GUIDs, at most 20); nothing else, no mixing, no duplicates | required with `entra` |
| `PUSH_ENTRA_AUTHORITY_HOST` | `https://login.microsoftonline.com/` or a sovereign cloud's host; `http://` only for loopback (tests) | `https://login.microsoftonline.com/` |
| `PUSH_ENTRA_REQUIRED_SCOPE` | the delegated scope `scp` must contain; `*` accepts any scope of this API | `access_as_user` |
| `PUSH_ENTRA_REQUIRED_ROLE` | an app role `roles` must contain | none |
| `PUSH_ENTRA_ALLOWED_CLIENT_APPS` | client app ids allowed in `azp`, comma-separated | any |
| `PUSH_ENTRA_MAX_SESSION_HOURS` | maximum session age of an Entra-backed device, 1 to 720 | `24` |
| `PUSH_AUTH_ADMIN_TOKEN` | the admin API's bearer token, 32 to 256 visible ASCII characters; unset: no admin listener | none |
| `PUSH_AUTH_ADMIN_ADDR` | address of the admin listener | `0.0.0.0:8082` |
| `HTTPS_PROXY`, `NO_PROXY` | standard proxy variables, honoured for the Entra requests only | none |

The `PUSH_ENTRA_*` variables are read only when `entra` is a provider.

| `PUSH_ENTRA_ALLOWED_TENANTS` | Admits `tid` | Keys from |
|---|---|---|
| `common` | any | `{authority}common/…` |
| `organizations` | any but `9188040d-6c67-4c5b-b112-36a304b66dad` | `{authority}organizations/…` |
| `consumers` | `9188040d-6c67-4c5b-b112-36a304b66dad` only | `{authority}consumers/…` |
| `<guid>[,<guid>…]` | exactly those | `{authority}{tid}/…` each |

### worker

| Variable | Meaning | Default |
|---|---|---|
| `PUSH_EMAIL_SENDER` | `log`, `resend` or **`none`** (no email at all: an `email.send` job is buried unsent; `PUSH_RESEND_WEBHOOK_SECRET` and the DEV ONLY mailbox are refused) | `log` |

### Egress

auth-svc reaches `https://{authority host}` (`login.microsoftonline.com` in the global
cloud) on 443 for the metadata and key set of each key source, and nothing else for
sign-in.

## 4. API

| Method and path | Listener | Auth | Body | Answer |
|---|---|---|---|---|
| `POST /v1/auth/entra` | public | none (the Entra token in the body) | `{token, handle?, device_id?}` | `{registration_token, expires_in_s}` or tokens |
| `PATCH /v1/users/me` | public | access token | `{display_name?, handle?}` (at least one) | own profile `{user_id, handle, display_name, email}` (`email` may be `null`) |
| `POST /v1/admin/users/revoke-sessions` | admin | `Bearer PUSH_AUTH_ADMIN_TOKEN` | exactly one of `{user_id}`, `{handle}`, `{entra: {tenant_id, object_id}}` | `{user_id, devices_revoked}` |

New error codes: `entra_token_invalid` (401), `entra_unavailable` (503, retryable),
`session_expired` (401).

## 5. Sign-in flow

```text
browser / app                      Entra ID                         auth-svc
  MSAL: auth code + PKCE  ───────▶  /authorize, /token
  ◀──── access token (aud = API, scp = access_as_user)
  POST /v1/auth/entra {token} ─────────────────────────────────────▶ validate (D2-D4)
                                                                     find or create (D5, D6)
  ◀──── {registration_token, expires_in_s} ─────────────────────────
  POST /v1/devices (Bearer registration_token) ────────────────────▶ bound to xid (D7)
  ◀──── {access_token, refresh_token, …} (session_expires_at set, D8)
  … refresh until the session ends, then 401 session_expired …
  POST /v1/auth/entra {token, device_id} ──────────────────────────▶ same device, new session
```

Browser apps use MSAL.js with a SPA redirect URI and request
`api://<api-client-id>/access_as_user`; native apps use MSAL for their platform. Neither
has a client secret. The guide (`docs/entra-sign-in.md`) covers both registrations,
`requestedAccessTokenVersion: 2`, tenant restriction and the keywords' implications, user
assignment, Conditional Access, handle changes, the admin API and switching modes.

## 6. Data model

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
`AND account.password_hash IS NOT NULL`. The worker's grants are unchanged. Message-svc is
unchanged: it learns new users from `user.verified` and revoked devices from
`device.revoked`.

## 7. Verification

Local proof without Entra ID (Plan 13A), with the stub issuer:

| AC | Local evidence |
|---|---|
| AC1 | Unchanged existing tests; config defaults; `/v1/auth/entra` 404 by default; no admin listener by default; chart default render unchanged (13B) |
| AC2 | API test: every password route 404 in Entra-only mode, no `email.send`; whole-stack test: two Entra users sign in with the SDK, register, become contacts and chat, worker with `PUSH_EMAIL_SENDER=none` |
| AC3 | Unit tests of the claim rules; verifier tests with real signatures for every refusal listed; API test: no account created |
| AC4 | Service and API tests: same `(tid, oid)` with a changed UPN, name and email; a password account and another Entra user with the same address; the same `oid` in another tenant |
| AC5 | Service and API tests: session moved into the past, refresh 401 `session_expired`, Entra sign-in with `device_id` restores the same device |
| AC6 | Key-set tests: rotation, cooldown, outage before the first fetch, stale keys during an outage, foreign `jwks_uri`, wrong issuer, timeout |
| AC7 | All of the above |
| AC8 | Settings tests for every accepted and refused form; verifier tests: `organizations`, `consumers`, `common` admit exactly their tenants and share one key source (one metadata fetch for many `tid`s) |
| AC9 | API tests for password and Entra users: rename, old handle free, 409, 400, 429, lookup; SDK and bindings tests |
| AC10 | Admin router tests: each selector, 401, 400, 404, 429, outbox `device.revoked` per device, refresh refused, security event without the token; config tests for the listener |
| AC11 | Service test: mixed accounts, sweep revokes only password devices with events, second run 0, refresh refused, password re-enabled revives nothing and a new sign-in gives a new device; server test: Entra-only start-up sweeps before ready |

Plan 13B adds the chart checks and a minikube rehearsal (Entra-only, the DEV ONLY
in-cluster issuer extended with OpenID metadata and user tokens, the admin port reachable
only from its peers). The consumer's acceptance on a real tenant follows the guide.

## 8. Delivery

| Plan | Delivers |
|---|---|
| 13A | auth-svc: configuration, validation, key cache, accounts, sign-in endpoint, session age, handle change, admin listener, Entra-only sweep; worker `none` sender; SDK and bindings; stub issuer; tests; README and `docs/entra-sign-in.md` |
| 13B | Chart values, validation, env, Ingress and Secret changes for Entra-only, the admin port, Secret key and NetworkPolicy, auth egress and proxy, guide section, minikube rehearsal (depends on 12B) |

## 9. Owner decisions (2026-10-18)

| Question | Decision |
|---|---|
| Tenant keywords | Allowed when configured explicitly, exactly one keyword or a list of ids (D3) |
| Handle of a new Entra user without a requested one | Random `u…` (D6) |
| Handle change | In scope, for all users (D14) |
| Admin API to sign a user out of all devices | Yes, separate listener and token; no unlink option (D15) |
| Custom signing keys | Not supported (D13) |
| Switching to Entra-only | Signs out password devices immediately (D16) |

Out of scope: account linking, SCIM or Graph lookups, group claims, v1.0 tokens, other
identity providers, an admin CLI, and provisioning Entra ID or Azure.
