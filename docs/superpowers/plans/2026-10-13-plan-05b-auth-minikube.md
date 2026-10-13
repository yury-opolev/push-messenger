# Plan 5B — Auth Service on Minikube Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run Plan 5A's auth-svc on the local minikube cluster with the same chart that will deploy to Azure, and prove it end to end: the smoke test now signs two users up through auth-svc's public API, verifies their email with the link from the DEV ONLY mailbox, signs them in, registers a device each (and retries the registration, which must return the same device), checks the access tokens against the JWKS, refreshes them, and then — as before — delivers a message from one to the other through the gateway. Deliverables: an `auth` image target; auth-svc in the chart with a database and a role of its own (`push_auth`, created by a PostgreSQL init script), generated signing and link keys in the Secret, an Ingress for `/v1/auth`, `/v1/devices` and `/v1/users`, and `PUSH_DEV_MAILBOX` behind the existing dev-mode guard; a preStop sleep and a grace period for auth-svc, as the gateway has; the smoke test extended; the bring-up scripts building the new image and stopping at once on a PostgreSQL volume without `push_auth`; CI building it; `test-chart.ps1` with 20 checks.

**Architecture:** The Dockerfile's shared builder also builds `push-auth`; a fourth distroless target runs it. In the chart, auth-svc is a Deployment (HTTP 8080, health 8081) behind a ClusterIP Service and a second Ingress on the same host (`push.local` on minikube). PostgreSQL's `/docker-entrypoint-initdb.d` gets a script from a ConfigMap that creates the role and database `push_auth` (the password comes from the Secret through the environment) and revokes `CONNECT` from `PUBLIC` on both databases, which keeps auth-svc's plain role out of `push_messaging` (message-svc still connects as the superuser `push`, which reaches every database, until Plan 11). auth-svc's public URL and token audience come from `ingress.scheme` and `ingress.host` (`http://push.local` and `ws://push.local/v1/socket` on minikube). The smoke Pod reaches auth-svc's public API through the ingress controller with `Host: push.local`, like a client, and the DEV ONLY mailbox straight at the auth Service, which the ingress does not route; it mirrors the users and devices auth-svc registered into message-svc through `DevSetup`, standing in for the worker's relay (Plan 6), and identifies on the WebSocket with those ids through the gateway's DEV ONLY identity (until Plan 8). Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §2, §5, §7.6, §11; roadmap "Running locally on minikube".

**Tech Stack:** Docker (BuildKit), minikube 1.34 (Kubernetes 1.31, docker driver, ingress addon), kubectl, Helm 3.16, PowerShell 7 and bash; new Rust dependency `reqwest` 0.12 (0.12.28, default features off, `json`: plain HTTP inside the cluster) for the smoke test, which also uses `jsonwebtoken` 9 from Plan 5A.

---

## Before you start

- **Plan 5A is merged.** This plan packages, deploys and smoke-tests `push-auth`; it
  changes no service code.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-05b-auth-minikube
```

- Fix a stale PATH for the current PowerShell session (each new shell needs it again):

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
```

- **Docker Desktop must be running**, with the resources of Plan 4B (the minikube node
  gets 4 CPUs and 6 GB). The minikube profile is `push` (Kubernetes 1.31). Note the
  current kubectl context first and restore it at the end — minikube switches it to
  `push`:

```powershell
kubectl config current-context
```

- **A cluster that still holds an earlier release must be wiped once.** PostgreSQL runs
  its init scripts only when it initialises an empty volume, so a volume from Plan 4B has
  no `push_auth` database and auth-svc would never become ready (Task 3's `up` scripts
  notice and stop with "run down.ps1 first"). A volume from an earlier run of this plan
  with another version of the migration fails differently: auth-svc logs
  `migration 1 was previously applied but has been modified` and never becomes ready.
  Task 3 runs `deploy/minikube/down.ps1` before `up.ps1`. Never delete the profile, and
  never touch other Docker containers on this machine.
- Helm may print `failed to load plugins … helm-diff … unknown field "platformHooks"`
  first: an outdated plugin, harmless.
- **Edit files with the edit tool, and keep them LF** (`.gitattributes`:
  `* text=auto eol=lf`). This matters for `up.sh`, which bash refuses with CRLF on Linux.
- Baseline (after Plan 5A): `cargo test --workspace --locked` passes 700 tests;
  `deploy/helm/test-chart.ps1` passes 12 checks.
- **Every task ends green:**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

  plus the task's own checks (`test-chart.ps1`, image builds, minikube).
- **Scope rule:** no changes to `push-auth`, `push-message-svc`, `push-gateway` or the
  other library crates; no TLS, Key Vault or registry pushes (the Azure plan); no worker
  (Plan 6).

## Key rules the configuration must preserve

- **One database per service.** auth-svc gets the URL of `push_auth` as the plain role
  `push_auth`, which cannot connect to `push_messaging`, the Redis URL and its two keys —
  never message-svc's database URL; the gateway still gets no database credentials.
  (message-svc still connects as the superuser `push` until Plan 11, which bypasses
  `CONNECT` grants.)
- **No secret in git or in a ConfigMap.** The auth database password, the Ed25519 seed
  and the HMAC key are generated on install (`randAlphaNum 32`, `randBytes 32`), kept on
  upgrades by reading the Secret back (`lookup`), and reach the containers only through
  `secretKeyRef`. The init script's ConfigMap reads the password from the environment.
- **The DEV ONLY mailbox cannot reach a real deployment by accident.** `PUSH_DEV_MAILBOX`
  follows `devMode` (acknowledgement and a local host required), and the ingress routes
  only `/v1/auth`, `/v1/devices`, `/v1/users` and `/v1/socket`, never `/dev`.
- **Links and tokens name the public host.** The public URL and the token audience come
  from the ingress values, never from a request; plain `http`/`ws` only for a local host —
  the chart refuses anything else, as auth-svc itself does.
- **Our containers stay hardened** (Plan 4B): non-root uid 65532, read-only root
  filesystem, no privilege escalation, all capabilities dropped, no service-account token.

## Decisions made for this plan

These were settled while writing the plan. Do not reopen them during execution.

1. **auth-svc's database is created by a PostgreSQL init script.** A ConfigMap mounted at
   `/docker-entrypoint-initdb.d` holds `10-push-auth.sh`, which the image's entrypoint
   sources as the superuser while it initialises the volume: `CREATE ROLE push_auth LOGIN
   PASSWORD :'password'` (a `psql` variable from `PUSH_AUTH_DB_PASSWORD`, which the
   StatefulSet takes from the Secret), `CREATE DATABASE push_auth OWNER push_auth`, and
   `REVOKE ALL ON DATABASE … FROM PUBLIC` for both databases, so auth-svc cannot even
   connect to `push_messaging` (verified on minikube). That does not hold message-svc to
   its database: its role `push` is the image's superuser, which ignores `CONNECT` grants,
   until Plan 11 gives it a plain role. auth-svc is a plain role, not a superuser; its migration creates `citext` as the database owner (Plan 5A, Task 1). An
   init script runs only on an empty volume: clusters from Plan 4B are wiped once with
   `down.ps1` (no data worth keeping exists). Alternatives were rejected: a Helm hook Job
   would deadlock `helm --wait` (auth-svc is not ready until the database exists), and an
   init container in auth-svc's pod would need the superuser password there.
2. **Keys** are generated like the passwords: `randBytes 32` returns base64 of 32 random
   bytes, exactly what `PUSH_AUTH_SIGNING_KEY` and `PUSH_AUTH_LINK_KEY` take. Values
   `auth.signingKey`, `auth.linkKey` and `postgres.authPassword` can set them.
3. **A second Ingress** routes `Prefix` `/v1/auth`, `/v1/devices` and `/v1/users` to
   auth-svc on the same host, with `proxy-body-size: 16k`; the WebSocket Ingress keeps its
   long timeouts. auth-svc reads the client IP from `X-Real-IP`, which ingress-nginx
   overwrites with the address it saw.
4. **`ingress.scheme`** (`https` by default, `http` in `values-minikube.yaml`) builds
   `PUSH_AUTH_PUBLIC_URL` (`<scheme>://<host>`) and `PUSH_AUTH_TOKEN_AUDIENCE`
   (`ws[s]://<host>/v1/socket`). The `push.publicUrl` helper refuses `http` with a
   non-local host, so a misconfiguration fails at `helm install`, not at pod start.
5. **The smoke test enrolls its users through auth-svc** (`--auth`, `--mailbox`, both
   required now). For each user: wait until the API answers (an ingress answers 503 for a
   few seconds after its backends become ready) and fetch the JWKS; sign up with a fresh
   address `<name>.<10 hex>@smoke.test` and handle `<name>_<10 hex>`; read the verification
   link from the mailbox; open the page through the ingress and check
   `Referrer-Policy: no-referrer`; post the token; sign in; register a device with a fresh
   `registration_id` and register again with the same one, which must return the same
   device (Plan 5A, decision 14); check the
   access token's signature against the JWKS and its `sub`, `device_id` and
   `email_verified` (not `aud`: the smoke test does not know the gateway's public URL; it
   prints it); refresh once. Then it mirrors both users and devices into message-svc with
   `DevSetup` — what the worker's relay will do in Plan 6 — and runs the chat of Plan 4A
   with the ids auth-svc assigned.
6. **`reqwest` 0.12 without default features** (no TLS, `json` only): the smoke test
   talks plain HTTP inside the cluster, like its WebSockets (TLS comes with the Azure
   plan). 0.12.28 builds on 1.85; 0.13 is not picked by the resolver.
7. **Resources:** auth-svc requests 100m CPU and 64 Mi, limited to 1 CPU and 256 Mi —
   two concurrent Argon2id hashes take 38 MiB.
8. **CI** builds the `auth` image too; the `chart` job runs the extended
   `test-chart.ps1`. The minikube run stays the merge gate, recorded in the pull request.
9. **auth-svc drains like the gateway:** a native `preStop` `sleep` of 5 s (the image has
   no shell) lets Kubernetes take the pod out of the Service and Ingress endpoints before
   SIGTERM, then auth-svc finishes the requests in progress within
   `terminationGracePeriodSeconds: 30` (a password hash waits at most 2 s for its turn).
10. **The up scripts check an existing volume first.** When the release's PostgreSQL
    StatefulSet exists, `up.ps1` and `up.sh` wait for it and ask it whether `push_auth`
    exists; without it they stop at once with "run down.ps1 first" instead of five
    10-minute `helm upgrade` attempts. A fresh install has no StatefulSet yet and skips the
    check.

## File structure

```
Cargo.toml                                    + reqwest
crates/push-smoke/Cargo.toml                  + jsonwebtoken, reqwest, serde, serde_json;
                                                dev: push-auth, sqlx
crates/push-smoke/src/auth.rs                 NEW: enrolling a user through auth-svc
crates/push-smoke/src/lib.rs                  --auth/--mailbox; enroll, mirror, chat
crates/push-smoke/tests/local.rs              auth-svc in the local stack
deploy/docker/Dockerfile                      + the auth target
deploy/helm/push-messenger/
├── values.yaml                               images.auth, auth.*, postgres.authPassword,
│                                               ingress.scheme, smoke.authUrl
├── values-minikube.yaml                      ingress.scheme: http
└── templates/
    ├── _helpers.tpl                          push.publicUrl, push.socketUrl
    ├── secret.yaml                           auth password, URL and keys
    ├── postgres-init.yaml                    NEW: the init script (no credentials)
    ├── postgres.yaml                         mounts it, passes the password
    ├── auth.yaml                             NEW: Service + Deployment (preStop, grace)
    ├── ingress.yaml                          + the auth Ingress
    ├── tests/smoke.yaml                      --auth, --mailbox
    └── NOTES.txt                             auth endpoints
deploy/helm/test-chart.ps1                    20 checks
deploy/minikube/up.ps1, up.sh                 build and roll the auth image; refuse a
                                                volume without push_auth
.github/workflows/ci.yml                      build the auth image
README.md, spec §11, roadmap                  docs
```

---

### Task 1: The smoke test enrolls its users through auth-svc

