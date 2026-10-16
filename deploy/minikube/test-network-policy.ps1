<#
.SYNOPSIS
Verifies the existing message-svc's internal API ingress policy on an enforcing CNI.

.DESCRIPTION
Uses disposable, unprivileged Python pods, never application containers or credentials.
First proves every API is live, then checks the caller/port matrix, unrelated callers
and release isolation. A refusal or exec failure is infrastructure failure, not denial.
Does not install infrastructure or change policies. Every kubectl call uses -Context.

.EXAMPLE
.\deploy\minikube\test-network-policy.ps1 -Context push11a
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $Context,
    [ValidateNotNullOrEmpty()]
    [string] $Namespace = 'push',
    [ValidatePattern('^[a-z0-9]([-a-z0-9]*[a-z0-9])?$')]
    [ValidateLength(1, 53)]
    [string] $Release = 'push',
    [ValidateNotNullOrEmpty()]
    [string] $ProbeImage = 'python:3.13-alpine'
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$kubectlArgs = @('--context', $Context, '--namespace', $Namespace)
$ownedPods = [System.Collections.Generic.List[string]]::new()
$failure = $null
$suffix = [guid]::NewGuid().ToString('N').Substring(0, 12)
$sources = @(
    @{ Peer = 'gateway'; Instance = $Release; Port = 50051 },
    @{ Peer = 'worker'; Instance = $Release; Port = 50052 },
    @{ Peer = 'auth'; Instance = $Release; Port = 50053 },
    @{ Peer = 'unrelated'; Instance = $Release; Port = 0 },
    @{ Peer = 'gateway'; Instance = "other-$suffix"; Port = 0 }
)
$probe = @'
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

