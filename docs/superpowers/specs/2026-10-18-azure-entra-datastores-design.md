# Passwordless Azure PostgreSQL and Redis

Date: 2026-10-18

Implementation status: **12A** done, #35 ([plan](../plans/2026-10-18-plan-12a-entra-datastore-credentials.md));
**12B** in progress ([plan](../plans/2026-10-18-plan-12b-entra-chart.md)).

This answers the downstream change request "passwordless Azure PostgreSQL and Redis
support" (based on `d1e1bf2`, PR #23). The services can run against external, managed
PostgreSQL and Redis that accept only Microsoft Entra ID tokens from Azure Workload ID
instead of static passwords. It is opt-in: the defaults keep the bundled datastores,
password URLs, local development and tests unchanged.

The portable-deployment ownership of
[2026-10-16-portable-deployment-readiness-design.md](2026-10-16-portable-deployment-readiness-design.md)
still holds. This repository supplies the mechanism, chart values and documentation.
The consumer owns the Azure resources, identities, role assignments, private endpoints,
DNS and the acceptance run on Azure. Nothing here provisions Azure.

## 1. Requirements

From the request, with their acceptance criteria (AC1 to AC8):

- A per-datastore switch, `password` (default) or `azure-entra`.
- Tokens from workload identity (federated service-account token), falling back to
  managed identity. Never logged. Startup fails clearly when no token can be had.
