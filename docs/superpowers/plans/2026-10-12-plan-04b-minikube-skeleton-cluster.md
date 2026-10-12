# Plan 4B — Walking Skeleton on Minikube Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run the walking skeleton of Plan 4A on a local minikube cluster with the Helm chart that will later deploy to Azure, and prove it with the end-to-end smoke test going through ingress-nginx. Deliverables: container images for message-svc, the gateway and the smoke test; the chart `deploy/helm/push-messenger` (gateway, message-svc, PostgreSQL, Redis with AUTH, generated secrets, probes, resources, an Ingress with WebSocket timeouts, the smoke test as a `helm test`); `values-minikube.yaml`, the only place the DEV ONLY identity is switched on, with chart guards that refuse it anywhere else; `deploy/minikube/up.ps1` / `up.sh` and `down.ps1` / `down.sh`; CI that builds the images and checks the chart. From this plan on, every plan is verified on minikube before its pull request is merged.

**Architecture:** One Dockerfile with a shared Rust build stage and one distroless, non-root target per binary. `up.ps1` starts the minikube profile `push` (docker driver), enables the ingress addon, builds the images straight into the cluster's Docker daemon (`minikube docker-env`), and installs the chart into namespace `push` with `--wait`. Inside the cluster: Postgres 17 (StatefulSet, volume, database `push_messaging` owned by user `push`), Redis 7 (`--requirepass`, no persistence), message-svc (gRPC 50051, health 8081) and the gateway (HTTP 8080, health 8081) behind an Ingress for host `push.local`, path `/v1/socket`. `helm test` runs `push-smoke` in a Pod: the DEV ONLY setup calls go to message-svc's ClusterIP Service, the WebSockets to the ingress-nginx controller Service with `Host: push.local`, the same path a client takes. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §2, §7.4–§7.5, §11; roadmap section "Running locally on minikube".

**Tech Stack:** Docker (BuildKit), `rust:1.85-slim-bookworm` builder, `gcr.io/distroless/cc-debian12:nonroot` runtime, minikube 1.34+ (Kubernetes 1.31, docker driver, ingress addon = ingress-nginx 1.11), kubectl, Helm 3 (3.16 here), `postgres:17-alpine`, `redis:7-alpine`, PowerShell 7 (`pwsh`) and bash for the scripts.

---

## Before you start

- **Plan 4A is merged.** This plan packages and deploys the binaries Plan 4A built; it
  changes no Rust code.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-04b-skeleton-minikube
```

- Fix a stale PATH for the current PowerShell session (each new shell needs it again):

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
```

- **Docker Desktop must be running** with at least 6 CPUs and 8 GB of memory available
  (the minikube node gets 4 CPUs and 6 GB). Check the tools:

```powershell
docker version --format '{{.Server.Version}}'
minikube version
kubectl version --client
helm version
```

  On this machine: Docker 29.7, minikube 1.34.0, kubectl 1.36, Helm 3.16.3. Helm may print
  `failed to load plugins … helm-diff … unknown field "platformHooks"` first: an outdated
  plugin, harmless.
- **minikube changes the current kubectl context** to the profile it starts. The scripts
  always pass `--context` / `--kube-context`, so they never act on another cluster. After
  `down.ps1 -Delete` the current context is unset; switch back with
  `kubectl config use-context <the one you had>`.
- **Edit files with the edit tool, and keep them LF** (`.gitattributes`:
  `* text=auto eol=lf`); git normalises CRLF from the file-creation tool on commit. This
  matters for `up.sh` and `down.sh`, which bash refuses to run with CRLF on Linux.
- **Do not pipe `cargo fmt`**; run `cargo fmt --all -- --check` on its own.
- If PowerShell refuses to run the `.ps1` scripts (execution policy), run them as
  `pwsh -NoProfile -File deploy/minikube/up.ps1 -Smoke` instead.
- Baseline (after Plan 4A): `cargo test --workspace` passes 604 tests. This plan adds no
  Rust tests; its checks are `deploy/helm/test-chart.ps1` (12 checks) and the smoke test
  on minikube.
- **Every task ends green:**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

  plus the task's own checks (image builds, `helm lint`, `test-chart.ps1`, minikube).
- **Scope rule:** no Rust changes; no registry pushes (images stay local; GHCR comes with
  the Azure plan); no TLS or cert-manager; no Key Vault.

## Key rules the configuration must preserve

- **No secret in git or in a ConfigMap.** Passwords come from values or are generated at
  install time, live only in the release's Secret, and reach the services as environment
  variables from `secretKeyRef`.
- **The DEV ONLY identity cannot reach a real deployment by accident.** `values.yaml`
  keeps it off; switching it on needs `devMode.enabled: true` *and*
  `devMode.acknowledge: insecure-local-only` *and* a local ingress host, or rendering fails.
- **Our containers run as a non-root user** (uid 65532) with a read-only root filesystem,
  no privilege escalation, all capabilities dropped and no service-account token; the
  images have no shell.
- **Probes match Plan 4A:** liveness `/healthz`, readiness `/readyz`, port 8081 of each
  service; nginx's read and send timeouts are 120 s (spec §7.5).
- **The gateway Service is reached only through the Ingress**, and only `/v1/socket`;
  message-svc and the stores have ClusterIP Services only.

## Decisions made for this plan

These were settled while writing the plan. Do not reopen them during execution.

1. **One Dockerfile, one target per binary** (`message-svc`, `gateway`, `smoke`), sharing
   one builder stage, so the second and third image reuse the first one's compile instead
   of building the workspace three times. The builder is `rust:1.85-slim-bookworm` — the
   MSRV, which `Cargo.lock` is resolved for — and builds `--release --locked` with BuildKit
   cache mounts for the cargo registry and `target`. The runtime is
   `gcr.io/distroless/cc-debian12:nonroot` (glibc and libgcc, no shell). Images are about
   30 MB.
2. **Images are built straight into minikube's Docker daemon** with `minikube docker-env`
   and the host's `docker build` (BuildKit, so the cache mounts work, and the layer cache
   stays in the node between runs), tagged `push-messenger/<name>:dev`, pulled
   `IfNotPresent`. The script passes the image ids as the chart value `buildId`, a pod
   annotation: a rebuilt image rolls the pods, an unchanged one does not.
3. **The chart is self-contained** for local use: Postgres 17 as a StatefulSet with a
   1 Gi volume (one database per service: `push_messaging`, owned by `push`, the user
   message-svc migrates as), Redis 7 as a Deployment with `--requirepass` from the Secret
   and no persistence (a doorbell keeps nothing). Passwords are 32 random letters and
   digits, generated on install and kept on upgrades by reading the existing Secret back
   (`lookup`); the Secret also holds the two connection URLs. The Azure plan may swap the
   stores for managed ones and the Secret for Key Vault.
4. **Ingress:** class `nginx`, host from values (`push.local` on minikube), path
   `/v1/socket` (`Exact`). ingress-nginx passes the WebSocket upgrade through by itself;
   the annotations set `proxy-read-timeout` and `proxy-send-timeout` to 120 s and
   `proxy-http-version` to 1.1.
5. **DEV ONLY safety in the chart.** One switch, `devMode.enabled`, turns on
   `PUSH_DEV_AUTH=1` (gateway), `PUSH_DEV_SETUP=1` (message-svc) and the smoke test; it is
   `false` in `values.yaml` and set only in `values-minikube.yaml`. The `push.devMode`
   helper fails rendering unless `devMode.acknowledge` is exactly `insecure-local-only`
   and the ingress host ends in `.local`, `.localhost` or `.test`. With Plan 4A's strict
   parsing and start-up warnings in the binaries, enabling it takes three deliberate
   values in one file and still cannot face a public host. `test-chart.ps1` checks the
   defaults, the minikube values and both refusals, in CI too.
6. **The smoke test reaches the cluster as a `helm test` Pod, inside it.** The dev setup
   calls go to message-svc's ClusterIP Service, so the DEV ONLY API is never exposed
   outside the cluster; the WebSockets go to the ingress-nginx controller Service
   (`ingress-nginx-controller.ingress-nginx.svc.cluster.local`) with `Host: push.local`,
   through nginx exactly like a client's. This needs no port-forwards (fragile on
   Windows, and they must be cleaned up), no hosts-file edit and no admin
   `minikube tunnel`, and works the same on Windows, Linux and CI. `helm test --logs`
   prints the smoke output. For debugging from the host, Task 4 also runs the binary
   through two port-forwards.