**Files:**
- Modify: `Cargo.toml`, `crates/push-smoke/Cargo.toml`, `crates/push-smoke/src/lib.rs`,
  `crates/push-smoke/tests/local.rs`
- Create: `crates/push-smoke/src/auth.rs`

- [ ] **Step 1: Dependencies**

Replace `Cargo.toml` with (adds `reqwest`):

```toml
[workspace]
resolver = "3"
members = [
    "crates/push-auth",
    "crates/push-domain",
    "crates/push-gateway",
    "crates/push-message-svc",
    "crates/push-proto",
    "crates/push-smoke",
    "crates/push-store",
    "crates/push-testkit",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
license = "MIT"
repository = "https://github.com/yury-opolev/push-messenger"

[workspace.dependencies]
anyhow = "1"
argon2 = { version = "0.5", features = ["std"] }
async-trait = "0.1"
axum = "0.8"
base64 = "0.22"
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
futures-util = { version = "0.3", default-features = false, features = ["sink"] }
hmac = "0.12"
http-body-util = "0.1"
jsonwebtoken = { version = "9", default-features = false }
libtest-mimic = "0.8"
prost = "0.14"
protoc-bin-vendored = "3"
push-auth = { path = "crates/push-auth" }
push-domain = { path = "crates/push-domain" }
push-gateway = { path = "crates/push-gateway" }
push-message-svc = { path = "crates/push-message-svc" }
push-proto = { path = "crates/push-proto" }
push-store = { path = "crates/push-store" }
push-testkit = { path = "crates/push-testkit" }
rand = "0.9"
redis = { version = "1", default-features = false, features = [
    "connection-manager",
    "tokio-comp",
] }
reqwest = { version = "0.12", default-features = false, features = ["json"] }
ring = "0.17"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
sqlx = { version = "0.8", default-features = false, features = [
    "chrono",
    "json",
    "macros",
    "migrate",
    "postgres",
    "runtime-tokio",
    "uuid",
] }
testcontainers-modules = { version = "0.13", features = ["postgres", "redis"] }
thiserror = "2"
tokio = { version = "1", features = ["sync"] }
tokio-stream = { version = "0.1", features = ["net"] }
tokio-tungstenite = "0.29"
tokio-util = { version = "0.7", features = ["rt"] }
tonic = "0.14"
tonic-prost = "0.14"
tonic-prost-build = "0.14"
tower = { version = "0.5", features = ["util"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
uuid = { version = "1", features = ["serde", "v4", "v7"] }

# Argon2id is far too slow unoptimised, and the auth tests hash many passwords.
[profile.dev.package.argon2]
opt-level = 3

[profile.dev.package.blake2]
opt-level = 3

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
```

Replace `crates/push-smoke/Cargo.toml` with:

```toml
[package]
name = "push-smoke"
description = "End-to-end smoke test of a running push-messenger deployment."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
anyhow.workspace = true
futures-util.workspace = true
jsonwebtoken.workspace = true
push-proto.workspace = true
reqwest.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "time"] }
tokio-tungstenite.workspace = true
tonic.workspace = true
uuid.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["io-util"] }
push-auth.workspace = true
push-gateway.workspace = true
push-message-svc.workspace = true
push-store.workspace = true
sqlx.workspace = true
testcontainers-modules.workspace = true
tokio-util.workspace = true

[lints]
workspace = true
```

- [ ] **Step 2: Put auth-svc into the local stack (the failing test)**

The local test now also runs auth-svc in-process, with a database `push_auth` of its own
in the same Postgres container (as in the chart) and the DEV ONLY mailbox on, and gives
the smoke test both URLs. Replace `crates/push-smoke/tests/local.rs` with:

```rust
//! The walking skeleton on this machine: Postgres and Redis in containers (Docker must be
//! running), auth-svc, message-svc and the gateway in this process, and the smoke test
//! against them over real HTTP, gRPC and WebSockets.

use std::net::SocketAddr;
use std::time::Duration;

use push_smoke::{Target, run};
use push_store::StoreOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn the_smoke_test_passes_against_the_whole_stack() {
    let postgres = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start Postgres (is Docker running?)");
    let redis = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start Redis");
    let database_url = format!(
        "postgres://postgres:postgres@127.0.0.1:{}/postgres",
        postgres.get_host_port_ipv4(5432).await.unwrap()
    );
    let redis_url = format!(
        "redis://127.0.0.1:{}",
        redis.get_host_port_ipv4(6379).await.unwrap()
    );
    let shutdown = CancellationToken::new();

    // auth-svc has a database of its own in the same server, as in the chart.
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    sqlx::query("CREATE DATABASE push_auth")
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    let (server_url, _) = database_url.rsplit_once('/').unwrap();
    let auth_database_url = format!("{server_url}/push_auth");
    let auth_http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth_health_addr = auth_health.local_addr().unwrap();
    let auth_url = format!("http://{}", auth_http.local_addr().unwrap());
    let auth_variables = [
        ("PUSH_DATABASE_URL", auth_database_url.as_str()),
        ("PUSH_REDIS_URL", redis_url.as_str()),
        ("PUSH_AUTH_PUBLIC_URL", "http://push.local"),
        ("PUSH_AUTH_TOKEN_AUDIENCE", "ws://push.local/v1/socket"),
        // base64 of 32 bytes of 0x07 and of 0x09.
        (
            "PUSH_AUTH_SIGNING_KEY",
            "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=",
        ),
        (
            "PUSH_AUTH_LINK_KEY",
            "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=",
        ),
        ("PUSH_DEV_MAILBOX", "1"),
    ];
    let auth_config = push_auth::Config::from_lookup(|name| {
        auth_variables
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    })
    .unwrap();
    let auth = tokio::spawn(push_auth::serve(
        auth_config,
        auth_http,
        auth_health,
        shutdown.clone(),
    ));

    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_health_addr = message_svc_health.local_addr().unwrap();
    let message_svc_url = format!("http://{}", grpc.local_addr().unwrap());
    let message_svc = tokio::spawn(push_message_svc::serve(
        push_message_svc::Config {
            database_url,
            redis_url: redis_url.clone(),
            grpc_addr: grpc.local_addr().unwrap(),
            health_addr: message_svc_health.local_addr().unwrap(),
            store: StoreOptions::default(),
            dev_setup: true,
        },
        grpc,
        message_svc_health,
        shutdown.clone(),
    ));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_health_addr = gateway_health.local_addr().unwrap();
    let socket_url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
    let gateway = tokio::spawn(push_gateway::serve(
        push_gateway::Config {
            message_svc_url: message_svc_url.clone(),
            redis_url,
            listen_addr: listener.local_addr().unwrap(),
            health_addr: gateway_health.local_addr().unwrap(),
            heartbeat: Duration::from_secs(30),
            dev_auth: true,
        },
        listener,
        gateway_health,
        shutdown.clone(),
    ));

    wait_until_ready("auth-svc", auth_health_addr, &auth).await;
    wait_until_ready("message-svc", message_svc_health_addr, &message_svc).await;
    wait_until_ready("the gateway", gateway_health_addr, &gateway).await;

    let target = Target {
        mailbox_url: auth_url.clone(),
        auth_url,
        setup_url: message_svc_url,
        socket_url,
        host: None,
    };
    run(&target).await.expect("the smoke test passes");

    shutdown.cancel();
    message_svc
        .await
        .unwrap()
        .expect("message-svc stops cleanly");
    gateway.await.unwrap().expect("the gateway stops cleanly");
    auth.await.unwrap().expect("auth-svc stops cleanly");
}

/// Polls `/readyz` of a server task until it answers 200; fails if the task stops first.
async fn wait_until_ready<T: std::fmt::Debug>(
    name: &str,
    health: SocketAddr,
    server: &JoinHandle<T>,
) {
    for _ in 0..240 {
        assert!(
            !server.is_finished(),
            "{name} stopped before it became ready"
        );
        let probe = tokio::time::timeout(Duration::from_secs(2), readyz_status(health));
        if matches!(probe.await, Ok(200)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{name} did not become ready within 60 s");
}

async fn readyz_status(addr: SocketAddr) -> u16 {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return 0;
    };
    let request = "GET /readyz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n";
    if stream.write_all(request.as_bytes()).await.is_err() {
        return 0;
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).await.is_err() {
        return 0;
    }
    response
        .get(9..12)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}
```

```powershell
cargo test -p push-smoke --test local
```

Expected: `Adding reqwest v0.12.28 (available: v0.13.5)` and a few other packages on the
first run, then a compile error — `struct Target has no field named mailbox_url` (and
`auth_url`).

- [ ] **Step 3: Enrolling through auth-svc**

The device is registered twice with one `registration_id`: the second answer must name
the same device, and its tokens are the ones used afterwards (the retry revoked the first
refresh token). Create `crates/push-smoke/src/auth.rs`:

