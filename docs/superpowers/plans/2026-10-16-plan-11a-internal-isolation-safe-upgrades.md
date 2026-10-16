# Plan 11A: Internal Isolation and Safe Upgrades Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Work sequentially; do not create worktrees or parallel feature branches.

**Goal:** Enforce the three internal API caller boundaries and remove message-svc's database superuser access without deleting existing installations' data.

**Architecture:** Keep one message-svc process and domain service, but expose Messaging, Accounts and Directory on three independent listeners with their existing tokens. The Helm chart supplies application-scoped ingress NetworkPolicies and bootstraps a dedicated messaging database role; an operator-run, transactional ownership migration upgrades existing data during an explicit maintenance window.

**Tech Stack:** Existing Rust 2024/MSRV 1.85 workspace, Tokio/tonic, PostgreSQL 17/sqlx, Helm 3, Kubernetes NetworkPolicy, PowerShell 7, Docker and minikube with an enforcing CNI.

---

## Approved specification and scope

Read `docs\superpowers\specs\2026-10-16-portable-deployment-readiness-design.md`,
sections 1-4 and the 11A acceptance criteria in section 8.

This plan implements **11A only**. Existing-Secret selection, controller-neutral
Ingress/TLS, image digests and external configuration are 11B. Load tools are
11C. No GitHub release/publish/deploy workflow, registry dependency, Azure
provisioning or consumer cluster metadata is added. Existing contributor CI is
not removed.