7. **Bring-up:** `up.ps1` (the owner's platform) and `up.sh` (Linux, macOS, CI) do the
   same: start the profile (`push`, 4 CPUs, 6 GB, docker driver) unless its node, kubelet
   and API server already all run — `minikube start` is idempotent, but on a running
   profile it restarts every container in the cluster (seen in verification: every pod's
   restart count went up on each run) — enable the ingress addon, wait for its controller
   *and* for its admission-webhook jobs (`ingress-nginx-admission-create` / `-patch`) to
   complete, since installing an Ingress before them fails; build; `helm upgrade
   --install … --wait`; print how to reach the gateway; and with `-Smoke` / `--smoke` run
   `helm test`. Every native call is checked: `up.ps1` throws on any non-zero
   `$LASTEXITCODE` (minikube, kubectl, docker, helm, including `docker-env`); `up.sh` runs
   with `set -euo pipefail` and builds in a subshell, **not** inside a command
   substitution, where bash disables `errexit` and a failed build would deploy stale
   `:dev` images. `down.ps1` / `down.sh` uninstall the release and delete the namespace
   (and its volume); `-Delete` / `--delete` delete the profile.
8. **CI** builds the three images (no push) and runs `test-chart.ps1` (Helm 3.16.3,
   pinned through `azure/setup-helm`; `pwsh` is on the runner). It runs no cluster: the minikube smoke
   test is the merge gate, run by the implementer and recorded in the pull request
   (owner requirement). A kind or minikube job in CI is deferred.

## File structure

```
.dockerignore                         NEW: the build context is the repository root
deploy/docker/Dockerfile              NEW: builder + message-svc, gateway, smoke targets
deploy/helm/test-chart.ps1            NEW: lint and render checks, no cluster needed
deploy/helm/push-messenger/           NEW
├── Chart.yaml
├── values.yaml                       safe defaults: dev mode off
├── values-minikube.yaml              dev mode on, host push.local
└── templates/
    ├── _helpers.tpl                  names, labels, the dev-mode guard, security context
    ├── secret.yaml                   generated passwords and connection URLs
    ├── postgres.yaml                 Service + StatefulSet
    ├── redis.yaml                    Service + Deployment
    ├── message-svc.yaml              Service + Deployment
    ├── gateway.yaml                  Service + Deployment
    ├── ingress.yaml                  /v1/socket with WebSocket timeouts
    ├── NOTES.txt
    └── tests/smoke.yaml              the smoke test as a helm test Pod
deploy/minikube/                      NEW
├── up.ps1, down.ps1                  Windows / PowerShell
└── up.sh, down.sh                    Linux, macOS, CI (executable)
.github/workflows/ci.yml              images and chart jobs
README.md                             running on minikube
docs/superpowers/specs/2026-10-11-push-messenger-design.md   §11 implementation notes
docs/superpowers/plans/2026-10-11-roadmap.md                 4B in review
```

---

### Task 1: The container images

**Files:**
- Create: `deploy/docker/Dockerfile`, `.dockerignore`

- [ ] **Step 1: The Dockerfile**

```powershell
New-Item -ItemType Directory deploy\docker -Force
```

Create `deploy/docker/Dockerfile`:

```dockerfile
# Container images of push-messenger: one target per binary, all built from one shared
# builder stage, so building the second and third image reuses the first one's build.
#
#   docker build -f deploy/docker/Dockerfile --target message-svc -t push-messenger/message-svc:dev .
#   docker build -f deploy/docker/Dockerfile --target gateway -t push-messenger/gateway:dev .
#   docker build -f deploy/docker/Dockerfile --target smoke -t push-messenger/smoke:dev .
#
# The context is the repository root. Needs BuildKit (the default for `docker build`
# since Docker 23) for the cache mounts.

FROM rust:1.85-slim-bookworm AS builder
WORKDIR /src
COPY . .
# The registry and target caches survive between builds on the same Docker daemon; the
# binaries are copied out of the cache into the image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
        -p push-message-svc -p push-gateway -p push-smoke \
    && mkdir /out \
    && cp target/release/push-message-svc target/release/push-gateway \
        target/release/push-smoke /out/

# glibc and libgcc, no shell, no package manager; runs as uid 65532 ("nonroot").
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime

FROM runtime AS message-svc
COPY --from=builder /out/push-message-svc /usr/local/bin/push-message-svc
EXPOSE 50051 8081
ENTRYPOINT ["/usr/local/bin/push-message-svc"]

FROM runtime AS gateway
COPY --from=builder /out/push-gateway /usr/local/bin/push-gateway
EXPOSE 8080 8081
ENTRYPOINT ["/usr/local/bin/push-gateway"]

FROM runtime AS smoke
COPY --from=builder /out/push-smoke /usr/local/bin/push-smoke
ENTRYPOINT ["/usr/local/bin/push-smoke"]
```

- [ ] **Step 2: Keep the build context small**

Create `.dockerignore` at the repository root:

```gitignore
# The Docker build context is the repository root; send only what the build needs.
.git
.github
.superpowers
deploy
docs
target
**/*.md
```

Excluding `deploy` and `docs` also means that editing the chart or the docs never
invalidates the image build cache.

- [ ] **Step 3: Build the three images with Docker Desktop**

```powershell
foreach ($target in 'message-svc', 'gateway', 'smoke') {
    docker build -f deploy/docker/Dockerfile --target $target -t "push-messenger/${target}:dev" .
    if ($LASTEXITCODE -ne 0) { throw "building $target failed" }
}
docker images --format '{{.Repository}}:{{.Tag}} {{.Size}}' | Select-String push-messenger
```

Expected: the first build compiles the workspace in release mode (about 1–4 minutes
depending on the machine; `Finished release profile`), the other two reuse it and take
seconds. The three images are about 28–34 MB.

- [ ] **Step 4: Check what the images do**

```powershell
docker run --rm push-messenger/gateway:dev; "exit=$LASTEXITCODE"
docker run --rm push-messenger/smoke:dev; "exit=$LASTEXITCODE"
docker run --rm --entrypoint id push-messenger/gateway:dev; "exit=$LASTEXITCODE"
```

Expected: the gateway prints `Error: PUSH_MESSAGE_SVC_URL must be set` and exits 1 (its
configuration is missing, as intended); the smoke test prints
`--setup is required; usage: push-smoke --setup <grpc-url> --socket <ws-url> [--host <host>]`
and exits 2; the third command fails with `exec: "id": executable file not found`: the
image has no tools and no shell.

- [ ] **Step 5: Remove the local images**

```powershell
docker rmi push-messenger/message-svc:dev push-messenger/gateway:dev push-messenger/smoke:dev
```

(Task 4 builds them inside minikube, where the pods need them.)

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add .dockerignore deploy/docker
git commit -m "build: container images for message-svc, the gateway and the smoke test"
```

Expected: 604 tests pass, no warnings, no diff.

---

### Task 2: The chart — secrets, PostgreSQL and Redis

**Files:**
- Create: `deploy/helm/push-messenger/Chart.yaml`, `values.yaml`, `values-minikube.yaml`,
  `templates/_helpers.tpl`, `templates/secret.yaml`, `templates/postgres.yaml`,
  `templates/redis.yaml`

- [ ] **Step 1: Chart and values**

```powershell
New-Item -ItemType Directory deploy\helm\push-messenger\templates\tests -Force
```

Create `deploy/helm/push-messenger/Chart.yaml`:

```yaml
apiVersion: v2
name: push-messenger
description: push-messenger on Kubernetes - the gateway, message-svc, PostgreSQL and Redis.
type: application
version: 0.1.0
appVersion: "0.1.0"
```

Create `deploy/helm/push-messenger/values.yaml`:

```yaml
# Safe defaults. Authentication cannot be switched off from here: devMode stays off
# unless a values file sets both `devMode.enabled` and `devMode.acknowledge`, and the
# chart refuses dev mode for an ingress host that is not local (see _helpers.tpl).

images:
  messageSvc:
    repository: push-messenger/message-svc
    tag: dev
  gateway:
    repository: push-messenger/gateway
    tag: dev
  smoke:
    repository: push-messenger/smoke
    tag: dev
  pullPolicy: IfNotPresent

# Changing this rolls out new gateway and message-svc pods even when the image tags stay
# the same (the bring-up script sets it after rebuilding the images).
buildId: ""

# DEV ONLY switches: the gateway accepts identities without a token (PUSH_DEV_AUTH=1),
# message-svc serves the DevSetup API (PUSH_DEV_SETUP=1), and `helm test` runs the
# smoke test. Never enable outside a local cluster.
devMode:
  enabled: false
  # Must be exactly "insecure-local-only" when enabled.
  acknowledge: ""

gateway:
  replicas: 1
  # Client ping interval announced in Ready, in milliseconds.
  heartbeatMs: 30000
  # Longer than the gateway's own 10 s drain.
  terminationGracePeriodSeconds: 30
  resources:
    requests:
      cpu: 50m
      memory: 64Mi
    limits:
      cpu: 500m
      memory: 256Mi

messageSvc:
  replicas: 1
  dbMaxConnections: 10
  resources:
    requests:
      cpu: 50m
      memory: 64Mi
    limits:
      cpu: 500m
      memory: 256Mi

postgres:
  image: postgres:17-alpine
  # Empty: generated on install and kept on upgrades. If set, use letters and digits
  # only (it is part of a URL).
  password: ""
  storage: 1Gi
  # Empty: the cluster's default storage class.
  storageClassName: ""
  resources:
    requests:
      cpu: 100m
      memory: 256Mi
    limits:
      cpu: "1"
      memory: 1Gi

redis:
  image: redis:7-alpine
  # Empty: generated on install and kept on upgrades. Letters and digits only.
  password: ""
  resources:
    requests:
      cpu: 50m
      memory: 64Mi
    limits:
      cpu: 250m
      memory: 128Mi

ingress:
  enabled: true
  className: nginx
  host: push.example.com
  # nginx closes a proxied connection that is silent for this long; clients ping every
  # 30 s (spec §7.5).
  timeoutSeconds: 120

smoke:
  # The WebSocket URL the smoke test connects to: the ingress controller inside the
  # cluster, with `ingress.host` as the Host header, so the test goes through the
  # ingress like a client.
  socketUrl: ws://ingress-nginx-controller.ingress-nginx.svc.cluster.local/v1/socket
```

Create `deploy/helm/push-messenger/values-minikube.yaml`:

```yaml
# Local minikube only (deploy/minikube/up.ps1). Turns on the DEV ONLY identity and setup
# API that the smoke test needs.

devMode:
  enabled: true
  acknowledge: insecure-local-only

ingress:
  host: push.local
```

- [ ] **Step 2: Helpers, including the dev-mode guard**

Create `deploy/helm/push-messenger/templates/_helpers.tpl`:

```
{{/* Resource names, prefixed with the release name. */}}
{{- define "push.name" -}}
{{- printf "%s-%s" .root.Release.Name .component | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "push.labels" -}}
app.kubernetes.io/name: {{ .component }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/part-of: push-messenger
app.kubernetes.io/managed-by: {{ .root.Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .root.Chart.Name .root.Chart.Version }}
{{- end -}}

{{- define "push.selector" -}}
app.kubernetes.io/name: {{ .component }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
{{- end -}}

{{- define "push.secretName" -}}
{{- printf "%s-secrets" .Release.Name -}}
{{- end -}}

{{- define "push.image" -}}
{{- printf "%s:%s" .repository .tag -}}
{{- end -}}

{{/*
"true" when the DEV ONLY switches are on, "" otherwise. Rendering fails when
devMode.enabled is set without the acknowledgement, or with an ingress host that is not
local, so dev mode cannot reach a real deployment by accident.
*/}}
{{- define "push.devMode" -}}
{{- if .Values.devMode.enabled -}}
{{- if ne .Values.devMode.acknowledge "insecure-local-only" -}}
{{- fail "devMode.enabled switches authentication off: set devMode.acknowledge to \"insecure-local-only\", and only in a local values file" -}}
{{- end -}}
{{- $host := .Values.ingress.host -}}
{{- if not (or (hasSuffix ".local" $host) (hasSuffix ".localhost" $host) (hasSuffix ".test" $host)) -}}
{{- fail (printf "devMode is only allowed with a local ingress host (*.local, *.localhost or *.test), not %q" $host) -}}
{{- end -}}
true
{{- end -}}
{{- end -}}

{{/* "1" or "0" for the PUSH_DEV_AUTH and PUSH_DEV_SETUP variables. */}}
{{- define "push.devFlag" -}}
{{- if include "push.devMode" . -}}1{{- else -}}0{{- end -}}
{{- end -}}

{{/* Hardened container settings for our own images (distroless, uid 65532). */}}
{{- define "push.containerSecurity" -}}
securityContext:
  runAsNonRoot: true
  runAsUser: 65532
  runAsGroup: 65532
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true
  capabilities:
    drop: ["ALL"]
  seccompProfile:
    type: RuntimeDefault
{{- end -}}
```

- [ ] **Step 3: The Secret**

Create `deploy/helm/push-messenger/templates/secret.yaml`:

```yaml
{{- /*
Passwords come from values, or are generated on install and read back from the existing
Secret on upgrades (`lookup`), so they never change and never live in git. The URLs embed
them, so the services read the URLs from this Secret too, never from a ConfigMap.
*/ -}}
{{- $existing := (lookup "v1" "Secret" .Release.Namespace (include "push.secretName" .)).data | default dict -}}
{{- $postgresPassword := .Values.postgres.password | default (get $existing "postgres-password" | b64dec) | default (randAlphaNum 32) -}}
{{- $redisPassword := .Values.redis.password | default (get $existing "redis-password" | b64dec) | default (randAlphaNum 32) -}}
{{- $postgresHost := include "push.name" (dict "root" . "component" "postgres") -}}
{{- $redisHost := include "push.name" (dict "root" . "component" "redis") -}}
apiVersion: v1
kind: Secret
metadata:
  name: {{ include "push.secretName" . }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "secrets") | nindent 4 }}
type: Opaque
stringData:
  postgres-password: {{ $postgresPassword | quote }}
  redis-password: {{ $redisPassword | quote }}
  # message-svc connects as the owner of push_messaging, so it can run its migrations.
  database-url: {{ printf "postgres://push:%s@%s:5432/push_messaging" $postgresPassword $postgresHost | quote }}
  redis-url: {{ printf "redis://:%s@%s:6379" $redisPassword $redisHost | quote }}
```

- [ ] **Step 4: PostgreSQL and Redis**

Create `deploy/helm/push-messenger/templates/postgres.yaml`:

```yaml
{{- $name := include "push.name" (dict "root" . "component" "postgres") -}}
apiVersion: v1
kind: Service
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "postgres") | nindent 4 }}
spec:
  selector:
    {{- include "push.selector" (dict "root" . "component" "postgres") | nindent 4 }}
  ports:
    - name: postgres
      port: 5432
      targetPort: postgres
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "postgres") | nindent 4 }}
spec:
  serviceName: {{ $name }}
  replicas: 1
  selector:
    matchLabels:
      {{- include "push.selector" (dict "root" . "component" "postgres") | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "push.labels" (dict "root" . "component" "postgres") | nindent 8 }}
    spec:
      containers:
        - name: postgres
          image: {{ .Values.postgres.image }}
          ports:
            - name: postgres
              containerPort: 5432
          env:
            # One database per service: message-svc owns push_messaging.
            - name: POSTGRES_DB
              value: push_messaging
            - name: POSTGRES_USER
              value: push
            - name: POSTGRES_PASSWORD
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: postgres-password
            - name: PGDATA
              value: /var/lib/postgresql/data/pgdata
          readinessProbe:
            exec:
              command: ["pg_isready", "-U", "push", "-d", "push_messaging", "-h", "127.0.0.1"]
            periodSeconds: 5
          livenessProbe:
            exec:
              command: ["pg_isready", "-U", "push", "-d", "push_messaging", "-h", "127.0.0.1"]
            initialDelaySeconds: 30
            periodSeconds: 10
          resources:
            {{- toYaml .Values.postgres.resources | nindent 12 }}
          volumeMounts:
            - name: data
              mountPath: /var/lib/postgresql/data
  volumeClaimTemplates:
    - metadata:
        name: data
      spec:
        accessModes: ["ReadWriteOnce"]
        {{- with .Values.postgres.storageClassName }}
        storageClassName: {{ . }}
        {{- end }}
        resources:
          requests:
            storage: {{ .Values.postgres.storage }}
