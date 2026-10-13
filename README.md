# push-messenger

A minimal, expandable messenger backend and client SDK, written in Rust.
We build the server side and an SDK; third parties build the user interfaces.

- Design: [`docs/superpowers/specs/2026-10-11-push-messenger-design.md`](docs/superpowers/specs/2026-10-11-push-messenger-design.md)
- Roadmap: [`docs/superpowers/plans/2026-10-11-roadmap.md`](docs/superpowers/plans/2026-10-11-roadmap.md)

## Build and test

The integration tests start `postgres:17-alpine` and `redis:7-alpine` containers through
Docker, so Docker must be running (the first run pulls the images):

    docker version
    cargo test --workspace

Building needs neither a database nor a system `protoc` (`push-proto` uses a vendored
one).

## Run it on minikube

The whole system runs on a local [minikube](https://minikube.sigs.k8s.io/) cluster with
the same Helm chart that will deploy to Azure. With Docker, minikube, kubectl and Helm
installed:

    deploy/minikube/up.ps1 -Smoke

starts (or reuses) the minikube profile `push`, enables its ingress addon, builds the
images into the cluster, installs the chart (`deploy/helm/push-messenger`) with
`values-minikube.yaml`, and runs the end-to-end smoke test: two users sign up, verify their
email, register a device and chat through the ingress; the DEV ONLY mailbox and the setup calls go
to the services directly. `deploy/minikube/up.sh --smoke`
does the same on Linux and macOS. Both wait for ingress-nginx and retry
`helm upgrade --install` up to 5 times while its admission webhook comes up.
`deploy/minikube/down.ps1` removes everything again: namespace, the
Secret, the PostgreSQL volume and its data directory (`-Delete` deletes the profile).
`helm uninstall` alone keeps the Secret (`helm.sh/resource-policy: keep`) and the volume,
so a later install reuses both and the passwords keep matching. Every pull request is
verified this way before it is merged.

The minikube values switch on DEV ONLY shortcuts: the gateway accepts any user and device
id without a token (until it checks access tokens, Plan 8), message-svc serves a setup API,
and auth-svc shows the email links waiting in its outbox at `/dev/mailbox` (until the
worker sends email, Plan 6); neither is routed by the ingress. The chart refuses that mode
unless the values also say `devMode.acknowledge: insecure-local-only` and the ingress host
is local (`*.local`, `*.localhost` or `*.test`). `pwsh deploy/helm/test-chart.ps1` checks
the chart without a cluster.

auth-svc has a database and a role of its own, `push_auth`, which PostgreSQL's init script
creates when it initialises an empty volume. A cluster whose Postgres volume predates auth-svc
(Plan 4B) has no such database. `up` stops when the kept Secret `push-secrets` (it survives a plain
`helm uninstall`) has no `auth-db-password`, or when the running Postgres has no `push_auth`: run
`deploy/minikube/down.ps1` once, then `up` again.

The smoke client checks the access token's audience (`socket_url` with its authority
replaced by the Host override: `ws://push.local/v1/socket` on minikube) and the refreshed
token's `sub` and `device_id`. It never prints response bodies, tokens or token-bearing
URLs, retries only on 502, 503, 504 and transport errors, and ignores `HTTP_PROXY`. The
per-IP signup limit (20/h) caps smoke runs to about 10 per hour on a long-lived cluster.

## Configuration

The services read environment variables (the chart sets them). Invalid values are refused
at start-up.

`push-message-svc`:

| Variable | Meaning | Default |
|----------|---------|---------|
| `PUSH_DATABASE_URL` | Postgres URL of the `push_messaging` database, as its owner | required |
| `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
| `PUSH_GRPC_ADDR` | address of the gRPC API | `0.0.0.0:50051` |
| `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
| `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
| `PUSH_DEV_SETUP` | `1` registers the DEV ONLY `DevSetup` service | off |

`push-gateway`:

| Variable | Meaning | Default |
|----------|---------|---------|
| `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc | required |
| `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
| `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
| `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
| `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready`, 1000 to 600000 | `30000` |
| `PUSH_DEV_AUTH` | `1` accepts DEV ONLY identities without a token | off |

`push-auth`:

| Variable | Meaning | Default |
|----------|---------|---------|
| `PUSH_DATABASE_URL` | Postgres URL of the `push_auth` database, as its owner | required |
| `PUSH_REDIS_URL` | Redis URL for rate limits, `redis://:password@host:6379` | required |
| `PUSH_AUTH_PUBLIC_URL` | base URL of email links, `https://api.example.com` (`http` only for local hosts) | required |
| `PUSH_AUTH_TOKEN_AUDIENCE` | the gateway's URL, the access tokens' `aud` (`wss://…/v1/socket`) | required |
| `PUSH_AUTH_SIGNING_KEY` | base64 of the 32-byte Ed25519 seed that signs access tokens | required |
| `PUSH_AUTH_LINK_KEY` | base64 of at least 32 bytes, the HMAC key of email links and registration tokens | required |
| `PUSH_AUTH_CLIENT_IP_HEADER` | header with the client's IP, set by the ingress (`x-real-ip`); its last line, then last entry is used; assumes exactly one proxy in front that overwrites or appends it, otherwise clients can spoof it (weakening rate limits only) | the peer address |
| `PUSH_AUTH_HASH_CONCURRENCY` | most password hashes at once (19 MiB each) | `2` |
| `PUSH_AUTH_HASH_QUEUE` | most requests waiting for a password hash; more get a retryable 503 (`Retry-After: 1`) | `32` |
| `PUSH_HTTP_ADDR` | address of the API | `0.0.0.0:8080` |
| `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
| `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
| `PUSH_DEV_MAILBOX` | `1` serves the DEV ONLY `GET /dev/mailbox`; refused unless the public URL's host is local | off |

## License

MIT