Base: `018cdca` contains the specification; the implementation base must also
include this reviewed plan. Current application base is `bf93bbd` (PR #19).
Do not start implementation until the owner approves the plan. Integrate the
documentation branch through the normal repository process before creating the
implementation branch.

The snippets below are additions or exact replacement regions, not instructions
to overwrite unrelated code in an existing file. Preserve current functionality
and tests outside those regions.

## File map

| File | Responsibility |
|---|---|
| `crates\push-message-svc\src\config.rs` | Three API listener addresses |
| `crates\push-message-svc\src\listeners.rs` (new) | Bind every listener before serving; drop all on partial bind failure |
| `crates\push-message-svc\src\server.rs` | Three token-protected servers over one domain service; coordinated shutdown |
| `crates\push-message-svc\src\main.rs`, `src\lib.rs` | Bind/export the listener bundle |
| `crates\push-message-svc\tests\service.rs` | Per-listener API routing and authentication; existing integration scenarios |
| `crates\push-smoke\tests\local.rs` | Local whole-stack caller wiring |
| `deploy\docker\Dockerfile` | Expose all three internal ports |
| `deploy\helm\push-messenger\templates\message-svc.yaml` | Three named ports on the existing Service and container |
| `deploy\helm\push-messenger\templates\auth.yaml`, `worker.yaml` | Directory and Accounts URLs |
| `deploy\helm\push-messenger\templates\networkpolicy.yaml` (new) | Release-scoped application ingress policies |
| `deploy\helm\push-messenger\templates\_helpers.tpl` | Explicit caller label shared by workloads and policies |
| `deploy\helm\push-messenger\values.yaml`, `values-minikube.yaml` | Policy switch, explicit controller selectors and dedicated DB password |
| `deploy\helm\push-messenger\files\postgres\messaging-role.sql` (new) | Transactional, password-free ownership migration for existing installations |
| `deploy\helm\push-messenger\templates\postgres-init.yaml`, `postgres.yaml`, `secret.yaml` | Fresh-install bootstrap and separate messaging credentials |
| `crates\push-store\tests\deployment_role.rs` (new) | Execute the actual ownership SQL against PostgreSQL 17 with seeded data |
| `deploy\helm\test-chart.ps1` | Render and reject invalid topology/credential configuration |
| `deploy\minikube\test-network-policy.ps1` (new) | Runtime allow/deny connection matrix; fail if policies are not enforced |
| `deploy\minikube\up.ps1`, `up.sh` | Refuse an unprepared legacy upgrade without deleting data |
| `docs\deployment-upgrade.md` (new) | Backup, maintenance, credential transition, verification and rollback checkpoints |
| `README.md`, configuration docs, roadmap | Consumer-facing changed internal-port contract and upgrade instructions |

## Before implementation

- Docker must answer `docker version`. Integration tests use Postgres and Redis.
- Run from the repository root; use Windows paths in PowerShell commands.
- Refresh PATH in each fresh shell and disable incremental artifacts:

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' +
    [Environment]::GetEnvironmentVariable('Path','User')
$env:CARGO_INCREMENTAL = '0'
```

- Start from a clean, current `main` containing the approved documentation:

```powershell
git switch main
git pull --ff-only
git switch -c feature/plan-11a-internal-isolation
cargo test -p push-message-svc -p push-smoke --locked
pwsh .\deploy\helm\test-chart.ps1
```

Expected: existing service/smoke tests and chart checks pass. Stop on failures;
do not confuse baseline failures with intended red tests.

- Every task follows red -> green. Run the smallest named test first, then the
  task's affected suite. Do not mutate production files to manufacture a red
  result when a new regression test can demonstrate the missing behaviour.
- Every commit includes:

```text
Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>
```

## Task 1: Bind the three API listeners as one startup unit

**Files:** `config.rs`, new `listeners.rs`, `lib.rs`, `main.rs`;
`crates\push-message-svc\tests\service.rs` and
`crates\push-smoke\tests\local.rs`.

- [ ] **Step 1: Add a red configuration test to the existing config test module.**

```rust
#[test]
fn malformed_additional_api_addresses_are_not_ignored() {
    for name in ["PUSH_ACCOUNTS_GRPC_ADDR", "PUSH_DIRECTORY_GRPC_ADDR"] {
        let mut variables = REQUIRED.to_vec();
        variables.push((name, "not-an-address"));
        assert!(
            matches!(config(&variables), Err(ConfigError::Invalid { name: bad, .. }) if bad == name)
        );
    }
}
```

- [ ] **Step 2: Run the test.**

```powershell
cargo test -p push-message-svc --lib malformed_additional_api_addresses --locked
```

Expected red: both new variables are currently ignored.

- [ ] **Step 3: Add configuration fields, parsing, Debug fields and documented defaults.**

Add next to `grpc_addr`:

```rust
pub accounts_grpc_addr: SocketAddr,
pub directory_grpc_addr: SocketAddr,
```

In `Config::from_lookup` add these initializer entries:

```rust
accounts_grpc_addr: parsed(
    &lookup, "PUSH_ACCOUNTS_GRPC_ADDR", ([0, 0, 0, 0], 50052).into(),
)?,
directory_grpc_addr: parsed(
    &lookup, "PUSH_DIRECTORY_GRPC_ADDR", ([0, 0, 0, 0], 50053).into(),
)?,
```

Add these Debug fields without changing credential redaction:

```rust
.field("accounts_grpc_addr", &self.accounts_grpc_addr)
.field("directory_grpc_addr", &self.directory_grpc_addr)
```

Rename the documented meaning of `PUSH_GRPC_ADDR` to Messaging and document the
two new variables with the above defaults.

Create `listeners.rs`:

```rust
use anyhow::Context;
use tokio::net::TcpListener;

use crate::Config;

pub struct Listeners {
    pub messaging: TcpListener,
    pub accounts: TcpListener,
    pub directory: TcpListener,
    pub health: TcpListener,
}

impl Listeners {
    pub async fn bind(config: &Config) -> anyhow::Result<Self> {
        let messaging = TcpListener::bind(config.grpc_addr)
            .await.context("bind Messaging listener")?;
        let accounts = TcpListener::bind(config.accounts_grpc_addr)
            .await.context("bind Accounts listener")?;
        let directory = TcpListener::bind(config.directory_grpc_addr)
            .await.context("bind Directory listener")?;
        let health = TcpListener::bind(config.health_addr)
            .await.context("bind health listener")?;
        Ok(Self { messaging, accounts, directory, health })
    }
}
```

Export it from `lib.rs`:

```rust
mod listeners;
pub use listeners::Listeners;
```

Do not change `serve` yet. The binder is independently testable before Task 2
wires it into the process.

- [ ] **Step 4: Add and run binder tests in `listeners.rs`.**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::from_lookup(|name| match name {
            "PUSH_DATABASE_URL" => Some("postgres://unused@localhost/push_messaging".into()),
            "PUSH_REDIS_URL" => Some("redis://localhost:6379".into()),
            "PUSH_GRPC_ADDR" | "PUSH_ACCOUNTS_GRPC_ADDR" |
            "PUSH_DIRECTORY_GRPC_ADDR" | "PUSH_HEALTH_ADDR" =>
                Some("127.0.0.1:0".into()),
            _ => None,
        }).unwrap()
    }

    #[tokio::test]
    async fn all_listeners_bind_distinct_ports() {
        let bound = Listeners::bind(&config()).await.unwrap();
        let addresses = [
            bound.messaging.local_addr().unwrap(),
            bound.accounts.local_addr().unwrap(),
            bound.directory.local_addr().unwrap(),
            bound.health.local_addr().unwrap(),
        ];
        let unique: std::collections::HashSet<_> = addresses.into_iter().collect();
        assert_eq!(unique.len(), 4);
    }

    #[tokio::test]
    async fn a_partial_bind_failure_releases_earlier_listeners() {
        let temporary = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let messaging = temporary.local_addr().unwrap();
        drop(temporary);
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = config();
        config.grpc_addr = messaging;
        config.accounts_grpc_addr = occupied.local_addr().unwrap();
        let error = Listeners::bind(&config()).await.err().unwrap();
        assert!(error.to_string().contains("Accounts"));
        let rebound = TcpListener::bind(messaging).await.unwrap();
        drop(rebound);
    }
}
```

Update both `Config` literals in `tests\service.rs` and the literal in
`push-smoke\tests\local.rs` with loopback port-zero values for the new fields.
These temporary literal entries are consumed/replaced by Task 2's real binding.

```rust
accounts_grpc_addr: "127.0.0.1:0".parse().unwrap(),
directory_grpc_addr: "127.0.0.1:0".parse().unwrap(),
```

Add assertions for new defaults and custom-address overrides to the existing
config tests using the two new fields. Run:

```powershell
cargo test -p push-message-svc --lib --locked
cargo check -p push-message-svc -p push-smoke --all-targets --locked
```

Expected: green, without changing service runtime behaviour yet.

- [ ] **Step 5: Commit.**

```powershell
git add -- crates\push-message-svc crates\push-smoke\tests\local.rs
git commit -m "feat: configure and bind dedicated internal API listeners" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 2: Serve only the intended API on each listener

**Files:** `server.rs`, `main.rs`, `tests\service.rs`,
`crates\push-smoke\tests\local.rs`.

- [ ] **Step 1: Add a regression test before changing the server.**

Add to `tests\service.rs`; existing `start()` and `accounts_client` suffice:

```rust
#[tokio::test]
async fn the_messaging_listener_does_not_serve_accounts() {
    let running = start().await;
    let mut client = accounts_client(format!("http://{}", running.grpc)).await;
    let outcome = client.device_registered(AccountDevice::default()).await;
    assert_eq!(outcome.unwrap_err().code(), tonic::Code::Unimplemented);
    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
}
```

- [ ] **Step 2: Run it red.**

```powershell
cargo test -p push-message-svc --test service the_messaging_listener_does_not_serve_accounts --locked
```

Expected: not `Unimplemented`; the shared listener currently routes the call.

- [ ] **Step 3: Change `serve` to consume the listener bundle.**

Add imports:

```rust
use tokio::task::JoinSet;
use crate::Listeners;
```

Remove the now-unused `tokio::net::TcpListener` import. Replace the signature and
destructure before spawning the existing health server:

```rust
pub async fn serve(
    config: Config,
    listeners: Listeners,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let Listeners { messaging: grpc, accounts, directory, health } = listeners;
```

Keep dependency retry, migrations, warnings and health setup. Replace the single
tonic server block (after constructing `Arc<Messaging<...>>`) through the final
`finish(health_server).await` with:

```rust
let mut servers = JoinSet::new();
servers.spawn(
    tonic::transport::Server::builder()
        .add_service(MessagingServer::with_interceptor(
            MessagingApi::new(Arc::clone(&messaging)),
            InternalToken::messaging(config.messaging_token.clone()),
        ))
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(grpc),
            shutdown.clone().cancelled_owned(),
        ),
);
servers.spawn(
    tonic::transport::Server::builder()
        .add_service(AccountsServer::with_interceptor(
            AccountsApi::new(Arc::clone(&messaging)),
            InternalToken::accounts(config.accounts_token.clone()),
        ))
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(accounts),
            shutdown.clone().cancelled_owned(),
        ),
);
servers.spawn(
    tonic::transport::Server::builder()
        .add_service(DirectoryServer::with_interceptor(
            DirectoryApi::new(messaging),
            InternalToken::directory(config.directory_token.clone()),
        ))
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(directory),
            shutdown.clone().cancelled_owned(),
        ),
);
tracing::info!(
    messaging = %config.grpc_addr,
    accounts = %config.accounts_grpc_addr,
    directory = %config.directory_grpc_addr,
    "serving internal APIs"
);
let first = servers.join_next().await;
let expected_shutdown = shutdown.is_cancelled();
shutdown.cancel();
let mut failure = match first {
    Some(Ok(Ok(()))) if expected_shutdown => None,
    Some(Ok(Ok(()))) => Some(anyhow::anyhow!("an API listener stopped unexpectedly")),
    Some(Ok(Err(error))) => Some(error.into()),
    Some(Err(error)) => Some(error.into()),
    None => Some(anyhow::anyhow!("no API listeners were running")),
};
while let Some(outcome) = servers.join_next().await {
    let error = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(anyhow::Error::from(error)),
        Err(error) => Some(anyhow::Error::from(error)),
    };
    if let Some(error) = error {
        tracing::error!(%error, "internal API listener failed");
        if failure.is_none() {
            failure = Some(error);
        }
    }
}
store.close().await;
let health_result = finish(health_server).await;
if let Some(error) = failure {
    health_result?;
    return Err(error);
}
health_result
```

All servers must complete before the shared store closes. No spawned sibling is
abandoned on the first failure. Keep readiness false during dependency startup
and shutdown; every listener is pre-bound before `serve` is called.

In `main.rs`, replace the two individual binds with `Listeners::bind` and call
the new `serve` signature:

```rust
use push_message_svc::{Config, Listeners, init_logging, serve, terminated};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    let config = Config::from_env()?;
    let listeners = Listeners::bind(&config).await?;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        terminated().await;
        tracing::info!("shutting down");
        signal.cancel();
    });
    serve(config, listeners, shutdown).await
}
```

- [ ] **Step 4: Adapt real integration fixtures and strengthen listener tests.**

In `Running`, retain `grpc` as Messaging and add:

```rust
accounts: SocketAddr,
directory: SocketAddr,
```

In `start()`, bind Accounts and Directory alongside the existing two listeners,
record their actual addresses in `Config`/`Running`, and pass:

```rust
push_message_svc::Listeners {
    messaging: grpc_listener,
    accounts: accounts_listener,
    directory: directory_listener,
    health: health_listener,
}
```

Update the unreachable-database shutdown test by binding the same two extra
listeners before constructing its `Config` and passing a bundle. Keep its
existing shutdown-time assertion.

Use separate URLs in the existing business integration tests:

```rust
let mut accounts = accounts_client(format!("http://{}", running.accounts)).await;
let mut messaging = messaging_client(format!("http://{}", running.grpc)).await;
```

In `accounts_are_served_only_with_the_workers_token`, create the unauthenticated
Accounts channel from `running.accounts`; create a second channel from
`running.grpc` for the unauthenticated Messaging check. Preserve the revocation
and token assertions. In the Directory section of
`the_client_requests_and_the_directory_run_on_postgres`, change only its
Directory channel address to `running.directory`.

Add this full routing matrix test:

```rust
#[tokio::test]
async fn each_listener_registers_only_its_own_api() {
    let running = start().await;
    for (addr, own) in [
        (running.grpc, "messaging"),
        (running.accounts, "accounts"),
        (running.directory, "directory"),
    ] {
        let channel = Channel::from_shared(format!("http://{addr}"))
            .unwrap().connect().await.unwrap();
        if own != "accounts" {
            let status = AccountsClient::new(channel.clone())
                .device_registered(AccountDevice::default()).await.unwrap_err();
            assert_eq!(status.code(), tonic::Code::Unimplemented);
        }
        if own != "directory" {
            let status = DirectoryClient::new(channel.clone())
                .visible_users(VisibleUsersRequest::default()).await.unwrap_err();
            assert_eq!(status.code(), tonic::Code::Unimplemented);
        }
        if own != "messaging" {
            let status = MessagingClient::new(channel.clone())
                .identify(IdentifyRequest::default()).await.unwrap_err();
            assert_eq!(status.code(), tonic::Code::Unimplemented);
        }
    }
    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
}
```

The existing own-listener tests must continue to prove calls work with their
token and fail without it. Retain all existing domain assertions.

In `push-smoke\tests\local.rs`, bind Accounts and Directory when Messaging is
bound, before constructing auth configuration:

```rust
let accounts = TcpListener::bind("127.0.0.1:0").await.unwrap();
let directory = TcpListener::bind("127.0.0.1:0").await.unwrap();
let accounts_url = format!("http://{}", accounts.local_addr().unwrap());
let directory_url = format!("http://{}", directory.local_addr().unwrap());
```

Set auth's `PUSH_MESSAGE_SVC_URL` to `directory_url`, worker's
`message_svc_url` to `accounts_url`, retain gateway's `message_svc_url`, record
the two addresses in message-svc `Config`, and pass the four listeners in its
bundle.

- [ ] **Step 5: Run the affected suites and commit.**

```powershell
cargo test -p push-message-svc -p push-smoke --locked
cargo clippy -p push-message-svc -p push-smoke --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git add -- crates\push-message-svc crates\push-smoke\tests\local.rs
git commit -m "feat: isolate internal gRPC APIs on dedicated listeners" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Expected: all service scenarios and whole-stack local smoke pass; no public
protocol or SDK changes.

## Task 3: Wire the chart and container metadata to the new listeners

**Files:** `message-svc.yaml`, `auth.yaml`, `worker.yaml`, `Dockerfile`,
`deploy\helm\test-chart.ps1`.