```

Create `deploy/helm/push-messenger/templates/redis.yaml`:

```yaml
{{- $name := include "push.name" (dict "root" . "component" "redis") -}}
apiVersion: v1
kind: Service
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "redis") | nindent 4 }}
spec:
  selector:
    {{- include "push.selector" (dict "root" . "component" "redis") | nindent 4 }}
  ports:
    - name: redis
      port: 6379
      targetPort: redis
---
# Redis is only a doorbell (spec §2): it keeps nothing worth persisting.
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "redis") | nindent 4 }}
spec:
  replicas: 1
  selector:
    matchLabels:
      {{- include "push.selector" (dict "root" . "component" "redis") | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "push.labels" (dict "root" . "component" "redis") | nindent 8 }}
    spec:
      containers:
        - name: redis
          image: {{ .Values.redis.image }}
          args: ["--requirepass", "$(REDIS_PASSWORD)", "--save", "", "--appendonly", "no"]
          env:
            - name: REDIS_PASSWORD
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: redis-password
          ports:
            - name: redis
              containerPort: 6379
          readinessProbe:
            tcpSocket:
              port: redis
            periodSeconds: 5
          livenessProbe:
            tcpSocket:
              port: redis
            initialDelaySeconds: 10
            periodSeconds: 10
          resources:
            {{- toYaml .Values.redis.resources | nindent 12 }}
