# Plan 12B: Entra ID Datastores in the Chart Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Work sequentially on one branch.

**Goal:** Let consumers deploy the chart against external PostgreSQL and Redis that accept only Microsoft Entra ID tokens: per-service service accounts and workload-identity wiring, no datastore credential in any Secret, an optional migration Job with its own identity, opt-in egress NetworkPolicies, a CA bundle, an Azure guide, and a minikube rehearsal that proves it all without Azure.

**Architecture:** `postgres.enabled` / `redis.enabled` switch off the bundled datastores; `postgres.external` / `redis.external` describe the managed ones. Small helpers in `_helpers.tpl` render each service's datastore environment (password `secretKeyRef`s as today, or the Entra variables of Plan 12A) and its identity (the AKS webhook's label and annotation, or the chart's own projected service-account token). A pre-install/pre-upgrade hook Job runs the three `migrate` subcommands as the migration identity. With everything at its default the rendered chart is byte-identical to today's.

**Tech Stack:** Helm 3 templates, PowerShell + PyYAML chart checks (`deploy\helm\test-chart.ps1`), minikube (docker driver, Calico for enforcement), the Plan 12A services, a DEV ONLY in-cluster token issuer built from `push-credentials`.

---

## Authority and boundaries

Read first: `docs\superpowers\specs\2026-10-18-azure-entra-datastores-design.md` and the
merged Plan 12A (`docs\superpowers\plans\2026-10-18-plan-12a-entra-datastore-credentials.md`):
its variable names are the contract this chart renders. Also
`docs\deployment-configuration.md` and `deploy\minikube\test-network-policy.ps1`.

The chart still provisions nothing outside the release namespace: no Azure resources,
identities, federated credentials, role assignments, private endpoints or DNS. Those are
the consumer's (the guide in Task 8 lists them). The default render, the bundled
datastores, `values-minikube.yaml` and the existing upgrade runbook stay unchanged.

Start from clean, current `main` with Plan 12A merged:

```powershell
git switch main
git pull --ff-only
git switch -c feature/plan-12b-entra-chart
pwsh deploy/helm/test-chart.ps1
helm template push deploy/helm/push-messenger > $env:TEMP\push-default-before.yaml
```

Expected: all chart checks pass. The saved render is the baseline for "unchanged".

Every commit ends with:

```text
Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>
```

## Facts the design depends on

| Fact | Source | Consequence |
|---|---|---|
| The services read Entra settings from `PUSH_POSTGRES_AUTH`, `PUSH_DATABASE_{HOST,PORT,NAME,USER,CA_FILE}`, `PUSH_AUTH_DATABASE_*` (worker), `PUSH_REDIS_{AUTH,URL,USER,CA_FILE}`, `PUSH_RUN_MIGRATIONS`, `AZURE_*`, and `<binary> migrate` with `PUSH_DATABASE_RUNTIME_ROLE` / `PUSH_AUTH_WORKER_ROLE` | Plan 12A, each `config.rs` | The chart renders exactly these |
| The AKS workload-identity webhook mutates pods labelled `azure.workload.identity/use: "true"` whose ServiceAccount carries `azure.workload.identity/client-id`: it projects a token with audience `api://AzureADTokenExchange` to `/var/run/secrets/azure/tokens/azure-identity-token` and sets `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_FEDERATED_TOKEN_FILE`, `AZURE_AUTHORITY_HOST` | Microsoft Learn, AKS workload identity overview | `injection: webhook` renders only the label and annotation |
| A projected `serviceAccountToken` volume works with `automountServiceAccountToken: false` | Kubernetes projected volumes | The chart's own projection (`injection: chart`) keeps the API token unmounted |
| Helm `pre-install` hooks run before the release's ordinary resources exist | Helm hooks documentation | The migration Job's ServiceAccount must be a hook too, with a lower weight |
| The existing NetworkPolicies are Ingress-only and `PolicyMatrix` asserts exactly that for the default render | `templates\networkpolicy.yaml`, `deploy\helm\test-chart.ps1:44-110` | Egress is a separate, opt-in addition |
| `deploy\minikube\test-network-policy.ps1` proves allow/deny with probe pods on an enforcing CNI | Plan 11A | Reused for egress |
| `AzureConfig` accepts `http://` authorities only on loopback | Plan 12A Task 3 | The in-cluster rehearsal issuer serves HTTPS with the test CA, trusted through `SSL_CERT_FILE` |
| The MSRV job builds the whole workspace on Rust 1.85; testcontainers needs 1.88 | `.github\workflows\ci.yml`, PR #23 | A binary may use `push-credentials`' issuer, never its `testing` feature (Task 9 splits the feature) |

## Values (the whole new surface)

```yaml
postgres:
  # true: the bundled PostgreSQL (default). false: an external server, below.
  enabled: true
  external:
    host: ""
    port: 5432
    # azure-entra: Entra ID tokens, no database credential anywhere. password: the
    # services read the URLs from a consumer-managed Secret (secrets.existingSecret).
    auth: azure-entra
    databases:
      messaging: push_messaging
      auth: push_auth
      worker: push_worker
    # azure-entra: each identity's PostgreSQL role name (its Entra principal name).
    users:
      messageSvc: ""
      auth: ""
      worker: ""
      # The migration Job's role (migrations.job.enabled).
      migrate: ""
    # A ConfigMap in the release namespace whose key ca.crt is a PEM bundle trusted
    # instead of the system roots. Empty: the system roots (Azure's public CAs).
    caConfigMap: ""
redis:
  enabled: true
  external:
    # rediss://host:port (Azure Managed Redis: port 10000), no credentials.
    url: ""
    auth: azure-entra
    # The ACL user; empty: the token's oid (Azure Managed Redis).
    user: ""
    caConfigMap: ""
azure:
  workloadIdentity:
    # Client ids of the user-assigned identities; distinct identities are allowed.
    clientIds:
      auth: ""
      messageSvc: ""
      gateway: ""
      worker: ""
      migrate: ""
    # webhook: the AKS workload-identity webhook projects the token and sets AZURE_*.
    # chart: the chart does both (clusters without the webhook; the local rehearsal).
    injection: webhook
    # injection: chart only.
    tenantId: ""
    authorityHost: https://login.microsoftonline.com/
    # injection: chart only: a ConfigMap whose ca.crt the services trust for the
    # authority (SSL_CERT_FILE), e.g. a TLS-inspecting proxy or the rehearsal issuer.
    authorityCaConfigMap: ""
serviceAccounts:
  # Extra annotations on every ServiceAccount the chart creates (Entra mode only).
  annotations: {}
migrations:
  job:
    # A pre-install/pre-upgrade Job migrates as azure.workloadIdentity.clientIds.migrate
    # and grants the runtime identities; the services then skip migrations.
    enabled: false
    backoffLimit: 3
networkPolicy:
  egress:
    # Adds Egress rules to every application policy: DNS, the release's own peers, the
    # datastores and Entra. Off: egress is not restricted (as before).
    enabled: false
    # Peers allowed on port 53 (UDP and TCP), e.g. kube-system's DNS pods.
    dns:
      - namespaceSelector:
          kubernetes.io/metadata.name: kube-system
        podSelector:
          k8s-app: kube-dns
    # CIDRs of the external datastores' private endpoints.
    postgresCidrs: []
    redisCidrs: []
    # CIDRs allowed on port 443 for token requests (Entra ID) and anything else HTTPS
    # the services call (the worker's Resend API).
    httpsCidrs: []
    # Raw NetworkPolicyEgressRule objects added to every application policy.
    extra: []
```

## Files

| File | Change |
|---|---|
| `deploy\helm\push-messenger\values.yaml` | the values above |
| `deploy\helm\push-messenger\templates\_helpers.tpl` | `push.entraPostgres`, `push.entraRedis`, `push.anyEntra`, `push.checkDatastores`, `push.postgresEnv`, `push.redisEnv`, `push.identity*` |
| `templates\validate.yaml` | calls `push.checkDatastores` |
| `templates\secret.yaml` | datastore keys only for bundled (or password-mode external) datastores |
| `templates\postgres.yaml`, `postgres-init.yaml`, `redis.yaml` | rendered only when enabled |
| `templates\auth.yaml`, `message-svc.yaml`, `worker.yaml`, `gateway.yaml` | datastore and identity helpers, ServiceAccount, CA volumes |
| `templates\serviceaccounts.yaml` | new: one per workload in Entra mode |
| `templates\migrate-job.yaml` | new: the hook Job and its ServiceAccount |
| `templates\networkpolicy.yaml` | bundled datastore policies only when enabled; opt-in egress |
| `deploy\helm\test-chart.ps1` | checks for every new path, and "the default is unchanged" |
| `crates\push-credentials\Cargo.toml`, `src\lib.rs`, `src\testing.rs` | feature `stub` (issuer only) split from `testing` |
| `crates\push-smoke\src\bin\push-dev-entra.rs`, `Cargo.toml` | DEV ONLY: test CA and certificates, and an in-cluster HTTPS token issuer |
| `deploy\minikube\entra\*.yaml`, `deploy\minikube\entra-rehearsal.ps1`, `deploy\minikube\values-entra-rehearsal.yaml` | the rehearsal |
| `deploy\minikube\test-network-policy.ps1` | egress allow/deny cases |
| `docs\deployment-azure-entra.md` | new guide |
| `docs\deployment-configuration.md`, `README.md`, roadmap, spec | links and status |

---

### Task 1: Values and validation

**Files:**
- Modify: `deploy\helm\push-messenger\values.yaml`, `templates\_helpers.tpl`, `templates\validate.yaml`
- Test: `deploy\helm\test-chart.ps1`

