# Consumer deployment configuration

push-messenger is a reusable core: consumers include it (for example as a Git
submodule) and build their own scaffolding around it. This guide is the contract of the
Helm chart `deploy/helm/push-messenger` for such consumers.

## Ownership

The chart deploys the four services, bundled PostgreSQL 17 and Redis, and their
release-scoped NetworkPolicies into one namespace. Consumers own:

- Infrastructure, clusters, cluster metadata and NetworkPolicy enforcement (CNI).
- Ingress/Gateway controllers, domains, DNS and TLS certificates.
- Secret delivery (for example Azure Key Vault with its own sync/CSI tooling).
- Images: building (`deploy/docker/Dockerfile`), registries, publication and digests.
- CI/CD, version selection, deployment, backups and capacity acceptance.

The chart installs no controller, CRD, Gateway API resource, certificate issuer,
ExternalSecret/CSI object, registry or pipeline, and nothing cluster-scoped. Managed
databases, multi-node HA and automatic or zero-downtime secret rotation are out of
scope (the chart can use managed PostgreSQL and Redis the consumer provisions, see
[Managed datastores with Microsoft Entra ID](#managed-datastores-with-microsoft-entra-id)).
Using the chart requires no GitHub Actions, GHCR or particular cloud.

Every container runs with `RuntimeDefault` seccomp, no privilege escalation and a
read-only root filesystem. The services and Redis run as non-root with no capabilities.
The bundled PostgreSQL's image entrypoint starts as root only to hand PGDATA and its
socket directory to the `postgres` user, so it keeps just `CHOWN`, `DAC_OVERRIDE`,
`FOWNER`, `SETGID` and `SETUID`; the server itself runs as `postgres` with none.

## Credentials

### Chart-managed (default, local development)

With `secrets.existingSecret: ""` and `secrets.volume: {}` the chart generates every credential on install,
stores them in `<release>-secrets` (`helm.sh/resource-policy: keep`) and reads them back
with `lookup` on upgrades. A render therefore **contains credentials** and needs cluster
access to reuse them: never send its output to shared logs or artifacts.
`auth.signingKey` / `auth.linkKey`, when set by hand, are validated at render time.

### Consumer-managed Secret

```yaml
secrets:
  existingSecret: consumer-credentials
```

The chart references that Secret in the release namespace and does **not** create,
adopt, read, update or delete it. It never generates fallback credentials and renders
offline (no `lookup`). Inline credentials conflict and are refused by name:
`postgres.password`, `postgres.messagingPassword`, `postgres.authPassword`,
`postgres.workerPassword`, `redis.password`, `auth.signingKey`, `auth.linkKey`,
`worker.email.resendApiKey` and `worker.email.webhookSecret`.

Each workload receives only the keys it uses, as individual `secretKeyRef`s (never
`envFrom`, never `optional`). Key names are fixed:

| Key | Required by / condition | Decoded value contract |
|---|---|---|
| `postgres-password` | bundled Postgres | bootstrap administrator `push` password |
| `messaging-db-password` | Postgres empty-volume initialization | `push_messaging` role password |
| `database-url` | message-svc | `postgres://push_messaging:<password>@<release>-postgres:5432/push_messaging` |
| `auth-db-password` | Postgres empty-volume initialization | `push_auth` role password |
| `auth-database-url` | auth-svc | `postgres://push_auth:<password>@<release>-postgres:5432/push_auth` |
| `worker-db-password` | Postgres empty-volume initialization | `push_worker` role password |
| `worker-database-url` | worker | `postgres://push_worker:<password>@<release>-postgres:5432/push_worker` |
| `worker-auth-database-url` | worker | `postgres://push_worker:<same-password>@<release>-postgres:5432/push_auth` |
| `redis-password` | Redis | Redis AUTH password |
| `redis-url` | message-svc, auth-svc, gateway | `redis://:<same-password>@<release>-redis:6379` |
| `auth-signing-key` | auth-svc; DEV ONLY smoke test | standard base64 of exactly 32 bytes (Ed25519 seed) |
| `auth-link-key` | auth-svc | standard base64 of at least 32 bytes |
| `accounts-token` | worker, message-svc | 32-256 visible ASCII characters |
| `directory-token` | auth-svc, message-svc | 32-256 visible ASCII characters, distinct from both other API tokens |
| `messaging-token` | gateway, message-svc | 32-256 visible ASCII characters, distinct from both other API tokens |
| `resend-api-key` | worker when `worker.email.sender: resend` | Resend API key |
| `resend-webhook-secret` | worker when `worker.email.webhook.enabled: true` | Svix signing secret `whsec_...` |
| `auth-admin-token` | auth-svc when `auth.admin.enabled: true` | the admin API's bearer token, 32 to 256 visible ASCII characters |

Kubernetes `data` values carry an outer base64 encoding; the decoded auth keys are
themselves standard base64. Do not double-encode values given as `stringData`.

The bundled database bootstrap requires **ASCII letters and digits only** for the
database and Redis passwords; each password must agree with the URL embedding it.
URL hosts depend on the actual release name (and namespace, if you qualify them).
The role passwords are applied only when PostgreSQL initialises an empty volume:
changing them later requires coordinated role changes (see
[the upgrade runbook](deployment-upgrade.md)). The chart cannot validate credentials
against the backing services; external datastores are configured separately (below).
An external Redis must be version 6 or later: the services' command connections speak
RESP3 (`HELLO 3`).

Materialize the Secret before the workloads start. Helm cannot inspect it offline:
a missing required key stops that workload with a Kubernetes
`CreateContainerConfigError`, and invalid formats or credentials fail application
start-up or readiness. Neither falls back to generated credentials. Changing the
Secret's content does not roll workloads or rotate database roles; coordinate
backing-service changes and rollouts yourself.

### Consumer-mounted application keys (CSI, without Secret sync)

Set `secrets.volume` to a consumer-supplied Kubernetes **VolumeSource**, without a
`name`. The chart supplies its name, `application-keys`, and read-only **individual
file** mounts using `subPath` on auth, message-svc, gateway and worker. Each container
can read only the application-key files it consumes, not the volume root or other
workloads' keys. It neither creates a CSI driver nor creates a `SecretProviderClass`.
Example values:

```yaml
secrets:
  existingSecret: ""
  volume:
    csi:
      driver: secrets-store.csi.k8s.io
      readOnly: true
      volumeAttributes:
        secretProviderClass: push-application-keys
  mountPath: /etc/push/application-keys
  files:
    auth-signing-key: auth-signing-key
    auth-link-key: auth-link-key
    accounts-token: accounts-token
    directory-token: directory-token
    messaging-token: messaging-token
    auth-admin-token: auth-admin-token
    resend-api-key: resend-api-key
    resend-webhook-secret: resend-webhook-secret
```

`mountPath` defaults as shown and must be an absolute, non-root path with no traversal
or trailing slash. `files` defaults to the existing Secret key names. Override a value
to change its filename; filenames cannot contain directory separators, be `.`, or
start with `..` (reserved Kubernetes projection directories, including `..data`).
Nonempty filenames must be distinct across all keys, including disabled optional
keys, so aliases cannot expose another workload's key or duplicate a mount path.
File mount paths must not overlap chart-reserved volume trees: `/etc/push/entra-ca`,
`/etc/push/authority-ca`, `/etc/push/postgres-ca`, `/etc/push/redis-ca`,
`/var/run/secrets`, `/data` and `/operator-accounts`.
Required files cannot have empty filenames. Admin and email filenames are
required only when those features are enabled; absent admin configuration remains
disabled and the chart neither references nor mounts its file. Inline application
key values and `secrets.existingSecret` conflict with mounted mode and are refused.

| Existing key / default filename | File environment variable | Consumers |
|---|---|---|
| `auth-signing-key` | `PUSH_AUTH_SIGNING_KEY_FILE` | auth |
| `auth-link-key` | `PUSH_AUTH_LINK_KEY_FILE` | auth |
| `accounts-token` | `PUSH_ACCOUNTS_TOKEN_FILE` | worker, message-svc |
| `directory-token` | `PUSH_DIRECTORY_TOKEN_FILE` | auth, message-svc |
| `messaging-token` | `PUSH_MESSAGING_TOKEN_FILE` | gateway, message-svc |
| `auth-admin-token` | `PUSH_AUTH_ADMIN_TOKEN_FILE` | auth, when admin enabled |
| `resend-api-key` | `PUSH_RESEND_API_KEY_FILE` | worker, when Resend selected |
| `resend-webhook-secret` | `PUSH_RESEND_WEBHOOK_SECRET_FILE` | worker, when webhook enabled |

The DEV password smoke hook also uses `auth-signing-key`, via
`PUSH_SMOKE_SIGNING_KEY_FILE`. Operator Entra smoke receives **no application keys**.
The migration Job and its init containers receive neither the volume nor key
variables; its distinct `-migrate-egress` hook policy is unchanged.

Contents are the **decoded values** in the earlier table: notably, signing and link
key files contain standard base64 text, not decoded binary seeds and not an extra
Kubernetes `data` encoding. The shared `push-credentials` reader reads files **once at
startup**, removes terminal CR/LF characters only, and otherwise preserves contents
(including spaces and embedded newlines, which the existing format validators may
reject). Direct environment values are not trimmed and continue working outside the
chart. Setting both a direct variable and its `_FILE` alternative, even to empty
strings, fails startup. Missing, unreadable, empty and non-UTF-8 files fail startup;
there is no fallback, secret content or path in configuration errors.

Only the files in the consumer table are bound into each workload, at
`<mountPath>/<filename>` matching its `_FILE` variables; the signing seed and link key
are not mounted into gateway, message-svc or worker. The DEV password smoke hook
mounts only the signing key. The consumer still owns the source volume's contents,
provider aliases and access policy; configure regular key files, not directory
aliases, and do not point different filenames at the same backing credential or
include unrelated credentials. Ensure the CSI driver makes its files readable by
the non-root service user. With bundled datastores their existing
chart-managed Secret still holds **datastore keys only**. With external Entra datastores
no credential Secret is rendered at all. Datastore password URLs, CA bundles and Azure
identity token files are separate mechanisms, not covered by this setting.
External password-mode datastores still require `secrets.existingSecret`; this
mutually exclusive mounted mode supports bundled datastores or external Entra
datastores, not a hybrid external password Secret plus mounted application keys.

For example, the consumer may create this Azure provider resource in the release
namespace (placeholders are consumer-owned; **no `secretObjects` Secret sync**):

```yaml
apiVersion: secrets-store.csi.x-k8s.io/v1
kind: SecretProviderClass
metadata:
  name: push-application-keys
spec:
  provider: azure
  parameters:
    usePodIdentity: "false"
    clientID: "<key-vault-workload-identity-client-id>"
    tenantId: "<tenant-id>"
    keyvaultName: "<vault-name>"
    objects: |
      array:
        - |
          objectName: auth-signing-key
          objectType: secret
          objectAlias: auth-signing-key
        - |
          objectName: auth-link-key
          objectType: secret
          objectAlias: auth-link-key
        - |
          objectName: accounts-token
          objectType: secret
          objectAlias: accounts-token
        - |
          objectName: directory-token
          objectType: secret
          objectAlias: directory-token
        - |
          objectName: messaging-token
          objectType: secret
          objectAlias: messaging-token
```

Add objects for admin and Resend only when needed, matching the filenames above.
The consumer installs/configures Secrets Store CSI and its Azure provider, grants
vault access, and configures workload identity federation/token projection for each
ServiceAccount mounting this class (including a DEV smoke hook if used). This is
independent of the datastores' identities. The chart creates no dummy workload to
trigger synchronization and requires no Kubernetes Secret for application keys.

For rotation, update the provider's backing values and coordinate a rollout of the
affected Deployments, verifying that replacement pods mount the new values.
**CSI driver rotation updates are not propagated to `subPath` binds in existing
containers**; checking an existing container's bound file does not verify delivery
of the rotated value. Running processes also read files only at startup, not on
changes. A coordinated rollout is required even when CSI auto-rotation is enabled.
Coordinate internal-token readers and callers; mixed generations can reject calls.
Signing/link key replacement can invalidate existing tokens or links. The chart promises neither
overlapping key generations nor zero-downtime rotation.

### Managed datastores with Microsoft Entra ID

With `postgres.enabled: false` / `redis.enabled: false` and `auth: azure-entra` the
services reach external Azure Database for PostgreSQL and Azure Managed Redis with
Microsoft Entra ID tokens from workload identity, one ServiceAccount per workload, an
optional migration Job and opt-in egress NetworkPolicies: see
[deployment-azure-entra.md](deployment-azure-entra.md). The Secret's database and Redis
keys (`postgres-password`, `*-db-password`, `*database-url`, `redis-password`,
`redis-url`) are then unused and not rendered; application keys use the selected
Secret or mounted-volume mode above.

### Microsoft Entra ID sign-in

`auth.signIn` turns on Entra ID sign-in beside or instead of email and password
([guide](entra-sign-in.md#with-the-helm-chart)). Entra-only (`providers: entra`) needs no
`resend-api-key` or `resend-webhook-secret` in a consumer-managed Secret and no email
provider at all (the worker runs with `PUSH_EMAIL_SENDER=none` and no link-page Ingress is
rendered); every other key is unchanged. With egress policies auth-svc then needs HTTPS to
`login.microsoftonline.com`: `networkPolicy.egress.entraSignInCidrs` (auth-svc only) or a
proxy (`auth.signIn.entra.httpsProxy`).

`auth.admin.enabled` serves auth-svc's admin API on port 8082 (sign a user out of every
device; [guide](entra-sign-in.md#admin-api)). It is never routed by the chart's Ingresses;
`networkPolicy.adminPeers` lists who may reach it (none by default: use
`kubectl port-forward`).

## Email and the Resend webhook

`worker.email.sender` is `log` (default), `resend`, or `none` (no email at all; only for
Entra-only installations, which render it anyway).

`worker.email.webhook.enabled` (default `false`) is an explicit decision, never
inferred from a Secret. When `true` the worker references `resend-webhook-secret`,
serves `/v1/webhooks/resend`, the chart Ingress routes that single Exact path, and the
worker's NetworkPolicy admits the configured controller peers (also when you supply
your own routes). `/dev/mailbox` is never routed and is DEV ONLY.

## Public routing

```yaml
ingress:
  enabled: true
  className: ""               # empty omits ingressClassName (the default class)
  host: messenger.example.com # public identity: email links and token audience
  scheme: https
  tls:
    secretName: messenger-tls # existing kubernetes.io/tls Secret; required for https
  annotations: {}             # on every public Ingress
  gatewayAnnotations: {}      # /v1/socket
  authAnnotations: {}         # /v1/auth, /v1/devices, /v1/users
  authLinkAnnotations: {}     # /v1/auth/verify, /v1/auth/password/reset (token-bearing)
  webhookAnnotations: {}      # /v1/webhooks/resend
```

All annotation values are strings. A route-specific key overrides the common key;
link annotations never leak onto the ordinary auth routes. With `https` every rendered
public Ingress carries the TLS Secret; the chart never creates a certificate.
Arbitrary controller snippets are not required.

`ingress.enabled: false` removes all chart Ingress resources while `host`/`scheme` keep
naming email links and the token audience, so consumers can supply their own routes,
including Gateway API routes, for the same `/v1/...` paths. Maintenance gates for such
routes are the consumer's: disabling chart Ingress is not a global traffic gate.

The retired ingress-nginx remains only as the local minikube compatibility path
(`values-minikube.yaml`); it is not a production recommendation. A maintained
controller (Traefik) is verified in
[deployment-ingress-verification.md](deployment-ingress-verification.md).

Consumer prerequisites the chart cannot provide: controller access/error/trace logs
that never record the link routes' `?token=` query; WebSocket upgrade with idle
timeouts above 75 s (clients ping every 30 s, the gateway closes after 2.5
heartbeats); and proxy trust that sanitizes client-supplied forwarding headers.

### Client IPs for rate limits

`auth.clientIpHeader` and `gateway.clientIpHeader` (default empty) name the header
the trusted controller writes; the services use its last line, then last
comma-separated entry. Empty uses the peer address, which avoids trusting client
headers but counts the proxy's address for every client behind a controller, so it is
not a production per-client setup. Only select a header after configuring the
controller's trusted hops so it overwrites or appends to it, and with direct backend
access blocked (the chart's NetworkPolicies admit only the configured controller
peers). For example `x-forwarded-for` with Traefik and no trusted upstream IPs. The
external load balancer's source-IP preservation is a deployment-specific acceptance
check.

## In-cluster transport

Public traffic is TLS at the controller. Traffic between the services inside the
cluster is not encrypted by the chart: gRPC to message-svc, HTTP from the gateway to
auth-svc's key set, PostgreSQL and Redis. The internal tokens and the NetworkPolicies
decide who may call what, but someone who can read pod traffic on the node network
could read messages and credentials. Production deployments should encrypt it with a
service mesh that adds mutual TLS (Istio, Linkerd) or a CNI that encrypts pod traffic
(Cilium or Calico with WireGuard). That choice belongs to the consumer.

When a mesh or a TLS-terminating Service fronts auth-svc, set `gateway.authUrl` to its
`https://` URL; the gateway trusts the image's system roots, or the PEM bundle that
`SSL_CERT_FILE` names (mount a private CA there). The URL must still reach this
release's auth-svc: its key set decides which access tokens the gateway accepts.

## Images

```yaml
images:
  gateway:
    repository: registry.example.com/messenger/gateway
    tag: consumer-release   # used when digest is empty
    digest: ""              # sha256:<64 lowercase hex>; takes precedence over tag
```

Each of `auth`, `messageSvc`, `gateway`, `worker` and `smoke` accepts a digest,
rendered as `repository@digest`. Use the digest your own publication returned for
**each** image; they differ. For example (synthetic, non-deployable):
`digest: sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa`.
`postgres.image` and `redis.image` are complete references and accept
`repository@sha256:...` directly; by default they are `postgres:17-alpine` and
`redis:7-alpine` pinned to the digest of their multi-arch index, as are the
Dockerfile's base images. Upgrading from a chart that named them by tag alone restarts
the bundled PostgreSQL and Redis once (their pod spec changes). Valid syntax does not prove a digest exists or is
signed; registry authorization is supplied through your own workload mechanisms.
Build images with `docker build -f deploy/docker/Dockerfile --target <name> .` for
`auth`, `message-svc`, `gateway`, `worker` and `smoke`.

## A complete consumer values example

References only; no credentials:

```yaml
secrets:
  existingSecret: consumer-credentials
ingress:
  enabled: true
  className: traefik
  host: messenger.example.com
  scheme: https
  tls:
    secretName: messenger-tls
  annotations:
    traefik.ingress.kubernetes.io/router.entrypoints: websecure
auth:
  clientIpHeader: x-forwarded-for
gateway:
  clientIpHeader: x-forwarded-for
worker:
  email:
    sender: resend
    from: "Messenger <no-reply@example.com>"
    webhook:
      enabled: true
networkPolicy:
  ingressControllerPeers:
    - namespaceSelector:
        kubernetes.io/metadata.name: edge
      podSelector:
        app.kubernetes.io/name: traefik
images:
  auth:
    repository: registry.example.com/messenger/auth
    tag: consumer-release
  messageSvc:
    repository: registry.example.com/messenger/message-svc
    tag: consumer-release
  gateway:
    repository: registry.example.com/messenger/gateway
    tag: consumer-release
  worker:
    repository: registry.example.com/messenger/worker
    tag: consumer-release
```

Check the controller's actual namespace and pod labels before using these
selectors; an empty `ingressControllerPeers` denies all public traffic.

## Offline rendering

With references only, lint and render without cluster access and without
credentials on the command line:

```powershell
$ConsumerValues = Read-Host 'Absolute consumer values file (references only, no inline credentials)'
if (-not [IO.Path]::IsPathFullyQualified($ConsumerValues) -or
    -not (Test-Path $ConsumerValues -PathType Leaf)) {
    throw 'Supply an existing absolute consumer values file'
}
helm lint .\deploy\helm\push-messenger --strict -f $ConsumerValues
if ($LASTEXITCODE -ne 0) { throw 'Consumer chart lint failed' }
helm template messenger .\deploy\helm\push-messenger --namespace messenger -f $ConsumerValues |
    Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Consumer offline render failed' }
```

This external mode emits no Secret. Package with `helm package` in your own automation.

## Moving an existing installation to a consumer-managed Secret

This is an ordered operator procedure, not a provisioning tool:

1. If the installation predates the dedicated messaging role, complete and rehearse
   the [data-preserving upgrade](deployment-upgrade.md) first. Record the current
   chart, images, values and credential source privately, with a verified backup.
2. Under consumer maintenance, securely copy the **unchanged** required values from
   the chart-managed `<release>-secrets` into a distinctly named consumer-owned Secret
   in the same namespace, and verify password/URL agreement privately. Do not generate
   new passwords or keys as a side effect. Never use `--set`, print values or log
   `helm get all`.
3. Set `secrets.existingSecret`, remove all nine inline credential settings, set
   `worker.email.webhook.enabled` explicitly (**an installation that served webhooks
   must set `true`: a retained key no longer implies the route**), choose the
   controller/TLS settings, and deploy the coordinated chart and images. The old kept
   chart Secret is retained, neither adopted by the new configuration nor deleted.
4. Verify readiness, existing data and normal application traffic before ending
   maintenance.
5. To roll back with unchanged credentials, restore the previous chart, values and
   images under maintenance; the retained chart-managed Secret still matches. If
   backing credentials changed, coordinate their restoration too: Helm rollback alone
   is insufficient. Keep both sources until rollback is explicitly retired.

Never change the external Secret's Helm labels or annotations, delete it on uninstall,
or commit a Secret manifest or credential fixture to a repository.
