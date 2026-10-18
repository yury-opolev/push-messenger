<#
.SYNOPSIS
DEV ONLY: rehearses the chart's Entra ID datastore mode on a local minikube profile,
without Azure (Plan 12B).

.DESCRIPTION
On its own minikube profile with Calico (never the shared "push" profile), it builds the
images, starts TLS-only PostgreSQL and Redis that accept only passwords, and a DEV ONLY
stand-in of Microsoft Entra ID (push-dev-entra issuer, over HTTPS with a test CA) that
makes every token it issues the principal's datastore password. It then installs the
chart with values-minikube.yaml and values-entra-rehearsal.yaml: the chart's own
service-account token projection (injection chart), the migration hook Job and the
egress NetworkPolicies. It proves:

- the smoke test passes (helm test);
- no datastore credential is in the release: no database, Redis or password key in its
  Secret, no PUSH_DATABASE_URL, PUSH_AUTH_DATABASE_URL or credential-bearing Redis URL in
  any pod, and the services run as non-owners of the migrated databases;
- traffic keeps flowing over many token rotations (60 s tokens, push-load for -LoadSeconds);
- killed Redis connections come back (CLIENT KILL USER, then helm test);
- restarted pods connect with fresh tokens (rollout restart, then helm test).

Every kubectl and helm call names the profile's context; the current kubectl context is
set back to "push" at the end (minikube start switches it). Without -Keep the profile is
stopped (not deleted) at the end. Test certificates, accounts and push-load state live in
a private directory under the user's local application data, outside every repository,
and are deleted at the end; no token or password is printed.

.EXAMPLE
deploy/minikube/entra-rehearsal.ps1
.EXAMPLE
deploy/minikube/entra-rehearsal.ps1 -SkipBuild -Keep
#>
[CmdletBinding()]
param(
    [ValidatePattern('^[a-z0-9][-a-z0-9]*$')]
    [string] $Cluster = 'push12b-entra',
    [switch] $SkipBuild,
    # Leave the profile running afterwards (e.g. for test-network-policy.ps1 -Egress).
    [switch] $Keep,
    [ValidateRange(60, 1500)]
    [int] $LoadSeconds = 280,
    [int] $Cpus = 4,
    [string] $Memory = '6g'
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
if ($Cluster -eq 'push') {
    throw 'The rehearsal never runs on the shared "push" profile: pass another -Cluster.'
}
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$chart = Join-Path $root 'deploy\helm\push-messenger'
# Outside every Git working tree, as push-load requires for its credential files.
$work = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) "push-messenger\entra-rehearsal-$Cluster"
$namespace = 'push'
$dsNamespace = 'push-ds'
$release = 'push'
$k = @('--context', $Cluster)
$clients = [ordered]@{
    'auth' = '0a000000-0000-4000-8000-000000000001'
    'message-svc' = '0a000000-0000-4000-8000-000000000002'
    'gateway' = '0a000000-0000-4000-8000-000000000003'
    'worker' = '0a000000-0000-4000-8000-000000000004'
    'migrate' = '0a000000-0000-4000-8000-000000000005'
}
# The Redis ACL users (object ids) of the three Redis clients.
$redisUsers = [ordered]@{
    'auth' = '0b000000-0000-4000-8000-000000000001'
    'message-svc' = '0b000000-0000-4000-8000-000000000002'
    'gateway' = '0b000000-0000-4000-8000-000000000003'
}
$summary = [System.Collections.Generic.List[string]]::new()
$portForwards = [System.Collections.Generic.List[System.Diagnostics.Process]]::new()

function Invoke-Step([string] $What, [scriptblock] $Command) {
    Write-Host "==> $What" -ForegroundColor Cyan
    $started = Get-Date
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$What failed (exit code $LASTEXITCODE)"
    }
    $script:summary.Add(('{0} ({1:n0} s)' -f $What, ((Get-Date) - $started).TotalSeconds))
}

function Restore-Context {
    $contexts = kubectl config get-contexts -o name
    if ($contexts -contains 'push') {
        kubectl config use-context push | Out-Null
    }
}