- PostgreSQL: TLS with full verification, the token as password, a background refresher
  that rotates the pool's connect options before expiry, a pool `max_lifetime` below the
  token lifetime, the migration connection included, and every connection covered
  (the worker's second database too).
- Redis: TLS (`rediss://`), tokens renewed on every connection without dropping
  users, including the gateway hub's dedicated pub/sub subscriber. Non-clustered Redis
  only.
- Migrations runnable as a separate step with their own identity
  (`PUSH_RUN_MIGRATIONS=false` on the services), the role model documented, and a
  clear error when `citext` is not allow-listed.
- Chart: external datastores, one service account per service with workload-identity
  annotations, no datastore Secrets in Entra mode, an optional migration Job, egress
  NetworkPolicies, and the existing security contexts kept.
- A deployment guide section with prerequisites and limitations.

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| D1 | No Azure SDK. A small crate `push-credentials` obtains tokens with plain OAuth 2.0 calls (workload identity: client-assertion token request to Entra; managed identity: IMDS) over the `reqwest` + rustls the workspace already uses. | `redis`'s `entra-id` feature pulls `azure_identity` 0.31 with `client_certificate` (OpenSSL) and `azure_core` with native-tls; the images and Windows development builds are OpenSSL-free. Two HTTP calls are a small, fully testable surface. |
| D2 | No Cargo feature gate. Entra support is always compiled. | D1 adds no heavy dependency (reqwest, rustls, serde are already linked), so one tested binary serves every mode. The request made the gate optional. |
| D3 | MSRV stays 1.85. redis stays on 1.2.2 (the last release for 1.85) with `token-based-authentication`, `tokio-rustls-comp`. sqlx 0.8.6 gains `tls-rustls-ring-native-roots`. | Verified on crates.io: redis 1.2.3+ need 1.88; backon 1.6 (pulled by token auth) is 1.85; ring is already the only rustls provider (no aws-lc-rs). The request's "redis >= 1.0.4" is met. |
| D4 | Redis tokens reach managed connections through redis-rs's own `StreamingCredentialsProvider` (re-`AUTH` on every multiplexed connection when the token changes). The hub's pub/sub connection cannot `AUTH` while subscribed (RESP2), so on each token refresh the hub prepares a new subscriber, subscribed to every channel, while the old one keeps serving, then swaps (make-before-break). | No gap without a subscription: nothing is lost, duplicates are harmless (a ring syncs, a kick closes). |
| D5 | Tokens are refreshed at 75% of their lifetime; failures retry with backoff (1 s doubling to 30 s) while the old token is still valid, and keep retrying after. | Azure tokens live 60 to 90 minutes, so refresh comes 15 or more minutes early. |
| D6 | PostgreSQL connections in Entra mode always use `sslmode=verify-full` against the system roots (Azure uses public CAs), optionally a CA bundle file. Redis in Entra mode requires `rediss://`, system roots or a CA bundle file. Credentials in the URL are refused in Entra mode. | AC7; no way to configure plaintext with a token. |
| D7 | Per datastore, in Entra mode only: a refused token (SQLSTATE 28P01/28000, Redis `WRONGPASS`/`NOPERM`/`NOAUTH`), a rejected server certificate, a configuration error or a migration refused for lack of privilege stops start-up with a secret-free message; a token that cannot be obtained stops it too. Unreachable datastores and interrupted handshakes are retried as today, and a password-mode datastore keeps today's behaviour even when the other uses Entra. The gateway proves one authenticated Redis connection before serving in Entra mode. | AC6: misconfiguration is permanent; networks recover. |
| D8 | Each of auth-svc, message-svc and the worker gains a `migrate` subcommand (migrations, then grants, then exit) and honours `PUSH_RUN_MIGRATIONS` (default `true`). The `migrate` step can grant a runtime role least privilege on the service's schema. | Separate migration identity without DDL rights at runtime; the default keeps today's self-migrating services. |
| D9 | PostgreSQL role names in Entra mode are Entra principal names (managed identities like `push-auth-id`, groups, UPNs), so they get their own validator: 1 to 63 bytes of ASCII letters, digits, `_`, `-`, `.`, `@`, quoted in SQL. | The existing `is_role_name` rejects `-`. |
| D10 | The chart (Plan 12B) projects the workload-identity token either through the AKS webhook (label + annotation) or itself (for clusters without the webhook and for the local rehearsal). | Rehearsal without Azure. |

## 3. Configuration

New variables. Everything else keeps its meaning.

### Every service with PostgreSQL (auth-svc, message-svc, worker)

| Variable | Meaning | Default |
|---|---|---|
| `PUSH_POSTGRES_AUTH` | `password` or `azure-entra` | `password` |
| `PUSH_DATABASE_URL` | password mode only, as today; refused in Entra mode | required in password mode |
| `PUSH_DATABASE_HOST` | Entra mode: server host name (the certificate's name) | required in Entra mode |
| `PUSH_DATABASE_PORT` | Entra mode | `5432` |
| `PUSH_DATABASE_NAME` | Entra mode: the service's database | `push_auth`, `push_messaging` or `push_worker` |
| `PUSH_DATABASE_USER` | Entra mode: the principal's PostgreSQL role name | required in Entra mode |
| `PUSH_DATABASE_CA_FILE` | Entra mode: PEM bundle to trust instead of the system roots | system roots |
| `PUSH_RUN_MIGRATIONS` | `true` or `false`: whether the service migrates (and grants) at start-up | `true` |

The worker's second database (`push_auth`) has `PUSH_AUTH_DATABASE_HOST`, `_PORT`,
`_NAME`, `_USER`, `_CA_FILE`, each defaulting to the worker's own `PUSH_DATABASE_*`
value except `_NAME` (default `push_auth`); in password mode `PUSH_AUTH_DATABASE_URL`
as today. One `PUSH_POSTGRES_AUTH` covers both.

The `migrate` subcommand reads only the database variables above, plus
`PUSH_DATABASE_RUNTIME_ROLE` (the runtime role to grant on the service's schema;
optional) and, for auth-svc, `PUSH_AUTH_WORKER_ROLE` as today.

### Every service with Redis (auth-svc, message-svc, gateway)

| Variable | Meaning | Default |
|---|---|---|
| `PUSH_REDIS_AUTH` | `password` or `azure-entra` | `password` |
| `PUSH_REDIS_URL` | password mode as today; Entra mode `rediss://host:port` without credentials | required |
| `PUSH_REDIS_USER` | Entra mode: the ACL user name (Azure Managed Redis: the principal's object id) | the token's `oid` claim |
| `PUSH_REDIS_CA_FILE` | Entra mode: PEM bundle to trust instead of the system roots | system roots |

### Identity (read only when a datastore uses `azure-entra`)

The AKS workload-identity webhook sets these; the chart can set them itself (D10).

| Variable | Meaning |
|---|---|
| `AZURE_CLIENT_ID` | the identity's client id (required for workload identity; selects a user-assigned managed identity) |
| `AZURE_TENANT_ID` | the tenant (required for workload identity) |
| `AZURE_FEDERATED_TOKEN_FILE` | the projected service-account token; when set, workload identity is used, otherwise managed identity (IMDS) |
| `AZURE_AUTHORITY_HOST` | `https://login.microsoftonline.com/` by default; must be `https://`, except a loopback host (tests) |

Scopes: PostgreSQL `https://ossrdbms-aad.database.windows.net/.default`, Redis
`https://redis.azure.com/.default`.

## 4. Token acquisition

Workload identity, re-reading the file on every request (the kubelet rotates it):

```text
POST {AZURE_AUTHORITY_HOST}/{AZURE_TENANT_ID}/oauth2/v2.0/token
Content-Type: application/x-www-form-urlencoded

client_id={AZURE_CLIENT_ID}&scope={scope}&grant_type=client_credentials
&client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer
&client_assertion={contents of AZURE_FEDERATED_TOKEN_FILE}
```

Managed identity:

```text
GET http://169.254.169.254/metadata/identity/oauth2/token?api-version=2018-02-01
    &resource={scope without /.default}[&client_id={AZURE_CLIENT_ID}]
Metadata: true
```

Both answer `access_token` and `expires_in` (a number, or a string from IMDS). Errors
report the HTTP status and the OAuth `error` code only: never the token, the assertion
or `error_description` (which can echo request data). Requests time out after 10 s.

## 5. PostgreSQL

`push-store` gains `DatabaseTarget` (password URL or Entra host/port/name/user/CA) and a
`Connector` that turns a target into sqlx connect options. In Entra mode the connector
holds the token stream: the request pool is built with the current token as password,
`max_lifetime` 30 minutes, and a task swaps the pool's connect options
(`Pool::set_connect_options`) whenever the token changes; existing connections stay
(PostgreSQL checks credentials only at connect). Migrations, grants and the worker's
outbox pool use the same connector. The existing URL functions stay as thin wrappers,
so tests and password mode are unchanged.

Roles (documented in 12B): an Entra administrator creates one PostgreSQL role per
principal (`pgaadauth_create_principal`) and the three databases. Either each service's
identity owns its database and migrates at start-up (simplest), or a migration identity
owns them, runs `migrate` as a Job, and grants each runtime identity `USAGE` on the
schema and `SELECT, INSERT, UPDATE, DELETE` on its tables, sequences and functions
(`PUSH_DATABASE_RUNTIME_ROLE`), plus the worker's existing narrow grants on `auth`.
Integration tests prove each service works as such a non-owner runtime role.

Before migrating auth-svc's and the worker's databases the connector checks
`current_setting('azure.extensions', true)`: when set and missing `CITEXT`, it stops
with "the citext extension is not allow-listed: add CITEXT to the server parameter
azure.extensions".

## 6. Redis

`push-credentials` gains `RedisTarget` and `RedisConnector`. In password mode it wraps
today's `redis::Client`. In Entra mode the client is built with TLS
(`Client::build_with_tls` when a CA file is given) and the credentials provider yields
`(user, token)` pairs; every `ConnectionManager` (auth-svc limits, gateway limits and
publisher, message-svc notifier) gets the provider. `RedisConnector::pubsub()` opens a
subscriber with the current credentials, and `rotations()` tells the hub when to
rotate (D4). In password mode `rotations()` never fires.

Only non-clustered Redis (Azure Managed Redis with the Enterprise or non-clustered
policy) is supported: the hub uses classic `SUBSCRIBE`, and the OSS cluster policy would
need sharded pub/sub and a cluster client.

## 7. Verification

Local proof without Azure (Plan 12A), with a stub token issuer whose tokens live a few
seconds and become the PostgreSQL role's password and the Redis ACL user's password on
issue, and TLS-enabled PostgreSQL and Redis containers using a test CA:

| AC | Local evidence |
|---|---|
| AC1 | Unchanged default tests, chart checks and renders |
| AC2 | Config, chart and log tests: no datastore password in Entra mode; tokens redacted |
| AC3 | Whole-stack test: all four services in Entra mode serve a chat |
| AC4 | Same test runs ten token lifetimes with steady traffic: no failed request, no closed socket, no lost message; a pool connection opened after several rotations succeeds. The 3-hour run on Azure is the consumer's acceptance |
| AC5 | `CLIENT KILL USER` in the test: the hub reconnects with a fresh token and resubscribes; messages keep arriving |
| AC6 | Tests for a failing issuer and a principal without a role: start-up stops with a clear, secret-free error |
| AC7 | Tests: plaintext servers and an untrusted CA are refused |
| AC8 | Unit tests for scheduling and rotation; integration tests above |

Plan 12B adds a minikube rehearsal of the chart in Entra mode (TLS datastores and the
stub issuer in the cluster, the chart's own token projection) and Calico enforcement of
the egress policies.

## 8. Delivery

| Plan | Delivers |
|---|---|
| 12A | `push-credentials`, Entra PostgreSQL and Redis in every service, the `migrate` subcommand and `PUSH_RUN_MIGRATIONS`, local proof of AC1 to AC8 |
| 12B | Chart: external datastores, service accounts, token projection, migration Job, egress NetworkPolicies, CA bundle; deployment guide; minikube rehearsal |

Out of scope: end-user Entra sign-in (the separate request, Plan 13), application keys
(signing, link and internal tokens stay in the Secret), OSS-cluster Redis, private
endpoint and DNS setup, and running on Azure.