```rust
//! The auth part of the smoke test: a user signs up, verifies the email through the
//! DEV ONLY mailbox, signs in, registers a device (and retries the registration, which
//! must find the same device) and refreshes its tokens, all over auth-svc's public HTTP
//! API, and checks the access token against the JWKS.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{CONNECT_TIMEOUT, Target, step};

/// A signed-in device.
pub(crate) struct Enrolled {
    pub user: Uuid,
    pub device: Uuid,
}

/// The access-token claims the smoke test checks.
#[derive(Debug, Deserialize)]
struct Claims {
    sub: Uuid,
    device_id: Uuid,
    email_verified: bool,
    aud: String,
}

/// auth-svc as a client sees it: the public API, through the ingress, and the DEV ONLY
/// mailbox, straight at the service.
pub(crate) struct Auth<'a> {
    http: Client,
    target: &'a Target,
    jwks: JwkSet,
}

impl<'a> Auth<'a> {
    /// Waits until the public API answers (an ingress can answer 503 for a few seconds
    /// after its backends are ready) and fetches the JWKS.
    pub async fn connect(target: &'a Target, timeout: Duration) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .context("build the HTTP client")?;
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let jwks = loop {
            let answer = public(&http, target, reqwest::Method::GET, "/v1/auth/jwks")
                .send()
                .await;
            match answer {
                Ok(response) if response.status() == StatusCode::OK => {
                    break response.json::<JwkSet>().await.context("read the JWKS")?;
                }
                Ok(response) if Instant::now() < deadline => {
                    step(&format!(
                        "auth-svc answers {} yet; retrying",
                        response.status()
                    ));
                }
                Err(error) if Instant::now() < deadline => {
                    step(&format!(
                        "auth-svc cannot be reached yet ({error}); retrying"
                    ));
                }
                Ok(response) => bail!("GET /v1/auth/jwks answered {}", response.status()),
                Err(error) => return Err(error).context("GET /v1/auth/jwks"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        Ok(Self { http, target, jwks })
    }

    fn post(&self, path: &str, body: Value) -> RequestBuilder {
        public(&self.http, self.target, reqwest::Method::POST, path).json(&body)
    }

    /// Signs `name` up with a fresh address and handle, verifies the address with the
    /// link from the dev mailbox, signs in, registers a device, retries the registration
    /// with the same `registration_id` and refreshes its tokens.
    pub async fn enroll(&self, name: &str) -> anyhow::Result<Enrolled> {
        let started = Instant::now();
        let suffix = Uuid::now_v7().simple().to_string();
        let suffix = &suffix[suffix.len() - 10..];
        let email = format!("{name}.{suffix}@smoke.test");
        let handle = format!("{name}_{suffix}");
        let password = format!("smoke-{}", Uuid::now_v7());

        expect(
            self.post(
                "/v1/auth/signup",
                json!({
                    "email": email,
                    "password": password,
                    "handle": handle,
                    "display_name": name,
                }),
            ),
            StatusCode::ACCEPTED,
            "sign up",
        )
        .await?;

        let token = self.verification_token(&email).await?;
        let page = public(
            &self.http,
            self.target,
            reqwest::Method::GET,
            &format!("/v1/auth/verify?token={token}"),
        )
        .send()
        .await
        .context("open the verification page")?;
        if page.status() != StatusCode::OK
            || page
                .headers()
                .get("referrer-policy")
                .is_none_or(|value| value != "no-referrer")
        {
            bail!(
                "the verification page answered {} without Referrer-Policy: no-referrer",
                page.status()
            );
        }
        expect(
            self.post("/v1/auth/verify", json!({"token": token})),
            StatusCode::OK,
            "verify the email",
        )
        .await?;

        let login = expect(
            self.post(
                "/v1/auth/login",
                json!({"email": email, "password": password}),
            ),
            StatusCode::OK,
            "sign in",
        )
        .await?;
        let registration = field(&login, "registration_token")?;
        // The client's idempotency key: the retry below must find the same device.
        let registration_id = Uuid::now_v7();
        let body = json!({
            "name": format!("{name}'s smoke device"),
            "platform": "other",
            "registration_id": registration_id,
        });
        let first = expect(
            public(
                &self.http,
                self.target,
                reqwest::Method::POST,
                "/v1/devices",
            )
            .bearer_auth(&registration)
            .json(&body),
            StatusCode::CREATED,
            "register a device",
        )
        .await?;
        let device = expect(
            public(
                &self.http,
                self.target,
                reqwest::Method::POST,
                "/v1/devices",
            )
            .bearer_auth(&registration)
            .json(&body),
            StatusCode::CREATED,
            "retry the registration",
        )
        .await?;
        if field(&device, "device_id")? != field(&first, "device_id")? {
            bail!("a retried registration of {name}'s device created a second device");
        }
        let user: Uuid = field(&device, "user_id")?.parse()?;
        let device_id: Uuid = field(&device, "device_id")?.parse()?;
        step(&format!(
            "{name} signed up, verified the email from the dev mailbox, signed in and registered device {device_id} (a retry returned the same device) in {} ms",
            started.elapsed().as_millis()
        ));

        let claims = self.check_access_token(&field(&device, "access_token")?)?;
        if claims.sub != user || claims.device_id != device_id || !claims.email_verified {
            bail!("{name}'s access token has the wrong claims: {claims:?}");
        }
        let refreshed = expect(
            self.post(
                "/v1/auth/refresh",
                json!({"refresh_token": field(&device, "refresh_token")?}),
            ),
            StatusCode::OK,
            "refresh the tokens",
        )
        .await?;
        if field(&refreshed, "device_id")? != device_id.to_string() {
            bail!("refreshing returned another device: {refreshed}");
        }
        self.check_access_token(&field(&refreshed, "access_token")?)?;
        step(&format!(
            "{name}'s access token verifies against the JWKS (EdDSA, aud {}); refresh rotated it",
            claims.aud
        ));
        Ok(Enrolled {
            user,
            device: device_id,
        })
    }

    /// The token of the newest verification link sent to `email`, from the dev mailbox.
    async fn verification_token(&self, email: &str) -> anyhow::Result<String> {
        let url = format!(
            "{}/dev/mailbox?to={email}",
            self.target.mailbox_url.trim_end_matches('/')
        );
        let mailbox: Value = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(Response::error_for_status)
            .with_context(|| format!("read the dev mailbox at {url}"))?
            .json()
            .await?;
        let link = mailbox["emails"]
            .as_array()
            .and_then(|emails| {
                emails
                    .iter()
                    .find(|email| email["template"] == "verify_email")
            })
            .and_then(|email| email["params"]["link"].as_str())
            .with_context(|| format!("no verification email for {email}: {mailbox}"))?;
        let (_, token) = link
            .split_once("?token=")
            .with_context(|| format!("a verification link without a token: {link}"))?;
        Ok(token.to_owned())
    }

    /// Checks an access token's signature against the JWKS, as the gateway will.
    fn check_access_token(&self, token: &str) -> anyhow::Result<Claims> {
        let header = jsonwebtoken::decode_header(token).context("decode the token header")?;
        let kid = header.kid.context("the token has no kid")?;
        let jwk = self
            .jwks
            .find(&kid)
            .with_context(|| format!("the JWKS has no key {kid}"))?;
        let key = DecodingKey::from_jwk(jwk).context("load the JWK")?;
        let mut validation = Validation::new(Algorithm::EdDSA);
        // The audience is the gateway's URL, which the smoke test does not know; it
        // checks the signature and the identity claims.
        validation.validate_aud = false;
        let data = jsonwebtoken::decode::<Claims>(token, &key, &validation)
            .context("the access token does not verify against the JWKS")?;
        Ok(data.claims)
    }
}

/// A request to the public API: through the ingress, with its host as `Host`.
fn public(http: &Client, target: &Target, method: reqwest::Method, path: &str) -> RequestBuilder {
    let url = format!("{}{path}", target.auth_url.trim_end_matches('/'));
    let request = http.request(method, url);
    match &target.host {
        Some(host) => request.header(reqwest::header::HOST, host),
        None => request,
    }
}

/// Sends `request`; its JSON body if the status is `expected`.
async fn expect(
    request: RequestBuilder,
    expected: StatusCode,
    what: &str,
) -> anyhow::Result<Value> {
    let response = request
        .send()
        .await
        .with_context(|| format!("{what}: request failed"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status != expected {
        bail!("{what}: expected {expected}, got {status}: {body}");
    }
    Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn field(value: &Value, name: &str) -> anyhow::Result<String> {
    value[name]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("no {name} in {value}"))
}
```

- [ ] **Step 4: The target and the run**

`Target` gains `auth_url` and `mailbox_url` (flags `--auth`, `--mailbox`; variables
`PUSH_SMOKE_AUTH_URL`, `PUSH_SMOKE_MAILBOX_URL`); `run` enrolls Alice and Bob first and
`Client::register` becomes `Client::mirror`, which registers the ids auth-svc assigned.
Replace `crates/push-smoke/src/lib.rs` with:

```rust
//! push-smoke: the end-to-end smoke test of a running push-messenger.
//!
//! Against a deployment it:
//!
//! 1. enrolls Alice and Bob through auth-svc's public API: each signs up, verifies the
//!    email with the link from the DEV ONLY mailbox, signs in, registers a device, checks
//!    the access token against the JWKS and refreshes it;
//! 2. mirrors both users and devices into message-svc and makes them accepted contacts
//!    with a direct conversation, through message-svc's DEV ONLY `DevSetup` API (the
//!    worker will relay auth-svc's events instead, Plan 6);
//! 3. connects Bob's WebSocket and then Alice's, each with a DEV ONLY `Identify` naming
//!    the device auth-svc registered, and waits for `Ready`;
//! 4. sends a message from Alice and waits for her `SendAck`;
//! 5. waits for Bob's `Deliver` of that message;
//! 6. acks it from Bob, then pings: the `Pong` proves the ack went through, since the
//!    gateway handles a connection's frames in order and answers a failed ack with an
//!    `Error`.
//!
//! The deployment must run auth-svc with `PUSH_DEV_MAILBOX=1`, message-svc with
//! `PUSH_DEV_SETUP=1` and the gateway with `PUSH_DEV_AUTH=1`.

mod auth;

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::{
    MakeContactsRequest, OpenDirectRequest, RegisterDeviceRequest, RegisterUserRequest,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, ContentType, Identify, Ping, Send};
use push_proto::{decode_frame, encode_frame, id_bytes};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tonic::transport::Endpoint;
use uuid::Uuid;

/// How long each step may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to keep trying to connect: an ingress may answer 503 for a few seconds after
/// its backends become ready.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a closing handshake may take; the test has passed by then.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Where the deployment is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Base URL of auth-svc's public API: `http://host` (the ingress controller).
    pub auth_url: String,
    /// Base URL of auth-svc itself, for the DEV ONLY mailbox, which the ingress does not
    /// route: `http://push-auth:8080`.
    pub mailbox_url: String,
    /// gRPC URL of message-svc, for the dev setup calls: `http://host:50051`.
    pub setup_url: String,
    /// WebSocket URL of the gateway: `ws://host/v1/socket`.
    pub socket_url: String,
    /// `Host` header for the WebSocket and public API requests, when the URLs name an
    /// ingress controller rather than the virtual host it routes (`push.local`).
    pub host: Option<String>,
}

/// The usage line of the `push-smoke` binary.
pub const USAGE: &str = "usage: push-smoke --auth <url> --mailbox <url> --setup <grpc-url> --socket <ws-url> [--host <host>]";

impl Target {
    /// Reads `--auth`, `--mailbox`, `--setup`, `--socket` and `--host` from `args`; each one
    /// missing there is read from `env` (`PUSH_SMOKE_AUTH_URL`, `PUSH_SMOKE_MAILBOX_URL`,
    /// `PUSH_SMOKE_SETUP_URL`, `PUSH_SMOKE_SOCKET_URL`, `PUSH_SMOKE_HOST`).
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let (mut auth_url, mut mailbox_url) = (None, None);
        let (mut setup_url, mut socket_url, mut host) = (None, None, None);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let slot = match flag.as_str() {
                "--auth" => &mut auth_url,
                "--mailbox" => &mut mailbox_url,
                "--setup" => &mut setup_url,
                "--socket" => &mut socket_url,
                "--host" => &mut host,
                other => return Err(format!("unknown argument {other:?}; {USAGE}")),
            };
            *slot = Some(args.next().ok_or_else(|| format!("{flag} needs a value"))?);
        }
        let auth_url = auth_url
            .or_else(|| env("PUSH_SMOKE_AUTH_URL"))
            .ok_or_else(|| format!("--auth is required; {USAGE}"))?;
        let mailbox_url = mailbox_url
            .or_else(|| env("PUSH_SMOKE_MAILBOX_URL"))
            .ok_or_else(|| format!("--mailbox is required; {USAGE}"))?;
        let setup_url = setup_url
            .or_else(|| env("PUSH_SMOKE_SETUP_URL"))
            .ok_or_else(|| format!("--setup is required; {USAGE}"))?;
        let socket_url = socket_url
            .or_else(|| env("PUSH_SMOKE_SOCKET_URL"))
            .ok_or_else(|| format!("--socket is required; {USAGE}"))?;
        let host = host
            .or_else(|| env("PUSH_SMOKE_HOST"))
            .filter(|host| !host.is_empty());
        Ok(Self {
            auth_url,
            mailbox_url,
            setup_url,
            socket_url,
            host,
        })
    }
}

/// One user with one device.
struct Client {
    name: &'static str,
    user: Uuid,
    device: Uuid,
}

