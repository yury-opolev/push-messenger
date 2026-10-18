//! The whole stack with Entra ID tokens over TLS (spec 2026-10-18 AC2 to AC7): a
//! migration identity owns the databases and runs each service's `migrate`; the services
//! run as granted principals with tokens that live 6 s; the smoke test, a 45-second
//! push-load run and forced new database connections all pass; no token reaches a log.
//! Plus the start-up refusals that must stop at once instead of retrying.
//!
//! Run with `--test-threads=1`: the first test owns the global log capture.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Datastores, Stack, lookup};
use futures_util::future::BoxFuture;
use push_credentials::testing::{
    IssueHook, Issued, StubIssuer, StubPrincipal, TestCa, TlsPostgres, TlsRedis,
    set_postgres_password, set_redis_password, tls_postgres, tls_redis,
};
use tokio_util::sync::CancellationToken;

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const ASSERTION: &str = "federated-service-account-token";
/// (client id, PostgreSQL role, Redis object id) of each identity.
const PRINCIPALS: [(&str, &str, &str); 5] = [
    (
        "migrator",
        "push-migrator-id",
        "00000000-0000-4000-8000-000000000000",
    ),
    (
        "auth",
        "push-auth-id",
        "00000000-0000-4000-8000-000000000001",
    ),
    (
        "message-svc",
        "push-message-svc-id",
        "00000000-0000-4000-8000-000000000002",
    ),
    (
        "worker",
        "push-worker-id",
        "00000000-0000-4000-8000-000000000003",
    ),
    (
        "gateway",
        "push-gateway-id",
        "00000000-0000-4000-8000-000000000004",
    ),
];

static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        LOG.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Azure {
    ca: TestCa,
    postgres: TlsPostgres,
    redis: TlsRedis,
    admin: sqlx::PgPool,
    stub: StubIssuer,
    token_file: std::path::PathBuf,
}

/// TLS datastores, the stub issuer (whose tokens become passwords), the roles and the
/// databases owned by the migration identity.
async fn azure(lifetime: Duration) -> Azure {
    push_credentials::install_crypto_provider();
    let ca = TestCa::new();
    let postgres = tls_postgres(&ca).await;
    let redis = tls_redis(&ca).await;
    let admin = postgres.admin(&ca).await;
    // One at a time: CREATE DATABASE refuses the implicit transaction of a batch.
    let mut statements = Vec::new();
    for (_, role, _) in PRINCIPALS {
        statements.push(format!("CREATE ROLE \"{role}\" LOGIN"));
    }
    for database in ["push_auth", "push_messaging", "push_worker"] {
        statements.push(format!(
            "CREATE DATABASE {database} OWNER \"push-migrator-id\""
        ));
    }
    for statement in statements {
        sqlx::raw_sql(&statement).execute(&admin).await.unwrap();
    }
    let redis_admin = redis.admin(&ca).await;
    let hook: IssueHook = {
        let admin = admin.clone();
        Arc::new(move |issued: Issued| {
            let admin = admin.clone();
            let mut redis_admin = redis_admin.clone();
            Box::pin(async move {
                let role = PRINCIPALS
                    .iter()
                    .find(|(client, _, _)| *client == issued.client_id)
                    .unwrap()
                    .1;
                if issued.scope.starts_with("https://ossrdbms-aad") {
                    set_postgres_password(&admin, role, &issued.token).await;
                } else {
                    set_redis_password(&mut redis_admin, &issued.object_id, &issued.token).await;
                }
            }) as BoxFuture<'static, ()>
        })
    };
    let principals = PRINCIPALS
        .iter()
        .map(|(client, _, oid)| StubPrincipal {
            client_id: (*client).into(),
            object_id: (*oid).into(),
        })
        .collect();
    let stub = StubIssuer::start(TENANT, ASSERTION, principals, lifetime, Some(hook)).await;
    let token_file = std::env::temp_dir().join(format!("push-entra-{}.jwt", uuid::Uuid::now_v7()));
    std::fs::write(&token_file, ASSERTION).unwrap();
    Azure {
        ca,
        postgres,
        redis,
        admin,
        stub,
        token_file,
    }
}

impl Azure {
    fn identity(&self, client: &str) -> Vec<(String, String)> {
        vec![
            ("AZURE_CLIENT_ID".into(), client.into()),
            ("AZURE_TENANT_ID".into(), TENANT.into()),
            (
                "AZURE_FEDERATED_TOKEN_FILE".into(),
                self.token_file.display().to_string(),
            ),
            ("AZURE_AUTHORITY_HOST".into(), self.stub.authority().into()),
        ]
    }