```

Kubernetes expands `$(REDIS_PASSWORD)` in `args` from the container's environment, so
the password never appears in the manifest.

- [ ] **Step 5: Lint and render**

```powershell
helm lint --strict deploy/helm/push-messenger
helm lint --strict deploy/helm/push-messenger -f deploy/helm/push-messenger/values-minikube.yaml
helm template push deploy/helm/push-messenger | Select-String 'kind:'
```

Expected: `1 chart(s) linted, 0 chart(s) failed` twice (an `[INFO] icon is recommended`
line is fine); the render lists `Secret`, `Service`, `Service`, `Deployment`,
`StatefulSet`.

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add deploy/helm
git commit -m "deploy: chart with generated secrets, PostgreSQL and Redis"
```

---

### Task 3: The chart — the services, the Ingress and the smoke test

**Files:**
- Create: `deploy/helm/push-messenger/templates/message-svc.yaml`, `gateway.yaml`,
  `ingress.yaml`, `NOTES.txt`, `tests/smoke.yaml`; `deploy/helm/test-chart.ps1`

- [ ] **Step 1: Write the chart checks first**

Create `deploy/helm/test-chart.ps1`:

```powershell
# Checks the Helm chart without a cluster: lint with each values file, the DEV ONLY
# switches, and the guards that keep dev mode out of real deployments.
#
#   pwsh deploy/helm/test-chart.ps1
$ErrorActionPreference = 'Stop'
$chart = Join-Path $PSScriptRoot 'push-messenger'
$minikube = Join-Path $chart 'values-minikube.yaml'
$failures = 0

function Check([string] $name, [scriptblock] $test) {
    if (& $test) {
        Write-Output "ok   $name"
    } else {
        Write-Output "FAIL $name"
        $script:failures++
    }
}

# Exit code of `helm template` with these arguments, output discarded.
function Render([string[]] $arguments) {
    helm template push $chart @arguments 2>&1 | Out-Null
    return $LASTEXITCODE
}

Check 'lint with the default values' { helm lint $chart --strict | Out-Null; $LASTEXITCODE -eq 0 }
Check 'lint with values-minikube.yaml' { helm lint $chart --strict -f $minikube | Out-Null; $LASTEXITCODE -eq 0 }

$default = helm template push $chart | Out-String
Check 'default: the gateway refuses dev identities' { $default -match 'name: PUSH_DEV_AUTH\s+value: "0"' }
Check 'default: message-svc has no DevSetup API' { $default -match 'name: PUSH_DEV_SETUP\s+value: "0"' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
Check 'default: no ConfigMap (credentials stay in the Secret)' { $default -notmatch 'kind: ConfigMap' }

$dev = helm template push $chart -f $minikube | Out-String
Check 'minikube: dev identities on' { $dev -match 'name: PUSH_DEV_AUTH\s+value: "1"' }
Check 'minikube: DevSetup API on' { $dev -match 'name: PUSH_DEV_SETUP\s+value: "1"' }
Check 'minikube: the smoke test is a helm test' { $dev -match 'helm.sh/hook: test' }
Check 'minikube: ingress timeouts are 120 s' { $dev -match 'proxy-read-timeout: "120"' -and $dev -match 'proxy-send-timeout: "120"' }

Check 'dev mode without the acknowledgement is refused' {
    (Render @('--set', 'devMode.enabled=true', '--set', 'ingress.host=push.local')) -ne 0
}
Check 'dev mode with a public ingress host is refused' {
    (Render @('-f', $minikube, '--set', 'ingress.host=chat.example.com')) -ne 0
}

if ($failures -gt 0) {
    Write-Output "$failures check(s) failed"
    exit 1
}
Write-Output 'all chart checks passed'
```

```powershell
pwsh -NoProfile -File deploy/helm/test-chart.ps1
```

Expected: the two lint checks and the two "no smoke test" / "no ConfigMap" checks pass;
the other eight `FAIL` (the templates that set the variables, run the smoke test, set the
timeouts and use the dev-mode guard do not exist yet); the script ends with
`8 check(s) failed` and exit code 1.

- [ ] **Step 2: message-svc and the gateway**

Create `deploy/helm/push-messenger/templates/message-svc.yaml`:

```yaml
{{- $name := include "push.name" (dict "root" . "component" "message-svc") -}}
apiVersion: v1
kind: Service
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "message-svc") | nindent 4 }}
spec:
  selector:
    {{- include "push.selector" (dict "root" . "component" "message-svc") | nindent 4 }}
  ports:
    - name: grpc
      port: 50051
      targetPort: grpc
      appProtocol: grpc
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "message-svc") | nindent 4 }}
spec:
  replicas: {{ .Values.messageSvc.replicas }}
  selector:
    matchLabels:
      {{- include "push.selector" (dict "root" . "component" "message-svc") | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "push.labels" (dict "root" . "component" "message-svc") | nindent 8 }}
      {{- with .Values.buildId }}
      annotations:
        push-messenger/build-id: {{ . | quote }}
      {{- end }}
    spec:
      automountServiceAccountToken: false
      containers:
        - name: message-svc
          image: {{ include "push.image" .Values.images.messageSvc }}
          imagePullPolicy: {{ .Values.images.pullPolicy }}
          {{- include "push.containerSecurity" . | nindent 10 }}
          env:
            - name: PUSH_DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: database-url
            - name: PUSH_REDIS_URL
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: redis-url
            - name: PUSH_DB_MAX_CONNECTIONS
              value: {{ .Values.messageSvc.dbMaxConnections | quote }}
            - name: PUSH_DEV_SETUP
              value: {{ include "push.devFlag" . | quote }}
          ports:
            - name: grpc
              containerPort: 50051
            - name: health
              containerPort: 8081
          readinessProbe:
            httpGet:
              path: /readyz
              port: health
            periodSeconds: 5
          livenessProbe:
            httpGet:
              path: /healthz
              port: health
            initialDelaySeconds: 5
            periodSeconds: 10
          resources:
            {{- toYaml .Values.messageSvc.resources | nindent 12 }}
```

Create `deploy/helm/push-messenger/templates/gateway.yaml`:

```yaml
{{- $name := include "push.name" (dict "root" . "component" "gateway") -}}
apiVersion: v1
kind: Service
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "gateway") | nindent 4 }}
spec:
  selector:
    {{- include "push.selector" (dict "root" . "component" "gateway") | nindent 4 }}
  ports:
    - name: http
      port: 80
      targetPort: http
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "gateway") | nindent 4 }}
spec:
  replicas: {{ .Values.gateway.replicas }}
  selector:
    matchLabels:
      {{- include "push.selector" (dict "root" . "component" "gateway") | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "push.labels" (dict "root" . "component" "gateway") | nindent 8 }}
      {{- with .Values.buildId }}
      annotations:
        push-messenger/build-id: {{ . | quote }}
      {{- end }}
    spec:
      automountServiceAccountToken: false
      # On SIGTERM the gateway closes every connection with 4005 and waits up to 10 s.
      terminationGracePeriodSeconds: {{ .Values.gateway.terminationGracePeriodSeconds }}
      containers:
        - name: gateway
          image: {{ include "push.image" .Values.images.gateway }}
          imagePullPolicy: {{ .Values.images.pullPolicy }}
          {{- include "push.containerSecurity" . | nindent 10 }}
          env:
            - name: PUSH_MESSAGE_SVC_URL
              value: {{ printf "http://%s:50051" (include "push.name" (dict "root" . "component" "message-svc")) | quote }}
            - name: PUSH_REDIS_URL
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: redis-url
            - name: PUSH_HEARTBEAT_MS
              value: {{ .Values.gateway.heartbeatMs | quote }}
            - name: PUSH_DEV_AUTH
              value: {{ include "push.devFlag" . | quote }}
          ports:
            - name: http
              containerPort: 8080
            - name: health
              containerPort: 8081
          readinessProbe:
            httpGet:
              path: /readyz
              port: health
            periodSeconds: 5
          livenessProbe:
            httpGet:
              path: /healthz
              port: health
            initialDelaySeconds: 5
            periodSeconds: 10
          resources:
            {{- toYaml .Values.gateway.resources | nindent 12 }}
```

