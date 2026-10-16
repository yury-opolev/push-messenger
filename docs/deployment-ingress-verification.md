# Maintained-controller ingress verification (Plan 11B)

This records how the portable chart interfaces were verified against a maintained
controller (Traefik) and against the existing local compatibility path, and what was
observed. It is **acceptance evidence for the reusable chart**, not a production probe,
a load test or a consumer deployment guide. Consumer load balancers, log sinks and
capacity remain the consumer's acceptance (see
[deployment-configuration.md](deployment-configuration.md)).

All fixtures are DEV ONLY and run on dedicated, owned minikube profiles. Never point
them at a shared or production installation: they scale auth-svc to zero, attach
fixture pods to its Service and exhaust a login rate limit.

## Results (2026-10-16)

Source: branch `feature/plan-11b-portable-chart` at `72a4548` (chart, docs) plus the
fixtures below. Windows 11, Docker 29.7.2, minikube v1.34.0 (docker driver), Kubernetes
v1.31.0, Calico v3.28.1 (`--cni=calico`), kubectl v1.36.1, Helm v3.16.3, Python 3.13.2.
Controller: Traefik Helm chart **42.0.0** (package SHA-256
`a6ede324662ddcca0160a5ce639b7b5b9dd40a20a475e1101e9f95b5e5d83dd5`), image
`docker.io/traefik:v3.7.14` (`sha256:575fa15b135078fe5e50aa847987d96dbddd7b093c172429618404df73f3fa7c`).
Local path: ingress-nginx addon `v1.11.2`.

| # | Context | Case | Expected | Observed |
|---|---|---|---|---|
| 1 | `push11b-local` | `up.ps1 -Smoke` on a fresh profile (chart-managed Secret, nginx compatibility values) | raw protocol and SDK smoke pass | PASS in 16.7 s |
| 2 | `push11b-local` | `test-network-policy.ps1` | 15 caller/API checks | PASS |
| 3 | `push11b-traefik` | install with `secrets.existingSecret` | release manifest has no Secret | 0 Secret objects (ConfigMap, 5 Deployments, 4 Ingress, 6 NetworkPolicies, 6 Services, StatefulSet) |
| 4 | `push11b-traefik` | install + second upgrade | external Secret UID, resourceVersion, data, labels, annotations, owners unchanged | all unchanged |
| 5 | `push11b-traefik` | `test-network-policy.ps1` | 15 caller/API checks | PASS |
| 6 | `push11b-traefik` | TLS with the fixture CA and SNI `push11b.test` | succeeds | PASS |
| 7 | `push11b-traefik` | TLS with system trust only; TLS with wrong SNI `wrong.test` | certificate verification error | both rejected |
| 8 | `push11b-traefik` | public `/dev/mailbox` | not routed | 404 |
| 9 | `push11b-traefik` | WSS upgrade, Identify, application Ping/Pong every 30 s | socket survives past the controller's 120 s read/idle timeouts | 180.0 s active, every Pong received |
| 10 | `push11b-traefik` | WSS Identify, then silence | gateway policy close 1008 after ~2.5 heartbeats (75 s), before 100 s | 1008 after 81.8 s |
| 11 | `push11b-traefik` | 11 wrong-password logins for one account, each with a different forged `X-Forwarded-For`/`X-Real-IP` | 10 x 401, then 429 with `Retry-After` (one per-account/IP identity) | 10 x 401, 1 x 429 |
| 12 | `push11b-traefik` | two devices redeem a nonexistent invite over WSS with different forged headers | both `INVITE_NOT_FOUND`; one shared gateway per-IP counter = 2; key not derived from either forged address | one key, count 2, equals the real peer (`127.0.0.1`), no forged match |
| 13 | `push11b-traefik` | both link routes, healthy | 200, no token marker in any controller/application log | PASS |
| 14 | `push11b-traefik` | both link routes, auth scaled to zero | 503 (`allowEmptyServices`), no marker | PASS |
| 15 | `push11b-traefik` | both link routes, connection refused by a policy-admitted endpoint | 502, no marker | PASS |
| 16 | `push11b-traefik` | both link routes, upstream withholds headers past 5 s | 504 after 5 s, no marker | PASS (11 s for both) |
| 17 | `push11b-traefik` | controller logs across 13-16 | access entries present, zero error-level lines, no marker | 53 access entries, 0 error lines, clean |
| 18 | `push-disposable` | sender `resend`, Secret lacks `resend-api-key` | worker `CreateContainerConfigError`, Helm wait fails; recovers once provided | as expected |
| 19 | `push-disposable` | webhook enabled, Secret lacks `resend-webhook-secret` | same | as expected |
| 20 | `push-disposable` | webhook disabled again | reference removed; retained key and Secret UID unchanged | as expected |
| 21 | `push-disposable` | Secret lacks `database-url`, message-svc restarted | new pod `CreateContainerConfigError`, old pod keeps serving, no fallback; recovers | as expected |
| 22 | `push-disposable` | `helm uninstall` | external Secret UID, data, labels, annotations, owners unchanged | all unchanged |
| 23 | static | image digests | rendered `repository@sha256:...` for all five application images | chart tests (`test-portability.py`) |

Controller access-log statuses over the whole run: `200x13 201x2 202x2 401x10 404x1
429x1 499x1 502x3 503x12 504x4`, plus four WebSocket upgrades logged with status 0.
The single 499 came from a superseded single-threaded timeout fixture (see corrections).

### Limitations of this evidence

