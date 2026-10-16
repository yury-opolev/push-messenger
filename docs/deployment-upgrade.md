# Data-preserving bundled deployment upgrade (Plan 11A)

This is an **operator-run maintenance procedure**, not a bootstrap script and not a
zero-downtime upgrade. It covers the bundled PostgreSQL 17 layout: bootstrap
administrator `push`, databases `push_messaging`, `push_auth`, `push_worker`, and the
chart-managed kept Secret. Older layouts missing a database or auth/worker
credentials **stop here**: inventory them and design/rehearse a separate migration.
Never repair an upgrade by deleting a volume, database, Secret, release or namespace.
Preserve message history, users, devices, contacts, outbox/queue and migration history.

Consumers own public-traffic maintenance, backups, images, chart delivery and deployment.
Do not change cluster-global ingress controllers, CNI configuration or other releases.
Build/package the new chart and **all matching service images as one coordinated set**
before the window. Old ports/new images and new ports/old images are unsupported.
Registry publication, consumer CI, external-Secret selection and TLS configuration are
not added by this procedure.

**Scope note (Plan 11B charts):** these tested steps describe the transition of the
chart-managed Secret layout. A chart that includes Plan 11B additionally needs explicit
values before the coordinated deployment in phase 6: `worker.email.webhook.enabled: true`
if the installation serves Resend webhooks (a retained key no longer implies the route),
and, with HTTPS chart Ingress, `ingress.tls.secretName`, a controller class/annotations
and trusted client-IP headers. Moving to a consumer-managed Secret is a separate step in
[deployment-configuration.md](deployment-configuration.md).

## 0. Preconditions and explicit target

The examples use **PowerShell 7 on Windows**, kubectl, Helm, Docker and a checked-out
new source tree. Linux/macOS operators need equivalent fail-closed automation,
including a private stdin transport for merge patches. Do not paste these into a shell with tracing/transcription,
command-output capture, or a CI job that logs stdin. All native exits must be checked.
Run each phase deliberately, not as an unattended whole-document script.

Required before proceeding:

- An approved maintenance/data-loss decision maker and a rehearsed restore, including
  matching old chart/images/values/Secret and Redis's ephemeral-state implications.
- A **private, encrypted backup directory outside any Git working tree**, with
  access restricted to the operator; encrypted storage also for the isolated restore
  host's Docker data. Verify encryption/ACLs with your platform's tools. No shared
  artifact store, console dumps, unencrypted scratch disks or public build logs.
  Backups and row snapshots contain personal data and password hashes; a roles dump
  and Secret backup also contain credentials. Apply your retention/disposal policy.
- A PostgreSQL superuser authorized to run the ownership migration, and exclusive
  application maintenance: no external writers, ad-hoc SQL clients, jobs or autoscalers
  that reconnect/restart these four deployments. Suspend only this application's
  public routing **before scaling**; record how to restore it. Keep PostgreSQL running.
  The consumer's maintenance gate must survive Helm reconciliation. A one-time patch
  to chart-managed Ingress backends is not sufficient: Helm can recreate the original
  routes. Keep consumer-managed routes gated independently; phase 6 also disables
  chart-managed Ingresses until validation finishes.
- A NetworkPolicy-enforcing CNI. The new chart enables six ingress policies. Configure
  `networkPolicy.ingressControllerPeers` with the actual controller's **namespace and
  pod label maps in the same peer**; the default empty list denies public traffic.
  Internal peers require same-namespace, same-release `app.kubernetes.io/instance`
  and `push-messenger/network-peer` labels. Restrict who can assign those labels.
  `networkPolicy.enabled: false` is only for equivalent consumer-managed policies;
  it is not a connectivity workaround. Rendered policies alone do not prove enforcement.

Set and verify these values (example names are not target discovery):

