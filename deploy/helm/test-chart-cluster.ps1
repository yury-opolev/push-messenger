# Runs the chart's manifests through a real API server's validation, which helm template
# (test-chart.ps1) never reaches: kubectl apply --dry-run=server of every scenario below,
# then a real helm upgrade --install (twice: install, upgrade) of internal TURN in a namespace
# enforcing the restricted Pod Security Standard, waiting for coturn to be Ready and running
# one TURN allocation against it. CI runs it on kind for each Kubernetes version AKS supports
# (.github/workflows/ci.yml, chart-cluster).
# Requires Helm and kubectl; use a disposable cluster, such as kind:
#
#   kind create cluster --name push-chart
#   pwsh deploy/helm/test-chart-cluster.ps1 -Context kind-push-chart
#
# -CoturnImage runs another coturn image (<repository>:<tag>@sha256:<digest>, pullable by the
# cluster), e.g. one built from deploy/docker/coturn.Dockerfile and pushed to a local
# registry, as CI does. -CopyBinary $false installs it without the copy init container
# (calls.turn.internal.copyBinary false: the image's own /opt/coturn/bin/turnserver); the
# upgrade then switches to the init container path, so both run.
param(
    [Parameter(Mandatory)] [string] $Context,
    [string] $Namespace = 'push-chart-test',
    [string] $CoturnImage = 'ghcr.io/yury-opolev/push-coturn:4.18.0-alpine-r2@sha256:8e37911d206a3352c49dd6861ceb19e34af0cfc58043504c56403c7c21534971',
    [bool] $CopyBinary = $true
)
$ErrorActionPreference = 'Stop'
$chart = Join-Path $PSScriptRoot 'push-messenger'
$failures = 0

function Check([string] $name, [scriptblock] $test) {
    if (& $test) {
        Write-Output "ok   $name"
    } else {
        Write-Output "FAIL $name"
        $script:failures++
    }
}

# kubectl apply --dry-run=server of $manifest: the API server validates every object
# (schemas and field rules) without creating it. Returns the output; sets $LASTEXITCODE.
function ServerDryRun([string] $manifest) {
    $manifest | kubectl --context $Context --namespace $Namespace apply --dry-run=server -f - 2>&1 | Out-String
}

$version = kubectl --context $Context version -o json | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw "kubectl cannot reach context $Context" }
Write-Output "API server $($version.serverVersion.gitVersion)"
# Enforces the restricted Pod Security Standard: coturn's pod is refused unless it complies.
kubectl --context $Context create namespace $Namespace --dry-run=client -o yaml |
    kubectl --context $Context apply -f - | Out-Null
