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
`values-minikube.yaml`, and runs the end-to-end smoke test: two users chat through the
ingress. `deploy/minikube/up.sh --smoke` does the same on Linux and macOS. Both wait for
ingress-nginx and retry `helm upgrade --install` up to 5 times while its admission
webhook comes up. `deploy/minikube/down.ps1` removes everything again: namespace, the
Secret, the PostgreSQL volume and its data directory (`-Delete` deletes the profile).
`helm uninstall` alone keeps the Secret (`helm.sh/resource-policy: keep`) and the volume,
so a later install reuses both and the passwords keep matching. Every pull request is
verified this way before it is merged.

The minikube values switch on a DEV ONLY identity: the gateway accepts any user and
device id without a token. The chart refuses that mode unless the values also say
`devMode.acknowledge: insecure-local-only` and the ingress host is local (`*.local`,
`*.localhost` or `*.test`). `pwsh deploy/helm/test-chart.ps1` checks the chart without a
cluster.

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
## License

MIT