- [ ] **Step 1: Add render regressions using the existing `Check` and `Doc` helpers.**

```powershell
Check 'message-svc exposes three separate API ports and callers choose their own' {
    $service = Doc $default 'Service' 'push-message-svc'
    $pod = Doc $default 'Deployment' 'push-message-svc'
    $auth = Doc $default 'Deployment' 'push-auth'
    $worker = Doc $default 'Deployment' 'push-worker'
    $gateway = Doc $default 'Deployment' 'push-gateway'
    $service -match 'name: messaging\s+port: 50051' -and
        $service -match 'name: accounts\s+port: 50052' -and
        $service -match 'name: directory\s+port: 50053' -and
        $pod -match 'name: accounts\s+containerPort: 50052' -and
        $pod -match 'name: directory\s+containerPort: 50053' -and
        $auth -match 'value: "http://push-message-svc:50053"' -and
        $worker -match 'value: "http://push-message-svc:50052"' -and
        $gateway -match 'value: "http://push-message-svc:50051"'
}
```

- [ ] **Step 2: Run the chart tests and observe the new failure.**

```powershell
pwsh .\deploy\helm\test-chart.ps1
```

- [ ] **Step 3: Replace message-svc's Service port list with:**

```yaml
  ports:
    - name: messaging
      port: 50051
      targetPort: messaging
      appProtocol: grpc
    - name: accounts
      port: 50052
      targetPort: accounts
      appProtocol: grpc
    - name: directory
      port: 50053
      targetPort: directory
      appProtocol: grpc
```

Replace the container's `grpc` port entry with:

```yaml
            - name: messaging
              containerPort: 50051
            - name: accounts
              containerPort: 50052
            - name: directory
              containerPort: 50053
```

Retain the health port. Add explicit listener environment entries:

```yaml
            - name: PUSH_GRPC_ADDR
              value: "0.0.0.0:50051"
            - name: PUSH_ACCOUNTS_GRPC_ADDR
              value: "0.0.0.0:50052"
            - name: PUSH_DIRECTORY_GRPC_ADDR
              value: "0.0.0.0:50053"
```

Change only the `PUSH_MESSAGE_SVC_URL` format string in `auth.yaml` to
`http://%s:50053` and in `worker.yaml` to `http://%s:50052`. Gateway stays on
50051. Update the existing Directory URL assertion in `test-chart.ps1` from
50051 to 50053; do not weaken its token-isolation assertions.

In the Dockerfile's message-svc target replace its `EXPOSE` line:

```dockerfile
EXPOSE 50051 50052 50053 8081
```

- [ ] **Step 4: Run chart tests and the local Rust smoke suite, then commit.**

```powershell
pwsh .\deploy\helm\test-chart.ps1
cargo test -p push-smoke --test local --locked
git add -- deploy\helm deploy\docker\Dockerfile
git commit -m "feat: wire Helm callers to dedicated internal API ports" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Do not run the standard cluster bring-up against a valuable existing installation:
the dedicated-role upgrade is not ready until Tasks 4, 5 and 8.

## Task 4: Implement and prove the password-free ownership migration

**Files:** new `files\postgres\messaging-role.sql`,
new `crates\push-store\tests\deployment_role.rs`.

- [ ] **Step 1: Add the real-Postgres regression test file.**

Use a runtime file read rather than `include_str!`, so the initial test fails
with the explicit missing-migration assertion rather than a compiler error.
The fixture uses test-only credentials, never a deployment connection string.

```rust
use std::path::Path;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, Executor, PgConnection};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

async fn legacy() -> (ContainerAsync<Postgres>, String, sqlx::PgPool) {
    let container = Postgres::default().with_tag("17-alpine").start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let root = format!("postgres://postgres:postgres@127.0.0.1:{port}");
    let mut admin = PgConnection::connect(&format!("{root}/postgres")).await.unwrap();
    sqlx::raw_sql(
        "CREATE ROLE push LOGIN SUPERUSER PASSWORD 'legacy_test_only';
         CREATE ROLE push_auth LOGIN PASSWORD 'auth_test_only';
         CREATE ROLE push_worker LOGIN PASSWORD 'worker_test_only';"
    ).execute(&mut admin).await.unwrap();
    for sql in [
        "CREATE DATABASE push_messaging OWNER push",
        "CREATE DATABASE push_auth OWNER push_auth",
        "CREATE DATABASE push_worker OWNER push_worker",
        "REVOKE ALL ON DATABASE push_auth FROM PUBLIC",
        "REVOKE ALL ON DATABASE push_worker FROM PUBLIC",
        "REVOKE ALL ON DATABASE push_messaging FROM PUBLIC",
    ] {
        admin.execute(sql).await.unwrap();
    }
    admin.close().await.unwrap();
    let old_url = format!("postgres://push:legacy_test_only@127.0.0.1:{port}/push_messaging");
    push_store::migrate_database(&old_url).await.unwrap();
    let pool = PgPoolOptions::new().max_connections(1)
        .connect(&format!("{root}/push_messaging")).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO messaging.known_user(user_id)
             VALUES ('00000000-0000-0000-0000-000000000001'),
                    ('00000000-0000-0000-0000-000000000002');
         INSERT INTO messaging.member_device(device_id,user_id)
             VALUES ('00000000-0000-0000-0000-000000000004',
                     '00000000-0000-0000-0000-000000000001');
         INSERT INTO messaging.contact(owner_id,peer_id,state)
             VALUES ('00000000-0000-0000-0000-000000000001',
                     '00000000-0000-0000-0000-000000000002',3);
         INSERT INTO messaging.conversation(conversation_id,kind,created_by,last_seq)
             VALUES ('00000000-0000-0000-0000-000000000003',1,
                     '00000000-0000-0000-0000-000000000001',1);
         INSERT INTO messaging.message(
             conversation_id,seq,message_id,client_msg_id,kind,
             sender_id,sender_device_id,content_type,body)
             VALUES ('00000000-0000-0000-0000-000000000003',1,
                     '00000000-0000-0000-0000-000000000005',
                     '00000000-0000-0000-0000-000000000006',1,
                     '00000000-0000-0000-0000-000000000001',
                     '00000000-0000-0000-0000-000000000004',1,
                     convert_to('kept history','UTF8'));
         INSERT INTO messaging.device_cursor(device_id,conversation_id,delivered_seq)
             VALUES ('00000000-0000-0000-0000-000000000004',
                     '00000000-0000-0000-0000-000000000003',1);"
    ).execute(&pool).await.unwrap();
    (container, root, pool)
}

fn migration() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..").join("..").join("deploy").join("helm")
        .join("push-messenger").join("files").join("postgres")
        .join("messaging-role.sql");
    std::fs::read_to_string(path).expect("the ownership migration must exist")
}

async fn snapshot(pool: &sqlx::PgPool) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for table in ["known_user", "member_device", "contact", "conversation", "message", "device_cursor"] {
        let sql = format!(
            "SELECT row_to_json(t)::text FROM messaging.{table} t ORDER BY 1"
        );
        rows.push(sqlx::query_scalar(&sql).fetch_all(pool).await.unwrap());
    }
    rows
}

