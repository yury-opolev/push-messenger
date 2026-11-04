# Runs the chart's manifests through a real API server's validation, which helm template
# (test-chart.ps1) never reaches: kubectl apply --dry-run=server of every scenario below,
# then a real helm upgrade --install (twice: install, upgrade) of internal TURN. CI runs it
# on kind for each Kubernetes version AKS supports (.github/workflows/ci.yml, chart-cluster).
# Requires Helm and kubectl; use a disposable cluster, such as kind:
#
#   kind create cluster --name push-chart
#   pwsh deploy/helm/test-chart-cluster.ps1 -Context kind-push-chart
param(
    [Parameter(Mandatory)] [string] $Context,
    [string] $Namespace = 'push-chart-test'
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
kubectl --context $Context create namespace $Namespace --dry-run=client -o yaml |
    kubectl --context $Context apply -f - | Out-Null

$coturnPeers = '[{"namespaceSelector":{"kubernetes.io/metadata.name":"monitoring"},"podSelector":{"app":"prometheus"}}]'
$turn = @('--set', 'calls.enabled=true', '--set', 'calls.turn.internal.publicHost=turn.example.com',
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
# waiting for pods need the application images, so neither runs here.
$release = @('push', $chart, '--kube-context', $Context, '--namespace', $Namespace, '--no-hooks')
$installed = $scenarios['internal TURN, default denied-peers.txt, dual-stack cluster CIDRs']
try {
    Check 'helm upgrade --install creates the internal TURN release' {
        $result = helm upgrade --install @release @installed 2>&1 | Out-String
        $passed = $LASTEXITCODE -eq 0
        if (-not $passed) { Write-Host $result }
        $passed -and (kubectl --context $Context --namespace $Namespace get networkpolicy push-coturn -o name) -eq
            'networkpolicy.networking.k8s.io/push-coturn'
    }
    Check 'helm upgrade updates it with other deniedPeerCidrs' {
        $result = helm upgrade --install @release @turn `
            --set 'calls.turn.internal.deniedPeerCidrs={10.240.0.0/12,fd00:10:240::/48}' 2>&1 | Out-String
        $passed = $LASTEXITCODE -eq 0
        if (-not $passed) { Write-Host $result }
        $except = kubectl --context $Context --namespace $Namespace get networkpolicy push-coturn `
            -o jsonpath='{.spec.egress[0].to[1].ipBlock.except}'
        $passed -and $except -match 'fd00:10:240::/48' -and $except -notmatch 'ffff'
    }
} finally {
    helm uninstall push --kube-context $Context --namespace $Namespace 2>&1 | Out-Null
}

if ($failures) {
    Write-Output "$failures check(s) failed"
    exit 1
}
Write-Output 'all cluster checks passed'
