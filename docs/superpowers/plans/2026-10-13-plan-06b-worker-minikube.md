# Plan 6B — Worker and Email: minikube Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run Plan 6A's worker on the local minikube cluster with the same chart that will deploy to Azure, and prove the real flow end to end: two users sign up through auth-svc; the worker relays each `email.send` from auth-svc's outbox and its log sender delivers the verification email into the DEV ONLY mailbox, now the worker's; the users verify, sign in and register devices; the worker relays `user.verified` and `device.registered` to message-svc's `Accounts` API — the smoke test waits for that instead of mirroring users and devices through `DevSetup`; and the two chat through the gateway. Deliverables: the smoke test on the real flow; auth-svc's own DEV ONLY mailbox removed; a `worker` image target; the worker in the chart with a database and a role of its own (`push_worker`, created by a second PostgreSQL init script, allowed to connect to `push_auth`), auth-svc told to grant that role its access, the shared Accounts token generated into the kept Secret and given to message-svc and the worker only, the Resend credentials and webhook secret in the Secret (a webhook secret generated in dev mode), an Ingress for `/v1/webhooks/resend` only, `PUSH_DEV_MAILBOX` behind the dev-mode guard on the worker, and guards that refuse a Resend sender without its key or in dev mode, and dev mode with more than one worker; the bring-up scripts building the new image and stopping at once on a volume or kept Secret from before the worker; CI building it; `test-chart.ps1` with 28 checks.

**Architecture:** The Dockerfile's shared builder also builds `push-worker`; a fifth distroless target runs it. In the chart, the worker is a Deployment (HTTP 8080 for webhooks and the dev mailbox, health 8081) behind a ClusterIP Service. PostgreSQL's init scripts (a ConfigMap, run in file name order on an empty volume) now also create the role and database `push_worker`, revoke `CONNECT` on it from `PUBLIC` and grant the role `CONNECT` on `push_auth`; auth-svc gets `PUSH_AUTH_WORKER_ROLE=push_worker` and grants the rest at start-up (Plan 6A). The worker gets two URLs from the Secret — its own database as its owner, and `push_auth` as the same role — message-svc's gRPC address and the shared Accounts token, which message-svc gets too and the gateway does not; it never sees message-svc's or auth-svc's own credentials. The smoke Pod reaches the worker's mailbox straight at its Service (the ingress routes only `/v1/webhooks/resend` to the worker) and polls message-svc's `Identify` until each relayed device is accepted (`UNAVAILABLE` means not relayed yet; `PERMISSION_DENIED` fails at once); `DevSetup` stays for contacts and the direct conversation (until Plan 7) and the gateway's DEV ONLY identity stays until Plan 8. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §2, §5, §9, §11; roadmap "Running locally on minikube".

**Tech Stack:** Docker (BuildKit), minikube 1.34 (Kubernetes 1.31, docker driver, ingress addon), kubectl, Helm 3.16, PowerShell 7 and bash. No new Rust dependencies.

---

## Before you start

- **Plan 6A is merged.** This plan changes the smoke test and auth-svc's DEV ONLY mailbox,
  and packages and deploys `push-worker`.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-06b-worker-minikube
```

- Fix a stale PATH for the current PowerShell session (each new shell needs it again):

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
```

- **Docker Desktop must be running**, with the resources of Plan 4B (the minikube node
  gets 4 CPUs and 6 GB). The minikube profile is `push` (Kubernetes 1.31). Note the
  current kubectl context first and restore it at the end — minikube switches it to `push`:

```powershell
kubectl config current-context
```

- **A cluster that still holds a release from before the worker must be wiped once.**
  PostgreSQL runs its init scripts only when it initialises an empty volume, so a volume
  from Plan 5B has no `push_worker` database, and its kept Secret no `worker-db-password`;
  Task 3's `up` scripts notice and stop with "run down.ps1 first". Task 3 runs
  `deploy/minikube/down.ps1` before `up.ps1`. Never delete the profile, and never touch
  other Docker containers on this machine.
- Helm may print `failed to load plugins … helm-diff … unknown field "platformHooks"`
  first: an outdated plugin, harmless.
- **Edit files with the edit tool, and keep them LF** (`.gitattributes`:
  `* text=auto eol=lf`). This matters for `up.sh`, which bash refuses with CRLF on Linux.
- **Replace blocks.** "Replace … with …" pairs below are exact copies of the file before and
  after the change, with two lines of context (more where two would not make the first block
  unique): find the first block in the file and replace it with the second.
- Baseline (after Plan 6A): `cargo test --workspace --locked` passes 778 tests;
  `deploy/helm/test-chart.ps1` passes 20 checks.
- **Every task ends green:**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

  plus the task's own checks (`test-chart.ps1`, image builds, minikube).
- **Scope rule:** no changes to `push-worker`, `push-email`, `push-message-svc`,
  `push-gateway` or the library crates; in `push-auth` only the removal of its DEV ONLY
  mailbox; no TLS, Key Vault, real Resend credentials or registry pushes (Plan 11).

## Key rules the configuration must preserve

- **One database per service.** The worker gets the URL of `push_worker` as its owner and of
  `push_auth` as the plain role `push_worker`, which can read and delete `auth.outbox` and
  call two cleanup functions there and nothing else, and cannot connect to `push_messaging`
  (message-svc still connects as the superuser `push` until Plan 11).
- **No secret in git or in a ConfigMap.** The worker's database password, the Accounts
  token and, in dev mode, the webhook secret are generated on install, kept on upgrades
  (`lookup`), and reach the containers only through `secretKeyRef`; the Resend API key and a production webhook
  secret come from values (Plan 11: Key Vault). The init scripts read passwords from the
  environment.
- **Only message-svc and the worker get the Accounts token.** message-svc refuses
  `Accounts` calls without it, so the gateway, which reaches message-svc too, cannot
  register devices (Plan 6A, decision 17).
- **The worker exposes one public path.** The only Ingress to the worker routes `Exact`
  `/v1/webhooks/resend`, and only when a webhook secret exists; its DEV ONLY `/dev/mailbox`
  is never routed.
- **The DEV ONLY mailbox cannot reach a real deployment by accident.** `PUSH_DEV_MAILBOX`
  follows `devMode` (acknowledgement and a local host required), the worker refuses it with
  any sender but `log`, and the chart refuses dev mode with the Resend sender — and with more
  than one worker, since the mailbox is in memory, per replica.
- **Our containers stay hardened** (Plan 4B): non-root uid 65532, read-only root
  filesystem, no privilege escalation, all capabilities dropped, no service-account token;
  a 5 s preStop sleep and a 30 s grace period, like the gateway and auth-svc.

## Decisions made for this plan

These were settled while writing the plan, revised after its review, and verified on
minikube. Plan 6A's owner decisions apply (all final): unverified accounts deleted after
7 days, retention 30 days, the sender `Push Messenger <no-reply@push.kubegames.dk>`, the
log sender until Plan 11 brings Resend, 12 attempts, plain English text and minimal HTML.
Do not reopen any of this during execution.

1. **The smoke test runs the real flow.** It reads the verification email from the
   worker's mailbox (`--mailbox` now names the worker), polling for up to 30 s while the
   worker relays `email.send` and its log sender delivers, and takes the link from the
   email's text — so the test also proves the template. After both enrollments it polls
   message-svc's `Identify` for each device (30 s at most) instead of mirroring users and
   devices through `DevSetup`: `UNAVAILABLE` means not relayed yet (Plan 6A, decision 16),
   `PERMISSION_DENIED` fails the test at once; `MakeContacts` and `OpenDirect` stay DEV ONLY until Plan 7.
   It prints how long the worker took for each step.
2. **auth-svc's DEV ONLY mailbox goes.** Once the worker drains the outbox it would show
   almost nothing; the worker's log sender keeps the emails instead (Plan 6A, decision 12).
   `PUSH_DEV_MAILBOX` leaves auth-svc's configuration, and its tests read links from the
   outbox with SQL.
3. **The worker's database comes from a second init script**, `20-push-worker.sh` beside
   `10-push-auth.sh` in the same ConfigMap (run in name order, so `push_auth` exists):
   `CREATE ROLE push_worker LOGIN PASSWORD :'password'`, `CREATE DATABASE push_worker OWNER
   push_worker`, `REVOKE ALL ON DATABASE push_worker FROM PUBLIC`, `GRANT CONNECT ON
   DATABASE push_auth TO push_worker`. Like 5B, it runs only on an empty volume: clusters
   from 5B are wiped once with `down.ps1`.
4. **Secret keys**: `worker-db-password` (generated), `worker-database-url`,
   `worker-auth-database-url`, `accounts-token` (48 random characters, generated and kept;
   message-svc and the worker read it, the gateway does not; a Secret from Plan 5B gets
   one on the upgrade, so it needs no wipe), and, when present, `resend-api-key` and
   `resend-webhook-secret`; the Deployment reads the last two with `optional: true`, so the
   worker serves no webhook without a secret. In dev mode a webhook secret is generated
   (`whsec_` + `randBytes 32`) so the webhook path can be exercised on minikube with signed
   requests.
5. **Chart values** `worker.email.{sender, from, productName, resendApiKey, webhookSecret}`,
   `worker.unverifiedAccountDays` (7), `worker.retentionDays` (30), `worker.dbMaxConnections`
   (5) and `postgres.workerPassword`. **The default sender is `log`**: real sending through
   Resend comes with Plan 11 (Plan 6A, owner decision 3). `worker.email.from` defaults to
   `Push Messenger <no-reply@push.kubegames.dk>` and `productName` to `Push Messenger`
   (owner decision 2). The chart refuses `resend` without a key or a From address, an
   unknown sender, and dev mode with `resend`.
