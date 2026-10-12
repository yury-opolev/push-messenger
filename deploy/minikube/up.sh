#!/usr/bin/env bash
# Brings push-messenger up on a local minikube cluster (Linux, macOS, CI); the same steps
# as up.ps1.
#
#   deploy/minikube/up.sh [--cluster NAME] [--smoke] [--skip-build]
set -euo pipefail

cluster=push
smoke=false
skip_build=false
cpus=4
memory=6g
while [[ $# -gt 0 ]]; do
  case "$1" in
    --cluster) cluster="$2"; shift 2 ;;
    --smoke) smoke=true; shift ;;
    --skip-build) skip_build=true; shift ;;
    --cpus) cpus="$2"; shift 2 ;;
    --memory) memory="$2"; shift 2 ;;
    *) echo "usage: $0 [--cluster NAME] [--smoke] [--skip-build] [--cpus N] [--memory SIZE]" >&2; exit 2 ;;
  esac
done

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
chart="$root/deploy/helm/push-messenger"
namespace=push
release=push

step() { printf '==> %s\n' "$*"; }

# `minikube start` is idempotent, but on a running profile it restarts every container in
# the cluster; skip it only when the node, kubelet and API server all run.
state="$(minikube status -p "$cluster" --format '{{.Host}}/{{.Kubelet}}/{{.APIServer}}' 2>/dev/null || true)"
if [[ "$state" != "Running/Running/Running" ]]; then
  step "start minikube profile '$cluster'"
  minikube start -p "$cluster" --driver=docker --cpus="$cpus" --memory="$memory"
fi
step 'enable the ingress addon'
minikube -p "$cluster" addons enable ingress
step 'wait for ingress-nginx'
kubectl --context "$cluster" -n ingress-nginx rollout status deployment/ingress-nginx-controller --timeout=300s
# The admission webhook validates Ingress objects; installing before its certificate
# jobs finish fails the install.
step 'wait for the ingress-nginx admission webhook'
kubectl --context "$cluster" -n ingress-nginx wait --for=condition=complete \
  job/ingress-nginx-admission-create job/ingress-nginx-admission-patch --timeout=300s

# Point docker at the cluster's own Docker daemon. Called in subshells only, so the
# DOCKER_* variables do not leak into this script.
use_cluster_docker() {
  local env
  env="$(minikube -p "$cluster" docker-env --shell bash)" || return 1
  eval "$env"
}

# Builds run in a subshell, not a command substitution: errexit stays on there, so a
# failed build stops the script before anything is deployed.
if [[ "$skip_build" == false ]]; then
  (
    use_cluster_docker
    for target in message-svc gateway smoke; do
      step "build push-messenger/$target:dev"
      docker build -f "$root/deploy/docker/Dockerfile" --target "$target" -t "push-messenger/$target:dev" "$root"
    done
  )
fi
# The image ids name the build: a rebuilt image rolls the pods, an unchanged one does not.
if ! ids="$(use_cluster_docker && docker image inspect --format '{{.Id}}' \
  push-messenger/message-svc:dev push-messenger/gateway:dev push-messenger/smoke:dev)"; then
  echo 'the images are not in the cluster yet; run without --skip-build' >&2
  exit 1
fi
build_id="$(printf '%s\n' "$ids" | cut -c8-19 | paste -sd- -)"

step "install release '$release' in namespace '$namespace'"
helm upgrade --install "$release" "$chart" \
  --kube-context "$cluster" --namespace "$namespace" --create-namespace \
  -f "$chart/values-minikube.yaml" \
  --set "buildId=$build_id" \
  --wait --timeout 10m

echo
echo "push-messenger is up: minikube profile '$cluster', namespace '$namespace'."
echo 'The gateway is behind ingress-nginx at ws://push.local/v1/socket. From this machine:'
echo "  kubectl --context $cluster -n ingress-nginx port-forward svc/ingress-nginx-controller 8080:80"
echo '  then connect to ws://127.0.0.1:8080/v1/socket with the header "Host: push.local".'

if [[ "$smoke" == true ]]; then
  step 'run the smoke test'
  helm test "$release" --kube-context "$cluster" --namespace "$namespace" --logs --timeout 5m
fi
