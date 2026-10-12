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
# jobs finish fails the install. The jobs may already be cleaned up; then there is
# nothing to wait for.
Write-Host '==> wait for the ingress-nginx admission jobs' -ForegroundColor Cyan
$jobs = kubectl --context $Cluster -n ingress-nginx get job -o name
if ($LASTEXITCODE -ne 0) {
    throw "listing the ingress-nginx jobs failed (exit code $LASTEXITCODE)"
}
if ($jobs) {
    kubectl --context $Cluster -n ingress-nginx wait --for=condition=complete job --all --timeout=300s
    if ($LASTEXITCODE -ne 0) {
        throw "the ingress-nginx admission jobs did not complete (exit code $LASTEXITCODE)"
    }
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

# Right after a cluster start the controller can be ready before its admission webhook
# answers; the install is then refused. Retry only when it fails.
$installed = $false
for ($attempt = 1; $attempt -le 5 -and -not $installed; $attempt++) {
    Write-Host "==> install release '$release' in namespace '$namespace' (attempt $attempt of 5)" -ForegroundColor Cyan
    helm upgrade --install $release $chart `
        --kube-context $Cluster --namespace $namespace --create-namespace `
        -f (Join-Path $chart 'values-minikube.yaml') `
        --set "buildId=$buildId" `
        --wait --timeout 10m
    if ($LASTEXITCODE -eq 0) {
        $installed = $true
    } elseif ($attempt -lt 5) {
        Write-Host "helm upgrade failed (exit code $LASTEXITCODE); retrying in 10 s" -ForegroundColor Yellow
        Start-Sleep -Seconds 10
    }
}
if (-not $installed) {
    throw "install release '$release' failed after 5 attempts"
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