function Test-ConnectionPolicy($Source, [int] $Port, [string] $Expectation) {
    $peer = $Source.Peer
    if ($Source.Instance -ne $Release) { $peer = 'cross-release-gateway' }
    Write-Host "peer=$peer target=$targetIP/$Port expect=$Expectation"
    # Each attempt opens a new connection. Retry only mismatched reachability, for at
    # most 5 attempts, to wait for endpoint/policy programming after pod readiness.
    for ($attempt = 1; $attempt -le 5; $attempt++) {
        kubectl @kubectlArgs exec $Source.Name --request-timeout=10s -- `
            python -B -c $probe $targetIP "$Port" $Expectation
        $code = $LASTEXITCODE
        if ($code -eq 0) { return }
        if ($code -notin 3, 5) {
            throw "Infrastructure failure: peer=$peer target=$targetIP/$Port expect=$Expectation exit=$code (4=socket error; other codes=exec failure)"
        }
        if ($attempt -eq 5) {
            throw "Policy mismatch: peer=$peer target=$targetIP/$Port expect=$Expectation exit=$code (3=timed out; 5=forbidden connection succeeded)"
        }
        Write-Host "peer=$peer target=$targetIP/$Port expect=$Expectation; waiting 2s for endpoint/policy convergence ($attempt/5)"
        Start-Sleep -Seconds 2
    }
}

try {
    $serviceJson = kubectl @kubectlArgs get service "$Release-message-svc" -o json --request-timeout=30s
    if ($LASTEXITCODE -ne 0) { throw 'Could not query message-svc Service' }
    $service = $serviceJson | ConvertFrom-Json
    $targetIP = [string] $service.spec.clusterIP
    $parsedIP = $null
    if (-not [System.Net.IPAddress]::TryParse($targetIP, [ref] $parsedIP) -or
        $targetIP -in '0.0.0.0', '::') {
        throw 'message-svc must have a concrete Service IP'
    }
    foreach ($port in 50051, 50052, 50053) {
        if ($port -notin $service.spec.ports.port) { throw "message-svc lacks port $port" }
    }
    $servicesJson = kubectl @kubectlArgs get services -o json --request-timeout=30s
    if ($LASTEXITCODE -ne 0) { throw 'Could not check Service selectors' }
    $services = ($servicesJson | ConvertFrom-Json).items
    foreach ($source in $sources) {
        $source.Name = "np-$($source.Peer)-$($ownedPods.Count)-$suffix"
        $labels = @{
            'app.kubernetes.io/instance' = $source.Instance
            'push-messenger/network-peer' = $source.Peer
        }
        foreach ($candidate in $services) {
            if (-not $candidate.spec.selector) { continue }
            $matches = $true
            foreach ($label in $candidate.spec.selector.PSObject.Properties) {
                if ($labels[$label.Name] -ne $label.Value) { $matches = $false; break }
            }
            if ($matches) { throw 'Probe labels would match a Service selector; refusing to create endpoints' }
        }
        $pod = @{
            apiVersion = 'v1'; kind = 'Pod'
            metadata = @{ name = $source.Name; namespace = $Namespace; labels = $labels }
            spec = @{
                automountServiceAccountToken = $false
                restartPolicy = 'Never'
                activeDeadlineSeconds = 600
                terminationGracePeriodSeconds = 0
                securityContext = @{
                    runAsNonRoot = $true; runAsUser = 65532; runAsGroup = 65532
                    seccompProfile = @{ type = 'RuntimeDefault' }
                }
                containers = @(@{
                    name = 'probe'; image = $ProbeImage
                    command = @('python', '-B', '-c', 'import time; time.sleep(540)')
                    securityContext = @{
                        allowPrivilegeEscalation = $false; readOnlyRootFilesystem = $true
                        capabilities = @{ drop = @('ALL') }
                    }
                    resources = @{
                        requests = @{ cpu = '10m'; memory = '16Mi' }
                        limits = @{ cpu = '100m'; memory = '64Mi' }
                    }
                })
            }
        }
        # Track before create, including a create whose response is lost.
        $ownedPods.Add($source.Name)
        $pod | ConvertTo-Json -Depth 12 -Compress |
            kubectl @kubectlArgs create -f - --request-timeout=30s | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Could not create probe pod' }
    }
    foreach ($source in $sources) {
        kubectl @kubectlArgs wait --for=condition=Ready "pod/$($source.Name)" `
            --timeout=120s --request-timeout=125s | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Probe pod did not become ready within 120s' }
    }
    $endpointsJson = kubectl @kubectlArgs get endpointslices -o json --request-timeout=30s
    if ($LASTEXITCODE -ne 0) { throw 'Could not verify probe endpoint exclusion' }
    foreach ($slice in ($endpointsJson | ConvertFrom-Json).items) {
        foreach ($endpoint in $slice.endpoints) {
            if ($endpoint.targetRef.name -in $ownedPods) { throw 'Probe unexpectedly became a Service endpoint' }
        }
    }
    # Positive controls must all pass before any timeout can count as policy denial.
    foreach ($source in $sources | Where-Object { $_.Port -ne 0 }) {
        Test-ConnectionPolicy $source $source.Port 'allow'
    }
    foreach ($source in $sources) {
        foreach ($port in 50051, 50052, 50053) {
            $expectation = if ($source.Port -eq $port) { 'allow' } else { 'deny' }
            Test-ConnectionPolicy $source $port $expectation
        }
    }
} catch {
    $failure = $_
} finally {
    if ($ownedPods.Count -gt 0) {
        try {
            kubectl @kubectlArgs delete pod @ownedPods --ignore-not-found=true `
                --wait=true --timeout=30s --request-timeout=30s | Out-Null
            if ($LASTEXITCODE -ne 0) { throw 'Probe cleanup failed within its 30s budget' }
        } catch {
            if ($null -eq $failure) { $failure = $_ }
            else { Write-Warning 'Probe cleanup also failed; preserving the primary test failure. Only this run''s named pods were targeted.' }
        }
    }
}
if ($null -ne $failure) { throw $failure }
Write-Host 'PASS: live API controls and all 15 caller/API checks matched; probe pods removed.'
