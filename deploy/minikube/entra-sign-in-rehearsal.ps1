<#
.SYNOPSIS
DEV ONLY: rehearses a Microsoft Entra ID-only installation of the chart on a local
minikube profile, without Entra ID (Plan 13B).

.DESCRIPTION
On its own minikube profile with Calico (never the shared "push" profile), it builds the
images, starts a DEV ONLY stand-in of Entra ID sign-in (push-dev-entra sign-in: Plan 13A's
stub OpenID metadata and key set over HTTPS with a test CA, and a token endpoint that
mints user tokens for anyone), and installs the chart with values-minikube.yaml and
values-entra-sign-in-rehearsal.yaml: auth.signIn.providers entra, auth-svc trusting the
test CA through auth.signIn.entra.caConfigMap, the admin listener, and egress policies
whose only way to the issuer is auth-svc's networkPolicy.egress.entraSignInCidrs (the
issuer pod's /32). It proves:

- the smoke test passes in Entra mode (helm test): login, signup and password reset answer
  404, two SDK apps sign in with Entra ID tokens, register devices, become contacts and
  chat, and signing in again keeps the device;
- no email anywhere: the worker runs with PUSH_EMAIL_SENDER=none, there is no link-page
  Ingress and no Resend key in the Secret;
- the admin API over a port-forward: a wrong token answers 401, an unknown user 404, and
  a user signed in through the ingress is signed out of every device (refresh then
  answers 401); the ingress does not route the admin path;
- on Calico, with real connections: only auth-svc reaches the issuer on 443 (not on its
  token port; the gateway, message-svc and worker not at all), and only the listed admin
  peers reach auth-svc's admin port 8082 (other pods in that namespace, the same labels in
  another namespace and the gateway's identity do not), each denial preceded by a positive
  control;
- no token or admin token reaches auth-svc's log.

Every kubectl and helm call names the profile's context; the current kubectl context is
set back to "push" at the end (minikube start switches it). Without -Keep the profile is
stopped (not deleted) at the end. Test certificates live in a private directory under the
user's local application data and are deleted at the end; no token is printed.

.EXAMPLE
deploy/minikube/entra-sign-in-rehearsal.ps1
.EXAMPLE
deploy/minikube/entra-sign-in-rehearsal.ps1 -SkipBuild -Keep
#>
[CmdletBinding()]
param(
    [ValidatePattern('^[a-z0-9][-a-z0-9]*$')]
    [string] $Cluster = 'push13b-entra',
    [switch] $SkipBuild,
    # Leave the profile running afterwards (e.g. for test-network-policy.ps1 -Egress).
    [switch] $Keep,
    [ValidateNotNullOrEmpty()]
    [string] $ProbeImage = 'python:3.13-alpine',
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
$work = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) "push-messenger\entra-sign-in-rehearsal-$Cluster"
$namespace = 'push'
$idpNamespace = 'push-idp'
$adminNamespace = 'push-admin'
$release = 'push'
$hostName = 'push.local'
# push_auth_core::entra::testing::TENANT: the tenant the issuer's tokens name.
$tenant = '33333333-0000-4000-8000-000000000003'
$k = @('--context', $Cluster)
$summary = [System.Collections.Generic.List[string]]::new()
$portForwards = [System.Collections.Generic.List[System.Diagnostics.Process]]::new()
$probes = [System.Collections.Generic.List[object]]::new()
$suffix = [guid]::NewGuid().ToString('N').Substring(0, 10)
$http = [System.Net.Http.HttpClient]::new()
$http.Timeout = [TimeSpan]::FromSeconds(20)

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

function Start-PortForward([string[]] $Arguments) {
    $process = Start-Process kubectl -ArgumentList ($k + $Arguments) -PassThru -WindowStyle Hidden
    $script:portForwards.Add($process)
}

# One HTTP call; never throws for an HTTP status. Returns the status and the parsed JSON.
function Invoke-Api([string] $Method, [string] $Uri, $Body, [hashtable] $Headers = @{}, [string] $HostHeader) {
    $request = [System.Net.Http.HttpRequestMessage]::new([System.Net.Http.HttpMethod]::new($Method), $Uri)
    if ($HostHeader) { $request.Headers.Host = $HostHeader }
    foreach ($name in $Headers.Keys) { $request.Headers.TryAddWithoutValidation($name, $Headers[$name]) | Out-Null }
    if ($null -ne $Body) {
        $json = $Body | ConvertTo-Json -Depth 6 -Compress
        $request.Content = [System.Net.Http.StringContent]::new($json, [Text.Encoding]::UTF8, 'application/json')
    }
    $response = $script:http.SendAsync($request).GetAwaiter().GetResult()
    $text = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
    $parsed = $null
    if ($text) { try { $parsed = $text | ConvertFrom-Json } catch { $parsed = $null } }
    [pscustomobject]@{ Status = [int] $response.StatusCode; Json = $parsed }
}