6. **One replica by default.** The worker scales (claims and drains skip locked rows, and
   outcomes are fenced by their claim), but the DEV ONLY mailbox is in memory, per
   replica, so the chart refuses dev mode with `worker.replicas` above 1 (the smoke test
   would read the wrong replica's mailbox).
7. **Resources:** the worker requests 50m CPU and 32 Mi, limited to 500m and 128 Mi.
8. **The up scripts check for the worker too**: the kept Secret must have
   `worker-db-password` as well as `auth-db-password`, and the running PostgreSQL both
   `push_auth` and `push_worker`; otherwise they stop at once with "run down.ps1 first".
9. **CI** builds the `worker` image too; the `chart` job runs the extended
   `test-chart.ps1`. The minikube run stays the merge gate, recorded in the pull request.

## File structure

```
crates/push-auth/src/api.rs, config.rs, server.rs, service.rs   the DEV ONLY mailbox removed
crates/push-auth/tests/api.rs                 links read from the outbox
crates/push-smoke/Cargo.toml                  dev: push-worker
crates/push-smoke/src/auth.rs                 the verification email from the worker's mailbox
crates/push-smoke/src/lib.rs                  wait for the relay; --mailbox names the worker
crates/push-smoke/tests/local.rs              the worker (and the Accounts token) in the local stack
deploy/docker/Dockerfile                      + the worker target
deploy/helm/push-messenger/
├── values.yaml                               images.worker, worker.*, postgres.workerPassword
├── values-minikube.yaml                      comment
└── templates/
    ├── _helpers.tpl                          push.hasWebhook, push.checkEmail (one dev worker)
    ├── secret.yaml                           worker password and URLs, Accounts token, Resend
    ├── message-svc.yaml                      PUSH_ACCOUNTS_TOKEN
    ├── postgres-init.yaml                    + 20-push-worker.sh (no credentials)
    ├── postgres.yaml                         passes the worker's password
    ├── auth.yaml                             PUSH_AUTH_WORKER_ROLE instead of the mailbox
    ├── worker.yaml                           NEW: Service + Deployment (preStop, grace)
    ├── ingress.yaml                          + the webhook Ingress
    ├── tests/smoke.yaml                      --mailbox names the worker
    └── NOTES.txt                             email and webhooks
deploy/helm/test-chart.ps1                    28 checks
deploy/minikube/up.ps1, up.sh                 build and roll the worker image; refuse a
                                                volume or Secret without the worker's
.github/workflows/ci.yml                      build the worker image
README.md, spec §5.6, §9, §11, roadmap        docs
```

---

### Task 1: The smoke test runs the real flow, and the mailbox moves to the worker

**Files:**
- Modify: `crates/push-auth/src/api.rs`, `src/config.rs`, `src/server.rs`,
  `src/service.rs`, `crates/push-auth/tests/api.rs`, `crates/push-smoke/Cargo.toml`,
  `crates/push-smoke/src/auth.rs`, `src/lib.rs`, `crates/push-smoke/tests/local.rs`

- [ ] **Step 1: The local stack gets the worker, failing**

The local test creates the worker's role and database beside auth-svc's, starts auth-svc
with `PUSH_AUTH_WORKER_ROLE` instead of `PUSH_DEV_MAILBOX`, gives message-svc and the worker
the same Accounts token, starts the worker (log sender, dev mailbox, polling every 250 ms),
and points `mailbox_url` at it. In
`crates/push-smoke/Cargo.toml`:

Replace

```toml
push-message-svc.workspace = true
push-store.workspace = true
sqlx.workspace = true
testcontainers-modules.workspace = true
```

with

```toml
push-message-svc.workspace = true
push-store.workspace = true
push-worker.workspace = true
sqlx.workspace = true
testcontainers-modules.workspace = true
```

In `crates/push-smoke/tests/local.rs`:

Replace

```rust
//! The walking skeleton on this machine: Postgres and Redis in containers (Docker must be
//! running), auth-svc, message-svc and the gateway in this process, and the smoke test
//! against them over real HTTP, gRPC and WebSockets.

use std::net::SocketAddr;
```

with

```rust
//! The walking skeleton on this machine: Postgres and Redis in containers (Docker must be
//! running), auth-svc, the worker, message-svc and the gateway in this process, and the
//! smoke test against them over real HTTP, gRPC and WebSockets.

use std::net::SocketAddr;
```

Replace

```rust
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket_url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
```

with

```rust
    let shutdown = CancellationToken::new();

    // auth-svc and the worker have databases of their own in the same server, as in the
    // chart; the worker's role may also drain auth-svc's outbox, which auth-svc grants.
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    for statement in [
        "CREATE DATABASE push_auth",
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement).execute(&admin).await.unwrap();
    }
    admin.close().await;
    let (server_url, _) = database_url.rsplit_once('/').unwrap();
    let auth_database_url = format!("{server_url}/push_auth");
    let worker_server = format!(
        "postgres://push_worker:worker-secret@127.0.0.1:{}",
        postgres.get_host_port_ipv4(5432).await.unwrap()
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket_url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
```

Replace

```rust
            "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=",
        ),
        ("PUSH_DEV_MAILBOX", "1"),
    ];
    let auth_config = push_auth::Config::from_lookup(|name| {
```

with

```rust
            "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=",
        ),
        ("PUSH_AUTH_WORKER_ROLE", "push_worker"),
    ];
    let auth_config = push_auth::Config::from_lookup(|name| {
```

Replace

```rust
    let message_svc_health_addr = message_svc_health.local_addr().unwrap();
    let message_svc_url = format!("http://{}", grpc.local_addr().unwrap());
    let message_svc = tokio::spawn(push_message_svc::serve(
        push_message_svc::Config {
```

with

```rust
    let message_svc_health_addr = message_svc_health.local_addr().unwrap();
    let message_svc_url = format!("http://{}", grpc.local_addr().unwrap());
    // The worker's token for message-svc's Accounts API, as the chart's Secret holds it.
    let accounts_token = "accounts-token-for-the-local-smoke-test".to_owned();
    let message_svc = tokio::spawn(push_message_svc::serve(
        push_message_svc::Config {
```

Replace

```rust
            health_addr: message_svc_health.local_addr().unwrap(),
            store: StoreOptions::default(),
            accounts_token: None,
            dev_setup: true,
        },
```

with

```rust
            health_addr: message_svc_health.local_addr().unwrap(),
            store: StoreOptions::default(),
            accounts_token: Some(accounts_token.clone()),
            dev_setup: true,
        },
```

Replace

```rust
    ));

    wait_until_ready("auth-svc", auth_health_addr, &auth).await;
    wait_until_ready("message-svc", message_svc_health_addr, &message_svc).await;
    wait_until_ready("the gateway", gateway_health_addr, &gateway).await;

    let target = Target {
        mailbox_url: auth_url.clone(),
        auth_url,
        setup_url: message_svc_url,
```

with

```rust
    ));

    let worker_http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker_health_addr = worker_health.local_addr().unwrap();
    let worker_url = format!("http://{}", worker_http.local_addr().unwrap());
    let worker = tokio::spawn(push_worker::serve(
        push_worker::Config {
            database_url: format!("{worker_server}/push_worker"),
            auth_database_url: format!("{worker_server}/push_auth"),
            message_svc_url: message_svc_url.clone(),
            accounts_token,
            sender: push_worker::Sender::Log,
            from: String::new(),
            product_name: "Push Messenger".to_owned(),
            webhook_secret: None,
            poll: Duration::from_millis(250),
            unverified_after: Duration::from_secs(7 * 24 * 3600),
            retention: Duration::from_secs(30 * 24 * 3600),
            http_addr: worker_http.local_addr().unwrap(),
            health_addr: worker_health_addr,
            store: StoreOptions::default(),
            dev_mailbox: true,
        },
        worker_http,
        worker_health,
        shutdown.clone(),
    ));

    wait_until_ready("auth-svc", auth_health_addr, &auth).await;
    wait_until_ready("the worker", worker_health_addr, &worker).await;
    wait_until_ready("message-svc", message_svc_health_addr, &message_svc).await;
    wait_until_ready("the gateway", gateway_health_addr, &gateway).await;

    let target = Target {
        mailbox_url: worker_url,
        auth_url,
        setup_url: message_svc_url,
```

Replace

```rust
    gateway.await.unwrap().expect("the gateway stops cleanly");
    auth.await.unwrap().expect("auth-svc stops cleanly");
}
```

with

```rust
    gateway.await.unwrap().expect("the gateway stops cleanly");
    auth.await.unwrap().expect("auth-svc stops cleanly");
    worker.await.unwrap().expect("the worker stops cleanly");
}
```

```powershell
cargo test -p push-smoke --test local
```

Expected: cargo records the new dev-dependency in `Cargo.lock`; the test fails — the smoke
test still reads the verification link from `mailbox_url` as auth-svc's outbox payloads
(`params.link`), which the worker's mailbox does not serve, and fails with
`no verification email for alice.… in the dev mailbox`.

- [ ] **Step 2: Read the email the worker delivered**

In `crates/push-smoke/src/auth.rs`:

Replace

```rust
//! The auth part of the smoke test: a user signs up, verifies the email through the
//! DEV ONLY mailbox, signs in, registers a device (and retries the registration, which
//! must find the same device) and refreshes its tokens, all over auth-svc's public HTTP
//! API, and checks the access token against the JWKS.
```

with

```rust
//! The auth part of the smoke test: a user signs up, verifies the email with the link the
//! worker's log sender delivered to the DEV ONLY mailbox, signs in, registers a device (and retries the registration, which
//! must find the same device) and refreshes its tokens, all over auth-svc's public HTTP
//! API, and checks the access token against the JWKS.
```

Replace

```rust
use uuid::Uuid;

use crate::{CONNECT_TIMEOUT, Target, step};

/// A signed-in device.
```

with

```rust
use uuid::Uuid;

use crate::{CONNECT_TIMEOUT, Target, WORKER_TIMEOUT, step};

/// A signed-in device.
```

Replace

```rust
}

/// auth-svc as a client sees it: the public API, through the ingress, and the DEV ONLY
/// mailbox, straight at the service.
pub(crate) struct Auth<'a> {
    http: Client,
```

with

```rust
}

/// auth-svc as a client sees it: the public API, through the ingress; and the DEV ONLY
/// mailbox, straight at the worker.
pub(crate) struct Auth<'a> {
    http: Client,
```

Replace

```rust
    /// Signs `name` up with a fresh address and handle, verifies the address with the
    /// link from the dev mailbox, signs in, registers a device, retries the registration
    /// with the same `registration_id` and refreshes its tokens.
    pub async fn enroll(&self, name: &str) -> anyhow::Result<Enrolled> {
```

with

```rust
    /// Signs `name` up with a fresh address and handle, verifies the address with the
    /// link the worker delivered to the dev mailbox, signs in, registers a device, retries the registration
    /// with the same `registration_id` and refreshes its tokens.
    pub async fn enroll(&self, name: &str) -> anyhow::Result<Enrolled> {
```

Replace

```rust
        .await?;

        let token = self.verification_token(&email).await?;
        let page = public(
            &self.http,
```

with

```rust
        .await?;

        let token = self.verification_token(name, &email).await?;
        let page = public(
            &self.http,
```

Replace

```rust
        let device_id: Uuid = field(&device, "device_id")?.parse()?;
        step(&format!(
            "{name} signed up, verified the email from the dev mailbox, signed in and registered device {device_id} (a retry returned the same device) in {} ms",
            started.elapsed().as_millis()
        ));
```

with

```rust
        let device_id: Uuid = field(&device, "device_id")?.parse()?;
        step(&format!(
            "{name} verified the email, signed in and registered device {device_id} (a retry returned the same device) in {} ms",
            started.elapsed().as_millis()
        ));
```

Replace

```rust
    }

    /// The token of the newest verification link sent to `email`, from the dev mailbox.
    async fn verification_token(&self, email: &str) -> anyhow::Result<String> {
        let url = format!(
            "{}/dev/mailbox",
            self.target.mailbox_url.trim_end_matches('/')
        );
        let mailbox: Value = self
            .http
            .get(&url)
            .query(&[("to", email)])
            .send()
            .await
            .and_then(Response::error_for_status)
            .map_err(|error| error.without_url())
            .with_context(|| format!("read the dev mailbox at {url}"))?
            .json()
            .await
            .map_err(|error| error.without_url())
            .context("read the dev mailbox")?;
        let link = mailbox["emails"]
            .as_array()
            .and_then(|emails| {
                emails
                    .iter()
                    .find(|email| email["template"] == "verify_email")
            })
            .and_then(|email| email["params"]["link"].as_str())
            .with_context(|| format!("no verification email for {email} in the dev mailbox"))?;
        let token = Url::parse(link)
            .map_err(|_| anyhow::anyhow!("the verification link is not a URL"))?
```

with

```rust
    }

    /// The token of the verification link sent to `email`: the worker relays the
    /// `email.send` event from auth-svc's outbox and its log sender drops the email into
    /// the dev mailbox, which is polled until it arrives.
    async fn verification_token(&self, name: &str, email: &str) -> anyhow::Result<String> {
        let url = format!(
            "{}/dev/mailbox",
            self.target.mailbox_url.trim_end_matches('/')
        );
        let started = Instant::now();
        let text = loop {
            let mailbox: Value = self
                .http
                .get(&url)
                .query(&[("to", email)])
                .send()
                .await
                .and_then(Response::error_for_status)
                .map_err(|error| error.without_url())
                .with_context(|| format!("read the dev mailbox at {url}"))?
                .json()
                .await
                .map_err(|error| error.without_url())
                .context("read the dev mailbox")?;
            let text = mailbox["emails"].as_array().and_then(|emails| {
                emails
                    .iter()
                    .find(|email| email["template"] == "verify_email")
                    .and_then(|email| email["text"].as_str().map(str::to_owned))
            });
            match text {
                Some(text) => break text,
                None if started.elapsed() < WORKER_TIMEOUT => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                None => bail!(
                    "no verification email for {email} in the dev mailbox within {WORKER_TIMEOUT:?}"
                ),
            }
        };
        step(&format!(
            "{name} signed up; the worker's log sender delivered the verification email after {} ms",
            started.elapsed().as_millis()
        ));
        let link = text
            .lines()
            .find(|line| line.starts_with("http"))
            .context("the verification email has no link")?;
        let token = Url::parse(link)
            .map_err(|_| anyhow::anyhow!("the verification link is not a URL"))?
```

- [ ] **Step 3: Wait for the relay instead of mirroring**

In `crates/push-smoke/src/lib.rs` (the doc comment, the imports, `WORKER_TIMEOUT`, the
`Target` doc, `run`, `Client::relayed` in place of `Client::mirror` — it waits while
`Identify` answers `UNAVAILABLE` and fails at once on `PERMISSION_DENIED` — and the tests'
example URLs):

Replace

```rust
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
```

with

```rust
//! Against a deployment it:
//!
//! 1. enrolls Alice and Bob through auth-svc's public API: each signs up, waits for the
//!    verification email that the worker's log sender delivers to the DEV ONLY mailbox,
//!    verifies with its link, signs in, registers a device, checks the access token
//!    against the JWKS and refreshes it;
//! 2. waits until the worker has relayed both users and devices from auth-svc's outbox to
//!    message-svc (message-svc's `Identify` accepts each device), then makes them accepted
//!    contacts with a direct conversation through message-svc's DEV ONLY `DevSetup` API
//!    (until contact requests, Plan 7);
//! 3. connects Bob's WebSocket and then Alice's, each with a DEV ONLY `Identify` naming
//!    the device auth-svc registered, and waits for `Ready`;
```

Replace

```rust
//!    `Error`.
//!
//! The deployment must run auth-svc with `PUSH_DEV_MAILBOX=1`, message-svc with
//! `PUSH_DEV_SETUP=1` and the gateway with `PUSH_DEV_AUTH=1`.

mod auth;
```

with

```rust
//!    `Error`.
//!
//! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`,
//! message-svc with `PUSH_DEV_SETUP=1` and the gateway with `PUSH_DEV_AUTH=1`.

mod auth;
```

Replace

```rust
use futures_util::{SinkExt, StreamExt};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::{
    MakeContactsRequest, OpenDirectRequest, RegisterDeviceRequest, RegisterUserRequest,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, ContentType, Identify, Ping, Send};
```

with

```rust
use futures_util::{SinkExt, StreamExt};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{IdentifyRequest, MakeContactsRequest, OpenDirectRequest};
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, ContentType, Identify, Ping, Send};
```

Replace

```rust
/// its backends become ready.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a closing handshake may take; the test has passed by then.
```

with

```rust
/// its backends become ready.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the worker may take to deliver an email or relay an event: it looks for work
/// every second.
pub(crate) const WORKER_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a closing handshake may take; the test has passed by then.
```

Replace

```rust
    /// Base URL of auth-svc's public API: `http://host` (the ingress controller).
    pub auth_url: String,
    /// Base URL of auth-svc itself, for the DEV ONLY mailbox, which the ingress does not
    /// route: `http://push-auth:8080`.
    pub mailbox_url: String,
    /// gRPC URL of message-svc, for the dev setup calls: `http://host:50051`.
    pub setup_url: String,
    /// WebSocket URL of the gateway: `ws://host/v1/socket`.
```

with

```rust
    /// Base URL of auth-svc's public API: `http://host` (the ingress controller).
    pub auth_url: String,
    /// Base URL of the worker, for the DEV ONLY mailbox of its log sender, which the
    /// ingress does not route: `http://push-worker:8080`.
    pub mailbox_url: String,
    /// gRPC URL of message-svc, to see the relayed devices and for the dev setup calls:
    /// `http://host:50051`.
    pub setup_url: String,
    /// WebSocket URL of the gateway: `ws://host/v1/socket`.
