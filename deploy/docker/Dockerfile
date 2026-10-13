# Container images of push-messenger: one target per binary, all built from one shared
# builder stage, so building the other images reuses the first one's build.
#
#   docker build -f deploy/docker/Dockerfile --target auth -t push-messenger/auth:dev .
#   docker build -f deploy/docker/Dockerfile --target message-svc -t push-messenger/message-svc:dev .
#   docker build -f deploy/docker/Dockerfile --target gateway -t push-messenger/gateway:dev .
#   docker build -f deploy/docker/Dockerfile --target smoke -t push-messenger/smoke:dev .
#
# The context is the repository root. Needs BuildKit (the default for `docker build`
# since Docker 23) for the cache mounts.

FROM rust:1.85-slim-bookworm AS builder
WORKDIR /src
COPY . .
# The registry and target caches survive between builds on the same Docker daemon; the
# binaries are copied out of the cache into the image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
        -p push-auth -p push-message-svc -p push-gateway -p push-smoke \
    && mkdir /out \
    && cp target/release/push-auth target/release/push-message-svc \
        target/release/push-gateway target/release/push-smoke /out/

# glibc and libgcc, no shell, no package manager; runs as uid 65532 ("nonroot").
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime

FROM runtime AS auth
COPY --from=builder /out/push-auth /usr/local/bin/push-auth
EXPOSE 8080 8081
ENTRYPOINT ["/usr/local/bin/push-auth"]

FROM runtime AS message-svc
COPY --from=builder /out/push-message-svc /usr/local/bin/push-message-svc
EXPOSE 50051 8081
ENTRYPOINT ["/usr/local/bin/push-message-svc"]

FROM runtime AS gateway
COPY --from=builder /out/push-gateway /usr/local/bin/push-gateway
EXPOSE 8080 8081
ENTRYPOINT ["/usr/local/bin/push-gateway"]

FROM runtime AS smoke
COPY --from=builder /out/push-smoke /usr/local/bin/push-smoke
ENTRYPOINT ["/usr/local/bin/push-smoke"]