# Waits until `$Uri` answers at all (a port-forward is up).
function Wait-Http([string] $Uri, [string] $HostHeader) {
    $deadline = (Get-Date).AddSeconds(30)
    while ($true) {
        try { Invoke-Api 'GET' $Uri $null @{} $HostHeader | Out-Null; return } catch { }
        if ((Get-Date) -gt $deadline) { throw "no answer from $Uri" }
        Start-Sleep -Seconds 1
    }
}

# A disposable, unprivileged probe pod (deploy/minikube/test-network-policy.ps1's kind).
function New-ProbePod([string] $Name, [string] $Namespace, [hashtable] $Labels, [bool] $NeverReady) {
    $container = @{
        name = 'probe'; image = $ProbeImage
        command = @('python', '-B', '-c', 'import time; time.sleep(900)')
        securityContext = @{
            allowPrivilegeEscalation = $false; readOnlyRootFilesystem = $true
            capabilities = @{ drop = @('ALL') }
        }
        resources = @{ requests = @{ cpu = '10m'; memory = '16Mi' }; limits = @{ cpu = '100m'; memory = '64Mi' } }
    }
    if ($NeverReady) {
        # Carries a component's labels so its policy selects it; never a ready endpoint.
        $container.readinessProbe = @{ exec = @{ command = @('python', '-c', 'raise SystemExit(1)') }; periodSeconds = 30 }
    }
    $pod = @{
        apiVersion = 'v1'; kind = 'Pod'
        metadata = @{ name = $Name; namespace = $Namespace; labels = $Labels }
        spec = @{
            automountServiceAccountToken = $false; restartPolicy = 'Never'
            activeDeadlineSeconds = 1200; terminationGracePeriodSeconds = 0
            securityContext = @{ runAsNonRoot = $true; runAsUser = 65532; runAsGroup = 65532; seccompProfile = @{ type = 'RuntimeDefault' } }
            containers = @($container)
        }
    }
    $script:probes.Add(@{ Name = $Name; Namespace = $Namespace })
    $pod | ConvertTo-Json -Depth 12 -Compress | kubectl @k create -f - --request-timeout=30s | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "could not create probe pod $Name" }
}

$connect = @'
import socket
import sys

host, port, expectation = sys.argv[1], int(sys.argv[2]), sys.argv[3]
try:
    with socket.create_connection((host, port), timeout=2):
        pass
except TimeoutError:
    sys.exit(0 if expectation == "deny" else 3)
except OSError:
    sys.exit(4)
else:
    sys.exit(0 if expectation == "allow" else 5)
'@

# A real TCP connection from a probe pod. A timeout is a denial (only after a positive
# control); a refusal or DNS error is an infrastructure failure, never a pass.
function Test-Connection([string] $From, [hashtable] $Probe, [string] $TargetHost, [int] $Port, [string] $Expectation) {
    for ($attempt = 1; $attempt -le 5; $attempt++) {
        kubectl @k -n $Probe.Namespace exec $Probe.Name --request-timeout=15s -- python -B -c $connect $TargetHost "$Port" $Expectation
        $code = $LASTEXITCODE
        if ($code -eq 0) {
            $script:calico.Add("$Expectation  $From -> ${TargetHost}:$Port")
            return
        }
        if ($code -notin 3, 5) {
            throw "infrastructure failure: $From -> ${TargetHost}:$Port expect=$Expectation exit=$code (4=socket or DNS error)"
        }
        if ($attempt -eq 5) {
            throw "policy mismatch: $From -> ${TargetHost}:$Port expect=$Expectation exit=$code (3=timed out; 5=forbidden connection succeeded)"
        }
        Start-Sleep -Seconds 2
    }
}