```

Replace

```rust
        .await
        .with_context(|| format!("connect to message-svc at {}", target.setup_url))?;
    let mut setup = DevSetupClient::new(channel);
    let alice = Client::mirror("alice", alice, &mut setup).await?;
    let bob = Client::mirror("bob", bob, &mut setup).await?;
    setup
        .make_contacts(MakeContactsRequest {
```

with

```rust
        .await
        .with_context(|| format!("connect to message-svc at {}", target.setup_url))?;
    let alice = Client::relayed("alice", alice, &channel).await?;
    let bob = Client::relayed("bob", bob, &channel).await?;
    let mut setup = DevSetupClient::new(channel);
    setup
        .make_contacts(MakeContactsRequest {
```

Replace

```rust
        .conversation_id;
    step(&format!(
        "mirrored alice={} bob={} into message-svc as contacts with a direct conversation",
        alice.user, bob.user
    ));
```

with

```rust
        .conversation_id;
    step(&format!(
        "made alice={} and bob={} contacts with a direct conversation (DEV ONLY setup)",
        alice.user, bob.user
    ));
```

Replace

```rust
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
```

with

```rust
impl Client {
    /// Waits until the worker has relayed the user and device auth-svc enrolled to
    /// message-svc, which then accepts the device in `Identify`.
    async fn relayed(
        name: &'static str,
        enrolled: auth::Enrolled,
        channel: &tonic::transport::Channel,
    ) -> anyhow::Result<Self> {
        let (user, device) = (enrolled.user, enrolled.device);
        let mut messaging = MessagingClient::new(channel.clone());
        let started = Instant::now();
        loop {
            let identified = messaging
                .identify(IdentifyRequest {
                    user_id: id_bytes(user),
                    device_id: id_bytes(device),
                })
                .await;
            match identified {
                Ok(_) => break,
                // Revoked, or another user's: waiting cannot help.
                Err(status) if status.code() == tonic::Code::PermissionDenied => bail!(
                    "message-svc refused {name}'s device {device} as revoked or another user's"
                ),
                // UNAVAILABLE is message-svc's answer for a device not relayed yet.
                Err(status) if started.elapsed() < WORKER_TIMEOUT => {
                    if status.code() != tonic::Code::Unavailable {
                        step(&format!(
                            "message-svc answers {:?} yet; retrying",
                            status.code()
                        ));
                    }
                }
                Err(status) => bail!(
                    "the worker did not relay {name}'s device to message-svc within {WORKER_TIMEOUT:?}: {:?}",
                    status.code()
                ),
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        step(&format!(
            "the worker relayed {name}'s account and device {device} to message-svc (waited {} ms)",
            started.elapsed().as_millis()
        ));
        Ok(Self { name, user, device })
    }
```

Replace

```rust
                "http://ingress",
                "--mailbox",
                "http://auth:8080",
                "--setup",
                "http://svc:50051",
```

with

```rust
                "http://ingress",
                "--mailbox",
                "http://worker:8080",
                "--setup",
                "http://svc:50051",
```

Replace

```rust
            Ok(Target {
                auth_url: "http://ingress".to_owned(),
                mailbox_url: "http://auth:8080".to_owned(),
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://ingress/v1/socket".to_owned(),
```

with

```rust
            Ok(Target {
                auth_url: "http://ingress".to_owned(),
                mailbox_url: "http://worker:8080".to_owned(),
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://ingress/v1/socket".to_owned(),
```

Replace

```rust
            |name| match name {
                "PUSH_SMOKE_AUTH_URL" => Some("http://auth:8080".to_owned()),
                "PUSH_SMOKE_MAILBOX_URL" => Some("http://auth:8080".to_owned()),
                "PUSH_SMOKE_SETUP_URL" => Some("http://svc:50051".to_owned()),
                _ => None,
```

with

```rust
            |name| match name {
                "PUSH_SMOKE_AUTH_URL" => Some("http://auth:8080".to_owned()),
                "PUSH_SMOKE_MAILBOX_URL" => Some("http://worker:8080".to_owned()),
                "PUSH_SMOKE_SETUP_URL" => Some("http://svc:50051".to_owned()),
                _ => None,
```

Replace

```rust
            Ok(Target {
                auth_url: "http://auth:8080".to_owned(),
                mailbox_url: "http://auth:8080".to_owned(),
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://gw/v1/socket".to_owned(),
```

with

```rust
            Ok(Target {
                auth_url: "http://auth:8080".to_owned(),
                mailbox_url: "http://worker:8080".to_owned(),
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://gw/v1/socket".to_owned(),
```

```powershell
cargo test -p push-smoke -- --nocapture
```

Expected: the unit tests pass, and the local test prints the real flow (recorded while
writing this plan; ids and times vary):

```text
smoke: alice signed up; the worker's log sender delivered the verification email after 260 ms
smoke: alice verified the email, signed in and registered device 01a136a0-0e43-75c4-97cb-542348533fb0 (a retry returned the same device) in 327 ms
smoke: alice's access token verifies against the JWKS (EdDSA, aud ws://127.0.0.1:63308/v1/socket); refresh rotated it
smoke: bob signed up; the worker's log sender delivered the verification email after 265 ms
smoke: bob verified the email, signed in and registered device 01a136a0-0f99-7579-a2a1-c1f6e18c803f (a retry returned the same device) in 334 ms
smoke: bob's access token verifies against the JWKS (EdDSA, aud ws://127.0.0.1:63308/v1/socket); refresh rotated it
smoke: the worker relayed alice's account and device 01a136a0-0e43-75c4-97cb-542348533fb0 to message-svc (waited 4 ms)
smoke: the worker relayed bob's account and device 01a136a0-0f99-7579-a2a1-c1f6e18c803f to message-svc (waited 266 ms)
smoke: made alice=01a136a0-0d1c-7689-8b29-a3268a7606c3 and bob=01a136a0-0e68-74d3-b4e3-1dc0386e3e67 contacts with a direct conversation (DEV ONLY setup)
smoke: bob connected (Ready, heartbeat 30000 ms)
smoke: alice connected (Ready, heartbeat 30000 ms)
smoke: alice got SendAck seq=1 after 21 ms
smoke: bob got Deliver seq=1 after 27 ms
smoke: bob acked; the gateway answered the following Ping
smoke: PASS in 1061 ms
```

Alice's device is relayed while Bob enrolls, hence the short wait.

- [ ] **Step 4: Remove auth-svc's mailbox**

Nothing uses it now (decision 2). In `crates/push-auth/src/api.rs`:

Replace

```rust
    /// always overwrites (or, for `X-Forwarded-For`, appends to): its last entry is used.
    pub client_ip_header: Option<HeaderName>,
    /// DEV ONLY: serve `GET /dev/mailbox`, the outbox's emails to one address.
    pub dev_mailbox: bool,
}

/// The router of the public API (and, when enabled, the DEV ONLY mailbox).
pub fn router(api: Api) -> Router {
    let mut router = Router::new()
        .route("/v1/auth/signup", post(signup))
        .route("/v1/auth/verify", get(verify_page).post(verify))
```

with

```rust
    /// always overwrites (or, for `X-Forwarded-For`, appends to): its last entry is used.
    pub client_ip_header: Option<HeaderName>,
}

/// The router of the public API.
pub fn router(api: Api) -> Router {
    Router::new()
        .route("/v1/auth/signup", post(signup))
        .route("/v1/auth/verify", get(verify_page).post(verify))
```

Replace

```rust
        .route("/v1/devices/{device_id}", delete(revoke_device))
        .route("/v1/users/me", get(own_profile).patch(set_display_name))
        .route("/v1/users/lookup", get(lookup));
    if api.dev_mailbox {
        router = router.route("/dev/mailbox", get(dev_mailbox));
    }
    router
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::map_response(no_store_by_default))
```

with

```rust
        .route("/v1/devices/{device_id}", delete(revoke_device))
        .route("/v1/users/me", get(own_profile).patch(set_display_name))
        .route("/v1/users/lookup", get(lookup))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::map_response(no_store_by_default))
```

Replace

```rust
}

#[derive(Deserialize)]
struct MailboxQuery {
    #[serde(default)]
    to: String,
}

/// DEV ONLY: the emails the outbox holds for one address, newest first.
async fn dev_mailbox(
    State(api): State<Api>,
    Query(query): Query<MailboxQuery>,
) -> Result<Response, AuthError> {
    let emails = api.auth.dev_mailbox(&query.to).await?;
    Ok(Json(json!({"emails": emails})).into_response())
}

/// A small HTML page with the headers every link page needs.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
```

with

```rust
}

/// A small HTML page with the headers every link page needs.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
```

In `crates/push-auth/src/config.rs` (the variable, its local-host check and its tests go;
the URL-injection cases the mailbox test held move to the public-URL test):

Replace

```rust
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_AUTH_WORKER_ROLE` | role the worker connects as: granted the outbox and the cleanups at start-up | none |
/// | `PUSH_DEV_MAILBOX` | `1` serves the DEV ONLY `GET /dev/mailbox` (local public URL only) | off |
///
/// `http://` public URLs and `ws://` audiences are accepted only for local hosts
/// (`localhost`, `127.0.0.1`, `*.local`, `*.localhost`, `*.test`), and so is the DEV ONLY
/// mailbox, like the chart's dev-mode guard.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
```

with

```rust
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_AUTH_WORKER_ROLE` | role the worker connects as: granted the outbox and the cleanups at start-up | none |
///
/// `http://` public URLs and `ws://` audiences are accepted only for local hosts
/// (`localhost`, `127.0.0.1`, `*.local`, `*.localhost`, `*.test`), like the chart's guard.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
```

Replace

```rust
    pub store: StoreOptions,
    pub worker_role: Option<String>,
    pub dev_mailbox: bool,
}
```

with

```rust
    pub store: StoreOptions,
    pub worker_role: Option<String>,
}
```

Replace

```rust
            .field("store", &self.store)
            .field("worker_role", &self.worker_role)
            .field("dev_mailbox", &self.dev_mailbox)
            .finish()
    }
```

with

```rust
            .field("store", &self.store)
            .field("worker_role", &self.worker_role)
            .finish()
    }
```

Replace

```rust
            }
        };
        let dev_mailbox = dev_flag(&lookup, "PUSH_DEV_MAILBOX")?;
        if dev_mailbox && !is_local_url(&public_url) {
            return Err(ConfigError::Invalid {
                name: "PUSH_DEV_MAILBOX",
                value: "1".to_owned(),
                reason: "the DEV ONLY mailbox shows every account's email links; it is only \
                         allowed with a local PUSH_AUTH_PUBLIC_URL host (localhost, 127.0.0.1, \
                         *.local, *.localhost, *.test)"
                    .to_owned(),
            });
        }
        Ok(Self {
            database_url,
```

with

```rust
            }
        };
        Ok(Self {
            database_url,
```

Replace

```rust
            store,
            worker_role,
            dev_mailbox,
        })
    }
}

/// Whether the host of `url`, a checked public URL, is local.
fn is_local_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    is_local_host(host_of(rest))
}

/// A key variable, base64-decoded; the value is never shown.
fn key(value: &str, name: &'static str) -> Result<Vec<u8>, ConfigError> {
```

with

```rust
            store,
            worker_role,
        })
    }
}

/// A key variable, base64-decoded; the value is never shown.
fn key(value: &str, name: &'static str) -> Result<Vec<u8>, ConfigError> {
```

Replace

```rust
}

/// A DEV ONLY switch: on only for exactly `1`; off when unset, empty or `0`. Anything
/// else (`true`, `yes`) is refused, so a typo can never switch it on or leave it in doubt.
fn dev_flag(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<bool, ConfigError> {
    match lookup(name).as_deref() {
        None | Some("") | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(ConfigError::Invalid {
            name,
            value: other.to_owned(),
            reason: "use 1 to enable or 0 to disable".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
```

with

```rust
}

#[cfg(test)]
mod tests {
```

Replace

```rust
        assert_eq!(config.store, StoreOptions::default());
        assert_eq!(config.worker_role, None);
        assert!(!config.dev_mailbox);
    }
```

with

```rust
        assert_eq!(config.store, StoreOptions::default());
        assert_eq!(config.worker_role, None);
    }
```

Replace

```rust
    }

    /// The required variables with a local public URL, and `name` set to `value`.
    fn local_with(name: &'static str, value: &'static str) -> Vec<(&'static str, &'static str)> {
        let mut variables = with("PUSH_AUTH_PUBLIC_URL", "https://push.test");
        variables.retain(|(key, _)| *key != name);
        variables.push((name, value));
        variables
    }

    #[test]
    fn optional_settings_can_be_set() {
```

with

```rust
    }

    #[test]
    fn optional_settings_can_be_set() {
```

Replace

```rust
            ("PUSH_DB_MAX_CONNECTIONS", "3"),
            ("PUSH_AUTH_WORKER_ROLE", "push_worker"),
            ("PUSH_DEV_MAILBOX", "1"),
        ]);
        let config = config(&variables).unwrap();
```

with

```rust
            ("PUSH_DB_MAX_CONNECTIONS", "3"),
            ("PUSH_AUTH_WORKER_ROLE", "push_worker"),
        ]);
        let config = config(&variables).unwrap();
```

Replace

```rust
        assert_eq!(config.store.max_connections, 3);
        assert_eq!(config.worker_role.as_deref(), Some("push_worker"));
        assert!(config.dev_mailbox);
    }
```

with

```rust
        assert_eq!(config.store.max_connections, 3);
        assert_eq!(config.worker_role.as_deref(), Some("push_worker"));
    }
```

Replace

```rust
            ("PUSH_AUTH_PUBLIC_URL", "https://api.example.com/?x=1"),
            ("PUSH_AUTH_PUBLIC_URL", "https://"),
            ("PUSH_AUTH_TOKEN_AUDIENCE", "ws://api.example.com/v1/socket"),
            (
```

with

```rust
            ("PUSH_AUTH_PUBLIC_URL", "https://api.example.com/?x=1"),
            ("PUSH_AUTH_PUBLIC_URL", "https://"),
            ("PUSH_AUTH_PUBLIC_URL", "https://evil.com\\.test"),
            ("PUSH_AUTH_PUBLIC_URL", "https://user@evil.com"),
            ("PUSH_AUTH_PUBLIC_URL", "http://evil.com@push.test"),
            ("PUSH_AUTH_TOKEN_AUDIENCE", "ws://api.example.com/v1/socket"),
            (
```

Replace

```rust
    }

    #[test]
    fn dev_mailbox_is_on_only_for_exactly_1() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            assert_eq!(
                config(&local_with("PUSH_DEV_MAILBOX", value))
                    .unwrap()
                    .dev_mailbox,
                expected
            );
        }
        for value in ["true", "yes", " 1"] {
            assert!(
                config(&local_with("PUSH_DEV_MAILBOX", value)).is_err(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn the_dev_mailbox_needs_a_local_public_url() {
        for url in [
            "http://localhost:8080",
            "http://127.0.0.1",
            "https://push.local",
            "http://push.localhost/",
            "https://push.test",
        ] {
            let mut variables = local_with("PUSH_DEV_MAILBOX", "1");
            variables.retain(|(key, _)| *key != "PUSH_AUTH_PUBLIC_URL");
            variables.push(("PUSH_AUTH_PUBLIC_URL", url));
            assert!(config(&variables).unwrap().dev_mailbox, "{url}");
        }
        for url in [
            "https://api.example.com",
            "https://localhost.example.com",
            "https://push.test.example.com",
        ] {
            let mut variables = with("PUSH_DEV_MAILBOX", "1");
            variables.retain(|(key, _)| *key != "PUSH_AUTH_PUBLIC_URL");
            variables.push(("PUSH_AUTH_PUBLIC_URL", url));
            assert!(
                matches!(
                    config(&variables),
                    Err(ConfigError::Invalid {
                        name: "PUSH_DEV_MAILBOX",
                        ..
                    })
                ),
                "{url}"
            );
        }
        for url in [
            "https://evil.com\\.test",
            "https://user@evil.com",
            "http://evil.com@push.test",
        ] {
            let mut variables = with("PUSH_DEV_MAILBOX", "1");
            variables.retain(|(key, _)| *key != "PUSH_AUTH_PUBLIC_URL");
            variables.push(("PUSH_AUTH_PUBLIC_URL", url));
            assert!(config(&variables).is_err(), "{url}");
        }
    }

    #[test]
    fn urls_and_keys_never_show_in_errors_or_debug_output() {
```

with

```rust
    }

    #[test]
    fn urls_and_keys_never_show_in_errors_or_debug_output() {
```

In `crates/push-auth/src/server.rs`:

Replace

```rust
    );

    if config.dev_mailbox {
        tracing::warn!(
            "DEV ONLY: PUSH_DEV_MAILBOX=1 serves GET /dev/mailbox, which shows anyone the \
             email links waiting in the outbox, and with them every account. Never enable it \
             outside a local cluster."
        );
    }
    let Some(pool) = retry("postgres", &shutdown, || async {
        // Migrations run on a dedicated connection, not on the request pool, whose
```

with

```rust
    );

    let Some(pool) = retry("postgres", &shutdown, || async {
        // Migrations run on a dedicated connection, not on the request pool, whose
```

Replace

```rust
        limits: Limits::new(Arc::new(counter)),
        client_ip_header: config.client_ip_header.clone(),
        dev_mailbox: config.dev_mailbox,
    });
    tracing::info!(addr = %config.http_addr, "serving HTTP");
```

with

```rust
        limits: Limits::new(Arc::new(counter)),
        client_ip_header: config.client_ip_header.clone(),
    });
    tracing::info!(addr = %config.http_addr, "serving HTTP");
```

In `crates/push-auth/src/service.rs`:

Replace

```rust
        Ok(())
    }

    /// DEV ONLY: the `email.send` payloads waiting in the outbox for the address `to`,
    /// newest first (at most 20). Until the worker sends email (Plan 6), this is how a
    /// local cluster and the smoke test read their verification links.
    pub async fn dev_mailbox(&self, to: &str) -> Result<Vec<serde_json::Value>, AuthError> {
        let emails = sqlx::query_scalar(
            "SELECT payload FROM auth.outbox WHERE kind = 'email.send' \
             AND lower(payload->>'to') = lower($1) \
             ORDER BY created_at DESC, event_id DESC LIMIT 20",
        )
        .bind(to.trim())
        .fetch_all(&self.pool)
        .await?;
        Ok(emails)
    }
}
```

with

```rust
        Ok(())
    }
}
```

The API tests read links from the outbox, where the worker picks them up, and the test of
the mailbox switch goes (`tests\server.rs` still checks that `/dev/mailbox` is a 404). In
`crates/push-auth/tests/api.rs`:

Replace

```rust
    _container: ContainerAsync<Postgres>,
    router: Router,
}

