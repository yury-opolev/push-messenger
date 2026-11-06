#!/usr/bin/env bash
# The third-party images the chart deploys, for the vulnerability scans of CI (chart-images
# in .github/workflows/ci.yml) and the weekly rescan (.github/workflows/image-rescan.yml):
# helm template with every optional component on (internal TURN, the bundled PostgreSQL and
# Redis, the helm test smoke Pod), every image: reference except this repository's own
# push-messenger/* images (built and scanned in CI's images job). Each must be pinned by
# digest (<repository>:<tag>@sha256:<64 hex digits>).
#
# Prints one image per line, or with --matrix a GitHub Actions matrix:
# {"include":[{"image":"...","ignorefile":"deploy/helm/trivyignore/<name>.yaml or empty"}]}
# where <name> is the repository's last path component (e.g. push-coturn, postgres).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

images="$(helm template push "$here/push-messenger" \
    --set calls.enabled=true \
    --set calls.turn.internal.publicHost=turn.example.com \
    --set calls.turn.internal.externalIp=203.0.113.10 \
    --set 'calls.turn.internal.deniedPeerCidrs={10.244.0.0/16}' \
    --set smoke.entraAccountsSecret=ci |
    sed -nE 's/^[[:space:]]*(- )?image:[[:space:]]*"?([^"[:space:]]+)"?[[:space:]]*$/\2/p' |
    grep -v '^push-messenger/' | sort -u)"

if [ -z "$images" ]; then
    echo "no third-party image in the rendered chart" >&2
    exit 1
fi
for image in $images; do
    if ! [[ "$image" =~ ^[^@[:space:]]+:[^@:/[:space:]]+@sha256:[0-9a-f]{64}$ ]]; then
        echo "not pinned by digest: $image" >&2
        exit 1
    fi
done

if [ "${1:-}" != "--matrix" ]; then
    echo "$images"
    exit 0
fi
entries=()
for image in $images; do
    name="${image%%@*}"
    name="${name%:*}"
    name="${name##*/}"
    ignorefile="deploy/helm/trivyignore/$name.yaml"
    [ -f "$root/$ignorefile" ] || ignorefile=""
    entries+=("{\"image\":\"$image\",\"ignorefile\":\"$ignorefile\"}")
done
(IFS=,; echo "{\"include\":[${entries[*]}]}")