#[tokio::test]
async fn ownership_upgrade_preserves_data_and_other_databases() {
    let (_container, root, pool) = legacy().await;
    let before = snapshot(&pool).await;
    let owners_before: Vec<(String, String)> = sqlx::query_as(
        "SELECT datname::text, pg_get_userbyid(datdba)::text
         FROM pg_database WHERE datname <> 'push_messaging' ORDER BY datname"
    ).fetch_all(&pool).await.unwrap();
    let sql = migration();
    sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
    sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
    assert_eq!(snapshot(&pool).await, before);
    let owners_after: Vec<(String, String)> = sqlx::query_as(
        "SELECT datname::text, pg_get_userbyid(datdba)::text
         FROM pg_database WHERE datname <> 'push_messaging' ORDER BY datname"
    ).fetch_all(&pool).await.unwrap();
    assert_eq!(owners_after, owners_before);
    pool.execute("ALTER ROLE push_messaging PASSWORD 'messaging_test_only'")
        .await.unwrap();
    let flags: (bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT rolsuper,rolcreatedb,rolcreaterole,rolreplication,rolbypassrls
         FROM pg_roles WHERE rolname='push_messaging'"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(flags, (false,false,false,false,false));
    pool.close().await;
    let port = root.rsplit(':').next().unwrap();
    let url = format!("postgres://push_messaging:messaging_test_only@127.0.0.1:{port}/push_messaging");
    push_store::migrate_database(&url).await.unwrap();
    let own = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    assert_eq!(snapshot(&own).await, before);
    own.execute("CREATE TABLE messaging.upgrade_permission_probe(id bigint)")
        .await.unwrap();
    own.execute("DROP TABLE messaging.upgrade_permission_probe").await.unwrap();
    let allowed: (bool, bool) = sqlx::query_as(
        "SELECT has_database_privilege(current_user,'push_auth','CONNECT'),
                has_database_privilege(current_user,'push_worker','CONNECT')"
    ).fetch_one(&own).await.unwrap();
    assert_eq!(allowed, (false,false));
    own.close().await;
}

#[tokio::test]
async fn an_incompatible_existing_role_is_refused_without_partial_transfer() {
    let (_container, _root, pool) = legacy().await;
    pool.execute("CREATE ROLE push_messaging LOGIN SUPERUSER").await.unwrap();
    assert!(sqlx::raw_sql(&migration()).execute(&pool).await.is_err());
    // raw_sql stopped inside the migration's explicit transaction.
    pool.execute("ROLLBACK").await.unwrap();
    let owner: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(datdba)::text FROM pg_database WHERE datname=current_database()"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(owner, "push");
    pool.close().await;
}
```

- [ ] **Step 2: Run the new test red.**

```powershell
cargo test -p push-store --test deployment_role --locked
```

Expected: migration file assertion fails. No new dependency is required.

- [ ] **Step 3: Create the complete ownership SQL.**

```sql
BEGIN;
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '60s';
SELECT pg_advisory_xact_lock(1123581321);

DO $upgrade$
DECLARE
    item record;
    target oid;
    old_owner oid;
    role_flags record;
BEGIN
    IF current_database() <> 'push_messaging' THEN
        RAISE EXCEPTION 'run this migration only in push_messaging';
    END IF;
    IF NOT (SELECT rolsuper FROM pg_roles WHERE rolname = current_user) THEN
        RAISE EXCEPTION 'this operator migration requires a database administrator';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_stat_activity
        WHERE datname = current_database() AND pid <> pg_backend_pid()
          AND backend_type = 'client backend'
    ) THEN
        RAISE EXCEPTION 'stop application writers and other database clients before migration';
    END IF;
    SELECT oid INTO old_owner FROM pg_roles WHERE rolname = 'push';
    IF old_owner IS NULL THEN
        RAISE EXCEPTION 'the expected legacy bootstrap role push is missing';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'push_messaging') THEN
        CREATE ROLE push_messaging LOGIN NOSUPERUSER NOCREATEDB
            NOCREATEROLE NOREPLICATION NOBYPASSRLS;
    END IF;
    SELECT * INTO role_flags FROM pg_roles WHERE rolname = 'push_messaging';
    target := role_flags.oid;
    IF role_flags.rolsuper OR role_flags.rolcreatedb OR role_flags.rolcreaterole
       OR role_flags.rolreplication OR role_flags.rolbypassrls
       OR NOT role_flags.rolcanlogin OR role_flags.rolconfig IS NOT NULL THEN
        RAISE EXCEPTION 'push_messaging has incompatible role attributes';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_auth_members WHERE roleid = target OR member = target
    ) THEN
        RAISE EXCEPTION 'push_messaging must not have role memberships or members';
    END IF;
    IF (SELECT count(*) FROM pg_database
        WHERE datname IN ('push_auth','push_worker')) <> 2 THEN
        RAISE EXCEPTION 'the expected auth and worker databases are missing';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_database
        WHERE datname IN ('push_auth','push_worker')
          AND has_database_privilege('push_messaging',datname,'CONNECT')
    ) THEN
        RAISE EXCEPTION 'push_messaging must not have access to auth or worker databases';
    END IF;
    IF (SELECT datdba FROM pg_database WHERE datname = current_database())
       NOT IN (old_owner, target) THEN
        RAISE EXCEPTION 'unexpected messaging database owner';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'messaging') THEN
        RAISE EXCEPTION 'legacy messaging schema is missing';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_namespace
        WHERE nspname = 'messaging' AND nspowner NOT IN (old_owner, target)
    ) OR EXISTS (
        SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE (n.nspname = 'messaging'
               OR (n.nspname = 'public' AND c.relname = '_sqlx_migrations'))
          AND c.relowner NOT IN (old_owner, target)
    ) OR EXISTS (
        SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
        WHERE n.nspname = 'messaging' AND p.proowner NOT IN (old_owner, target)
    ) THEN
        RAISE EXCEPTION 'application objects have an unexpected owner';
    END IF;
    IF (SELECT nspowner FROM pg_namespace WHERE nspname='public')
       NOT IN (old_owner,target,
           (SELECT oid FROM pg_roles WHERE rolname='pg_database_owner')) THEN
        RAISE EXCEPTION 'the public schema has an unexpected owner';
    END IF;
    ALTER DATABASE push_messaging OWNER TO push_messaging;
    ALTER SCHEMA public OWNER TO pg_database_owner;
    ALTER SCHEMA messaging OWNER TO push_messaging;
    FOR item IN
        SELECT n.nspname, c.relname, c.relkind
        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE (n.nspname = 'messaging'
               OR (n.nspname = 'public' AND c.relname = '_sqlx_migrations'))
          AND c.relkind IN ('r','p','S','v','m')
        ORDER BY CASE WHEN c.relkind = 'S' THEN 1 ELSE 0 END, c.oid
    LOOP
        EXECUTE format('ALTER %s %I.%I OWNER TO push_messaging',
            CASE item.relkind WHEN 'S' THEN 'SEQUENCE'
                WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW'
                ELSE 'TABLE' END,
            item.nspname, item.relname);
    END LOOP;
    FOR item IN
        SELECT p.oid::regprocedure AS signature
        FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
        WHERE n.nspname = 'messaging'
    LOOP
        EXECUTE format('ALTER FUNCTION %s OWNER TO push_messaging', item.signature);
    END LOOP;
    REVOKE ALL ON DATABASE push_messaging FROM PUBLIC;
END
$upgrade$;
COMMIT;
```

Do not use `REASSIGN OWNED BY push`: it can also transfer ownership of shared
objects such as other databases. Do not put a password in this SQL. Setting the
new role's password is a separate operator/bootstrap step.

- [ ] **Step 4: Run green and add a live-client refusal case.**

Add to the same test file:

```rust
#[tokio::test]
async fn active_clients_prevent_an_ownership_upgrade() {
    let (_container, root, pool) = legacy().await;
    let client = PgConnection::connect(&format!("{root}/push_messaging")).await.unwrap();
    assert!(sqlx::raw_sql(&migration()).execute(&pool).await.is_err());
    pool.execute("ROLLBACK").await.unwrap();
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='push_messaging')"
    ).fetch_one(&pool).await.unwrap();
    assert!(!exists);
    client.close().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn an_existing_role_with_cross_database_access_is_refused() {
    let (_container, _root, pool) = legacy().await;
    pool.execute("CREATE ROLE push_messaging LOGIN").await.unwrap();
    pool.execute("GRANT CONNECT ON DATABASE push_auth TO push_messaging")
        .await.unwrap();
    assert!(sqlx::raw_sql(&migration()).execute(&pool).await.is_err());
    pool.execute("ROLLBACK").await.unwrap();
    let owner: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(datdba)::text FROM pg_database WHERE datname=current_database()"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(owner, "push");
    pool.close().await;
}
```

Run:

```powershell
cargo test -p push-store --test deployment_role --locked
cargo clippy -p push-store --test deployment_role --locked -- -D warnings
```

Expected: ownership migration is idempotent, data survives byte-for-byte, other
database ownership stays unchanged, incompatible roles and active clients fail
without a partial transfer. Diagnose PostgreSQL errors by SQLSTATE/message,
never by logging credential-bearing URLs.

- [ ] **Step 5: Commit.**

```powershell
git add -- deploy\helm\push-messenger\files\postgres\messaging-role.sql crates\push-store\tests\deployment_role.rs
git commit -m "feat: add transactional data-preserving messaging role migration" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 5: Bootstrap fresh installations with separate messaging credentials

**Files:** `values.yaml`, `secret.yaml`, `postgres.yaml`,
`postgres-init.yaml`, `test-chart.ps1`, `deployment_role.rs`.

- [ ] **Step 1: Add a red chart check.**

```powershell
Check 'message-svc never receives the bootstrap administrator credential' {
    $secret = Doc $default 'Secret' 'push-secrets'
    $postgres = Doc $default 'StatefulSet' 'push-postgres'
    $message = Doc $default 'Deployment' 'push-message-svc'
    $init = Doc $default 'ConfigMap' 'push-postgres-init'
    $secret -match 'messaging-db-password: "[A-Za-z0-9]+"' -and
        $secret -match 'database-url: "postgres://push_messaging:' -and
        $postgres -match 'name: PUSH_MESSAGING_DB_PASSWORD' -and
        $init -match 'CREATE ROLE push_messaging' -and
        $init -match 'ALTER DATABASE push_messaging OWNER TO push_messaging' -and
        $message -notmatch 'key: postgres-password'
}
```

Run `pwsh .\deploy\helm\test-chart.ps1`; expect the new assertion to fail.

- [ ] **Step 2: Add the new chart value and keep the admin password separate.**

Under `postgres`:

```yaml
  # Dedicated non-superuser owner of push_messaging; not the bootstrap password.
  messagingPassword: ""
```

Immediately after loading `$existing` in `secret.yaml`, add:

```gotemplate
{{- if and $existing (not (hasKey $existing "messaging-db-password")) -}}
{{- fail "existing installation needs the data-preserving messaging-role upgrade; see docs/deployment-upgrade.md; do not delete its volume" -}}
{{- end -}}
{{- $messagingPassword := .Values.postgres.messagingPassword | default (get $existing "messaging-db-password" | b64dec) | default (randAlphaNum 32) -}}
{{- if not (regexMatch "^[A-Za-z0-9]+$" $messagingPassword) -}}
{{- fail "postgres.messagingPassword must contain only ASCII letters and digits" -}}
{{- end -}}
```