- [ ] **Step 1: Write the failing checks.** Append to `test-chart.ps1` before its final
summary (keep the file's `Check` style):

```powershell
# ---- Entra ID datastores (Plan 12B) ----
$entraArgs = @(
    '--set', 'postgres.enabled=false',
    '--set', 'postgres.external.host=pg.example.net',
    '--set', 'postgres.external.users.messageSvc=push-message-svc-id',
    '--set', 'postgres.external.users.auth=push-auth-id',
    '--set', 'postgres.external.users.worker=push-worker-id',
    '--set', 'redis.enabled=false',
    '--set', 'redis.external.url=rediss://cache.example.net:10000',
    '--set', 'azure.workloadIdentity.clientIds.auth=00000000-0000-0000-0000-00000000000a',
    '--set', 'azure.workloadIdentity.clientIds.messageSvc=00000000-0000-0000-0000-00000000000b',
    '--set', 'azure.workloadIdentity.clientIds.gateway=00000000-0000-0000-0000-00000000000c',
    '--set', 'azure.workloadIdentity.clientIds.worker=00000000-0000-0000-0000-00000000000d'
)
Check 'entra: a complete external configuration renders' { (Render $entraArgs) -eq 0 }
Check 'entra: an external PostgreSQL needs a host and every user' {
    (Render ($entraArgs + @('--set', 'postgres.external.host='))) -ne 0 -and
        (Render ($entraArgs + @('--set', 'postgres.external.users.worker='))) -ne 0
}
Check 'entra: an Entra Redis needs a rediss:// URL without credentials' {
    (Render ($entraArgs + @('--set', 'redis.external.url=redis://cache:6379'))) -ne 0 -and
        (Render ($entraArgs + @('--set', 'redis.external.url=rediss://:pw@cache:10000'))) -ne 0
}
Check 'entra: every identity that reaches an Entra datastore needs a client id' {
    (Render ($entraArgs + @('--set', 'azure.workloadIdentity.clientIds.gateway='))) -ne 0
}
Check 'entra: the bundled datastores ignore the external settings' {
    $bundled = helm template push $chart --set postgres.external.auth=azure-entra --set redis.external.auth=azure-entra | Out-String
    (Doc $bundled 'Deployment' 'push-auth') -match 'key: auth-database-url' -and $bundled -notmatch 'PUSH_POSTGRES_AUTH'
}
Check 'entra: an external password-mode datastore needs a consumer-managed Secret' {
    (Render @('--set', 'postgres.enabled=false', '--set', 'postgres.external.host=pg',
        '--set', 'postgres.external.auth=password')) -ne 0 -and
    (Render @('--set', 'postgres.enabled=false', '--set', 'postgres.external.host=pg',
        '--set', 'postgres.external.auth=password', '--set', 'secrets.existingSecret=creds')) -eq 0
}
Check 'entra: chart injection needs a tenant; unknown modes are refused' {
    (Render ($entraArgs + @('--set', 'azure.workloadIdentity.injection=chart'))) -ne 0 -and
        (Render ($entraArgs + @('--set', 'azure.workloadIdentity.injection=chart',
            '--set', 'azure.workloadIdentity.tenantId=11111111-2222-3333-4444-555555555555'))) -eq 0 -and
        (Render ($entraArgs + @('--set', 'azure.workloadIdentity.injection=magic'))) -ne 0 -and
        (Render ($entraArgs + @('--set', 'postgres.external.auth=kerberos'))) -ne 0
}
Check 'entra: the migration Job needs an Entra PostgreSQL, its user and its client id' {
    (Render @('--set', 'migrations.job.enabled=true')) -ne 0 -and
        (Render ($entraArgs + @('--set', 'migrations.job.enabled=true'))) -ne 0 -and
        (Render ($entraArgs + @('--set', 'migrations.job.enabled=true',
            '--set', 'postgres.external.users.migrate=push-migrator-id',
            '--set', 'azure.workloadIdentity.clientIds.migrate=00000000-0000-0000-0000-00000000000e'))) -eq 0
}
Check 'entra: role names are Entra principal names' {
    (Render ($entraArgs + @('--set', 'postgres.external.users.auth=bad"name'))) -ne 0
}
Check 'default: the render is unchanged by Plan 12B' {
    $before = Get-Content (Join-Path $env:TEMP 'push-default-before.yaml') -Raw -ErrorAction SilentlyContinue
    (-not $before) -or ($before -eq ($default -replace "`r`n", "`n"))
}
```

The "unchanged" check compares with the baseline saved at the start (the file is absent
on CI, where it passes trivially; the other default checks still guard CI).

- [ ] **Step 2: Run them to see them fail.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: the new `entra:` checks FAIL (the values do not exist; renders ignore them).

- [ ] **Step 3: Values and helpers.** Add the values block above to `values.yaml`
(postgres and redis keys go inside the existing `postgres:` and `redis:` maps; keep their
existing keys and comments). Add to `_helpers.tpl`:

```yaml
{{/* "true" when the services reach an external PostgreSQL with Entra ID tokens. */}}
{{- define "push.entraPostgres" -}}
{{- if and (not .Values.postgres.enabled) (eq .Values.postgres.external.auth "azure-entra") -}}true{{- end -}}
{{- end -}}

{{/* "true" when the services reach an external Redis with Entra ID tokens. */}}
{{- define "push.entraRedis" -}}
{{- if and (not .Values.redis.enabled) (eq .Values.redis.external.auth "azure-entra") -}}true{{- end -}}
{{- end -}}

{{/* "true" when any datastore uses Entra ID: identities and ServiceAccounts are needed. */}}
{{- define "push.anyEntra" -}}
{{- if or (include "push.entraPostgres" .) (include "push.entraRedis" .) -}}true{{- end -}}
{{- end -}}

{{/* A PostgreSQL role name the services accept: 1-63 of A-Z a-z 0-9 _ - . @ */}}
{{- define "push.checkPrincipal" -}}
{{- if not (regexMatch "^[A-Za-z0-9_.@-]{1,63}$" .value) -}}
{{- fail (printf "%s must be an Entra principal name (1 to 63 ASCII letters, digits, _ - . or @)" .name) -}}
{{- end -}}
{{- end -}}

