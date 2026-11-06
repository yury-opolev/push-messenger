# syntax=docker/dockerfile:1.10
# The chart's internal TURN image (calls.turn.internal.image): upstream coturn's Alpine
# release, pinned by digest, with the Alpine security updates applied and a copy of
# turnserver without file capabilities at /opt/coturn/bin/turnserver. Everything else is
# upstream's: the same turnserver, user (nobody, 65534), entrypoint, command and layout
# (/usr/bin/turnserver stays as it is).
#
# Build it into your own registry and rebuild it when Alpine ships fixes (docs/calls.md,
# §13.7); nothing from the repository is copied in, so any context works:
#   docker build -f deploy/docker/coturn.Dockerfile -t <registry>/push-coturn:<tag> deploy/docker
#   az acr build --registry <acr> --image push-coturn:<tag> --file deploy/docker/coturn.Dockerfile deploy/docker
#
# Why the copy: upstream's /usr/bin/turnserver carries cap_net_bind_service=ep. With the
# chart's restricted securityContext (drop ALL, no privilege escalation) execve refuses a
# binary whose file capabilities it cannot grant. A plain cp leaves the security.capability
# extended attribute behind; coturn listens above 1024 only, so it never needs it. With
# this image the chart can run /opt/coturn/bin/turnserver directly
# (calls.turn.internal.copyBinary: false).

# The upstream multi-arch index, pinned by digest.
ARG COTURN_BASE=coturn/coturn:4.18.0-alpine@sha256:c29135c08565810d37053f384584392b7c24b69570fc81a721b878d6fe63e04e

FROM ${COTURN_BASE} AS patched
USER root
# Every pending update from the base's Alpine branch (security fixes included), and a check
# of the fix that prompted this image: brotli-libs >= 1.2.1-r0 (CVE-2026-111497).
RUN set -eu; \
    apk upgrade --no-cache; \
    brotli="$(apk list --installed brotli-libs | sed -n 's/^brotli-libs-\([^ ]*\) .*/\1/p')"; \
    test -n "$brotli"; \
    case "$(apk version -t "$brotli" 1.2.1-r0)" in \
        '<') echo "brotli-libs $brotli < 1.2.1-r0" >&2; exit 1 ;; \
    esac; \
    echo "brotli-libs $brotli"; \
    mkdir -p /opt/coturn/bin; \
    cp /usr/bin/turnserver /opt/coturn/bin/turnserver; \
    chmod 0755 /opt/coturn/bin/turnserver

# Proves the copy carries no file capabilities (and that the original did, so the check
# means something). Not part of the image: only its marker file is.
FROM patched AS verify
RUN set -eu; \
    apk add --no-cache libcap-utils; \
    getcap /usr/bin/turnserver | tee /dev/stderr | grep -q cap_net_bind_service; \
    test -z "$(getcap /opt/coturn/bin/turnserver)"; \
    cmp /usr/bin/turnserver /opt/coturn/bin/turnserver; \
    echo "/opt/coturn/bin/turnserver: no file capabilities" > /verified

FROM patched
COPY --from=verify /verified /opt/coturn/verified
USER nobody:nogroup

LABEL org.opencontainers.image.source="https://github.com/yury-opolev/push-messenger" \
      org.opencontainers.image.description="coturn 4.18.0 (Alpine) with Alpine security updates and a capability-free turnserver, for the push-messenger chart" \
      org.opencontainers.image.licenses="BSD-3-Clause" \
      org.opencontainers.image.version="4.18.0" \
      org.opencontainers.image.base.name="docker.io/coturn/coturn:4.18.0-alpine" \
      org.opencontainers.image.base.digest="sha256:c29135c08565810d37053f384584392b7c24b69570fc81a721b878d6fe63e04e"