# Waits until exactly one Ready worker pod remains: the DEV ONLY mailbox is in memory.
function Wait-OneWorker {
    $selector = "app.kubernetes.io/name=worker,app.kubernetes.io/instance=$release"
    $deadline = (Get-Date).AddSeconds(120)
    while (@(kubectl @k -n $namespace get pods -l $selector -o name).Count -ne 1) {
        if ((Get-Date) -gt $deadline) { throw 'expected exactly one worker pod' }
        Start-Sleep -Seconds 2
    }
    kubectl @k -n $namespace wait --for=condition=Ready pod -l $selector --timeout=120s | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'the worker pod is not Ready' }
}

# After a rollout: waits until each application Deployment has one pod left, then for one
# token refresh (45 s). The stand-in issuer can only keep a principal's latest token as its
# PostgreSQL password (one password per role), so an old pod's last refresh would lock its
# successor out of new connections until the successor refreshes; Azure accepts every
# unexpired token and needs no such wait.
function Wait-Settled([string] $Why) {
    Write-Host "==> let the rollout settle ($Why)" -ForegroundColor Cyan
    foreach ($component in 'auth', 'message-svc', 'gateway', 'worker') {
        $selector = "app.kubernetes.io/name=$component,app.kubernetes.io/instance=$release"
        $deadline = (Get-Date).AddSeconds(180)
        while (@(kubectl @k -n $namespace get pods -l $selector -o name).Count -ne 1) {
            if ((Get-Date) -gt $deadline) { throw "expected exactly one $component pod" }
            Start-Sleep -Seconds 2
        }
    }
    Start-Sleep -Seconds 50
}

function Invoke-Smoke([string] $Why) {
    Wait-OneWorker
    Invoke-Step "smoke test ($Why)" {
        helm test $release --kube-context $Cluster --namespace $namespace --logs --timeout 6m
    }
}

function Start-PortForward([string[]] $Arguments) {
    $process = Start-Process kubectl -ArgumentList ($k + $Arguments) -PassThru -WindowStyle Hidden
    $script:portForwards.Add($process)
}

