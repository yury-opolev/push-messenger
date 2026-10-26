# Agent-key provider: a key-pair credential for owned agents

Status: approved (autonomous run; the owner was away and delegated every decision)
Request: "REQUEST PUSH feature 6" (consumer repo yuopolev/push-messenger)
Builds on: owned agents (`agents.rs`, migration 0006) and identity release on delete
(PR #46, migration 0008).

## Problem

Owned agents can sign in only with an Entra app-only token (`entra-app`). Such a token
needs an app secret or certificate, a managed identity or a federated credential. Many
tenants forbid secrets and certificates, and workstations and Cloud PCs have no managed
identity and no OIDC issuer. An agent that runs on a person's own machine therefore
cannot become an agent at all.

## Decision summary

A new, opt-in built-in authenticator `agent-key`. The agent proves possession of an
ECDSA P-256 private key with a short-lived compact JWS. Push-messenger stores only the
key's RFC 7638 thumbprint. The key is bound to an agent by redeeming the owner's
one-time enrollment code, exactly as `entra-app` does. An unknown key never creates an
account.

## D1. Assertion format

The token is a compact JWS (`header.payload.signature`, base64url without padding).

Protected header, exactly these members (any other member is refused, including
`crit`, `kid`, `jku`, `x5u`, `x5c`, `x5t`, `b64`, `zip`; duplicate members are refused):

```json
{"alg":"ES256","typ":"push-agent+jwt","jwk":{"kty":"EC","crv":"P-256","x":"<43 chars>","y":"<43 chars>"}}
```

- `alg` is exactly `ES256`. `none`, `HS256`, `RS256`, `ES384`, `EdDSA` are refused.
- `typ` is exactly `push-agent+jwt`.
- `jwk` has exactly `kty` = `EC`, `crv` = `P-256`, `x`, `y`. Any other member, notably
  the private `d`, is refused, as are `kid`, `use`, `key_ops`, `alg`, `x5c`.
- `x` and `y` each decode (strict base64url, no padding, canonical trailing bits) to
  exactly 32 bytes. The point `04 || x || y` must be on the curve and not the identity;
  ring checks this when it parses the public key.

Claims, exactly these members (any other is refused, duplicates are refused):

```json
{"aud":"https://api.example.com","iat":1793000000,"exp":1793000120,"jti":"<16-128 chars>"}
```

- `aud` is one string equal to the configured audience. An array is refused.
- `iat` and `exp` are integers (seconds since the epoch).
- `jti` is 16 to 128 characters from `A-Z a-z 0-9 - _ .`.

Signature: exactly 64 bytes `r || s` (JWS ES256, RFC 7518 §3.4), verified with ring's
`ECDSA_P256_SHA256_FIXED` over the ASCII bytes `header.payload`. DER signatures, which
are 70 to 72 bytes, are refused by length. A high-S signature is accepted: the token is
single-use and bound to the key, so malleability gives an attacker nothing.

Time, with a clock skew of 60 seconds:

- `exp > iat`, and `exp - iat <= 300`;
- `iat <= now + 60` (not issued in the future);
- `exp > now - 60` (not expired).

The token is at most 4096 bytes. The identity middleware already caps tokens at 16 KiB.

Refusal reasons go to the security log (`auth.external.rejected`, `provider=agent-key`,
`reason`) and the answer is 401 `token_invalid`, as for every external provider.
Reasons: `size`, `format`, `header`, `algorithm`, `type`, `key`, `signature`, `claims`,
`audience`, `lifetime`, `expired`, `not_yet_valid`, `replay`, `unenrolled_key`.

## D2. Replay cache

Each `(thumbprint, jti)` can be used once. The verifier records it after the signature
and claims are checked, in a new table `auth.agent_key_assertion (thumbprint, jti,
expires_at)` with primary key `(thumbprint, jti)` and an index on `expires_at`:
`INSERT … ON CONFLICT DO NOTHING RETURNING 1`. No row returned means a replay.
`expires_at` is `exp + 60 s`. Before the insert, a bounded batch (100 rows,
`FOR UPDATE SKIP LOCKED`) of rows more than five minutes past `expires_at` is deleted.
The margin keeps a database clock that runs ahead of a replica's from purging an entry
the replica would still accept. The table holds only the assertions of the last few
minutes.

Why PostgreSQL and not Redis: Redis holds only rate-limit counters, which may be lost
or evicted without harm. A lost or evicted replay entry would reopen a replay. PostgreSQL
is durable, shared by every replica, and already required on every sign-in. Sign-ins are
rate limited, so the extra write is cheap. A database error answers 503
`identity_unavailable` (fail closed, retryable).

A token that is refused later, for example because its enrollment code is spent, has
still used up its `jti`. The client mints a new assertion for each call.

## D3. Principal

`provider = "agent-key"`, `issuer = "agent-key"` (a constant), and
`subject = base64url(SHA-256({"crv":"P-256","kty":"EC","x":"…","y":"…"}))`: the
RFC 7638 thumbprint, 43 characters. The thumbprint is computed from the members the
signature was checked with, so a substituted key cannot match another agent. `claims`
holds the verified claims and `name` and `email` are `None`.

## D4. Enrollment and sign-in

`POST /v1/auth/agent-key {token, enrollment_code}` redeems a reservation with
`enroll_agent`, which is now provider-generic for the two agent providers (`entra-app`,
`agent-key`):

- the identity advisory lock key is `"{provider}:{issuer}:{subject}"` (unchanged for
  `entra-app`);
- `identity_in_use` (409) when `(provider, issuer, subject)` is already bound;
- reservation lookup, owner lock, expiry and spending the code are unchanged;
  `enrollment_invalid` (400) for a bad, expired or spent code;
- the tenant check (`tenant == principal.issuer`) applies to `entra-app` only. For
  `agent-key` the owner's reservation and the code are the authority;
- `client_app_id` is recorded for `entra-app` only (`azp`).

After that, `{token}` and `{token, device_id}` sign in through the existing
`sign_in_external` and `sign_in_external_device`.

**No account on first sign-in.** `create_external` refuses both agent providers. An
unknown `entra-app` principal keeps its 403 `sign_in_not_permitted`. An unknown
`agent-key` principal answers **401 `token_invalid`** (reason `unenrolled_key`): a
self-made key is not a vouched-for identity, so "a token of another key" is an invalid
credential. Without this a released key would create a human account (PR #46).

## D5. Reservations without `entra-app`

Today `reserve_agent` requires `entra-app` and records a tenant. Now a reservation is
possible when either agent provider is offered:

- With `entra-app` offered, the tenant is chosen as today (the delegated owner's tenant
  if it is allowed; otherwise the one allowed tenant, if there is exactly one).
- If no tenant can be chosen: with `agent-key` offered, the reservation stores a NULL
  tenant. It can then be redeemed only with `agent-key`: `entra-app` refuses a NULL
  tenant with `enrollment_not_permitted`. Without `agent-key` the answer stays
  `enrollment_not_permitted`.
- With neither provider offered: `enrollment_not_permitted`, as today.

Migration 0009 makes `auth.agent_enrollment.tenant_id` nullable; its nil-GUID check
stays. During a rolling upgrade an old pod that redeems a NULL-tenant reservation fails
that request only. Such reservations exist only once a new pod with `agent-key` serves
reservations.

## D6. Human owners

`owner()` treats an account as human only if it has no agent identity: the
`NOT EXISTS` clause now covers `provider IN ('entra-app', 'agent-key')`. One constant
list, `AGENT_PROVIDERS`, names both agent providers in Rust.

## D7. Agent listing

`GET /v1/agents` adds two fields to each agent:

- `credential`: `"entra-app"`, `"agent-key"`, or `null` while pending;
- `key_thumbprint`: the thumbprint for `agent-key`, otherwise `null`.

`client_app_id` stays. It is null for key agents.

## D8. Configuration

- `PUSH_AUTH_PROVIDERS` accepts `agent-key`. It is off by default. `Providers` gets an
  `agent_key` field.
- `PUSH_AUTH_AGENT_KEY_AUDIENCE`: the required `aud`. 1 to 256 visible ASCII characters.
  The default is `PUSH_AUTH_PUBLIC_URL` without a trailing `/`. It is read only with
  `agent-key`.
- `PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS`: 1 to 720, default `24`, read only with
  `agent-key`.
- `PUSH_AGENTS_PER_USER` applies unchanged.
- Helm: `auth.signIn.providers` accepts `agent-key`, and
  `auth.signIn.agentKey.{audience,maxSessionHours}` render the two variables (audience
  only when set). The chart validates them.
- `check_overrides` refuses an `Overrides` authenticator with the id `agent-key`, as it
  does for `entra-app`. An admission is allowed with `agent-key` alone.

`Auth::with_agent_key(AgentKeySignIn)` offers the route. `AgentKeySignIn::new(settings,
pool)` is built in `serve_with` once the pool exists. Without the provider the route
does not exist and answers 404, as an unknown provider does.

## D9. Audit and rate limits

The route uses the shared identity middleware: the per-IP sign-in limit (`EntraSignIn`),
admission, and the per-identity limit (`EntraAccount`, key `agent-key:agent-key:<thumbprint>`).
The audit middleware classifies `/v1/auth/agent-key` answers as
`auth.agent-key.success` and `auth.agent-key.failure`, with the same status rules as
`auth.external.*` (ordinary 400s and 503s are not audited; `enrollment_invalid` is).
Enrollment logs `auth.agent.enrolled` with `provider`. Deletion logs
`auth.agent.identity_released`, unchanged.

## D10. Lifecycle

Deleting an active agent disables it, revokes its devices and refresh tokens, and moves
its `agent-key` identity row to `auth.released_identity`. This is already
provider-agnostic since PR #46. The key then has no account and gets 401. It can enroll
again with a fresh code.

**Key rotation is not implemented here.** Rotation needs a new owner route that issues
a code for an active agent, and the `agent_enrollment` invariant
`(enrolled_at IS NULL) = (code_hash IS NOT NULL)` would have to change, together with
expiry and redemption semantics for an active account. It is recorded as a follow-up.
The workaround is to delete the agent and enroll a new one.

## D11. SDK and bindings

The SDK and the bindings have no agent or `entra-app` helpers; they cover delegated
Entra sign-in only. No change. The docs give a client example.

## D12. Documentation

`docs/agent-key-sign-in.md` covers how to enable the provider (environment and Helm),
the assertion format, the enrollment and sign-in flow, errors, and a client example in
PowerShell/.NET. The example creates a non-exportable P-256 key in the
`Microsoft Platform Crypto Provider` (TPM), falling back to the
`Microsoft Software Key Storage Provider`. Also updated: `docs/entra-sign-in.md` (a
cross-link, and the listing fields), `docs/deployment-configuration.md`, the `config.rs`
table, and CHANGELOG "Unreleased".

## Testing

Unit tests (`agent_key`): a valid assertion verifies and gives the RFC 7638 thumbprint
(checked against a fixed vector computed independently with Python's `hashlib`). Each is
refused: aud mismatch, an `aud` array, exp - iat > 300, expired, iat in the future,
exp <= iat, `alg: none`, `alg: HS256`, missing `jwk`, `jwk` with `d`, extra header
member (`kid`, `crit`, `jku`), wrong `typ`, wrong `crv`/`kty`, a short coordinate, an
off-curve point, a DER signature, a signature by another key, a tampered payload, an
oversized token, padding characters, extra claims, a bad `jti`.

Integration tests (real PostgreSQL):

- reserve X, redeem with key K: 200 and X active; `{token}` with K: a registration
  token for X, and a device session; a token with another key K2: 401; a K-header token
  signed by K2: 401;
- a reused `jti`: 401;
- two live agents cannot share K: `identity_in_use`; an expired or spent code:
  `enrollment_invalid`;
- an unknown key without a code: 401 and no account created;
- with `agent-key` not enabled, `/v1/auth/agent-key` is 404, like an unknown provider;
- `GET /v1/agents` shows `credential` and `key_thumbprint`;
- deleting the agent releases the key; the key then gets 401, and a fresh code enrolls
  it again;
- an agent-key agent is not a human owner (cannot reserve);
- reservation works with only `agent-key` offered (NULL tenant), and `entra-app` cannot
  redeem a NULL-tenant reservation;
- audit classifies `auth.agent-key.success` and `auth.agent-key.failure`.

Config and chart: provider parsing, the audience default and validation, the
max-session range, the `check_overrides` refusal, and the chart render and refusal
checks in `test-chart.ps1`.
