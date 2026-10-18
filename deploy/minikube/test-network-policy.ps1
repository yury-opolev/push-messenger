<#
.SYNOPSIS
Verifies the existing message-svc's internal API ingress policy on an enforcing CNI.

.DESCRIPTION
Uses disposable, unprivileged Python pods, never application containers or credentials.
First proves every API is live, then checks the caller/port matrix, unrelated callers
and release isolation. A refusal or exec failure is infrastructure failure, not denial.
Does not install infrastructure or change policies. Every kubectl call uses -Context.

With -Egress (a release with networkPolicy.egress.enabled, e.g. the Entra ID rehearsal
deploy/minikube/entra-rehearsal.ps1 -Keep) it then checks the egress rules from probe
pods that carry each component's own labels (so its policy selects them) and a readiness
probe that never passes (so no Service ever routes to them): DNS resolves from every
probe, release peers and the rehearsal's datastores and issuer are reachable as allowed,
and everything else times out, including targets only egress rules can deny (the
internet, CoreDNS's metrics port, the ingress controller): positive controls first prove
those reachable from an unrestricted probe, so their timeouts count as denial.

.EXAMPLE
.\deploy\minikube\test-network-policy.ps1 -Context push11a

.EXAMPLE
.\deploy\minikube\test-network-policy.ps1 -Context push12b-entra -Egress
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
    [string] $ProbeImage = 'python:3.13-alpine',
    # Also check the egress rules (networkPolicy.egress.enabled).
    [switch] $Egress,
    # The rehearsal's datastores and issuer (deploy/minikube/entra/datastores.yaml).
    [ValidateNotNullOrEmpty()]
    [string] $DatastoreNamespace = 'push-ds',
    # Outside the cluster: denied by the egress rules, reachable without them.
    [ValidateNotNullOrEmpty()]
    [string] $ExternalAddress = '1.1.1.1'
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

$resolve = @'
import socket
import sys

try:
    socket.getaddrinfo(sys.argv[1], None)
except OSError:
    sys.exit(6)
'@

# A probe pod with `labels`; `neverReady` adds a readiness probe that never passes.
function New-ProbePod([string] $Name, [hashtable] $Labels, [bool] $NeverReady) {
    $container = @{
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
    }
    if ($NeverReady) {
        $container.readinessProbe = @{
            exec = @{ command = @('python', '-c', 'raise SystemExit(1)') }
            periodSeconds = 30
        }
    }
    $pod = @{
        apiVersion = 'v1'; kind = 'Pod'
        metadata = @{ name = $Name; namespace = $Namespace; labels = $Labels }
        spec = @{
            automountServiceAccountToken = $false
            restartPolicy = 'Never'
            activeDeadlineSeconds = 600
            terminationGracePeriodSeconds = 0
            securityContext = @{
                runAsNonRoot = $true; runAsUser = 65532; runAsGroup = 65532
                seccompProfile = @{ type = 'RuntimeDefault' }
            }
            containers = @($container)
        }
    }
    # Track before create, including a create whose response is lost.
    $script:ownedPods.Add($Name)
    $pod | ConvertTo-Json -Depth 12 -Compress |
        kubectl @kubectlArgs create -f - --request-timeout=30s | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Could not create probe pod' }
}

function Test-EgressPolicy([string] $From, [string] $Pod, [string] $TargetHost, [int] $Port, [string] $Expectation) {
    Write-Host "egress from=$From target=${TargetHost}:$Port expect=$Expectation"
    for ($attempt = 1; $attempt -le 5; $attempt++) {
        kubectl @kubectlArgs exec $Pod --request-timeout=15s -- `
            python -B -c $probe $TargetHost "$Port" $Expectation
        $code = $LASTEXITCODE
        if ($code -eq 0) { return }
        if ($code -notin 3, 5) {
            throw "Infrastructure failure: egress from=$From target=${TargetHost}:$Port expect=$Expectation exit=$code (4=socket or DNS error; other codes=exec failure)"
        }
        if ($attempt -eq 5) {
            throw "Policy mismatch: egress from=$From target=${TargetHost}:$Port expect=$Expectation exit=$code (3=timed out; 5=forbidden connection succeeded)"
        }
        Start-Sleep -Seconds 2
    }
}

$egressChecks = 0
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

    if ($Egress) {
        $policiesJson = kubectl @kubectlArgs get networkpolicy -o json --request-timeout=30s
        if ($LASTEXITCODE -ne 0) { throw 'Could not list the NetworkPolicies' }
        $egressComponents = @(($policiesJson | ConvertFrom-Json).items |
            Where-Object { 'Egress' -in $_.spec.policyTypes } |
            ForEach-Object { $_.spec.podSelector.matchLabels.'app.kubernetes.io/name' })
        foreach ($component in 'auth', 'message-svc', 'gateway', 'worker') {
            if ($component -notin $egressComponents) {
                throw "No egress policy for ${component}: install with networkPolicy.egress.enabled"
            }
        }
        $datastores = @(
            @{ Host = "push-ds-postgres.$DatastoreNamespace.svc"; Port = 5432 },
            @{ Host = "push-ds-redis.$DatastoreNamespace.svc"; Port = 6380 },
            @{ Host = "push-ds-issuer.$DatastoreNamespace.svc"; Port = 443 }
        )
        $peers = @('auth', 'message-svc', 'gateway', 'worker')
        if ('migrate' -in $egressComponents) { $peers += 'migrate' }
        $egressPods = @{}
        foreach ($peer in $peers) {
            $name = "np-egress-$peer-$suffix"
            # The component's own labels: its policy selects the probe. The readiness probe
            # never passes, so the Service never routes to it.
            New-ProbePod $name @{
                'app.kubernetes.io/name' = $peer
                'app.kubernetes.io/instance' = $Release
                'push-messenger/network-peer' = $peer
            } $true
            $egressPods[$peer] = $name
        }
        $control = "np-egress-control-$suffix"
        New-ProbePod $control @{
            'app.kubernetes.io/instance' = $Release
            'push-messenger/network-peer' = 'unrestricted'
        } $false
        foreach ($name in @($egressPods.Values) + $control) {
            kubectl @kubectlArgs wait --for=jsonpath='{.status.phase}'=Running "pod/$name" `
                --timeout=120s --request-timeout=125s | Out-Null
            if ($LASTEXITCODE -ne 0) { throw 'Egress probe pod did not start within 120s' }
        }
        $endpointsJson = kubectl @kubectlArgs get endpointslices -o json --request-timeout=30s
        if ($LASTEXITCODE -ne 0) { throw 'Could not verify that the egress probes serve nothing' }
        foreach ($slice in ($endpointsJson | ConvertFrom-Json).items) {
            foreach ($endpoint in $slice.endpoints) {
                if ($endpoint.targetRef.name -in $egressPods.Values -and $endpoint.conditions.ready) {
                    throw 'An egress probe became a ready Service endpoint'
                }
            }
        }
        # Targets whose own ingress admits anyone, so only egress rules can deny them: the
        # internet, CoreDNS's metrics port (DNS allows only 53) and the ingress controller.
        $egressOnly = @(
            @{ Host = $ExternalAddress; Port = 443 },
            @{ Host = 'kube-dns.kube-system.svc.cluster.local'; Port = 9153 },
            @{ Host = 'ingress-nginx-controller.ingress-nginx.svc.cluster.local'; Port = 80 }
        )
        # Positive controls first: those targets answer without egress rules, and DNS and
        # every allowed target answer from each probe, before a timeout counts.
        foreach ($target in $egressOnly) {
            Test-EgressPolicy 'unrestricted' $control $target.Host $target.Port 'allow'
            $egressChecks++
        }
        $allowed = @{
            'gateway' = @(@{ Host = "$Release-auth"; Port = 8080 }, @{ Host = "$Release-message-svc"; Port = 50051 })
            'worker' = @(@{ Host = "$Release-message-svc"; Port = 50052 })
            'auth' = @(@{ Host = "$Release-message-svc"; Port = 50053 })
            'message-svc' = @()
            'migrate' = @()
        }
        $denied = @{
            'gateway' = @(@{ Host = "$Release-message-svc"; Port = 50052 }, @{ Host = "$Release-message-svc"; Port = 50053 })
            'worker' = @(@{ Host = "$Release-auth"; Port = 8080 })
            'auth' = @(@{ Host = "$Release-auth"; Port = 8080 })
            'message-svc' = @(@{ Host = "$Release-auth"; Port = 8080 })
            'migrate' = @(@{ Host = "$Release-auth"; Port = 8080 }, @{ Host = "$Release-message-svc"; Port = 50051 })
        }
        foreach ($peer in $peers) {
            Write-Host "egress from=$peer resolve kubernetes.default.svc.cluster.local"
            kubectl @kubectlArgs exec $egressPods[$peer] --request-timeout=15s -- `
                python -B -c $resolve 'kubernetes.default.svc.cluster.local'
            if ($LASTEXITCODE -ne 0) { throw "DNS does not resolve from the $peer probe (exit $LASTEXITCODE)" }
            $egressChecks++
            foreach ($target in $allowed[$peer] + $datastores) {
                Test-EgressPolicy $peer $egressPods[$peer] $target.Host $target.Port 'allow'
                $egressChecks++
            }
        }
        foreach ($peer in $peers) {
            foreach ($target in $denied[$peer] + $egressOnly) {
                Test-EgressPolicy $peer $egressPods[$peer] $target.Host $target.Port 'deny'
                $egressChecks++
            }
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
if ($Egress) {
    Write-Host "PASS: all $egressChecks egress checks (controls, DNS, allowed and denied targets) matched."
}
