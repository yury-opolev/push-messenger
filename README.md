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
`values-minikube.yaml`, and runs the end-to-end smoke test: three users sign up, verify
their email (the worker's log sender delivers it), register a device and wait for the worker
to relay it to message-svc; they become contacts by request and by invite, see only each
other's allowed profiles, chat through the ingress, edit, read, form a group and delete,
and finally a revoked device and a password reset are kicked on Redis. The DEV ONLY mailbox
and the calls to message-svc's API (the smoke test moves onto the gateway in Plan 8B) go to the
services directly. `deploy/minikube/up.sh --smoke` does the same on Linux
and macOS. Both wait for ingress-nginx, retry
`helm upgrade --install` up to 5 times while its admission webhook comes up, and
(with `--smoke`) wait until only one worker pod is left before the smoke test.
`deploy/minikube/down.ps1` removes everything again: namespace, the
Secret, the PostgreSQL volume and its data directory (`-Delete` deletes the profile).
`helm uninstall` alone keeps the Secret (`helm.sh/resource-policy: keep`) and the volume,
so a later install reuses both and the passwords keep matching. Every pull request is
verified this way before it is merged.

The minikube values switch on a DEV ONLY shortcut: the worker, which logs emails instead
of sending them, shows them at `/dev/mailbox`, which the ingress does not route (it
reaches the worker only at `/v1/webhooks/resend`). Clients, the smoke test included,
always identify with real access tokens. The chart refuses that mode unless the
values also say `devMode.acknowledge: insecure-local-only` and the ingress host is local
(`*.local`, `*.localhost` or `*.test`), and refuses it with the Resend sender.
`pwsh deploy/helm/test-chart.ps1` checks the chart without a cluster.

auth-svc and the worker have databases and roles of their own, `push_auth` and
`push_worker`, which PostgreSQL's init scripts create when it initialises an empty volume;
the worker's role may also drain auth-svc's outbox, which auth-svc grants at start-up.
message-svc and the worker share a generated Accounts token from the same Secret, and
message-svc and auth-svc a Directory token; the gateway gets neither. A
cluster whose Postgres volume predates them (Plans 4B and 5B) lacks a database. `up` stops
when the kept Secret `push-secrets` (it survives a plain `helm uninstall`) has no
`auth-db-password` or `worker-db-password`, or when the running Postgres lacks `push_auth`
or `push_worker`: run `deploy/minikube/down.ps1` once, then `up` again.

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
| `PUSH_ACCOUNTS_TOKEN` | the worker's token for the `Accounts` API, 32 to 256 visible ASCII characters; without it `Accounts` refuses every call | none |
| `PUSH_DIRECTORY_TOKEN` | auth-svc's token for the `Directory` API, 32 to 256 visible ASCII characters and different from `PUSH_ACCOUNTS_TOKEN`; without it `Directory` refuses every call | none |

`push-gateway`:

| Variable | Meaning | Default |
|----------|---------|---------|
| `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc | required |
| `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
| `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
| `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
| `PUSH_AUTH_URL` | base URL of auth-svc (`http://push-auth:8080`); the token keys come from its `/v1/auth/jwks` | required |
| `PUSH_TOKEN_AUDIENCE` | the gateway's public URL (`wss://…/v1/socket`), the `aud` access tokens must name | required |
| `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready`, 1000 to 600000 | `30000` |
| `PUSH_GROUPS` | `1` serves group requests; `0` refuses only the six group-management requests (create, add and remove members, leave, rename, set role) with `NOT_AVAILABLE`; groups that already exist (made while it was `1`, or directly through message-svc) still reach clients, and message-svc keeps serving them | `0` |
| `PUSH_CLIENT_IP_HEADER` | header with the client's IP, set by the ingress (`x-real-ip`), for the per-IP limits on redeeming invites and contact requests; same trust assumption as auth-svc's | the peer address |

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
| `PUSH_AUTH_WORKER_ROLE` | the worker's role: granted `auth.outbox` and the two cleanup functions at start-up | none |
| `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc, `http://host:50051` (plain `http://` only), asked whose profiles a caller may see; without it `GET /v1/users` answers 503 | none |
| `PUSH_DIRECTORY_TOKEN` | message-svc's token for its `Directory` API, the same value as message-svc's, 32 to 256 visible ASCII characters; required with `PUSH_MESSAGE_SVC_URL` | none |

`push-worker`:

| Variable | Meaning | Default |
|----------|---------|---------|
| `PUSH_DATABASE_URL` | Postgres URL of the `push_worker` database, as its owner | required |
| `PUSH_AUTH_DATABASE_URL` | Postgres URL of auth-svc's `push_auth` database, as the worker's role | required |
| `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc, `http://host:50051` (plain `http://` only: the worker is built without TLS) | required |
| `PUSH_ACCOUNTS_TOKEN` | message-svc's token for its `Accounts` API, the same value as message-svc's | required |
| `PUSH_EMAIL_SENDER` | `log` (logs instead of sending) or `resend` | `log` |
| `PUSH_EMAIL_FROM` | the From of every email, `Push Messenger <no-reply@push.kubegames.dk>` | required for `resend` |
| `PUSH_EMAIL_PRODUCT_NAME` | the name emails sign with | `Push Messenger` |
| `PUSH_RESEND_API_KEY` | Resend API key | required for `resend` |
| `PUSH_RESEND_API_URL` | Resend's API (`http` only for `127.0.0.1`/`localhost`) | `https://api.resend.com` |
| `PUSH_RESEND_WEBHOOK_SECRET` | the webhook's signing secret, `whsec_…`; without it the webhook is not served | none |
| `PUSH_WORKER_POLL_MS` | how often an idle worker looks for work, in milliseconds | `1000` |
| `PUSH_UNVERIFIED_ACCOUNT_DAYS` | accounts unverified this many days are deleted | `7` |
| `PUSH_WORKER_RETENTION_DAYS` | buried jobs, email records and delivery events are kept this many days; a buried `email.send` job keeps the recipient address until then | `30` |
| `PUSH_HTTP_ADDR` | address of `/v1/webhooks/resend` (and the DEV ONLY mailbox) | `0.0.0.0:8080` |
| `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
| `PUSH_DB_MAX_CONNECTIONS` | size of each of its two pools, at least 1 | `5` |
| `PUSH_DEV_MAILBOX` | `1` serves the DEV ONLY `GET /dev/mailbox`; refused with any sender but `log` | off |

## License

MIT
