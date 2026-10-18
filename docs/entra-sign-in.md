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
in-cluster names) on auth-svc. Nothing else calls Entra ID. When Entra ID cannot be reached
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