- **Source address.** The minikube docker driver on Windows does not route the node IP
  (`192.168.85.2`) to the host, so the client reached Traefik's Service through an
  owned `kubectl port-forward` (TCP, TLS/SNI end to end). Traefik therefore saw
  `127.0.0.1`. Cases 11-12 prove that client-supplied forwarding headers are discarded
  and that the services count the address Traefik appends; they do **not** prove
  external load-balancer source-IP preservation, which remains a consumer acceptance
  check for their actual LB, trusted hops and controller configuration.
- **Image digests.** No consumer registry was used; digest rendering is proven by the
  chart tests. Pulling by digest needs consumer-published images and was not performed.
- **Private directory encryption.** EFS was unavailable on the test host; the fixture
  directory was restricted to the current user by ACL only. It held only non-secret
  files (fixture code, values, the public test CA); credentials and the TLS private key
  stayed in memory and in the cluster.
- **Controller version.** Two advisories published on 2026-10-16 (GHSA-p8wx-ph99-xgcr,
  GHSA-28hp-c82p-8vqj) are fixed in Traefik v3.7.15; both affect only the Kubernetes
  Ingress-NGINX provider, which this configuration does not enable, and no chart release
  pinned v3.7.15 yet. Consumers should run a currently supported, patched Traefik
  (v3.7.15 or later) and recheck advisories.

### Corrections discovered during execution

The plan's draft fixtures were corrected; the code below is what ran.

- Chart 42 sets the Service type with `service.spec.type`, not `service.type`.
- A Service `targetPort` pointing at an unused port cannot produce a refusal on an
  enforcing CNI: the auth NetworkPolicy admits the controller on 8080 only, so the SYN is
  dropped and the dial times out. The refusal fixture instead declares port 8080 on a
  same-release, auth-labelled pod that listens only on a readiness port, so the
  policy-admitted connection gets a TCP reset.
- Each failure phase waits for the EndpointSlice change **and** for the controller to
  answer the expected status on a non-link route (`Wait-RouteStatus`) before probing the
  link routes; Kubernetes convergence alone raced the controller's view.
- The timeout backend must be concurrent (`ThreadingHTTPServer`); a single-threaded
  server serialized the second request behind the first.
- `Wait-AuthEndpoints` must ignore the `$null` item PowerShell yields when piping an
  empty endpoint list.
- `kubectl rollout status` right after `scale` can report the previous state; restore
  waits for `condition=Available` and ready endpoints instead.

## Reproducing it

Run from the repository root in PowerShell 7 without tracing or transcription. Use only
newly created, owned profiles; never `push` or profiles holding data you need.
`minikube start`/`stop` change kubectl's current context; restore yours afterwards.

### 1. Local compatibility path

```powershell
minikube start -p push11b-local --driver=docker --cni=calico --cpus=4 --memory=6g
if ($LASTEXITCODE -ne 0) { throw 'Local verification profile failed' }
.\deploy\minikube\up.ps1 -Cluster push11b-local -Smoke
if ($LASTEXITCODE -ne 0) { throw 'Existing local smoke failed' }
.\deploy\minikube\test-network-policy.ps1 -Context push11b-local
if ($LASTEXITCODE -ne 0) { throw 'Local network isolation failed' }
minikube stop -p push11b-local
```

### 2. Private directory, values and helpers

Create a directory outside every Git tree, readable only by you (encrypted where the
platform supports it), and save these files in it.

`traefik-values.yaml` (verification only; untrusted forwarding headers, 120 s
read/idle timeouts, field-allow-listed JSON access logs without paths or queries, a 5 s
upstream response-header timeout, no CRD or Gateway providers):

```yaml
ingressClass:
  enabled: true
  isDefaultClass: false
  name: traefik
providers:
  kubernetesCRD:
    enabled: false
  kubernetesGateway:
    enabled: false
  kubernetesIngress:
    enabled: true
    ingressClass: traefik
    allowEmptyServices: true
api:
  dashboard: false
service:
  spec:
    type: NodePort
additionalArguments:
  - --serverstransport.forwardingtimeouts.responseheadertimeout=5s
ports:
  web:
    forwardedHeaders:
      insecure: false
      trustedIPs: []
  websecure:
    forwardedHeaders:
      insecure: false
      trustedIPs: []
    transport:
      respondingTimeouts:
        readTimeout: 120s
        writeTimeout: 0s
        idleTimeout: 120s
log:
  level: ERROR
  format: json
accessLog:
  enabled: true
  format: json
  bufferingSize: 0
  fields:
    defaultMode: drop
    names:
      RequestMethod: keep
      DownstreamStatus: keep
      RouterName: keep
      Duration: keep
    headers:
      defaultMode: drop
    queryParameters:
      defaultMode: drop
```

`app-values.yaml` (references only; DEV ONLY mode for the local mailbox):

```yaml
secrets:
  existingSecret: consumer-credentials
devMode:
  enabled: true
  acknowledge: insecure-local-only
worker:
  replicas: 1
  email:
    sender: log
    webhook:
      enabled: true
ingress:
  enabled: true
  className: traefik
  host: push11b.test
  scheme: https
  tls:
    secretName: push-tls
  annotations:
    traefik.ingress.kubernetes.io/router.entrypoints: websecure
auth:
  clientIpHeader: x-forwarded-for
gateway:
  clientIpHeader: x-forwarded-for
networkPolicy:
  ingressControllerPeers:
    - namespaceSelector:
        kubernetes.io/metadata.name: edge
      podSelector:
        app.kubernetes.io/name: traefik
```

`disp-values.yaml` (disposable missing-key release):