/// Runs the smoke test, printing each step. Fails at the first step that does not
/// complete within 10 s.
pub async fn run(target: &Target) -> anyhow::Result<()> {
    let started = Instant::now();
    let auth = auth::Auth::connect(target, STEP_TIMEOUT).await?;
    let alice = auth.enroll("alice").await?;
    let bob = auth.enroll("bob").await?;

    let channel = Endpoint::from_shared(target.setup_url.clone())
        .with_context(|| format!("message-svc URL {}", target.setup_url))?
        .connect_timeout(STEP_TIMEOUT)
        .timeout(STEP_TIMEOUT)
        .connect()
        .await
        .with_context(|| format!("connect to message-svc at {}", target.setup_url))?;
    let mut setup = DevSetupClient::new(channel);
    let alice = Client::mirror("alice", alice, &mut setup).await?;
    let bob = Client::mirror("bob", bob, &mut setup).await?;
    setup
        .make_contacts(MakeContactsRequest {
            user_id: id_bytes(alice.user),
            peer_id: id_bytes(bob.user),
        })
        .await
        .context("make Alice and Bob contacts")?;
    let conversation = setup
        .open_direct(OpenDirectRequest {
            user_id: id_bytes(alice.user),
            peer_id: id_bytes(bob.user),
        })
        .await
        .context("open their direct conversation")?
        .into_inner()
        .conversation_id;
    step(&format!(
        "mirrored alice={} bob={} into message-svc as contacts with a direct conversation",
        alice.user, bob.user
    ));

    let mut bob_socket = bob.connect(target).await?;
    let mut alice_socket = alice.connect(target).await?;

    let client_msg_id = id_bytes(Uuid::now_v7());
    let text = format!("hello from push-smoke {}", Uuid::now_v7());
    let sent_at = Instant::now();
    send(
        &mut alice_socket,
        Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.clone(),
            content_type: ContentType::Text.into(),
            body: text.clone().into_bytes(),
            client_ts: None,
        }),
    )
    .await?;
    let ack = loop {
        match next(&mut alice_socket, "alice's SendAck").await? {
            Body::SendAck(ack) if ack.client_msg_id == client_msg_id => break ack,
            Body::Error(error) => bail!("alice's send failed: {error:?}"),
            // Alice's own devices also receive her message.
            _ => {}
        }
    };
    step(&format!(
        "alice got SendAck seq={} after {} ms",
        ack.seq,
        sent_at.elapsed().as_millis()
    ));

    let entry = loop {
        match next(&mut bob_socket, "bob's Deliver").await? {
            Body::Deliver(entry)
                if entry.seq == ack.seq && entry.conversation_id == conversation =>
            {
                break entry;
            }
            Body::Error(error) => bail!("bob got an error: {error:?}"),
            _ => {}
        }
    };
    if entry.body.as_deref() != Some(text.as_bytes()) || entry.sender_id != id_bytes(alice.user) {
        bail!("bob received the wrong message: {entry:?}");
    }
    step(&format!(
        "bob got Deliver seq={} after {} ms",
        entry.seq,
        sent_at.elapsed().as_millis()
    ));

    send(
        &mut bob_socket,
        Body::Ack(Ack {
            conversation_id: conversation,
            seq: entry.seq,
        }),
    )
    .await?;
    send(&mut bob_socket, Body::Ping(Ping {})).await?;
    loop {
        match next(&mut bob_socket, "bob's Pong").await? {
            Body::Pong(_) => break,
            Body::Error(error) => bail!("bob's ack failed: {error:?}"),
            _ => {}
        }
    }
    step("bob acked; the gateway answered the following Ping");

    for socket in [&mut alice_socket, &mut bob_socket] {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.close(None)).await;
    }
    step(&format!("PASS in {} ms", started.elapsed().as_millis()));
    Ok(())
}

impl Client {
    /// Registers a user and device that auth-svc enrolled with message-svc, as the
    /// worker's relay of `user.verified` and `device.registered` will (Plan 6).
    async fn mirror(
        name: &'static str,
        enrolled: auth::Enrolled,
        setup: &mut DevSetupClient<tonic::transport::Channel>,
    ) -> anyhow::Result<Self> {
        let (user, device) = (enrolled.user, enrolled.device);
        setup
            .register_user(RegisterUserRequest {
                user_id: id_bytes(user),
            })
            .await
            .with_context(|| format!("register {name}"))?;
        setup
            .register_device(RegisterDeviceRequest {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
            })
            .await
            .with_context(|| format!("register {name}'s device"))?;
        Ok(Self { name, user, device })
    }

