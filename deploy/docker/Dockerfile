# Container images of push-messenger: one target per binary, all built from one shared
# builder stage, so building the other images reuses the first one's build.
#
#   docker build -f deploy/docker/Dockerfile --target auth -t push-messenger/auth:dev .
#   docker build -f deploy/docker/Dockerfile --target message-svc -t push-messenger/message-svc:dev .
#   docker build -f deploy/docker/Dockerfile --target gateway -t push-messenger/gateway:dev .
#   docker build -f deploy/docker/Dockerfile --target worker -t push-messenger/worker:dev .
#   docker build -f deploy/docker/Dockerfile --target smoke -t push-messenger/smoke:dev .
#   docker build -f deploy/docker/Dockerfile --target load -t push-messenger/load:dev .
#
# The context is the repository root. Needs BuildKit (the default for `docker build`
# since Docker 23) for the cache mounts.

# The base images are pinned by digest (their multi-arch index), so a rebuild uses
# exactly the reviewed bytes; Dependabot proposes new digests.
FROM rust:1.98-slim-bookworm@sha256:ff521445a372125ed4f76e1453a1f8098f2d05332d1601d30db1c1f62757e730 AS builder
WORKDIR /src
COPY . .
# The registry and target caches survive between builds on the same Docker daemon; the
# binaries are copied out of the cache into the image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
        -p push-auth -p push-message-svc -p push-gateway -p push-worker -p push-smoke -p push-load \
    && mkdir /out \
    && cp target/release/push-auth target/release/push-message-svc \
        target/release/push-gateway target/release/push-worker target/release/push-smoke \
        target/release/push-load /out/

# glibc and libgcc, no shell, no package manager; runs as uid 65532 ("nonroot").
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS runtime

FROM runtime AS auth
COPY --from=builder /out/push-auth /usr/local/bin/push-auth
EXPOSE 8080 8081
ENTRYPOINT ["/usr/local/bin/push-auth"]

FROM runtime AS message-svc
COPY --from=builder /out/push-message-svc /usr/local/bin/push-message-svc
EXPOSE 50051 50052 50053 8081
ENTRYPOINT ["/usr/local/bin/push-message-svc"]

FROM runtime AS gateway
COPY --from=builder /out/push-gateway /usr/local/bin/push-gateway
EXPOSE 8080 8081
ENTRYPOINT ["/usr/local/bin/push-gateway"]

FROM runtime AS worker
COPY --from=builder /out/push-worker /usr/local/bin/push-worker
EXPOSE 8080 8081
ENTRYPOINT ["/usr/local/bin/push-worker"]

FROM runtime AS smoke
COPY --from=builder /out/push-smoke /usr/local/bin/push-smoke
ENTRYPOINT ["/usr/local/bin/push-smoke"]

# push-load, the explicitly targeted load harness (docs/consumer-verification.md). It
# needs a writable mounted directory for its state file and report.
FROM runtime AS load
COPY --from=builder /out/push-load /usr/local/bin/push-load
ENTRYPOINT ["/usr/local/bin/push-load"]