```yaml
secrets:
  existingSecret: disp-credentials
ingress:
  enabled: false
  host: disp.example.test
worker:
  email:
    sender: resend
    from: "Disposable <no-reply@disp.example.test>"
    webhook:
      enabled: false
```

`verify-helpers.ps1` (dot-sourced; never prints request or Secret bodies):

```powershell
$ErrorActionPreference = 'Stop'
$Private = Split-Path -Parent $PSCommandPath
$Ctx = 'push11b-traefik'
function NativeOK([string] $Operation) {
    if ($LASTEXITCODE -ne 0) { throw "$Operation failed (exit $LASTEXITCODE)" }
}
function New-FixturePassword {
    [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(24))
}
# Creates a manifest through stdin; never prints request or response bodies.
function Invoke-FixtureCreate([object] $Manifest, [string] $Namespace = 'push', [string] $Verb = 'create') {
    $info = [Diagnostics.ProcessStartInfo]::new('kubectl')
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($arg in @('--context', $Ctx, '-n', $Namespace, $Verb,
        '--request-timeout=30s', '-f', '-', '-o', 'name')) {
        $info.ArgumentList.Add($arg)
    }
    $child = [Diagnostics.Process]::Start($info)
    $stdout = $child.StandardOutput.ReadToEndAsync()
    $stderr = $child.StandardError.ReadToEndAsync()
    try {
        $payload = $Manifest | ConvertTo-Json -Depth 24 -Compress
        if ($payload.Length -gt 65536) { throw 'Unexpectedly large verification manifest' }
        $child.StandardInput.WriteLine($payload)
        $child.StandardInput.Close()
        if (-not $child.WaitForExit(45000)) { throw 'Verification create timed out' }
        if ($child.ExitCode -ne 0) { throw 'Verification create failed; diagnostic bodies withheld' }
    } finally {
        if (-not $child.HasExited) { $child.Kill($true); $child.WaitForExit() }
        $child.Dispose()
        $payload = $stdout = $stderr = $null
    }
}
# A full consumer key set for release $Release in $Namespace, built in memory.
function New-FixtureSecret([string] $Name, [string] $Namespace, [string] $Release, [string[]] $Omit = @()) {
    $admin = New-FixturePassword; $messaging = New-FixturePassword
    $auth = New-FixturePassword; $worker = New-FixturePassword; $redis = New-FixturePassword
    $data = [ordered]@{
        'postgres-password' = $admin
        'messaging-db-password' = $messaging
        'database-url' = "postgres://push_messaging:${messaging}@${Release}-postgres:5432/push_messaging"
        'auth-db-password' = $auth
        'auth-database-url' = "postgres://push_auth:${auth}@${Release}-postgres:5432/push_auth"
        'worker-db-password' = $worker
        'worker-database-url' = "postgres://push_worker:${worker}@${Release}-postgres:5432/push_worker"
        'worker-auth-database-url' = "postgres://push_worker:${worker}@${Release}-postgres:5432/push_auth"
        'redis-password' = $redis
        'redis-url' = "redis://:${redis}@${Release}-redis:6379"
        'auth-signing-key' = [Convert]::ToBase64String([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
        'auth-link-key' = [Convert]::ToBase64String([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
        'accounts-token' = New-FixturePassword
        'directory-token' = New-FixturePassword
        'messaging-token' = New-FixturePassword
        'resend-api-key' = 're_fixture_' + (New-FixturePassword)
        'resend-webhook-secret' = 'whsec_' + [Convert]::ToBase64String([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
    }
    foreach ($key in $Omit) { $data.Remove($key) }
    @{ apiVersion = 'v1'; kind = 'Secret'; type = 'Opaque'
       metadata = @{ name = $Name; namespace = $Namespace }; stringData = $data }
}
function Get-SecretIdentity([string] $Name, [string] $Namespace) {
    $json = kubectl --context $Ctx -n $Namespace get secret $Name -o json
    NativeOK 'Read Secret identity'
    $s = $json | ConvertFrom-Json
    $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData(
        [Text.Encoding]::UTF8.GetBytes(($s.data | ConvertTo-Json -Compress))))
    [pscustomobject]@{
        Uid = $s.metadata.uid; Version = $s.metadata.resourceVersion; DataHash = $hash
        Labels = ($s.metadata.labels | ConvertTo-Json -Compress)
        Annotations = ($s.metadata.annotations | ConvertTo-Json -Compress)
        Owners = ($s.metadata.ownerReferences | ConvertTo-Json -Compress)
    }
}
function Invoke-IngressFixture([int] $LinkStatus = 200, [switch] $LinksOnly) {
    $info = [Diagnostics.ProcessStartInfo]::new('python')
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($arg in @((Join-Path $Private 'check-ingress.py'),
        '--connect-host', "$ConnectHost", '--connect-port', "$ConnectPort",
        '--ca', (Join-Path $Private 'push11b-ca.pem'), '--canary', $Canary,
        '--link-status', "$LinkStatus")) {
        $info.ArgumentList.Add($arg)
    }
    if ($LinksOnly) { $info.ArgumentList.Add('--links-only') }
    $child = [Diagnostics.Process]::Start($info)
    $stdout = $child.StandardOutput.ReadToEndAsync()
    $stderr = $child.StandardError.ReadToEndAsync()
    try {
        if (-not $child.WaitForExit(480000)) { throw 'Ingress fixture timed out' }
        Write-Host ($stdout.GetAwaiter().GetResult())
        if ($child.ExitCode -ne 0) { throw 'Ingress fixture failed; diagnostic bodies withheld' }
    } finally {
        if (-not $child.HasExited) { $child.Kill($true); $child.WaitForExit() }
        $child.Dispose()
        $stdout = $stderr = $null
    }
}
# Owned loopback port-forward; returns the process. Caller stops it by PID.
function Start-OwnedForward([string] $Namespace, [string] $Target, [int] $Local, [int] $Remote) {
    $p = Start-Process kubectl -NoNewWindow -PassThru -RedirectStandardOutput (Join-Path $Private "pf-$Local.out") -RedirectStandardError (Join-Path $Private "pf-$Local.err") -ArgumentList @(
        '--context', $Ctx, '-n', $Namespace, 'port-forward', $Target, "${Local}:${Remote}", '--address', '127.0.0.1')
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        if ($p.HasExited) { throw "Owned port-forward $Local exited" }
        $probe = [Net.Sockets.TcpClient]::new()
        try {
            $connected = $probe.ConnectAsync('127.0.0.1', $Local)
            if ($connected.Wait(1000) -and $probe.Connected) { return $p }
        } catch [AggregateException] {
        } finally { $probe.Dispose() }
        Start-Sleep -Seconds 1
    }
    Stop-Process -Id $p.Id
    throw "Owned port-forward $Local not responsive"
}
# Fails if any controller or application log since $Since holds the canary or a token query.
function Assert-LogsClean([string] $Since, [string] $Phase) {
    $sources = @(
        @('edge', 'app.kubernetes.io/name=traefik'),
        @('push', 'app.kubernetes.io/instance=push')
    )
    $access = 0
    foreach ($s in $sources) {
        $pods = @(kubectl --context $Ctx -n $s[0] get pods -l $s[1] -o 'jsonpath={range .items[*]}{.metadata.name}{"\n"}{end}') | Where-Object { $_ }
        NativeOK 'List log pods'
        foreach ($pod in $pods) {
            $text = (kubectl --context $Ctx -n $s[0] logs $pod --all-containers --since-time=$Since 2>$null) -join "`n"
            if ($LASTEXITCODE -ne 0) { throw "Read logs failed for $pod" }
            foreach ($needle in @($Canary, [uri]::EscapeDataString($Canary), '?token=', 'token=push11b')) {
                if ($text.Contains($needle)) { throw "$Phase : token marker found in $pod logs" }
            }
            if ($s[0] -eq 'edge') { $access += ([regex]::Matches($text, '"DownstreamStatus"')).Count }
        }
    }
    if ($access -eq 0) { throw "$Phase : no controller access-log entries; cannot prove token safety" }
    Write-Host "ok   $Phase logs: $access controller access entries, no token marker"
}
function Wait-AuthEndpoints([int] $Port) {
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        $json = kubectl --context $Ctx -n push get endpointslice `
            -l 'kubernetes.io/service-name=push-auth' -o json
        NativeOK 'Read fixture auth EndpointSlices'
        # `$_ -and`: piping a null endpoints list yields one $null item.
        $ready = @(($json | ConvertFrom-Json).items | Where-Object {
            @($_.endpoints | Where-Object { $_ -and $_.conditions.ready -ne $false }).Count -gt 0
        })
        if ($Port -eq 0 -and $ready.Count -eq 0) { return }
        $ports = @($ready | ForEach-Object { $_.ports } | ForEach-Object { $_.port })
        if ($Port -ne 0 -and $ready.Count -gt 0 -and $ports.Count -gt 0 -and
            @($ports | Where-Object { $_ -ne $Port }).Count -eq 0) { return }
        Start-Sleep -Seconds 2
    }
    throw 'Fixture auth endpoints did not converge'
}
# Polls a non-link public route (/v1/auth/jwks) until the controller answers $Status, so a
# phase starts only after the controller has observed the endpoint change.
function Wait-RouteStatus([int] $Status) {
    $probe = @"
import http.client, socket, ssl, sys
ctx = ssl.create_default_context(cafile=sys.argv[1])
conn = http.client.HTTPConnection('127.0.0.1', 18443, timeout=12)
conn.sock = ctx.wrap_socket(socket.create_connection(('127.0.0.1', 18443), timeout=12), server_hostname='push11b.test')
conn.request('GET', '/v1/auth/jwks', headers={'Host': 'push11b.test'})
print(conn.getresponse().status)
"@
    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        $got = python -c $probe (Join-Path $Private 'push11b-ca.pem') 2>$null
        if ("$got".Trim() -eq "$Status") { return }
        Start-Sleep -Seconds 2
    }
    throw "Controller did not converge to $Status (last: $got)"
}
# A same-release pod selected by the auth Service: declares port 8080 (named http) but
# listens only on 8081 for readiness, so the policy-admitted connection is refused (RST).
# With -Stall it instead accepts on 8080 and withholds response headers for 15 s.
function New-AuthFixtureJob([string] $Name, [switch] $Stall) {
    $code = if ($Stall) {
        "from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer`nimport time`nclass Stall(BaseHTTPRequestHandler):`n    def do_GET(self):`n        time.sleep(15)`n        self.send_response(503)`n        self.end_headers()`n    def log_message(self, *args):`n        pass`nThreadingHTTPServer(('0.0.0.0', 8080), Stall).serve_forever()"
    } else {
        "import socket, time`ns = socket.socket()`ns.bind(('0.0.0.0', 8081))`ns.listen(16)`ntime.sleep(85)"
    }
    $probePort = if ($Stall) { 8080 } else { 8081 }
    $labels = @{ 'app.kubernetes.io/name' = 'auth'; 'app.kubernetes.io/instance' = 'push'
        'push-messenger/network-peer' = 'auth'; 'push-messenger/verification-fixture' = $Name }
    Invoke-FixtureCreate @{ apiVersion = 'batch/v1'; kind = 'Job'
        metadata = @{ name = $Name; namespace = 'push' }
        spec = @{ activeDeadlineSeconds = 90; backoffLimit = 0; template = @{
            metadata = @{ labels = $labels }
            spec = @{ restartPolicy = 'Never'; automountServiceAccountToken = $false
                securityContext = @{ runAsNonRoot = $true; runAsUser = 65532; seccompProfile = @{ type = 'RuntimeDefault' } }
                containers = @(@{ name = 'fixture'; image = 'python:3.13-alpine'
                    command = @('python', '-B', '-u', '-c', $code)
                    ports = @(@{ name = 'http'; containerPort = 8080 }, @{ name = 'probe'; containerPort = 8081 })
                    readinessProbe = @{ tcpSocket = @{ port = $probePort }; periodSeconds = 1 }
                    securityContext = @{ allowPrivilegeEscalation = $false; readOnlyRootFilesystem = $true; capabilities = @{ drop = @('ALL') } } }) } } } }
    kubectl --context $Ctx -n push wait --for=condition=Ready pod -l "push-messenger/verification-fixture=$Name" --timeout=60s | Out-Null
    NativeOK "Ready fixture $Name"
}
function Remove-AuthFixtureJob([string] $Name) {
    kubectl --context $Ctx -n push delete job $Name --ignore-not-found --wait=true --timeout=120s | Out-Null
    NativeOK "Remove only owned fixture $Name"
    kubectl --context $Ctx -n push wait --for=delete pod -l "push-messenger/verification-fixture=$Name" --timeout=120s 2>$null | Out-Null
}
function Restore-Auth([int] $Replicas) {
    kubectl --context $Ctx -n push scale deployment push-auth --replicas=$Replicas | Out-Null
    NativeOK 'Restore fixture auth replicas'
    kubectl --context $Ctx -n push wait --for=condition=Available deployment/push-auth --timeout=180s | Out-Null
    NativeOK 'Restore healthy auth upstream'
    Wait-AuthEndpoints 8080
}
```

`check-ingress.py` (standard library only; never prints tokens, bodies or URLs):

```python
import argparse
import base64
import hashlib
import http.client
import json
import os
import socket
import ssl
import struct
import time
import urllib.parse
import uuid