    /// Connects and identifies; returns once `Ready` arrived.
    async fn connect(&self, target: &Target) -> anyhow::Result<Socket> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut socket = loop {
            let mut request = target.socket_url.as_str().into_client_request()?;
            if let Some(host) = &target.host {
                request
                    .headers_mut()
                    .insert("Host", HeaderValue::from_str(host)?);
            }
            match tokio::time::timeout(STEP_TIMEOUT, connect_async(request)).await {
                Ok(Ok((socket, _))) => break socket,
                Ok(Err(error)) if Instant::now() < deadline => {
                    step(&format!(
                        "{} cannot connect yet ({error}); retrying",
                        self.name
                    ));
                }
                Ok(Err(error)) => {
                    return Err(error)
                        .context(format!("connect {} to {}", self.name, target.socket_url));
                }
                Err(_) => bail!("connecting {} timed out", self.name),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        send(
            &mut socket,
            Body::Identify(Identify {
                token: String::new(),
                dev_user_id: id_bytes(self.user),
                dev_device_id: id_bytes(self.device),
            }),
        )
        .await?;
        match next(&mut socket, "Ready").await? {
            Body::Ready(ready) => {
                step(&format!(
                    "{} connected (Ready, heartbeat {} ms)",
                    self.name, ready.heartbeat_ms
                ));
                Ok(socket)
            }
            other => bail!("{} expected Ready, got {other:?}", self.name),
        }
    }
}

async fn send(socket: &mut Socket, body: Body) -> anyhow::Result<()> {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .context("send a frame")
}

/// The next frame, within the step timeout.
async fn next(socket: &mut Socket, waiting_for: &str) -> anyhow::Result<Body> {
    loop {
        let message = tokio::time::timeout(STEP_TIMEOUT, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("no {waiting_for} within {STEP_TIMEOUT:?}"))?
            .with_context(|| format!("the connection ended while waiting for {waiting_for}"))?
            .with_context(|| format!("the connection failed while waiting for {waiting_for}"))?;
        match message {
            Message::Binary(bytes) => return decode_frame(&bytes).context("decode a frame"),
            Message::Close(frame) => bail!("closed while waiting for {waiting_for}: {frame:?}"),
            _ => {}
        }
    }
}

fn step(message: &str) {
    println!("smoke: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn flags_name_the_target() {
        let target = Target::parse(
            args(&[
                "--auth",
                "http://ingress",
                "--mailbox",
                "http://auth:8080",
                "--setup",
                "http://svc:50051",
                "--socket",
                "ws://ingress/v1/socket",
                "--host",
                "push.local",
            ]),
            |_| None,
        );
        assert_eq!(
            target,
            Ok(Target {
                auth_url: "http://ingress".to_owned(),
                mailbox_url: "http://auth:8080".to_owned(),
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://ingress/v1/socket".to_owned(),
                host: Some("push.local".to_owned()),
            })
        );
    }

    #[test]
    fn missing_flags_come_from_the_environment() {
        let target = Target::parse(
            args(&["--socket", "ws://gw/v1/socket"]),
            |name| match name {
                "PUSH_SMOKE_AUTH_URL" => Some("http://auth:8080".to_owned()),
                "PUSH_SMOKE_MAILBOX_URL" => Some("http://auth:8080".to_owned()),
                "PUSH_SMOKE_SETUP_URL" => Some("http://svc:50051".to_owned()),
                _ => None,
            },
        );
        assert_eq!(
            target,
            Ok(Target {
                auth_url: "http://auth:8080".to_owned(),
                mailbox_url: "http://auth:8080".to_owned(),
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://gw/v1/socket".to_owned(),
                host: None,
            })
        );
    }

    #[test]
    fn a_missing_url_or_unknown_flag_is_refused() {
        assert!(Target::parse(args(&["--setup", "http://svc:50051"]), |_| None).is_err());
        let without_auth = args(&[
            "--setup",
            "http://svc:50051",
            "--socket",
            "ws://gw/v1/socket",
        ]);
        assert!(Target::parse(without_auth, |_| None).is_err());
        assert!(Target::parse(args(&["--verbose"]), |_| None).is_err());
        assert!(Target::parse(args(&["--setup"]), |_| None).is_err());
    }
}
```

- [ ] **Step 5: Run the smoke test locally**

```powershell
cargo test -p push-smoke -- --nocapture
```

Expected: the 3 unit tests pass, and the local test prints (ids and times vary):

```text
smoke: alice signed up, verified the email from the dev mailbox, signed in and registered device 01a13476-6462-7002-ab3c-e507882e953d (a retry returned the same device) in 72 ms
smoke: alice's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: bob signed up, verified the email from the dev mailbox, signed in and registered device 01a13476-64a7-7321-bda2-c9c681f5ab31 (a retry returned the same device) in 92 ms
smoke: bob's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: mirrored alice=01a13476-643b-74b6-8a39-9647df23890c bob=01a13476-6485-7024-adaa-82d985da4db2 into message-svc as contacts with a direct conversation
smoke: bob connected (Ready, heartbeat 30000 ms)
smoke: alice connected (Ready, heartbeat 30000 ms)
smoke: alice got SendAck seq=1 after 17 ms
smoke: bob got Deliver seq=1 after 24 ms
smoke: bob acked; the gateway answered the following Ping
smoke: PASS in 330 ms
test the_smoke_test_passes_against_the_whole_stack ... ok
```

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.toml Cargo.lock crates/push-smoke
git commit -m "feat(smoke): enroll the users through auth-svc before they chat"
```

Expected: 700 tests pass (the smoke test is one of them, as before).

---

### Task 2: auth-svc in the image and the chart

**Files:**
- Modify: `deploy/docker/Dockerfile`, `deploy/helm/push-messenger/values.yaml`,
  `values-minikube.yaml`, `templates/_helpers.tpl`, `templates/secret.yaml`,
  `templates/postgres.yaml`, `templates/ingress.yaml`, `templates/tests/smoke.yaml`,
  `templates/NOTES.txt`, `deploy/helm/test-chart.ps1`
- Create: `deploy/helm/push-messenger/templates/postgres-init.yaml`, `templates/auth.yaml`

- [ ] **Step 1: The image**

Replace `deploy/docker/Dockerfile` with:

```dockerfile
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
```

```powershell
docker build -f deploy/docker/Dockerfile --target auth -t push-messenger/auth:dev .
docker run --rm push-messenger/auth:dev; "exit=$LASTEXITCODE"
docker images --format '{{.Repository}}:{{.Tag}} {{.Size}}' | Select-String push-messenger/auth
docker rmi push-messenger/auth:dev
```

Expected: the build compiles the workspace in release mode (minutes the first time,
seconds when the cache mounts hold a previous build); the container prints
`Error: PUSH_DATABASE_URL must be set` and exits 1; the image is about 34 MB.

- [ ] **Step 2: The failing chart checks**

`test-chart.ps1` gains a `Doc` helper (one rendered document by kind and name) and eight
checks, one of them for auth-svc's preStop sleep and grace period; the old "no ConfigMap"
check becomes "the only ConfigMap is the init script, without credentials" (it compares
against the generated password). Replace `deploy/helm/test-chart.ps1` with:

```powershell
# Checks the Helm chart without a cluster: lint with each values file, the DEV ONLY
# switches, and the guards that keep dev mode out of real deployments.
#
#   pwsh deploy/helm/test-chart.ps1
$ErrorActionPreference = 'Stop'
$chart = Join-Path $PSScriptRoot 'push-messenger'
$minikube = Join-Path $chart 'values-minikube.yaml'
$failures = 0

function Check([string] $name, [scriptblock] $test) {
    if (& $test) {
        Write-Output "ok   $name"
    } else {
        Write-Output "FAIL $name"
        $script:failures++
    }
}

# The rendered document of this kind and name.
function Doc([string] $rendered, [string] $kind, [string] $name) {
    $rendered -split "(?m)^---\s*$" | Where-Object {
        $_ -match "(?m)^kind: $kind\s*$" -and $_ -match "(?m)^  name: $name\s*$"
    } | Select-Object -First 1
}

# Exit code of `helm template` with these arguments, output discarded.
function Render([string[]] $arguments) {
    helm template push $chart @arguments 2>&1 | Out-Null
    return $LASTEXITCODE
}

Check 'lint with the default values' { helm lint $chart --strict | Out-Null; $LASTEXITCODE -eq 0 }
Check 'lint with values-minikube.yaml' { helm lint $chart --strict -f $minikube | Out-Null; $LASTEXITCODE -eq 0 }

$default = helm template push $chart | Out-String
Check 'default: the gateway refuses dev identities' { $default -match 'name: PUSH_DEV_AUTH\s+value: "0"' }
Check 'default: message-svc has no DevSetup API' { $default -match 'name: PUSH_DEV_SETUP\s+value: "0"' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
Check 'default: auth-svc has no dev mailbox' { $default -match 'name: PUSH_DEV_MAILBOX\s+value: "0"' }
Check 'default: links use https and tokens name the gateway at wss' {
    $default -match 'name: PUSH_AUTH_PUBLIC_URL\s+value: "https://push.example.com"' -and
    $default -match 'name: PUSH_AUTH_TOKEN_AUDIENCE\s+value: "wss://push.example.com/v1/socket"'
}
Check 'default: the only ConfigMap is the Postgres init script, without credentials' {
    $configMaps = @($default -split "(?m)^---\s*$" | Where-Object { $_ -match '(?m)^kind: ConfigMap' })
    $authPassword = [regex]::Match($default, 'auth-db-password: "([^"]+)"').Groups[1].Value
    $configMaps.Count -eq 1 -and $configMaps[0] -match 'CREATE DATABASE push_auth' -and
        $authPassword -and -not $configMaps[0].Contains($authPassword)
}
Check 'default: auth-svc gets its own database, the gateway none' {
    $auth = Doc $default 'Deployment' 'push-auth'
    $gateway = Doc $default 'Deployment' 'push-gateway'
    $auth -match 'key: auth-database-url' -and $auth -notmatch 'key: database-url' -and
        $gateway -match 'PUSH_MESSAGE_SVC_URL' -and $gateway -notmatch 'database-url'
}
Check 'default: auth-svc drains like the gateway (preStop sleep, grace period)' {
    $auth = Doc $default 'Deployment' 'push-auth'
    $auth -match 'preStop:\s+sleep:\s+seconds: 5' -and $auth -match 'terminationGracePeriodSeconds: 30'
}
Check 'default: the ingress routes /v1/auth, /v1/devices and /v1/users to auth-svc, not /dev' {
    $ingress = Doc $default 'Ingress' 'push-auth'
    $ingress -match 'path: /v1/auth\s' -and $ingress -match 'path: /v1/devices\s' -and
        $ingress -match 'path: /v1/users\s' -and $default -notmatch 'path: /dev'
}

$dev = helm template push $chart -f $minikube | Out-String
Check 'minikube: dev identities on' { $dev -match 'name: PUSH_DEV_AUTH\s+value: "1"' }
Check 'minikube: DevSetup API on' { $dev -match 'name: PUSH_DEV_SETUP\s+value: "1"' }
Check 'minikube: dev mailbox on' { $dev -match 'name: PUSH_DEV_MAILBOX\s+value: "1"' }
Check 'minikube: links use http://push.local, tokens ws://push.local/v1/socket' {
    $dev -match 'name: PUSH_AUTH_PUBLIC_URL\s+value: "http://push.local"' -and
    $dev -match 'name: PUSH_AUTH_TOKEN_AUDIENCE\s+value: "ws://push.local/v1/socket"'
}
Check 'minikube: the smoke test is a helm test' { $dev -match 'helm.sh/hook: test' }
Check 'minikube: ingress timeouts are 120 s' { $dev -match 'proxy-read-timeout: "120"' -and $dev -match 'proxy-send-timeout: "120"' }

Check 'dev mode without the acknowledgement is refused' {
    (Render @('--set', 'devMode.enabled=true', '--set', 'ingress.host=push.local')) -ne 0
}
Check 'dev mode with a public ingress host is refused' {
    (Render @('-f', $minikube, '--set', 'ingress.host=chat.example.com')) -ne 0
}
Check 'plain http with a public ingress host is refused' {
    (Render @('--set', 'ingress.scheme=http')) -ne 0
}

if ($failures -gt 0) {
    Write-Output "$failures check(s) failed"
    exit 1
}
Write-Output 'all chart checks passed'
```

```powershell
pwsh -NoProfile -File deploy/helm/test-chart.ps1
```

Expected: the auth checks fail (`FAIL default: auth-svc has no dev mailbox` and others),
then `9 check(s) failed`.

- [ ] **Step 3: Values**

Replace `deploy/helm/push-messenger/values.yaml` with:

```yaml
# Safe defaults. Authentication cannot be switched off from here: devMode stays off
# unless a values file sets both `devMode.enabled` and `devMode.acknowledge`, and the
# chart refuses dev mode for an ingress host that is not local (see _helpers.tpl).

images:
  auth:
    repository: push-messenger/auth
    tag: dev
  messageSvc:
    repository: push-messenger/message-svc
    tag: dev
  gateway:
    repository: push-messenger/gateway
    tag: dev
  smoke:
    repository: push-messenger/smoke
    tag: dev
  pullPolicy: IfNotPresent

# Changing this rolls out new auth, gateway and message-svc pods even when the image tags
# stay the same (the bring-up script sets it after rebuilding the images).
buildId: ""

# DEV ONLY switches: the gateway accepts identities without a token (PUSH_DEV_AUTH=1),
# message-svc serves the DevSetup API (PUSH_DEV_SETUP=1), auth-svc serves the outbox's
# email links at /dev/mailbox (PUSH_DEV_MAILBOX=1), and `helm test` runs the smoke test.
# Never enable outside a local cluster.
devMode:
  enabled: false
  # Must be exactly "insecure-local-only" when enabled.
  acknowledge: ""

gateway:
  replicas: 1
  # Client ping interval announced in Ready, in milliseconds.
  heartbeatMs: 30000
  # 5 s preStop sleep + the gateway's own 10 s drain + margin.
  terminationGracePeriodSeconds: 30
  resources:
    requests:
      cpu: 50m
      memory: 64Mi
    limits:
      cpu: 500m
      memory: 256Mi

auth:
  replicas: 1
  # 5 s preStop sleep + finishing the requests in progress (a password hash waits at
  # most 2 s for its turn) + margin.
  terminationGracePeriodSeconds: 30
  dbMaxConnections: 10
  # Password hashes computed at once; each takes 19 MiB (Argon2id, spec §5.6).
  hashConcurrency: 2
  # Empty: generated on install and kept on upgrades. The Ed25519 seed that signs access
  # tokens and the HMAC key of email links and registration tokens, base64 of 32 bytes.
  signingKey: ""
  linkKey: ""
  resources:
    requests:
      cpu: 100m
      memory: 64Mi
    limits:
      cpu: "1"
      memory: 256Mi

messageSvc:
  replicas: 1
  terminationGracePeriodSeconds: 30
  dbMaxConnections: 10
  resources:
    requests:
      cpu: 50m
      memory: 64Mi
    limits:
      cpu: 500m
      memory: 256Mi

postgres:
  image: postgres:17-alpine
  # Empty: generated on install and kept on upgrades. If set, use letters and digits
  # only (it is part of a URL).
  password: ""
  # The password of auth-svc's own role, push_auth; the same rules.
  authPassword: ""
  storage: 1Gi
  # Empty: the cluster's default storage class.
  storageClassName: ""
  resources:
    requests:
      cpu: 100m
      memory: 256Mi
    limits:
      cpu: "1"
      memory: 1Gi

redis:
  image: redis:7-alpine
  # Empty: generated on install and kept on upgrades. Letters and digits only.
  password: ""
  resources:
    requests:
      cpu: 50m
      memory: 64Mi
    limits:
      cpu: 250m
      memory: 128Mi

ingress:
  enabled: true
  className: nginx
  host: push.example.com
  # The scheme clients use: email links start with `<scheme>://<host>` and access tokens
  # name the gateway at `ws[s]://<host>/v1/socket`. TLS comes with the Azure plan; plain
  # http is accepted only for a local host.
  scheme: https
  # nginx closes a proxied connection that is silent for this long; clients ping every
  # 30 s (spec §7.5).
  timeoutSeconds: 120

smoke:
  # auth-svc's public API as the smoke test reaches it: the ingress controller, with
  # `ingress.host` as the Host header.
  authUrl: http://ingress-nginx-controller.ingress-nginx.svc.cluster.local
  # The WebSocket URL the smoke test connects to: the ingress controller inside the
  # cluster, with `ingress.host` as the Host header, so the test goes through the
  # ingress like a client.
  socketUrl: ws://ingress-nginx-controller.ingress-nginx.svc.cluster.local/v1/socket
```

Replace `deploy/helm/push-messenger/values-minikube.yaml` with:

```yaml
# Local minikube only (deploy/minikube/up.ps1). Turns on the DEV ONLY identity, setup API
# and mailbox that the smoke test needs.

devMode:
  enabled: true
  acknowledge: insecure-local-only

ingress:
  host: push.local
  scheme: http
```

- [ ] **Step 4: Helpers and the Secret**

Replace `deploy/helm/push-messenger/templates/_helpers.tpl` with:

```text
{{/* Resource names, prefixed with the release name. */}}
{{- define "push.name" -}}
{{- printf "%s-%s" .root.Release.Name .component | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "push.labels" -}}
app.kubernetes.io/name: {{ .component }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/part-of: push-messenger
app.kubernetes.io/managed-by: {{ .root.Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .root.Chart.Name .root.Chart.Version }}
{{- end -}}

{{- define "push.selector" -}}
app.kubernetes.io/name: {{ .component }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
{{- end -}}

{{- define "push.secretName" -}}
{{- printf "%s-secrets" .Release.Name -}}
{{- end -}}

{{/*
The base URL clients reach the API at: email links start with it. Plain http only for a
local host, as auth-svc itself insists.
*/}}
{{- define "push.publicUrl" -}}
{{- if not (has .Values.ingress.scheme (list "https" "http")) -}}
{{- fail (printf "ingress.scheme must be https or http, not %q" .Values.ingress.scheme) -}}
{{- end -}}
{{- $host := .Values.ingress.host -}}
{{- if and (eq .Values.ingress.scheme "http") (not (or (hasSuffix ".local" $host) (hasSuffix ".localhost" $host) (hasSuffix ".test" $host))) -}}
{{- fail (printf "ingress.scheme http is only allowed with a local ingress host (*.local, *.localhost or *.test), not %q" $host) -}}
{{- end -}}
{{- printf "%s://%s" .Values.ingress.scheme .Values.ingress.host -}}
{{- end -}}

{{/* The gateway's WebSocket URL: the audience of access tokens. */}}
{{- define "push.socketUrl" -}}
{{- printf "%s://%s/v1/socket" (ternary "wss" "ws" (eq .Values.ingress.scheme "https")) .Values.ingress.host -}}
{{- end -}}

{{- define "push.image" -}}
{{- printf "%s:%s" .repository .tag -}}
{{- end -}}

{{/*
"true" when the DEV ONLY switches are on, "" otherwise. Rendering fails when
devMode.enabled is set without the acknowledgement, or with an ingress host that is not
local, so dev mode cannot reach a real deployment by accident.
*/}}
{{- define "push.devMode" -}}
{{- if .Values.devMode.enabled -}}
{{- if ne .Values.devMode.acknowledge "insecure-local-only" -}}
{{- fail "devMode.enabled switches authentication off: set devMode.acknowledge to \"insecure-local-only\", and only in a local values file" -}}
{{- end -}}
{{- $host := .Values.ingress.host -}}
{{- if not (or (hasSuffix ".local" $host) (hasSuffix ".localhost" $host) (hasSuffix ".test" $host)) -}}
{{- fail (printf "devMode is only allowed with a local ingress host (*.local, *.localhost or *.test), not %q" $host) -}}
{{- end -}}
true
{{- end -}}
{{- end -}}

{{/* "1" or "0" for the PUSH_DEV_AUTH and PUSH_DEV_SETUP variables. */}}
{{- define "push.devFlag" -}}
{{- if include "push.devMode" . -}}1{{- else -}}0{{- end -}}
{{- end -}}

{{/* Hardened container settings for our own images (distroless, uid 65532). */}}
{{- define "push.containerSecurity" -}}
securityContext:
  runAsNonRoot: true
  runAsUser: 65532
  runAsGroup: 65532
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true
  capabilities:
    drop: ["ALL"]
  seccompProfile:
    type: RuntimeDefault
{{- end -}}
```

Replace `deploy/helm/push-messenger/templates/secret.yaml` with:

```yaml
{{- /*
Passwords come from values, or are generated on install and read back from the existing
Secret on upgrades (`lookup`), so they never change and never live in git. The URLs embed
them, so the services read the URLs from this Secret too, never from a ConfigMap.
*/ -}}
{{- $existing := (lookup "v1" "Secret" .Release.Namespace (include "push.secretName" .)).data | default dict -}}
{{- $postgresPassword := .Values.postgres.password | default (get $existing "postgres-password" | b64dec) | default (randAlphaNum 32) -}}
{{- $redisPassword := .Values.redis.password | default (get $existing "redis-password" | b64dec) | default (randAlphaNum 32) -}}
{{- $authPassword := .Values.postgres.authPassword | default (get $existing "auth-db-password" | b64dec) | default (randAlphaNum 32) -}}
{{- /* The auth keys are base64 of 32 random bytes; randBytes returns exactly that. */ -}}
{{- $signingKey := .Values.auth.signingKey | default (get $existing "auth-signing-key" | b64dec) | default (randBytes 32) -}}
{{- $linkKey := .Values.auth.linkKey | default (get $existing "auth-link-key" | b64dec) | default (randBytes 32) -}}
{{- $postgresHost := include "push.name" (dict "root" . "component" "postgres") -}}
{{- $redisHost := include "push.name" (dict "root" . "component" "redis") -}}
apiVersion: v1
kind: Secret
metadata:
  name: {{ include "push.secretName" . }}
  annotations:
    # Kept when an install fails or the release is uninstalled, so the next install finds
    # it with `lookup` and reuses the password the Postgres volume was initialised with.
    # The meta.helm.sh annotations and the managed-by label let that install adopt it.
    helm.sh/resource-policy: keep
    meta.helm.sh/release-name: {{ .Release.Name | quote }}
    meta.helm.sh/release-namespace: {{ .Release.Namespace | quote }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "secrets") | nindent 4 }}
type: Opaque
stringData:
  postgres-password: {{ $postgresPassword | quote }}
  redis-password: {{ $redisPassword | quote }}
  # message-svc connects as the owner of push_messaging, so it can run its migrations.
  database-url: {{ printf "postgres://push:%s@%s:5432/push_messaging" $postgresPassword $postgresHost | quote }}
  redis-url: {{ printf "redis://:%s@%s:6379" $redisPassword $redisHost | quote }}
  # auth-svc connects as push_auth, the owner of its own database push_auth, created by
  # the Postgres init script (postgres-init.yaml). It cannot read push_messaging.
  auth-db-password: {{ $authPassword | quote }}
  auth-database-url: {{ printf "postgres://push_auth:%s@%s:5432/push_auth" $authPassword $postgresHost | quote }}
  auth-signing-key: {{ $signingKey | quote }}
  auth-link-key: {{ $linkKey | quote }}
```

- [ ] **Step 5: PostgreSQL creates auth-svc's database**

Create `deploy/helm/push-messenger/templates/postgres-init.yaml`:

```yaml
{{- /*
Runs once, when PostgreSQL initialises an empty data volume (/docker-entrypoint-initdb.d):
creates auth-svc's own role and database (one database per service, spec §2). The
password comes from the environment, which reads it from the Secret; this ConfigMap holds
no credentials. A volume initialised before this script existed (Plan 4B) has no push_auth:
remove it with deploy/minikube/down.ps1 first.
*/ -}}
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ include "push.name" (dict "root" . "component" "postgres-init") }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "postgres") | nindent 4 }}
data:
  10-push-auth.sh: |
    # Sourced by the image's entrypoint (bash) as the superuser, before the server opens.
    psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres \
      --set=password="$PUSH_AUTH_DB_PASSWORD" <<'SQL'
    CREATE ROLE push_auth LOGIN PASSWORD :'password';
    CREATE DATABASE push_auth OWNER push_auth;
    -- Revoking CONNECT from PUBLIC keeps push_auth out of push_messaging and every
    -- other role out of push_auth. Superusers bypass it: message-svc still connects as
    -- the superuser push until Plan 11.
    REVOKE ALL ON DATABASE push_auth FROM PUBLIC;
    REVOKE ALL ON DATABASE push_messaging FROM PUBLIC;
    SQL