async fn world(dev_mailbox: bool) -> World {
    let container = Postgres::default()
        .with_tag("17-alpine")
```

with

```rust
    _container: ContainerAsync<Postgres>,
    router: Router,
    pool: PgPool,
}

async fn world() -> World {
    let container = Postgres::default()
        .with_tag("17-alpine")
```

Replace

```rust
    let pool = PgPool::connect(&url).await.expect("connect");
    let auth = Auth::new(
        pool,
        Settings {
            public_url: "http://push.local".to_owned(),
```

with

```rust
    let pool = PgPool::connect(&url).await.expect("connect");
    let auth = Auth::new(
        pool.clone(),
        Settings {
            public_url: "http://push.local".to_owned(),
```

Replace

```rust
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
        dev_mailbox,
    });
    World {
        _container: container,
        router,
    }
}

struct Answer {
    status: StatusCode,
```

with

```rust
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
    });
    World {
        _container: container,
        router,
        pool,
    }
}

/// The link of the newest `template` email to `to`, from the outbox, where the worker
/// would pick it up.
async fn last_link(world: &World, to: &str, template: &str) -> String {
    sqlx::query_scalar(
        "SELECT payload->'params'->>'link' FROM auth.outbox WHERE kind = 'email.send' \
         AND payload->>'to' = $1 AND payload->>'template' = $2 \
         ORDER BY created_at DESC, event_id DESC LIMIT 1",
    )
    .bind(to)
    .bind(template)
    .fetch_one(&world.pool)
    .await
    .unwrap()
}

