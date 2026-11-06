# syntax=docker/dockerfile:1.10
# The chart's internal TURN image (calls.turn.internal.image): upstream coturn's Alpine
# release with the Alpine security updates applied, for when upstream has not rebuilt yet
# (docs/calls.md, "Security updates of the coturn image"). Nothing else changes: the same
# turnserver binary, entrypoint, command, user and file layout as the base, so the chart's
# init containers (the configuration, and the capability-free copy of /usr/bin/turnserver
# run as /opt/coturn/bin/turnserver) work unchanged.
#
# Built and published by .github/workflows/coturn-image.yml to ghcr.io/yury-opolev/push-coturn;
# the chart pins the published digest. Local build:
#   docker build -t push-coturn:local deploy/coturn

# The upstream multi-arch index, pinned by digest.
ARG COTURN_BASE=coturn/coturn:4.18.0-alpine@sha256:c29135c08565810d37053f384584392b7c24b69570fc81a721b878d6fe63e04e

FROM ${COTURN_BASE}

USER root
# Every pending update from the base's Alpine branch (security fixes included), and a
# check of the fix that prompted this image: brotli-libs >= 1.2.1-r0 (CVE-2026-111497).
RUN set -eu; \
    apk upgrade --no-cache; \
    brotli="$(apk list --installed brotli-libs | sed -n 's/^brotli-libs-\([^ ]*\) .*/\1/p')"; \
    test -n "$brotli"; \
    case "$(apk version -t "$brotli" 1.2.1-r0)" in \
        '<') echo "brotli-libs $brotli < 1.2.1-r0" >&2; exit 1 ;; \
    esac; \
    echo "brotli-libs $brotli"
USER nobody:nogroup

LABEL org.opencontainers.image.source="https://github.com/yury-opolev/push-messenger" \
      org.opencontainers.image.description="coturn 4.18.0 (Alpine) with Alpine security updates, for the push-messenger chart" \
      org.opencontainers.image.licenses="BSD-3-Clause" \
      org.opencontainers.image.version="4.18.0" \
      org.opencontainers.image.base.name="docker.io/coturn/coturn:4.18.0-alpine" \
      org.opencontainers.image.base.digest="sha256:c29135c08565810d37053f384584392b7c24b69570fc81a721b878d6fe63e04e"
