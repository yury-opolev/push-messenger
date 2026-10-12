<#
.SYNOPSIS
Removes push-messenger from the local minikube cluster.

.DESCRIPTION
Uninstalls the Helm release and deletes its namespace, including the Postgres volume, its
data directory on the node and the kept Secret.
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
# After a cluster restart the hostpath provisioner no longer recognises its volumes: it
# leaves them Released and keeps the data directory, which the next install would reuse
# with a freshly generated Secret password. Remove both.
$volumes = kubectl --context $Cluster get pv -o jsonpath='{range .items[?(@.spec.claimRef.namespace=="push")]}{.metadata.name}{"\n"}{end}'
if ($LASTEXITCODE -ne 0) {
    throw "listing the volumes failed (exit code $LASTEXITCODE)"
}
foreach ($volume in $volumes | Where-Object { $_ }) {
    kubectl --context $Cluster delete pv $volume --ignore-not-found --wait=false
    if ($LASTEXITCODE -ne 0) {
        throw "deleting the volume $volume failed (exit code $LASTEXITCODE)"
    }
}
minikube -p $Cluster ssh -- sudo rm -rf /tmp/hostpath-provisioner/push
if ($LASTEXITCODE -ne 0) {
    throw "removing the volume data failed (exit code $LASTEXITCODE)"
}