```

Replace `deploy/helm/push-messenger/templates/postgres.yaml` with:

```yaml
{{- $name := include "push.name" (dict "root" . "component" "postgres") -}}
apiVersion: v1
kind: Service
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "postgres") | nindent 4 }}
spec:
  selector:
    {{- include "push.selector" (dict "root" . "component" "postgres") | nindent 4 }}
  ports:
    - name: postgres
      port: 5432
      targetPort: postgres
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "postgres") | nindent 4 }}
spec:
  serviceName: {{ $name }}
  replicas: 1
  selector:
    matchLabels:
      {{- include "push.selector" (dict "root" . "component" "postgres") | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "push.labels" (dict "root" . "component" "postgres") | nindent 8 }}
    spec:
      automountServiceAccountToken: false
      securityContext:
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: postgres
          image: {{ .Values.postgres.image }}
          ports:
            - name: postgres
              containerPort: 5432
          env:
            # One database per service: message-svc owns push_messaging; the init script
            # creates push_auth for auth-svc.
            - name: POSTGRES_DB
              value: push_messaging
            - name: POSTGRES_USER
              value: push
            - name: POSTGRES_PASSWORD
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: postgres-password
            - name: PGDATA
              value: /var/lib/postgresql/data/pgdata
            - name: PUSH_AUTH_DB_PASSWORD
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: auth-db-password
          readinessProbe:
            exec:
              command: ["pg_isready", "-U", "push", "-d", "push_messaging", "-h", "127.0.0.1"]
            periodSeconds: 5
          livenessProbe:
            exec:
              command: ["pg_isready", "-U", "push", "-d", "push_messaging", "-h", "127.0.0.1"]
            initialDelaySeconds: 30
            periodSeconds: 10
          resources:
            {{- toYaml .Values.postgres.resources | nindent 12 }}
          volumeMounts:
            - name: data
              mountPath: /var/lib/postgresql/data
            - name: init
              mountPath: /docker-entrypoint-initdb.d
              readOnly: true
      volumes:
        - name: init
          configMap:
            name: {{ include "push.name" (dict "root" . "component" "postgres-init") }}
  volumeClaimTemplates:
    - metadata:
        name: data
      spec:
        accessModes: ["ReadWriteOnce"]
        {{- with .Values.postgres.storageClassName }}
        storageClassName: {{ . }}
        {{- end }}
        resources:
          requests:
            storage: {{ .Values.postgres.storage }}
```

- [ ] **Step 6: auth-svc and its Ingress**

The Deployment has the gateway's shutdown budget: a native `preStop` sleep of 5 s and
`terminationGracePeriodSeconds` from the values (decision 9). Create
`deploy/helm/push-messenger/templates/auth.yaml`:

```yaml
{{- $name := include "push.name" (dict "root" . "component" "auth") -}}
apiVersion: v1
kind: Service
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "auth") | nindent 4 }}
spec:
  selector:
    {{- include "push.selector" (dict "root" . "component" "auth") | nindent 4 }}
  ports:
    - name: http
      port: 8080
      targetPort: http
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "auth") | nindent 4 }}
spec:
  replicas: {{ .Values.auth.replicas }}
  selector:
    matchLabels:
      {{- include "push.selector" (dict "root" . "component" "auth") | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "push.labels" (dict "root" . "component" "auth") | nindent 8 }}
      {{- with .Values.buildId }}
      annotations:
        push-messenger/build-id: {{ . | quote }}
      {{- end }}
    spec:
      automountServiceAccountToken: false
      # Shutdown budget, as for the gateway: the 5 s preStop sleep lets Kubernetes take
      # the pod out of the Service and Ingress endpoints before SIGTERM, on which
      # auth-svc stops accepting, finishes the requests in progress and closes its pool.
      # Native sleep action: the image has no shell.
      terminationGracePeriodSeconds: {{ .Values.auth.terminationGracePeriodSeconds }}
      containers:
        - name: auth
          image: {{ include "push.image" .Values.images.auth }}
          imagePullPolicy: {{ .Values.images.pullPolicy }}
          {{- include "push.containerSecurity" . | nindent 10 }}
          lifecycle:
            preStop:
              sleep:
                seconds: 5
          env:
            # Its own database only: auth-svc never sees message-svc's credentials.
            - name: PUSH_DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: auth-database-url
            - name: PUSH_REDIS_URL
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: redis-url
            - name: PUSH_AUTH_SIGNING_KEY
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: auth-signing-key
            - name: PUSH_AUTH_LINK_KEY
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: auth-link-key
            # Email links are built from this, never from a request's Host header.
            - name: PUSH_AUTH_PUBLIC_URL
              value: {{ include "push.publicUrl" . | quote }}
            - name: PUSH_AUTH_TOKEN_AUDIENCE
              value: {{ include "push.socketUrl" . | quote }}
            # ingress-nginx overwrites X-Real-IP with the client's address.
            - name: PUSH_AUTH_CLIENT_IP_HEADER
              value: x-real-ip
            - name: PUSH_AUTH_HASH_CONCURRENCY
              value: {{ .Values.auth.hashConcurrency | quote }}
            - name: PUSH_DB_MAX_CONNECTIONS
              value: {{ .Values.auth.dbMaxConnections | quote }}
            - name: PUSH_DEV_MAILBOX
              value: {{ include "push.devFlag" . | quote }}
          ports:
            - name: http
              containerPort: 8080
            - name: health
              containerPort: 8081
          readinessProbe:
            httpGet:
              path: /readyz
              port: health
            periodSeconds: 5
          livenessProbe:
            httpGet:
              path: /healthz
              port: health
            initialDelaySeconds: 5
            periodSeconds: 10
          resources:
            {{- toYaml .Values.auth.resources | nindent 12 }}
```

Replace `deploy/helm/push-messenger/templates/ingress.yaml` with:

```yaml
{{- if .Values.ingress.enabled -}}
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: {{ include "push.name" (dict "root" . "component" "gateway") }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "gateway") | nindent 4 }}
  annotations:
    # ingress-nginx passes the WebSocket upgrade through by itself; what it needs is
    # longer timeouts than its 60 s default, so idle sockets between client pings
    # survive (spec §7.5).
    nginx.ingress.kubernetes.io/proxy-read-timeout: {{ .Values.ingress.timeoutSeconds | quote }}
    nginx.ingress.kubernetes.io/proxy-send-timeout: {{ .Values.ingress.timeoutSeconds | quote }}
    nginx.ingress.kubernetes.io/proxy-http-version: "1.1"
spec:
  ingressClassName: {{ .Values.ingress.className }}
  rules:
    - host: {{ .Values.ingress.host }}
      http:
        paths:
          - path: /v1/socket
            pathType: Exact
            backend:
              service:
                name: {{ include "push.name" (dict "root" . "component" "gateway") }}
                port:
                  name: http
---
# auth-svc's public API (spec §2): /v1/auth, /v1/devices and /v1/users. Its DEV ONLY
# /dev/mailbox is not routed.
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: {{ include "push.name" (dict "root" . "component" "auth") }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "auth") | nindent 4 }}
  annotations:
    # Requests are small JSON bodies.
    nginx.ingress.kubernetes.io/proxy-body-size: 16k
spec:
  ingressClassName: {{ .Values.ingress.className }}
  rules:
    - host: {{ .Values.ingress.host }}
      http:
        paths:
          {{- range list "/v1/auth" "/v1/devices" "/v1/users" }}
          - path: {{ . }}
            pathType: Prefix
            backend:
              service:
                name: {{ include "push.name" (dict "root" $ "component" "auth") }}
                port:
                  name: http
          {{- end }}
{{- end }}
```

- [ ] **Step 7: The smoke Pod and the notes**

Replace `deploy/helm/push-messenger/templates/tests/smoke.yaml` with:

```yaml
{{- if include "push.devMode" . -}}
# `helm test <release>` runs the end-to-end smoke test inside the cluster: auth-svc's public
# API and the WebSockets go through the ingress controller with the ingress host as the
# Host header, as a client's would; the DEV ONLY mailbox and setup calls go straight to
# auth-svc and message-svc, which the ingress does not expose.
apiVersion: v1
kind: Pod
metadata:
  name: {{ include "push.name" (dict "root" . "component" "smoke") }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "smoke") | nindent 4 }}
  annotations:
    helm.sh/hook: test
    helm.sh/hook-delete-policy: before-hook-creation