{{/*
Checks the datastore and identity settings (spec 2026-10-18). Bundled datastores ignore
the external blocks. An external datastore needs its address; azure-entra needs a role
per identity (PostgreSQL), a rediss:// URL without credentials (Redis) and a client id for
every identity that reaches it; password mode needs a consumer-managed Secret with the URLs.
*/}}
{{- define "push.checkDatastores" -}}
{{- $pg := .Values.postgres -}}
{{- $redis := .Values.redis -}}
{{- $wi := .Values.azure.workloadIdentity -}}
{{- range $name, $value := dict "postgres.enabled" $pg.enabled "redis.enabled" $redis.enabled "migrations.job.enabled" .Values.migrations.job.enabled "networkPolicy.egress.enabled" .Values.networkPolicy.egress.enabled -}}
{{- if not (kindIs "bool" $value) -}}
{{- fail (printf "%s must be a boolean" $name) -}}
{{- end -}}
{{- end -}}
{{- if not $pg.enabled -}}
{{- if not $pg.external.host -}}
{{- fail "postgres.enabled false needs postgres.external.host" -}}
{{- end -}}
{{- if not (has $pg.external.auth (list "azure-entra" "password")) -}}
{{- fail (printf "postgres.external.auth must be azure-entra or password, not %q" $pg.external.auth) -}}
{{- end -}}
{{- if and (eq $pg.external.auth "password") (not .Values.secrets.existingSecret) -}}
{{- fail "postgres.external.auth password needs secrets.existingSecret holding the database URLs" -}}
{{- end -}}
{{- if eq $pg.external.auth "azure-entra" -}}
{{- range $key := list "messageSvc" "auth" "worker" -}}
{{- include "push.checkPrincipal" (dict "name" (printf "postgres.external.users.%s" $key) "value" (index $pg.external.users $key)) -}}
{{- end -}}
{{- range $key := list "messaging" "auth" "worker" -}}
{{- include "push.checkPrincipal" (dict "name" (printf "postgres.external.databases.%s" $key) "value" (index $pg.external.databases $key)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if not $redis.enabled -}}
{{- if not (has $redis.external.auth (list "azure-entra" "password")) -}}
{{- fail (printf "redis.external.auth must be azure-entra or password, not %q" $redis.external.auth) -}}
{{- end -}}
{{- if eq $redis.external.auth "azure-entra" -}}
{{- if not (regexMatch "^rediss://[^@/:]+(:[0-9]+)?/?$" $redis.external.url) -}}
{{- fail "redis.external.url must be rediss://host:port without credentials for azure-entra" -}}
{{- end -}}
{{- else if not .Values.secrets.existingSecret -}}
{{- fail "redis.external.auth password needs secrets.existingSecret holding redis-url" -}}
{{- end -}}
{{- end -}}
{{- if include "push.anyEntra" . -}}
{{- if not (has $wi.injection (list "webhook" "chart")) -}}
{{- fail (printf "azure.workloadIdentity.injection must be webhook or chart, not %q" $wi.injection) -}}
{{- end -}}
{{- if and (eq $wi.injection "chart") (not $wi.tenantId) -}}
{{- fail "azure.workloadIdentity.injection chart needs azure.workloadIdentity.tenantId" -}}
{{- end -}}
{{- $needs := list -}}
{{- if include "push.entraPostgres" . -}}{{- $needs = concat $needs (list "auth" "messageSvc" "worker") -}}{{- end -}}
{{- if include "push.entraRedis" . -}}{{- $needs = concat $needs (list "auth" "messageSvc" "gateway") -}}{{- end -}}
{{- range $key := uniq $needs -}}
{{- if not (index $wi.clientIds $key) -}}
{{- fail (printf "azure.workloadIdentity.clientIds.%s is required: that workload reaches an Entra ID datastore" $key) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if .Values.migrations.job.enabled -}}
{{- if not (include "push.entraPostgres" .) -}}
{{- fail "migrations.job.enabled needs an external PostgreSQL with postgres.external.auth azure-entra" -}}
{{- end -}}
{{- include "push.checkPrincipal" (dict "name" "postgres.external.users.migrate" "value" $pg.external.users.migrate) -}}
{{- if not $wi.clientIds.migrate -}}
{{- fail "migrations.job.enabled needs azure.workloadIdentity.clientIds.migrate" -}}
{{- end -}}
{{- end -}}
{{- end -}}
```

and in `templates\validate.yaml` add `{{- include "push.checkDatastores" . -}}` after
`push.checkIngress`.

- [ ] **Step 4: Run.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: the validation checks pass; "a complete external configuration renders" passes
(nothing uses the values yet); every existing check still passes.

- [ ] **Step 5: Commit.**

```powershell
git add -A
git commit -m "feat(chart): add the external datastore and workload identity values"
```

### Task 2: Bundled datastores only when enabled; no datastore credential otherwise

**Files:**
- Modify: `templates\secret.yaml`, `templates\postgres.yaml`, `templates\postgres-init.yaml`, `templates\redis.yaml`, `templates\networkpolicy.yaml`
- Test: `deploy\helm\test-chart.ps1`

- [ ] **Step 1: Failing checks** (append to the Entra section):

```powershell
$entra = helm template push $chart @entraArgs | Out-String
Check 'entra: no bundled datastore, and no datastore credential in any Secret or ConfigMap' {
    $docs = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
names = {(d["kind"], d["metadata"]["name"]) for d in docs}
assert ("StatefulSet", "push-postgres") not in names and ("Deployment", "push-redis") not in names
assert ("ConfigMap", "push-postgres-init") not in names
for d in docs:
    if d["kind"] in ("Secret", "ConfigMap"):
        data = {**d.get("data", {}), **d.get("stringData", {})}
        bad = [k for k in data if "database" in k or "redis" in k or "db-password" in k or k == "postgres-password"]
        assert not bad, (d["metadata"]["name"], bad)
'@
    $entra | & $python.Source -c $docs; $LASTEXITCODE -eq 0
}
Check 'entra: only the four application policies exist' {
    ($entra -split "(?m)^---\s*$" | Where-Object { $_ -match '(?m)^kind: NetworkPolicy\s*$' }).Count -eq 4 -and
        -not (Doc $entra 'NetworkPolicy' 'push-postgres') -and -not (Doc $entra 'NetworkPolicy' 'push-redis')
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: both FAIL.

- [ ] **Step 3: Gate the bundled datastores.** Wrap the whole of `postgres.yaml` and
`postgres-init.yaml` in `{{- if .Values.postgres.enabled }}` … `{{- end }}` and the whole
of `redis.yaml` in `{{- if .Values.redis.enabled }}` … `{{- end }}`, so the default
output is unchanged (place the `if` before the first line and keep the original first
line's whitespace-trimming).

In `secret.yaml`: guard the `messaging-db-password` upgrade check with
`.Values.postgres.enabled`; compute the database passwords only when
`.Values.postgres.enabled` and the Redis password only when `.Values.redis.enabled`; in
`stringData` wrap `postgres-password`, `messaging-db-password`, `database-url`,
`auth-db-password`, `auth-database-url`, `worker-db-password`, `worker-database-url`,
`worker-auth-database-url` (and their comments) in `{{- if .Values.postgres.enabled }}`
and `redis-password`, `redis-url` in `{{- if .Values.redis.enabled }}`. The application
keys (`auth-signing-key`, `auth-link-key`, the three internal tokens, Resend) stay
unconditional: they are not datastore credentials (spec §8).

In `networkpolicy.yaml`, build the policy list conditionally:

```yaml
{{- $policies := list
  (dict "component" "message-svc" "rules" (list
    (dict "port" 50051 "callers" (list "gateway"))
    (dict "port" 50052 "callers" (list "worker"))
    (dict "port" 50053 "callers" (list "auth"))))
-}}
{{- if .Values.postgres.enabled -}}
{{- $policies = append $policies (dict "component" "postgres" "rules" (list
    (dict "port" 5432 "callers" (list "message-svc" "auth" "worker")))) -}}
{{- end -}}
{{- if .Values.redis.enabled -}}
{{- $policies = append $policies (dict "component" "redis" "rules" (list
    (dict "port" 6379 "callers" (list "message-svc" "auth" "gateway")))) -}}
{{- end -}}
{{- $policies = concat $policies (list
  (dict "component" "auth" "rules" (list
    (dict "port" 8080 "callers" (list "gateway") "controllers" $controllers)))
  (dict "component" "gateway" "rules" $gatewayRules)
  (dict "component" "worker" "rules" $workerRules)) -}}
```

The order (message-svc, postgres, redis, auth, gateway, worker) and the rendered text
of the default case stay identical.

- [ ] **Step 4: Run.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: all pass, including "the render is unchanged by Plan 12B".

- [ ] **Step 5: Commit.**

```powershell
git add -A
git commit -m "feat(chart): render the bundled datastores and their credentials only when enabled"
```

### Task 3: The services' datastore environment

**Files:**
- Modify: `templates\_helpers.tpl`, `templates\auth.yaml`, `templates\message-svc.yaml`, `templates\worker.yaml`, `templates\gateway.yaml`
- Test: `deploy\helm\test-chart.ps1`

- [ ] **Step 1: Failing check.**

```powershell
Check 'entra: each service gets exactly its datastore variables and no datastore secret' {
    $envCheck = @'
import json, sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
def env(name):
    d = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == name)
    c = d["spec"]["template"]["spec"]["containers"][0]
    return {e["name"]: e.get("value", e.get("valueFrom")) for e in c["env"]}
pg = {"PUSH_POSTGRES_AUTH": "azure-entra", "PUSH_DATABASE_HOST": "pg.example.net", "PUSH_DATABASE_PORT": "5432", "PUSH_RUN_MIGRATIONS": "true"}
redis = {"PUSH_REDIS_AUTH": "azure-entra", "PUSH_REDIS_URL": "rediss://cache.example.net:10000"}
cases = {
    "push-auth": {**pg, **redis, "PUSH_DATABASE_NAME": "push_auth", "PUSH_DATABASE_USER": "push-auth-id", "PUSH_AUTH_WORKER_ROLE": "push-worker-id"},
    "push-message-svc": {**pg, **redis, "PUSH_DATABASE_NAME": "push_messaging", "PUSH_DATABASE_USER": "push-message-svc-id"},
    "push-worker": {**pg, "PUSH_DATABASE_NAME": "push_worker", "PUSH_DATABASE_USER": "push-worker-id", "PUSH_AUTH_DATABASE_NAME": "push_auth"},
    "push-gateway": redis,
}
for name, expected in cases.items():
    actual = env(name)
    for key, value in expected.items():
        assert actual.get(key) == value, (name, key, actual.get(key))
    assert "PUSH_DATABASE_URL" not in actual and "PUSH_AUTH_DATABASE_URL" not in actual, name
    refs = [v["secretKeyRef"]["key"] for v in actual.values() if isinstance(v, dict) and "secretKeyRef" in v]
    assert not [k for k in refs if "database" in k or "redis" in k], (name, refs)
'@
    $entra | & $python.Source -c $envCheck; $LASTEXITCODE -eq 0
}
```

- [ ] **Step 2: Run it to see it fail.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: FAIL (the services still reference `auth-database-url` etc.).

- [ ] **Step 3: Helpers** in `_helpers.tpl`:

```yaml
{{/*
A service's PostgreSQL variables: the URL from the Secret (bundled, or external password
mode), or the Entra variables of Plan 12A. Arguments: root, secretKey, database, user.
*/}}
{{- define "push.postgresEnv" -}}
{{- $root := .root -}}
{{- if include "push.entraPostgres" $root }}
- name: PUSH_POSTGRES_AUTH
  value: azure-entra
- name: PUSH_DATABASE_HOST
  value: {{ $root.Values.postgres.external.host | quote }}
- name: PUSH_DATABASE_PORT
  value: {{ $root.Values.postgres.external.port | quote }}
- name: PUSH_DATABASE_NAME
  value: {{ .database | quote }}
- name: PUSH_DATABASE_USER
  value: {{ .user | quote }}
{{- if $root.Values.postgres.external.caConfigMap }}
- name: PUSH_DATABASE_CA_FILE
  value: /etc/push/postgres-ca/ca.crt
{{- end }}
- name: PUSH_RUN_MIGRATIONS
  value: {{ ternary "false" "true" $root.Values.migrations.job.enabled | quote }}
{{- else }}
- name: PUSH_DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: {{ include "push.secretName" $root }}
      key: {{ .secretKey }}
{{- end }}
{{- end -}}

{{/* A service's Redis variables: the URL from the Secret, or the Entra variables. */}}
{{- define "push.redisEnv" -}}
{{- if include "push.entraRedis" . }}
- name: PUSH_REDIS_AUTH
  value: azure-entra
- name: PUSH_REDIS_URL
  value: {{ .Values.redis.external.url | quote }}
{{- with .Values.redis.external.user }}
- name: PUSH_REDIS_USER
  value: {{ . | quote }}
{{- end }}
{{- if .Values.redis.external.caConfigMap }}
- name: PUSH_REDIS_CA_FILE
  value: /etc/push/redis-ca/ca.crt
{{- end }}
{{- else }}
- name: PUSH_REDIS_URL
  valueFrom:
    secretKeyRef:
      name: {{ include "push.secretName" . }}
      key: redis-url
{{- end }}
{{- end -}}
```

- [ ] **Step 4: Use them.** In each template replace the datastore `env` entries, keeping
every comment line above its `include` (comments are part of the rendered output):

`auth.yaml`:

```yaml
            # Its own database only: auth-svc never sees message-svc's credentials.
            {{- include "push.postgresEnv" (dict "root" . "secretKey" "auth-database-url" "database" .Values.postgres.external.databases.auth "user" .Values.postgres.external.users.auth) | nindent 12 }}
            {{- include "push.redisEnv" . | nindent 12 }}
```

and its worker role:

```yaml
            - name: PUSH_AUTH_WORKER_ROLE
              value: {{ ternary .Values.postgres.external.users.worker "push_worker" (eq (include "push.entraPostgres" .) "true") | quote }}
```

(the default renders `value: "push_worker"`: if the baseline has `value: push_worker`
unquoted, keep the bundled branch unquoted so the default render stays identical:
`{{- if include "push.entraPostgres" . }}{{ .Values.postgres.external.users.worker | quote }}{{- else }}push_worker{{- end }}`.)

`message-svc.yaml`: the same two includes with `secretKey` `database-url`, database
`.Values.postgres.external.databases.messaging`, user `.Values.postgres.external.users.messageSvc`.

`gateway.yaml`: only `{{- include "push.redisEnv" . | nindent 12 }}`.

`worker.yaml`: the PostgreSQL include with `worker-database-url`, `.databases.worker`,
`.users.worker`, then for the second database:

```yaml
            {{- if include "push.entraPostgres" . }}
            # auth-svc's database on the same server, as the same principal.
            - name: PUSH_AUTH_DATABASE_NAME
              value: {{ .Values.postgres.external.databases.auth | quote }}
            {{- else }}
            (the existing PUSH_AUTH_DATABASE_URL entry, unchanged)
            {{- end }}
```

- [ ] **Step 5: Run.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: all pass, including the unchanged default render. A diff in the default render
is a whitespace or quoting slip: compare with
`helm template push deploy/helm/push-messenger | Out-File $env:TEMP\after.yaml; git diff --no-index $env:TEMP\push-default-before.yaml $env:TEMP\after.yaml`.

- [ ] **Step 6: Commit.**

```powershell
git add -A
git commit -m "feat(chart): render the Entra ID datastore variables of each service"
```

### Task 4: ServiceAccounts, workload identity and CA bundles

**Files:**
- Create: `templates\serviceaccounts.yaml`
- Modify: `templates\_helpers.tpl`, the four service templates
- Test: `deploy\helm\test-chart.ps1`

- [ ] **Step 1: Failing checks.**

```powershell
Check 'entra webhook: one ServiceAccount per workload with its client id; pods opt in' {
    $sa = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
ids = {"push-auth": "a", "push-message-svc": "b", "push-gateway": "c", "push-worker": "d"}
for name, suffix in ids.items():
    account = next(d for d in docs if d["kind"] == "ServiceAccount" and d["metadata"]["name"] == name)
    assert account["metadata"]["annotations"]["azure.workload.identity/client-id"].endswith(suffix), name
    assert account["automountServiceAccountToken"] is False
    pod = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == name)["spec"]["template"]
    assert pod["metadata"]["labels"]["azure.workload.identity/use"] == "true", name
    assert pod["spec"]["serviceAccountName"] == name and pod["spec"]["automountServiceAccountToken"] is False
    assert not [e for e in pod["spec"]["containers"][0]["env"] if e["name"].startswith("AZURE_")], name
'@
    $entra | & $python.Source -c $sa; $LASTEXITCODE -eq 0
}
Check 'entra chart injection: the chart projects the token, sets AZURE_* and mounts the CAs' {
    $chartInjection = helm template push $chart @entraArgs `
        --set azure.workloadIdentity.injection=chart `
        --set azure.workloadIdentity.tenantId=11111111-2222-3333-4444-555555555555 `
        --set azure.workloadIdentity.authorityCaConfigMap=authority-ca `
        --set postgres.external.caConfigMap=pg-ca --set redis.external.caConfigMap=redis-ca | Out-String
    $projection = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
pod = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == "push-auth")["spec"]["template"]
assert "azure.workload.identity/use" not in pod["metadata"]["labels"]
volumes = {v["name"]: v for v in pod["spec"]["volumes"]}
source = volumes["azure-identity-token"]["projected"]["sources"][0]["serviceAccountToken"]
assert source == {"audience": "api://AzureADTokenExchange", "expirationSeconds": 3600, "path": "azure-identity-token"}
assert volumes["postgres-ca"]["configMap"]["name"] == "pg-ca" and volumes["redis-ca"]["configMap"]["name"] == "redis-ca"
assert volumes["authority-ca"]["configMap"]["name"] == "authority-ca"
container = pod["spec"]["containers"][0]
env = {e["name"]: e.get("value") for e in container["env"]}
assert env["AZURE_CLIENT_ID"].endswith("a") and env["AZURE_TENANT_ID"].startswith("1111")
assert env["AZURE_FEDERATED_TOKEN_FILE"] == "/var/run/secrets/azure/tokens/azure-identity-token"
assert env["AZURE_AUTHORITY_HOST"] == "https://login.microsoftonline.com/"
assert env["SSL_CERT_FILE"] == "/etc/push/authority-ca/ca.crt"
assert env["PUSH_DATABASE_CA_FILE"] == "/etc/push/postgres-ca/ca.crt"
mounts = {m["mountPath"]: m for m in container["volumeMounts"]}
for path in ["/var/run/secrets/azure/tokens", "/etc/push/postgres-ca", "/etc/push/redis-ca", "/etc/push/authority-ca"]:
    assert mounts[path]["readOnly"] is True, path
'@
    $chartInjection | & $python.Source -c $projection; $LASTEXITCODE -eq 0
}
Check 'default: no ServiceAccount and no identity wiring' {
    $default -notmatch '(?m)^kind: ServiceAccount' -and $default -notmatch 'azure.workload.identity' -and
        $default -notmatch 'serviceAccountName'
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: the two `entra` checks FAIL.

- [ ] **Step 3: `serviceaccounts.yaml`.**

```yaml
{{- /* One ServiceAccount per workload when a datastore uses Entra ID: each carries its
own identity (azure.workload.identity/client-id), so services can have distinct ones. */ -}}
{{- if include "push.anyEntra" . -}}
{{- range $component, $key := dict "auth" "auth" "message-svc" "messageSvc" "gateway" "gateway" "worker" "worker" }}
{{- $context := dict "root" $ "component" $component }}
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: {{ include "push.name" $context }}
  labels:
    {{- include "push.labels" $context | nindent 4 }}
  {{- $annotations := deepCopy $.Values.serviceAccounts.annotations }}
  {{- with index $.Values.azure.workloadIdentity.clientIds $key }}
  {{- $_ := set $annotations "azure.workload.identity/client-id" . }}
  {{- end }}
  {{- with $annotations }}
  annotations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
automountServiceAccountToken: false
{{- end }}
{{- end }}
```

- [ ] **Step 4: Identity helpers** in `_helpers.tpl`. Each takes `(dict "root" . "key" "<clientIds key>")`:

```yaml
{{/* Pod label: the AKS webhook mutates only pods that opt in. */}}
{{- define "push.identityLabels" -}}
{{- $root := .root -}}
{{- if and (include "push.anyEntra" $root) (eq $root.Values.azure.workloadIdentity.injection "webhook") (index $root.Values.azure.workloadIdentity.clientIds .key) }}
azure.workload.identity/use: "true"
{{- end }}
{{- end -}}

{{/* Pod spec line: the workload's ServiceAccount in Entra mode. */}}
{{- define "push.serviceAccount" -}}
{{- if include "push.anyEntra" .root }}
serviceAccountName: {{ include "push.name" (dict "root" .root "component" .component) }}
{{- end }}
{{- end -}}

{{/* Container env: the AZURE_* variables when the chart injects the identity itself. */}}
{{- define "push.identityEnv" -}}
{{- $root := .root -}}
{{- $wi := $root.Values.azure.workloadIdentity -}}
{{- if and (include "push.anyEntra" $root) (eq $wi.injection "chart") (index $wi.clientIds .key) }}
- name: AZURE_CLIENT_ID
  value: {{ index $wi.clientIds .key | quote }}
- name: AZURE_TENANT_ID
  value: {{ $wi.tenantId | quote }}
- name: AZURE_FEDERATED_TOKEN_FILE
  value: /var/run/secrets/azure/tokens/azure-identity-token
- name: AZURE_AUTHORITY_HOST
  value: {{ $wi.authorityHost | quote }}
{{- if $wi.authorityCaConfigMap }}
- name: SSL_CERT_FILE
  value: /etc/push/authority-ca/ca.crt
{{- end }}
{{- end }}
{{- end -}}

{{/* Container volume mounts: the projected token and the CA bundles in use. */}}
{{- define "push.identityMounts" -}}
{{- $root := .root -}}
{{- $wi := $root.Values.azure.workloadIdentity -}}
{{- if and (include "push.anyEntra" $root) (eq $wi.injection "chart") (index $wi.clientIds .key) }}
- name: azure-identity-token
  mountPath: /var/run/secrets/azure/tokens
  readOnly: true
{{- if $wi.authorityCaConfigMap }}
- name: authority-ca
  mountPath: /etc/push/authority-ca
  readOnly: true
{{- end }}
{{- end }}
{{- if and (include "push.entraPostgres" $root) $root.Values.postgres.external.caConfigMap .postgres }}
- name: postgres-ca
  mountPath: /etc/push/postgres-ca
  readOnly: true
{{- end }}
{{- if and (include "push.entraRedis" $root) $root.Values.redis.external.caConfigMap .redis }}
- name: redis-ca
  mountPath: /etc/push/redis-ca
  readOnly: true
{{- end }}
{{- end -}}

{{/* Pod volumes matching push.identityMounts. */}}
{{- define "push.identityVolumes" -}}
{{- $root := .root -}}
{{- $wi := $root.Values.azure.workloadIdentity -}}
{{- if and (include "push.anyEntra" $root) (eq $wi.injection "chart") (index $wi.clientIds .key) }}
- name: azure-identity-token
  projected:
    sources:
      - serviceAccountToken:
          audience: api://AzureADTokenExchange
          expirationSeconds: 3600
          path: azure-identity-token
{{- if $wi.authorityCaConfigMap }}
- name: authority-ca
  configMap:
    name: {{ $wi.authorityCaConfigMap }}
{{- end }}
{{- end }}
{{- if and (include "push.entraPostgres" $root) $root.Values.postgres.external.caConfigMap .postgres }}
- name: postgres-ca
  configMap:
    name: {{ $root.Values.postgres.external.caConfigMap }}
{{- end }}
{{- if and (include "push.entraRedis" $root) $root.Values.redis.external.caConfigMap .redis }}
- name: redis-ca
  configMap:
    name: {{ $root.Values.redis.external.caConfigMap }}
{{- end }}
{{- end -}}
```

- [ ] **Step 5: Wire each workload.** In each of the four Deployments (shown for auth;
`message-svc` uses key `messageSvc` with `postgres` and `redis` true; `gateway` key
`gateway`, `postgres` false; `worker` key `worker`, `redis` false):

```yaml
    metadata:
      labels:
        {{- include "push.labels" (dict "root" . "component" "auth") | nindent 8 }}
        {{- include "push.identityLabels" (dict "root" . "key" "auth") | nindent 8 }}
...
    spec:
      automountServiceAccountToken: false
      {{- include "push.serviceAccount" (dict "root" . "component" "auth") | nindent 6 }}
...
          env:
            {{- include "push.identityEnv" (dict "root" . "key" "auth") | nindent 12 }}
            (the existing entries)
          {{- with include "push.identityMounts" (dict "root" . "key" "auth" "postgres" true "redis" true) }}
          volumeMounts:
            {{- . | nindent 12 }}
          {{- end }}
...
      {{- with include "push.identityVolumes" (dict "root" . "key" "auth" "postgres" true "redis" true) }}
      volumes:
        {{- . | nindent 8 }}
      {{- end }}
```

An empty helper renders nothing, so the default output stays identical; with `nindent`
on an empty string Helm emits a newline, so where the "unchanged" check fails, wrap the
include in `{{- with include ... }}{{- . | nindent N }}{{- end }}` as for the volumes.

- [ ] **Step 6: Run.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: all pass.

- [ ] **Step 7: Commit.**

```powershell
git add -A
git commit -m "feat(chart): give each workload a ServiceAccount and its workload identity"
```

### Task 5: The migration Job

**Files:**
- Create: `templates\migrate-job.yaml`
- Test: `deploy\helm\test-chart.ps1`

- [ ] **Step 1: Failing check.**

```powershell
Check 'entra: the migration hook Job migrates all three databases as its own identity' {
    $jobArgs = $entraArgs + @('--set', 'migrations.job.enabled=true',
        '--set', 'postgres.external.users.migrate=push-migrator-id',
        '--set', 'azure.workloadIdentity.clientIds.migrate=00000000-0000-0000-0000-00000000000e')
    $rendered = helm template push $chart @jobArgs | Out-String
    $job = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
job = next(d for d in docs if d["kind"] == "Job" and d["metadata"]["name"] == "push-migrate")
account = next(d for d in docs if d["kind"] == "ServiceAccount" and d["metadata"]["name"] == "push-migrate")
hooks = lambda d: d["metadata"]["annotations"]
assert hooks(job)["helm.sh/hook"] == "pre-install,pre-upgrade"
assert int(hooks(account)["helm.sh/hook-weight"]) < int(hooks(job)["helm.sh/hook-weight"])
assert account["metadata"]["annotations"]["azure.workload.identity/client-id"].endswith("e")
pod = job["spec"]["template"]["spec"]
assert pod["serviceAccountName"] == "push-migrate" and pod["restartPolicy"] == "Never"
steps = pod["initContainers"] + pod["containers"]
assert [c["name"] for c in steps] == ["message-svc", "auth", "worker"]
expected = {"message-svc": ("push_messaging", "push-message-svc-id"), "auth": ("push_auth", "push-auth-id"), "worker": ("push_worker", "push-worker-id")}
for c in steps:
    env = {e["name"]: e.get("value") for e in c["env"]}
    assert c["args"] == ["migrate"], c["name"]
    assert (env["PUSH_DATABASE_NAME"], env["PUSH_DATABASE_RUNTIME_ROLE"]) == expected[c["name"]]
    assert env["PUSH_DATABASE_USER"] == "push-migrator-id"
    assert c["securityContext"]["readOnlyRootFilesystem"] is True
assert {e["name"]: e.get("value") for e in steps[1]["env"]}["PUSH_AUTH_WORKER_ROLE"] == "push-worker-id"
services = [d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] in ("push-auth", "push-message-svc", "push-worker")]
for d in services:
    env = {e["name"]: e.get("value") for e in d["spec"]["template"]["spec"]["containers"][0]["env"]}
    assert env["PUSH_RUN_MIGRATIONS"] == "false", d["metadata"]["name"]
'@
    $rendered | & $python.Source -c $job; $LASTEXITCODE -eq 0
}
```

- [ ] **Step 2: Run it to see it fail.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: FAIL (no Job).

- [ ] **Step 3: `migrate-job.yaml`.**

```yaml
{{- /*
The migration Job (migrations.job.enabled): before every install and upgrade, the
migration identity applies each service's migrations and grants the runtime identities
their access (Plan 12A `migrate`). The services run with PUSH_RUN_MIGRATIONS=false and
need no DDL rights. Its ServiceAccount is a hook too: pre-install hooks run before the
release's ordinary resources exist.
*/ -}}
{{- if .Values.migrations.job.enabled -}}
{{- $context := dict "root" . "component" "migrate" -}}
{{- $name := include "push.name" $context -}}
{{- $pg := .Values.postgres.external -}}
apiVersion: v1
kind: ServiceAccount
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" $context | nindent 4 }}
  annotations:
    helm.sh/hook: pre-install,pre-upgrade
    helm.sh/hook-weight: "-10"
    helm.sh/hook-delete-policy: before-hook-creation
    azure.workload.identity/client-id: {{ .Values.azure.workloadIdentity.clientIds.migrate | quote }}
    {{- with .Values.serviceAccounts.annotations }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
automountServiceAccountToken: false
---
apiVersion: batch/v1
kind: Job
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" $context | nindent 4 }}
  annotations:
    helm.sh/hook: pre-install,pre-upgrade
    helm.sh/hook-weight: "0"
    helm.sh/hook-delete-policy: before-hook-creation,hook-succeeded
spec:
  backoffLimit: {{ .Values.migrations.job.backoffLimit }}
  template:
    metadata:
      labels:
        {{- include "push.labels" $context | nindent 8 }}
        {{- include "push.identityLabels" (dict "root" . "key" "migrate") | nindent 8 }}
    spec:
      automountServiceAccountToken: false
      serviceAccountName: {{ $name }}
      restartPolicy: Never
      securityContext:
        seccompProfile:
          type: RuntimeDefault
      {{- $steps := list
        (dict "name" "message-svc" "image" .Values.images.messageSvc "database" $pg.databases.messaging "runtime" $pg.users.messageSvc)
        (dict "name" "auth" "image" .Values.images.auth "database" $pg.databases.auth "runtime" $pg.users.auth "worker" $pg.users.worker)
        (dict "name" "worker" "image" .Values.images.worker "database" $pg.databases.worker "runtime" $pg.users.worker) }}
      initContainers:
        {{- range $step := (initial $steps) }}
        {{- include "push.migrateStep" (dict "root" $ "step" $step) | nindent 8 }}
        {{- end }}
      containers:
        {{- include "push.migrateStep" (dict "root" . "step" (last $steps)) | nindent 8 }}
      {{- with include "push.identityVolumes" (dict "root" . "key" "migrate" "postgres" true "redis" false) }}
      volumes:
        {{- . | nindent 8 }}
      {{- end }}
{{- end }}
```

and in `_helpers.tpl`:

```yaml
{{/* One `migrate` step of the migration Job. */}}
{{- define "push.migrateStep" -}}
{{- $root := .root -}}
{{- $step := .step -}}
- name: {{ $step.name }}
  image: {{ include "push.image" $step.image }}
  imagePullPolicy: {{ $root.Values.images.pullPolicy }}
  args: ["migrate"]
  {{- include "push.containerSecurity" $root | nindent 2 }}
  env:
    {{- include "push.identityEnv" (dict "root" $root "key" "migrate") | nindent 4 }}
    - name: PUSH_POSTGRES_AUTH
      value: azure-entra
    - name: PUSH_DATABASE_HOST
      value: {{ $root.Values.postgres.external.host | quote }}
    - name: PUSH_DATABASE_PORT
      value: {{ $root.Values.postgres.external.port | quote }}
    - name: PUSH_DATABASE_NAME
      value: {{ $step.database | quote }}
    - name: PUSH_DATABASE_USER
      value: {{ $root.Values.postgres.external.users.migrate | quote }}
    - name: PUSH_DATABASE_RUNTIME_ROLE
      value: {{ $step.runtime | quote }}
    {{- with $step.worker }}
    - name: PUSH_AUTH_WORKER_ROLE
      value: {{ . | quote }}
    {{- end }}
    {{- if $root.Values.postgres.external.caConfigMap }}
    - name: PUSH_DATABASE_CA_FILE
      value: /etc/push/postgres-ca/ca.crt
    {{- end }}
  {{- with include "push.identityMounts" (dict "root" $root "key" "migrate" "postgres" true "redis" false) }}
  volumeMounts:
    {{- . | nindent 4 }}
  {{- end }}
  resources:
    requests:
      cpu: 50m
      memory: 64Mi
    limits:
      cpu: 500m
      memory: 256Mi
{{- end -}}
```

- [ ] **Step 4: Run.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: all pass.

- [ ] **Step 5: Commit.**

```powershell
git add -A
git commit -m "feat(chart): migrate external databases with a hook Job and its own identity"
```

### Task 6: Opt-in egress NetworkPolicies

**Files:**
- Modify: `templates\networkpolicy.yaml`
- Test: `deploy\helm\test-chart.ps1`

- [ ] **Step 1: Failing check.**

```powershell
Check 'egress: opt-in rules per component (DNS, release peers, datastores, HTTPS, extra)' {
    $egressArgs = $entraArgs + @('--set', 'networkPolicy.egress.enabled=true',
        '--set', 'networkPolicy.egress.postgresCidrs={10.1.0.4/32}',
        '--set', 'networkPolicy.egress.redisCidrs={10.1.0.5/32}',
        '--set', 'networkPolicy.egress.httpsCidrs={20.190.128.0/18}')
    $rendered = helm template push $chart @egressArgs | Out-String
    $egress = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
policies = {d["metadata"]["name"]: d["spec"] for d in docs if d["kind"] == "NetworkPolicy"}
def peers(rule):
    return [p.get("ipBlock", {}).get("cidr") or p["podSelector"]["matchLabels"].get("push-messenger/network-peer") or "dns" for p in rule["to"]]
def ports(rule):
    return sorted((p["protocol"], p["port"]) for p in rule.get("ports", []))
expected = {
    "push-auth": {("message-svc", 50053), ("10.1.0.4/32", 5432), ("10.1.0.5/32", 10000), ("20.190.128.0/18", 443)},
    "push-message-svc": {("10.1.0.4/32", 5432), ("10.1.0.5/32", 10000), ("20.190.128.0/18", 443)},
    "push-gateway": {("auth", 8080), ("message-svc", 50051), ("10.1.0.5/32", 10000), ("20.190.128.0/18", 443)},
    "push-worker": {("message-svc", 50052), ("10.1.0.4/32", 5432), ("20.190.128.0/18", 443)},
}
for name, wanted in expected.items():
    spec = policies[name]
    assert spec["policyTypes"] == ["Ingress", "Egress"], name
    rules = spec["egress"]
    dns = [r for r in rules if ports(r) == [("TCP", 53), ("UDP", 53)]]
    assert len(dns) == 1, name
    got = {(peer, port) for r in rules if r not in dns for peer in peers(r) for (_, port) in ports(r)}
    assert got == wanted, (name, got)
'@
    $rendered | & $python.Source -c $egress; $LASTEXITCODE -eq 0
}
Check 'egress: with bundled datastores the rules name their pods' {
    $bundled = helm template push $chart --set networkPolicy.egress.enabled=true | Out-String
    $policy = Doc $bundled 'NetworkPolicy' 'push-auth'
    $policy -match 'Egress' -and $policy -match 'network-peer: postgres' -and $policy -match 'network-peer: redis' -and
        (Doc $bundled 'NetworkPolicy' 'push-postgres') -match '(?s)policyTypes:\s+- Ingress\s+- Egress.*egress: \[\]'
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: both FAIL.

- [ ] **Step 3: Implement.** In `networkpolicy.yaml`, give each policy dict an
`egress` list (empty for postgres and redis) when `.Values.networkPolicy.egress.enabled`,
built like this before the `range`:

```yaml
{{- $egress := .Values.networkPolicy.egress -}}
{{- $postgresTo := list (dict "peer" "postgres" "port" 5432) -}}
{{- if not .Values.postgres.enabled -}}
{{- $postgresTo = list -}}
{{- range $egress.postgresCidrs }}{{- $postgresTo = append $postgresTo (dict "cidr" . "port" (int $.Values.postgres.external.port)) -}}{{- end -}}
{{- end -}}
{{- $redisPort := 6379 -}}
{{- $redisTo := list (dict "peer" "redis" "port" 6379) -}}
{{- if not .Values.redis.enabled -}}
{{- $found := regexFind ":[0-9]+/?$" .Values.redis.external.url | trimPrefix ":" | trimSuffix "/" -}}
{{- $redisPort = ternary (int $found) 6379 (ne $found "") -}}
{{- $redisTo = list -}}
{{- range $egress.redisCidrs }}{{- $redisTo = append $redisTo (dict "cidr" . "port" $redisPort) -}}{{- end -}}
{{- end -}}
{{- $https := list -}}
{{- range $egress.httpsCidrs }}{{- $https = append $https (dict "cidr" . "port" 443) -}}{{- end -}}
{{- $egressOf := dict
  "auth" (concat (list (dict "peer" "message-svc" "port" 50053)) $postgresTo $redisTo $https)
  "message-svc" (concat $postgresTo $redisTo $https)
  "gateway" (concat (list (dict "peer" "auth" "port" 8080) (dict "peer" "message-svc" "port" 50051)) $redisTo $https)
  "worker" (concat (list (dict "peer" "message-svc" "port" 50052)) $postgresTo $https)
  "postgres" (list)
  "redis" (list) -}}
```

and in the policy body:

```yaml
  policyTypes:
    - Ingress
    {{- if $egress.enabled }}
    - Egress
    {{- end }}
  (the ingress rules, unchanged)
  {{- if $egress.enabled }}
  {{- $targets := index $egressOf .component }}
  {{- if and (not $targets) (has .component (list "postgres" "redis")) }}
  egress: []
  {{- else }}
  egress:
    - ports:
        - protocol: UDP
          port: 53
        - protocol: TCP
          port: 53
      to:
        {{- range $egress.dns }}
        - namespaceSelector:
            matchLabels:
              {{- toYaml .namespaceSelector | nindent 14 }}
          podSelector:
            matchLabels:
              {{- toYaml .podSelector | nindent 14 }}
        {{- end }}
    {{- range $targets }}
    - ports:
        - protocol: TCP
          port: {{ .port }}
      to:
        {{- if .peer }}
        - podSelector:
            matchLabels:
              app.kubernetes.io/instance: {{ $.Release.Name }}
              push-messenger/network-peer: {{ .peer }}
        {{- else }}
        - ipBlock:
            cidr: {{ .cidr }}
        {{- end }}
    {{- end }}
    {{- with $egress.extra }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- end }}
  {{- end }}
```

Validate in `push.checkDatastores` that every CIDR matches
`^[0-9a-fA-F:.]+/[0-9]{1,3}$` and that `dns` entries have non-empty selector maps (reuse
the ingress peer check's wording). Add a migration-Job policy when both
`migrations.job.enabled` and `networkPolicy.egress.enabled` hold: component `migrate`,
no ingress (`ingress: []`), egress DNS + `$postgresTo` + `$https`; extend the test above
with it.

- [ ] **Step 4: Run.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: all pass, and the default `PolicyMatrix` checks are untouched (Ingress only).

- [ ] **Step 5: Commit.**

```powershell
git add -A
git commit -m "feat(chart): add opt-in egress NetworkPolicies for external datastores and Entra"
```

### Task 7: The Azure guide

**Files:**
- Create: `docs\deployment-azure-entra.md`
- Modify: `docs\deployment-configuration.md`, `README.md`

- [ ] **Step 1: Write `docs\deployment-azure-entra.md`** with these sections (content, not
placeholders: each bullet below is a paragraph or table in the guide):

1. **What it does.** The services sign in to Azure Database for PostgreSQL flexible
   server and Azure Managed Redis with Microsoft Entra ID tokens from AKS workload
   identity; no datastore password exists anywhere. Application keys (signing, link,
   internal API tokens, Resend) still come from the Secret.
2. **Azure prerequisites (consumer-owned).** A table: PostgreSQL with Microsoft Entra
   authentication only and an Entra administrator; `azure.extensions` containing `CITEXT`
   (the auth and worker migrations create it; without it `migrate` stops with "the citext
   extension is not allow-listed"); Azure Managed Redis with access keys disabled,
   Microsoft Entra authentication on, the **Enterprise or non-clustered** policy (the hub
   uses classic `SUBSCRIBE`; the OSS cluster policy is unsupported) and TLS on port 10000;
   private endpoints and private DNS for both; an AKS cluster with the OIDC issuer and
   workload identity enabled; one user-assigned managed identity per service (auth-svc,
   message-svc, gateway, worker) plus one for migrations, each with a federated identity
   credential for subject `system:serviceaccount:<namespace>:<release>-<component>`
   (component `auth`, `message-svc`, `gateway`, `worker`, `migrate`) and audience
   `api://AzureADTokenExchange`; Redis access policy assignments for the identities of
   auth-svc, message-svc and the gateway, each assigned by its object id.
3. **Database roles.** SQL to run once as the Entra administrator, on the `postgres`
   database:

   ```sql
   SELECT * FROM pgaadauth_create_principal('push-migrator-id', false, false);
   SELECT * FROM pgaadauth_create_principal('push-auth-id', false, false);
   SELECT * FROM pgaadauth_create_principal('push-message-svc-id', false, false);
   SELECT * FROM pgaadauth_create_principal('push-worker-id', false, false);
   CREATE DATABASE push_auth OWNER "push-migrator-id";
   CREATE DATABASE push_messaging OWNER "push-migrator-id";
   CREATE DATABASE push_worker OWNER "push-migrator-id";
   ```

   Two models: with `migrations.job.enabled: true` (recommended) the migration identity
   owns the databases and grants each service `USAGE` on its schema and DML on its
   tables, sequences and functions, and the worker its narrow access to `auth`; the
   services run with `PUSH_RUN_MIGRATIONS=false` and no DDL rights. Without the Job,
   make each service's principal the owner of its database instead (and grant the
   worker's principal `CONNECT` on `push_auth`); the services migrate at start-up.
4. **Values.** A complete example (the Task 1 values with realistic placeholders such as
   `push-pg.postgres.database.azure.com`, `rediss://push-redis.westeurope.redis.azure.net:10000`,
   client ids, `migrations.job.enabled: true`, `networkPolicy.egress` with the private
   endpoint CIDRs and the Entra CIDRs).
5. **Egress.** NetworkPolicies match IP blocks, not names: list the private endpoint
   addresses; for `login.microsoftonline.com` either list Microsoft's published Entra
   ranges (service tag `AzureActiveDirectory`) in `httpsCidrs` or enforce FQDN egress in
   the cluster's firewall (Azure Firewall, a CNI with FQDN policies) and leave HTTPS
   open; the worker's Resend API needs HTTPS too. DNS must stay allowed.
6. **Variables reference.** The tables of the design spec §3.
7. **Verification on Azure (the consumer's acceptance).** Install; check that the
   Secret has no `*database*`, `redis*` or `*password*` key except the application ones;
   run `helm test` in a non-production namespace with dev mode or the consumer's own
   probes; keep traffic running for at least 3 hours (several token lifetimes) with
   `push-load` ([consumer verification](consumer-verification.md)) and check no failed
   requests, no dropped WebSocket sessions and no lost messages; restart one pod after
   70 minutes and see it connect; restart Redis (or fail it over) and see the gateways
   resubscribe. Expected log lines: `refreshed an access token` (debug),
   `replaced the Redis subscriber for a new token` (info).
8. **Limitations.** Non-clustered Redis only; no automatic linking of datastores to
   identities (Azure-side setup is the consumer's); tokens are cached in memory only;
   tokens of the bundled datastores are not supported (they take passwords).

- [ ] **Step 2: Links.** In `docs\deployment-configuration.md`, after "Consumer-managed
Secret", add "### Managed datastores with Microsoft Entra ID" with two sentences pointing
to the new guide and listing which Secret keys are then unused (all database and Redis
keys). Replace the README sentence added by Plan 12A ("the chart values and guide arrive
with Plan 12B") with a link to the guide.

- [ ] **Step 3: Commit.**

```powershell
git add -A
git commit -m "docs: add the Azure Entra ID datastore deployment guide"
```

### Task 8: A DEV ONLY in-cluster token issuer

The rehearsal needs Plan 12A's stub issuer inside the cluster, serving HTTPS (the
services accept `http://` authorities only on loopback), and a test CA for the TLS
datastores. A service image must never contain it; it goes into the DEV ONLY smoke image.

**Files:**
- Modify: `crates\push-credentials\Cargo.toml`, `src\lib.rs`, `src\testing.rs`
- Create: `crates\push-smoke\src\bin\push-dev-entra.rs`
- Modify: `crates\push-smoke\Cargo.toml`, `deploy\docker\Dockerfile`

- [ ] **Step 1: Split the feature.** testcontainers needs Rust 1.88 and the MSRV job
builds every binary on 1.85, so a binary may only use the issuer part:

```toml
[features]
# The stub token issuer, the test CA and the password helpers: DEV ONLY tools and tests.
stub = ["dep:axum", "dep:rcgen", "dep:uuid", "dep:tokio-rustls", "dep:sqlx", "dep:hyper", "dep:hyper-util", "dep:tower"]
# Plus TLS containers: tests only (testcontainers needs Rust 1.88).
testing = ["stub", "dep:testcontainers-modules"]
```

(`tokio-rustls = { version = "0.26", default-features = false, features = ["ring"], optional = true }`
in `[dependencies]`, the same version redis already pulls.) Split `testing.rs` in two:
`stub.rs` (`StubIssuer`, `StubPrincipal`, `Issued`, `IssueHook`, `TestCa`,
`serve_tls`, `set_postgres_password`, `set_redis_password`), declared in `lib.rs` as
`#[cfg(any(test, feature = "stub"))] pub mod stub;`, and `testing.rs`
(the containers, `set_postgres_password`, `set_redis_password`, re-exporting `stub::*`)
under `#[cfg(any(test, feature = "testing"))]`. Existing test code keeps importing
`push_credentials::testing::...`.

- [ ] **Step 2: Extend the stub.** In `stub.rs`:

1. `StubIssuer::new(tenant, assertion: Option<&str>, principals, lifetime, hook) -> (Self, Router)`
   builds the state and router without listening (`None` accepts any non-empty
   assertion: in a cluster it is a real, rotating service-account token);
   `StubIssuer::start` becomes `new` plus the existing plain-HTTP listener on 127.0.0.1.
   `authority` is set by whoever serves it: add `pub fn with_authority(mut self, authority: String) -> Self`.
2. `TestCa::server_for(names: &[&str]) -> (String, String)` (the current `server()`
   calls it with `["127.0.0.1", "localhost"]`), and `TestCa::write(dir)` writing
   `ca.crt`.
3. TLS serving for any router:

```rust
/// Serves `router` over TLS with `cert_pem` and `key_pem` on `listener`, until the
/// process ends. Failed handshakes are dropped.
pub async fn serve_tls(listener: tokio::net::TcpListener, router: Router, cert_pem: &str, key_pem: &str) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .expect("a PEM certificate");
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).expect("a PEM key");
    let config = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(certs, key).expect("a valid key pair");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    loop {
        let Ok((tcp, _)) = listener.accept().await else { continue };
        let (acceptor, router) = (acceptor.clone(), router.clone());
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(tcp).await else { return };
            let service = hyper::service::service_fn(move |request| tower::ServiceExt::oneshot(router.clone(), request));
            let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                .await;
        });
    }
}
```

(add `hyper`, `hyper-util` with `server-auto`/`tokio`, and `tower` to the `stub` feature;
they are already in the tree through axum and tonic: use the locked versions. If
`PemObject` is missing in the locked `rustls-pki-types`, use `rustls-pemfile`'s
`certs` and `private_key` instead.)

Add a unit test in `src\tests\mod.rs`: serve the issuer router over TLS with a test CA,
build `WorkloadIdentity` with a `reqwest` client trusting the CA
(`reqwest::Certificate::from_pem`, `add_root_certificate`), and get a token from
`https://localhost:<port>/`.

- [ ] **Step 3: The tool** `crates\push-smoke\src\bin\push-dev-entra.rs` (add
`push-credentials = { workspace = true, features = ["stub"] }`, `sqlx` and `redis` to
push-smoke's `[dependencies]` if missing):

```rust
//! DEV ONLY: tools for the Entra ID rehearsal on a local cluster (Plan 12B). Never in a
//! service image and never against real datastores.
//!
//!   push-dev-entra certs <dir> <dns name>...   a test CA and one server certificate
//!   push-dev-entra issuer                      an HTTPS stub of Microsoft Entra ID
//!
//! The issuer reads PUSH_DEV_ENTRA_TLS_DIR (server.crt, server.key), PUSH_DEV_ENTRA_TENANT,
//! PUSH_DEV_ENTRA_PRINCIPALS ("client:role:oid,..."), PUSH_DEV_ENTRA_LIFETIME_S,
//! PUSH_DEV_ENTRA_POSTGRES (an admin URL, sslmode=verify-full, sslrootcert),
//! PUSH_DEV_ENTRA_REDIS (an admin rediss:// URL) and PUSH_DEV_ENTRA_CA (the CA file), and
//! makes every token it issues the principal's PostgreSQL or Redis password before
//! answering, as Azure accepts its tokens. It logs one line per token without the token.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use push_credentials::stub::{IssueHook, Issued, StubIssuer, StubPrincipal, TestCa, serve_tls};

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    push_credentials::install_crypto_provider();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("certs") if args.len() >= 3 => {
            let dir = std::path::Path::new(&args[1]);
            std::fs::create_dir_all(dir)?;
            let ca = TestCa::new();
            let names: Vec<&str> = args[2..].iter().map(String::as_str).collect();
            let (cert, key) = ca.server_for(&names);
            std::fs::write(dir.join("ca.crt"), ca.pem())?;
            std::fs::write(dir.join("server.crt"), cert)?;
            std::fs::write(dir.join("server.key"), key)?;
            println!("wrote ca.crt, server.crt and server.key to {}", dir.display());
            Ok(())
        }
        Some("issuer") => issuer().await,
        _ => anyhow::bail!("usage: push-dev-entra certs <dir> <dns name>... | push-dev-entra issuer"),
    }
}

async fn issuer() -> anyhow::Result<()> {
    let principals: Vec<(String, String, String)> = var("PUSH_DEV_ENTRA_PRINCIPALS")
        .split(',')
        .map(|entry| {
            let parts: Vec<&str> = entry.split(':').collect();
            (parts[0].to_owned(), parts[1].to_owned(), parts[2].to_owned())
        })
        .collect();
    let ca = var("PUSH_DEV_ENTRA_CA");
    let postgres = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&var("PUSH_DEV_ENTRA_POSTGRES"))
        .await?;
    let certificates = redis::TlsCertificates { client_tls: None, root_cert: Some(std::fs::read(&ca)?) };
    let redis = redis::Client::build_with_tls(var("PUSH_DEV_ENTRA_REDIS"), certificates)?
        .get_multiplexed_async_connection()
        .await?;
    let roles: Arc<Vec<(String, String, String)>> = Arc::new(principals.clone());
    let hook: IssueHook = Arc::new(move |issued: Issued| {
        let (postgres, mut redis, roles) = (postgres.clone(), redis.clone(), Arc::clone(&roles));
        Box::pin(async move {
            let role = roles.iter().find(|(client, _, _)| *client == issued.client_id).map(|(_, role, _)| role.clone());
            if issued.scope.starts_with("https://ossrdbms-aad") {
                if let Some(role) = role {
                    push_credentials::stub::set_postgres_password(&postgres, &role, &issued.token).await;
                }
            } else {
                push_credentials::stub::set_redis_password(&mut redis, &issued.object_id, &issued.token).await;
            }
            println!("issued a token for {} ({})", issued.client_id, issued.scope);
        }) as BoxFuture<'static, ()>
    });
    let lifetime = Duration::from_secs(var("PUSH_DEV_ENTRA_LIFETIME_S").parse()?);
    let stub_principals = principals
        .iter()
        .map(|(client, _, oid)| StubPrincipal { client_id: client.clone(), object_id: oid.clone() })
        .collect();
    let (_issuer, router) = StubIssuer::new(&var("PUSH_DEV_ENTRA_TENANT"), None, stub_principals, lifetime, Some(hook));
    let dir = std::path::PathBuf::from(var("PUSH_DEV_ENTRA_TLS_DIR"));
    let cert = std::fs::read_to_string(dir.join("server.crt"))?;
    let key = std::fs::read_to_string(dir.join("server.key"))?;
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8443").await?;
    println!("serving the DEV ONLY Entra stub on :8443");
    serve_tls(listener, router, &cert, &key).await;
    Ok(())
}
```

`set_postgres_password` and `set_redis_password` move into `stub.rs` (they need only
sqlx and redis, which `stub` then also enables: `"dep:sqlx"` joins `stub`; sqlx builds on
1.85). Only the container helpers stay in `testing.rs`.

- [ ] **Step 4: The image.** In `deploy\docker\Dockerfile`, build and copy
`push-dev-entra` into the DEV ONLY `smoke` target only:

```dockerfile
        target/release/push-load target/release/push-dev-entra /out/
...
FROM runtime AS smoke
COPY --from=builder /out/push-smoke /usr/local/bin/push-smoke
COPY --from=builder /out/push-dev-entra /usr/local/bin/push-dev-entra
```

(`-p push-smoke` already builds every binary of the crate.)

- [ ] **Step 5: Run.**

Run: `cargo test -p push-credentials --locked; cargo +1.85 build --workspace --locked; cargo run -p push-smoke --bin push-dev-entra -- certs $env:TEMP\entra-certs push-ds-postgres.push-ds.svc`
Expected: tests pass; the MSRV build passes; three PEM files are written.

- [ ] **Step 6: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "test: add a DEV ONLY in-cluster Entra stub for the chart rehearsal"
```

### Task 9: The minikube rehearsal

Proves the chart in Entra mode end to end without Azure: TLS PostgreSQL and Redis that
take only tokens, the stub issuer over HTTPS, the chart's own token projection
(`injection: chart`), the migration Job, the services as non-owners, egress policies on
Calico, the smoke test before and after many rotations and after killed Redis
connections, and no datastore credential in the release.

**Files:**
- Create: `deploy\minikube\entra\datastores.yaml`, `deploy\minikube\values-entra-rehearsal.yaml`, `deploy\minikube\entra-rehearsal.ps1`
- Modify: `deploy\minikube\up.ps1` (switch `-SkipInstall`)

- [ ] **Step 1: `up.ps1 -SkipInstall`.** Add `[switch] $SkipInstall` and return after the
image build when it is set (`if ($SkipInstall) { Write-Host 'Images built; not installing (-SkipInstall).'; return }`).

- [ ] **Step 2: `entra\datastores.yaml`** in namespace `push-ds` (DEV ONLY): a Secret
`push-ds-tls` (created by the script from the certs), a Secret `push-ds-admin` with a
generated admin password (created by the script), and:

- A PostgreSQL 17 StatefulSet (the same digest-pinned image as the chart) started like
  Plan 12A's `tls_postgres` (`install` the key as `postgres` 0600, `ssl=on`), with an
  init script creating `push-migrator-id`, `push-auth-id`, `push-message-svc-id` and
  `push-worker-id` (LOGIN, no password) and the three databases owned by
  `push-migrator-id`, and `pg_hba` requiring `hostssl ... scram-sha-256` for TCP. Service
  `push-ds-postgres:5432`.
- Redis 7 (digest-pinned) with `--port 0 --tls-port 6380 --tls-auth-clients no` and the
  `default` user password-protected with the admin password (`--requirepass`). Service
  `push-ds-redis:6380`.
- The issuer: Deployment of `push-messenger/smoke:dev` with
  `command: ["/usr/local/bin/push-dev-entra", "issuer"]`, the TLS Secret mounted, env
  `PUSH_DEV_ENTRA_*` (tenant `11111111-2222-3333-4444-555555555555`, principals
  `auth:push-auth-id:<oid>,message-svc:push-message-svc-id:<oid>,worker:push-worker-id:<oid>,gateway:push-gateway-id:<oid>,migrate:push-migrator-id:<oid>`
  with fixed UUIDs as object ids and the same UUIDs as client ids, lifetime `60`, admin
  URLs from `push-ds-admin`). Service `push-ds-issuer:443 -> 8443`.
- Security contexts as the chart's (non-root, read-only root, no capabilities) except
  PostgreSQL, which uses the chart's PostgreSQL context (Plan F11).

- [ ] **Step 3: `values-entra-rehearsal.yaml`** (layered after `values-minikube.yaml`):

```yaml
# DEV ONLY rehearsal of the Entra ID datastore mode on minikube (Plan 12B).
postgres:
  enabled: false
  external:
    host: push-ds-postgres.push-ds.svc
    auth: azure-entra
    users:
      messageSvc: push-message-svc-id
      auth: push-auth-id
      worker: push-worker-id
      migrate: push-migrator-id
    caConfigMap: push-ds-ca
redis:
  enabled: false
  external:
    url: rediss://push-ds-redis.push-ds.svc:6380
    auth: azure-entra
    caConfigMap: push-ds-ca
azure:
  workloadIdentity:
    injection: chart
    tenantId: 11111111-2222-3333-4444-555555555555
    authorityHost: https://push-ds-issuer.push-ds.svc/
    authorityCaConfigMap: push-ds-ca
    clientIds:
      auth: 0a000000-0000-4000-8000-000000000001
      messageSvc: 0a000000-0000-4000-8000-000000000002
      gateway: 0a000000-0000-4000-8000-000000000003
      worker: 0a000000-0000-4000-8000-000000000004
      migrate: 0a000000-0000-4000-8000-000000000005
migrations:
  job:
    enabled: true
networkPolicy:
  egress:
    enabled: true
    extra:
      - to:
          - namespaceSelector:
              matchLabels:
                kubernetes.io/metadata.name: push-ds
```

(the `extra` rule stands in for private-endpoint CIDRs: in-cluster Services are reached
through pod IPs, which ipBlocks do not describe).

- [ ] **Step 4: `entra-rehearsal.ps1`** with parameters `-Cluster` (default
`push12b-entra`; refuses `push`) and `-SkipBuild`. It runs, each step through the
`Invoke-Step` pattern of `up.ps1`, and restores the kubectl context to `push` in a
`finally`:

1. `minikube start -p $Cluster --driver=docker --cni=calico --cpus=4 --memory=6g` (if
   not running) and `minikube addons enable ingress`.
2. `deploy/minikube/up.ps1 -Cluster $Cluster -SkipInstall` unless `-SkipBuild`.
3. `cargo run -p push-smoke --bin push-dev-entra -- certs <temp dir> push-ds-postgres.push-ds.svc push-ds-redis.push-ds.svc push-ds-issuer.push-ds.svc`;
   `kubectl create namespace push-ds` and `push` (idempotent with `--dry-run=client -o yaml | kubectl apply -f -`);
   Secret `push-ds-tls` (server cert and key) and `push-ds-admin` (a generated
   alphanumeric password) in `push-ds`; ConfigMap `push-ds-ca` (`ca.crt`) in `push`. The
   temp directory is deleted at the end; the files never enter the repository.
4. `kubectl apply -f deploy/minikube/entra/datastores.yaml`; wait for the three
   workloads to be ready.
5. `helm upgrade --install push deploy/helm/push-messenger -n push -f values-minikube.yaml -f values-entra-rehearsal.yaml --set buildId=<timestamp> --wait --timeout 10m`.
   The migration Job runs first (hook); the services start with `PUSH_RUN_MIGRATIONS=false`.
6. `helm test push -n push --logs` (the smoke test, including its SDK half).
7. Assertions, failing the script on a mismatch:
   - `kubectl get secret push-secrets -n push -o json`: no key containing `database`,
     `redis` or `password`.
   - `kubectl get pods -n push -o json`: no env var whose name contains `PASSWORD` or
     equals `PUSH_DATABASE_URL`, `PUSH_REDIS_URL` with credentials (`@`).
   - The issuer's log shows tokens for every client id.
8. Wait 300 s (five token lifetimes of 60 s) while running
   `push-load` from the load image against the ingress (2 accounts made by
   `push-dev-accounts`, 4 connections, `--duration-seconds 280`, `--no-reconnect`), and
   require its report `status: passed`.
9. Kill the services' Redis connections:
   `kubectl exec -n push-ds deploy/push-ds-redis -- redis-cli --tls --cacert /tls/ca.crt -p 6380 -a <admin> CLIENT KILL USER <gateway oid>`
   (and the other two oids); then `helm test` again.
10. Restart every application pod (`kubectl rollout restart` of the four Deployments),
    wait, and `helm test` once more: new PostgreSQL and Redis connections on fresh
    tokens, after many rotations.
11. Print a summary; leave the profile running only with `-Keep`, otherwise
    `minikube stop -p $Cluster` (data kept).

- [ ] **Step 5: Run it.**

Run: `pwsh deploy/minikube/entra-rehearsal.ps1`
Expected: every step passes; the context is `push` afterwards; the shared `push` profile
is untouched. Record the run's summary (counts, durations, no secrets) in the PR body.

- [ ] **Step 6: Commit.**

```powershell
git add -A
git commit -m "test(minikube): rehearse the Entra ID datastore mode without Azure"
```

### Task 10: Egress enforcement on Calico

**Files:**
- Modify: `deploy\minikube\test-network-policy.ps1`

- [ ] **Step 1: An `-Egress` switch.** With it, after the existing ingress matrix, the
script probes egress from disposable probe pods labelled as each peer (the existing
probe-pod mechanism), against the rehearsal's targets, expecting:

| From (peer label) | To | Expect |
|---|---|---|
| gateway | `push-auth:8080`, `push-message-svc:50051` | allow |
| gateway | `push-message-svc:50052`, `push-message-svc:50053` | deny |
| worker | `push-message-svc:50052` | allow |
| worker | `push-auth:8080` | deny |
| auth | `push-message-svc:50053` | allow |
| message-svc | `push-auth:8080` | deny |
| any peer | `push-ds-postgres.push-ds.svc:5432`, `push-ds-redis.push-ds.svc:6380`, `push-ds-issuer.push-ds.svc:443` | allow (the rehearsal's `extra` rule) |
| any peer | `1.1.1.1:443` | deny |

and DNS resolution works from every probe (positive control first, as the ingress checks
do: a refusal or a failed exec is an infrastructure failure, never a pass).

- [ ] **Step 2: Run it** against the rehearsal (`-Context push12b-entra -Egress`, during
Task 9 with `-Keep`).
Expected: all checks pass. Restore the context to `push`; stop the profile.

- [ ] **Step 3: Commit.**

```powershell
git add -A
git commit -m "test(minikube): prove the egress NetworkPolicies on Calico"
```

### Task 11: Verification and delivery

- [ ] **Step 1: Full checks.**

```powershell
$env:CARGO_INCREMENTAL = '0'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 build --workspace --locked
pwsh deploy/helm/test-chart.ps1
helm template push deploy/helm/push-messenger | Out-File $env:TEMP\push-default-after.yaml
git diff --no-index --stat $env:TEMP\push-default-before.yaml $env:TEMP\push-default-after.yaml
```

Expected: everything passes and the default render is identical (no diff).

- [ ] **Step 2: Password mode on minikube.** `deploy/minikube/up.ps1 -Cluster push11b-local -Smoke`
on the existing volume: the upgrade keeps the data and the smoke test passes. Stop the
profile; restore the context to `push`.

- [ ] **Step 3: Status.** Roadmap row 12B `Done (#N)`, 12A already done; the spec's status
line names both; the README and `deployment-configuration.md` link the guide.

- [ ] **Step 4: Review and deliver.** Ask for a code review of `git diff main...HEAD`,
fix findings with tests, then push, open the PR, wait for CI, squash-merge with
`Entra ID datastores in the chart (Plan 12B) (#N)`, switch to `main`, pull, delete the
branch.

## Self-review against the spec

| Spec item | Task |
|---|---|
| External datastores, bundled ones unchanged | 1, 2, 11 (default render identical) |
| Per-service ServiceAccounts, workload-identity annotations and label, distinct identities | 4 |
| No datastore Secret in Entra mode | 2, 3, 9 (live check) |
| Optional migration Job with its own identity; services without DDL rights | 5, 9 |
| Egress NetworkPolicies (datastore endpoints, Entra) | 6, 10 |
| Existing security contexts kept | 4, 5 (`push.containerSecurity`), 9 |
| D10 token projection by webhook or chart | 4, 9 |
| CA bundles | 4, 9 |
| Guide: variables, Azure prerequisites, roles, citext, non-clustered Redis, limitations | 7 |
| AC2, AC3, AC5 in a cluster | 9 |
| Rotations in a cluster (AC4 locally; 3 h on Azure is the consumer's) | 9, 7 |