parser = argparse.ArgumentParser()
parser.add_argument("--connect-host", required=True)
parser.add_argument("--connect-port", required=True, type=int)
parser.add_argument("--ca", required=True)
parser.add_argument("--mailbox-port", type=int, default=18081)
parser.add_argument("--canary", required=True)
parser.add_argument("--links-only", action="store_true")
parser.add_argument("--link-status", type=int, default=200)
args = parser.parse_args()
HOST = "push11b.test"
context = ssl.create_default_context(cafile=args.ca)


def tls(ctx=context, name=HOST):
    raw = socket.create_connection((args.connect_host, args.connect_port), timeout=10)
    try:
        return ctx.wrap_socket(raw, server_hostname=name)
    except BaseException:
        raw.close()
        raise


def request(method, path, body=None, bearer=None, forged=None, mailbox=False):
    conn = http.client.HTTPConnection(
        "127.0.0.1" if mailbox else args.connect_host,
        args.mailbox_port if mailbox else args.connect_port, timeout=15)
    if not mailbox:
        conn.sock = tls()
    headers = {"Host": HOST}
    if bearer:
        headers["Authorization"] = "Bearer " + bearer
    if forged:
        headers.update({"X-Forwarded-For": forged, "X-Real-IP": forged})
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    try:
        conn.request(method, path, data, headers)
        response = conn.getresponse()
        payload = response.read(1024 * 1024 + 1)
        assert len(payload) <= 1024 * 1024
        return response.status, dict(response.getheaders()), payload
    finally:
        conn.close()