    fn postgres(&self, client: &str, role: &str) -> Vec<(String, String)> {
        let mut variables = self.identity(client);
        variables.extend([
            ("PUSH_POSTGRES_AUTH".into(), "azure-entra".into()),
            ("PUSH_DATABASE_HOST".into(), "127.0.0.1".into()),
            ("PUSH_DATABASE_PORT".into(), self.postgres.port.to_string()),
            ("PUSH_DATABASE_USER".into(), role.into()),
            (
                "PUSH_DATABASE_CA_FILE".into(),
                self.ca.file().display().to_string(),
            ),
            ("PUSH_RUN_MIGRATIONS".into(), "false".into()),
        ]);
        variables
    }

    fn redis(&self) -> Vec<(String, String)> {
        vec![
            ("PUSH_REDIS_AUTH".into(), "azure-entra".into()),
            ("PUSH_REDIS_URL".into(), self.redis.url()),
            (
                "PUSH_REDIS_CA_FILE".into(),
                self.ca.file().display().to_string(),
            ),
        ]
    }

    /// Each service's `migrate`, as the migration job runs them.
    async fn migrate(&self) {
        let migrator = |runtime: &str| {
            let mut variables = self.postgres("migrator", "push-migrator-id");
            variables.push(("PUSH_DATABASE_RUNTIME_ROLE".into(), runtime.into()));
            variables
        };
        let mut auth = migrator("push-auth-id");
        auth.push(("PUSH_AUTH_WORKER_ROLE".into(), "push-worker-id".into()));
        push_auth::migrate(push_auth::MigrateConfig::from_lookup(lookup(auth)).unwrap())
            .await
            .unwrap();
        push_message_svc::migrate(
            push_message_svc::MigrateConfig::from_lookup(lookup(migrator("push-message-svc-id")))
                .unwrap(),
        )
        .await
        .unwrap();
        push_worker::migrate(
            push_worker::MigrateConfig::from_lookup(lookup(migrator("push-worker-id"))).unwrap(),
        )
        .await
        .unwrap();
    }

    fn datastores(&self) -> Datastores {
        let mut auth = self.postgres("auth", "push-auth-id");
        auth.extend(self.redis());
        let mut message_svc = self.postgres("message-svc", "push-message-svc-id");
        message_svc.extend(self.redis());
        let mut gateway = self.identity("gateway");
        gateway.extend(self.redis());
        let mut worker = self.postgres("worker", "push-worker-id");
        worker.push(("PUSH_AUTH_DATABASE_NAME".into(), "push_auth".into()));
        Datastores {
            auth,
            message_svc,
            gateway,
            worker,
        }
    }
}

fn replaced(
    mut variables: Vec<(String, String)>,
    name: &str,
    value: String,
) -> Vec<(String, String)> {
    variables.retain(|(key, _)| key != name);
    variables.push((name.to_owned(), value));
    variables
}

