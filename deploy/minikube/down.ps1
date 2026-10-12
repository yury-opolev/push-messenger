<#
.SYNOPSIS
Removes push-messenger from the local minikube cluster.

.DESCRIPTION
Uninstalls the Helm release and deletes its namespace, including the Postgres volume.
With -Delete it deletes the whole minikube profile instead.

.EXAMPLE
deploy/minikube/down.ps1
Removes the release and its data; the cluster keeps running.

.EXAMPLE
deploy/minikube/down.ps1 -Delete
Deletes the "push" minikube profile.
#>
[CmdletBinding()]
param(
    [string] $Cluster = 'push',
    [switch] $Delete
)

$ErrorActionPreference = 'Stop'

if ($Delete) {
    minikube delete -p $Cluster
    if ($LASTEXITCODE -ne 0) {
        throw "minikube delete failed (exit code $LASTEXITCODE)"
    }
    return
}

helm uninstall push --kube-context $Cluster --namespace push --wait --ignore-not-found
if ($LASTEXITCODE -ne 0) {
    throw "helm uninstall failed (exit code $LASTEXITCODE)"
}
kubectl --context $Cluster delete namespace push --ignore-not-found --wait=true
if ($LASTEXITCODE -ne 0) {
    throw "deleting the namespace failed (exit code $LASTEXITCODE)"
}