spec:
  restartPolicy: Never
  automountServiceAccountToken: false
  containers:
    - name: smoke
      image: {{ include "push.image" .Values.images.smoke }}
      imagePullPolicy: {{ .Values.images.pullPolicy }}
      {{- include "push.containerSecurity" . | nindent 6 }}
      args:
        - --auth
        - {{ .Values.smoke.authUrl | quote }}
        - --mailbox
        - {{ printf "http://%s:8080" (include "push.name" (dict "root" . "component" "auth")) | quote }}
        - --setup
        - {{ printf "http://%s:50051" (include "push.name" (dict "root" . "component" "message-svc")) | quote }}
        - --socket
        - {{ .Values.smoke.socketUrl | quote }}
        - --host
        - {{ .Values.ingress.host | quote }}
      resources:
        requests:
          cpu: 10m
          memory: 16Mi
        limits:
          cpu: 200m
          memory: 64Mi
{{- end }}
```

Replace `deploy/helm/push-messenger/templates/NOTES.txt` with:

```text
push-messenger {{ .Chart.AppVersion }} is installed as release "{{ .Release.Name }}" in namespace "{{ .Release.Namespace }}".

WebSocket endpoint: {{ include "push.socketUrl" . }} (through the ingress controller).
Auth API: {{ include "push.publicUrl" . }}/v1/auth, /v1/devices and /v1/users; JWKS at /v1/auth/jwks.

`helm uninstall` keeps the PostgreSQL volume (data-{{ .Release.Name }}-postgres-0) and the
Secret ({{ include "push.secretName" . }}, helm.sh/resource-policy: keep), so a later install
reuses the password the database was initialised with.
To wipe everything on minikube, use deploy/minikube/down.ps1 (or down.sh): it also removes
the released volumes and the provisioner's data directory, which deleting the namespace
alone can leave behind.
{{- if include "push.devMode" . }}

DEV MODE IS ON: the gateway accepts identities without a token, message-svc serves the
DevSetup API and auth-svc shows the outbox's email links at /dev/mailbox (not routed by the
ingress). This is for a local cluster only.

Run the end-to-end smoke test:
  helm test {{ .Release.Name }} --namespace {{ .Release.Namespace }} --logs
{{- end }}
```

- [ ] **Step 8: Check the chart**

```powershell
pwsh -NoProfile -File deploy/helm/test-chart.ps1
helm template push deploy/helm/push-messenger -f deploy/helm/push-messenger/values-minikube.yaml | Select-String 'PUSH_AUTH_PUBLIC_URL|PUSH_AUTH_TOKEN_AUDIENCE|path: /v1' -Context 0,1
```

Expected: 20 `ok` lines and `all chart checks passed`; the render shows
`http://push.local`, `ws://push.local/v1/socket` and the paths `/v1/socket`, `/v1/auth`,
`/v1/devices`, `/v1/users`.

