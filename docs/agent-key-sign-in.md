# Agent sign-in with a key pair

An owned agent can sign in with a P-256 key pair of its own instead of an Entra app
token: the `agent-key` provider. The agent signs a short-lived assertion with its private
key; auth-svc stores only the key's thumbprint. The key is bound to an agent when it
redeems the one-time enrollment code of the agent's human owner. Design:
[spec 2026-10-26](superpowers/specs/2026-10-26-agent-key-provider-design.md).

## Why

`entra-app` needs an app-only Entra token, and so a client secret, a certificate, a
managed identity or a federated credential. Many tenants forbid secrets and certificates,
and a workstation or a Cloud PC has no managed identity and no OIDC issuer. An agent that
runs on a person's own machine can still keep a private key, for example in the TPM, and
`agent-key` needs nothing else: no Entra registration, no secret stored anywhere.

Owners, reservations, the agent management API, devices, contacts and deletion work as for
`entra-app` ([owned agents](entra-sign-in.md#owned-agents-and-app-only-authentication-opt-in)).
Only the credential differs.

## Enable it

| Variable | Value |
|---|---|
| `PUSH_AUTH_PROVIDERS` | add `agent-key`, e.g. `password,agent-key` or `password,entra,entra-app,agent-key`. Off by default |
| `PUSH_AUTH_AGENT_KEY_AUDIENCE` | the `aud` every assertion must carry: 1 to 256 visible ASCII characters. Default: `PUSH_AUTH_PUBLIC_URL` without a trailing `/` |
| `PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS` | how long a device signed in with a key stays signed in: whole hours, 1 to 720. Default `24` |
| `PUSH_AGENTS_PER_USER` | unchanged: active and pending agents per owner, 0 to 100 (0 disables reservation and redemption). Default `2` |

The two `PUSH_AUTH_AGENT_KEY_*` variables are read only with `agent-key`; an invalid value
stops auth-svc at start-up. `agent-key` needs no `PUSH_ENTRA_*` variable and no egress.
Without `password` the email and password routes answer 404 and no email is written, as
for [`entra`](entra-sign-in.md#modes). Without `agent-key`, `/v1/auth/agent-key` answers
404, as an unknown provider does.

With the Helm chart:

```yaml
auth:
  signIn:
    providers: password,agent-key
    agentKey:
      audience: ""            # empty: auth-svc's public URL without a trailing /
      maxSessionHours: 24     # 1-720
  agentsPerUser: 2
```

The chart checks both values before it installs anything and renders
`PUSH_AUTH_AGENT_KEY_AUDIENCE` only when `audience` is set. The `/v1/auth` Ingress route
already carries `/v1/auth/agent-key`.

**Reservations without `entra-app`.** `POST /v1/agents` works when `entra-app` or
`agent-key` is offered. With `entra-app` the tenant is chosen as before (the owner's Entra
tenant if it is allowed, otherwise the one allowed tenant). When no tenant can be chosen,
or `entra-app` is off, the reservation has no tenant and only `agent-key` can redeem it;
`entra-app` refuses it with 403 `enrollment_not_permitted`. A reservation that has a
tenant can be redeemed by either provider: for `agent-key` the owner's reservation and its
code are the authority, and the tenant is not checked.

## The assertion

The token is a compact JWS: `header.payload.signature`, each part base64url without
padding. auth-svc refuses anything else.

**Header**: exactly these three members, in any order:

```json
{"alg":"ES256","typ":"push-agent+jwt","jwk":{"kty":"EC","crv":"P-256","x":"<43 characters>","y":"<43 characters>"}}
```

- `alg` is `ES256` and `typ` is `push-agent+jwt`.
- `jwk` has exactly `kty` `EC`, `crv` `P-256`, `x` and `y`: the public key. `x` and `y`
  are the 32-byte coordinates, base64url (43 characters, canonical trailing bits). The
  point must be on the curve.
- Any other member is refused, in the header (`kid`, `crit`, `jku`, `x5u`, `x5c`, `x5t`,
  `b64`, `zip`, …) and in `jwk` (`d`, `kid`, `use`, `key_ops`, `alg`, …), as are duplicate
  members and arrays in place of the objects.

**Claims**: exactly these four members, in any order:

```json
{"aud":"https://api.example.com","iat":1793000000,"exp":1793000120,"jti":"0f8e4c2b9a7d4e1f8c3b6a5d2e9f1a7c"}
```

- `aud`: one string equal to the configured audience (an array is refused).
- `iat` and `exp`: whole seconds since the epoch, with `iat < exp <= iat + 300`.
- `jti`: 16 to 128 characters of `A-Z a-z 0-9 - _ .`, new for each assertion.

**Signature**: ECDSA P-256 with SHA-256 over the ASCII bytes of `header.payload`, as the
64-byte `r || s` of JWS (RFC 7518 §3.4). A DER signature (70 to 72 bytes) is refused.
Both `s` and `n - s` are accepted.

**Time and size**: with 60 seconds of clock skew, `iat` may be at most 60 s in the future
and `exp` must be later than 60 s ago. The token is at most 4096 bytes.

**Single use**: each `(key, jti)` is accepted once. auth-svc records it in PostgreSQL
(`auth.agent_key_assertion`), shared by every replica, after the signature and claims
check and before any account is looked up. An entry is kept until `exp` plus 60 s and
purged by later sign-ins five minutes after that. A token refused afterwards, for example
for a spent enrollment code, has still used its `jti`: mint a new assertion for every
request. If the database cannot be reached the answer is 503 `identity_unavailable`
(retry with a new assertion).

**The key's identity** is its RFC 7638 thumbprint: SHA-256 over the canonical JSON of the
public key (the members `crv`, `kty`, `x`, `y` in that order, no white space), in base64url
(43 characters). For the curve's base point (private key 1, for illustration only):

```text
canonical:  {"crv":"P-256","kty":"EC","x":"axfR8uEsQkf4vOblY6RA8ncDfYEt6zOg9KE5RdiYwpY","y":"T-NC4v4af5uO5-tKfA-eFivOM1drMV7Oy7ZAaDe_UfU"}
thumbprint: xx0BcA-wMohw8atYDJOe6peGModklG2wRHBlXHMvl0M
```

auth-svc computes it from the key the signature was checked with, so a token can only
ever speak for its own key. The identity is `(agent-key, agent-key, <thumbprint>)`.

## Enroll and operate an agent

1. As a signed-in human owner, call `POST /v1/agents` with your device access token and
   `{"handle":"build-agent","display_name":"Build agent"}`. HTTP 201 answers
   `{user_id, enrollment_code, expires_at, expires_in_s:600}`. Give the code to the agent
   over a channel you trust; it works once, for ten minutes.
2. The agent creates its key (once) and calls `POST /v1/auth/agent-key` with
   `{"token":"<assertion>","enrollment_code":"<code>"}`. The answer is
   `{user_id, registration_token, expires_in_s}`: the agent's account is active and bound
   to the key. The token may also be sent as `Authorization: Bearer <assertion>`.
3. The agent registers a device with the registration token at `POST /v1/devices`
   (`Authorization: Bearer <registration_token>`,
   `{"name":"…","platform":"desktop","registration_id":"<new UUID>"}`; platforms are `ios`,
   `android`, `web`, `desktop`, `other`) and uses its access and refresh tokens and
   `/v1/socket` normally. Owner and agent become accepted contacts, as for `entra-app`.
4. Later, `{"token":"<new assertion>"}` gives a new registration token, and
   `{"token":"<new assertion>","device_id":"<id>"}` signs that device in again: new
   tokens and a new session. A refresh past `PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS`
   answers 401 `session_expired`; sign the device in again with a new assertion.
   A `handle` in the body is ignored: no sign-in ever creates an account.

`GET /v1/agents` lists each agent with `credential` (`"agent-key"`, `"entra-app"`, or
`null` while pending) and `key_thumbprint` (the thumbprint for `agent-key`, otherwise
`null`); `client_app_id` stays `null` for key agents. Before you hand over a code, ask the
agent for its thumbprint; after enrollment, check that the listing shows the same one.

`DELETE /v1/agents/{user_id}` disables the agent for good, revokes its devices and refresh
tokens, and releases its key (the identity row moves to `auth.released_identity`). The key
then gets 401 `token_invalid`; with a fresh code it enrolls a new agent.

**Key rotation is not supported yet.** To replace a key, delete the agent and enroll a new
one with the new key: that is a new account, with a new user id.

### Answers

| Status | Code | When |
|---|---|---|
| 200 | | enrolled, or signed in |
| 400 | `enrollment_invalid` | the code is wrong, expired or spent |
| 400 | `invalid` | a malformed body, or both `enrollment_code` and `device_id` |
| 401 | `token_invalid` | the assertion is refused (see the reasons below), including a replay and a key that no agent has enrolled; no account is created |
| 403 | `enrollment_not_permitted` | `PUSH_AGENTS_PER_USER` is `0` |
| 403 | `sign_in_not_permitted` | an admission of your own refused the agent |
| 404 | | `agent-key` is not enabled |
| 409 | `identity_in_use` | the key is already bound to a live agent; the code is not spent |
| 429 | `rate_limited` | the limits below |
| 503 | `identity_unavailable` | the replay cache (PostgreSQL) cannot be reached |

The route shares the token sign-in limits: 60 requests per 15 minutes per IP, and 20 per
15 minutes per key once its assertion is accepted.

### Events

Every refused assertion is logged under the `security` target as `auth.external.rejected`
with `provider=agent-key` and a `reason` (never the token): `size`, `format`, `header`,
`algorithm`, `type`, `key`, `signature`, `claims`, `audience`, `lifetime`, `expired`,
`not_yet_valid`, `replay`, `unenrolled_key`. Answers of `/v1/auth/agent-key` are audited as
`auth.agent-key.success` and `auth.agent-key.failure` (ordinary malformed bodies and 503s
are not; `enrollment_invalid` is). Enrollment logs `auth.agent.enrolled` with
`provider=agent-key`, and deletion `auth.agent.identity_released`.

## Client example (PowerShell, Windows)

An example for PowerShell 7.4 or later on Windows. It keeps a persisted, non-exportable
P-256 key in the TPM (`Microsoft Platform Crypto Provider`), or in the
`Microsoft Software Key Storage Provider` where there is no usable TPM, and opens the same
key on later runs. Only the public key ever leaves the key store. The SDK and its bindings
have no agent helpers.

```powershell
$ErrorActionPreference = 'Stop'
$audience = 'https://api.example.com'   # PUSH_AUTH_AGENT_KEY_AUDIENCE or the public URL
$keyName = 'push-messenger-build-agent' # one key per agent and Windows user

function ConvertTo-Base64Url([byte[]] $Bytes) {
    [Convert]::ToBase64String($Bytes).TrimEnd('=').Replace('+', '-').Replace('/', '_')
}

# Opens the agent's key, or creates it: in the TPM if it can, otherwise in software.
function Get-AgentKey([string] $Name) {
    $providers = 'Microsoft Platform Crypto Provider', 'Microsoft Software Key Storage Provider'
    foreach ($providerName in $providers) {
        $provider = [System.Security.Cryptography.CngProvider]::new($providerName)
        if ([System.Security.Cryptography.CngKey]::Exists($Name, $provider)) {
            return [System.Security.Cryptography.CngKey]::Open($Name, $provider)
        }
    }
    foreach ($providerName in $providers) {
        $parameters = [System.Security.Cryptography.CngKeyCreationParameters]::new()
        $parameters.Provider = [System.Security.Cryptography.CngProvider]::new($providerName)
        $parameters.ExportPolicy = [System.Security.Cryptography.CngExportPolicies]::None
        $parameters.KeyCreationOptions = [System.Security.Cryptography.CngKeyCreationOptions]::None
        try {
            return [System.Security.Cryptography.CngKey]::Create(
                [System.Security.Cryptography.CngAlgorithm]::ECDsaP256, $Name, $parameters)
        } catch {
            Write-Warning "$providerName cannot create the key: $($_.Exception.Message)"
        }
    }
    throw 'no key storage provider could create the key'
}

# The public key as a JWK; ExportParameters($false) works for a non-exportable key.
function Get-AgentJwk([System.Security.Cryptography.ECDsa] $Key) {
    $public = $Key.ExportParameters($false)
    [ordered]@{
        kty = 'EC'; crv = 'P-256'
        x = ConvertTo-Base64Url $public.Q.X
        y = ConvertTo-Base64Url $public.Q.Y
    }
}

# RFC 7638: the canonical JSON is built by hand, members in this exact order.
function Get-AgentThumbprint([System.Security.Cryptography.ECDsa] $Key) {
    $jwk = Get-AgentJwk $Key
    $canonical = '{"crv":"P-256","kty":"EC","x":"' + $jwk.x + '","y":"' + $jwk.y + '"}'
    ConvertTo-Base64Url ([System.Security.Cryptography.SHA256]::HashData(
        [System.Text.Encoding]::ASCII.GetBytes($canonical)))
}

# A new single-use assertion, valid for two minutes.
function New-AgentAssertion([System.Security.Cryptography.ECDsa] $Key, [string] $Audience) {
    $now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
    $header = [ordered]@{ alg = 'ES256'; typ = 'push-agent+jwt'; jwk = Get-AgentJwk $Key }
    $claims = [ordered]@{
        aud = $Audience; iat = $now; exp = $now + 120
        jti = [Guid]::NewGuid().ToString('N')
    }
    $encode = { param($value) ConvertTo-Base64Url (
        [System.Text.Encoding]::UTF8.GetBytes(($value | ConvertTo-Json -Compress -Depth 3))) }
    $signed = (& $encode $header) + '.' + (& $encode $claims)
    # .NET signs in the IEEE P1363 format: the 64-byte r || s that ES256 needs.
    $signature = $Key.SignData([System.Text.Encoding]::ASCII.GetBytes($signed),
        [System.Security.Cryptography.HashAlgorithmName]::SHA256)
    $signed + '.' + (ConvertTo-Base64Url $signature)
}

$key = [System.Security.Cryptography.ECDsaCng]::new((Get-AgentKey $keyName))
"Key thumbprint (give it to the owner): $(Get-AgentThumbprint $key)"
```

Then, with that key:

```powershell
$api = 'https://api.example.com'
$json = 'application/json'

# Once: redeem the owner's enrollment code and register this machine as a device.
$code = Read-Host 'Enrollment code'
$enrolled = Invoke-RestMethod -Method Post -Uri "$api/v1/auth/agent-key" -ContentType $json `
    -Body (@{ token = (New-AgentAssertion $key $audience); enrollment_code = $code } | ConvertTo-Json)
$registrationId = [Guid]::NewGuid().ToString()   # send the same id again on a retry
$device = Invoke-RestMethod -Method Post -Uri "$api/v1/devices" -ContentType $json `
    -Headers @{ Authorization = "Bearer $($enrolled.registration_token)" } `
    -Body (@{ name = $env:COMPUTERNAME; platform = 'desktop'; registration_id = $registrationId } |
        ConvertTo-Json)
# Keep $device.device_id; keep $device.refresh_token as a secret (e.g. DPAPI-protected).

# Later, or after session_expired: sign the same device in again.
$tokens = Invoke-RestMethod -Method Post -Uri "$api/v1/auth/agent-key" -ContentType $json `
    -Body (@{ token = (New-AgentAssertion $key $audience); device_id = $device.device_id } |
        ConvertTo-Json)
```

`$key.Key.Delete()` removes the key from the key store for good (after the agent is
deleted, for example).

## Security notes

- Parsing is strict: exact members, no duplicates, strict base64url, a fixed algorithm and
  a bounded size and lifetime. Unknown header members, key ids and key URLs are refused,
  so the server never fetches a key and never chooses an algorithm.
- An agent is bound to its key's thumbprint, only through the owner's one-time code. A key
  that no agent has enrolled gets 401 and never creates an account.
- Each assertion is single-use, enforced in PostgreSQL across every replica; a database
  failure refuses the sign-in (fail closed).
- Keep the private key non-exportable, preferably in the TPM. Anyone who can use the key
  can sign in as the agent until the owner deletes it.
- Key rotation is not supported yet: delete the agent and enroll a new one.