def post(path, body, expected, bearer=None, forged=None):
    status, _, payload = request("POST", path, body, bearer, forged)
    assert status == expected, "unexpected API status"
    return json.loads(payload) if payload else {}


def enroll():
    suffix = uuid.uuid4().hex[:10]
    email, password = suffix + "@ingress.test", uuid.uuid4().hex + "Az!7"
    post("/v1/auth/signup", {"email": email, "password": password,
         "handle": "t_" + suffix, "display_name": "Ingress fixture"}, 202)
    deadline, token = time.monotonic() + 30, None
    while time.monotonic() < deadline:
        status, _, payload = request("GET", "/dev/mailbox?to=" +
            urllib.parse.quote(email), mailbox=True)
        assert status == 200
        emails = json.loads(payload).get("emails", [])
        for sent in emails:
            if sent["template"] == "verify_email":
                link = next(line for line in sent["text"].splitlines()
                            if line.startswith("http"))
                token = urllib.parse.parse_qs(
                    urllib.parse.urlsplit(link).query)["token"][0]
        if token:
            break
        time.sleep(0.25)
    assert token, "local verification email not delivered"
    post("/v1/auth/verify", {"token": token}, 200)
    login = post("/v1/auth/login", {"email": email, "password": password}, 200)
    registered = post("/v1/devices", {"name": "Ingress fixture", "platform": "other",
        "registration_id": str(uuid.uuid4())}, 201, login["registration_token"])
    return registered["access_token"]