Emit the new key and replace the messaging connection URL:

```gotemplate
  messaging-db-password: {{ $messagingPassword | quote }}
  database-url: {{ printf "postgres://push_messaging:%s@%s:5432/push_messaging" $messagingPassword $postgresHost | quote }}
```

Keep `postgres-password` and its bootstrap usage unchanged. In `postgres.yaml`,
add next to the existing role-password environment entries:

```yaml
            - name: PUSH_MESSAGING_DB_PASSWORD
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: messaging-db-password
```

Under `data:` in `postgres-init.yaml`, before the existing `10-...` script:

```yaml
  05-push-messaging.sh: |
    psql -X -q -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres <<'SQL'
    \getenv password PUSH_MESSAGING_DB_PASSWORD
    SELECT :'password' ~ '^[A-Za-z0-9]+$' AS valid_password \gset
    \if :valid_password
    SELECT format(
      'CREATE ROLE push_messaging LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD %L',
      :'password'
    ) \gexec
    ALTER DATABASE push_messaging OWNER TO push_messaging;
    \else
    \echo 'messaging database password must contain only ASCII letters and digits'
    \quit 1
    \endif
    SQL
```

The image entrypoint has already created `push_messaging` as bootstrap `push`.
No application schema exists yet: message-svc creates it later as its own role.
Do not run the legacy ownership SQL at fresh-install initialization, because it
requires a migrated legacy schema.

In `10-push-auth.sh` update the comment that currently says message-svc remains
superuser until Plan 11; keep its `REVOKE` statements. Do not change auth/worker
database ownership or outbox grants.

- [ ] **Step 3: Prove the fresh role can run the actual migrations.**

Add to `deployment_role.rs`:

```rust
#[tokio::test]
async fn a_fresh_non_superuser_database_owner_can_run_migrations() {
    let container = Postgres::default().with_tag("17-alpine").start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let root = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let mut admin = PgConnection::connect(&root).await.unwrap();
    admin.execute(
        "CREATE ROLE push_messaging LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE
         NOREPLICATION NOBYPASSRLS PASSWORD 'fresh_test_only'"
    ).await.unwrap();
    admin.execute("CREATE DATABASE push_messaging OWNER push_messaging")
        .await.unwrap();
    admin.execute("REVOKE ALL ON DATABASE push_messaging FROM PUBLIC")
        .await.unwrap();
    admin.close().await.unwrap();
    let url = format!("postgres://push_messaging:fresh_test_only@127.0.0.1:{port}/push_messaging");
    push_store::migrate_database(&url).await.unwrap();
    push_store::migrate_database(&url).await.unwrap();
    let pool = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let table_owner: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(relowner)::text FROM pg_class
         WHERE oid='messaging.message'::regclass"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(table_owner, "push_messaging");
    pool.close().await;
}
```

Add a render guard case:

```powershell
Check 'invalid dedicated messaging passwords are refused' {
    (Render @('--set-string', 'postgres.messagingPassword=bad:password')) -ne 0
}
```

- [ ] **Step 4: Run green and commit.**

```powershell
pwsh .\deploy\helm\test-chart.ps1
cargo test -p push-store --test deployment_role --locked
git add -- deploy\helm crates\push-store\tests\deployment_role.rs
git commit -m "feat: bootstrap message service as a non-superuser database owner" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Expected: a fresh chart creates separate credentials; an existing legacy Secret
is explicitly refused until prepared. Runtime fresh-install proof follows in
Task 7, not just in these render checks.

## Task 6: Add release-scoped ingress NetworkPolicies

**Files:** `_helpers.tpl`, new `networkpolicy.yaml`, `values.yaml`,
`values-minikube.yaml`, `test-chart.ps1`.

- [ ] **Step 1: Add the policy render regression.**

```powershell
Check 'internal APIs have per-caller NetworkPolicy ports' {
    $policy = Doc $default 'NetworkPolicy' 'push-message-svc'
    $policy -match 'port: 50051' -and $policy -match 'port: 50052' -and
        $policy -match 'port: 50053' -and
        $policy -match 'push-messenger/network-peer: gateway' -and
        $policy -match 'push-messenger/network-peer: worker' -and
        $policy -match 'push-messenger/network-peer: auth' -and
        $policy -match 'app.kubernetes.io/instance: push'
}
Check 'NetworkPolicies have an explicit opt-out' {
    $off = helm template push $chart --set networkPolicy.enabled=false | Out-String
    $LASTEXITCODE -eq 0 -and $off -notmatch '(?m)^kind: NetworkPolicy$'
}
```

Run `pwsh .\deploy\helm\test-chart.ps1`; expect the first assertion to fail.

- [ ] **Step 2: Define explicit policy configuration.**

Append to `values.yaml`:

```yaml
networkPolicy:
  enabled: true
  # Empty denies public ingress until a consumer explicitly selects its controller.
  # Both namespaceSelector and podSelector must be non-empty for each peer.
  ingressControllerPeers: []
```

Append to `values-minikube.yaml`:

```yaml
networkPolicy:
  ingressControllerPeers:
    - namespaceSelector:
        kubernetes.io/metadata.name: ingress-nginx
      podSelector:
        app.kubernetes.io/name: ingress-nginx
```

Add a label to `push.labels`, not to `push.selector`:

```gotemplate
push-messenger/network-peer: {{ .component }}
```

Every existing Pod template already includes `push.labels`. The new label is
not a Service selector. Test probe pods can carry a caller identity without
becoming Service endpoints.

- [ ] **Step 3: Create the complete policy template.**

```gotemplate
{{- if .Values.networkPolicy.enabled -}}
{{- $root := . -}}
{{- $controllers := .Values.networkPolicy.ingressControllerPeers -}}
{{- range $controllers -}}
{{- if or (empty .namespaceSelector) (empty .podSelector) -}}
{{- fail "each networkPolicy.ingressControllerPeers entry needs non-empty namespaceSelector and podSelector" -}}
{{- end -}}
{{- end -}}
{{- $targets := list
    (dict "name" "message-svc" "rules" (list
        (dict "port" 50051 "peers" (list "gateway"))
        (dict "port" 50052 "peers" (list "worker"))
        (dict "port" 50053 "peers" (list "auth"))))
    (dict "name" "postgres" "rules" (list
        (dict "port" 5432 "peers" (list "message-svc" "auth" "worker"))))
    (dict "name" "redis" "rules" (list
        (dict "port" 6379 "peers" (list "message-svc" "auth" "gateway"))))
    (dict "name" "auth" "public" true "rules" (list
        (dict "port" 8080 "peers" (list "gateway"))))
    (dict "name" "gateway" "public" true "rules" (list))
    (dict "name" "worker" "public" (not (empty (include "push.hasWebhook" .))) "rules" (list))
-}}
{{- range $target := $targets }}
{{- $public := and $target.public (not (empty $controllers)) -}}
{{- $mailbox := and (eq $target.name "worker") (not (empty (include "push.devMode" $root))) -}}
{{ printf "\n---\n" -}}
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: {{ include "push.name" (dict "root" $root "component" $target.name) }}
  labels:
    {{- include "push.labels" (dict "root" $root "component" $target.name) | nindent 4 }}
spec:
  podSelector:
    matchLabels:
      {{- include "push.selector" (dict "root" $root "component" $target.name) | nindent 6 }}
  policyTypes: [Ingress]
  ingress:{{ if not (or (gt (len $target.rules) 0) $public $mailbox) }} []{{ end }}
    {{- range $rule := $target.rules }}
    - from:
        {{- range $peer := $rule.peers }}
        - podSelector:
            matchLabels:
              app.kubernetes.io/instance: {{ $root.Release.Name }}
              push-messenger/network-peer: {{ $peer }}
        {{- end }}
      ports:
        - protocol: TCP
          port: {{ $rule.port }}
    {{- end }}
    {{- if $public }}
    - from:
        {{- range $peer := $controllers }}
        - namespaceSelector:
            matchLabels:
              {{- toYaml $peer.namespaceSelector | nindent 14 }}
          podSelector:
            matchLabels:
              {{- toYaml $peer.podSelector | nindent 14 }}
        {{- end }}
      ports:
        - protocol: TCP
          port: 8080
    {{- end }}
    {{- if $mailbox }}
    - from:
        - podSelector:
            matchLabels:
              app.kubernetes.io/instance: {{ $root.Release.Name }}
              push-messenger/network-peer: smoke
      ports:
        - protocol: TCP
          port: 8080
    {{- end }}
{{- end }}
{{- end }}
```

For a target with no allowed peers, emit `ingress: []`, never an empty rule
`{}`, which would allow everything. The render checks must
assert the default gateway has no broad allow rule and the minikube gateway
has only the configured controller peer.

There is no namespace-wide default-deny resource and no egress policy. Health
probes from the node continue to work under the standard NetworkPolicy model.
The worker does not receive Redis access.

- [ ] **Step 4: Test rejected selectors and cross-release scope in rendered output.**

```powershell
Check 'empty controller selectors cannot create a wildcard allow' {
    (Render @('--set', 'networkPolicy.ingressControllerPeers[0].podSelector.app=x')) -ne 0
}
Check 'a second release has distinct policy scope' {
    $other = helm template other $chart | Out-String
    $LASTEXITCODE -eq 0 -and
        (Doc $other 'NetworkPolicy' 'other-message-svc') -match
        'app.kubernetes.io/instance: other'
}
```

Run:

```powershell
pwsh .\deploy\helm\test-chart.ps1
helm template push .\deploy\helm\push-messenger -f .\deploy\helm\push-messenger\values-minikube.yaml
```

Read the six policies once to confirm each intended source/port pair, especially
the AND-combined namespace/pod controller selector. A rendered policy alone does
not satisfy the acceptance test; Task 7 is mandatory.

- [ ] **Step 5: Commit.**

```powershell
git add -- deploy\helm
git commit -m "feat: enforce release-scoped internal API network boundaries" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 7: Prove runtime policy enforcement on a dedicated test cluster