- [ ] **Step 3: The Ingress**

Create `deploy/helm/push-messenger/templates/ingress.yaml`:

```yaml
{{- if .Values.ingress.enabled -}}
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: {{ include "push.name" (dict "root" . "component" "gateway") }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "gateway") | nindent 4 }}
  annotations:
    # ingress-nginx passes the WebSocket upgrade through by itself; what it needs is
    # longer timeouts than its 60 s default, so idle sockets between client pings
    # survive (spec §7.5).
    nginx.ingress.kubernetes.io/proxy-read-timeout: {{ .Values.ingress.timeoutSeconds | quote }}
    nginx.ingress.kubernetes.io/proxy-send-timeout: {{ .Values.ingress.timeoutSeconds | quote }}
    nginx.ingress.kubernetes.io/proxy-http-version: "1.1"
spec:
  ingressClassName: {{ .Values.ingress.className }}
  rules:
    - host: {{ .Values.ingress.host }}
      http:
        paths:
          - path: /v1/socket
            pathType: Exact
            backend:
              service:
                name: {{ include "push.name" (dict "root" . "component" "gateway") }}
                port:
                  name: http
{{- end }}
```

- [ ] **Step 4: The smoke test and the notes**

Create `deploy/helm/push-messenger/templates/tests/smoke.yaml`:

```yaml
{{- if include "push.devMode" . -}}
# `helm test <release>` runs the end-to-end smoke test inside the cluster: the dev setup
# calls go straight to message-svc, the WebSockets through the ingress controller with
# the ingress host as the Host header, as a client's would.
apiVersion: v1
kind: Pod
metadata:
  name: {{ include "push.name" (dict "root" . "component" "smoke") }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "smoke") | nindent 4 }}
  annotations:
    helm.sh/hook: test
    helm.sh/hook-delete-policy: before-hook-creation
spec:
  restartPolicy: Never
  automountServiceAccountToken: false
  containers:
    - name: smoke
      image: {{ include "push.image" .Values.images.smoke }}
      imagePullPolicy: {{ .Values.images.pullPolicy }}
      {{- include "push.containerSecurity" . | nindent 6 }}
      args:
        - --setup
        - {{ printf "http://%s:50051" (include "push.name" (dict "root" . "component" "message-svc")) | quote }}
        - --socket
        - {{ .Values.smoke.socketUrl | quote }}
        - --host
        - {{ .Values.ingress.host | quote }}
      resources:
        requests:
          cpu: 10m
          memory: 16Mi
        limits:
          cpu: 200m
          memory: 64Mi
{{- end }}
```

`before-hook-creation` keeps the finished Pod until the next `helm test`, so its logs can
still be read.

Create `deploy/helm/push-messenger/templates/NOTES.txt`:

```
push-messenger {{ .Chart.AppVersion }} is installed as release "{{ .Release.Name }}" in namespace "{{ .Release.Namespace }}".

WebSocket endpoint: ws://{{ .Values.ingress.host }}/v1/socket (through the ingress controller).

To remove it, delete the namespace too:
  helm uninstall {{ .Release.Name }} --namespace {{ .Release.Namespace }}
  kubectl delete namespace {{ .Release.Namespace }}
`helm uninstall` alone keeps the PostgreSQL volume (data-{{ .Release.Name }}-postgres-0)
but deletes the Secret, so a later install generates a new password that the old
database does not accept.
{{- if include "push.devMode" . }}

DEV MODE IS ON: the gateway accepts identities without a token and message-svc serves the
DevSetup API. This is for a local cluster only.

Run the end-to-end smoke test:
  helm test {{ .Release.Name }} --namespace {{ .Release.Namespace }} --logs
{{- end }}
```

- [ ] **Step 5: Run the chart checks**

```powershell
pwsh -NoProfile -File deploy/helm/test-chart.ps1
```

Expected:

```
ok   lint with the default values
ok   lint with values-minikube.yaml
ok   default: the gateway refuses dev identities
ok   default: message-svc has no DevSetup API
ok   default: no smoke test
ok   default: no ConfigMap (credentials stay in the Secret)
ok   minikube: dev identities on
ok   minikube: DevSetup API on
ok   minikube: the smoke test is a helm test
ok   minikube: ingress timeouts are 120 s
ok   dev mode without the acknowledgement is refused
ok   dev mode with a public ingress host is refused
all chart checks passed
```

See a refusal for yourself:

```powershell
helm template push deploy/helm/push-messenger --set devMode.enabled=true
```

Expected: `Error: execution error at (push-messenger/templates/tests/smoke.yaml:1:7): devMode.enabled switches authentication off: set devMode.acknowledge to "insecure-local-only", and only in a local values file`.

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add deploy/helm
git commit -m "deploy: gateway, message-svc, ingress and the smoke test in the chart"
```

---

### Task 4: Bring it up on minikube and run the smoke test

**Files:**
- Create: `deploy/minikube/up.ps1`, `down.ps1`, `up.sh`, `down.sh`

- [ ] **Step 1: The PowerShell scripts**

```powershell
New-Item -ItemType Directory deploy\minikube -Force
```

Create `deploy/minikube/up.ps1`:

```powershell
<#
.SYNOPSIS
Brings push-messenger up on a local minikube cluster.

.DESCRIPTION
Starts (or reuses) a minikube profile with the docker driver, enables the ingress addon,
builds the images straight into the cluster's Docker daemon, installs or upgrades the Helm
chart with values-minikube.yaml, waits for every pod to be ready and prints how to reach
the gateway. With -Smoke it then runs the end-to-end smoke test (`helm test`).

.EXAMPLE
deploy/minikube/up.ps1
Starts the "push" profile, builds the images and installs the chart.

.EXAMPLE
deploy/minikube/up.ps1 -Smoke
The same, then runs the smoke test.

.EXAMPLE
deploy/minikube/up.ps1 -SkipBuild -Smoke
Reinstalls the chart with the images already in the cluster and runs the smoke test.
#>
[CmdletBinding()]
param(
    # The minikube profile; also the kubectl context.
    [string] $Cluster = 'push',
    [switch] $Smoke,
    [switch] $SkipBuild,
    [int] $Cpus = 4,
    [string] $Memory = '6g'
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$chart = Join-Path $root 'deploy\helm\push-messenger'
$dockerfile = Join-Path $root 'deploy\docker\Dockerfile'
$namespace = 'push'
$release = 'push'

# Runs a native command and stops the script if it fails.
function Invoke-Step([string] $What, [scriptblock] $Command) {
    Write-Host "==> $What" -ForegroundColor Cyan
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$What failed (exit code $LASTEXITCODE)"
    }
}

