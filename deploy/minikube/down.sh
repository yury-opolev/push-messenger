#!/usr/bin/env bash
# Removes push-messenger from the local minikube cluster; the same as down.ps1.
#
#   deploy/minikube/down.sh [--cluster NAME] [--delete]
set -euo pipefail

cluster=push
delete=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --cluster) cluster="$2"; shift 2 ;;
    --delete) delete=true; shift ;;
    *) echo "usage: $0 [--cluster NAME] [--delete]" >&2; exit 2 ;;
  esac
done

if [[ "$delete" == true ]]; then
  minikube delete -p "$cluster"
  exit 0
fi
helm uninstall push --kube-context "$cluster" --namespace push --wait --ignore-not-found
kubectl --context "$cluster" delete namespace push --ignore-not-found --wait=true
# After a cluster restart the hostpath provisioner no longer recognises its volumes: it
# leaves them Released and keeps the data directory, which the next install would reuse
# with a freshly generated Secret password. Remove both.
volumes="$(kubectl --context "$cluster" get pv -o jsonpath='{range .items[?(@.spec.claimRef.namespace=="push")]}{.metadata.name}{"\n"}{end}')"
for volume in $volumes; do
  kubectl --context "$cluster" delete pv "$volume" --ignore-not-found --wait=false
done
minikube -p "$cluster" ssh -- sudo rm -rf /tmp/hostpath-provisioner/push