**Files:** new `deploy\minikube\test-network-policy.ps1`.

- [ ] **Step 1: Create the executable runtime test before running it.**

The probe uses its own names, has no Service-selection labels, mounts no
credentials, and tests actual sockets. A timeout is a deny result; DNS failure,
connection refusal, Python failure and kubectl failure are infrastructure errors,
not successful denial tests.

```powershell
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $Context,
    [string] $Namespace = 'push',
    [string] $Release = 'push',
    [string] $ProbeImage = 'python:3.13-alpine'
)
$ErrorActionPreference = 'Stop'
$created = [System.Collections.Generic.List[string]]::new()
$suffix = [Guid]::NewGuid().ToString('N').Substring(0, 10)
$cleanupFailed = $false

function Kube([string[]] $Arguments) {
    $output = & kubectl --context $Context -n $Namespace @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "kubectl failed (exit code $LASTEXITCODE)"
    }
    return $output
}

$probe = @'
import errno
import socket
import sys

host, port, expected = sys.argv[1], int(sys.argv[2]), sys.argv[3]
try:
    connection = socket.create_connection((host, port), timeout=2)
except TimeoutError:
    sys.exit(0 if expected == "deny" else 3)
except OSError as error:
    if error.errno == errno.ETIMEDOUT:
        sys.exit(0 if expected == "deny" else 3)
    print("probe infrastructure error:", type(error).__name__, error.errno)
    sys.exit(4)
else:
    connection.close()
    sys.exit(0 if expected == "allow" else 5)
'@

function New-Probe([string] $Peer, [string] $Instance) {
    $name = "netcheck-$Peer-$suffix"
    $pod = @{
        apiVersion = 'v1'; kind = 'Pod'
        metadata = @{
            name = $name
            labels = @{
                'app.kubernetes.io/instance' = $Instance
                'push-messenger/network-peer' = $Peer
            }
        }
        spec = @{
            automountServiceAccountToken = $false
            restartPolicy = 'Never'
            activeDeadlineSeconds = 600
            securityContext = @{
                runAsNonRoot = $true; runAsUser = 65532; runAsGroup = 65532
                seccompProfile = @{type = 'RuntimeDefault'}
            }
            containers = @(@{
                name = 'probe'; image = $ProbeImage
                command = @('python', '-c', 'import time; time.sleep(540)')
                securityContext = @{
                    allowPrivilegeEscalation = $false
                    readOnlyRootFilesystem = $true
                    capabilities = @{drop = @('ALL')}
                }
                resources = @{
                    requests = @{cpu = '10m'; memory = '24Mi'}
                    limits = @{cpu = '100m'; memory = '64Mi'}
                }
            })
        }
    }
    $pod | ConvertTo-Json -Depth 20 |
        & kubectl --context $Context -n $Namespace apply -f - | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "creating probe $name failed" }
    $created.Add($name)
    Kube @('wait', '--for=condition=Ready', "pod/$name", '--timeout=120s') | Out-Null
    return $name
}

function Assert-Connection(
    [string] $Pod, [string] $HostAddress, [int] $Port, [string] $Expected
) {
    & kubectl --context $Context -n $Namespace exec $Pod -- `
        python -c $probe $HostAddress "$Port" $Expected
    if ($LASTEXITCODE -ne 0) {
        throw "$Pod -> ${HostAddress}:$Port expected $Expected; probe exit $LASTEXITCODE"
    }
    Write-Output "ok $Pod -> ${HostAddress}:$Port $Expected"
}

