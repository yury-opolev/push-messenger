# Azure PostgreSQL and Redis with Microsoft Entra ID

This guide configures the chart for external, managed datastores that accept only
Microsoft Entra ID tokens: Azure Database for PostgreSQL flexible server and Azure
Managed Redis, reached from AKS with workload identity. It complements the chart
contract in [deployment-configuration.md](deployment-configuration.md); the design is
[2026-10-18-azure-entra-datastores-design.md](superpowers/specs/2026-10-18-azure-entra-datastores-design.md).

## 1. What it does

With `postgres.enabled: false` and `redis.enabled: false` the chart deploys neither
bundled datastore. With `auth: azure-entra` (the default of both `external` blocks)
auth-svc, message-svc, the gateway and the worker sign in to PostgreSQL and Redis with
Entra ID access tokens that they obtain from their workload identity and renew before
expiry (at 75% of the lifetime). No datastore password exists anywhere: not in values,
not in a Secret, not in an environment variable. Each workload gets its own
ServiceAccount, so each can have its own identity.

The application keys come from a Secret (chart-managed or `secrets.existingSecret`)
by default: `auth-signing-key`, `auth-link-key`, the three internal API tokens,
`auth-admin-token` when enabled and, with Resend, `resend-api-key` and
`resend-webhook-secret`. Alternatively, `secrets.volume` mounts consumer-delivered
files (for example Secrets Store CSI with a `SecretProviderClass`, **without Secret
sync**): see [mounted application keys](deployment-configuration.md#consumer-mounted-application-keys-csi-without-secret-sync)
for the complete values, filenames and Azure CSI example. With external Entra
datastores and mounted application keys the chart renders no credential Secret.
Each workload mounts only its consumed files through read-only `subPath` binds.
CSI rotation updates do not propagate to existing binds, and files load at startup
only: rotate by coordinating the backing values and rolling out their consumers.
Datastore tokens still refresh independently.

The chart provisions nothing outside the release namespace: the Azure resources,
identities, federated credentials, role assignments, private endpoints and DNS below are
the consumer's.

## 2. Azure prerequisites (consumer-owned)

| Item | Requirement |
|---|---|
| PostgreSQL | Azure Database for PostgreSQL flexible server, **Microsoft Entra authentication only**, with an Entra administrator. |
| `citext` | The server parameter `azure.extensions` contains `CITEXT`: the auth-svc and worker migrations create it. Without it `migrate` stops with "the citext extension is not allow-listed: add CITEXT to the server parameter azure.extensions". |
| Redis | Azure Managed Redis with access keys disabled and Microsoft Entra authentication on, TLS on port 10000, the **Enterprise or non-clustered** cluster policy. The gateway's hub uses classic `SUBSCRIBE`; the OSS cluster policy is not supported. |
| Network | Private endpoints and private DNS zones for both datastores, resolvable from the cluster. |
| AKS | The OIDC issuer and workload identity enabled (`--enable-oidc-issuer --enable-workload-identity`). |
| Identities | One user-assigned managed identity per workload: auth-svc, message-svc, gateway, worker, and one for migrations (`migrations.job.enabled`). One identity may serve several workloads; distinct ones keep the blast radius small. |
| Federated credentials | On each identity: issuer the cluster's OIDC issuer URL, subject `system:serviceaccount:<namespace>:<release>-<component>` (component `auth`, `message-svc`, `gateway`, `worker`, `migrate`), audience `api://AzureADTokenExchange`. |
| Redis access | A data access policy assignment (e.g. "Data Owner" or a narrower custom policy) for the identities of auth-svc, message-svc and the gateway, each assigned by its **object id**. The worker does not use Redis. |
| PostgreSQL roles | One role per identity and the three databases (section 3). The gateway does not use PostgreSQL. |

## 3. Database roles

Run once as the Entra administrator, connected to the `postgres` database. The role
names are the identities' names (`postgres.external.users.*`); use your own.

```sql
SELECT * FROM pgaadauth_create_principal('push-migrator-id', false, false);
SELECT * FROM pgaadauth_create_principal('push-auth-id', false, false);
SELECT * FROM pgaadauth_create_principal('push-message-svc-id', false, false);
SELECT * FROM pgaadauth_create_principal('push-worker-id', false, false);
-- PostgreSQL 16+ requires membership before assigning another role as database owner.
GRANT "push-migrator-id" TO CURRENT_USER;
CREATE DATABASE push_auth OWNER "push-migrator-id";
CREATE DATABASE push_messaging OWNER "push-migrator-id";
CREATE DATABASE push_worker OWNER "push-migrator-id";
```

Then, still as the **Entra administrator**, connect to **each** of `push_auth`,
`push_messaging` and `push_worker` and run:

```sql
GRANT USAGE, CREATE ON SCHEMA public TO "push-migrator-id";
```

These are per-database schema privileges, not a grant on the `postgres` database.
They let the migration identity create the SQLx migration table in `public`.

Two models:

- **Migration Job (recommended, `migrations.job.enabled: true`).** The migration
  identity owns the three databases. Before every install and upgrade a Helm hook Job
  runs `push-message-svc migrate`, `push-auth migrate` and `push-worker migrate` as that
  identity: each applies its migrations and grants the service's runtime role `USAGE`
  on its schema and `SELECT, INSERT, UPDATE, DELETE` on its tables, sequences and
  functions (`PUSH_DATABASE_RUNTIME_ROLE`); auth-svc's step also grants the worker its
  narrow access to the `auth` schema (`PUSH_AUTH_WORKER_ROLE`: the outbox and two
  cleanup functions). The services run with `PUSH_RUN_MIGRATIONS=false` and hold no DDL
  rights.
- **Self-migrating services.** Without the Job, make each service's role the owner of
  its own database instead (`push_auth` to auth-svc's, `push_messaging` to
  message-svc's, `push_worker` to the worker's); the services migrate at start-up as
  today, and auth-svc grants the worker's role its access to `auth`.

In both models the worker connects to `push_auth` as its own role. If you revoke
`CONNECT` from `PUBLIC` on the databases, grant it back explicitly: each runtime role on
its own database, the worker's role also on `push_auth`, and the migration role on all
three.

## 4. Values

```yaml
postgres:
  enabled: false
  external:
    host: push-pg.postgres.database.azure.com
    port: 5432
    auth: azure-entra
    databases:
      messaging: push_messaging
      auth: push_auth
      worker: push_worker
    users:
      messageSvc: push-message-svc-id
      auth: push-auth-id
      worker: push-worker-id
      migrate: push-migrator-id
redis:
  enabled: false
  external:
    url: rediss://push-redis.westeurope.redis.azure.net:10000
    auth: azure-entra
azure:
  workloadIdentity:
    injection: webhook
    clientIds:
      auth: 6a1c2f0e-1111-4c4c-9a9a-000000000001
      messageSvc: 6a1c2f0e-1111-4c4c-9a9a-000000000002
      gateway: 6a1c2f0e-1111-4c4c-9a9a-000000000003
      worker: 6a1c2f0e-1111-4c4c-9a9a-000000000004
      migrate: 6a1c2f0e-1111-4c4c-9a9a-000000000005
migrations:
  job:
    enabled: true
networkPolicy:
  egress:
    enabled: true
    postgresCidrs: [10.20.1.4/32]
    redisCidrs: [10.20.1.5/32]
    # Entra ID (service tag AzureActiveDirectory) and Resend; or 0.0.0.0/0 with FQDN
    # egress enforced by the cluster's firewall (section 5).
    httpsCidrs: [20.190.128.0/18, 40.126.0.0/18]
secrets:
  existingSecret: push-app-keys
```

| Value | Meaning |
|---|---|
| `postgres.enabled`, `redis.enabled` | `false`: the external datastore; the bundled one, its policy and its Secret keys are not rendered |
| `postgres.external.auth`, `redis.external.auth` | `azure-entra`, or `password` (the URLs then come from `secrets.existingSecret`, keys as in the contract) |
| `postgres.external.users.*` | each identity's PostgreSQL role (an Entra principal name: 1 to 63 ASCII letters, digits, `_ - . @`) |
| `redis.external.url` | `rediss://host:port`, no credentials; `redis.external.user` overrides the ACL user (default: the token's `oid`, as Azure Managed Redis expects) |
| `postgres.external.caConfigMap`, `redis.external.caConfigMap` | a ConfigMap whose `ca.crt` is trusted instead of the system roots; empty for Azure (public CAs) |
| `azure.workloadIdentity.injection` | `webhook`: the AKS webhook projects the token and sets `AZURE_*` (the pods get the label `azure.workload.identity/use: "true"`, the ServiceAccounts the annotation `azure.workload.identity/client-id`); `chart`: the chart projects the token itself (audience `api://AzureADTokenExchange`) and sets `AZURE_*`, for clusters without the webhook; then `tenantId` is required and `authorityHost` and `authorityCaConfigMap` apply |
| `serviceAccounts.annotations` | extra annotations on every ServiceAccount the chart creates |
| `migrations.job.enabled`, `migrations.job.backoffLimit` | the hook Job of section 3 |

The chart checks these at render time: an external datastore without its address, a
missing role or client id, a Redis URL with credentials or without TLS, an unknown mode
and the migration Job without an Entra PostgreSQL are refused with a named error.

`authorityCaConfigMap` sets `SSL_CERT_FILE`, which replaces the system roots for the
token requests (and for other native-root clients in the same process): give it a full
bundle when the authority sits behind a TLS-inspecting proxy.

## 5. Egress

`networkPolicy.egress.enabled: true` adds Egress rules to every application policy:
DNS to `networkPolicy.egress.dns` (kube-system's `k8s-app: kube-dns` by default), the
release's own peers (gateway to auth-svc and message-svc, auth-svc and the worker to
message-svc), the datastores, HTTPS (443) to `httpsCidrs` and any raw rules in `extra`.
The bundled datastores, when enabled, are reached by their pod labels and get
`egress: []` themselves. With the migration Job the chart adds a policy for the Job's
pod (DNS, PostgreSQL, HTTPS). That migrate-only policy is a `pre-install,pre-upgrade`
hook with a lower weight than the Job, so it exists even on the first install under
a namespace default-deny policy. It persists after the Job finishes and is replaced
before the next hook run (`before-hook-creation`, not `hook-succeeded`); the regular
application policies remain normal release resources. As a Helm hook resource, the
migration policy may need explicit removal when permanently removing the release.
The hook is named `<release>-migrate-egress`, distinct from the legacy ordinary
`<release>-migrate` policy, so Helm's upgrade cleanup can remove the old resource
without deleting the retained hook. Its migration-pod selector is unchanged.

NetworkPolicies match IP blocks, not names:

- List the private endpoint addresses of PostgreSQL and Redis in `postgresCidrs` and
  `redisCidrs`; the port is `postgres.external.port` and the port of
  `redis.external.url`.
- For `login.microsoftonline.com` either list Microsoft's published Entra ID ranges
  (service tag `AzureActiveDirectory`) in `httpsCidrs` and keep them current, or enforce
  FQDN egress in the cluster's firewall (Azure Firewall, a CNI with FQDN policies) and
  allow `0.0.0.0/0` on 443 here.
- The worker's Resend API needs HTTPS too (`worker.email.sender: resend`).
- DNS must stay allowed, or nothing resolves.

As with ingress isolation, the rules need a NetworkPolicy-enforcing CNI; a render proves
nothing about enforcement.

## 6. Variables reference

The chart renders exactly these variables of the services (spec §3).

| Variable | Workloads | Value |
|---|---|---|
| `PUSH_POSTGRES_AUTH` | auth-svc, message-svc, worker, migration Job | `azure-entra` |
| `PUSH_DATABASE_HOST`, `PUSH_DATABASE_PORT` | the same | `postgres.external.host`, `.port` |
| `PUSH_DATABASE_NAME` | the same | the workload's database (`postgres.external.databases.*`) |
| `PUSH_DATABASE_USER` | the same | the workload's role; the migration Job's `users.migrate` |
| `PUSH_DATABASE_CA_FILE` | the same, with `postgres.external.caConfigMap` | `/etc/push/postgres-ca/ca.crt` |
| `PUSH_RUN_MIGRATIONS` | auth-svc, message-svc, worker | `false` with the migration Job, else `true` |
| `PUSH_AUTH_DATABASE_NAME` | worker | `postgres.external.databases.auth` (host, port, user and CA default to the worker's own) |
| `PUSH_AUTH_WORKER_ROLE` | auth-svc, migration Job (auth step) | `postgres.external.users.worker` |
| `PUSH_DATABASE_RUNTIME_ROLE` | migration Job | the step's service role |
| `PUSH_REDIS_AUTH`, `PUSH_REDIS_URL` | auth-svc, message-svc, gateway | `azure-entra`, `redis.external.url` |
| `PUSH_REDIS_USER` | the same, when set | `redis.external.user` |
| `PUSH_REDIS_CA_FILE` | the same, with `redis.external.caConfigMap` | `/etc/push/redis-ca/ca.crt` |
| `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_FEDERATED_TOKEN_FILE`, `AZURE_AUTHORITY_HOST` | every workload reaching an Entra datastore | set by the webhook, or by the chart with `injection: chart` (token file `/var/run/secrets/azure/tokens/azure-identity-token`) |
| `SSL_CERT_FILE` | the same, with `injection: chart` and `authorityCaConfigMap` | `/etc/push/authority-ca/ca.crt` |

Token scopes: PostgreSQL `https://ossrdbms-aad.database.windows.net/.default`, Redis
`https://redis.azure.com/.default`.

## 7. Verification on Azure (the consumer's acceptance)

1. Install into a non-production namespace. `helm install` waits for the migration Job
   (its log shows each step) before the services start.
2. Check that no datastore credential exists: the Secret's keys are only the
   application keys of section 1 (no `*database*`, `redis*` or `*password*` key), and no
   container has a `PUSH_DATABASE_URL`, `PUSH_AUTH_DATABASE_URL` or a `PUSH_REDIS_URL`
   with credentials.
3. Run the [operator Entra smoke test](consumer-verification.md#operator-entra-smoke-test)
   with two dedicated preexisting identities, or your own probes. Run the DEV ONLY
   smoke test only in an isolated namespace that may use dev mode (never in production).
4. Keep traffic running for at least 3 hours, several token lifetimes, with
   `push-load` ([consumer verification](consumer-verification.md)): no failed requests,
   no dropped WebSocket sessions and no lost messages.
5. Restart one pod after 70 minutes and see it connect with a fresh token; restart Redis
   (or fail it over) and see the gateways resubscribe.

Expected log lines: `refreshed an access token` (debug), `replaced the Redis subscriber
for a new token` (info). A refused token, an untrusted certificate or a missing role
stops start-up with a message naming the datastore and never the token.

The repository proves the chart in this mode without Azure on minikube:
`deploy/minikube/entra-rehearsal.ps1` (TLS datastores that accept only tokens from a
DEV ONLY in-cluster stub issuer, `injection: chart`, the migration Job, egress policies
on Calico, 60-second tokens under `push-load` traffic, killed Redis connections and
restarted pods), and `deploy/minikube/test-network-policy.ps1 -Egress` proves the egress
rules there with live allowed and denied connections.

## 8. Limitations

- Non-clustered Redis only (Azure Managed Redis with the Enterprise or non-clustered
  policy); the OSS cluster policy is not supported.
- No automatic linking of datastores to identities: the Azure-side setup of section 2 is
  the consumer's, as are private endpoints and DNS.
- Tokens are cached in memory only; a restarted pod requests new ones.
- A Redis connection the server closes (a failover, `CLIENT KILL`, an expired token) is
  replaced with the current token: the services' command connections speak RESP3, so they
  hear the close and reconnect at once, and a command that still meets the closed
  connection is sent once more on the new one (a rate-limit hit, a notification or kick
  `PUBLISH`; a hit counted twice only tightens a limit, a notification heard twice is a
  second doorbell). Only a command the server ran but whose answer was lost is counted
  twice. If reconnecting fails as well, the request fails as before: auth-svc answers a
  retryable 503, the gateway refuses that connection attempt's rate-limit check,
  message-svc misses that notification (clients catch up at their next sync). The
  gateway's subscriber stays on RESP2 and reconnects at once by itself. RESP3 needs
  Redis 6 or later, as Azure Managed Redis and the bundled Redis 7 are.
- The bundled datastores take passwords only; Entra ID applies to external ones.
- NetworkPolicies cannot match host names: Entra ID egress needs its IP ranges or a
  firewall with FQDN rules.
