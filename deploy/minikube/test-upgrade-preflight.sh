#!/usr/bin/env bash
# Executes up.sh's real pre-build path with fail-closed native-command mocks.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
prefix="$(sed '/^# Point docker/,$d' "$root/up.sh")"
for test_case in missing-messaging-key legacy-owner query-error prepared fresh; do
  export test_case
  if output="$(
    bash -c '
minikube() { echo Running/Running/Running; }
kubectl() {
  [[ "$*" == "--context fixture "* ]] || { echo "missing context" >&2; return 20; }
  case "$*" in
    *"get job"*) ;;
    *"get secret "*" -o name"*) [[ "$test_case" == fresh ]] || echo secret/push-secrets ;;
    *"get secret "*"jsonpath"*)
      if [[ "$*" != *messaging-db-password* || "$test_case" != missing-messaging-key ]]; then echo synthetic; fi ;;
    *"get statefulset"*) [[ "$test_case" == fresh ]] || echo statefulset.apps/push-postgres ;;
    *"rollout status"*) ;;
    *"SELECT count(*) FROM pg_database"*) echo 2 ;;
    *"SELECT count(*) FROM pg_roles"*)
      [[ "$*" == *"psql -X -v ON_ERROR_STOP=1 -U push -d postgres -Atc"* ]] || return 21
      if [[ "$test_case" == query-error ]]; then return 7; fi
      if [[ "$test_case" == legacy-owner ]]; then echo 0; else echo 1; fi ;;
    *) echo "unexpected or mutating command" >&2; return 22 ;;
  esac
  return 0
}
'"$prefix" -- --cluster fixture 2>&1
  )"; then
    [[ "$test_case" == prepared || "$test_case" == fresh ]] || {
      echo "FAIL $test_case unexpectedly accepted"; exit 1;
    }
  else
    case "$test_case" in
      missing-messaging-key|legacy-owner)
        [[ "$output" == *docs/deployment-upgrade.md* ]] || { echo "FAIL $test_case: $output"; exit 1; } ;;
      query-error)
        [[ "$output" == *"checking the messaging database owner failed"* ]] || { echo "FAIL $test_case: $output"; exit 1; } ;;
      *) echo "FAIL $test_case: $output"; exit 1 ;;
    esac
  fi
  echo "ok   $test_case"
done