kubectl --context $Context label namespace $Namespace --overwrite `
    pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/enforce-version=latest | Out-Null
$coturnImage = $CoturnImage
Write-Output "coturn image $coturnImage, copyBinary $($CopyBinary.ToString().ToLower())"

# coturn's Deployment is rolled out and Ready (helm --wait would also wait for the
# application images, which CI does not build here).
function CoturnReady {
    $result = kubectl --context $Context --namespace $Namespace rollout status deploy/push-coturn --timeout=180s 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) {
        Write-Host $result
        kubectl --context $Context --namespace $Namespace describe pods -l app.kubernetes.io/name=coturn 2>&1 | Write-Host
        kubectl --context $Context --namespace $Namespace logs deploy/push-coturn -c coturn --tail 20 2>&1 | Write-Host
        return $false
    }
    $true
}

# One TURN allocation (and refresh) against the coturn Service with TURN REST credentials
# minted from the chart's shared secret: username "<expiry>:<user>", password
# base64(HMAC-SHA1(secret, username)). turnutils_uclient runs from coturn's own image in a
# restricted pod; -n 0 sends no data, so the peer is never contacted. Returns the exit code
# and the log.
function TurnAllocation([string] $password) {
    $encoded = kubectl --context $Context --namespace $Namespace get secret push-secrets -o jsonpath='{.data.turn-shared-secret}'
    $secret = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($encoded))
    $username = "$([DateTimeOffset]::UtcNow.AddMinutes(10).ToUnixTimeSeconds()):chart-cluster"
    if (-not $password) {
        $hmac = [Security.Cryptography.HMACSHA1]::new([Text.Encoding]::UTF8.GetBytes($secret))
        $password = [Convert]::ToBase64String($hmac.ComputeHash([Text.Encoding]::UTF8.GetBytes($username)))
    }
    $pod = @{
        spec = @{
            securityContext = @{ runAsNonRoot = $true; runAsUser = 65534; runAsGroup = 65534
                seccompProfile = @{ type = 'RuntimeDefault' } }
            containers = @(@{
                name = 'uclient'; image = $coturnImage
                command = @('turnutils_uclient', '-v', '-u', $username, '-w', $password, '-n', '0', '-m', '1',
                    '-e', '1.1.1.1', 'push-coturn')
                securityContext = @{ allowPrivilegeEscalation = $false; readOnlyRootFilesystem = $true
                    capabilities = @{ drop = @('ALL') } }
            })
        }
    } | ConvertTo-Json -Depth 10 -Compress
    kubectl --context $Context --namespace $Namespace delete pod turn-allocation --ignore-not-found --wait | Out-Null
    kubectl --context $Context --namespace $Namespace run turn-allocation --restart=Never `
        --image=$coturnImage --overrides=$pod | Out-Null
    $deadline = (Get-Date).AddSeconds(120)
    do {
        Start-Sleep -Seconds 2
        $phase = kubectl --context $Context --namespace $Namespace get pod turn-allocation -o jsonpath='{.status.phase}'
    } while ($phase -notin 'Succeeded', 'Failed' -and (Get-Date) -lt $deadline)
    $exit = kubectl --context $Context --namespace $Namespace get pod turn-allocation `
        -o jsonpath='{.status.containerStatuses[0].state.terminated.exitCode}'
    $log = kubectl --context $Context --namespace $Namespace logs turn-allocation 2>&1 | Out-String
    kubectl --context $Context --namespace $Namespace delete pod turn-allocation --ignore-not-found --wait=false | Out-Null
    @{ exit = "$exit"; log = $log }
}

$coturnPeers = '[{"namespaceSelector":{"kubernetes.io/metadata.name":"monitoring"},"podSelector":{"app":"prometheus"}}]'
$turn = @('--set', "calls.turn.internal.image=$coturnImage",
    '--set', 'calls.enabled=true', '--set', 'calls.turn.internal.publicHost=turn.example.com',
    '--set', 'calls.turn.internal.externalIp=203.0.113.10')
$scenarios = [ordered]@{
    'default values (calls off)' = @()
    'internal TURN, default denied-peers.txt, dual-stack cluster CIDRs' = $turn + @(
        '--set', 'calls.turn.internal.deniedPeerCidrs={10.244.0.0/16,10.96.0.0/12,fd00:10:244::/56,fd00:10:96::/112}')
    'internal TURN, IPv4 single-stack cluster CIDRs' = $turn + @(
        '--set', 'calls.turn.internal.deniedPeerCidrs={10.244.0.0/16,10.96.0.0/12}')
    'internal TURN, IPv6 single-stack: IPv6 publicHost and cluster CIDRs, ClusterIP' = @(
        '--set', 'calls.enabled=true', '--set', 'calls.turn.internal.publicHost=2001:db8::10',
        '--set', 'calls.turn.internal.service.type=ClusterIP',
        '--set', 'calls.turn.internal.deniedPeerCidrs={fd00:10:244::/56,fd00:10:96::/112}')
    'internal TURN, non-canonical deniedPeerCidrs overrides, TLS, metrics peers' = $turn + @(
        '--set', 'calls.turn.internal.deniedPeerCidrs={10.244.0.0/14,FD00:10:244:0:0:0:0:0/56,fd00:0010:0096::/112,2001:db8:0:0:1::/80}',
        '--set', 'calls.turn.internal.tlsSecret=turn-tls', '--set-json', "calls.turn.internal.metricsPeers=$coturnPeers")
}

# The control: the API server refuses what the coturn NetworkPolicy held before the fix,
# so a passing dry run means something.
Check 'the API server refuses an IPv4-mapped ipBlock except (control)' {
    $refused = ServerDryRun @'
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: ipv4-mapped-control
spec:
  podSelector: {}
  policyTypes: [Egress]
  egress:
    - to:
        - ipBlock:
            cidr: "::/0"
            except: ["::ffff:0:0/96"]
'@
    $passed = $LASTEXITCODE -ne 0 -and $refused -match 'IPv4-mapped'
    if (-not $passed) { Write-Host $refused }
    $passed
}

foreach ($scenario in $scenarios.GetEnumerator()) {
    Check "dry-run=server: $($scenario.Key)" {
        $manifest = helm template push $chart --namespace $Namespace @($scenario.Value) 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { Write-Host $manifest; return $false }
        $result = ServerDryRun $manifest
        # The API server warns about non-canonical CIDRs ("should be in RFC 5952 canonical format").
        $passed = $LASTEXITCODE -eq 0 -and $result -match 'networkpolicy\.networking\.k8s\.io/push-message-svc' -and
            $result -notmatch '(?i)warning:.*CIDR'
        if ($scenario.Value.Count) {
            $passed = $passed -and $result -match 'networkpolicy\.networking\.k8s\.io/push-coturn created \(server dry run\)'
        }
        if (-not $passed) { Write-Host $result }
        $passed
    }
}

# A real install and upgrade, as the DEV deployment runs it; hooks (the migration Job) and
# the application pods need the application images, so neither runs or is waited for here:
# coturn's own rollout is, and one TURN allocation runs against it.
$release = @('push', $chart, '--kube-context', $Context, '--namespace', $Namespace, '--no-hooks')
$installed = $scenarios['internal TURN, default denied-peers.txt, dual-stack cluster CIDRs'] +
    @('--set', "calls.turn.internal.copyBinary=$($CopyBinary.ToString().ToLower())")
try {
    Check 'coturn passes the restricted Pod Security Standard (server dry run, no PodSecurity warning)' {
        $manifest = helm template push $chart --namespace $Namespace @installed --show-only templates/coturn.yaml 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { Write-Host $manifest; return $false }
        $result = ServerDryRun $manifest
        $passed = $LASTEXITCODE -eq 0 -and $result -match 'deployment\.apps/push-coturn created \(server dry run\)' -and
            $result -notmatch 'PodSecurity'
        if (-not $passed) { Write-Host $result }
        $passed
    }
    Check 'helm upgrade --install creates the internal TURN release' {
        $result = helm upgrade --install @release @installed 2>&1 | Out-String
        $passed = $LASTEXITCODE -eq 0
        if (-not $passed) { Write-Host $result }
        $passed -and (kubectl --context $Context --namespace $Namespace get networkpolicy push-coturn -o name) -eq
            'networkpolicy.networking.k8s.io/push-coturn'
    }
    Check 'the coturn pod becomes Ready in the restricted namespace' { CoturnReady }
    Check "the coturn pod runs the chosen image with the expected init containers (copyBinary $($CopyBinary.ToString().ToLower()))" {
        $spec = kubectl --context $Context --namespace $Namespace get pods -l app.kubernetes.io/name=coturn `
            -o jsonpath='{.items[0].spec.initContainers[*].name}|{.items[0].spec.containers[0].image}'
        $expected = "$(if ($CopyBinary) { 'config turnserver' } else { 'config' })|$coturnImage"
        if ($spec -ne $expected) { Write-Host "got '$spec', expected '$expected'" }
        $spec -eq $expected
    }
    Check 'a TURN allocation and refresh against the coturn Service succeed with REST credentials' {
        # The Service's UDP forwarding can lag the pod's readiness by a few seconds
        # ("recv: Connection refused"): a bounded retry.
        for ($attempt = 1; $attempt -le 10; $attempt++) {
            $allocation = TurnAllocation
            $passed = $allocation.exit -eq '0' -and $allocation.log -match 'Received relay addr: 203\.0\.113\.10:' -and
                $allocation.log -match 'refresh response received: \s*\S*\s*INFO success'
            if ($passed) { break }
            Write-Host "allocation attempt $attempt failed (exit code $($allocation.exit))"
            Start-Sleep -Seconds 3
        }
        if (-not $passed) { Write-Host $allocation.log }
        $passed
    }
    Check 'coturn refuses an allocation with a wrong password (control)' {
        $allocation = TurnAllocation 'not-the-hmac'
        $passed = $allocation.exit -ne '0' -and $allocation.log -match 'Cannot complete Allocation'
        if (-not $passed) { Write-Host "exit code $($allocation.exit)"; Write-Host $allocation.log }
        $passed
    }
    Check 'helm upgrade updates it with other deniedPeerCidrs (and the copy init container)' {
        $result = helm upgrade --install @release @turn --set calls.turn.internal.copyBinary=true `
            --set 'calls.turn.internal.deniedPeerCidrs={10.240.0.0/12,fd00:10:240::/48}' 2>&1 | Out-String
        $passed = $LASTEXITCODE -eq 0
        if (-not $passed) { Write-Host $result }
        $except = kubectl --context $Context --namespace $Namespace get networkpolicy push-coturn `
            -o jsonpath='{.spec.egress[0].to[1].ipBlock.except}'
        $passed -and $except -match 'fd00:10:240::/48' -and $except -notmatch 'ffff'
    }
    Check 'the upgraded coturn pod becomes Ready' { CoturnReady }
} finally {
    helm uninstall push --kube-context $Context --namespace $Namespace 2>&1 | Out-Null
}

if ($failures) {
    Write-Output "$failures check(s) failed"
    exit 1
}
Write-Output 'all cluster checks passed'