- [ ] **Step 9: Run the gates and commit**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
git add deploy/docker deploy/helm
git commit -m "deploy: auth-svc in the image and the chart, with its own database"
```

(No Rust changed; the full gates run again in Task 3.)

---

### Task 3: Bring it up on minikube and run the smoke test

**Files:**
- Modify: `deploy/minikube/up.ps1`, `deploy/minikube/up.sh`

- [ ] **Step 1: Check an existing volume, and build and roll the auth image too**

In `deploy/minikube/up.ps1`, after the block that waits for the ingress-nginx admission
jobs (it ends with the two closing braces after
`throw "the ingress-nginx admission jobs did not complete (exit code $LASTEXITCODE)"`),
add a blank line and (decision 10):

```powershell
# PostgreSQL runs the init script that creates auth-svc's database only when it
# initialises an empty volume. A volume from before auth-svc (Plan 4B) has no push_auth,
# and auth-svc would never become ready: stop now, not after five slow install attempts.
$postgres = kubectl --context $Cluster -n $namespace get statefulset push-postgres -o name --ignore-not-found
if ($LASTEXITCODE -ne 0) {
    throw "looking for an earlier Postgres failed (exit code $LASTEXITCODE)"
}
if ($postgres) {
    Invoke-Step 'check that the Postgres volume has the push_auth database' {
        kubectl --context $Cluster -n $namespace rollout status statefulset/push-postgres --timeout=300s
    }
    $hasAuth = kubectl --context $Cluster -n $namespace exec push-postgres-0 -- psql -U push -d postgres -Atc "SELECT count(*) FROM pg_database WHERE datname = 'push_auth'"
    if ($LASTEXITCODE -ne 0) {
        throw "checking the Postgres volume failed (exit code $LASTEXITCODE)"
    }
    if ("$hasAuth".Trim() -ne '1') {
        throw "The Postgres volume predates auth-svc: it has no push_auth database, and PostgreSQL creates one only on an empty volume. Run deploy/minikube/down.ps1 -Cluster $Cluster first (it deletes the cluster's data), then run this script again."
    }
}
```

replace

```powershell
        foreach ($target in 'message-svc', 'gateway', 'smoke') {
```

with

```powershell
        foreach ($target in 'auth', 'message-svc', 'gateway', 'smoke') {
```

replace

```powershell
    $ids = docker image inspect --format '{{.Id}}' push-messenger/message-svc:dev push-messenger/gateway:dev push-messenger/smoke:dev
```

with

```powershell
    $ids = docker image inspect --format '{{.Id}}' push-messenger/auth:dev push-messenger/message-svc:dev push-messenger/gateway:dev push-messenger/smoke:dev
```

and after the line
`Write-Host '  then connect to ws://127.0.0.1:8080/v1/socket with the header "Host: push.local".'`
add

```powershell
Write-Host '  auth-svc answers on the same port: http://127.0.0.1:8080/v1/auth/..., /v1/devices, /v1/users.'
```

In `deploy/minikube/up.sh`, after the `fi` that closes `if [[ -n "$jobs" ]]; then`, add a
blank line and:

```bash
# PostgreSQL runs the init script that creates auth-svc's database only when it
# initialises an empty volume. A volume from before auth-svc (Plan 4B) has no push_auth,
# and auth-svc would never become ready: stop now, not after five slow install attempts.
postgres="$(kubectl --context "$cluster" -n "$namespace" get statefulset push-postgres -o name --ignore-not-found)"
if [[ -n "$postgres" ]]; then
  step 'check that the Postgres volume has the push_auth database'
  kubectl --context "$cluster" -n "$namespace" rollout status statefulset/push-postgres --timeout=300s
  if ! has_auth="$(kubectl --context "$cluster" -n "$namespace" exec push-postgres-0 -- \
    psql -U push -d postgres -Atc "SELECT count(*) FROM pg_database WHERE datname = 'push_auth'")"; then
    echo 'checking the Postgres volume failed' >&2
    exit 1
  fi
  if [[ "$has_auth" != 1 ]]; then
    echo "The Postgres volume predates auth-svc: it has no push_auth database, and PostgreSQL creates one only on an empty volume. Run deploy/minikube/down.sh --cluster $cluster first (it deletes the cluster's data), then run this script again." >&2
    exit 1
  fi
fi
```

replace `    for target in message-svc gateway smoke; do` with

```bash
    for target in auth message-svc gateway smoke; do
```

replace the line
`  push-messenger/message-svc:dev push-messenger/gateway:dev push-messenger/smoke:dev)"; then`
with

```bash
  push-messenger/auth:dev push-messenger/message-svc:dev push-messenger/gateway:dev \
  push-messenger/smoke:dev)"; then
```

and after the line
`echo '  then connect to ws://127.0.0.1:8080/v1/socket with the header "Host: push.local".'`
add

```bash
echo '  auth-svc answers on the same port: http://127.0.0.1:8080/v1/auth/..., /v1/devices, /v1/users.'
```

```powershell
& 'C:\Program Files\Git\bin\bash.exe' -n deploy/minikube/up.sh; "syntax=$LASTEXITCODE"
git ls-files -s deploy/minikube/up.sh
```

Expected: `syntax=0`, and mode `100755` (still executable).

- [ ] **Step 2: Wipe a release from before auth-svc**

```powershell
minikube start -p push --driver=docker --cpus=4 --memory=6g
helm list -A --kube-context push
```

If the list shows the release `push` (installed by Plan 4B), remove it with its volume:

```powershell
deploy/minikube/down.ps1
```

Expected: `release "push" uninstalled`, `namespace "push" deleted`, and the released
volume deleted.

- [ ] **Step 3: Bring it up and run the smoke test**

```powershell
deploy/minikube/up.ps1 -Smoke
```

Expected: the four images build (`==> build push-messenger/auth:dev` first), the install
succeeds at the first attempt, the notes show
`Auth API: http://push.local/v1/auth, /v1/devices and /v1/users; JWKS at /v1/auth/jwks.`,
and `helm test` prints `Phase: Succeeded` and the smoke output. Recorded while writing
this plan (a fresh install; the first two lines are the ingress answering 503 for a few
seconds after the backends became ready):

```text
smoke: auth-svc answers 503 Service Unavailable yet; retrying
smoke: auth-svc answers 503 Service Unavailable yet; retrying
smoke: alice signed up, verified the email from the dev mailbox, signed in and registered device 01a13489-7059-7383-b7eb-4fbca94e4034 (a retry returned the same device) in 59 ms
smoke: alice's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: bob signed up, verified the email from the dev mailbox, signed in and registered device 01a13489-708d-7112-9529-e7af44db8e6c (a retry returned the same device) in 49 ms
smoke: bob's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: mirrored alice=01a13489-703a-7210-ad31-6e033cdefedc bob=01a13489-7073-7524-9a56-f92aa60be286 into message-svc as contacts with a direct conversation
smoke: bob connected (Ready, heartbeat 30000 ms)
smoke: alice connected (Ready, heartbeat 30000 ms)
smoke: alice got SendAck seq=1 after 13 ms
smoke: bob got Deliver seq=1 after 16 ms
smoke: bob acked; the gateway answered the following Ping
smoke: PASS in 4262 ms
```

The 59 ms of an enrollment include two Argon2id computations at the OWASP parameters
(signup and login) and two device registrations (the retry) in the release build.

- [ ] **Step 4: Check what the smoke test does not**

```powershell
kubectl --context push -n push get pods
kubectl --context push -n push logs deploy/push-auth --tail=5
kubectl --context push -n push exec push-postgres-0 -- psql -U push -d postgres -Atc "select datname, pg_get_userbyid(datdba) from pg_database where datname like 'push%'"
kubectl --context push -n push exec push-postgres-0 -- psql -U push -d push_auth -Atc "select kind, count(*) from auth.outbox group by kind order by kind"
$pw = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String((kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.auth-db-password}')))
kubectl --context push -n push exec push-postgres-0 -- env PGPASSWORD=$pw psql -h 127.0.0.1 -U push_auth -d push_messaging -Atc "select 1"
kubectl --context push -n push exec push-postgres-0 -- env PGPASSWORD=$pw psql -h 127.0.0.1 -U push_auth -d push_auth -Atc "select rolsuper from pg_roles where rolname = current_user"
```

Expected: every pod `Running` (`push-smoke` `Completed`), 0 restarts; auth-svc's JSON
logs show the DEV ONLY warning, `connected` for postgres and redis and `serving HTTP`
(the first Postgres attempt may log `not reachable yet; retrying` while the database
starts), no tokens, passwords or emails; the databases `push_auth|push_auth` and
`push_messaging|push`; the outbox holds `device.registered|2` (a retried registration
announces nothing), `email.send|2`, `user.verified|2`; `push_auth` connecting to `push_messaging` fails with
`FATAL: permission denied for database "push_messaging"` and
`User does not have CONNECT privilege.`; and `push_auth` is not a superuser (`f`).

Then check the routing from this machine:

```powershell
$forward = Start-Process kubectl -ArgumentList '--context','push','-n','ingress-nginx','port-forward','svc/ingress-nginx-controller','18080:80' -PassThru -WindowStyle Hidden
Start-Sleep 4
foreach ($path in '/dev/mailbox?to=a@b.test', '/v1/auth/jwks', '/v1/users/me') {
    $answer = Invoke-WebRequest -Uri "http://127.0.0.1:18080$path" -Headers @{ Host = 'push.local' } -SkipHttpErrorCheck
    "$path -> $($answer.StatusCode)"
}
Stop-Process -Id $forward.Id
```

Expected: `/dev/mailbox?to=a@b.test -> 404` (not routed), `/v1/auth/jwks -> 200`,
`/v1/users/me -> 401`.

- [ ] **Step 5: An upgrade keeps the keys**

```powershell
$before = kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.auth-signing-key}'
deploy/minikube/up.ps1 -SkipBuild -Smoke
$after = kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.auth-signing-key}'
"signing key kept: $($before -eq $after)"
```

Expected: `==> check that the Postgres volume has the push_auth database` (the release's
StatefulSet exists now), `REVISION: 2`, the smoke test passes again, and
`signing key kept: True`.

- [ ] **Step 6: The up scripts refuse a volume without `push_auth`**

Drop the database to stand in for a volume from Plan 4B, then run both scripts:

```powershell
kubectl --context push -n push exec push-postgres-0 -- psql -U push -d postgres -c "DROP DATABASE push_auth WITH (FORCE)"
pwsh -NoProfile -File deploy/minikube/up.ps1 -SkipBuild; "exit=$LASTEXITCODE"
& 'C:\Program Files\Git\bin\bash.exe' deploy/minikube/up.sh --skip-build; "exit=$LASTEXITCODE"
```

Expected: `DROP DATABASE`; each script prints `==> check that the Postgres volume has the
push_auth database` and stops within seconds, before building or installing anything —
`up.ps1` throws `The Postgres volume predates auth-svc: it has no push_auth database, and
PostgreSQL creates one only on an empty volume. Run deploy/minikube/down.ps1 -Cluster push
first (it deletes the cluster's data), then run this script again.` and `up.sh` prints the
same with `down.sh --cluster push`, both with exit code 1. Task 4 starts with `down.ps1`,
which leaves a fresh cluster again.

- [ ] **Step 7: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add deploy/minikube
git commit -m "deploy: build and roll the auth-svc image on minikube"
```

Expected: 700 tests pass. Leave the cluster running for Task 4.

---

### Task 4: CI, README, spec, roadmap and pull request

**Files:**
- Modify: `.github/workflows/ci.yml`, `README.md`,
  `docs/superpowers/specs/2026-10-11-push-messenger-design.md`,
  `docs/superpowers/plans/2026-10-11-roadmap.md`

- [ ] **Step 1: Build the auth image in CI**

In `.github/workflows/ci.yml`, job `images`, replace
`          for target in message-svc gateway smoke; do` with

```yaml
          for target in auth message-svc gateway smoke; do
```

- [ ] **Step 2: README**

In `README.md`, section `Run it on minikube`, replace the first paragraph after the
command (from `starts (or reuses) the minikube profile` to `verified this way before it is
merged.`) with:

```markdown
starts (or reuses) the minikube profile `push`, enables its ingress addon, builds the
images into the cluster, installs the chart (`deploy/helm/push-messenger`) with
`values-minikube.yaml`, and runs the end-to-end smoke test: two users sign up, verify their
email, register a device and chat, all through the ingress. `deploy/minikube/up.sh --smoke`
does the same on Linux and macOS. Both wait for ingress-nginx and retry
`helm upgrade --install` up to 5 times while its admission webhook comes up.
`deploy/minikube/down.ps1` removes everything again: namespace, the Secret, the
PostgreSQL volume and its data directory (`-Delete` deletes the profile).
`helm uninstall` alone keeps the Secret (`helm.sh/resource-policy: keep`) and the volume,
so a later install reuses both and the passwords keep matching. Every pull request is
verified this way before it is merged.
```

Replace the next paragraph (from `The minikube values switch on a DEV ONLY identity` to
`cluster.`) with:

```markdown
The minikube values switch on DEV ONLY shortcuts: the gateway accepts any user and device
id without a token (until it checks access tokens, Plan 8), message-svc serves a setup API,
and auth-svc shows the email links waiting in its outbox at `/dev/mailbox` (until the
worker sends email, Plan 6); neither is routed by the ingress. The chart refuses that mode
unless the values also say `devMode.acknowledge: insecure-local-only` and the ingress host
is local (`*.local`, `*.localhost` or `*.test`). `pwsh deploy/helm/test-chart.ps1` checks
the chart without a cluster.
```

and add, after it, a blank line and:

```markdown
auth-svc has a database and a role of its own, `push_auth`, which PostgreSQL's init script
creates when it initialises an empty volume. A cluster that still holds a release from before
auth-svc (Plan 4B) has no such database; `up` notices and stops: run
`deploy/minikube/down.ps1` once, then `up` again.
```

- [ ] **Step 3: Spec**

In §11 of the spec, in the Plan 4B implementation notes, insert before the bullet that
starts `` - `deploy/minikube/up.ps1` (and `up.sh`) wait for ingress-nginx ``:

```markdown
- auth-svc (Plan 5B) is a third Deployment with its own database `push_auth` and role
  `push_auth`, created by a PostgreSQL init script (a ConfigMap without credentials; the
  password comes from the Secret). `REVOKE ALL … FROM PUBLIC` on both databases keeps
  auth-svc's plain role out of `push_messaging`; message-svc still connects as the
  superuser `push`, which can reach every database, until Plan 11. The Secret also holds
  the generated Ed25519 seed and HMAC key (`randBytes 32`). A second Ingress routes
  `/v1/auth`, `/v1/devices` and `/v1/users` to it; the public URL and the token audience
  come from `ingress.scheme` and `ingress.host` (plain `http` only for a local host).
  Like the gateway, it has a 5 s preStop sleep and a 30 s termination grace period.
```

and replace that bullet (it says what the up scripts do) with:

```markdown
- `deploy/minikube/up.ps1` (and `up.sh`) wait for ingress-nginx, stop at once with
  "run down.ps1 first" when an existing PostgreSQL volume has no `push_auth` database,
  retry `helm upgrade --install` up to 5 times, and bring it up on minikube; `helm test` runs `push-smoke`
  inside the cluster through the ingress controller: two users sign up, verify through
  the DEV ONLY mailbox, register devices and chat. The Azure plan replaces the in-chart
  secrets with Key Vault and adds TLS, and may replace the in-chart PostgreSQL and Redis.
```

- [ ] **Step 4: Mark plan 5B in review**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the empty status cell at the
end of the row for plan 5B (`| 5B | Auth service: minikube | … | 5A | |`) with
`In review`.

- [ ] **Step 5: Final gates and a fresh minikube run**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 test --workspace --locked
pwsh -NoProfile -File deploy/helm/test-chart.ps1
deploy/minikube/down.ps1
deploy/minikube/up.ps1 -Smoke
```

Expected: no diff, no warnings, 700 tests on both toolchains, `all chart checks passed`,
and the smoke test `PASS`. Recorded while writing this plan (a fresh install after
`down.ps1`):

```text
smoke: auth-svc answers 503 Service Unavailable yet; retrying
smoke: auth-svc answers 503 Service Unavailable yet; retrying
smoke: alice signed up, verified the email from the dev mailbox, signed in and registered device 01a1348c-4752-71d8-be17-bb25e776a83f (a retry returned the same device) in 61 ms
smoke: alice's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: bob signed up, verified the email from the dev mailbox, signed in and registered device 01a1348c-4785-76d4-a2f2-9f58b5de5587 (a retry returned the same device) in 48 ms
smoke: bob's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: mirrored alice=01a1348c-4730-745d-8051-fb298c375ef6 bob=01a1348c-476c-77e1-b4bc-8bb2ec5c26f7 into message-svc as contacts with a direct conversation
smoke: bob connected (Ready, heartbeat 30000 ms)
smoke: alice connected (Ready, heartbeat 30000 ms)
smoke: alice got SendAck seq=1 after 11 ms
smoke: bob got Deliver seq=1 after 16 ms
smoke: bob acked; the gateway answered the following Ping
smoke: PASS in 4294 ms
```

Copy the `smoke:` lines for the pull request. Then stop the profile (keep it for later
plans) and restore the kubectl context you noted at the start:

```powershell
minikube stop -p push
kubectl config use-context <the context you noted>
```

- [ ] **Step 6: Commit and push**

```powershell
git add .github README.md docs
git commit -m "docs: record plan 5B in the spec, README, roadmap and CI"
git push -u origin feature/plan-05b-auth-minikube
```

- [ ] **Step 7: Open the pull request and wait for CI**

Put the smoke output from Step 5 into the body (replace the placeholder line):

```powershell
$body = @'
Implements docs/superpowers/plans/2026-10-13-plan-05b-auth-minikube.md: an auth image target; auth-svc in the chart with its own database and role (push_auth, created by a PostgreSQL init script; CONNECT revoked from PUBLIC, so auth-svc cannot reach push_messaging — message-svc stays a superuser until Plan 11), a preStop sleep and grace period like the gateway's, generated Ed25519 and HMAC keys in the Secret, an Ingress for /v1/auth, /v1/devices and /v1/users, public URL and token audience from ingress.scheme/host, PUSH_DEV_MAILBOX behind the dev-mode guard; the smoke test signs two users up through auth-svc, verifies them through the DEV ONLY mailbox, registers devices (a retried registration returns the same device), checks the access tokens against the JWKS, refreshes them, and chats through the gateway; up scripts and CI build the new image, and the up scripts stop at once on a PostgreSQL volume without push_auth; test-chart.ps1 has 20 checks.

Verified on minikube (deploy/minikube/up.ps1 -Smoke):

<paste the smoke: lines here>
'@
gh pr create --base main --title "Plan 5B: auth service on minikube" --body $body
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust`, `msrv`, `images`, `chart`). Do not merge;
the controller merges after the final review and its own minikube run.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §2 each service owns one database and never reads another's (auth-svc cannot connect to `push_messaging`) | 2, 3 |
| §2 public endpoints `/v1/auth/*`, `/v1/devices/*`, `/v1/users/*` (auth-svc) beside `/v1/socket` | 2, 3 |
| §5.2 verification is the gate: the smoke test verifies before it can sign in | 1, 3 |
| §5.3 explicit device registration with a registration token, idempotent by `registration_id`; refresh rotation | 1, 3 |
| §5.6 links from a configured base URL; pages with `Referrer-Policy: no-referrer` (checked through the ingress) | 1, 2, 3 |
| §7.6 DEV ONLY switches, strict and off by default; the gateway's dev identity kept until Plan 8 | 2, 3 |
| §11 secrets generated, kept in the Secret, never in git or a ConfigMap; probes; graceful shutdown (preStop sleep, grace period); hardened containers; the same chart as Azure | 2, 3 |
| Roadmap: every plan verified on minikube before merge | 3, 4 |

## Deferred to later plans

- **The worker** (Plan 6): relays auth-svc's outbox to message-svc and sends email; the
  smoke test then drops the `DevSetup` mirroring of users and devices, and the DEV ONLY
  mailbox reads the worker's log sender instead of the outbox.
- **Token sign-in on the WebSocket** (Plan 8): the smoke test identifies with its access
  tokens instead of the DEV ONLY identity; `PUSH_DEV_AUTH` and the dev fields go.
- **Contacts through the real API** (Plan 7): handle lookup and contact requests instead
  of `DevSetup.MakeContacts`.
- **Upgrading a volume from before auth-svc in place** (an idempotent bootstrap Job that
  creates missing roles and databases): only needed once a cluster holds data worth
  keeping (Azure, Plan 11); until then `down.ps1` resets it.
- **TLS** for the auth API and `https`/`wss` in the smoke test, Key Vault for the keys,
  signing-key rotation, `NetworkPolicies` (only the ingress reaches auth-svc's port 8080;
  only auth-svc reaches `push_auth`) (Plan 11).
- **The PostgreSQL superuser** for message-svc on minikube (Plan 4B's deferral) remains,
  so revoking `CONNECT` keeps only auth-svc confined; auth-svc already runs as a plain
  owner role (Plan 11).
- **Link tokens in ingress access logs**: ingress-nginx logs the query string of the link
  pages' `GET`, and with it the verify or reset token (single purpose, 30 minutes). The
  production ingress's log format comes with Plan 11.
