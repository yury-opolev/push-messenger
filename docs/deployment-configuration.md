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
scope. Using the chart requires no GitHub Actions, GHCR or particular cloud.

## Credentials

### Chart-managed (default, local development)

With `secrets.existingSecret: ""` the chart generates every credential on install,
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

Kubernetes `data` values carry an outer base64 encoding; the decoded auth keys are
themselves standard base64. Do not double-encode values given as `stringData`.

The bundled database bootstrap requires **ASCII letters and digits only** for the
database and Redis passwords; each password must agree with the URL embedding it.
URL hosts depend on the actual release name (and namespace, if you qualify them).
The role passwords are applied only when PostgreSQL initialises an empty volume:
changing them later requires coordinated role changes (see
[the upgrade runbook](deployment-upgrade.md)). This does not add external-database
support, and the chart cannot validate credentials against the backing services.

Materialize the Secret before the workloads start. Helm cannot inspect it offline:
a missing required key stops that workload with a Kubernetes
`CreateContainerConfigError`, and invalid formats or credentials fail application
start-up or readiness. Neither falls back to generated credentials. Changing the
Secret's content does not roll workloads or rotate database roles; coordinate
backing-service changes and rollouts yourself.

## Email and the Resend webhook

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
`repository@sha256:...` directly. Valid syntax does not prove a digest exists or is
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
