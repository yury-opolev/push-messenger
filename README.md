# push-messenger

A minimal, expandable messenger backend and client SDK, written in Rust.
We build the server side and an SDK; third parties build the user interfaces.

- Design: [`docs/superpowers/specs/2026-10-11-push-messenger-design.md`](docs/superpowers/specs/2026-10-11-push-messenger-design.md)
- Roadmap: [`docs/superpowers/plans/2026-10-11-roadmap.md`](docs/superpowers/plans/2026-10-11-roadmap.md)
- Security review: [`docs/security-review-2026-10-17.md`](docs/security-review-2026-10-17.md) (OWASP Top 10:2025, all findings resolved)

## Build and test

The integration tests start `postgres:17-alpine` and `redis:7-alpine` containers through
Docker, so Docker must be running (the first run pulls the images):

    docker version
    cargo test --workspace

Building needs neither a database nor a system `protoc` (`push-proto` uses a vendored
one).

## The client SDK

`crates/push-sdk` is the Rust core that apps embed, from Rust or through its Node.js and C
bindings. A `Client` signs up and in through auth-svc, keeps the device's credentials in
a `SecretStore` the app provides (the platform's keychain), and holds the WebSocket to the
gateway, reconnecting with backoff; it signs out only when the server says so (4001) and
stops only when the device is removed (4003) or its data was opened elsewhere (4007).
Messages, edits and deletes go through an outbox in the client's SQLite database, so they
survive a restart and are stored once, and the UI follows one stream of events:

    let (client, mut events) = Client::open(Config::new(auth_url, socket_url, "push.db"), secrets)?;
    client.sign_in(email, password, "Alice's phone", Platform::Ios).await?;
    client.connect()?;
    let chat = client.open_direct(bob).await?;
    client.send(chat.id, "hello")?;
    while let Some(event) = events.next().await { /* update the UI */ }

Given a `DatabaseKey` (`Config::database_key`), the client encrypts the whole database;
an app derives the key from a PIN or password the user enters (`DatabaseKey::derive`), so
without it nobody can read the history, not even the app. The encryption is SQLite3
Multiple Ciphers, whose amalgamation is vendored in `crates/push-sdk/vendor/sqlite3mc`
byte for byte as released; every build checks it against the SHA-256s in its `VERSION`
file, and `scripts/update-sqlite3mc.ps1 -Version <x.y.z>` (or `.sh`) replaces it after
verifying the release's Sigstore signatures. The SDK ships no keychain store and never
stores the key: [docs/sdk-secure-storage.md](docs/sdk-secure-storage.md) shows how to keep
the credentials in the platform's keychain and how to derive and change the key. Groups
are not offered to apps in the first release, so the SDK has no group calls yet.

From Node.js, `bindings/node` (napi-rs) gives the same client with promises, one event
listener and the secret store as a JavaScript object (`cd bindings/node; npm test`). From
C, `bindings/c` builds a shared library with a generated header,
`bindings/c/include/push_sdk.h`: opaque handles, error codes, UTF-8 JSON and blocking calls
(`cargo test -p push-sdk-c` compiles and runs a C program against it). Both report the
same error codes as Rust: [docs/sdk-bindings.md](docs/sdk-bindings.md) is the guide.

## Run it on minikube

The whole system runs on a local [minikube](https://minikube.sigs.k8s.io/) cluster with
the same Helm chart consumers deploy with. Locally it uses minikube's ingress addon, the
retired ingress-nginx, as a compatibility path only (`values-minikube.yaml`), not as a
production recommendation. With Docker, minikube, kubectl and Helm
installed:

    deploy/minikube/up.ps1 -Smoke

starts (or reuses) the minikube profile `push`, enables its ingress addon, builds the
images into the cluster, installs the chart (`deploy/helm/push-messenger`) with
`values-minikube.yaml`, and runs the end-to-end smoke test, as a client app through the
ingress: three users sign up, verify their email (the worker's log sender delivers it),
register a device and connect its WebSocket with the access token (the gateway answers
4006, "not ready yet", until the worker has relayed the device); over the WebSocket they
become contacts by request and by invite, chat, edit, read and delete, see a group request
refused, and see their allowed profiles from auth-svc; the gateway refuses an expired token
and one for another audience; finally a revoked device and a password reset close their
sockets with 4003 and 4001. Then two apps built on `push-sdk` do the same through the
SDK: they sign up and in, chat with receipts, edit and delete, restart with messages in
their outbox (sent in order, once; one app's database encrypted), and see `OpenedElsewhere`
when a copy of an app takes over its device, `DeviceRemoved` after a revocation and
`SignedOut` after a password reset. Only the DEV ONLY mailbox is read from the worker
directly.
`deploy/minikube/up.sh --smoke` does the same on Linux
and macOS. Both wait for ingress-nginx, retry
`helm upgrade --install` up to 5 times while its admission webhook comes up, and
(with `--smoke`) wait until only one worker pod is left before the smoke test.
`deploy/minikube/down.ps1` removes everything again: namespace, the
Secret, the PostgreSQL volume and its data directory (`-Delete` deletes the profile).
`helm uninstall` alone keeps the Secret (`helm.sh/resource-policy: keep`) and the volume,
so a later install reuses both and the passwords keep matching. Every pull request is
verified this way before it is merged.

With the cluster up, `deploy/minikube/node-example.ps1` runs the Node.js binding the same
way: it forwards local ports to the ingress and to the DEV ONLY mailbox, and two Node.js
apps (`bindings/node/examples/minikube.js`) sign up, verify, sign in, become contacts by
invite and chat with a read receipt.

To verify internal API isolation on an **existing, enforcing-CNI** cluster, run
`.\deploy\minikube\test-network-policy.ps1 -Context <explicit-context>`.
It accepts `-Namespace` and `-Release` (both default to `push`) and `-ProbeImage`
(default `python:3.13-alpine`). Disposable non-root pods first prove each API is live,
then test gateway/worker/auth, unrelated and cross-release callers against all three
message-svc ports. Only timeouts count as denied connections; refusals and exec errors
fail the check. It deletes only its own named probe pods, including on failure.
The script neither provisions a CNI nor changes policies. For local acceptance, use
a separate fresh profile started with `minikube start -p push11a --driver=docker
--cni=calico --cpus=4 --memory=6g`, then
`.\deploy\minikube\up.ps1 -Cluster push11a -Smoke`. Do not install the new chart on
an existing data-bearing profile as part of this test.

The Entra ID datastore mode ([docs/deployment-azure-entra.md](docs/deployment-azure-entra.md))
is rehearsed without Azure by `.\deploy\minikube\entra-rehearsal.ps1` on its own
profile (default `push12b-entra`, Calico; it refuses the `push` profile): TLS-only
PostgreSQL and Redis and a DEV ONLY stand-in of Entra ID whose 60-second tokens are the
only datastore credentials, the migration Job, egress policies, the smoke test, `push-load`
traffic over many token rotations, killed Redis connections and restarted pods. With
`-Keep` the profile stays up for
`.\deploy\minikube\test-network-policy.ps1 -Context push12b-entra -Egress`.

The minikube values switch on a DEV ONLY shortcut: the worker, which logs emails instead
of sending them, shows them at `/dev/mailbox`, which the ingress does not route (it
reaches the worker only at `/v1/webhooks/resend`). Clients, the smoke test included,
always identify with real access tokens. The chart refuses that mode unless the
values also say `devMode.acknowledge: insecure-local-only` and the ingress host is local
(`*.local`, `*.localhost` or `*.test`), and refuses it with the Resend sender.
`pwsh deploy/helm/test-chart.ps1` checks the chart without a cluster. These contributor
checks need Python 3 and `python -m pip install --requirement deploy/helm/requirements-test.txt`
for structural YAML assertions; installing the chart itself does not need Python.
`pwsh deploy/minikube/test-upgrade-preflight.ps1` and
`bash deploy/minikube/test-upgrade-preflight.sh` exercise the bring-up refusal paths
with command mocks; they neither deploy nor replace the real upgrade rehearsal.

auth-svc and the worker have databases and roles of their own, `push_auth` and
`push_worker`, which PostgreSQL's init scripts create when it initialises an empty volume;
the worker's role may also drain auth-svc's outbox, which auth-svc grants at start-up.
message-svc and the worker share a generated Accounts token from the same Secret,
message-svc and auth-svc a Directory token, and message-svc and the gateway a Messaging
token; each internal API takes only its own. A
cluster whose Postgres volume predates them (Plans 4B and 5B) lacks a database. `up` stops
when the kept Secret `push-secrets` (it survives a plain `helm uninstall`) has no
`auth-db-password`, `worker-db-password` or `messaging-db-password`, when the running
Postgres lacks `push_auth` or `push_worker`, or when `push_messaging` is not owned by
its plain login role. It does not repair old volumes or generate a missing key.
message-svc now uses the dedicated non-superuser `push_messaging` role, not bootstrap
administrator `push`. For existing data, use the
[maintenance-window upgrade runbook](docs/deployment-upgrade.md): encrypted backup,
isolated restore rehearsal, stopped writers, exact ownership SQL, password/Secret
transition, row-hash comparisons and coordinated chart/images. **Do not run `down`
to repair an upgrade**; it deletes data.

The chart enables six application ingress NetworkPolicies by default and requires
a CNI that actually enforces them. Messaging (`50051`) accepts the same-release
gateway, Accounts (`50052`) the worker, and Directory (`50053`) auth-svc, using
`app.kubernetes.io/instance` plus `push-messenger/network-peer` caller labels in the
same namespace. API tokens remain required. Configure
`networkPolicy.ingressControllerPeers` with explicit nonempty namespace and pod label
maps for the actual controller (ANDed within each peer); the default empty list
denies public traffic. Only set `networkPolicy.enabled: false` when equivalent
consumer-managed policies enforce these boundaries. Restrict label assignment and
verify enforcement with the live checker above.

The smoke client checks the access token's audience (`socket_url` with its authority
replaced by the Host override: `ws://push.local/v1/socket` on minikube) and the refreshed
token's `sub` and `device_id`. It never prints response bodies, tokens or token-bearing
URLs, retries only on 502, 503, 504 and transport errors, and ignores `HTTP_PROXY`. The
per-IP signup limit (20/h) caps smoke runs to about 10 per hour on a long-lived cluster.

## Deploying it

This repository is a reusable core: consumers bring their own infrastructure,
controllers, certificates, Secret delivery, registries and CI/CD, and configure the
chart with values. [docs/deployment-configuration.md](docs/deployment-configuration.md)
is the contract: a consumer-managed Secret (`secrets.existingSecret`) and its key names,
offline rendering, the explicit Resend webhook switch, controller-neutral Ingress with
TLS and annotation maps, trusted client-IP headers, and image digests.
Managed PostgreSQL and Redis with Microsoft Entra ID tokens instead of passwords:
[docs/deployment-azure-entra.md](docs/deployment-azure-entra.md) (chart values, Azure
prerequisites, database roles, egress and limitations).
[docs/deployment-ingress-verification.md](docs/deployment-ingress-verification.md)
records the routing checks against a maintained controller (Traefik).
[docs/consumer-verification.md](docs/consumer-verification.md) gives the build, image,
chart packaging and verification commands, and `push-load`, an explicitly targeted,
bounded harness for connection, heartbeat, message-latency and reconnect checks with
dedicated test accounts.

## Configuration

The services read environment variables (the chart sets them). Invalid values are refused
at start-up.

`push-message-svc`:

| Variable | Meaning | Default |
|----------|---------|---------|
| `PUSH_DATABASE_URL` | Postgres URL of the `push_messaging` database, as its owner | required |
| `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
| `PUSH_GRPC_ADDR` | address of the gateway-only Messaging API | `0.0.0.0:50051` |
| `PUSH_ACCOUNTS_GRPC_ADDR` | address of the worker-only Accounts API | `0.0.0.0:50052` |
| `PUSH_DIRECTORY_GRPC_ADDR` | address of the auth-only Directory API | `0.0.0.0:50053` |
| `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
| `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
| `PUSH_ACCOUNTS_TOKEN` | the worker's token for the `Accounts` API, 32 to 256 visible ASCII characters; without it `Accounts` refuses every call | none |
| `PUSH_DIRECTORY_TOKEN` | auth-svc's token for the `Directory` API, 32 to 256 visible ASCII characters and different from `PUSH_ACCOUNTS_TOKEN`; without it `Directory` refuses every call | none |
| `PUSH_MESSAGING_TOKEN` | the gateway's token for the `Messaging` API, 32 to 256 visible ASCII characters and different from the other two; without it `Messaging` refuses every call | none |

`push-gateway`:

| Variable | Meaning | Default |
|----------|---------|---------|
| `PUSH_MESSAGE_SVC_URL` | Messaging gRPC URL of message-svc, `http://host:50051` | required |
| `PUSH_MESSAGING_TOKEN` | message-svc's token for its `Messaging` API, the same value as message-svc's | required |
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
| `PUSH_MESSAGE_SVC_URL` | Directory gRPC URL of message-svc, `http://host:50053` (plain `http://` only), asked whose profiles a caller may see; without it `GET /v1/users` answers 503 | none |
| `PUSH_DIRECTORY_TOKEN` | message-svc's token for its `Directory` API, the same value as message-svc's, 32 to 256 visible ASCII characters; required with `PUSH_MESSAGE_SVC_URL` | none |

`push-worker`:

| Variable | Meaning | Default |
|----------|---------|---------|
| `PUSH_DATABASE_URL` | Postgres URL of the `push_worker` database, as its owner | required |
| `PUSH_AUTH_DATABASE_URL` | Postgres URL of auth-svc's `push_auth` database, as the worker's role | required |
| `PUSH_MESSAGE_SVC_URL` | Accounts gRPC URL of message-svc, `http://host:50052` (plain `http://` only: the worker is built without TLS) | required |
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