$calico = [System.Collections.Generic.List[string]]::new()
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
    Invoke-Step 'wait for ingress-nginx' {
        kubectl @k -n ingress-nginx rollout status deployment/ingress-nginx-controller --timeout=300s
    }

    # ---- 3. The issuer: test CA and certificate (DEV ONLY), Secret, ConfigMap ----
    if (Test-Path $work) { Remove-Item -Recurse -Force $work }
    New-Item -ItemType Directory -Force $work | Out-Null
    $certs = Join-Path $work 'certs'
    Invoke-Step 'create a DEV ONLY test CA and certificate' {
        cargo run -q --locked -p push-smoke --example dev-entra-certs -- $certs "push-idp.$idpNamespace.svc"
    }
    foreach ($ns in $idpNamespace, $namespace, $adminNamespace) {
        Invoke-Step "namespace $ns" {
            kubectl @k create namespace $ns --dry-run=client -o yaml | kubectl @k apply -f - | Out-Null
        }
    }
    Invoke-Step 'Secret push-idp-tls and ConfigMap push-idp-ca' {
        kubectl @k -n $idpNamespace create secret generic push-idp-tls --from-file=server.crt=$certs\server.crt `
            --from-file=server.key=$certs\server.key --dry-run=client -o yaml | kubectl @k apply -f - | Out-Null
        if ($LASTEXITCODE -ne 0) { return }
        kubectl @k -n $namespace create configmap push-idp-ca --from-file=ca.crt=$certs\ca.crt `
            --dry-run=client -o yaml | kubectl @k apply -f - | Out-Null
    }
    Invoke-Step 'apply the DEV ONLY sign-in issuer' {
        kubectl @k apply -f (Join-Path $PSScriptRoot 'entra\sign-in-issuer.yaml')
    }
    Invoke-Step 'restart and wait for the issuer (new certificate)' {
        kubectl @k -n $idpNamespace rollout restart deployment/push-idp | Out-Null
        kubectl @k -n $idpNamespace rollout status deployment/push-idp --timeout=300s
    }
    $issuerPod = (kubectl @k -n $idpNamespace get pods -l app.kubernetes.io/name=push-idp `
        --field-selector=status.phase=Running -o json | ConvertFrom-Json).items |
        Where-Object { -not $_.metadata.deletionTimestamp } | Select-Object -First 1
    $issuerIp = [string] $issuerPod.status.podIP
    if (-not $issuerIp) { throw 'the issuer pod has no IP' }
    $summary.Add("issuer pod $($issuerPod.metadata.name) at $issuerIp (auth-svc's entraSignInCidrs: $issuerIp/32)")

    # ---- 4. The chart, Entra ID only ----
    $buildId = Get-Date -Format 'yyyyMMddHHmmss'
    $installed = $false
    for ($attempt = 1; $attempt -le 3 -and -not $installed; $attempt++) {
        Write-Host "==> install the chart, Entra ID only (attempt $attempt of 3)" -ForegroundColor Cyan
        helm upgrade --install $release $chart --kube-context $Cluster --namespace $namespace `
            -f (Join-Path $chart 'values-minikube.yaml') -f (Join-Path $PSScriptRoot 'values-entra-sign-in-rehearsal.yaml') `
            --set "networkPolicy.egress.entraSignInCidrs={$issuerIp/32}" `
            --set "buildId=$buildId" --wait --timeout 10m
        if ($LASTEXITCODE -eq 0) { $installed = $true } elseif ($attempt -lt 3) { Start-Sleep -Seconds 10 }
    }
    if (-not $installed) { throw 'the chart did not install' }
    $summary.Add('chart installed: auth.signIn.providers entra, admin listener, egress policies')

    # ---- 5. The smoke test in Entra mode ----
    $workerSelector = "app.kubernetes.io/name=worker,app.kubernetes.io/instance=$release"
    $deadline = (Get-Date).AddSeconds(120)
    while (@(kubectl @k -n $namespace get pods -l $workerSelector -o name).Count -ne 1) {
        if ((Get-Date) -gt $deadline) { throw 'expected exactly one worker pod' }
        Start-Sleep -Seconds 2
    }
    Write-Host '==> smoke test (Entra mode)' -ForegroundColor Cyan
    $started = Get-Date
    $smoke = helm test $release --kube-context $Cluster --namespace $namespace --logs --timeout 6m 2>&1
    $smokeExit = $LASTEXITCODE
    $smoke | ForEach-Object { Write-Host $_ }
    if ($smokeExit -ne 0) { throw "the smoke test failed (exit code $smokeExit)" }
    $steps = @($smoke | Where-Object { "$_" -match '^smoke: ' } | ForEach-Object { "$_" -replace '^smoke: ', '' })
    foreach ($wanted in 'email and password sign-in is off', 'erin signed in with Entra ID and connected',
        'finn signed in with Entra ID and connected', 'became contacts by invite and chatted',
        'the same device', 'PASS') {
        if (-not ($steps | Where-Object { $_ -match [regex]::Escape($wanted) })) { throw "the smoke log lacks '$wanted'" }
    }
    $summary.Add(('smoke test, Entra mode ({0:n0} s): {1}' -f ((Get-Date) - $started).TotalSeconds, ($steps -join ' | ')))

    # ---- 6. No email anywhere; Entra-only configuration ----
    Write-Host '==> check the Entra-only configuration' -ForegroundColor Cyan
    $deployments = @{}
    foreach ($component in 'auth', 'worker') {
        $deployments[$component] = kubectl @k -n $namespace get deployment "$release-$component" -o json | ConvertFrom-Json
    }
    $authEnv = $deployments['auth'].spec.template.spec.containers[0].env
    $workerEnv = $deployments['worker'].spec.template.spec.containers[0].env
    if (($authEnv | Where-Object name -eq 'PUSH_AUTH_PROVIDERS').value -ne 'entra') { throw 'auth-svc is not Entra-only' }
    if (($workerEnv | Where-Object name -eq 'PUSH_EMAIL_SENDER').value -ne 'none') { throw 'the worker has an email sender' }
    if (($workerEnv | Where-Object name -eq 'PUSH_DEV_MAILBOX').value -ne '0') { throw 'the worker serves the DEV ONLY mailbox' }
    if ($workerEnv | Where-Object { $_.name -match 'RESEND' }) { throw 'the worker references a Resend key' }
    $ingresses = @((kubectl @k -n $namespace get ingress -o json | ConvertFrom-Json).items | ForEach-Object { $_.metadata.name })
    if ("$release-auth-links" -in $ingresses -or "$release-worker" -in $ingresses) { throw "unexpected Ingresses: $($ingresses -join ', ')" }
    $secret = kubectl @k -n $namespace get secret "$release-secrets" -o json | ConvertFrom-Json
    $keys = @($secret.data.PSObject.Properties.Name)
    if ($keys | Where-Object { $_ -match 'resend' }) { throw 'the Secret holds an email key' }
    if ('auth-admin-token' -notin $keys) { throw 'the Secret has no auth-admin-token' }
    $workerLog = kubectl @k -n $namespace logs "deploy/$release-worker" --tail=-1 | Out-String
    if ($workerLog -notmatch 'PUSH_EMAIL_SENDER=none') { throw 'the worker did not report that email is off' }
    $summary.Add("Entra-only: worker sender none, mailbox 0, no Resend key; Ingresses $($ingresses -join ', '); Secret keys $($keys.Count), no email key")

    # ---- 7. The admin API, over a port-forward only ----
    Write-Host '==> the admin API over a port-forward' -ForegroundColor Cyan
    Start-PortForward @('-n', 'ingress-nginx', 'port-forward', 'svc/ingress-nginx-controller', '28180:80')
    Start-PortForward @('-n', $idpNamespace, 'port-forward', 'svc/push-idp', '28190:8080')
    Start-PortForward @('-n', $namespace, 'port-forward', "deploy/$release-auth", '28182:8082')
    $public = 'http://127.0.0.1:28180'
    $admin = 'http://127.0.0.1:28182/v1/admin/users/revoke-sessions'
    Wait-Http "$public/healthz" $hostName
    Wait-Http 'http://127.0.0.1:28190/' $null
    Wait-Http 'http://127.0.0.1:28182/' $null
    $encoded = kubectl @k -n $namespace get secret "$release-secrets" -o 'jsonpath={.data.auth-admin-token}'
    $adminToken = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($encoded))
    $oid = [guid]::NewGuid().ToString()
    $minted = Invoke-Api 'POST' 'http://127.0.0.1:28190/dev/token' @{ oid = $oid }
    if ($minted.Status -ne 200) { throw "the issuer answered $($minted.Status)" }
    $signIn = Invoke-Api 'POST' "$public/v1/auth/entra" @{ token = $minted.Json.token; handle = "admin_probe_$suffix" } @{} $hostName
    if ($signIn.Status -ne 200) { throw "POST /v1/auth/entra answered $($signIn.Status)" }
    $device = Invoke-Api 'POST' "$public/v1/devices" @{ name = 'rehearsal'; platform = 'other'; registration_id = [guid]::NewGuid().ToString() } `
        @{ Authorization = "Bearer $($signIn.Json.registration_token)" } $hostName
    if ($device.Status -ne 201) { throw "POST /v1/devices answered $($device.Status)" }
    $refresh = @{ refresh_token = $device.Json.refresh_token }
    $refreshed = Invoke-Api 'POST' "$public/v1/auth/refresh" $refresh @{} $hostName
    if ($refreshed.Status -ne 200) { throw "refresh before the revocation answered $($refreshed.Status)" }
    $refresh = @{ refresh_token = $refreshed.Json.refresh_token }
    $selector = @{ entra = @{ tenant_id = $tenant; object_id = $oid } }
    $routed = Invoke-Api 'POST' "$public/v1/admin/users/revoke-sessions" $selector @{ Authorization = "Bearer $adminToken" } $hostName
    if ($routed.Status -ne 404) { throw "the ingress answered $($routed.Status) for the admin path, not 404" }
    $wrong = Invoke-Api 'POST' $admin $selector @{ Authorization = 'Bearer wrong-token-wrong-token-wrong-token-wrong' }
    if ($wrong.Status -ne 401) { throw "a wrong admin token answered $($wrong.Status), not 401" }
    $unknown = Invoke-Api 'POST' $admin @{ handle = "nobody_$suffix" } @{ Authorization = "Bearer $adminToken" }
    if ($unknown.Status -ne 404) { throw "an unknown handle answered $($unknown.Status), not 404" }
    $revoked = Invoke-Api 'POST' $admin $selector @{ Authorization = "Bearer $adminToken" }
    if ($revoked.Status -ne 200 -or $revoked.Json.devices_revoked -ne 1) {
        throw "the revocation answered $($revoked.Status) with $($revoked.Json.devices_revoked) devices"
    }
    $after = Invoke-Api 'POST' "$public/v1/auth/refresh" $refresh @{} $hostName
    if ($after.Status -ne 401) { throw "refresh after the revocation answered $($after.Status), not 401" }
    $summary.Add('admin API (port-forward only): ingress 404 for the admin path, wrong token 401, unknown handle 404, revoke by Entra tenant+object id 200 with devices_revoked 1, refresh then 401')
    foreach ($process in $portForwards) { if (-not $process.HasExited) { Stop-Process -Id $process.Id } }
    $portForwards.Clear()

    $authLog = kubectl @k -n $namespace logs "deploy/$release-auth" --tail=-1 | Out-String
    if ($authLog -match 'eyJ[A-Za-z0-9_-]{10,}\.') { throw 'a JWT reached auth-svc''s log' }
    if ($authLog.Contains($adminToken)) { throw 'the admin token reached auth-svc''s log' }
    # The keys came over HTTPS through the CA bundle and the egress rule; the Entra-only
    # start-up swept password devices (none on a fresh install).
    foreach ($event in 'fetched the Entra ID signing keys', 'auth.password_sessions.revoked', 'auth.entra.provisioned',
        'auth.admin.sessions_revoked', 'auth.admin.token.refused') {
        if (-not $authLog.Contains($event)) { throw "auth-svc logged no $event" }
    }
    $adminToken = $null
    $summary.Add('auth-svc log: signing keys fetched, auth.password_sessions.revoked, auth.entra.provisioned, auth.admin.sessions_revoked, auth.admin.token.refused; no JWT, no admin token')

    # ---- 8. Calico: auth-only egress to the issuer, admin port for its peers only ----
    Write-Host '==> Calico allow/deny probes' -ForegroundColor Cyan
    $instance = @{ 'app.kubernetes.io/instance' = $release }
    $egressProbes = @{}
    foreach ($component in 'auth', 'gateway', 'message-svc', 'worker') {
        $name = "sp-$component-$suffix"
        New-ProbePod $name $namespace ($instance + @{ 'app.kubernetes.io/name' = $component; 'push-messenger/network-peer' = $component }) $true
        $egressProbes[$component] = @{ Name = $name; Namespace = $namespace }
    }
    # No component name: no policy selects these, so only the target's ingress rules decide.
    $control = @{ Name = "sp-control-$suffix"; Namespace = $namespace }
    New-ProbePod $control.Name $namespace ($instance + @{ 'push-messenger/network-peer' = 'rehearsal-control' }) $false
    $gatewayIdentity = @{ Name = "sp-gateway-identity-$suffix"; Namespace = $namespace }
    New-ProbePod $gatewayIdentity.Name $namespace ($instance + @{ 'push-messenger/network-peer' = 'gateway' }) $false
    $adminPeer = @{ Name = "sp-admin-peer-$suffix"; Namespace = $adminNamespace }
    New-ProbePod $adminPeer.Name $adminNamespace @{ 'push-messenger/admin-shell' = 'true' } $false
    $adminOther = @{ Name = "sp-admin-other-$suffix"; Namespace = $adminNamespace }
    New-ProbePod $adminOther.Name $adminNamespace @{ 'push-messenger/admin-shell' = 'false' } $false
    $wrongNamespace = @{ Name = "sp-admin-elsewhere-$suffix"; Namespace = $namespace }
    New-ProbePod $wrongNamespace.Name $namespace @{ 'push-messenger/admin-shell' = 'true' } $false
    foreach ($probe in $probes) {
        kubectl @k -n $probe.Namespace wait --for=jsonpath='{.status.phase}'=Running "pod/$($probe.Name)" --timeout=180s | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "probe pod $($probe.Name) did not start" }
    }
    $issuerHost = "push-idp.$idpNamespace.svc.cluster.local"
    $authPod = (kubectl @k -n $namespace get pods -l "app.kubernetes.io/name=auth,app.kubernetes.io/instance=$release" -o json |
        ConvertFrom-Json).items | Where-Object { $_.status.podIP -and ($_.metadata.labels.'push-messenger/network-peer' -eq 'auth') -and
            ($_.status.conditions | Where-Object { $_.type -eq 'Ready' -and $_.status -eq 'True' }) } | Select-Object -First 1
    $authIp = [string] $authPod.status.podIP
    if (-not $authIp) { throw 'no ready auth-svc pod' }
    $authService = "$release-auth.$namespace.svc.cluster.local"

    # Positive controls: the issuer answers on both ports, and auth-svc on both, from
    # unrestricted probes; only then does a timeout count as a denial.
    Test-Connection 'control (no policy)' $control $issuerHost 443 'allow'
    Test-Connection 'control (no policy)' $control $issuerHost 8080 'allow'
    Test-Connection 'admin peer (push-admin, admin-shell=true)' $adminPeer $authService 8082 'allow'
    Test-Connection 'admin peer (push-admin, admin-shell=true)' $adminPeer $authIp 8082 'allow'
    Test-Connection 'gateway identity' $gatewayIdentity $authService 8080 'allow'
    # Egress: auth-svc alone reaches the Entra ID stand-in, on 443 only.
    Test-Connection 'auth' $egressProbes['auth'] $issuerHost 443 'allow'
    Test-Connection 'auth' $egressProbes['auth'] $issuerHost 8080 'deny'
    foreach ($component in 'gateway', 'message-svc', 'worker') {
        Test-Connection $component $egressProbes[$component] $issuerHost 443 'deny'
    }
    # Ingress: auth-svc's admin port for the listed peers only.
    Test-Connection 'admin peer (push-admin, admin-shell=true)' $adminPeer $authService 8080 'deny'
    Test-Connection 'other pod (push-admin, admin-shell=false)' $adminOther $authService 8082 'deny'
    Test-Connection 'same label, other namespace (push)' $wrongNamespace $authService 8082 'deny'
    Test-Connection 'same label, other namespace (push)' $wrongNamespace $authIp 8082 'deny'
    Test-Connection 'gateway identity' $gatewayIdentity $authService 8082 'deny'
    Test-Connection 'gateway identity' $gatewayIdentity $authIp 8082 'deny'
    $summary.Add("Calico: $($calico.Count) checks matched: $($calico -join '; ')")

    Write-Host ''
    Write-Host "PASS: Entra ID sign-in rehearsal on '$Cluster'" -ForegroundColor Green
    $summary | ForEach-Object { Write-Host "  - $_" }
} finally {
    foreach ($process in $portForwards) { if (-not $process.HasExited) { Stop-Process -Id $process.Id } }
    $adminToken = $null
    foreach ($group in ($probes | Group-Object { $_.Namespace })) {
        kubectl @k -n $group.Name delete pod @($group.Group | ForEach-Object { $_.Name }) --ignore-not-found=true `
            --wait=true --timeout=60s | Out-Null
    }
    if (Test-Path $work) { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }
    $http.Dispose()
    if (-not $Keep) {
        minikube stop -p $Cluster | Out-Null
    }
    Restore-Context
    Write-Host "kubectl context: $(kubectl config current-context)"
}