```powershell
$ErrorActionPreference = 'Stop'
$Context = 'YOUR-EXPLICIT-CONTEXT'
$Namespace = 'push'
$Release = 'push'
$PgPod = "$Release-postgres-0"        # verify this exact pod belongs to this release
$SecretName = "$Release-secrets"
$PostgresHost = "$Release-postgres"  # bundled Service in this namespace
$Backup = Read-Host 'Absolute PRIVATE ENCRYPTED backup directory outside every Git tree'
if (-not [IO.Path]::IsPathFullyQualified($Backup) -or -not (Test-Path $Backup -PathType Container)) {
    throw 'Prepare and verify the encrypted backup directory first'
}
$ConsumerPrivateValues = Read-Host 'Absolute private values file with the coordinated new image references'
if (-not [IO.Path]::IsPathFullyQualified($ConsumerPrivateValues) -or
    -not (Test-Path $ConsumerPrivateValues -PathType Leaf)) {
    throw 'Prepare consumer values and matching image/chart artifacts first'
}
# Confirm encryption, ACLs and location yourself; existence is not an encryption check.
function NativeOK([string] $operation) {
    if ($LASTEXITCODE -ne 0) { throw "$operation failed (exit $LASTEXITCODE); KEEP MAINTENANCE" }
}
kubectl --context $Context -n $Namespace get pod $PgPod -o wide
NativeOK 'target Postgres pod'
kubectl --context $Context -n $Namespace get deployment "$Release-auth" "$Release-gateway" "$Release-message-svc" "$Release-worker"
NativeOK 'target deployments'
kubectl --context $Context -n $Namespace get pvc
NativeOK 'target PVC inventory'
helm history $Release --kube-context $Context -n $Namespace -o json |
    Set-Content -Encoding utf8 (Join-Path $Backup 'helm-history.json')
NativeOK 'record chart revisions'
# Also retain the exact old chart package, immutable images and private consumer values,
# including the intended ingress.enabled setting for restoring routing after validation.
# Do not print inline values or use `helm get all` in logs.
kubectl --context $Context -n $Namespace get secret $SecretName -o json |
    Set-Content -Encoding utf8 (Join-Path $Backup 'old-secret.json')
NativeOK 'private rollback Secret backup'
```

If a prerequisite fails, **stop**, keep maintenance and investigate. The `up.ps1` /
`up.sh` helpers refuse an unprepared kept Secret or non-plain messaging owner. They
do not generate a missing key, migrate a data-bearing volume, or validate a backup.

### Windows private merge-patch transport

**Rehearsal-discovered compatibility correction:** both installed kubectl 1.36.1
and minikube's 1.31.0 treat `--patch-file=-` as a literal filename, not stdin.
Do not use that command, `/dev/stdin` on Windows, `--patch $secret`, or a plaintext
patch file. This function sends the pipeline's JSON through a unique, current-user-only
Windows named pipe into `kubectl patch --type=merge --patch-file=<pipe>`.
The payload stays in memory; neither arguments nor files contain credentials.
It patches only the explicitly named resource/context, has bounded waits, and kills
only its own child on failure. This is a transport correction, not external-Secret
selection or an automatic upgrade helper.

```powershell
function Invoke-PrivateMergePatch {
    param(
        [Parameter(Mandatory, ValueFromPipeline)][string] $Json,
        [Parameter(Mandatory)][ValidateSet('secret', 'ingress')][string] $Resource,
        [Parameter(Mandatory)][string] $Name
    )
    process {
        if (-not $IsWindows) { throw 'This private named-pipe transport requires Windows' }
        if ($Json.Length -gt 16384) { throw 'Unexpectedly large upgrade patch' }
        $pipeName = 'push-upgrade-' + [guid]::NewGuid().ToString('N')
        $options = [IO.Pipes.PipeOptions]::Asynchronous -bor [IO.Pipes.PipeOptions]::CurrentUserOnly
        $pipe = [IO.Pipes.NamedPipeServerStream]::new($pipeName,
            [IO.Pipes.PipeDirection]::Out, 1, [IO.Pipes.PipeTransmissionMode]::Byte, $options)
        $child = $null
        $bytes = $null
        try {
            $connect = $pipe.WaitForConnectionAsync()
            $info = [Diagnostics.ProcessStartInfo]::new('kubectl')
            $info.UseShellExecute = $false
            $info.RedirectStandardOutput = $true
            $info.RedirectStandardError = $true
            foreach ($argument in @('--context', $Context, '-n', $Namespace,
                'patch', $Resource, $Name, '--type=merge', "--patch-file=\\.\pipe\$pipeName", '-o', 'name')) {
                $info.ArgumentList.Add($argument)
            }
            $child = [Diagnostics.Process]::Start($info)
            $stdout = $child.StandardOutput.ReadToEndAsync()
            $stderr = $child.StandardError.ReadToEndAsync()
            if (-not $connect.Wait(10000)) { throw 'Private patch connection timed out' }
            $bytes = [Text.Encoding]::UTF8.GetBytes($Json)
            if (-not $pipe.WriteAsync($bytes, 0, $bytes.Length).Wait(10000)) {
                throw 'Private patch write timed out'
            }
            $pipe.Dispose()
            if (-not $child.WaitForExit(30000)) { throw 'Private patch request timed out' }
            # Never print a response body or stderr that could quote a rejected patch.
            if ($child.ExitCode -ne 0) { throw "Private merge patch failed (exit $($child.ExitCode)); KEEP MAINTENANCE" }
        } finally {
            $pipe.Dispose()
            if ($child) {
                if (-not $child.HasExited) { $child.Kill($true); $child.WaitForExit() }
                $child.Dispose()
            }
            if ($bytes) { [Array]::Clear($bytes, 0, $bytes.Length) }
            $Json = $bytes = $stdout = $stderr = $null
        }
    }
}
```