struct Answer {
    status: StatusCode,
```

Replace

```rust
/// Signs up `name@example.com` and returns the token of its verification link, read from
/// the DEV ONLY mailbox.
async fn sign_up(world: &World, name: &str) -> String {
    let answer = call(
```

with

```rust
/// Signs up `name@example.com` and returns the token of its verification link, read from
/// the outbox.
async fn sign_up(world: &World, name: &str) -> String {
    let answer = call(
```

Replace

```rust
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{}", answer.text);
    assert_eq!(answer.json(), json!({"status": "check_email"}));
    let mailbox = call(
        world,
        Method::GET,
        &format!("/dev/mailbox?to={name}@example.com"),
        None,
        None,
    )
    .await
    .json();
    let link = mailbox["emails"][0]["params"]["link"].as_str().unwrap();
    assert!(
        link.starts_with("http://push.local/v1/auth/verify?token="),
```

with

```rust
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{}", answer.text);
    assert_eq!(answer.json(), json!({"status": "check_email"}));
    let link = last_link(world, &format!("{name}@example.com"), "verify_email").await;
    assert!(
        link.starts_with("http://push.local/v1/auth/verify?token="),
```

Replace

```rust
#[tokio::test]
async fn sign_up_verify_register_and_use_a_device() {
    let world = world(true).await;
    let tokens = with_device(&world, "alice").await;
    assert_eq!(tokens["token_type"], "Bearer");
```

with

```rust
#[tokio::test]
async fn sign_up_verify_register_and_use_a_device() {
    let world = world().await;
    let tokens = with_device(&world, "alice").await;
    assert_eq!(tokens["token_type"], "Bearer");
```

Replace

```rust
#[tokio::test]
async fn link_pages_change_nothing_and_leak_nothing() {
    let world = world(true).await;
    let token = sign_up(&world, "alice").await;
    let page = call(
```

with

```rust
#[tokio::test]
async fn link_pages_change_nothing_and_leak_nothing() {
    let world = world().await;
    let token = sign_up(&world, "alice").await;
    let page = call(
```

Replace

```rust
#[tokio::test]
async fn a_password_reset_works_through_its_page() {
    let world = world(true).await;
    with_device(&world, "alice").await;
    let forgot = call(
```

with

```rust
#[tokio::test]
async fn a_password_reset_works_through_its_page() {
    let world = world().await;
    with_device(&world, "alice").await;
    let forgot = call(
```

Replace

```rust
    .await;
    assert_eq!(forgot.status, StatusCode::ACCEPTED);
    let mailbox = call(
        &world,
        Method::GET,
        "/dev/mailbox?to=ALICE@example.com",
        None,
        None,
    )
    .await
    .json();
    assert_eq!(mailbox["emails"][0]["template"], "reset_password");
    let link = mailbox["emails"][0]["params"]["link"].as_str().unwrap();
    let path = link.strip_prefix("http://push.local").unwrap();
    let page = call(&world, Method::GET, path, None, None).await;
```

with

```rust
    .await;
    assert_eq!(forgot.status, StatusCode::ACCEPTED);
    let link = last_link(&world, "alice@example.com", "reset_password").await;
    let path = link.strip_prefix("http://push.local").unwrap();
    let page = call(&world, Method::GET, path, None, None).await;
```

Replace

```rust
#[tokio::test]
async fn errors_have_one_format() {
    let world = world(true).await;
    let garbage = call(
        &world,
```

with

```rust
#[tokio::test]
async fn errors_have_one_format() {
    let world = world().await;
    let garbage = call(
        &world,
```

Replace

```rust
#[tokio::test]
async fn login_is_rate_limited_per_account_and_ip_without_locking_the_account() {
    let world = world(true).await;
    with_device(&world, "alice").await;
    // with_device logged in once; 9 more wrong attempts fit in the window.
```

with

```rust
#[tokio::test]
async fn login_is_rate_limited_per_account_and_ip_without_locking_the_account() {
    let world = world().await;
    with_device(&world, "alice").await;
    // with_device logged in once; 9 more wrong attempts fit in the window.
```

Replace

```rust
    assert_eq!(elsewhere.status, StatusCode::OK, "{}", elsewhere.text);
}

#[tokio::test]
async fn the_dev_mailbox_exists_only_when_enabled() {
    let world = world(false).await;
    let answer = call(
        &world,
        Method::GET,
        "/dev/mailbox?to=a@example.com",
        None,
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
}
```

with

```rust
    assert_eq!(elsewhere.status, StatusCode::OK, "{}", elsewhere.text);
}
```

```powershell
cargo test -p push-auth -p push-smoke
```

Expected: everything passes; `push-auth` has 3 tests fewer (two configuration tests and
`the_dev_mailbox_exists_only_when_enabled`).

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock crates/push-auth crates/push-smoke
git commit -m "feat(smoke): the worker relays and delivers; the dev mailbox moves to it"
```

Expected: 775 tests pass.

---

### Task 2: The worker in the image and the chart

**Files:**
- Modify: `deploy/docker/Dockerfile`, `deploy/helm/push-messenger/values.yaml`,
  `values-minikube.yaml`, `templates/_helpers.tpl`, `templates/secret.yaml`,
  `templates/postgres-init.yaml`, `templates/postgres.yaml`, `templates/auth.yaml`,
  `templates/ingress.yaml`, `templates/tests/smoke.yaml`, `templates/NOTES.txt`,
  `deploy/helm/test-chart.ps1`
- Create: `deploy/helm/push-messenger/templates/worker.yaml`

- [ ] **Step 1: The chart checks, failing**

`test-chart.ps1` grows from 20 to 28 checks: the worker's dev mailbox and sender, both init
scripts without credentials, the worker's two databases (and auth-svc's role variable), the
Accounts token shared by message-svc and the worker only, the worker's drain budget, no
webhook Ingress without a secret, exactly one webhook path with one, the Resend settings,
the minikube mailbox and generated secret, and the refusals — dev mode with more than one
worker among them. In
`deploy/helm/test-chart.ps1`:

Replace

```powershell
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
Check 'default: the ingress routes the auth API, not /dev, and keeps link tokens out of the access log' {
```

with

```powershell
Check 'default: message-svc has no DevSetup API' { $default -match 'name: PUSH_DEV_SETUP\s+value: "0"' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
Check 'default: the worker has no dev mailbox and logs email; auth-svc has no mailbox at all' {
    $worker = Doc $default 'Deployment' 'push-worker'
    $auth = Doc $default 'Deployment' 'push-auth'
    $worker -match 'name: PUSH_DEV_MAILBOX\s+value: "0"' -and
        $worker -match 'name: PUSH_EMAIL_SENDER\s+value: "log"' -and $auth -notmatch 'PUSH_DEV_MAILBOX'
}
Check 'default: links use https and tokens name the gateway at wss' {
    $default -match 'name: PUSH_AUTH_PUBLIC_URL\s+value: "https://push.example.com"' -and
    $default -match 'name: PUSH_AUTH_TOKEN_AUDIENCE\s+value: "wss://push.example.com/v1/socket"'
}
Check 'default: the only ConfigMap is the Postgres init scripts, without credentials' {
    $configMaps = @($default -split "(?m)^---\s*$" | Where-Object { $_ -match '(?m)^kind: ConfigMap' })
    $authPassword = [regex]::Match($default, 'auth-db-password: "([^"]+)"').Groups[1].Value
    $workerPassword = [regex]::Match($default, 'worker-db-password: "([^"]+)"').Groups[1].Value
    $configMaps.Count -eq 1 -and $configMaps[0] -match 'CREATE DATABASE push_auth' -and
        $configMaps[0] -match 'CREATE DATABASE push_worker' -and
        $configMaps[0] -match 'GRANT CONNECT ON DATABASE push_auth TO push_worker' -and
        $authPassword -and -not $configMaps[0].Contains($authPassword) -and
        $workerPassword -and -not $configMaps[0].Contains($workerPassword)
}
Check 'default: auth-svc and the worker get their own databases, the gateway none' {
    $auth = Doc $default 'Deployment' 'push-auth'
    $worker = Doc $default 'Deployment' 'push-worker'
    $gateway = Doc $default 'Deployment' 'push-gateway'
    $auth -match 'key: auth-database-url' -and $auth -notmatch 'key: database-url' -and
        $auth -match 'name: PUSH_AUTH_WORKER_ROLE\s+value: push_worker' -and
        $worker -match 'key: worker-database-url' -and $worker -match 'key: worker-auth-database-url' -and
        $worker -notmatch 'key: (auth-)?database-url' -and
        $gateway -match 'PUSH_MESSAGE_SVC_URL' -and $gateway -notmatch 'database-url'
}
Check 'default: message-svc and the worker share the Accounts token from the Secret, the gateway has none' {
    $messageSvc = Doc $default 'Deployment' 'push-message-svc'
    $worker = Doc $default 'Deployment' 'push-worker'
    $gateway = Doc $default 'Deployment' 'push-gateway'
    $token = [regex]::Match($default, 'accounts-token: "([^"]+)"').Groups[1].Value
    $token.Length -eq 48 -and
        $messageSvc -match 'name: PUSH_ACCOUNTS_TOKEN\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: accounts-token' -and
        $worker -match 'name: PUSH_ACCOUNTS_TOKEN\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: accounts-token' -and
        $gateway -notmatch 'accounts-token' -and -not $messageSvc.Contains($token) -and -not $worker.Contains($token)
}
Check 'default: auth-svc and the worker drain like the gateway (preStop sleep, grace period)' {
    $auth = Doc $default 'Deployment' 'push-auth'
    $worker = Doc $default 'Deployment' 'push-worker'
    $auth -match 'preStop:\s+sleep:\s+seconds: 5' -and $auth -match 'terminationGracePeriodSeconds: 30' -and
        $worker -match 'preStop:\s+sleep:\s+seconds: 5' -and $worker -match 'terminationGracePeriodSeconds: 30'
}
Check 'default: without a webhook secret there is no webhook Ingress' {
    -not (Doc $default 'Ingress' 'push-worker') -and $default -notmatch 'resend-webhook-secret:'
}
Check 'default: the ingress routes the auth API, not /dev, and keeps link tokens out of the access log' {
```

Replace

```powershell
}

$dev = helm template push $chart -f $minikube | Out-String
Check 'minikube: dev identities on' { $dev -match 'name: PUSH_DEV_AUTH\s+value: "1"' }
Check 'minikube: DevSetup API on' { $dev -match 'name: PUSH_DEV_SETUP\s+value: "1"' }
Check 'minikube: dev mailbox on' { $dev -match 'name: PUSH_DEV_MAILBOX\s+value: "1"' }
Check 'minikube: links use http://push.local, tokens ws://push.local/v1/socket' {
    $dev -match 'name: PUSH_AUTH_PUBLIC_URL\s+value: "http://push.local"' -and
```

with

```powershell
}

$webhook = helm template push $chart --set worker.email.webhookSecret=whsec_AAAAAAAAAAAAAAAAAAAAAAAA | Out-String
Check 'with a webhook secret: the worker''s Ingress routes /v1/webhooks/resend and nothing else' {
    $ingress = Doc $webhook 'Ingress' 'push-worker'
    $ingress -and ([regex]::Matches($ingress, '- path: ')).Count -eq 1 -and
        $ingress -match 'path: /v1/webhooks/resend\s+pathType: Exact' -and
        $webhook -match 'resend-webhook-secret: "whsec_AAAAAAAAAAAAAAAAAAAAAAAA"'
}
$resend = helm template push $chart --set worker.email.sender=resend --set worker.email.resendApiKey=re_test --set 'worker.email.from=Push <no-reply@push.example.com>' | Out-String
Check 'resend: the worker sends through Resend with the key from the Secret' {
    $worker = Doc $resend 'Deployment' 'push-worker'
    $worker -match 'name: PUSH_EMAIL_SENDER\s+value: "resend"' -and
        $worker -match 'key: resend-api-key' -and $resend -match 'resend-api-key: "re_test"' -and
        $worker -notmatch 're_test'
}

$dev = helm template push $chart -f $minikube | Out-String
Check 'minikube: dev identities on' { $dev -match 'name: PUSH_DEV_AUTH\s+value: "1"' }
Check 'minikube: DevSetup API on' { $dev -match 'name: PUSH_DEV_SETUP\s+value: "1"' }
Check 'minikube: the worker''s dev mailbox on' {
    (Doc $dev 'Deployment' 'push-worker') -match 'name: PUSH_DEV_MAILBOX\s+value: "1"'
}
Check 'minikube: a generated webhook secret and the webhook Ingress' {
    $dev -match 'resend-webhook-secret: "whsec_' -and (Doc $dev 'Ingress' 'push-worker')
}
Check 'minikube: links use http://push.local, tokens ws://push.local/v1/socket' {
    $dev -match 'name: PUSH_AUTH_PUBLIC_URL\s+value: "http://push.local"' -and
```

Replace

```powershell
    (Render @('--set', 'ingress.scheme=http')) -ne 0
}

if ($failures -gt 0) {
```

with

```powershell
    (Render @('--set', 'ingress.scheme=http')) -ne 0
}
Check 'resend without an API key or a From address is refused' {
    (Render @('--set', 'worker.email.sender=resend')) -ne 0 -and
    (Render @('--set', 'worker.email.sender=resend', '--set', 'worker.email.resendApiKey=re_test',
        '--set', 'worker.email.from=')) -ne 0
}
Check 'dev mode with the resend sender, or an unknown sender, is refused' {
    (Render @('-f', $minikube, '--set', 'worker.email.sender=resend', '--set', 'worker.email.resendApiKey=re_test',
        '--set', 'worker.email.from=a@push.local')) -ne 0 -and
    (Render @('--set', 'worker.email.sender=smtp')) -ne 0
}
Check 'dev mode with more than one worker is refused' {
    (Render @('-f', $minikube, '--set', 'worker.replicas=2')) -ne 0 -and
    (Render @('--set', 'worker.replicas=2')) -eq 0
}

if ($failures -gt 0) {
```

```powershell
pwsh -NoProfile -File deploy/helm/test-chart.ps1
```

Expected: `FAIL` for the checks that need the worker, and `… check(s) failed`.

- [ ] **Step 2: The image**

In `deploy/docker/Dockerfile`:

Replace

```dockerfile
#   docker build -f deploy/docker/Dockerfile --target message-svc -t push-messenger/message-svc:dev .
#   docker build -f deploy/docker/Dockerfile --target gateway -t push-messenger/gateway:dev .
#   docker build -f deploy/docker/Dockerfile --target smoke -t push-messenger/smoke:dev .
#
```

with

```dockerfile
#   docker build -f deploy/docker/Dockerfile --target message-svc -t push-messenger/message-svc:dev .
#   docker build -f deploy/docker/Dockerfile --target gateway -t push-messenger/gateway:dev .
#   docker build -f deploy/docker/Dockerfile --target worker -t push-messenger/worker:dev .
#   docker build -f deploy/docker/Dockerfile --target smoke -t push-messenger/smoke:dev .
#
```

Replace

```dockerfile
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
        -p push-auth -p push-message-svc -p push-gateway -p push-smoke \
    && mkdir /out \
    && cp target/release/push-auth target/release/push-message-svc \
        target/release/push-gateway target/release/push-smoke /out/

# glibc and libgcc, no shell, no package manager; runs as uid 65532 ("nonroot").
```

with

```dockerfile
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
        -p push-auth -p push-message-svc -p push-gateway -p push-worker -p push-smoke \
    && mkdir /out \
    && cp target/release/push-auth target/release/push-message-svc \
        target/release/push-gateway target/release/push-worker target/release/push-smoke \
        /out/

# glibc and libgcc, no shell, no package manager; runs as uid 65532 ("nonroot").
```

Replace

```dockerfile
ENTRYPOINT ["/usr/local/bin/push-gateway"]

FROM runtime AS smoke
COPY --from=builder /out/push-smoke /usr/local/bin/push-smoke
```

with

```dockerfile
ENTRYPOINT ["/usr/local/bin/push-gateway"]

FROM runtime AS worker
COPY --from=builder /out/push-worker /usr/local/bin/push-worker
EXPOSE 8080 8081
ENTRYPOINT ["/usr/local/bin/push-worker"]

FROM runtime AS smoke
COPY --from=builder /out/push-smoke /usr/local/bin/push-smoke
```

```powershell
docker build -f deploy/docker/Dockerfile --target worker -t push-messenger/worker:dev .
```

Expected: the build succeeds (the first one compiles rustls; later ones reuse the cache).

- [ ] **Step 3: Values and helpers**

In `deploy/helm/push-messenger/values.yaml`:

Replace

```yaml
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
```

with

```yaml
    repository: push-messenger/smoke
    tag: dev
  worker:
    repository: push-messenger/worker
    tag: dev
  pullPolicy: IfNotPresent

# Changing this rolls out new auth, gateway, message-svc and worker pods even when the image tags
# stay the same (the bring-up script sets it after rebuilding the images).
buildId: ""

# DEV ONLY switches: the gateway accepts identities without a token (PUSH_DEV_AUTH=1),
# message-svc serves the DevSetup API (PUSH_DEV_SETUP=1), the worker shows the emails its
# log sender delivered at /dev/mailbox (PUSH_DEV_MAILBOX=1), a webhook secret is generated
# when none is set, and `helm test` runs the smoke test. Never enable outside a local
# cluster.
devMode:
  enabled: false
```

Replace

```yaml
      memory: 256Mi

messageSvc:
  replicas: 1
```

with

```yaml
      memory: 256Mi

worker:
  # Workers share the queue, so more replicas are safe; dev mode needs exactly 1 (its
  # DEV ONLY mailbox is in memory, per replica), and the chart refuses more there.
  replicas: 1
  # 5 s preStop sleep (the webhook endpoint is behind the ingress) + the job in progress
  # (at most about 15 s) + giving back the jobs not started + margin.
  terminationGracePeriodSeconds: 30
  # Each of its two pools (its own database and auth-svc's outbox).
  dbMaxConnections: 5
  email:
    # log: logs each email instead of sending it. resend: sends through Resend, which
    # needs resendApiKey and from. Real sending comes with the Azure plan (Plan 11), which
    # brings the key and the webhook secret through Key Vault.
    sender: log
    # The From of every email (owner decision: the push.kubegames.dk domain).
    from: "Push Messenger <no-reply@push.kubegames.dk>"
    # The name emails sign with.
    productName: Push Messenger
    # Resend's API key and the signing secret of its webhook (whsec_...): kept in the
    # Secret. Without a webhook secret the worker does not serve the webhook, and the
    # webhook Ingress is not rendered (in dev mode one is generated).
    resendApiKey: ""
    webhookSecret: ""
  # Accounts left unverified this many days are deleted.
  unverifiedAccountDays: 7
  # Buried jobs, email records and delivery events are kept this many days.
  retentionDays: 30
  resources:
    requests:
      cpu: 50m
      memory: 32Mi
    limits:
      cpu: 500m
      memory: 128Mi

messageSvc:
  replicas: 1
```

Replace

```yaml
  # The password of auth-svc's own role, push_auth; the same rules.
  authPassword: ""
  storage: 1Gi
  # Empty: the cluster's default storage class.
```

with

```yaml
  # The password of auth-svc's own role, push_auth; the same rules.
  authPassword: ""
  # The password of the worker's own role, push_worker; the same rules.
  workerPassword: ""
  storage: 1Gi
  # Empty: the cluster's default storage class.
```

In `deploy/helm/push-messenger/values-minikube.yaml`:

Replace

```yaml
# Local minikube only (deploy/minikube/up.ps1). Turns on the DEV ONLY identity, setup API
# and mailbox that the smoke test needs.

devMode:
```

with

```yaml
# Local minikube only (deploy/minikube/up.ps1). Turns on the DEV ONLY identity, setup API
# and mailbox that the smoke test needs; the worker logs emails instead of sending them.

devMode:
```

In `deploy/helm/push-messenger/templates/_helpers.tpl` (decisions 4, 5 and 6):

Replace

```yaml
{{- end -}}

{{/* "1" or "0" for the PUSH_DEV_AUTH and PUSH_DEV_SETUP variables. */}}
{{- define "push.devFlag" -}}
{{- if include "push.devMode" . -}}1{{- else -}}0{{- end -}}
{{- end -}}

{{/* Hardened container settings for our own images (distroless, uid 65532). */}}
{{- define "push.containerSecurity" -}}
```

with

```yaml
{{- end -}}

{{/* "1" or "0" for the PUSH_DEV_AUTH, PUSH_DEV_SETUP and PUSH_DEV_MAILBOX variables. */}}
{{- define "push.devFlag" -}}
{{- if include "push.devMode" . -}}1{{- else -}}0{{- end -}}
{{- end -}}

{{/*
"true" when the worker gets a webhook secret, so it serves /v1/webhooks/resend and the
webhook Ingress is rendered: set in values, kept in the existing Secret, or generated in
dev mode (secret.yaml).
*/}}
{{- define "push.hasWebhook" -}}
{{- $existing := (lookup "v1" "Secret" .Release.Namespace (include "push.secretName" .)).data | default dict -}}
{{- if or .Values.worker.email.webhookSecret (hasKey $existing "resend-webhook-secret") (include "push.devMode" .) -}}
true
{{- end -}}
{{- end -}}

{{/*
Checks the worker's email settings: the sender is log or resend; resend needs an API key
and a From address and is refused in dev mode, whose mailbox shows every email it sent.
Dev mode needs exactly one worker: its mailbox is in memory, per replica, so with more
the smoke test would look in the wrong one.
*/}}
{{- define "push.checkEmail" -}}
{{- $email := .Values.worker.email -}}
{{- if and (include "push.devMode" .) (gt (int .Values.worker.replicas) 1) -}}
{{- fail "dev mode keeps the DEV ONLY mailbox in memory, per worker replica: use worker.replicas 1 with it" -}}
{{- end -}}
{{- if not (has $email.sender (list "log" "resend")) -}}
{{- fail (printf "worker.email.sender must be log or resend, not %q" $email.sender) -}}
{{- end -}}
{{- if eq $email.sender "resend" -}}
{{- $existing := (lookup "v1" "Secret" .Release.Namespace (include "push.secretName" .)).data | default dict -}}
{{- if not (or $email.resendApiKey (hasKey $existing "resend-api-key")) -}}
{{- fail "worker.email.sender resend needs worker.email.resendApiKey" -}}
{{- end -}}
{{- if not $email.from -}}
{{- fail "worker.email.sender resend needs worker.email.from" -}}
{{- end -}}
{{- if include "push.devMode" . -}}
{{- fail "dev mode shows every email the worker sends at /dev/mailbox: use worker.email.sender log with it" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* Hardened container settings for our own images (distroless, uid 65532). */}}
{{- define "push.containerSecurity" -}}
```

- [ ] **Step 4: The database, the Secret and auth-svc**

In `deploy/helm/push-messenger/templates/postgres-init.yaml` (decision 3):

Replace

```yaml
{{- /*
Runs once, when PostgreSQL initialises an empty data volume (/docker-entrypoint-initdb.d):
creates auth-svc's own role and database (one database per service, spec §2). The
password comes from the environment, which reads it from the Secret; this ConfigMap holds
no credentials. A volume initialised before this script existed (Plan 4B) has no push_auth:
remove it with deploy/minikube/down.ps1 first.
*/ -}}
apiVersion: v1
```

with

```yaml
{{- /*
Runs once, when PostgreSQL initialises an empty data volume (/docker-entrypoint-initdb.d):
creates auth-svc's and the worker's own roles and databases (one database per service,
spec §2), in file name order. The passwords come from the environment, which reads them
from the Secret; this ConfigMap holds no credentials. A volume initialised before a script
existed (Plan 4B: no push_auth; Plan 5B: no push_worker) must be removed with
deploy/minikube/down.ps1 first.
*/ -}}
apiVersion: v1
```

Replace

```yaml
    REVOKE ALL ON DATABASE push_messaging FROM PUBLIC;
    SQL
```

with

```yaml
    REVOKE ALL ON DATABASE push_messaging FROM PUBLIC;
    SQL
  20-push-worker.sh: |
    # The worker's role and database. It may also connect to push_auth, where auth-svc
    # grants it auth.outbox and two cleanup functions at start-up, and nothing else.
    psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres \
      --set=password="$PUSH_WORKER_DB_PASSWORD" <<'SQL'
    CREATE ROLE push_worker LOGIN PASSWORD :'password';
    CREATE DATABASE push_worker OWNER push_worker;
    REVOKE ALL ON DATABASE push_worker FROM PUBLIC;
    GRANT CONNECT ON DATABASE push_auth TO push_worker;
    SQL
```

In `deploy/helm/push-messenger/templates/postgres.yaml`:

Replace

```yaml
              containerPort: 5432
          env:
            # One database per service: message-svc owns push_messaging; the init script
            # creates push_auth for auth-svc.
            - name: POSTGRES_DB
              value: push_messaging
```

with

```yaml
              containerPort: 5432
          env:
            # One database per service: message-svc owns push_messaging; the init scripts
            # create push_auth for auth-svc and push_worker for the worker.
            - name: POSTGRES_DB
              value: push_messaging
```

Replace

```yaml
                  name: {{ include "push.secretName" . }}
                  key: auth-db-password
          readinessProbe:
            exec:
```

with

```yaml
                  name: {{ include "push.secretName" . }}
                  key: auth-db-password
            - name: PUSH_WORKER_DB_PASSWORD
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: worker-db-password
          readinessProbe:
            exec:
```

In `deploy/helm/push-messenger/templates/secret.yaml` (decision 4):

Replace

```yaml
{{- $redisPassword := .Values.redis.password | default (get $existing "redis-password" | b64dec) | default (randAlphaNum 32) -}}
{{- $authPassword := .Values.postgres.authPassword | default (get $existing "auth-db-password" | b64dec) | default (randAlphaNum 32) -}}
{{- /* The auth keys are base64 of 32 random bytes; randBytes returns exactly that. */ -}}
{{- $signingKey := .Values.auth.signingKey | default (get $existing "auth-signing-key" | b64dec) | default (randBytes 32) -}}
```

with

```yaml
{{- $redisPassword := .Values.redis.password | default (get $existing "redis-password" | b64dec) | default (randAlphaNum 32) -}}
{{- $authPassword := .Values.postgres.authPassword | default (get $existing "auth-db-password" | b64dec) | default (randAlphaNum 32) -}}
{{- $workerPassword := .Values.postgres.workerPassword | default (get $existing "worker-db-password" | b64dec) | default (randAlphaNum 32) -}}
{{- $accountsToken := get $existing "accounts-token" | b64dec | default (randAlphaNum 48) -}}
{{- /* Resend's credentials come from values (Plan 11: Key Vault); a local cluster gets a
generated webhook secret, so the webhook path can be tried with signed requests. */ -}}
{{- $resendApiKey := .Values.worker.email.resendApiKey | default (get $existing "resend-api-key" | b64dec) -}}
{{- $webhookSecret := .Values.worker.email.webhookSecret | default (get $existing "resend-webhook-secret" | b64dec) -}}
{{- if and (not $webhookSecret) (include "push.devMode" .) -}}
{{- $webhookSecret = printf "whsec_%s" (randBytes 32) -}}
{{- end -}}
{{- /* The auth keys are base64 of 32 random bytes; randBytes returns exactly that. */ -}}
{{- $signingKey := .Values.auth.signingKey | default (get $existing "auth-signing-key" | b64dec) | default (randBytes 32) -}}
```

Replace

```yaml
  auth-signing-key: {{ $signingKey | quote }}
  auth-link-key: {{ $linkKey | quote }}
```

with

```yaml
  auth-signing-key: {{ $signingKey | quote }}
  auth-link-key: {{ $linkKey | quote }}
  # The worker connects to its own database as its owner, push_worker, and to auth-svc's
  # as the same role, which may only drain auth.outbox and call two cleanup functions
  # (auth-svc grants that at start-up). It never sees another service's credentials.
  worker-db-password: {{ $workerPassword | quote }}
  worker-database-url: {{ printf "postgres://push_worker:%s@%s:5432/push_worker" $workerPassword $postgresHost | quote }}
  worker-auth-database-url: {{ printf "postgres://push_worker:%s@%s:5432/push_auth" $workerPassword $postgresHost | quote }}
  # The worker's token for message-svc's Accounts API. The gateway, which can reach
  # message-svc too, does not get it, so it cannot register devices.
  accounts-token: {{ $accountsToken | quote }}
  {{- with $resendApiKey }}
  resend-api-key: {{ . | quote }}
  {{- end }}
  {{- with $webhookSecret }}
  resend-webhook-secret: {{ . | quote }}
  {{- end }}
```

In `deploy/helm/push-messenger/templates/auth.yaml`:

Replace

```yaml
            - name: PUSH_DB_MAX_CONNECTIONS
              value: {{ .Values.auth.dbMaxConnections | quote }}
            - name: PUSH_DEV_MAILBOX
              value: {{ include "push.devFlag" . | quote }}
          ports:
            - name: http
```

with

```yaml
            - name: PUSH_DB_MAX_CONNECTIONS
              value: {{ .Values.auth.dbMaxConnections | quote }}
            # The role the worker connects as: auth-svc grants it auth.outbox and the two
            # cleanup functions at start-up (the Postgres init script creates it).
            - name: PUSH_AUTH_WORKER_ROLE
              value: push_worker
          ports:
            - name: http
```

In `deploy/helm/push-messenger/templates/message-svc.yaml` (Plan 6A, decision 17):

Replace

```yaml
            - name: PUSH_DB_MAX_CONNECTIONS
              value: {{ .Values.messageSvc.dbMaxConnections | quote }}
            - name: PUSH_DEV_SETUP
              value: {{ include "push.devFlag" . | quote }}
```

with

```yaml
            - name: PUSH_DB_MAX_CONNECTIONS
              value: {{ .Values.messageSvc.dbMaxConnections | quote }}
            # Only the worker has this token: the Accounts API refuses calls without it.
            - name: PUSH_ACCOUNTS_TOKEN
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: accounts-token
            - name: PUSH_DEV_SETUP
              value: {{ include "push.devFlag" . | quote }}
```

- [ ] **Step 5: The worker, its Ingress, the smoke test and the notes**

Create `deploy/helm/push-messenger/templates/worker.yaml`:

```yaml
{{- include "push.checkEmail" . -}}
{{- $name := include "push.name" (dict "root" . "component" "worker") -}}
apiVersion: v1
kind: Service
metadata:
  name: {{ $name }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "worker") | nindent 4 }}
spec:
  selector:
    {{- include "push.selector" (dict "root" . "component" "worker") | nindent 4 }}
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
    {{- include "push.labels" (dict "root" . "component" "worker") | nindent 4 }}
spec:
  replicas: {{ .Values.worker.replicas }}
  selector:
    matchLabels:
      {{- include "push.selector" (dict "root" . "component" "worker") | nindent 6 }}
  template:
    metadata:
      labels:
        {{- include "push.labels" (dict "root" . "component" "worker") | nindent 8 }}
      {{- with .Values.buildId }}
      annotations:
        push-messenger/build-id: {{ . | quote }}
      {{- end }}
    spec:
      automountServiceAccountToken: false
      # The webhook endpoint is behind the ingress: the 5 s preStop sleep lets Kubernetes
      # take the pod out of its endpoints before SIGTERM, on which the worker finishes the
      # job in progress, gives back the jobs it claimed but did not start (another replica
      # takes them at once) and stops. Native sleep action: the image has no shell.
      terminationGracePeriodSeconds: {{ .Values.worker.terminationGracePeriodSeconds }}
      containers:
        - name: worker
          image: {{ include "push.image" .Values.images.worker }}
          imagePullPolicy: {{ .Values.images.pullPolicy }}
          {{- include "push.containerSecurity" . | nindent 10 }}
          lifecycle:
            preStop:
              sleep:
                seconds: 5
          env:
            # Its own database, and auth-svc's as a role that may only drain the outbox
            # and call two cleanup functions; never message-svc's database.
            - name: PUSH_DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: worker-database-url
            - name: PUSH_AUTH_DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: worker-auth-database-url
            - name: PUSH_MESSAGE_SVC_URL
              value: {{ printf "http://%s:50051" (include "push.name" (dict "root" . "component" "message-svc")) | quote }}
            - name: PUSH_ACCOUNTS_TOKEN
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: accounts-token
            - name: PUSH_EMAIL_SENDER
              value: {{ .Values.worker.email.sender | quote }}
            - name: PUSH_EMAIL_FROM
              value: {{ .Values.worker.email.from | quote }}
            - name: PUSH_EMAIL_PRODUCT_NAME
              value: {{ .Values.worker.email.productName | quote }}
            - name: PUSH_RESEND_API_KEY
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: resend-api-key
                  optional: true
            - name: PUSH_RESEND_WEBHOOK_SECRET
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: resend-webhook-secret
                  optional: true
            - name: PUSH_UNVERIFIED_ACCOUNT_DAYS
              value: {{ .Values.worker.unverifiedAccountDays | quote }}
            - name: PUSH_WORKER_RETENTION_DAYS
              value: {{ .Values.worker.retentionDays | quote }}
            - name: PUSH_DB_MAX_CONNECTIONS
              value: {{ .Values.worker.dbMaxConnections | quote }}
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
            {{- toYaml .Values.worker.resources | nindent 12 }}
```

In `deploy/helm/push-messenger/templates/ingress.yaml`:

Replace

```yaml
                  name: http
---
# auth-svc's public API (spec §2): /v1/auth, /v1/devices and /v1/users. Its DEV ONLY
# /dev/mailbox is not routed.
apiVersion: networking.k8s.io/v1
kind: Ingress
```

with

```yaml
                  name: http
---
# auth-svc's public API (spec §2): /v1/auth, /v1/devices and /v1/users.
apiVersion: networking.k8s.io/v1
kind: Ingress
```

Replace

```yaml
                  name: http
          {{- end }}
{{- end }}
```

with

```yaml
                  name: http
          {{- end }}
{{- if include "push.hasWebhook" . }}
---
# Resend's delivery webhooks (spec §9), the worker's only public path: one Exact path, so
# its DEV ONLY /dev/mailbox is never routed. The worker checks each request's Svix
# signature and refuses timestamps more than 5 minutes off.
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: {{ include "push.name" (dict "root" . "component" "worker") }}
  labels:
    {{- include "push.labels" (dict "root" . "component" "worker") | nindent 4 }}
  annotations:
    nginx.ingress.kubernetes.io/proxy-body-size: 256k
spec:
  ingressClassName: {{ .Values.ingress.className }}
  rules:
    - host: {{ .Values.ingress.host }}
      http:
        paths:
          - path: /v1/webhooks/resend
            pathType: Exact
            backend:
              service:
                name: {{ include "push.name" (dict "root" . "component" "worker") }}
                port:
                  name: http
{{- end }}
{{- end }}
```

In `deploy/helm/push-messenger/templates/tests/smoke.yaml`:

Replace

```yaml
# `helm test <release>` runs the end-to-end smoke test inside the cluster: auth-svc's public
# API and the WebSockets go through the ingress controller with the ingress host as the
# Host header, as a client's would; the DEV ONLY mailbox and setup calls go straight to
# auth-svc and message-svc, which the ingress does not expose.
apiVersion: v1
kind: Pod
```

with

```yaml
# `helm test <release>` runs the end-to-end smoke test inside the cluster: auth-svc's public
# API and the WebSockets go through the ingress controller with the ingress host as the
# Host header, as a client's would; the DEV ONLY mailbox and the calls to message-svc go
# straight to the worker and message-svc, which the ingress does not expose.
apiVersion: v1
kind: Pod
```

Replace

```yaml
        - {{ .Values.smoke.authUrl | quote }}
        - --mailbox
        - {{ printf "http://%s:8080" (include "push.name" (dict "root" . "component" "auth")) | quote }}
        - --setup
        - {{ printf "http://%s:50051" (include "push.name" (dict "root" . "component" "message-svc")) | quote }}
```

with

```yaml
        - {{ .Values.smoke.authUrl | quote }}
        - --mailbox
        - {{ printf "http://%s:8080" (include "push.name" (dict "root" . "component" "worker")) | quote }}
        - --setup
        - {{ printf "http://%s:50051" (include "push.name" (dict "root" . "component" "message-svc")) | quote }}
```

In `deploy/helm/push-messenger/templates/NOTES.txt`:

Replace

```text
WebSocket endpoint: {{ include "push.socketUrl" . }} (through the ingress controller).
Auth API: {{ include "push.publicUrl" . }}/v1/auth, /v1/devices and /v1/users; JWKS at /v1/auth/jwks.

`helm uninstall` keeps the PostgreSQL volume (data-{{ .Release.Name }}-postgres-0) and the
```

with

```text
WebSocket endpoint: {{ include "push.socketUrl" . }} (through the ingress controller).
Auth API: {{ include "push.publicUrl" . }}/v1/auth, /v1/devices and /v1/users; JWKS at /v1/auth/jwks.
Email: the worker's {{ .Values.worker.email.sender }} sender{{ if include "push.hasWebhook" . }}; Resend webhooks at {{ include "push.publicUrl" . }}/v1/webhooks/resend{{ end }}.

`helm uninstall` keeps the PostgreSQL volume (data-{{ .Release.Name }}-postgres-0) and the
```

Replace

```text
DEV MODE IS ON: the gateway accepts identities without a token, message-svc serves the
DevSetup API and auth-svc shows the outbox's email links at /dev/mailbox (not routed by the
ingress). This is for a local cluster only.

Run the end-to-end smoke test:
```

with

```text
DEV MODE IS ON: the gateway accepts identities without a token, message-svc serves the
DevSetup API and the worker shows the emails its log sender delivered at /dev/mailbox (not
routed by the ingress). This is for a local cluster only.

Run the end-to-end smoke test:
```

```powershell
pwsh -NoProfile -File deploy/helm/test-chart.ps1
```

Expected (the plugin warning aside):

```text
ok   lint with the default values
ok   lint with values-minikube.yaml
ok   default: the gateway refuses dev identities
ok   default: message-svc has no DevSetup API
ok   default: no smoke test
ok   default: the worker has no dev mailbox and logs email; auth-svc has no mailbox at all
ok   default: links use https and tokens name the gateway at wss
ok   default: the only ConfigMap is the Postgres init scripts, without credentials
ok   default: auth-svc and the worker get their own databases, the gateway none
ok   default: message-svc and the worker share the Accounts token from the Secret, the gateway has none
ok   default: auth-svc and the worker drain like the gateway (preStop sleep, grace period)
ok   default: without a webhook secret there is no webhook Ingress
ok   default: the ingress routes the auth API, not /dev, and keeps link tokens out of the access log
ok   with a webhook secret: the worker's Ingress routes /v1/webhooks/resend and nothing else
ok   resend: the worker sends through Resend with the key from the Secret
ok   minikube: dev identities on
ok   minikube: DevSetup API on
ok   minikube: the worker's dev mailbox on
ok   minikube: a generated webhook secret and the webhook Ingress
ok   minikube: links use http://push.local, tokens ws://push.local/v1/socket
ok   minikube: the smoke test is a helm test
ok   minikube: ingress timeouts are 120 s
ok   dev mode without the acknowledgement is refused
ok   dev mode with a public ingress host is refused
ok   plain http with a public ingress host is refused
ok   resend without an API key or a From address is refused
ok   dev mode with the resend sender, or an unknown sender, is refused
ok   dev mode with more than one worker is refused
all chart checks passed
```

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add deploy
git commit -m "deploy: the worker in the image and the chart"
```

Expected: 775 tests pass.

---

### Task 3: Bring it up on minikube and run the smoke test

**Files:**
- Modify: `deploy/minikube/up.ps1`, `deploy/minikube/up.sh`

- [ ] **Step 1: Check for the worker's database and Secret key, and build its image**

In `deploy/minikube/up.ps1` (decision 8):

Replace

```powershell
}

# PostgreSQL runs the init script that creates auth-svc's database only when it
# initialises an empty volume. A volume from before auth-svc (Plan 4B) has no push_auth,
# and auth-svc would never become ready: stop now, not after five slow install attempts.
$predatesAuth = "The Postgres volume predates auth-svc: it has no push_auth database, and PostgreSQL creates one only on an empty volume. Run deploy/minikube/down.ps1 -Cluster $Cluster first (it deletes the cluster's data), then run this script again."
# A plain `helm uninstall` removes the StatefulSet but keeps the volume and the Secret
# (helm.sh/resource-policy: keep), so the Secret is the evidence that survives it.
```

with

```powershell
}

# PostgreSQL runs the init scripts that create auth-svc's and the worker's databases only
# when it initialises an empty volume. A volume from before auth-svc (Plan 4B) has no
# push_auth, one from before the worker (Plan 5B) no push_worker, and the services would
# never become ready: stop now, not after five slow install attempts.
$predates = "The Postgres volume predates auth-svc or the worker: it lacks the push_auth or push_worker database, and PostgreSQL creates them only on an empty volume. Run deploy/minikube/down.ps1 -Cluster $Cluster first (it deletes the cluster's data), then run this script again."
# A plain `helm uninstall` removes the StatefulSet but keeps the volume and the Secret
# (helm.sh/resource-policy: keep), so the Secret is the evidence that survives it.
```

Replace

```powershell
}
if ($secret) {
    $authPassword = kubectl --context $Cluster -n $namespace get secret $secretName -o jsonpath='{.data.auth-db-password}'
    if ($LASTEXITCODE -ne 0) {
        throw "reading the kept Secret failed (exit code $LASTEXITCODE)"
    }
    if (-not $authPassword) {
        throw "The kept Secret $secretName has no auth-db-password: it predates auth-svc. $predatesAuth"
    }
}
```

with

```powershell
}
if ($secret) {
    foreach ($key in 'auth-db-password', 'worker-db-password') {
        $password = kubectl --context $Cluster -n $namespace get secret $secretName -o jsonpath="{.data.$key}"
        if ($LASTEXITCODE -ne 0) {
            throw "reading the kept Secret failed (exit code $LASTEXITCODE)"
        }
        if (-not $password) {
            throw "The kept Secret $secretName has no ${key}: it predates the service that needs it. $predates"
        }
    }
}
```

Replace

```powershell
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
        throw $predatesAuth
    }
}
```

with

```powershell
}
if ($postgres) {
    Invoke-Step 'check that the Postgres volume has the push_auth and push_worker databases' {
        kubectl --context $Cluster -n $namespace rollout status statefulset/push-postgres --timeout=300s
    }
    $databases = kubectl --context $Cluster -n $namespace exec push-postgres-0 -- psql -U push -d postgres -Atc "SELECT count(*) FROM pg_database WHERE datname IN ('push_auth', 'push_worker')"
    if ($LASTEXITCODE -ne 0) {
        throw "checking the Postgres volume failed (exit code $LASTEXITCODE)"
    }
    if ("$databases".Trim() -ne '2') {
        throw $predates
    }
}
```

Replace

```powershell
    $dockerEnv | Out-String | Invoke-Expression
    if (-not $SkipBuild) {
        foreach ($target in 'auth', 'message-svc', 'gateway', 'smoke') {
            Invoke-Step "build push-messenger/${target}:dev" {
                docker build -f $dockerfile --target $target -t "push-messenger/${target}:dev" $root
```

with

```powershell
    $dockerEnv | Out-String | Invoke-Expression
    if (-not $SkipBuild) {
        foreach ($target in 'auth', 'message-svc', 'gateway', 'worker', 'smoke') {
            Invoke-Step "build push-messenger/${target}:dev" {
                docker build -f $dockerfile --target $target -t "push-messenger/${target}:dev" $root
```

Replace

```powershell
    # The image ids name the build: a rebuilt image rolls the pods, an unchanged one
    # does not.
    $ids = docker image inspect --format '{{.Id}}' push-messenger/auth:dev push-messenger/message-svc:dev push-messenger/gateway:dev push-messenger/smoke:dev
    if ($LASTEXITCODE -ne 0) {
        throw 'the images are not in the cluster yet; run without -SkipBuild'
```

with

```powershell
    # The image ids name the build: a rebuilt image rolls the pods, an unchanged one
    # does not.
    $ids = docker image inspect --format '{{.Id}}' push-messenger/auth:dev push-messenger/message-svc:dev push-messenger/gateway:dev push-messenger/worker:dev push-messenger/smoke:dev
    if ($LASTEXITCODE -ne 0) {
        throw 'the images are not in the cluster yet; run without -SkipBuild'
```

Replace

```powershell
Write-Host "  kubectl --context $Cluster -n ingress-nginx port-forward svc/ingress-nginx-controller 8080:80"
Write-Host '  then connect to ws://127.0.0.1:8080/v1/socket with the header "Host: push.local".'
Write-Host '  auth-svc answers on the same port: http://127.0.0.1:8080/v1/auth/..., /v1/devices, /v1/users.'
Write-Host "Smoke test: deploy/minikube/up.ps1 -Cluster $Cluster -SkipBuild -Smoke"
Write-Host "Tear down:  deploy/minikube/down.ps1 -Cluster $Cluster"
```

with

```powershell
Write-Host "  kubectl --context $Cluster -n ingress-nginx port-forward svc/ingress-nginx-controller 8080:80"
Write-Host '  then connect to ws://127.0.0.1:8080/v1/socket with the header "Host: push.local".'
Write-Host '  auth-svc answers on the same port: http://127.0.0.1:8080/v1/auth/..., /v1/devices, /v1/users;'
Write-Host '  the worker takes signed Resend webhooks at http://127.0.0.1:8080/v1/webhooks/resend.'
Write-Host "Smoke test: deploy/minikube/up.ps1 -Cluster $Cluster -SkipBuild -Smoke"
Write-Host "Tear down:  deploy/minikube/down.ps1 -Cluster $Cluster"
```

In `deploy/minikube/up.sh`:

Replace

```bash
fi

# PostgreSQL runs the init script that creates auth-svc's database only when it
# initialises an empty volume. A volume from before auth-svc (Plan 4B) has no push_auth,
# and auth-svc would never become ready: stop now, not after five slow install attempts.
predates_auth="The Postgres volume predates auth-svc: it has no push_auth database, and PostgreSQL creates one only on an empty volume. Run deploy/minikube/down.sh --cluster $cluster first (it deletes the cluster's data), then run this script again."
# A plain `helm uninstall` removes the StatefulSet but keeps the volume and the Secret
# (helm.sh/resource-policy: keep), so the Secret is the evidence that survives it.
```

with

```bash
fi

# PostgreSQL runs the init scripts that create auth-svc's and the worker's databases only
# when it initialises an empty volume. A volume from before auth-svc (Plan 4B) has no
# push_auth, one from before the worker (Plan 5B) no push_worker, and the services would
# never become ready: stop now, not after five slow install attempts.
predates="The Postgres volume predates auth-svc or the worker: it lacks the push_auth or push_worker database, and PostgreSQL creates them only on an empty volume. Run deploy/minikube/down.sh --cluster $cluster first (it deletes the cluster's data), then run this script again."
# A plain `helm uninstall` removes the StatefulSet but keeps the volume and the Secret
# (helm.sh/resource-policy: keep), so the Secret is the evidence that survives it.
```

Replace

```bash
secret="$(kubectl --context "$cluster" -n "$namespace" get secret "$secret_name" -o name --ignore-not-found)"
if [[ -n "$secret" ]]; then
  auth_password="$(kubectl --context "$cluster" -n "$namespace" get secret "$secret_name" -o jsonpath='{.data.auth-db-password}')"
  if [[ -z "$auth_password" ]]; then
    echo "The kept Secret $secret_name has no auth-db-password: it predates auth-svc. $predates_auth" >&2
    exit 1
  fi
fi
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
    echo "$predates_auth" >&2
    exit 1
  fi
```

with

```bash
secret="$(kubectl --context "$cluster" -n "$namespace" get secret "$secret_name" -o name --ignore-not-found)"
if [[ -n "$secret" ]]; then
  for key in auth-db-password worker-db-password; do
    password="$(kubectl --context "$cluster" -n "$namespace" get secret "$secret_name" -o "jsonpath={.data.$key}")"
    if [[ -z "$password" ]]; then
      echo "The kept Secret $secret_name has no $key: it predates the service that needs it. $predates" >&2
      exit 1
    fi
  done
fi
postgres="$(kubectl --context "$cluster" -n "$namespace" get statefulset push-postgres -o name --ignore-not-found)"
if [[ -n "$postgres" ]]; then
  step 'check that the Postgres volume has the push_auth and push_worker databases'
  kubectl --context "$cluster" -n "$namespace" rollout status statefulset/push-postgres --timeout=300s
  if ! databases="$(kubectl --context "$cluster" -n "$namespace" exec push-postgres-0 -- \
    psql -U push -d postgres -Atc "SELECT count(*) FROM pg_database WHERE datname IN ('push_auth', 'push_worker')")"; then
    echo 'checking the Postgres volume failed' >&2
    exit 1
  fi
  if [[ "$databases" != 2 ]]; then
    echo "$predates" >&2
    exit 1
  fi
```

Replace

```bash
  (
    use_cluster_docker
    for target in auth message-svc gateway smoke; do
      step "build push-messenger/$target:dev"
      docker build -f "$root/deploy/docker/Dockerfile" --target "$target" -t "push-messenger/$target:dev" "$root"
```

with

```bash
  (
    use_cluster_docker
    for target in auth message-svc gateway worker smoke; do
      step "build push-messenger/$target:dev"
      docker build -f "$root/deploy/docker/Dockerfile" --target "$target" -t "push-messenger/$target:dev" "$root"
```

Replace

```bash
if ! ids="$(use_cluster_docker && docker image inspect --format '{{.Id}}' \
  push-messenger/auth:dev push-messenger/message-svc:dev push-messenger/gateway:dev \
  push-messenger/smoke:dev)"; then
  echo 'the images are not in the cluster yet; run without --skip-build' >&2
  exit 1
```

with

```bash
if ! ids="$(use_cluster_docker && docker image inspect --format '{{.Id}}' \
  push-messenger/auth:dev push-messenger/message-svc:dev push-messenger/gateway:dev \
  push-messenger/worker:dev push-messenger/smoke:dev)"; then
  echo 'the images are not in the cluster yet; run without --skip-build' >&2
  exit 1
```

Replace

```bash
echo "  kubectl --context $cluster -n ingress-nginx port-forward svc/ingress-nginx-controller 8080:80"
echo '  then connect to ws://127.0.0.1:8080/v1/socket with the header "Host: push.local".'
echo '  auth-svc answers on the same port: http://127.0.0.1:8080/v1/auth/..., /v1/devices, /v1/users.'

if [[ "$smoke" == true ]]; then
```

with

```bash
echo "  kubectl --context $cluster -n ingress-nginx port-forward svc/ingress-nginx-controller 8080:80"
echo '  then connect to ws://127.0.0.1:8080/v1/socket with the header "Host: push.local".'
echo '  auth-svc answers on the same port: http://127.0.0.1:8080/v1/auth/..., /v1/devices, /v1/users;'
echo '  the worker takes signed Resend webhooks at http://127.0.0.1:8080/v1/webhooks/resend.'

if [[ "$smoke" == true ]]; then
```

```powershell
& 'C:\Program Files\Git\bin\bash.exe' -n deploy/minikube/up.sh; "syntax=$LASTEXITCODE"
git ls-files -s deploy/minikube/up.sh
```

Expected: `syntax=0`, and mode `100755` (still executable).

- [ ] **Step 2: The scripts refuse a cluster from Plan 5B**

If the profile still holds the release of Plan 5B, both scripts must stop before building
anything (they start the profile first if it is stopped):

```powershell
pwsh -NoProfile -File deploy/minikube/up.ps1 -SkipBuild; "exit=$LASTEXITCODE"
& 'C:\Program Files\Git\bin\bash.exe' deploy/minikube/up.sh --skip-build; "exit=$LASTEXITCODE"
```

Expected (recorded while writing this plan, on the cluster Plan 5B left): each stops right
after the ingress-nginx admission jobs with `The kept Secret push-secrets has no
worker-db-password: it predates the service that needs it. The Postgres volume predates
auth-svc or the worker: it lacks the push_auth or push_worker database, and PostgreSQL
creates them only on an empty volume. Run deploy/minikube/down.ps1 -Cluster push first (it
deletes the cluster's data), then run this script again.` (`down.sh --cluster push` from
`up.sh`), and `exit=1`. Without a release from 5B (a fresh profile), both go on to build;
stop them with Ctrl-C and continue.

- [ ] **Step 3: Wipe and bring it up with the smoke test**

```powershell
deploy/minikube/down.ps1
deploy/minikube/up.ps1 -Smoke
```

Expected: `release "push" uninstalled`, the namespace and the released volume deleted; then
five images build (`==> build push-messenger/worker:dev` fourth), the install succeeds at
the first attempt, the notes show
`Email: the worker's log sender; Resend webhooks at http://push.local/v1/webhooks/resend.`,
and `helm test` prints `Phase: Succeeded` and the smoke output. Recorded while revising this
plan (a fresh install after `down.ps1`; the worker looks for work every second, so an email
or a device takes up to about a second; a run may start with `smoke: auth-svc answers 503
Service Unavailable yet; retrying` while the ingress catches up with the backends):

```text
smoke: alice signed up; the worker's log sender delivered the verification email after 253 ms
smoke: alice verified the email, signed in and registered device 01a136b0-5edc-74ae-bb69-567fe02d8c7d (a retry returned the same device) in 322 ms
smoke: alice's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: bob signed up; the worker's log sender delivered the verification email after 1006 ms
smoke: bob verified the email, signed in and registered device 01a136b0-6334-768c-8e48-2d859caa9b5f (a retry returned the same device) in 1110 ms
smoke: bob's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: the worker relayed alice's account and device 01a136b0-5edc-74ae-bb69-567fe02d8c7d to message-svc (waited 0 ms)
smoke: the worker relayed bob's account and device 01a136b0-6334-768c-8e48-2d859caa9b5f to message-svc (waited 1010 ms)
smoke: made alice=01a136b0-5dc1-745e-960b-e862bc3bee26 and bob=01a136b0-5ef5-7083-b88f-0fffb11dd11d contacts with a direct conversation (DEV ONLY setup)
smoke: bob connected (Ready, heartbeat 30000 ms)
smoke: alice connected (Ready, heartbeat 30000 ms)
smoke: alice got SendAck seq=1 after 7 ms
smoke: bob got Deliver seq=1 after 39 ms
smoke: bob acked; the gateway answered the following Ping
smoke: PASS in 2581 ms
```

- [ ] **Step 4: Check what the smoke test does not**

```powershell
kubectl --context push -n push get pods
kubectl --context push -n push logs deploy/push-worker --tail=40
kubectl --context push -n push exec push-postgres-0 -- psql -U push -d postgres -Atc "select datname, pg_get_userbyid(datdba) from pg_database where datname like 'push%' order by 1"
kubectl --context push -n push exec push-postgres-0 -- psql -U push -d push_auth -Atc "select count(*) from auth.outbox"
kubectl --context push -n push exec push-postgres-0 -- psql -U push -d push_worker -Atc "select kind, attempts, dead_at is null, run_after > now() + interval '50 minutes' from worker.job order by kind"
kubectl --context push -n push exec push-postgres-0 -- psql -U push -d push_worker -Atc "select template, status, recipient_domain, params from worker.email_message"
$pw = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String((kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.worker-db-password}')))
kubectl --context push -n push exec push-postgres-0 -- env PGPASSWORD=$pw psql -h 127.0.0.1 -U push_worker -d push_auth -Atc "select count(*) from auth.user_account"
kubectl --context push -n push exec push-postgres-0 -- env PGPASSWORD=$pw psql -h 127.0.0.1 -U push_worker -d push_messaging -Atc "select 1"
kubectl --context push -n push exec push-postgres-0 -- env PGPASSWORD=$pw psql -h 127.0.0.1 -U push_worker -d push_worker -Atc "select rolsuper from pg_roles where rolname = current_user"
$token = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String((kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.accounts-token}')))
"accounts-token length: $($token.Length)"
kubectl --context push -n push logs deploy/push-message-svc | Select-String 'PUSH_ACCOUNTS_TOKEN'
kubectl --context push -n push get deploy push-gateway -o yaml | Select-String 'accounts-token'
```

Expected (recorded while revising this plan): every pod `Running` (`push-smoke`
`Completed`), 0 restarts. The worker's JSON logs show the DEV ONLY warning; `connected` for
postgres (possibly after one `not reachable yet; retrying` while Postgres starts); one
`not reachable yet; retrying` for `auth-svc's database` with
`permission denied for schema auth` (the worker started before auth-svc had granted its
role), then `connected` and `serving HTTP`; and one line per email, without address or body:

```text
{"timestamp":"…","level":"INFO","fields":{"message":"email delivered by the log sender (not sent)","email_id":"01a136b0-5dc2-72d7-b61a-d965e9ad41d5","template":"verify_email","recipient_domain":"smoke.test"},"target":"push_email::sender"}
```

The databases are `push_auth|push_auth`, `push_messaging|push` and
`push_worker|push_worker`; the outbox is empty (`0`); the queue holds the three cleanups,
waiting about an hour (`cleanup.refresh_tokens|0|t|t`, `cleanup.unverified_accounts|0|t|t`,
`cleanup.worker_records|0|t|t`); `worker.email_message` has two `verify_email|2|smoke.test`
rows whose params hold `handle` and `expires_in_minutes` and no link; `push_worker` reading
`auth.user_account` fails with `permission denied for table user_account`, connecting to
`push_messaging` fails with `permission denied for database "push_messaging"` and
`User does not have CONNECT privilege.`; `push_worker` is not a superuser (`f`); the
Accounts token is 48 characters long; message-svc logs no `PUSH_ACCOUNTS_TOKEN is not set`
warning (it has the token, and the smoke test's relayed devices prove the worker presents
it); and the gateway's Deployment never mentions `accounts-token`.

Then post webhooks through the ingress as Resend would, signed with the generated secret,
and check what the ingress routes:

```powershell
$secret = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String((kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.resend-webhook-secret}')))
$key = [Convert]::FromBase64String($secret.Substring(6))
$body = '{"type":"email.bounced","created_at":"2026-10-13T12:00:00.000Z","data":{"email_id":"re-test","to":["bounce@smoke.test"],"bounce":{"type":"Permanent"}}}'
$id = 'msg_minikube_1'; $ts = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$hmac = [Security.Cryptography.HMACSHA256]::new($key)
$sig = 'v1,' + [Convert]::ToBase64String($hmac.ComputeHash([Text.Encoding]::UTF8.GetBytes("$id.$ts.$body")))
$forward = Start-Process kubectl -ArgumentList '--context','push','-n','ingress-nginx','port-forward','svc/ingress-nginx-controller','18080:80' -PassThru -WindowStyle Hidden
Start-Sleep 4
$headers = @{ Host = 'push.local'; 'svix-id' = $id; 'svix-timestamp' = "$ts"; 'svix-signature' = $sig }
foreach ($try in 1..2) {
    $answer = Invoke-WebRequest -Method Post -Uri 'http://127.0.0.1:18080/v1/webhooks/resend' -Headers $headers -ContentType 'application/json' -Body $body -SkipHttpErrorCheck
    "signed webhook -> $($answer.StatusCode)"
}
$malformed = '{"type":"email.bounced"}'
$id2 = 'msg_minikube_2'
$sig2 = 'v1,' + [Convert]::ToBase64String($hmac.ComputeHash([Text.Encoding]::UTF8.GetBytes("$id2.$ts.$malformed")))
$answer = Invoke-WebRequest -Method Post -Uri 'http://127.0.0.1:18080/v1/webhooks/resend' -Headers @{ Host = 'push.local'; 'svix-id' = $id2; 'svix-timestamp' = "$ts"; 'svix-signature' = $sig2 } -ContentType 'application/json' -Body $malformed -SkipHttpErrorCheck
"signed but malformed webhook -> $($answer.StatusCode)"
$answer = Invoke-WebRequest -Method Post -Uri 'http://127.0.0.1:18080/v1/webhooks/resend' -Headers @{ Host = 'push.local' } -ContentType 'application/json' -Body $body -SkipHttpErrorCheck
"unsigned webhook -> $($answer.StatusCode)"
foreach ($path in '/dev/mailbox?to=a@b.test', '/v1/webhooks', '/v1/auth/jwks') {
    $answer = Invoke-WebRequest -Uri "http://127.0.0.1:18080$path" -Headers @{ Host = 'push.local' } -SkipHttpErrorCheck
    "$path -> $($answer.StatusCode)"
}
Stop-Process -Id $forward.Id
kubectl --context push -n push exec push-postgres-0 -- psql -U push -d push_worker -Atc "select address, reason from worker.email_suppression"
kubectl --context push -n push exec push-postgres-0 -- psql -U push -d push_worker -Atc "select event_id, kind from worker.email_event"
```

Expected (recorded): `signed webhook -> 204` twice (the second is a replay, recorded once),
`signed but malformed webhook -> 204` (Plan 6A, decision 11: Resend would retry anything
else for days), `unsigned webhook -> 401`, `/dev/mailbox?to=a@b.test -> 404` and
`/v1/webhooks -> 404` (not routed), `/v1/auth/jwks -> 200`; the suppression list holds
`bounce@smoke.test|1` (hard bounce) and the event table only `msg_minikube_1|2`
(bounced), once. The worker logs `delivery event recorded` with
`"kind":2,"suppressed":1`, for the malformed one `a signed webhook is not a delivery event
we can read; dropped` with `"webhook_id":"msg_minikube_2"`, and for the unsigned one
`webhook refused` with `the webhook lacks its signature headers`.

- [ ] **Step 5: An upgrade keeps the secrets**

```powershell
$before = kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.resend-webhook-secret}'
$beforeToken = kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.accounts-token}'
deploy/minikube/up.ps1 -SkipBuild -Smoke
$after = kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.resend-webhook-secret}'
$afterToken = kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.accounts-token}'
"webhook secret kept: $($before -eq $after)"
"accounts token kept: $($beforeToken -eq $afterToken)"
```

Expected: `==> check that the Postgres volume has the push_auth and push_worker databases`
(the release's StatefulSet exists now), `REVISION: 2`, the smoke test passes again
(recorded: `PASS in 3211 ms`), `webhook secret kept: True` and `accounts token kept: True`.

- [ ] **Step 6: The up scripts refuse a volume without `push_worker`**

Drop the database (the worker first, so nothing holds a connection) to stand in for a
volume from Plan 5B whose Secret was rewritten, then run both scripts:

```powershell
kubectl --context push -n push scale deploy/push-worker --replicas=0
kubectl --context push -n push exec push-postgres-0 -- psql -U push -d postgres -c "DROP DATABASE push_worker WITH (FORCE)"
pwsh -NoProfile -File deploy/minikube/up.ps1 -SkipBuild; "exit=$LASTEXITCODE"
& 'C:\Program Files\Git\bin\bash.exe' deploy/minikube/up.sh --skip-build; "exit=$LASTEXITCODE"
```

Expected: `DROP DATABASE`; each script prints `==> check that the Postgres volume has the
push_auth and push_worker databases` and stops within seconds, before building or
installing anything, with `The Postgres volume predates auth-svc or the worker: it lacks
the push_auth or push_worker database, …` and `exit=1` (recorded while writing this plan;
the revision did not change the scripts). Task 4 starts with `down.ps1`, which leaves a
fresh cluster again.

- [ ] **Step 7: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add deploy/minikube
git commit -m "deploy: build and roll the worker image on minikube"
```

Expected: 775 tests pass.

---

### Task 4: CI, README, spec, roadmap and pull request

**Files:**
- Modify: `.github/workflows/ci.yml`, `README.md`,
  `docs/superpowers/specs/2026-10-11-push-messenger-design.md`,
  `docs/superpowers/plans/2026-10-11-roadmap.md`

- [ ] **Step 1: Build the worker image in CI**

In `.github/workflows/ci.yml`, job `images`, replace
`          for target in auth message-svc gateway smoke; do` with

```yaml
          for target in auth message-svc gateway worker smoke; do
```

- [ ] **Step 2: README**

In `README.md`, section `Run it on minikube`, in the first paragraph after the command,
replace `email, register a device and chat through the ingress; the DEV ONLY mailbox and the
setup calls go` with

```markdown
email (the worker's log sender delivers it), register a device, wait for the worker to
relay it to message-svc, and chat through the ingress; the DEV ONLY mailbox and the setup
calls go
```

Replace the paragraph that starts `The minikube values switch on DEV ONLY shortcuts` (up
to `the chart without a cluster.`) with:

```markdown
The minikube values switch on DEV ONLY shortcuts: the gateway accepts any user and device
id without a token (until it checks access tokens, Plan 8), message-svc serves a setup API
for contacts and conversations (until Plan 7), and the worker, which logs emails instead of
sending them, shows them at `/dev/mailbox`; neither is routed by the ingress, which
reaches the worker only at `/v1/webhooks/resend`. The chart refuses that mode unless the
values also say `devMode.acknowledge: insecure-local-only` and the ingress host is local
(`*.local`, `*.localhost` or `*.test`), and refuses it with the Resend sender.
`pwsh deploy/helm/test-chart.ps1` checks the chart without a cluster.
```

Replace the paragraph that starts `auth-svc has a database and a role of its own` (up to
`then `up` again.`) with:

```markdown
auth-svc and the worker have databases and roles of their own, `push_auth` and
`push_worker`, which PostgreSQL's init scripts create when it initialises an empty volume;
the worker's role may also drain auth-svc's outbox, which auth-svc grants at start-up.
message-svc and the worker share a generated Accounts token from the same Secret, which
the gateway does not get. A
cluster whose Postgres volume predates them (Plans 4B and 5B) lacks a database. `up` stops
when the kept Secret `push-secrets` (it survives a plain `helm uninstall`) has no
`auth-db-password` or `worker-db-password`, or when the running Postgres lacks `push_auth`
or `push_worker`: run `deploy/minikube/down.ps1` once, then `up` again.
```

In the `push-auth` table, delete the last row (the one for `PUSH_DEV_MAILBOX`).

- [ ] **Step 3: Spec**

In §5.6, replace the bullet that starts `- **DEV ONLY mailbox.** auth-svc started with` (up
to `like the chart's guard.`) with:

```markdown
- **DEV ONLY mailbox.** Until Plan 6B auth-svc served the outbox's emails at
  `/dev/mailbox`; the worker's log sender keeps them now (§9), and auth-svc has no mailbox.
```

In §9, after the Plan 6A notes, add a blank line and:

```markdown
Implementation notes (Plan 6B): the chart's default sender is `log`; `resend` needs
`worker.email.resendApiKey` and `worker.email.from` and is refused in dev mode. The webhook
Ingress routes only `/v1/webhooks/resend` (Exact), and only when a webhook secret exists
(generated in dev mode). Verified on minikube: a signed webhook through the ingress is
recorded once and suppresses a hard bounce; an unsigned one answers 401.
```

In §11, in the Plan 4B implementation notes, after the bullet about auth-svc (Plan 5B;
it ends `a 30 s termination grace period.`), add:

```markdown
- The worker (Plan 6B) is a fourth Deployment with its own database `push_worker` and role
  `push_worker`, created by a second init script, which also grants the role `CONNECT` on
  `push_auth`; auth-svc grants it `auth.outbox` and two cleanup functions at start-up
  (`PUSH_AUTH_WORKER_ROLE`). The Secret holds its password and two URLs, the Accounts token
  (generated and kept; message-svc and the worker read it, the gateway does not), and
  Resend's API key and webhook secret when set. Its only Ingress routes `Exact`
  `/v1/webhooks/resend`; its DEV ONLY mailbox is not routed, and dev mode allows one worker
  only. It has a 5 s preStop sleep and a 30 s grace period.
```

Replace the bullet that starts ``- `deploy/minikube/up.ps1` (and `up.sh`) wait for
ingress-nginx`` (up to `may replace the in-chart PostgreSQL and Redis.`) with:

```markdown
- `deploy/minikube/up.ps1` (and `up.sh`) wait for ingress-nginx, stop at once with
  "run down.ps1 first" when an existing PostgreSQL volume lacks the `push_auth` or
  `push_worker` database (or the kept Secret their passwords), retry
  `helm upgrade --install` up to 5 times, and bring it up on minikube; `helm test` runs
  `push-smoke` inside the cluster through the ingress controller: two users sign up, verify
  with the emails the worker's log sender delivered to the DEV ONLY mailbox, register
  devices, wait for the worker to relay them to message-svc, and chat. The Azure plan
  replaces the in-chart secrets with Key Vault and adds TLS, and may replace the in-chart
  PostgreSQL and Redis.
```

- [ ] **Step 4: Mark plan 6B in review**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the empty status cell at the end
of the row for plan 6B (`| 6B | Worker and email: minikube | … | 6A | |`) with
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

Expected: no diff, no warnings, 775 tests on both toolchains, `all chart checks passed`,
and the smoke test `PASS` as in Task 3, Step 3. Copy the `smoke:` lines for the pull
request. Then stop the profile (keep it for later plans) and restore the kubectl context you
noted at the start:

```powershell
minikube stop -p push
kubectl config use-context <the context you noted>
```

- [ ] **Step 6: Commit and push**

```powershell
git add .github README.md docs
git commit -m "docs: record plan 6B in the spec, README, roadmap and CI"
git push -u origin feature/plan-06b-worker-minikube
```

- [ ] **Step 7: Open the pull request and wait for CI**

Put the smoke output from Step 5 into the body (replace the placeholder line):

```powershell
$body = @'
Implements docs/superpowers/plans/2026-10-13-plan-06b-worker-minikube.md: the smoke test runs the real flow (the worker relays email.send and its log sender delivers the verification email to the DEV ONLY mailbox, now the worker's; the smoke test waits for the worker to relay users and devices to message-svc instead of mirroring them through DevSetup); auth-svc's own mailbox is removed; a worker image target; the worker in the chart with its own database and role (push_worker, a second PostgreSQL init script, CONNECT on push_auth, auth-svc grants the outbox and two cleanup functions), Resend credentials and a webhook secret in the Secret (generated in dev mode), an Ingress for /v1/webhooks/resend only, the dev mailbox behind the dev-mode guard, refusals for resend without a key or in dev mode and for dev mode with more than one worker; the Accounts token generated into the kept Secret for message-svc and the worker only; up scripts and CI build the new image, and the up scripts stop at once on a volume or Secret from before the worker; test-chart.ps1 has 28 checks.

Verified on minikube (deploy/minikube/up.ps1 -Smoke):

<paste the smoke: lines here>
'@
gh pr create --base main --title "Plan 6B: worker and email on minikube" --body $body
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust`, `msrv`, `images`, `chart`). Do not merge; the
controller merges after the final review and its own minikube run.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §2 each service owns one database (the worker's role reaches `push_auth` only for the outbox and two cleanups, never `push_messaging`) | 2, 3 |
| §2 only the worker may call message-svc's `Accounts` API: the token in the Secret, for message-svc and the worker only | 2, 3 |
| §2 the worker relays auth-svc's user and device events to message-svc; sends email | 1, 3 |
| §5.2, §5.3 verification through an emailed link; devices relayed from auth-svc, the DEV ONLY mirror gone | 1, 3 |
| §7.6 DEV ONLY switches, strict and off by default; the mailbox only beside the log sender | 1, 2 |
| §9 the log sender locally; Resend behind the trait, its key and webhook secret in the Secret; webhooks verified, recorded once, hard bounces suppressed | 2, 3 |
| §11 secrets generated, kept in the Secret, never in git or a ConfigMap; probes; graceful shutdown (preStop sleep, grace period); hardened containers; the same chart as Azure | 2, 3 |
| Roadmap: every plan verified on minikube before merge | 3, 4 |

## Deferred to later plans

- **Contacts and conversations without `DevSetup`** (Plan 7): the smoke test still makes
  its two users contacts and opens their conversation through the DEV ONLY setup API.
- **Real tokens on the socket** (Plan 8): the smoke test still identifies with the gateway's
  DEV ONLY identity.
- **Production email** (Plan 11): Resend's API key and webhook secret through Key Vault, the
  sending domain with SPF, DKIM and DMARC, open and click tracking off at Resend, TLS on the
  webhook path; the owner decisions of Plan 6A.
- **A NetworkPolicy** for message-svc's `Accounts` API and the worker's port — a Plan 11
  must-do; until then only the shared token keeps `Accounts` to the worker.
- **Several workers on a local cluster**: the DEV ONLY mailbox is in memory, per replica, so
  the chart refuses dev mode with more than one.
- **Checking `worker.email.webhookSecret`'s format when the chart renders** (`whsec_` and
  base64); today the worker refuses a malformed secret at start-up, after the install.
- **A dev webhook secret can outlive dev mode**: the generated secret stays in the kept
  Secret if the same release later leaves dev mode (Plan 11's Key Vault secrets replace it).
- **Unsetting `PUSH_AUTH_WORKER_ROLE` does not revoke the grants** auth-svc gave the role
  earlier; revoking them is manual SQL until a deployment needs it.
- **`x-real-ip`, the nginx error log, the superuser `push`** — as listed in Plan 5B,
  unchanged (Plan 11).
