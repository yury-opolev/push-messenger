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
# jobs finish fails the install. The jobs may already be cleaned up; then there is
# nothing to wait for.
step 'wait for the ingress-nginx admission jobs'
jobs="$(kubectl --context "$cluster" -n ingress-nginx get job -o name)"
if [[ -n "$jobs" ]]; then
  kubectl --context "$cluster" -n ingress-nginx wait --for=condition=complete job --all --timeout=300s
fi

# PostgreSQL runs the init scripts that create auth-svc's and the worker's databases only
# when it initialises an empty volume. A volume from before auth-svc (Plan 4B) has no
# push_auth, one from before the worker (Plan 5B) no push_worker, and the services would
# never become ready: stop now, not after five slow install attempts.
predates="The Postgres volume predates auth-svc or the worker: it lacks the push_auth or push_worker database, and PostgreSQL creates them only on an empty volume. Run deploy/minikube/down.sh --cluster $cluster first (it deletes the cluster's data), then run this script again."
# A plain `helm uninstall` removes the StatefulSet but keeps the volume and the Secret
# (helm.sh/resource-policy: keep), so the Secret is the evidence that survives it.
secret_name="$release-secrets"
secret="$(kubectl --context "$cluster" -n "$namespace" get secret "$secret_name" -o name --ignore-not-found)"
if [[ -n "$secret" ]]; then
  for key in auth-db-password worker-db-password; do
    password="$(kubectl --context "$cluster" -n "$namespace" get secret "$secret_name" -o "jsonpath={.data.$key}")"
    if [[ -z "$password" ]]; then
      echo "The kept Secret $secret_name has no $key: it predates the service that needs it. $predates" >&2
      exit 1
    fi
  done
fi
postgres="$(kubectl --context "$cluster" -n "$namespace" get statefulset push-postgres -o name --ignore-not-found)"
if [[ -n "$postgres" ]]; then
  step 'check that the Postgres volume has the push_auth and push_worker databases'
  kubectl --context "$cluster" -n "$namespace" rollout status statefulset/push-postgres --timeout=300s
  if ! databases="$(kubectl --context "$cluster" -n "$namespace" exec push-postgres-0 -- \
    psql -U push -d postgres -Atc "SELECT count(*) FROM pg_database WHERE datname IN ('push_auth', 'push_worker')")"; then
    echo 'checking the Postgres volume failed' >&2
    exit 1
  fi
  if [[ "$databases" != 2 ]]; then
    echo "$predates" >&2
    exit 1
  fi
fi

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
    for target in auth message-svc gateway worker smoke; do
      step "build push-messenger/$target:dev"
      docker build -f "$root/deploy/docker/Dockerfile" --target "$target" -t "push-messenger/$target:dev" "$root"
    done
  )
fi
# The image ids name the build: a rebuilt image rolls the pods, an unchanged one does not.
if ! ids="$(use_cluster_docker && docker image inspect --format '{{.Id}}' \
  push-messenger/auth:dev push-messenger/message-svc:dev push-messenger/gateway:dev \
  push-messenger/worker:dev push-messenger/smoke:dev)"; then
  echo 'the images are not in the cluster yet; run without --skip-build' >&2
  exit 1
fi
build_id="$(printf '%s\n' "$ids" | cut -c8-19 | paste -sd- -)"

# Right after a cluster start the controller can be ready before its admission webhook
# answers; the install is then refused. Retry only when it fails.
installed=false
for attempt in 1 2 3 4 5; do
  step "install release '$release' in namespace '$namespace' (attempt $attempt of 5)"
  if helm upgrade --install "$release" "$chart" \
    --kube-context "$cluster" --namespace "$namespace" --create-namespace \
    -f "$chart/values-minikube.yaml" \
    --set "buildId=$build_id" \
    --wait --timeout 10m; then
    installed=true
    break
  fi
  if [[ "$attempt" -lt 5 ]]; then
    echo 'helm upgrade failed; retrying in 10 s' >&2
    sleep 10
  fi
done
if [[ "$installed" == false ]]; then
  echo "install release '$release' failed after 5 attempts" >&2
  exit 1
fi

echo
echo "push-messenger is up: minikube profile '$cluster', namespace '$namespace'."
echo 'The gateway is behind ingress-nginx at ws://push.local/v1/socket. From this machine:'
echo "  kubectl --context $cluster -n ingress-nginx port-forward svc/ingress-nginx-controller 8080:80"
echo '  then connect to ws://127.0.0.1:8080/v1/socket with the header "Host: push.local".'
echo '  auth-svc answers on the same port: http://127.0.0.1:8080/v1/auth/..., /v1/devices, /v1/users;'
echo '  the worker takes signed Resend webhooks at http://127.0.0.1:8080/v1/webhooks/resend.'

if [[ "$smoke" == true ]]; then
  # The DEV ONLY mailbox is in the worker's memory. `helm upgrade --wait` returns once the new
  # worker is ready, but the old pod can still claim and deliver the verification email into
  # its own mailbox; wait until only the new one remains (terminating pods count).
  worker_selector="app.kubernetes.io/name=worker,app.kubernetes.io/instance=$release"
  step 'wait for the old worker pod to go away'
  deadline=$((SECONDS + 90))
  while true; do
    mapfile -t worker_pods < <(kubectl --context "$cluster" -n "$namespace" get pods -l "$worker_selector" -o name)
    echo "worker pods: ${#worker_pods[@]} (${worker_pods[*]})"
    if [[ "${#worker_pods[@]}" -eq 1 ]]; then
      break
    fi
    if [[ "$SECONDS" -gt "$deadline" ]]; then
      echo "more than one worker pod (${#worker_pods[@]}) after 90 s; the smoke test needs exactly one" >&2
      exit 1
    fi
    sleep 2
  done
  step 'wait for the worker pod to be Ready'
  kubectl --context "$cluster" -n "$namespace" wait --for=condition=Ready pod -l "$worker_selector" --timeout=90s
  step 'run the smoke test'
  helm test "$release" --kube-context "$cluster" --namespace "$namespace" --logs --timeout 5m
fi