## 1. Quiesce and back up

Suspend this application's external routes using the consumer's maintenance process
now. Record and suspend its autoscalers/GitOps reconciliation/other writers. This is
not a request to scale down or reconfigure the shared ingress controller.

```powershell
$Names = @("$Release-auth", "$Release-gateway", "$Release-message-svc", "$Release-worker")
$Replicas = @{}
foreach ($name in $Names) {
    $count = kubectl --context $Context -n $Namespace get deployment $name -o 'jsonpath={.spec.replicas}'
    NativeOK "record replicas of $name"
    $Replicas[$name] = [int]$count
}
$Replicas | ConvertTo-Json | Set-Content (Join-Path $Backup 'replicas.json')
kubectl --context $Context -n $Namespace get pods -l "app.kubernetes.io/instance=$Release" `
    -o 'jsonpath={range .items[*]}{.metadata.name}{" "}{range .status.containerStatuses[*]}{.imageID}{" "}{end}{"\n"}{end}' |
    Set-Content (Join-Path $Backup 'old-image-ids.txt')
NativeOK 'record actual image IDs'
foreach ($name in $Names) {
    kubectl --context $Context -n $Namespace scale deployment $name --replicas=0
    NativeOK "stop $name"
}
foreach ($component in 'auth', 'gateway', 'message-svc', 'worker') {
    kubectl --context $Context -n $Namespace wait --for=delete pod `
        -l "app.kubernetes.io/instance=$Release,app.kubernetes.io/name=$component" --timeout=180s
    NativeOK "wait for $component pods to disappear"
}
# All three databases AND roles/password hashes in one dump. No terminal output.
$Dump = Join-Path $Backup 'cluster.sql'
kubectl --context $Context -n $Namespace exec $PgPod -- `
    pg_dumpall -U push --clean --if-exists |
    Set-Content -Encoding utf8 $Dump
NativeOK 'pg_dumpall'
if (-not (Test-Path $Dump) -or (Get-Item $Dump).Length -eq 0) { throw 'Empty backup' }
```

Confirm there are no other client backends in the application databases before
continuing. The SQL itself refuses other `push_messaging` clients; it cannot prevent
a consumer scheduler/provisioner from reconnecting later.

## 2. Canonical row evidence while all writers are stopped

Save all ordinary/partitioned application tables, including `auth.outbox`, worker
queue/email records and `public._sqlx_migrations`, not just message counts. Run this
before the restore comparison and again after the ownership/Secret phases, **before
any new application pod starts**:

```powershell
$Sql = @'
SELECT format(
  'SELECT %L || row_to_json(t)::text FROM %I.%I AS t ORDER BY row_to_json(t)::text;',
  n.nspname || '.' || c.relname || ':', n.nspname, c.relname)
FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
WHERE n.nspname IN ('messaging','auth','worker','public') AND c.relkind IN ('r','p')
ORDER BY n.nspname, c.relname
\gexec
'@
function Snapshot([string] $phase) {
    foreach ($db in 'push_messaging', 'push_auth', 'push_worker') {
        $file = Join-Path $Backup "$phase-$db.rows"
        $Sql | kubectl --context $Context -n $Namespace exec -i $PgPod -- `
            psql -X -qAt -v ON_ERROR_STOP=1 -U push -d $db |
            Set-Content -Encoding utf8 $file
        NativeOK "snapshot $phase $db"
        if (-not (Test-Path $file) -or (Get-Item $file).Length -eq 0) { throw "Empty $db snapshot" }
    }
}
Snapshot 'before'
# For restore proof, run the same $Sql against each restored database, save under
# restore-$db.rows with the same encoding, then compare hashes, never print row diffs.
function Compare-Snapshots([string] $phase) {
    foreach ($db in 'push_messaging', 'push_auth', 'push_worker') {
        if ((Get-FileHash (Join-Path $Backup "before-$db.rows") -Algorithm SHA256).Hash -ne
            (Get-FileHash (Join-Path $Backup "$phase-$db.rows") -Algorithm SHA256).Hash) {
            throw "$db row evidence differs at $phase; KEEP MAINTENANCE, inspect privately"
        }
    }
}
```

PostgreSQL partition parents and children may read a row twice; the same canonical
ordering remains stable before/after. These are private evidence, not publishable logs.

## 3. Restore proof — mandatory before changing ownership

Use an isolated throwaway PostgreSQL **17** container with no published host ports,
no network, a **unique bootstrap role and database** (so restored roles/databases do
not collide), and encrypted Docker storage. Do not aim the dump at any live database.
Use a consumer-approved offline/preloaded `postgres:17-alpine` image.

```powershell
$Restore = 'push-restore-' + [guid]::NewGuid().ToString('N')
$Bootstrap = 'restore_' + [guid]::NewGuid().ToString('N')
$env:POSTGRES_USER = $Bootstrap
$env:POSTGRES_DB = $Bootstrap
$env:POSTGRES_PASSWORD = [guid]::NewGuid().ToString('N') # throwaway bootstrap only
try {
    docker run -d --pull=never --name $Restore --network none `
        -e POSTGRES_USER -e POSTGRES_DB -e POSTGRES_PASSWORD `
        -e 'POSTGRES_INITDB_ARGS=--auth-host=scram-sha-256 --auth-local=trust' postgres:17-alpine | Out-Null
    NativeOK 'create isolated restore target'
    $ready = $false
    for ($i = 0; $i -lt 60; $i++) {
        docker exec $Restore pg_isready -h 127.0.0.1 -U $Bootstrap -d $Bootstrap *> $null
        if ($LASTEXITCODE -eq 0) { $ready = $true; break }
        Start-Sleep 1
    }
    if (-not $ready) { throw 'Restore target not ready' }
    Get-Content $Dump | docker exec -i $Restore psql -X -v ON_ERROR_STOP=1 -U $Bootstrap -d $Bootstrap `
        *> (Join-Path $Backup 'restore-private.log')
    NativeOK 'restore all databases and roles'
    foreach ($db in 'push_messaging', 'push_auth', 'push_worker') {
        $file = Join-Path $Backup "restore-$db.rows"
        $Sql | docker exec -i $Restore psql -X -qAt -v ON_ERROR_STOP=1 -U push -d $db |
            Set-Content -Encoding utf8 $file
        NativeOK "restored snapshot $db"
        if (-not (Test-Path $file) -or (Get-Item $file).Length -eq 0) { throw "Empty restored $db snapshot" }
    }
    Compare-Snapshots 'restore'
    # Verify that the saved old Secret and restored roles really match.
    # Initdb above forces SCRAM for TCP; local socket trust is not password proof.
    # Strip Windows pipeline CR framing; bundled passwords are ASCII letters/digits.
    $oldSecret = Get-Content (Join-Path $Backup 'old-secret.json') -Raw | ConvertFrom-Json
    $login = 'IFS= read -r PGPASSWORD; PGPASSWORD=$(printf %s "$PGPASSWORD" | tr -d "\r"); export PGPASSWORD; exec psql -X -w -v ON_ERROR_STOP=1 -h 127.0.0.1 -U "$1" -d "$2" -Atc "SELECT current_user"'
    foreach ($case in @(
        @('push', 'push_messaging', 'postgres-password'),
        @('push_auth', 'push_auth', 'auth-db-password'),
        @('push_worker', 'push_worker', 'worker-db-password'),
        @('push_worker', 'push_auth', 'worker-db-password')
    )) {
        $password = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($oldSecret.data.($case[2])))
        $who = $password | docker exec -i $Restore sh -c $login sh $case[0] $case[1]
        NativeOK 'restored old Secret TCP authentication'
        if ("$who".Trim() -ne $case[0]) { throw 'Restored credential identity mismatch' }
        $password = $who = $null
    }
    $bad = [guid]::NewGuid().ToString('N')
    $rejected = $bad | docker exec -i $Restore sh -c $login sh push_auth push_auth 2>&1
    $negativeExit = $LASTEXITCODE
    $rejected | Set-Content (Join-Path $Backup 'restore-negative-password-private.log')
    if ($negativeExit -eq 0 -or "$rejected" -notmatch 'password authentication failed') {
        throw 'Restore negative-password control failed'
    }
    # Also validate owners/grants and your application's recovery procedure.
} finally {
    $oldSecret = $password = $who = $bad = $rejected = $null
    Remove-Item Env:POSTGRES_USER, Env:POSTGRES_DB, Env:POSTGRES_PASSWORD -ErrorAction SilentlyContinue
    # Remove only this named throwaway container, including its anonymous data volume.
    docker rm -fv $Restore *> $null
    NativeOK 'remove isolated restore container'
}
```

A zero restore exit is necessary but not sufficient: all three row hashes must
match. Rehearse starting the matching old application set on a separate isolated
installation as part of your recovery procedure. Keep the real deployment stopped.
Do not proceed until the backup actually recovers your deployment.
Check application database/object owners and worker outbox/cleanup grants explicitly.
The intentionally unique restore bootstrap owns its own system catalogs and may own
recreated extension members; those administrator identities are not application row
data and should not be mistaken for lost application grants.

## 4. Transfer messaging ownership, then provision its password

Read/review the actual checked-in
[`messaging-role.sql`](../deploy/helm/push-messenger/files/postgres/messaging-role.sql).
It is transactional, password-free and idempotent, requires the legacy layout and
superuser `push`, refuses incompatible owners/role attributes/memberships or cross-DB
CONNECT, and transfers only the intended messaging objects and SQLx history. It does
**not** use global `REASSIGN OWNED` or change auth/worker ownership/outbox grants.

From the repository root:

```powershell
Get-Content -Raw deploy\helm\push-messenger\files\postgres\messaging-role.sql |
    kubectl --context $Context -n $Namespace exec -i $PgPod -- `
    psql -X -v ON_ERROR_STOP=1 -U push -d push_messaging
NativeOK 'ownership migration'
# On failure the connection closes and PostgreSQL rolls back. Keep maintenance.
```

Provision a new **ASCII alphanumeric** password through a secure consumer provisioner
or the following interactive psql prompt. Do not put it in command arguments, SQL files,
environment-setting command history, `--set`, or logs. The password is deliberately
not part of the ownership migration:

```powershell
kubectl --context $Context -n $Namespace exec -it $PgPod -- `
    psql -X -v ON_ERROR_STOP=1 -U push -d push_messaging -c '\password push_messaging'
NativeOK 'set messaging password interactively'
```

Use the **same** value in phase 5. Preserve all old bootstrap/auth/worker passwords,
API tokens and signing/link keys.
If consumer values explicitly set `postgres.messagingPassword`, update that private
source to the same new value through your secure provisioner; otherwise leave it
empty so the chart reuses the patched key. A stale explicit override would replace it.

## 5. Transition exactly two keys of the existing kept Secret

This is the current **chart-managed** Secret, not an external-Secret feature.
Patch only `messaging-db-password` and `database-url`, using the private pipeline
transport from phase 0. Never dump all Secret values to the console.

```powershell
$secure = Read-Host 'New password just provisioned for push_messaging (letters/digits)' -AsSecureString
$ptr = [IntPtr]::Zero
try {
    $ptr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    $plain = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr)
    if ($plain -cnotmatch '^[A-Za-z0-9]+$') { throw 'Use ASCII letters and digits only' }
    $url = "postgres://push_messaging:${plain}@${PostgresHost}:5432/push_messaging"
    $patch = @{ stringData = @{ 'messaging-db-password' = $plain; 'database-url' = $url } } |
        ConvertTo-Json -Compress
    $patch | Invoke-PrivateMergePatch -Resource secret -Name $SecretName
    # Read only the two required keys, never echo them, and verify the patch.
    $storedPassword = kubectl --context $Context -n $Namespace get secret $SecretName -o 'jsonpath={.data.messaging-db-password}'
    NativeOK 'verify password key'
    $storedUrl = kubectl --context $Context -n $Namespace get secret $SecretName -o 'jsonpath={.data.database-url}'
    NativeOK 'verify URL key'
    if ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($storedPassword)) -cne $plain -or
        [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($storedUrl)) -cne $url) {
        throw 'Secret verification failed'
    }
} finally {
    if ($ptr -ne [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr) }
    if ($secure) { $secure.Dispose() }
    $plain = $url = $patch = $storedPassword = $storedUrl = $secure = $null
}
```

Verify PostgreSQL and Secret agree using a short-lived credential check pod.
The manifest references only the new password key, never literal credentials.
A local
`psql -U push_messaging` without `-h` is **not** a password test (local trust).
The acceptance rehearsal below tests TCP authentication with the Secret's password.

```powershell
$Check = 'upgrade-credentials-' + [guid]::NewGuid().ToString('N')
$manifest = @{
    apiVersion = 'v1'; kind = 'Pod'
    metadata = @{ name = $Check; namespace = $Namespace; labels = @{
        'app.kubernetes.io/instance' = $Release
        'push-messenger/network-peer' = 'message-svc'
    } }
    spec = @{
        restartPolicy = 'Never'; automountServiceAccountToken = $false
        activeDeadlineSeconds = 90
        containers = @(@{
            name = 'check'; image = 'postgres:17-alpine'
            command = @('psql', '-X', '-w', '-v', 'ON_ERROR_STOP=1', '-h', $PostgresHost,
                '-U', 'push_messaging', '-d', 'push_messaging', '-Atc', 'SELECT current_user')
            env = @(@{ name = 'PGPASSWORD'; valueFrom = @{ secretKeyRef = @{
                name = $SecretName; key = 'messaging-db-password'
            } } })
        })
    }
}
try {
    $manifest | ConvertTo-Json -Depth 12 -Compress |
        kubectl --context $Context -n $Namespace create -f - | Out-Null
    NativeOK 'create credential check'
    kubectl --context $Context -n $Namespace wait --for=jsonpath='{.status.phase}'=Succeeded `
        pod/$Check --timeout=120s
    NativeOK 'credential check completion'
    $who = kubectl --context $Context -n $Namespace logs $Check
    NativeOK 'credential check result'
    if ("$who".Trim() -ne 'push_messaging') { throw 'Unexpected credential identity' }
} finally {
    kubectl --context $Context -n $Namespace delete pod $Check --ignore-not-found --wait=true | Out-Null
    NativeOK 'remove named credential check'
}
$owner = kubectl --context $Context -n $Namespace exec $PgPod -- `
    psql -X -v ON_ERROR_STOP=1 -U push -d postgres -Atc `
    "SELECT count(*) FROM pg_roles r JOIN pg_database d ON d.datdba=r.oid WHERE r.rolname='push_messaging' AND d.datname='push_messaging' AND r.rolcanlogin AND NOT r.rolsuper AND NOT r.rolcreatedb AND NOT r.rolcreaterole AND NOT r.rolreplication AND NOT r.rolbypassrls"
NativeOK 'plain messaging database owner'
if ("$owner".Trim() -ne '1') { throw 'Invalid owner; KEEP MAINTENANCE' }
Snapshot 'after'
Compare-Snapshots 'after'
```

## 6. Coordinated deployment and validation

Keep external routing suspended. Deploy the new chart and matching images using the
consumer's pinned packages/private values and deployment process. **Initially set
all four application replicas to zero** so Helm cannot restart writers before the
snapshot comparisons and credential checks. Also set **`ingress.enabled=false`**
throughout validation, including after restoring application replicas. This removes
the chart-managed routes instead of relying on a backend patch Helm can overwrite.
Consumer-managed Ingress/Gateway routes need their own persistent maintenance gate.
For example (replace paths with prepared consumer artifacts; no secrets on the
command line):

```powershell
helm upgrade $Release .\deploy\helm\push-messenger --kube-context $Context -n $Namespace `
    -f $ConsumerPrivateValues --set auth.replicas=0 --set gateway.replicas=0 `
    --set messageSvc.replicas=0 --set worker.replicas=0 --set ingress.enabled=false `
    --wait --timeout 10m
NativeOK 'deploy coordinated chart and images while stopped'
# Check your pinned image references, three Service ports/endpoints and six policies.
# Ensure Snapshot 'after' / Compare-Snapshots 'after' have passed before starting writers.
function Assert-ChartRoutesClosed {
    $routes = kubectl --context $Context -n $Namespace get ingress `
        -l "app.kubernetes.io/instance=$Release" -o json
    NativeOK 'verify chart-managed routes remain closed'
    if (@(($routes | ConvertFrom-Json).items).Count -ne 0) {
        throw 'Release Ingress routes were recreated; KEEP MAINTENANCE and do not resume traffic'
    }
}
Assert-ChartRoutesClosed
$Replicas = Get-Content (Join-Path $Backup 'replicas.json') -Raw | ConvertFrom-Json -AsHashtable
foreach ($name in $Names) {
    kubectl --context $Context -n $Namespace scale deployment $name --replicas=$($Replicas[$name])
    NativeOK "restore replicas of $name"
}
foreach ($name in $Names) {
    kubectl --context $Context -n $Namespace rollout status deployment/$name --timeout=300s
    NativeOK "ready $name"
}
Assert-ChartRoutesClosed
.\deploy\minikube\test-network-policy.ps1 -Context $Context -Namespace $Namespace -Release $Release
NativeOK 'live allowed/denied internal API matrix'
```

Readiness plus data verification is required **before restoring public routing**.
Use an operator-only validation path that does not admit general public traffic,
with the consumer's normal credentials/client (no signing-key access or DEV ONLY
mailbox): sign in an existing test account, verify its original devices, contacts
and history, send a new uniquely marked message and read it from the peer, and check
worker/outbox progress and normal email delivery. Record outcomes privately. Pending
outbox/queue work may legitimately settle after restarting; do not compare complete
pre/post hashes after writers resume. Verify original rows/history separately.
Restore public routing and the recorded reconciliation/autoscaling settings only
when readiness, history, new-message and policy checks all pass. At that point,
remove the temporary `ingress.enabled=false` override through the consumer's
deployment process, restoring its explicit intended routing setting and retaining
the same verified images and recorded replica counts. Do not reuse the zero-replica
command above to reopen traffic. Persist the original replica counts and intended
routing in your consumer values/reconciliation source too.

For **local test installations only**, `up.ps1 -Cluster <owned-profile> -Smoke`
(or `up.sh --cluster <owned-profile> --smoke`) builds the coordinated new images/chart,
starts the configured replicas, and runs raw-protocol and SDK checks. Its DEV ONLY
mailbox and signing-key tests are not reusable production checks.
Synthetic Windows acceptance fixtures may retain current-user DPAPI-encrypted
backups/evidence locally. That is **test-only**, not a required consumer platform or
a substitute for the private encrypted backup/restore environment in phase 0.

## Rollback checkpoints — not `helm rollback` alone

1. **Before ownership SQL:** with no database changes, restore the recorded old
   replicas and routing after checks. Retain the backup and investigate.
2. **After ownership but before new writes:** finish the coordinated upgrade, or
   restore the **rehearsed** backup together with the matching old Secret, old chart
   and old images under maintenance. Follow your tested restore process; do not
   invent a reverse ownership command against live data.
3. **After new writes:** an old-backup restore loses those writes. Stop writers and
   obtain an explicit data-loss/reconciliation decision; prefer a forward fix.

Helm rollback does **not** undo database ownership, migrations or password changes.
Never run old images against newly changed ownership/schema as an assumed rollback.
Never restore an old backup over new writes without the explicit data-loss decision.
Any failed prerequisite, SQL, password/Secret check, restore comparison, readiness,
history, new-message or network check means **KEEP MAINTENANCE**, not success fallback.