def varint(value):
    result = bytearray()
    while value > 127:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def blob(number, data):
    return varint(number * 8 + 2) + varint(len(data)) + data


def read_varint(data, offset):
    value = 0
    for shift in range(0, 70, 7):
        byte = data[offset]
        offset += 1
        value |= (byte & 127) << shift
        if byte < 128:
            return value, offset
    raise AssertionError("oversized protobuf varint")


def fields(data):
    offset, result = 0, {}
    while offset < len(data):
        tag, offset = read_varint(data, offset)
        number, wire = tag >> 3, tag & 7
        assert number > 0
        if wire == 0:
            value, offset = read_varint(data, offset)
        elif wire == 2:
            size, offset = read_varint(data, offset)
            assert offset + size <= len(data)
            value, offset = data[offset:offset + size], offset + size
        else:
            raise AssertionError("unexpected fixture protobuf wire type")
        result[number] = value
    return result


def exact(conn, size):
    result = bytearray()
    while len(result) < size:
        part = conn.recv(size - len(result))
        assert part, "unexpected EOF"
        result.extend(part)
    return bytes(result)


def send(conn, opcode, payload):
    mask = os.urandom(4)
    size = len(payload)
    assert size <= 65535
    header = (bytes([128 | opcode, 128 | size]) if size < 126 else
              bytes([128 | opcode, 254]) + struct.pack("!H", size))
    conn.sendall(header + mask + bytes(b ^ mask[i % 4]
                                     for i, b in enumerate(payload)))


def receive(conn):
    fragments = bytearray()
    started = False
    while True:
        a, b = exact(conn, 2)
        assert not (a & 112) and not (b & 128), "unexpected extension/server mask"
        opcode, size = a & 15, b & 127
        if size == 126:
            size = struct.unpack("!H", exact(conn, 2))[0]
        elif size == 127:
            size = struct.unpack("!Q", exact(conn, 8))[0]
        assert size <= 65536 and len(fragments) + size <= 65536
        payload = exact(conn, size)
        if opcode == 8:
            return "close", struct.unpack("!H", payload[:2])[0] if len(payload) >= 2 else 1005
        if opcode == 9:
            send(conn, 10, payload)
            continue
        if opcode == 10:
            continue
        assert opcode == (0 if started else 2), "expected binary protobuf"
        started = True
        fragments.extend(payload)
        if a & 128:
            return "data", bytes(fragments)


