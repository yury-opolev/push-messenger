<#
.SYNOPSIS
Runs the Node.js SDK example against push-messenger on minikube.

.DESCRIPTION
Builds the Node binding, forwards a local port to the cluster's ingress controller and one
to the worker's DEV ONLY mailbox, and runs bindings/node/examples/minikube.js: two apps sign
up, verify, sign in, become contacts by invite and chat with a read receipt, through the
ingress like any client. The cluster must be up (deploy/minikube/up.ps1). The port
forwards are stopped at the end.

.EXAMPLE
deploy/minikube/node-example.ps1
#>
[CmdletBinding()]
param(
    # The minikube profile; also the kubectl context.
    [string] $Cluster = 'push',
    [int] $IngressPort = 18080,
    [int] $MailboxPort = 18081
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$node = Join-Path $root 'bindings\node'

# Waits until our own port-forward process listens on a local port: the process must still
# run, and the process that owns the listening socket must be it (not something else that
# already held the port).
function Wait-Forward($Forward, [int] $Port) {
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        $Forward.Refresh()
        if ($Forward.HasExited) {
            throw "the port-forward to port $Port stopped (exit code $($Forward.ExitCode))"
        }
        $owners = @(Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue |
            Select-Object -ExpandProperty OwningProcess -Unique)
        if ($owners -contains $Forward.Id) {
            return
        }
        if ($owners.Count -gt 0) {
            throw "port $Port is held by process $($owners -join ', '), not by our port-forward (process $($Forward.Id))"
        }
        Start-Sleep -Milliseconds 500
    }
    throw "our port-forward never listened on port $Port"
}

# Refuses a port something already listens on, so a stale listener is never mistaken for ours.
function Assert-PortFree([int] $Port) {
    if (Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue) {
        throw "port $Port is already in use"
    }
}
Write-Host '==> build the Node binding' -ForegroundColor Cyan
node (Join-Path $node 'scripts\build.js')
if ($LASTEXITCODE -ne 0) {
    throw 'the build failed'
}

Assert-PortFree $IngressPort
Assert-PortFree $MailboxPort
$forwards = @()
try {
    $ingress = Start-Process kubectl -PassThru -WindowStyle Hidden -ArgumentList @(
        '--context', $Cluster, '-n', 'ingress-nginx', 'port-forward',
        'svc/ingress-nginx-controller', "${IngressPort}:80")
    $forwards += $ingress
    $mailbox = Start-Process kubectl -PassThru -WindowStyle Hidden -ArgumentList @(
        '--context', $Cluster, '-n', 'push', 'port-forward', 'svc/push-worker', "${MailboxPort}:8080")
    $forwards += $mailbox
    Wait-Forward $ingress $IngressPort
    Wait-Forward $mailbox $MailboxPort
    Write-Host '==> run the Node.js example' -ForegroundColor Cyan
    node (Join-Path $node 'examples\minikube.js') `
        --auth "http://127.0.0.1:$IngressPort" `
        --socket "ws://127.0.0.1:$IngressPort/v1/socket" `
        --host push.local `
        --mailbox "http://127.0.0.1:$MailboxPort"
    if ($LASTEXITCODE -ne 0) {
        throw 'the Node.js example failed'
    }
} finally {
    foreach ($forward in $forwards) {
        Stop-Process -Id $forward.Id -ErrorAction SilentlyContinue
    }
}