#[tokio::test]
async fn the_whole_stack_runs_on_rotating_entra_tokens() {
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(|| Capture)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let azure = azure(Duration::from_secs(6)).await;
    azure.migrate().await;
    let stack = Stack::start(azure.datastores()).await;
    let data_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("push-entra-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let target = stack.smoke_target(data_dir.to_string_lossy().into_owned());
    tokio::time::timeout(Duration::from_secs(300), push_smoke::run(&target))
        .await
        .expect("the smoke test finishes within 300 s")
        .expect("the smoke test passes");

    // 45 s of steady traffic: seven or more rotations of every token. No connection may
    // drop (reconnect off) and no message may be lost.
    let issued_before = azure.stub.issued().len();
    let report = common::load_run(&stack, Duration::from_secs(45)).await;
    assert_eq!(
        report.status,
        push_load::report::Status::Passed,
        "{:?}",
        report.failures
    );
    assert_eq!(report.connections.ready_once, 4);
    assert!(
        report.messages.lost == 0 && report.messages.delivered >= 10,
        "{:?}",
        report.messages
    );
    let rotations = azure.stub.issued().len() - issued_before;
    // Six token streams (auth-svc and message-svc: PostgreSQL and Redis; the gateway:
    // Redis; the worker: PostgreSQL for both databases), each refreshed every 4.5 s.
    assert!(rotations >= 6 * 7, "{rotations} tokens issued in 45 s");

    // New PostgreSQL connections after many rotations: only the newest token works now.
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename LIKE 'push-%-id'",
    )
    .execute(&azure.admin)
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(300), push_smoke::run(&target))
        .await
        .expect("the smoke test finishes within 300 s")
        .expect("the smoke test passes again");
    stack.stop().await;

    let log = String::from_utf8(LOG.lock().unwrap().clone()).unwrap();
    assert!(!log.is_empty(), "the log was captured");
    assert!(
        !log.contains(ASSERTION),
        "the federated token reached a log"
    );
    for issued in azure.stub.issued() {
        // The offending line, with the token itself masked.
        let leak = log
            .lines()
            .find(|line| line.contains(&issued.token))
            .map(|line| line.replace(&issued.token, "<TOKEN>"));
        assert!(leak.is_none(), "an access token reached a log: {leak:?}");
    }
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn start_up_stops_clearly_without_a_token_a_role_or_trust() {
    let azure = azure(Duration::from_secs(3600)).await;
    azure.migrate().await;
    let listeners = || async {
        let bind = || tokio::net::TcpListener::bind("127.0.0.1:0");
        push_message_svc::Listeners {
            messaging: bind().await.unwrap(),
            accounts: bind().await.unwrap(),
            directory: bind().await.unwrap(),
            health: bind().await.unwrap(),
        }
    };
    let serve = |variables: Vec<(String, String)>| async {
        let config = push_message_svc::Config::from_lookup(lookup(variables)).unwrap();
        tokio::time::timeout(
            Duration::from_secs(20),
            push_message_svc::serve(config, listeners().await, CancellationToken::new()),
        )
        .await
        .expect("start-up stops within 20 s instead of retrying")
        .expect_err("start-up fails")
    };
    let mut base = azure.postgres("message-svc", "push-message-svc-id");
    base.extend(azure.redis());

    azure.stub.set_failing(true);
    let message = format!("{:#}", serve(base.clone()).await);
    assert!(
        message.contains("credentials") && message.contains("503"),
        "{message}"
    );
    azure.stub.set_failing(false);

    // A token for a principal without a PostgreSQL role.
    let no_role = replaced(base.clone(), "PUSH_DATABASE_USER", "push-nobody-id".into());
    let message = format!("{:#}", serve(no_role).await);
    assert!(message.contains("postgres"), "{message}");

    // A server certificate from a CA nobody trusts.
    let other = TestCa::new();
    let untrusted = replaced(
        base.clone(),
        "PUSH_DATABASE_CA_FILE",
        other.file().display().to_string(),
    );
    let message = format!("{:#}", serve(untrusted).await);
    assert!(message.contains("postgres"), "{message}");

    // An extension the server does not allow-list.
    sqlx::raw_sql("ALTER DATABASE push_auth SET azure.extensions = 'PG_TRGM'")
        .execute(&azure.admin)
        .await
        .unwrap();
    let mut auth = azure.postgres("migrator", "push-migrator-id");
    auth.push(("PUSH_DATABASE_NAME".into(), "push_auth".into()));
    let error = push_auth::migrate(push_auth::MigrateConfig::from_lookup(lookup(auth)).unwrap())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("azure.extensions"),
        "{error:#}"
    );

    // Redis: a principal without an ACL user, and an untrusted certificate.
    let mut no_user = base.clone();
    no_user.push(("PUSH_REDIS_USER".into(), "nobody-oid".into()));
    let message = format!("{:#}", serve(no_user).await);
    assert!(message.contains("redis"), "{message}");
    let untrusted_redis = replaced(
        base.clone(),
        "PUSH_REDIS_CA_FILE",
        other.file().display().to_string(),
    );
    let message = format!("{:#}", serve(untrusted_redis).await);
    assert!(message.contains("redis"), "{message}");

    // The gateway proves its Redis credentials before serving.
    let mut gateway = azure.identity("gateway");
    gateway.extend(azure.redis());
    let mut gateway = replaced(
        gateway,
        "PUSH_REDIS_CA_FILE",
        other.file().display().to_string(),
    );
    gateway.extend(common::gateway_variables_for_a_failing_start());
    let config = push_gateway::Config::from_lookup(lookup(gateway)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        push_gateway::serve(config, listener, health, CancellationToken::new()),
    )
    .await
    .expect("the gateway stops within 20 s");
    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("redis"), "{message}");

    // Mixed modes: Entra PostgreSQL with a password-mode Redis keeps today's behaviour for
    // Redis: an unreachable Redis is retried, not fatal.
    let mut mixed = azure.postgres("message-svc", "push-message-svc-id");
    mixed.push(("PUSH_REDIS_URL".into(), "redis://:wrong@127.0.0.1:1".into()));
    let config = push_message_svc::Config::from_lookup(lookup(mixed)).unwrap();
    let shutdown = CancellationToken::new();
    let running = tokio::spawn(push_message_svc::serve(
        config,
        listeners().await,
        shutdown.clone(),
    ));
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(
        !running.is_finished(),
        "a password-mode Redis outage is retried"
    );
    shutdown.cancel();
    running.await.unwrap().unwrap();
}