def connect(token, forged):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        conn = tls()
        try:
            key = base64.b64encode(os.urandom(16)).decode()
            headers = ("GET /v1/socket HTTP/1.1\r\nHost: " + HOST +
                "\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: " +
                key + "\r\nSec-WebSocket-Version: 13\r\nX-Forwarded-For: " + forged +
                "\r\nX-Real-IP: " + forged + "\r\n\r\n")
            conn.sendall(headers.encode("ascii"))
            response = bytearray()
            while not response.endswith(b"\r\n\r\n"):
                assert len(response) < 16384
                response.extend(exact(conn, 1))
            lines = response.decode("ascii").split("\r\n")
            assert lines[0].split()[1] == "101"
            returned = dict(line.split(":", 1) for line in lines[1:] if ":" in line)
            returned = {k.lower(): v.strip() for k, v in returned.items()}
            accept = base64.b64encode(hashlib.sha1((key +
                "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            assert returned["sec-websocket-accept"] == accept
            send(conn, 2, blob(1, blob(1, token.encode("ascii"))))
            kind, answer = receive(conn)
            if kind == "close" and answer == 4006:
                conn.close()
                time.sleep(1)
                continue
            assert kind == "data" and 32 in fields(answer), "Identify did not produce Ready"
            heartbeat = fields(fields(answer)[32])[3] / 1000
            assert heartbeat == 30, "fixture expects default heartbeat"
            return conn, heartbeat
        except BaseException:
            conn.close()
            raise
    raise AssertionError("device relay not ready within deadline")


def close(conn):
    try:
        send(conn, 8, struct.pack("!H", 1000))
    except OSError:
        pass  # Closing a fixture after a peer close needs no successful write.
    finally:
        conn.close()


APPLICATION_PING = b"\x22\x00"  # Frame.ping = 4, empty Ping.
APPLICATION_PONG = b"\xa2\x02\x00"  # Frame.pong = 36, empty Pong.


def main():
    assert args.canary.startswith("push11b-canary-")
    for path in ("/v1/auth/verify", "/v1/auth/password/reset"):
        status, _, _ = request("GET", path + "?token=" +
                               urllib.parse.quote(args.canary))
        assert status == args.link_status, "unexpected link-route status"
        print("ok   link route status", status)
    if args.links_only:
        return
    with tls():
        pass
    for ctx, name in ((ssl.create_default_context(), HOST), (context, "wrong.test")):
        try:
            with tls(ctx, name):
                raise AssertionError("invalid TLS trust/name accepted")
        except ssl.SSLCertVerificationError:
            pass
    assert request("GET", "/v1/auth/jwks")[0] == 200
    assert request("GET", "/dev/mailbox")[0] == 404
    tokens = [enroll(), enroll()]
    conn, heartbeat = connect(tokens[0], "198.51.100.10")
    active_started = time.monotonic()
    try:
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            time.sleep(min(heartbeat, max(0, deadline - time.monotonic())))
            send(conn, 2, APPLICATION_PING)
            assert receive(conn) == ("data", APPLICATION_PONG), "application Pong missing"
        active_elapsed = time.monotonic() - active_started
    finally:
        close(conn)
    conn, heartbeat = connect(tokens[1], "198.51.100.11")
    try:
        conn.settimeout(100)
        started = time.monotonic()
        assert receive(conn) == ("close", 1008), "not the application idle close"
        elapsed = time.monotonic() - started
        assert heartbeat * 2 <= elapsed <= heartbeat * 3 + 10
    finally:
        close(conn)
    # Exercise each gateway's IP selection without exhausting a per-user quota.
    for index, token in enumerate(tokens):
        conn, _ = connect(token, "198.51.100." + str(10 + index))
        try:
            redeem = blob(1, b"ZZZZZZZZZZ")
            request_body = b"\x08\x01" + blob(12, redeem)
            send(conn, 2, blob(9, request_body))
            kind, payload = receive(conn)
            assert kind == "data"
            error = fields(fields(payload)[35])
            assert error[1] == 18 and error[5] == 1, "expected invite-not-found"
        finally:
            close(conn)
    unknown = uuid.uuid4().hex + "@negative.test"
    for index in range(11):
        status, headers, _ = request("POST", "/v1/auth/login",
            {"email": unknown, "password": "invalid-fixture-password"},
            forged="198.51.100." + str(30 + index))
        assert status == (401 if index < 10 else 429), "spoofed login quota identity"
        if status == 429:
            assert int(next(v for k, v in headers.items()
                            if k.lower() == "retry-after")) > 0
    print("ok   TLS trust/name, WSS, heartbeat and auth spoof guard")
    print("ok   active socket seconds", round(active_elapsed, 1))
    print("ok   idle socket seconds", round(elapsed, 1), "close", 1008)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Never echo request URLs, bodies, headers or exception messages.
        print("FAIL ingress fixture:", type(error).__name__)
        raise SystemExit(1) from None
```

### 3. Controller, images and fixture credentials

```powershell
. 'C:\path\to\private\verify-helpers.ps1'
minikube start -p push11b-traefik --driver=docker --cni=calico --cpus=4 --memory=6g
NativeOK 'Traefik verification profile'
$root = (Get-Location).Path
$dockerVariables = 'DOCKER_TLS_VERIFY', 'DOCKER_HOST', 'DOCKER_CERT_PATH', 'MINIKUBE_ACTIVE_DOCKERD'
$saved = @{}
foreach ($name in $dockerVariables) { $saved[$name] = [Environment]::GetEnvironmentVariable($name) }
try {
    $dockerEnv = minikube -p push11b-traefik docker-env --shell powershell
    NativeOK 'Verification Docker environment'
    $dockerEnv | Out-String | Invoke-Expression
    foreach ($target in 'auth', 'message-svc', 'gateway', 'worker', 'smoke') {
        docker build -f .\deploy\docker\Dockerfile --target $target -t "push-messenger/${target}:dev" $root
        NativeOK "Verification image build $target"
    }
} finally {
    foreach ($name in $dockerVariables) { [Environment]::SetEnvironmentVariable($name, $saved[$name]) }
}
helm repo add traefik https://traefik.github.io/charts
helm upgrade --install edge traefik/traefik --version 42.0.0 --skip-crds `
    --kube-context $Ctx -n edge --create-namespace -f (Join-Path $Private 'traefik-values.yaml') `
    --wait --timeout 5m
NativeOK 'Pinned verification controller'

kubectl --context $Ctx create namespace push
NativeOK 'Create the fresh verification namespace'
Invoke-FixtureCreate (New-FixtureSecret -Name consumer-credentials -Namespace push -Release push -Omit @('resend-api-key'))
$rsa = [Security.Cryptography.RSA]::Create(2048)
$request = [Security.Cryptography.X509Certificates.CertificateRequest]::new(
    'CN=push11b.test', $rsa, [Security.Cryptography.HashAlgorithmName]::SHA256,
    [Security.Cryptography.RSASignaturePadding]::Pkcs1)
$san = [Security.Cryptography.X509Certificates.SubjectAlternativeNameBuilder]::new()
$san.AddDnsName('push11b.test')
$request.CertificateExtensions.Add($san.Build())
# Test-only self-signed trust anchor; never add it to a trust store.
$request.CertificateExtensions.Add(
    [Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new($true, $false, 0, $true))
$cert = $request.CreateSelfSigned([DateTimeOffset]::UtcNow.AddMinutes(-5), [DateTimeOffset]::UtcNow.AddDays(2))
try {
    $pem = $cert.ExportCertificatePem()
    Invoke-FixtureCreate @{ apiVersion = 'v1'; kind = 'Secret'
        metadata = @{ name = 'push-tls'; namespace = 'push' }; type = 'kubernetes.io/tls'
        stringData = @{ 'tls.crt' = $pem; 'tls.key' = $rsa.ExportPkcs8PrivateKeyPem() } }
    [IO.File]::WriteAllText((Join-Path $Private 'push11b-ca.pem'), $pem)
} finally { $cert.Dispose(); $rsa.Dispose(); $pem = $null }

$before = Get-SecretIdentity consumer-credentials push
helm upgrade --install push .\deploy\helm\push-messenger --kube-context $Ctx -n push `
    -f (Join-Path $Private 'app-values.yaml') --wait --timeout 10m
NativeOK 'External-Secret fixture'
helm get manifest push --kube-context $Ctx -n push | Select-String '^kind: Secret' # expect none
$after = Get-SecretIdentity consumer-credentials push # compare Uid, Version, DataHash, Labels, Annotations, Owners
.\deploy\minikube\test-network-policy.ps1 -Context $Ctx
NativeOK 'Traefik fixture internal isolation'
```

Check the controller pod's actual labels before relying on the
`ingressControllerPeers` selectors in `app-values.yaml`.

### 4. TLS, WebSocket and forwarding checks

Run once on a fresh installation: the quota proof requires no prior redemption keys.

```powershell
. 'C:\path\to\private\verify-helpers.ps1'
$Since = (Get-Date).ToUniversalTime().ToString('o')
$Canary = 'push11b-canary-' + [guid]::NewGuid().ToString('N')
$ConnectHost = '127.0.0.1'; $ConnectPort = 18443
$edge = Start-OwnedForward edge svc/edge-traefik 18443 443
$mail = Start-OwnedForward push svc/push-worker 18081 8080
try {
    $RedisPod = kubectl --context $Ctx -n push get pod -l 'app.kubernetes.io/name=redis,app.kubernetes.io/instance=push' `
        -o 'jsonpath={.items[0].metadata.name}'
    NativeOK 'Fixture Redis pod'
    function Get-FixtureRedeemKeys {
        $keys = @(kubectl --context $Ctx -n push exec $RedisPod -- sh -c `
            'export REDISCLI_AUTH="$REDIS_PASSWORD"; exec redis-cli --scan --pattern "gw:redeem:ip:*"')
        NativeOK 'Read fixture rate-limit key inventory'
        @($keys | Where-Object { $_ })
    }
    if (@(Get-FixtureRedeemKeys).Count -ne 0) { throw 'This quota proof requires a fresh fixture; do not flush Redis' }
    Invoke-IngressFixture
    $keys = @(Get-FixtureRedeemKeys)
    if ($keys.Count -ne 1) { throw 'Forged headers split the gateway IP quota' }
    $count = kubectl --context $Ctx -n push exec $RedisPod -- sh -c `
        'export REDISCLI_AUTH="$REDIS_PASSWORD"; exec redis-cli get "$1"' sh $keys[0]
    if ("$count".Trim() -ne '2') { throw 'Both real gateway requests must increment one IP quota' }
    foreach ($forged in '198.51.100.10', '198.51.100.11') {
        $hash = [Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($forged))
        if ($keys[0] -eq ('gw:redeem:ip:' + [Convert]::ToHexString($hash, 0, 16).ToLowerInvariant())) {
            throw 'Gateway accepted a forged address'
        }
    }
} finally { $edge.Kill(); $mail.Kill() }
Assert-LogsClean $Since 'healthy'
```

### 5. Token-safe logging during upstream failures

With the same `$Since` and `$Canary`:

```powershell
$edge = Start-OwnedForward edge svc/edge-traefik 18443 443
$AuthReplicas = [int](kubectl --context $Ctx -n push get deployment push-auth -o 'jsonpath={.spec.replicas}')
$Refuse = 'push11b-refuse-' + [guid]::NewGuid().ToString('N').Substring(0, 10)
$Stall = 'push11b-stall-' + [guid]::NewGuid().ToString('N').Substring(0, 10)
try {
    Wait-RouteStatus 200
    kubectl --context $Ctx -n push scale deployment push-auth --replicas=0 | Out-Null
    NativeOK 'Stop fixture auth'
    kubectl --context $Ctx -n push wait --for=delete pod -l 'app.kubernetes.io/name=auth,app.kubernetes.io/instance=push' --timeout=120s | Out-Null
    Wait-AuthEndpoints 0; Wait-RouteStatus 503
    Invoke-IngressFixture -LinksOnly -LinkStatus 503
    New-AuthFixtureJob $Refuse; Wait-AuthEndpoints 8080; Wait-RouteStatus 502
    Invoke-IngressFixture -LinksOnly -LinkStatus 502
    Remove-AuthFixtureJob $Refuse; Wait-AuthEndpoints 0; Wait-RouteStatus 503
    New-AuthFixtureJob $Stall -Stall; Wait-AuthEndpoints 8080; Wait-RouteStatus 504
    Invoke-IngressFixture -LinksOnly -LinkStatus 504
} finally {
    try { Remove-AuthFixtureJob $Refuse; Remove-AuthFixtureJob $Stall } finally { Restore-Auth $AuthReplicas }
}
Wait-RouteStatus 200
Invoke-IngressFixture -LinksOnly -LinkStatus 200
$edge.Kill()
Assert-LogsClean $Since 'after 503/502/504'
```

`Assert-LogsClean` reads every controller and application pod's logs since the start
in memory, fails on the canary, its encoded form or any `?token=`, and fails if the
controller produced no access entries (empty logs are not proof).

### 6. Missing keys and Secret retention

In a separate namespace `push-disposable` with release `disp` and
`disp-values.yaml`: create the Secret with `New-FixtureSecret ... -Omit
@('resend-api-key', 'resend-webhook-secret')`, then install with `--wait --timeout
150s` and require failure with the worker's `CreateContainerConfigError` naming the
key; `replace` the in-memory Secret with the key added and require `helm upgrade
--wait` to succeed. Repeat with `--set worker.email.webhook.enabled=true` for
`resend-webhook-secret`; disable it again and require the reference to disappear while
`Get-SecretIdentity` stays equal. Remove `database-url`, `rollout restart` message-svc
and require the new pod's `CreateContainerConfigError` while the old pod keeps serving;
restore it. Finally `helm uninstall disp` and require `Get-SecretIdentity` unchanged.

### 7. Finish

Kill owned port-forwards by their process objects, delete only uniquely named fixture
Jobs (the helpers do), and stop the profile with its data retained:

```powershell
minikube stop -p push11b-traefik
```
