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
| `password` (default) | Today's email and password sign-up and sign-in, with email confirmation and password reset by email (an email provider is needed) |
| `entra` | Entra ID only: no email confirmation, no passwords, no email provider. The signup, verification, login and password-reset routes answer 404; no email is ever written, so the worker runs with `PUSH_EMAIL_SENDER=none` |
| `password,entra` | Both; the two kinds of accounts are never linked, even with the same address |
| `external`, alone or with the others | The sign-in of a push-auth of your own: its authenticator at `/v1/auth/{its provider id}` ([Your own wrapper](#your-own-wrapper)) |

**Switching an existing installation to `entra` is destructive for password sessions.**
When auth-svc starts without `password`, it signs every device of every email/password
account out before it reports ready: refresh fails, open connections close within seconds,
and their access tokens stop working. While `password` is off it checks again every minute,
so devices that replicas of the previous version sign in during a rolling update are
signed out too. Those accounts cannot sign in any more. Entra ID
sign-in does not link to them: the same people get **new, empty accounts** (new user ids,
no contacts or history). Switching `password` back on later revives none of the signed-out
devices; those users sign in again with their password as new devices.

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
| `PUSH_ENTRA_ALLOWED_TENANTS` | your tenant id, or several, comma-separated; or exactly one of `common`, `organizations`, `consumers` (see below). Never a mixture |
| `PUSH_ENTRA_AUTHORITY_HOST` | `https://login.microsoftonline.com/` (default) or your sovereign cloud's host |
| `PUSH_ENTRA_REQUIRED_SCOPE` | `access_as_user` (default) |
| `PUSH_ENTRA_REQUIRED_ROLE` | optional app role value |
| `PUSH_ENTRA_ALLOWED_CLIENT_APPS` | optional client app ids |
| `PUSH_ENTRA_MAX_SESSION_HOURS` | `24` (default), 1 to 720 |

And for the worker in Entra-only mode: `PUSH_EMAIL_SENDER=none`.

### Which tenants

`PUSH_ENTRA_ALLOWED_TENANTS` is exactly one of these; anything else (a mixture such as
`common,<id>`, a duplicate, an empty entry) stops auth-svc at start-up:

| Value | Who can sign in |
|---|---|
| `<tenant id>[,<tenant id>…]` | users of exactly these tenants (recommended) |
| `organizations` | users of **any** Microsoft Entra tenant (work or school accounts) |
| `consumers` | **any** personal Microsoft account |
| `common` | **any** work, school or personal Microsoft account |

**With `organizations`, `consumers` or `common`, anyone who has such an account and can get
a token for your API creates an account here at their first sign-in.** The API registration
must then be multi-tenant (or allow personal accounts), and the only gates are its user
assignment, consent and Conditional Access, and `PUSH_ENTRA_REQUIRED_ROLE` /
`PUSH_ENTRA_ALLOWED_CLIENT_APPS`. Use a keyword only when that is what you want. Each token's
issuer is still checked against the tenant in the token, and auth-svc fetches the keyword's
one shared key set, never one per tenant.

### With the Helm chart

```yaml
auth:
  signIn:
    providers: entra            # or "password,entra"
    entra:
      clientIds:
        - 11111111-0000-4000-8000-000000000001   # the API's application id
        # - api://push-api                       # optionally its URI: one per entry, no commas
      tenants:
        - 33333333-0000-4000-8000-000000000003   # your tenant id(s); or exactly one of
                                                 # common, organizations, consumers
      maxSessionHours: 24
      # requiredRole: Messenger.User
      # allowedClientApps: [22222222-0000-4000-8000-000000000002]
      # httpsProxy: http://proxy.corp.example:3128
      # caConfigMap: corp-ca      # ca.crt: every root auth-svc must trust
  admin:
    enabled: true               # the admin API on port 8082 (Secret key auth-admin-token)
networkPolicy:
  adminPeers:                   # who may reach port 8082; empty: port-forward only
    - namespaceSelector: {kubernetes.io/metadata.name: ops}
      podSelector: {app: admin-shell}
  egress:
    enabled: true               # opt-in egress policies (deployment-azure-entra.md)
    entraSignInCidrs: []        # AzureActiveDirectory service tag ranges, or use a proxy
```

The chart checks these values before it installs anything, as auth-svc does at start-up:
the tenant forms above (a mixture, a duplicate or anything else is refused), the
application id among `clientIds`, an `https://` authority host, 1 to 720 session hours.

With `providers: entra` the chart renders `PUSH_EMAIL_SENDER=none` for the worker, no link
page Ingress, and references no Resend key: a consumer-managed Secret needs no email keys.
`worker.email.sender: resend` and `worker.email.webhook.enabled: true` are refused then, and
`worker.email.sender: none` is refused while `password` is on (its verification and reset
emails would never arrive). The `/v1/auth` Ingress route already carries `/v1/auth/entra`.
Changing `providers` from `password` or `password,entra` to `entra` signs every password
device out when the new auth-svc pods start (see [Modes](#modes)).

`httpsProxy` applies to auth-svc's requests to Entra ID only (`NO_PROXY` keeps in-cluster
names direct). `caConfigMap` replaces the system roots for **every** TLS connection auth-svc
makes, so its `ca.crt` must hold every root auth-svc needs (a TLS-inspecting proxy's root
plus the public roots); with the datastore mode's
`azure.workloadIdentity.authorityCaConfigMap` it must name the same ConfigMap.

With `admin.enabled`, reach the admin port with `kubectl port-forward deploy/push-auth
8082:8082` or from the peers in `networkPolicy.adminPeers`; it is never routed by the
chart's Ingresses. The token is the `auth-admin-token` key of the release's Secret
(generated and kept for the chart-managed Secret; yours with `secrets.existingSecret`).

## Handles

A new Entra ID account gets the handle the app asked for at its first sign-in, or a random
one such as `u7k2m9q4x1z`. Every user, Entra ID or password, changes it with
`PATCH /v1/users/me {"handle": "erin"}` (SDK: `update_profile`) with their access token: no
password or new Entra sign-in is needed, since a handle is a public name and contacts and
conversations follow the user id, not the handle. The old handle is free at once; a taken
one answers 409 `handle_taken`; 5 changes a day per account.

## Admin API

To sign a user out of every device at once (a lost laptop, a compromised account, someone
leaving), set `PUSH_AUTH_ADMIN_TOKEN` (32 to 256 visible characters, from your Secret store).
auth-svc then serves, on `PUSH_AUTH_ADMIN_ADDR` (`:8082`) only:

```powershell
kubectl port-forward -n push deploy/push-auth 8082:8082
$headers = @{ Authorization = "Bearer $env:PUSH_AUTH_ADMIN_TOKEN" }
Invoke-RestMethod -Method Post -Uri http://127.0.0.1:8082/v1/admin/users/revoke-sessions `
  -Headers $headers -ContentType application/json `
  -Body '{"entra": {"tenant_id": "<tid>", "object_id": "<oid>"}}'
# or {"handle": "erin"} or {"user_id": "<uuid>"}
# or {"external": {"provider": "<id>", "issuer": "<issuer>", "subject": "<subject>"}}
```

The answer is `{"user_id", "devices_revoked"}`. Every device is revoked, refresh fails, and
open connections close within seconds. The account stays: the person can sign in again
unless you also block them where they sign in (disable or unassign them in Entra ID; for a
password account, reset the password). There is deliberately no way to remove the Entra ID
link: object ids are never reused, so removing it would only orphan the account and make
the next sign-in create a duplicate person. Calls with the token are limited to 60 per 15 minutes per IP; failed attempts have a
separate limit (20 per 15 minutes per IP), so they never lock the token's holder out.
Never expose the admin port through an Ingress;
the chart allows it only from the peers you list (`networkPolicy.adminPeers`).

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
after their last sign-in. To end it at once, use the [admin API](#admin-api) with their
tenant and object id (or a user revokes a device with `DELETE /v1/devices/{id}`).

Users are identified by tenant and object id, never by name or address: a renamed user, a
new UPN or a changed address is the same account; someone else with the same address is
another account.

## 7. Network

auth-svc fetches, per listed tenant (or once for a keyword, with `common`, `organizations` or `consumers` in place of `<tid>`), `https://login.microsoftonline.com/<tid>/v2.0/.well-known/openid-configuration`
and the key set it names (`https://login.microsoftonline.com/<tid>/discovery/v2.0/keys`), at
start-up, about daily and when Microsoft rotates keys. Allow HTTPS (443) from auth-svc to
`login.microsoftonline.com` (FQDN rules in your firewall, or Microsoft's
`AzureActiveDirectory` service tag ranges), or set `HTTPS_PROXY` (and `NO_PROXY` for
in-cluster names) on auth-svc. With the chart's opt-in egress policies,
`networkPolicy.egress.entraSignInCidrs` opens 443 to those ranges for auth-svc alone.
Nothing else calls Entra ID. When Entra ID cannot be reached
and no keys are cached, Entra sign-ins answer 503 `entra_unavailable`; refresh, devices and
messaging keep working.

## 8. Troubleshooting

auth-svc logs every refused token as `auth.entra.rejected` with a reason (never the token):

| Reason | Usual cause |
|---|---|
| `issuer` | v1.0 tokens: set `requestedAccessTokenVersion` to 2 on the API registration |
| `audience` | the token is for another API (Graph), or an ID token was sent; request `api://<api-client-id>/access_as_user` |
| `tenant` | the user's tenant is not admitted by `PUSH_ENTRA_ALLOWED_TENANTS` (for example a personal account with `organizations`) |
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
8. A user changes their handle in the app; others find them by the new one only.
9. The admin API signs a test user out: their apps report `DeviceRemoved` within seconds.
10. Switching a test installation from `password,entra` to `entra`: every password device is
    signed out at start-up (`auth.password_sessions.revoked` in the log); Entra users are not.

## Your own wrapper

auth-svc is two crates ([spec](superpowers/specs/2026-10-19-pluggable-identity-design.md)):
`crates/push-auth-core`, the library (accounts, devices, tokens, the API, the admin API, the
Entra ID check, the migrations), and `crates/push-auth`, a thin binary that reads the
configuration and calls `push_auth_core::serve`. The image, the chart and this guide use
the binary. To change who may sign in, or how tokens are checked, write your own binary
of ~40 lines that calls `serve_with` with:

- an **`Authenticator`** (`provider()`, `max_session()`, `authenticate(token)` giving a
  `Principal { provider, issuer, subject, name, email, claims }`): served at
  `/v1/auth/{provider}` when `PUSH_AUTH_PROVIDERS` names `external`. Accounts are found by
  `(provider, issuer, subject)`, never by the address;
- an **`Admission`** (`admit(principal)` giving `Admit`, `Refuse` or `Unavailable`): asked
  after the token is accepted and before any account is found or created, for every
  sign-in by token, Entra ID's included.

`crates/push-auth/examples/custom_identity.rs` is a tested starting point: it wraps the
built-in Entra ID check (keeping the provider id `entra`, so `/v1/auth/entra` and the SDK's
`sign_in_with_entra` keep working), refuses guest accounts, and admits only the members of
the group `EXAMPLE_REQUIRED_GROUP` (the API registration must emit the `groups` claim). It
runs with `PUSH_AUTH_PROVIDERS=external` (plus `password` if wanted) and the usual
`PUSH_ENTRA_*` variables, which the wrapper reads itself (`EntraSettings::from_env`).

Rules the service enforces at start-up: `external` needs an authenticator and an
authenticator needs `external`; its id is 1 to 32 lower-case letters, digits or `-`, not a
route of auth-svc (`signup`, `verify`, `login`, `password`, `refresh`, `jwks`), and not
`entra` beside `PUSH_AUTH_PROVIDERS=entra`; its maximum session is 1 s to 720 h; an
admission needs `entra` or `external`. Anything else stops start-up with a message that
says what to change.

**Building an image.** Copy the example into a binary crate of your own that depends on
`push-auth-core` (by path, with this repository as a submodule), build it in the builder
stage of `deploy/docker/Dockerfile`, and put the binary at `/usr/local/bin/push-auth` in
the `auth` target, for example:

```dockerfile
RUN cargo build --release --locked -p my-push-auth \
    && cp target/release/my-push-auth /out/push-auth
```

Do not ship the example itself: Cargo builds examples with the package's
dev-dependencies, which enable `push-auth-core`'s `testing` feature (the stub issuer's
public test keys). For the same reason, never build your crate together with
`push-smoke`. Then set `images.auth` in the chart to your
image and `auth.signIn.providers` (`PUSH_AUTH_PROVIDERS`) to include `external`; nothing
else in the chart changes.

**Duties of a custom authenticator.** It is the only check between a token and an
account. It must verify the signature with the issuer's published keys and pin the
algorithm (never `none` or a symmetric one with a public key); check the issuer, the
audience (your API, not Graph or an ID token), `exp` and `nbf`; accept only delegated user
tokens (no client-credentials tokens); give a `subject` that is stable and immutable at
that `issuer` (Entra ID: `oid`, never the UPN or the address), at most 128 characters,
and an issuer of at most 256; and never log the token or its claims (an `Unavailable` text
must not hold them either). A principal that breaks these limits, or names another
provider, is refused as a rejected token.

**Answers and events.** The token comes from `Authorization: Bearer` or the JSON body's
`token` (both present and different: 400 `invalid`); the per-IP rate limit runs before the
authenticator. For provider `entra` the answers and events are those above. For another
provider: a rejected token is 401 `token_invalid` and `auth.external.rejected` (provider,
reason); an unavailable provider 503 `identity_unavailable`; sign-ins
`auth.external.success`/`auth.external.failure` and `auth.external.provisioned`. For every
provider, an admission's `Refuse` is 403 `sign_in_not_permitted` with
`auth.external.not_permitted` (provider, issuer) and creates no account, also on a device
renewal; its `Unavailable` is 503 `identity_unavailable`.