try {
    # ---- 1. The profile, with Calico for NetworkPolicy enforcement ----
    $state = minikube status -p $Cluster --format '{{.Host}}/{{.Kubelet}}/{{.APIServer}}' 2>$null
    if ($state -ne 'Running/Running/Running') {
        Invoke-Step "start minikube profile '$Cluster' (Calico)" {
            minikube start -p $Cluster --driver=docker --cni=calico --cpus=$Cpus --memory=$Memory
        }
        Restore-Context
    }
    Invoke-Step 'wait for Calico' {
        kubectl @k -n kube-system rollout status daemonset/calico-node --timeout=300s
    }

    # ---- 2. The images (the services, and the DEV ONLY smoke image with push-dev-entra) ----
    if (-not $SkipBuild) {
        Invoke-Step 'build the images into the profile' {
            & (Join-Path $PSScriptRoot 'up.ps1') -Cluster $Cluster -SkipInstall -Cpus $Cpus -Memory $Memory
            $global:LASTEXITCODE = 0
        }
    } else {
        Invoke-Step 'enable the ingress addon' { minikube -p $Cluster addons enable ingress }
    }
    Restore-Context

    # ---- 3. Test CA, certificates and the administrator password (DEV ONLY) ----
    if (Test-Path $work) { Remove-Item -Recurse -Force $work }
    New-Item -ItemType Directory -Force $work | Out-Null
    $certs = Join-Path $work 'certs'
    Invoke-Step 'create a DEV ONLY test CA and certificate' {
        cargo run -q --locked -p push-smoke --example dev-entra-certs -- $certs `
            push-ds-postgres.push-ds.svc push-ds-redis.push-ds.svc push-ds-issuer.push-ds.svc localhost 127.0.0.1
    }
    $passwordFile = Join-Path $work 'admin-password'
    $alphabet = [char[]] 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789'
    $bytes = [System.Security.Cryptography.RandomNumberGenerator]::GetBytes(32)
    [IO.File]::WriteAllText($passwordFile, -join ($bytes | ForEach-Object { $alphabet[$_ % $alphabet.Length] }))
    foreach ($ns in $dsNamespace, $namespace) {
        Invoke-Step "namespace $ns" {
            kubectl @k create namespace $ns --dry-run=client -o yaml | kubectl @k apply -f - | Out-Null
        }
    }
    Invoke-Step 'Secrets push-ds-tls and push-ds-admin, ConfigMap push-ds-ca' {
        kubectl @k -n $dsNamespace create secret generic push-ds-tls `
            --from-file=ca.crt=$certs\ca.crt --from-file=server.crt=$certs\server.crt `
            --from-file=server.key=$certs\server.key --dry-run=client -o yaml | kubectl @k apply -f - | Out-Null
        if ($LASTEXITCODE -ne 0) { return }
        kubectl @k -n $dsNamespace create secret generic push-ds-admin --from-file=password=$passwordFile `
            --dry-run=client -o yaml | kubectl @k apply -f - | Out-Null
        if ($LASTEXITCODE -ne 0) { return }
        kubectl @k -n $namespace create configmap push-ds-ca --from-file=ca.crt=$certs\ca.crt `
            --dry-run=client -o yaml | kubectl @k apply -f - | Out-Null
    }
    Remove-Item -Force $passwordFile

    # ---- 4. The datastores and the issuer (restarted: new certificates and password) ----
    Invoke-Step 'apply the DEV ONLY datastores and issuer' {
        kubectl @k apply -f (Join-Path $PSScriptRoot 'entra\datastores.yaml')
    }
    foreach ($name in 'push-ds-postgres', 'push-ds-redis', 'push-ds-issuer') {
        Invoke-Step "restart and wait for $name" {
            kubectl @k -n $dsNamespace rollout restart deployment/$name | Out-Null
            kubectl @k -n $dsNamespace rollout status deployment/$name --timeout=300s
        }
    }

    # ---- 5. The chart in Entra mode; the migration Job runs first ----
    $buildId = Get-Date -Format 'yyyyMMddHHmmss'
    $installed = $false
    for ($attempt = 1; $attempt -le 3 -and -not $installed; $attempt++) {
        Write-Host "==> install the chart in Entra ID mode (attempt $attempt of 3)" -ForegroundColor Cyan
        helm upgrade --install $release $chart --kube-context $Cluster --namespace $namespace `
            -f (Join-Path $chart 'values-minikube.yaml') -f (Join-Path $PSScriptRoot 'values-entra-rehearsal.yaml') `
            --set "buildId=$buildId" --wait --timeout 10m
        if ($LASTEXITCODE -eq 0) { $installed = $true } elseif ($attempt -lt 3) { Start-Sleep -Seconds 10 }
    }
    if (-not $installed) { throw 'the chart did not install' }
    $summary.Add('chart installed in Entra ID mode (migration hook Job succeeded)')

    # ---- 6. Smoke test ----
    Wait-Settled 'install'
    Invoke-Smoke 'after install'

    # ---- 7. No datastore credential anywhere in the release ----
    Write-Host '==> check that no datastore credential exists in the release' -ForegroundColor Cyan
    $secret = kubectl @k -n $namespace get secret "$release-secrets" -o json | ConvertFrom-Json
    $keys = @($secret.data.PSObject.Properties.Name)
    $bad = @($keys | Where-Object { $_ -match 'database|redis|password' })
    if ($bad) { throw "datastore keys in the Secret: $($bad -join ', ')" }
    $pods = (kubectl @k -n $namespace get pods -o json | ConvertFrom-Json).items
    foreach ($pod in $pods) {
        foreach ($container in @($pod.spec.containers) + @($pod.spec.initContainers | Where-Object { $_ })) {
            foreach ($variable in @($container.env | Where-Object { $_ })) {
                if ($variable.name -match 'PASSWORD' -or $variable.name -in 'PUSH_DATABASE_URL', 'PUSH_AUTH_DATABASE_URL' -or
                    ($variable.name -eq 'PUSH_REDIS_URL' -and "$($variable.value)" -match '@')) {
                    throw "pod $($pod.metadata.name) has $($variable.name)"
                }
            }
        }
    }
    $summary.Add("no datastore credential: Secret keys $($keys.Count) (application keys only), $($pods.Count) pods checked")
    $owners = kubectl @k -n $dsNamespace exec deploy/push-ds-postgres -- psql -X -U postgres -d postgres -Atc `
        "SELECT string_agg(datname || '=' || pg_get_userbyid(datdba), ',' ORDER BY datname) FROM pg_database WHERE datname LIKE 'push_%'"
    if ($LASTEXITCODE -ne 0 -or "$owners".Trim() -ne 'push_auth=push-migrator-id,push_messaging=push-migrator-id,push_worker=push-migrator-id') {
        throw "unexpected database owners: $owners"
    }
    $schemas = foreach ($database in 'push_auth', 'push_messaging', 'push_worker') {
        kubectl @k -n $dsNamespace exec deploy/push-ds-postgres -- psql -X -U postgres -d $database -Atc `
            "SELECT string_agg(nspname || '=' || pg_get_userbyid(nspowner), ',') FROM pg_namespace WHERE nspname IN ('auth', 'messaging', 'worker')"
    }
    if (($schemas -join ',') -ne 'auth=push-migrator-id,messaging=push-migrator-id,worker=push-migrator-id') {
        throw "unexpected schema owners: $($schemas -join ',')"
    }
    $summary.Add('the migration principal owns the databases and schemas; the services run as non-owners')

    # ---- 8. Traffic over many token rotations ----
    Start-PortForward @('-n', 'ingress-nginx', 'port-forward', 'svc/ingress-nginx-controller', '28080:80')
    Start-PortForward @('-n', $namespace, 'port-forward', "deploy/$release-worker", '28081:8080')
    Start-Sleep -Seconds 5
    $accounts = Join-Path $work 'accounts.jsonl'
    $state = Join-Path $work 'state.json'
    $report = Join-Path $work 'report.json'
    Invoke-Step 'DEV ONLY accounts' {
        cargo run -q --locked -p push-smoke --bin push-dev-accounts -- --auth http://127.0.0.1:28080 `
            --host push.local --mailbox http://127.0.0.1:28081 --count 2 --out $accounts
    }
    Invoke-Step 'push-load prepare' {
        cargo run -q --locked -p push-load -- prepare --target http://push.local:28080 --authorized-target push.local:28080 `
            --connect-to 127.0.0.1 --accounts $accounts --state $state --devices-per-account 2 --sample-pairs 1
    }
    $issuedBefore = @(kubectl @k -n $dsNamespace logs deploy/push-ds-issuer | Select-String 'issued a token').Count
    Invoke-Step "push-load run for $LoadSeconds s (60 s tokens)" {
        cargo run -q --locked -p push-load -- run --target http://push.local:28080 --authorized-target push.local:28080 `
            --connect-to 127.0.0.1 --state $state --connections 4 --ramp-per-second 4 `
            --duration-seconds $LoadSeconds --message-interval-seconds 5 --no-reconnect `
            --target-build "entra rehearsal $buildId" --report $report
    }
    $result = Get-Content $report -Raw | ConvertFrom-Json
    if ($result.status -ne 'passed') { throw "push-load ended $($result.status)" }
    $issuedDuring = @(kubectl @k -n $dsNamespace logs deploy/push-ds-issuer | Select-String 'issued a token').Count - $issuedBefore
    $m = $result.messages
    $summary.Add("push-load: $($result.status); messages sent $($m.sent), stored $($m.stored), delivered $($m.delivered), lost $($m.lost); $issuedDuring tokens issued during the run")
    foreach ($process in $portForwards) { if (-not $process.HasExited) { Stop-Process -Id $process.Id } }
    $portForwards.Clear()
    $rotations = @(kubectl @k -n $namespace logs "deploy/$release-gateway" | Select-String 'replaced the Redis subscriber for a new token').Count
    $summary.Add("gateway: $rotations make-before-break Redis subscriber replacements for new tokens so far")

    # ---- 9. Killed Redis connections come back ----
    $killedAt = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    $killed = 0
    foreach ($user in $redisUsers.Values) {
        $count = kubectl @k -n $dsNamespace exec deploy/push-ds-redis -- sh -c `
            "REDISCLI_AUTH=`"`$REDIS_PASSWORD`" redis-cli --tls --cacert /tls/ca.crt -h 127.0.0.1 -p 6380 CLIENT KILL USER $user"
        if ($LASTEXITCODE -ne 0) { throw 'CLIENT KILL USER failed' }
        $killed += [int] "$count".Trim()
    }
    if ($killed -lt 3) { throw "expected the services' Redis connections to be killed, killed $killed" }
    $summary.Add("CLIENT KILL USER: $killed Redis connections of auth-svc, message-svc and the gateway killed")
    # The gateway's hub notices at once (it is always reading) and subscribes again.
    $deadline = (Get-Date).AddSeconds(60)
    while (-not (kubectl @k -n $namespace logs "deploy/$release-gateway" --since-time=$killedAt |
            Select-String 'subscribed to Redis')) {
        if ((Get-Date) -gt $deadline) { throw 'the gateway did not subscribe to Redis again within 60 s' }
        Start-Sleep -Seconds 2
    }
    $summary.Add('the gateway hub subscribed to Redis again after the kill')
    # A managed connection notices a server-side kill only when it is next used: that one
    # command fails (auth-svc answers a retryable 503, the gateway refuses that identify,
    # message-svc misses that notification, which clients catch up on at their next sync;
    # Plan 12A's tests accept the same), and the manager then reconnects with the current
    # token. Each smoke run stops at its first failure, so it may take one run per killed
    # connection (four: auth-svc's limits, message-svc's notifier, the gateway's limits and
    # publisher) before a run passes; more is a failure.
    $failures = [System.Collections.Generic.List[string]]::new()
    $recovered = $false
    for ($run = 1; $run -le 5 -and -not $recovered; $run++) {
        Wait-OneWorker
        Write-Host "==> smoke test after killed Redis connections (run $run of 5)" -ForegroundColor Cyan
        $output = helm test $release --kube-context $Cluster --namespace $namespace --logs --timeout 6m 2>&1
        $output | Where-Object { "$_" -match '^smoke: (FAIL|PASS)' } | ForEach-Object { Write-Host $_ }
        if ($LASTEXITCODE -eq 0) {
            $recovered = $true
        } else {
            $failures.Add((@($output | Where-Object { "$_" -match '^smoke: FAIL' }) -join ' ').Trim())
        }
    }
    if (-not $recovered) { throw "the smoke test did not pass after the killed Redis connections: $($failures -join ' | ')" }
    $summary.Add("smoke test after killed Redis connections: passed after $($failures.Count) run(s) that met a killed connection once ($($failures -join ' | '))")

    # ---- 10. Restarted pods connect with fresh tokens ----
    Invoke-Step 'restart every application pod' {
        foreach ($component in 'auth', 'message-svc', 'gateway', 'worker') {
            kubectl @k -n $namespace rollout restart "deployment/$release-$component" | Out-Null
        }
        foreach ($component in 'auth', 'message-svc', 'gateway', 'worker') {
            kubectl @k -n $namespace rollout status "deployment/$release-$component" --timeout=300s
            if ($LASTEXITCODE -ne 0) { return }
        }
    }
    Wait-Settled 'restarts'
    Invoke-Smoke 'after restarts'

    # ---- Issuer evidence ----
    $issuerLog = kubectl @k -n $dsNamespace logs deploy/push-ds-issuer
    foreach ($component in $clients.Keys) {
        $issued = @($issuerLog | Select-String "issued a token for $($clients[$component])").Count
        if ($issued -lt 1) { throw "the issuer issued no token for $component" }
        $summary.Add("issuer: $issued tokens for $component")
    }
    $failed = @($issuerLog | Select-String 'could not make it the password').Count
    if ($failed) { throw "the issuer could not set $failed passwords" }
    Write-Host ''
    Write-Host "PASS: Entra ID rehearsal on '$Cluster'" -ForegroundColor Green
    $summary | ForEach-Object { Write-Host "  - $_" }
} finally {
    foreach ($process in $portForwards) { if (-not $process.HasExited) { Stop-Process -Id $process.Id } }
    if (Test-Path $work) { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }
    if (-not $Keep) {
        minikube stop -p $Cluster | Out-Null
    }
    Restore-Context
    Write-Host "kubectl context: $(kubectl config current-context)"
}