try {
    $service = (Kube @('get', 'service', "$Release-message-svc", '-o', 'json') |
        Out-String | ConvertFrom-Json)
    $hostAddress = [string] $service.spec.clusterIP
    if (-not $hostAddress -or $hostAddress -eq 'None') {
        throw 'message-svc needs a concrete Service IP for this test'
    }
    $sources = @{}
    foreach ($peer in 'gateway', 'worker', 'auth', 'unrelated') {
        $sources[$peer] = New-Probe $peer $Release
    }
    $sources['foreign'] = New-Probe 'foreign' "$Release-other"
    # foreign carries the correct gateway identity, but the wrong release.
    Kube @('label', "pod/$($sources['foreign'])",
        'push-messenger/network-peer=gateway', '--overwrite') | Out-Null

    $ports = @{gateway = 50051; worker = 50052; auth = 50053}
    # Establish every target is alive before accepting any denied connection.
    foreach ($peer in 'gateway', 'worker', 'auth') {
        Assert-Connection $sources[$peer] $hostAddress $ports[$peer] 'allow'
    }
    foreach ($peer in 'gateway', 'worker', 'auth', 'unrelated', 'foreign') {
        foreach ($port in 50051, 50052, 50053) {
            $expected = if ($ports.ContainsKey($peer) -and $ports[$peer] -eq $port) {
                'allow'
            } else { 'deny' }
            Assert-Connection $sources[$peer] $hostAddress $port $expected
        }
    }
} finally {
    foreach ($name in $created) {
        & kubectl --context $Context -n $Namespace delete pod $name `
            --ignore-not-found --wait=true --timeout=30s | Out-Null
        if ($LASTEXITCODE -ne 0) {
            $cleanupFailed = $true
            Write-Warning "probe cleanup failed: $name"
        }
    }
}
if ($cleanupFailed) { throw 'network test passed but probe cleanup failed' }
```

Cleanup preserves a primary failure and fails an otherwise successful run if
probe deletion fails. There is no background process to leave running and no
cluster/namespace deletion in this script.

- [ ] **Step 2: Start a separate policy-enforcing test profile.**

Do not change the shared `push` profile's CNI. Inspect the named test profile
first and stop for owner guidance if it contains unrelated workloads.

```powershell
minikube start -p push11a --driver=docker --cni=calico --cpus=4 --memory=6g
.\deploy\minikube\up.ps1 -Cluster push11a -Smoke
```

Expected: fresh roles bootstrap correctly; all original smoke flows pass.
If the CNI images cannot be pulled or the machine cannot run the profile,
report the blocked prerequisite; do not claim isolation from YAML.

- [ ] **Step 3: Run red against disabled policies on this test profile only.**

```powershell
helm upgrade push .\deploy\helm\push-messenger --kube-context push11a `
    -n push --reuse-values --set networkPolicy.enabled=false --wait --timeout 5m
.\deploy\minikube\test-network-policy.ps1 -Context push11a
```

Expected red: a wrong caller reaches a forbidden API port (probe exit 5).
An image pull or kubectl failure is not the intended red.

- [ ] **Step 4: Enable policies and run the exact same test green.**

```powershell
helm upgrade push .\deploy\helm\push-messenger --kube-context push11a `
    -n push --reuse-values --set networkPolicy.enabled=true --wait --timeout 5m
.\deploy\minikube\test-network-policy.ps1 -Context push11a
helm test push --kube-context push11a -n push --logs --timeout 6m
```

Expected: all three correct callers reach their own ports; wrong APIs, unrelated
pods and other releases time out; functional smoke still passes.
An unenforced CNI makes the deny matrix fail instead of appearing successful.

- [ ] **Step 5: Commit the reusable runtime test.**

```powershell
git add -- deploy\minikube\test-network-policy.ps1
git commit -m "test: verify internal API NetworkPolicy enforcement at runtime" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 8: Document and guard the data-preserving installation upgrade

**Files:** new `docs\deployment-upgrade.md`, `up.ps1`, `up.sh`,
README/configuration comments and the roadmap.

- [ ] **Step 1: Add the upgrade runbook with these executable phases.**

The runbook is for the current bundled-Postgres layout: bootstrap role `push`,
database `push_messaging`, release `push`, namespace `push`. Consumers substitute
their explicit context/release/namespace; no script relies on kubectl's current
context. It must not automatically run maintenance on a consumer's installation.

Start with explicit operator-selected parameters and a private backup directory:

```powershell
$Context = Read-Host 'Kubernetes context to upgrade'
$Namespace = Read-Host 'Namespace'
$Release = Read-Host 'Helm release'
$BackupDirectory = Read-Host 'Existing encrypted, operator-only backup directory outside the repository'
if (-not (Test-Path -LiteralPath $BackupDirectory -PathType Container)) {
    throw 'prepare the private backup directory before continuing'
}
$PostgresPod = "$Release-postgres-0"
$SecretName = "$Release-secrets"
$replicas = @{}
foreach ($component in 'gateway', 'message-svc', 'auth', 'worker') {
    $name = "$Release-$component"
    $raw = kubectl --context $Context -n $Namespace get deployment $name -o json
    if ($LASTEXITCODE -ne 0) { throw "cannot inspect $name" }
    $replicas[$name] = ($raw | ConvertFrom-Json).spec.replicas
}
$replicas | ConvertTo-Json |
    Set-Content -LiteralPath (Join-Path $BackupDirectory 'replicas.json') -Encoding utf8NoBOM
```

Record the current image identifiers and Helm revision in that same private
directory:

```powershell
kubectl --context $Context -n $Namespace get pods `
    -l "app.kubernetes.io/instance=$Release" -o json |
    Set-Content -LiteralPath (Join-Path $BackupDirectory 'pods-before.json') -Encoding utf8NoBOM
if ($LASTEXITCODE -ne 0) { throw 'could not record deployment images' }
helm history $Release --kube-context $Context -n $Namespace -o json |
    Set-Content -LiteralPath (Join-Path $BackupDirectory 'helm-history.json') -Encoding utf8NoBOM
if ($LASTEXITCODE -ne 0) { throw 'could not record Helm history' }
```

The pod status records pulled image IDs as well as requested tags. Do not dump
secret-bearing Helm values to terminal logs. Warn that Helm's release data can
itself contain sensitive values. The consumer must suspend external routing or
its deployment-specific maintenance gate for this application before starting
the replica transition, and keep it suspended until verification completes.

The maintenance phase must be an operator decision:

```powershell
foreach ($name in $replicas.Keys) {
    kubectl --context $Context -n $Namespace scale deployment $name --replicas=0
    if ($LASTEXITCODE -ne 0) { throw "could not stop $name" }
}
kubectl --context $Context -n $Namespace wait --for=delete pod `
    -l "app.kubernetes.io/instance=$Release,app.kubernetes.io/name in (gateway,message-svc,auth,worker)" `
    --timeout=180s
if ($LASTEXITCODE -ne 0) { throw 'application pods did not stop; do not migrate' }
```

Take a consistent backup while all application writers are stopped:

```powershell
$dump = Join-Path $BackupDirectory 'before-11a.sql'
kubectl --context $Context -n $Namespace exec $PostgresPod -- `
    pg_dumpall -U push |
    Set-Content -LiteralPath $dump -Encoding utf8NoBOM
if ($LASTEXITCODE -ne 0) { throw 'backup failed; do not migrate' }
if ((Get-Item -LiteralPath $dump).Length -eq 0) {
    throw 'backup is empty; do not migrate'
}
```

The backup contains message bodies, account data and password/token material;
require encryption, private access and a successful restore rehearsal in an
isolated Postgres instance before ownership changes. File existence alone is
not a restore verification. A consumer with an established backup tool can
substitute it, but must document and verify that restore checkpoint.

For the bundled-layout rehearsal, use a unique administrator name so restoring
the source roles does not collide with a scratch server's bootstrap user. The
scratch server publishes no ports and has no network:

```powershell
$restoreName = 'push11a-restore-' + [Guid]::NewGuid().ToString('N').Substring(0, 10)
$restoreUser = 'restore_' + [Guid]::NewGuid().ToString('N').Substring(0, 10)
try {
    docker run -d --name $restoreName --network none `
        -e POSTGRES_HOST_AUTH_METHOD=trust "-e" "POSTGRES_USER=$restoreUser" `
        -e POSTGRES_DB=restore_control postgres:17-alpine | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'could not start isolated restore fixture' }
    $ready = $false
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        docker exec $restoreName pg_isready -U $restoreUser -d restore_control 2>&1 |
            Out-Null
        if ($LASTEXITCODE -eq 0) { $ready = $true; break }
        Start-Sleep -Seconds 1
    }
    if (-not $ready) { throw 'restore fixture did not become ready' }
    Get-Content -Raw -LiteralPath $dump |
        docker exec -i $restoreName psql -X -q -v ON_ERROR_STOP=1 `
            -U $restoreUser -d restore_control | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'backup restore failed; do not migrate' }
    foreach ($database in 'push_messaging','push_auth','push_worker') {
        docker exec $restoreName psql -X -qAt -v ON_ERROR_STOP=1 `
            -U $restoreUser -d $database -c 'SELECT 1' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "restored database $database is unavailable" }
    }
    docker exec $restoreName psql -X -qAt -v ON_ERROR_STOP=1 `
        -U $restoreUser -d push_messaging `
        -c 'SELECT count(*) FROM messaging.message' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'restored message history is unavailable' }
} finally {
    docker rm -f $restoreName | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "restore fixture cleanup failed: $restoreName"
    }
}
```

The fixture's trust authentication is safe only because it is an isolated,
short-lived restore check with no network or published ports. Never copy it to
an application deployment. The full dump must restore without SQL errors and
the three restored databases must be readable. Source row snapshots in the next
step separately prove that the ownership migration itself preserves data.

For the upgrade rehearsal, capture canonical row snapshots of all application
tables while writers remain stopped:

```powershell
function Save-DatabaseSnapshots([string] $Phase) {
    $queries = @'
SELECT format(
    'SELECT %L || row_to_json(t)::text FROM %I.%I t ORDER BY row_to_json(t)::text',
    n.nspname || '.' || c.relname || ' ', n.nspname, c.relname)
FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
WHERE c.relkind IN ('r','p')
  AND n.nspname IN ('messaging','auth','worker','public')
ORDER BY n.nspname,c.relname
\gexec
'@
    foreach ($database in 'push_messaging','push_auth','push_worker') {
        $path = Join-Path $BackupDirectory "$Phase-$database.rows"
        $queries | kubectl --context $Context -n $Namespace exec -i $PostgresPod -- `
            psql -X -qAt -v ON_ERROR_STOP=1 -U push -d $database |
            Set-Content -LiteralPath $path -Encoding utf8NoBOM
        if ($LASTEXITCODE -ne 0) { throw "snapshot failed for $database" }
    }
}
Save-DatabaseSnapshots 'before'
```

These files are private data, not CI artifacts to publish. Empty tables are
verified by the database schema/permission checks and by the full backup, not
represented as fabricated data rows.

Apply the exact version-controlled ownership SQL:

```powershell
Get-Content -Raw -LiteralPath `
    .\deploy\helm\push-messenger\files\postgres\messaging-role.sql |
    kubectl --context $Context -n $Namespace exec -i $PostgresPod -- `
        psql -X -v ON_ERROR_STOP=1 -U push -d push_messaging
if ($LASTEXITCODE -ne 0) { throw 'ownership migration failed; keep maintenance mode' }
```

Before restarting any application, verify unchanged rows:

```powershell
Save-DatabaseSnapshots 'after'
foreach ($database in 'push_messaging','push_auth','push_worker') {
    $before = Get-FileHash -Algorithm SHA256 -LiteralPath `
        (Join-Path $BackupDirectory "before-$database.rows")
    $after = Get-FileHash -Algorithm SHA256 -LiteralPath `
        (Join-Path $BackupDirectory "after-$database.rows")
    if ($before.Hash -ne $after.Hash) {
        throw "$database snapshot changed; keep maintenance mode and inspect private snapshots"
    }
}
```

Do not print differing rows. Ownership changes are intentionally absent from
row snapshots and are checked separately. Compare pending outbox/queue rows
before workers restart: processing them after the upgrade is legitimate work,
not evidence that the migration deleted them.

Set the dedicated role's password through PostgreSQL's interactive password
command; do not place the password in process arguments, a shell history or SQL
checked into the repository:

```powershell
kubectl --context $Context -n $Namespace exec -it $PostgresPod -- `
    psql -X -v ON_ERROR_STOP=1 -U push -d push_messaging `
    -c '\password push_messaging'
if ($LASTEXITCODE -ne 0) { throw 'password provisioning failed; keep maintenance mode' }
```

Use an alphanumeric password because the bundled connection URLs embed it.
Consumers may use their secure credential-provisioning mechanism instead.

For the current chart-managed Secret, capture that same password securely and
send only the changed keys as a stdin JSON merge patch. Never print the patch
or export all Secret values. This does not add the 11B external-Secret feature:

```powershell
$secure = Read-Host 'The messaging password just provisioned' -AsSecureString
$credential = [System.Net.NetworkCredential]::new('', $secure)
$password = $credential.Password
try {
    if ($password -cnotmatch '^[A-Za-z0-9]+$') {
        throw 'the bundled chart requires an ASCII alphanumeric password'
    }
    $url = "postgres://push_messaging:${password}@$Release-postgres`:5432/push_messaging"
    $patch = @{
        stringData = @{
            'messaging-db-password' = $password
            'database-url' = $url
        }
    } | ConvertTo-Json -Depth 4
    $patch | kubectl --context $Context -n $Namespace patch secret $SecretName `
        --type=merge --patch-file=-
    if ($LASTEXITCODE -ne 0) { throw 'Secret transition failed; keep maintenance mode' }
} finally {
    $password = $null
    $url = $null
    $patch = $null
    $credential = $null
    $secure.Dispose()
}
```

Before accepting the task, exercise
this command on the disposable test Secret with the installed kubectl version;
if the CLI lacks that interface, stop and select a supported protected-file
input mechanism rather than putting credentials in command arguments.

Deploy the new chart and image set together through the consumer's deployment
process. Do not call Helm rollback and assume it undoes the ownership migration.
Keep public traffic disabled until service readiness and data checks pass.
Restore the recorded replica counts explicitly if the consumer chart values
do not already supply them:

```powershell
foreach ($name in $replicas.Keys) {
    kubectl --context $Context -n $Namespace scale deployment $name `
        "--replicas=$($replicas[$name])"
    if ($LASTEXITCODE -ne 0) { throw "could not restore replicas for $name" }
}
```

Verify users/devices, contacts, original message bodies/IDs/sequence positions,
pending outbox state and new sends through the ordinary API. Local DEV ONLY
smoke can be used on test installations, not as a production check requiring
signing-key/mailbox access.

Rollback checkpoints must be explicit:

- Before ownership migration: restore replica counts and retain old versions.
- After migration but before any new writes: either complete the coordinated
  upgrade or restore the rehearsed backup and matching old Secret/chart/images.
- After new writes: do not restore an old backup without an explicit data-loss
  decision. Keep maintenance mode and perform an operator-led recovery.

No step deletes a PVC, database or namespace. New credential provisioning is
separate from ownership transfer so failed/retried SQL cannot rotate passwords.

- [ ] **Step 2: Make both local bring-up scripts refuse an unprepared upgrade.**

Change their existing required-Secret-key loops to include
`messaging-db-password`. Replace the old remediation text that recommends
deleting the cluster with:

```text
This installation needs an explicit database/credential upgrade.
Keep its volume and Secret. Back up and follow docs/deployment-upgrade.md
before rerunning bring-up; no automatic destructive repair is performed.
```

Within the existing Postgres preflight block, after the two service databases
are checked, add the following query:

```sql
SELECT count(*)
FROM pg_roles r JOIN pg_database d ON d.datdba = r.oid
WHERE r.rolname = 'push_messaging' AND d.datname = 'push_messaging'
  AND r.rolcanlogin AND NOT r.rolsuper AND NOT r.rolcreatedb
  AND NOT r.rolcreaterole AND NOT r.rolreplication AND NOT r.rolbypassrls;
```

PowerShell execution and refusal:

```powershell
$roleQuery = @'
SELECT count(*)
FROM pg_roles r JOIN pg_database d ON d.datdba = r.oid
WHERE r.rolname = 'push_messaging' AND d.datname = 'push_messaging'
  AND r.rolcanlogin AND NOT r.rolsuper AND NOT r.rolcreatedb
  AND NOT r.rolcreaterole AND NOT r.rolreplication AND NOT r.rolbypassrls
'@
$role = kubectl --context $Cluster -n $namespace exec push-postgres-0 -- `
    psql -X -v ON_ERROR_STOP=1 -U push -d postgres -Atc $roleQuery
if ($LASTEXITCODE -ne 0 -or "$role".Trim() -ne '1') {
    throw 'The messaging role is not prepared; follow docs/deployment-upgrade.md without deleting data.'
}
```

Bash execution and refusal, inside its corresponding existing preflight block:

```bash
role_query="SELECT count(*) FROM pg_roles r JOIN pg_database d ON d.datdba=r.oid WHERE r.rolname='push_messaging' AND d.datname='push_messaging' AND r.rolcanlogin AND NOT r.rolsuper AND NOT r.rolcreatedb AND NOT r.rolcreaterole AND NOT r.rolreplication AND NOT r.rolbypassrls"
role="$(kubectl --context "$cluster" -n "$namespace" exec push-postgres-0 -- \
  psql -X -v ON_ERROR_STOP=1 -U push -d postgres -Atc "$role_query")"
if [[ "$role" != 1 ]]; then
  echo 'The messaging role is not prepared; follow docs/deployment-upgrade.md without deleting data.' >&2
  exit 1
fi
```

Do not silently generate the missing Secret key during this preflight.

- [ ] **Step 3: Rehearse an actual old-chart upgrade, not just fresh installation.**

Use a second explicitly disposable profile, `push11a-upgrade`. Keep the
`push11a` enforcement profile intact until its results are recorded.

Export the old source without checking out another branch or creating a worktree:

```powershell
$fixture = Join-Path $env:TEMP ("push11a-upgrade-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixture | Out-Null
$archive = Join-Path $fixture 'legacy.tar'
git archive --format=tar "--output=$archive" bf93bbd
if ($LASTEXITCODE -ne 0) { throw 'could not export the legacy fixture' }
tar -xf $archive -C $fixture
if ($LASTEXITCODE -ne 0) { throw 'could not extract the legacy fixture' }
minikube start -p push11a-upgrade --driver=docker --cni=calico --cpus=4 --memory=6g
& (Join-Path $fixture 'deploy\minikube\up.ps1') -Cluster push11a-upgrade -Smoke
if ($LASTEXITCODE -ne 0) { throw 'legacy smoke failed' }
```

The old bring-up runs from the exported source and builds its own old images.
Use the runbook to capture a backup and table snapshots from **all three**
databases before the migration, including auth outbox and worker queue. Preserve
the original records' values, not just their counts.

Run the new bring-up first and expect a refusal for the missing messaging key;
verify it has not deleted a volume or changed the Secret. Perform the runbook's
maintenance/migration/credential steps. On this disposable fixture, use a
synthetic test password, never real deployment credentials.

Then build/deploy the current code:

```powershell
.\deploy\minikube\up.ps1 -Cluster push11a-upgrade -Smoke
.\deploy\minikube\test-network-policy.ps1 -Context push11a-upgrade
```

Before starting the new applications, compare all three snapshots using the
runbook commands and verify ownership/non-superuser flags. After smoke, verify
the original conversation history again; new smoke records may be additional
rows and resumed workers may legitimately settle pending work. Confirm old
auth/worker database owners and worker-specific outbox grants have not changed.
Re-run the ownership script during a second maintenance window to prove its
idempotence, restore replicas and verify the installation still serves requests.

If local resource limits prevent two profiles at once, finish and record the
first profile's checks, then stop only that named profile before starting the
second. Never delete the shared `push` profile.

- [ ] **Step 4: Update the consumer-facing configuration documentation.**

In README and the config rustdoc tables, document:

```text
PUSH_GRPC_ADDR             Messaging  0.0.0.0:50051
PUSH_ACCOUNTS_GRPC_ADDR    Accounts   0.0.0.0:50052
PUSH_DIRECTORY_GRPC_ADDR   Directory  0.0.0.0:50053
```

Update the worker's Accounts URL examples to port 50052 and auth-svc's
Directory examples to 50053 in:

- `crates\push-worker\src\accounts.rs` and `src\config.rs` rustdoc;
- `crates\push-auth\src\directory.rs` and `src\config.rs` rustdoc;
- `README.md` configuration tables.

Do not change arbitrary URL parser tests that use port 50051 as test input:
those parsers accept any valid port and are not wiring tests. Document
`networkPolicy.enabled`, explicit controller selectors, CNI enforcement,
same-release caller scope and the existing-Secret upgrade requirement.
Link `docs\deployment-upgrade.md` from the README.

Update the main design's current deployment notes to describe the implemented
listener/database boundaries; keep historical plan code samples unchanged.
Set the roadmap's 11A status to implemented/under review, not Done, until its
PR is actually merged. Do not mark 11B or 11C complete.

- [ ] **Step 5: Final gates and commit.**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
pwsh .\deploy\helm\test-chart.ps1
cargo +1.85 build --workspace --all-targets --locked
git diff --check
git add -- deploy\minikube\up.ps1 deploy\minikube\up.sh docs README.md crates\push-auth\src\config.rs crates\push-auth\src\directory.rs crates\push-worker\src\config.rs crates\push-worker\src\accounts.rs
git commit -m "docs: document and guard data-preserving deployment upgrades" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Expected: all Rust/format/lint/chart/MSRV gates pass, both runtime cluster
acceptance paths have recorded successes, no deployment credentials appear in
tracked files, and the only scaffolding added is reusable verification and
upgrade support. If the 1.85 toolchain is unavailable, report that prerequisite;
do not silently replace its check with a newer compiler.

## Completion checklist

- [ ] Three API listeners register only their own services.
- [ ] Existing token checks and domain scenarios remain green.
- [ ] Startup bind failure releases all listeners; shutdown waits for all servers.
- [ ] The chart/callers use the new ports without changing public `/v1` URLs.
- [ ] Enforced policies accept intended callers and deny wrong/unrelated releases.
- [ ] Fresh installs migrate as a non-superuser messaging owner.
- [ ] A real legacy installation upgrades without deleting or changing original data.
- [ ] SQL failures/role mismatches/live clients stop without partial transfer.
- [ ] Backup restoration and rollback checkpoints are documented and rehearsed.
- [ ] Missing upgrade prerequisites are explicit, never repaired by deletion.
- [ ] No GitHub/registry/cloud deployment dependency has been introduced.
- [ ] 11B and 11C remain separate work.

After review and integration, update the roadmap with the actual PR reference.
Do not provision Azure or start the next plan automatically.