# `minikube start` is idempotent, but on a running profile it restarts every container in
# the cluster; skip it only when the node, kubelet and API server all run.
$state = minikube status -p $Cluster --format '{{.Host}}/{{.Kubelet}}/{{.APIServer}}' 2>$null
if ($state -ne 'Running/Running/Running') {
    Invoke-Step "start minikube profile '$Cluster'" {
        minikube start -p $Cluster --driver=docker --cpus=$Cpus --memory=$Memory
    }
}
Invoke-Step 'enable the ingress addon' { minikube -p $Cluster addons enable ingress }
Invoke-Step 'wait for ingress-nginx' {
    kubectl --context $Cluster -n ingress-nginx rollout status deployment/ingress-nginx-controller --timeout=300s
}
# The admission webhook validates Ingress objects; installing before its certificate
# jobs finish fails the install.
Invoke-Step 'wait for the ingress-nginx admission webhook' {
    kubectl --context $Cluster -n ingress-nginx wait --for=condition=complete `
        job/ingress-nginx-admission-create job/ingress-nginx-admission-patch --timeout=300s
}

# Build into the cluster's own Docker daemon, so the pods find the images without a
# registry. The DOCKER_* variables are restored afterwards.
$dockerVariables = 'DOCKER_TLS_VERIFY', 'DOCKER_HOST', 'DOCKER_CERT_PATH', 'MINIKUBE_ACTIVE_DOCKERD'
$saved = @{}
foreach ($name in $dockerVariables) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name)
}
try {
    $dockerEnv = minikube -p $Cluster docker-env --shell powershell
    if ($LASTEXITCODE -ne 0) {
        throw "minikube docker-env failed (exit code $LASTEXITCODE)"
    }
    $dockerEnv | Out-String | Invoke-Expression
    if (-not $SkipBuild) {
        foreach ($target in 'message-svc', 'gateway', 'smoke') {
            Invoke-Step "build push-messenger/${target}:dev" {
                docker build -f $dockerfile --target $target -t "push-messenger/${target}:dev" $root
            }
        }
    }
    # The image ids name the build: a rebuilt image rolls the pods, an unchanged one
    # does not.
    $ids = docker image inspect --format '{{.Id}}' push-messenger/message-svc:dev push-messenger/gateway:dev push-messenger/smoke:dev
    if ($LASTEXITCODE -ne 0) {
        throw 'the images are not in the cluster yet; run without -SkipBuild'
    }
    $buildId = ($ids | ForEach-Object { $_.Substring(7, 12) }) -join '-'
} finally {
    foreach ($name in $dockerVariables) {
        [Environment]::SetEnvironmentVariable($name, $saved[$name])
    }
}

Invoke-Step "install release '$release' in namespace '$namespace'" {
    helm upgrade --install $release $chart `
        --kube-context $Cluster --namespace $namespace --create-namespace `
        -f (Join-Path $chart 'values-minikube.yaml') `
        --set "buildId=$buildId" `
        --wait --timeout 10m
}

Write-Host ''
Write-Host "push-messenger is up: minikube profile '$Cluster', namespace '$namespace'." -ForegroundColor Green
Write-Host 'The gateway is behind ingress-nginx at ws://push.local/v1/socket. From this machine:'
Write-Host "  kubectl --context $Cluster -n ingress-nginx port-forward svc/ingress-nginx-controller 8080:80"
Write-Host '  then connect to ws://127.0.0.1:8080/v1/socket with the header "Host: push.local".'
Write-Host "Smoke test: deploy/minikube/up.ps1 -Cluster $Cluster -SkipBuild -Smoke"
Write-Host "Tear down:  deploy/minikube/down.ps1 -Cluster $Cluster"

if ($Smoke) {
    Invoke-Step 'run the smoke test' {
        helm test $release --kube-context $Cluster --namespace $namespace --logs --timeout 5m
    }
}
```

Create `deploy/minikube/down.ps1`:

```powershell
<#
.SYNOPSIS
Removes push-messenger from the local minikube cluster.

.DESCRIPTION
Uninstalls the Helm release and deletes its namespace, including the Postgres volume.
With -Delete it deletes the whole minikube profile instead.

.EXAMPLE
deploy/minikube/down.ps1
Removes the release and its data; the cluster keeps running.

.EXAMPLE
deploy/minikube/down.ps1 -Delete
Deletes the "push" minikube profile.
#>
[CmdletBinding()]
param(
    [string] $Cluster = 'push',
    [switch] $Delete
)

$ErrorActionPreference = 'Stop'

if ($Delete) {
    minikube delete -p $Cluster
    if ($LASTEXITCODE -ne 0) {
        throw "minikube delete failed (exit code $LASTEXITCODE)"
    }
    return
}

helm uninstall push --kube-context $Cluster --namespace push --wait --ignore-not-found
if ($LASTEXITCODE -ne 0) {
    throw "helm uninstall failed (exit code $LASTEXITCODE)"
}
kubectl --context $Cluster delete namespace push --ignore-not-found --wait=true
if ($LASTEXITCODE -ne 0) {
    throw "deleting the namespace failed (exit code $LASTEXITCODE)"
}
```

- [ ] **Step 2: The bash scripts**

Create `deploy/minikube/up.sh`:

```bash
#!/usr/bin/env bash
# Brings push-messenger up on a local minikube cluster (Linux, macOS, CI); the same steps
# as up.ps1.
#
#   deploy/minikube/up.sh [--cluster NAME] [--smoke] [--skip-build]
set -euo pipefail

cluster=push
smoke=false
skip_build=false
cpus=4
memory=6g
while [[ $# -gt 0 ]]; do
  case "$1" in
    --cluster) cluster="$2"; shift 2 ;;
    --smoke) smoke=true; shift ;;
    --skip-build) skip_build=true; shift ;;
    --cpus) cpus="$2"; shift 2 ;;
    --memory) memory="$2"; shift 2 ;;
    *) echo "usage: $0 [--cluster NAME] [--smoke] [--skip-build] [--cpus N] [--memory SIZE]" >&2; exit 2 ;;
  esac
done

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
chart="$root/deploy/helm/push-messenger"
namespace=push
release=push

step() { printf '==> %s\n' "$*"; }

# `minikube start` is idempotent, but on a running profile it restarts every container in
# the cluster; skip it only when the node, kubelet and API server all run.
state="$(minikube status -p "$cluster" --format '{{.Host}}/{{.Kubelet}}/{{.APIServer}}' 2>/dev/null || true)"
if [[ "$state" != "Running/Running/Running" ]]; then
  step "start minikube profile '$cluster'"
  minikube start -p "$cluster" --driver=docker --cpus="$cpus" --memory="$memory"
fi
step 'enable the ingress addon'
minikube -p "$cluster" addons enable ingress
step 'wait for ingress-nginx'
kubectl --context "$cluster" -n ingress-nginx rollout status deployment/ingress-nginx-controller --timeout=300s
# The admission webhook validates Ingress objects; installing before its certificate
# jobs finish fails the install.
step 'wait for the ingress-nginx admission webhook'
kubectl --context "$cluster" -n ingress-nginx wait --for=condition=complete \
  job/ingress-nginx-admission-create job/ingress-nginx-admission-patch --timeout=300s

# Point docker at the cluster's own Docker daemon. Called in subshells only, so the
# DOCKER_* variables do not leak into this script.
use_cluster_docker() {
  local env
  env="$(minikube -p "$cluster" docker-env --shell bash)" || return 1
  eval "$env"
}

# Builds run in a subshell, not a command substitution: errexit stays on there, so a
# failed build stops the script before anything is deployed.
if [[ "$skip_build" == false ]]; then
  (
    use_cluster_docker
    for target in message-svc gateway smoke; do
      step "build push-messenger/$target:dev"
      docker build -f "$root/deploy/docker/Dockerfile" --target "$target" -t "push-messenger/$target:dev" "$root"
    done
  )
fi
# The image ids name the build: a rebuilt image rolls the pods, an unchanged one does not.
if ! ids="$(use_cluster_docker && docker image inspect --format '{{.Id}}' \
  push-messenger/message-svc:dev push-messenger/gateway:dev push-messenger/smoke:dev)"; then
  echo 'the images are not in the cluster yet; run without --skip-build' >&2
  exit 1
fi
build_id="$(printf '%s\n' "$ids" | cut -c8-19 | paste -sd- -)"

step "install release '$release' in namespace '$namespace'"
helm upgrade --install "$release" "$chart" \
  --kube-context "$cluster" --namespace "$namespace" --create-namespace \
  -f "$chart/values-minikube.yaml" \
  --set "buildId=$build_id" \
  --wait --timeout 10m

echo
echo "push-messenger is up: minikube profile '$cluster', namespace '$namespace'."
echo 'The gateway is behind ingress-nginx at ws://push.local/v1/socket. From this machine:'
echo "  kubectl --context $cluster -n ingress-nginx port-forward svc/ingress-nginx-controller 8080:80"
echo '  then connect to ws://127.0.0.1:8080/v1/socket with the header "Host: push.local".'

if [[ "$smoke" == true ]]; then
  step 'run the smoke test'
  helm test "$release" --kube-context "$cluster" --namespace "$namespace" --logs --timeout 5m
fi
```

Create `deploy/minikube/down.sh`:

```bash
#!/usr/bin/env bash
# Removes push-messenger from the local minikube cluster; the same as down.ps1.
#
#   deploy/minikube/down.sh [--cluster NAME] [--delete]
set -euo pipefail

cluster=push
delete=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --cluster) cluster="$2"; shift 2 ;;
    --delete) delete=true; shift ;;
    *) echo "usage: $0 [--cluster NAME] [--delete]" >&2; exit 2 ;;
  esac
done

if [[ "$delete" == true ]]; then
  minikube delete -p "$cluster"
  exit 0
fi
helm uninstall push --kube-context "$cluster" --namespace push --wait --ignore-not-found
kubectl --context "$cluster" delete namespace push --ignore-not-found --wait=true
```

Mark them executable in git (Windows does not record the bit by itself):

```powershell
git add deploy/minikube
git update-index --chmod=+x deploy/minikube/up.sh deploy/minikube/down.sh
git ls-files -s deploy/minikube
```

Expected: `100755` for `up.sh` and `down.sh`, `100644` for the `.ps1` files.

- [ ] **Step 3: Bring it up and run the smoke test**

```powershell
deploy/minikube/up.ps1 -Smoke
```

This starts the minikube profile `push` (about 1–2 minutes the first time; a warning
"Failing to connect to https://registry.k8s.io/ from inside the minikube container" can
appear and is harmless if the ingress controller rolls out), enables the ingress addon,
waits for its controller and admission jobs (`job.batch/ingress-nginx-admission-create
condition met`, the same for `-patch`), builds the three images inside the node (about
3 minutes the first time: `Finished release profile … in 2m 39s` here), installs the
chart and runs the smoke test. The whole first run took about 5 minutes here. Expected
ending (ids and times differ):

```
==> install release 'push' in namespace 'push'
Release "push" does not exist. Installing it now.
NAME: push
NAMESPACE: push
STATUS: deployed
REVISION: 1
…
push-messenger is up: minikube profile 'push', namespace 'push'.
The gateway is behind ingress-nginx at ws://push.local/v1/socket. From this machine:
  kubectl --context push -n ingress-nginx port-forward svc/ingress-nginx-controller 8080:80
  then connect to ws://127.0.0.1:8080/v1/socket with the header "Host: push.local".
Smoke test: deploy/minikube/up.ps1 -Cluster push -SkipBuild -Smoke
Tear down:  deploy/minikube/down.ps1 -Cluster push
==> run the smoke test
…
TEST SUITE:     push-smoke
Phase:          Succeeded
…
POD LOGS: push-smoke
smoke: set up alice=01a1315e-fc37-70a1-b677-f6fbaee00264 bob=01a1315e-fc45-7685-b3ce-2f636a8bcfe8 as contacts with a direct conversation
smoke: bob connected (Ready, heartbeat 30000 ms)
smoke: alice connected (Ready, heartbeat 30000 ms)
smoke: alice got SendAck seq=1 after 7 ms
smoke: bob got Deliver seq=1 after 41 ms
smoke: bob acked; the gateway answered the following Ping
smoke: PASS in 206 ms
```

If the smoke test fails, read `kubectl --context push -n push logs deploy/push-gateway`
and `deploy/push-message-svc`, fix the cause, and rerun `up.ps1 -Smoke`.

- [ ] **Step 4: Check the deployment**

```powershell
kubectl --context push -n push get pods,svc,ingress,pvc
kubectl --context push -n push logs deploy/push-gateway | Select-Object -First 2
$controller = kubectl --context push -n ingress-nginx get pods -l app.kubernetes.io/component=controller -o name
kubectl --context push -n ingress-nginx exec $controller -- cat /etc/nginx/nginx.conf | Select-String 'server_name push.local|location = /v1/socket|proxy_read_timeout|proxy_send_timeout'
```

Expected: `push-gateway-…`, `push-message-svc-…`, `push-postgres-0` and `push-redis-…`
`1/1 Running` with 0 restarts, `push-smoke` `Completed`; four ClusterIP Services; the
Ingress `push-gateway` for `push.local`; the PVC `data-push-postgres-0` `Bound`. The
gateway's first log line is the JSON `WARN` "DEV ONLY: PUSH_DEV_AUTH=1 accepts any user
and device id …". nginx has `server_name push.local`, `location = /v1/socket` and, inside
it, `proxy_send_timeout 120s;` and `proxy_read_timeout 120s;` (the server-wide defaults
of `60s` also appear).

- [ ] **Step 5: Rerun without rebuilding, then restart the services**

```powershell
deploy/minikube/up.ps1 -SkipBuild -Smoke
kubectl --context push -n push get pods
```

Expected: no `start minikube profile` step (the profile runs), `REVISION: 2`, the smoke
test passes again, and the gateway and message-svc pods keep their names, ages and
restart counts: unchanged images do not roll the pods, the running cluster is not
restarted, and the generated passwords are kept.

Then check the start path from a stopped profile:

```powershell
minikube stop -p push
deploy/minikube/up.ps1 -SkipBuild -Smoke
```

Expected: `==> start minikube profile 'push'` runs, the admission jobs are still
complete, and the smoke test passes.

```powershell
kubectl --context push -n push delete pod -l app.kubernetes.io/name=gateway
kubectl --context push -n push delete pod -l app.kubernetes.io/name=message-svc
kubectl --context push -n push rollout status deploy/push-gateway --timeout=120s
kubectl --context push -n push rollout status deploy/push-message-svc --timeout=120s
helm test push --kube-context push --namespace push --logs
kubectl --context push -n push logs deploy/push-message-svc | Select-String 'already exists|connected'
```

Expected: each deletion returns within a few seconds (graceful shutdown on SIGTERM),
the new pods become ready, the smoke test passes, and message-svc's log shows the
migrations finding `_sqlx_migrations` already there (`relation "_sqlx_migrations"
already exists, skipping`) and `connected` to Postgres and Redis.

- [ ] **Step 6: Run the smoke binary from the host through port-forwards**

```powershell
$ingress = Start-Process kubectl -ArgumentList '--context','push','-n','ingress-nginx','port-forward','svc/ingress-nginx-controller','18080:80' -PassThru -WindowStyle Hidden
$grpc = Start-Process kubectl -ArgumentList '--context','push','-n','push','port-forward','svc/push-message-svc','15051:50051' -PassThru -WindowStyle Hidden
Start-Sleep 4
try {
    cargo run -q -p push-smoke -- --setup http://127.0.0.1:15051 --socket ws://127.0.0.1:18080/v1/socket --host push.local
} finally {
    Stop-Process -Id $ingress.Id, $grpc.Id
}
```

Expected: the same seven `smoke:` lines ending in `PASS`. This is the path to use when
debugging from the host; both port-forwards are stopped afterwards.

- [ ] **Step 7: The bash script (if Git Bash is installed)**

```powershell
& 'C:\Program Files\Git\bin\bash.exe' deploy/minikube/up.sh --cluster push --skip-build --smoke
```

Expected: `REVISION: 4` and the smoke test passes. Skip this step if Git Bash is not
installed; CI does not run it.

- [ ] **Step 8: Tear down**

```powershell
deploy/minikube/down.ps1
kubectl --context push get namespace push
```

Expected: `release "push" uninstalled`, `namespace "push" deleted`, then
`Error from server (NotFound): namespaces "push" not found`. Leave the profile running
for Task 5's final check, or delete it now with `deploy/minikube/down.ps1 -Delete`.

- [ ] **Step 9: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add deploy/minikube
git commit -m "deploy: bring-up and tear-down scripts for minikube"
```

(`git update-index` in Step 2 already staged the scripts with their executable bits.)

---

### Task 5: CI, spec, roadmap and pull request

**Files:**
- Modify: `.github/workflows/ci.yml`, `README.md`,
  `docs/superpowers/specs/2026-10-11-push-messenger-design.md`,
  `docs/superpowers/plans/2026-10-11-roadmap.md`

- [ ] **Step 1: Build the images and check the chart in CI**

Replace `.github/workflows/ci.yml` with:

```yaml
name: ci

on:
  push:
    branches: [main]
  pull_request:

permissions:
  contents: read

jobs:
  rust:
    runs-on: ubuntu-latest
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace --all-targets --locked -- -D warnings
      # The integration tests start these images through the runner's Docker.
      - run: docker pull postgres:17-alpine
      - run: docker pull redis:7-alpine
      - run: cargo test --workspace --locked

  msrv:
    runs-on: ubuntu-latest
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.85
      - uses: Swatinem/rust-cache@v2
      - run: docker pull postgres:17-alpine
      - run: docker pull redis:7-alpine
      - run: cargo test --workspace --locked

  # The container images build (nothing is pushed). Deploying them is verified on
  # minikube before merge: deploy/minikube/up.ps1 -Smoke.
  images:
    runs-on: ubuntu-latest
    timeout-minutes: 45
    steps:
      - uses: actions/checkout@v4
      - name: Build the images
        run: |
          for target in message-svc gateway smoke; do
            docker build -f deploy/docker/Dockerfile --target "$target" \
              -t "push-messenger/$target:ci" .
          done

  chart:
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@v4
      - uses: azure/setup-helm@v4
        with:
          version: v3.16.3
      - run: pwsh deploy/helm/test-chart.ps1
```

Check the image job's loop locally (Docker Desktop, cached from Task 1 if the images
were built there):

```powershell
& 'C:\Program Files\Git\bin\bash.exe' -c 'for target in message-svc gateway smoke; do docker build -q -f deploy/docker/Dockerfile --target "$target" -t "push-messenger/$target:ci" . || exit 1; done; echo built'
docker rmi push-messenger/message-svc:ci push-messenger/gateway:ci push-messenger/smoke:ci
```

Expected: three image ids and `built`.

- [ ] **Step 2: Tell developers how to run it**

In `README.md`, add a new section between the `## Build and test` section and
`## License`: a heading line `## Run it on minikube`, a blank line, then this text and a
blank line:

```markdown
The whole system runs on a local [minikube](https://minikube.sigs.k8s.io/) cluster with
the same Helm chart that will deploy to Azure. With Docker, minikube, kubectl and Helm
installed:

    deploy/minikube/up.ps1 -Smoke

starts (or reuses) the minikube profile `push`, enables its ingress addon, builds the
images into the cluster, installs the chart (`deploy/helm/push-messenger`) with
`values-minikube.yaml`, and runs the end-to-end smoke test: two users chat through the
ingress. `deploy/minikube/up.sh --smoke` does the same on Linux and macOS;
`deploy/minikube/down.ps1` removes it again, namespace and database volume included
(`-Delete` deletes the profile). Removing the release with `helm uninstall` alone keeps
the PostgreSQL volume but not the Secret, so a later install generates a password the
old database rejects: delete the namespace as well. Every pull request is verified this
way before it is merged.

The minikube values switch on a DEV ONLY identity: the gateway accepts any user and
device id without a token. The chart refuses that mode unless the values also say
`devMode.acknowledge: insecure-local-only` and the ingress host is local (`*.local`,
`*.localhost` or `*.test`). `pwsh deploy/helm/test-chart.ps1` checks the chart without a
cluster.
```

- [ ] **Step 3: Record the deployment in the spec**

In §11 of the spec, after the line
`- Region, VM size and domain are confirmed with the owner before any cloud spend.`, add:

```markdown

Implementation notes (local minikube, Plan 4B):

- One Helm chart, `deploy/helm/push-messenger`: the gateway and message-svc
  (Deployments), PostgreSQL 17 (StatefulSet with a volume, database `push_messaging`
  owned by the user message-svc migrates as) and Redis 7 (AUTH on, no persistence), and an
  Ingress for `/v1/socket` with ingress-nginx's read and send timeouts at 120 s. Database
  and Redis passwords are generated on install and kept in one Secret; the services read
  their connection URLs from it. Probes: `/healthz` and `/readyz` on port 8081 of each
  service.
- Images (`deploy/docker/Dockerfile`, one target per binary from one shared build stage)
  run as a non-root user on distroless, with a read-only root filesystem.
- `deploy/minikube/up.ps1` (and `up.sh`) bring it up on minikube; `helm test` runs
  `push-smoke` inside the cluster through the ingress controller. The Azure plan replaces
  the in-chart secrets with Key Vault and adds TLS, and may replace the in-chart
  PostgreSQL and Redis.
```

- [ ] **Step 4: Mark plan 4B in review**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the empty status cell at the
end of the row for plan 4B (`| 4B | **Walking skeleton: minikube** | … | 4A | |`) with
`In review`, so the row ends `| 4A | In review |`.

- [ ] **Step 5: Final gates and a fresh minikube run**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 test --workspace --locked
pwsh -NoProfile -File deploy/helm/test-chart.ps1
deploy/minikube/up.ps1 -Smoke
```

Expected: no diff, no warnings, 604 tests on both toolchains, `all chart checks passed`,
and the smoke test `PASS`. Copy the seven `smoke:` lines for the pull request. Then tear
down: `deploy/minikube/down.ps1` (keep the profile for later plans) or
`deploy/minikube/down.ps1 -Delete`.

- [ ] **Step 6: Commit and push**

```powershell
git add .github README.md docs
git commit -m "docs: record plan 4B in the spec, roadmap and CI"
git push -u origin feature/plan-04b-skeleton-minikube
```

- [ ] **Step 7: Open the pull request and wait for CI**

Put the smoke output from Step 5 into the body (replace the placeholder line):

```powershell
$body = @'
Implements docs/superpowers/plans/2026-10-12-plan-04b-minikube-skeleton-cluster.md: one Dockerfile with distroless non-root targets for message-svc, the gateway and push-smoke; the Helm chart deploy/helm/push-messenger (gateway, message-svc, PostgreSQL 17 StatefulSet, Redis 7 with AUTH, generated secrets, probes, resources, ingress-nginx with 120 s WebSocket timeouts, the smoke test as a helm test); values-minikube.yaml with the DEV ONLY identity, refused by the chart without the acknowledgement or with a non-local host; deploy/minikube/up.ps1/up.sh/down.ps1/down.sh; CI builds the images and checks the chart.

Verified on minikube (deploy/minikube/up.ps1 -Smoke):

<paste the seven smoke: lines here>
'@
gh pr create --base main --title "Plan 4B: walking skeleton on minikube" --body $body
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust`, `msrv`, `images`, `chart`). Do not merge;
the controller merges after the final review and its own minikube run.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §1/§11 Kubernetes, Helm; the same chart as the Azure deployment | 2, 3 |
| §2 four units behind ingress-nginx; Postgres and Redis in the cluster; the gateway has no database credentials (its Deployment gets only the Redis URL and message-svc's address) | 2, 3 |
| §2 public endpoint `/v1/socket` only; message-svc internal | 3 |
| §7.4 4005 on shutdown: SIGTERM, `terminationGracePeriodSeconds` 30 > the gateway's 10 s drain | 3, 4 |
| §7.5 nginx `proxy_read_timeout` / `proxy_send_timeout` 120 s; WebSocket upgrade through ingress-nginx | 3, 4 |
| §11 PostgreSQL and Redis in the cluster with a persistent volume and Redis AUTH; secrets never in git or ConfigMaps | 2, 3 |
| Roadmap "Running locally on minikube": one script brings everything up; every plan verified on minikube before merge | 4, 5 |
| Plan 4 decisions: images built into minikube, `push-messenger/<name>:dev`; probes; resources; dev auth off by default and impossible to enable by accident; the smoke test through the ingress | 1–5 |

## Deferred to later plans

- **A cluster in CI** (kind or minikube running `up.sh --smoke`): minikube on the
  implementer's machine is the merge gate for now.
- **Azure** (Plan 11): GHCR images by digest, Key Vault secrets through the CSI driver,
  TLS with cert-manager, the Azure Load Balancer idle timeout, k3s on a VM, possibly
  managed PostgreSQL and Redis; `values-azure.yaml` with dev mode off.
- **Gateway draining in batches in a `preStop` hook** and a `PodDisruptionBudget`
  (Plan 8); horizontal scaling and anti-affinity.
- **auth-svc and the worker in the chart** (Plans 5 and 6), each with its own database.
- **The Postgres role is a superuser on minikube** (`POSTGRES_USER` of the official
  image). Fix before Azure: a superuser only for bootstrap, and message-svc connecting as
  a plain role that owns `push_messaging`.
- **gRPC load balancing across message-svc replicas:** HTTP/2 keeps one connection per
  gateway, so the ClusterIP Service pins each gateway to one replica (see Plan 4A).
- **NetworkPolicies** (only the gateway may call message-svc; only the services may reach
  the stores), Postgres backups, resource tuning from the load test.
