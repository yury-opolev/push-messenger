//! PostgreSQL with Entra tokens over verified TLS: rotation for new connections, an
//! unknown principal and an untrusted CA refused, the citext allow-list, and the
//! messaging schema used by a non-owner runtime role (Docker must be running).

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use push_credentials::testing::{
    IssueHook, Issued, StubIssuer, StubPrincipal, TestCa, TlsPostgres, set_postgres_password,
    tls_postgres,
};
use push_credentials::{AzureConfig, Tokens};
use push_domain::{ClientMsgId, DeviceId, Messaging, NoopNotifier, SendText, SystemClock, UserId};
use push_store::{
    Connector, EntraDatabase, PgStore, StoreOptions, grant_runtime_access, is_permanent,
};
use tokio_util::sync::CancellationToken;

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const ROLE: &str = "push-store-id";

struct Setup {
    ca: TestCa,
    postgres: TlsPostgres,
    admin: sqlx::PgPool,
    stub: StubIssuer,
}

async fn setup(lifetime: Duration, register: bool) -> Setup {
    push_credentials::install_crypto_provider();
    let ca = TestCa::new();
    let postgres = tls_postgres(&ca).await;
    let admin = postgres.admin(&ca).await;
    sqlx::raw_sql(&format!("CREATE ROLE \"{ROLE}\" LOGIN"))
        .execute(&admin)
        .await
        .unwrap();
    sqlx::raw_sql(&format!("CREATE DATABASE push_messaging OWNER \"{ROLE}\""))
        .execute(&admin)
        .await
        .unwrap();
    let hook: Option<IssueHook> = register.then(|| {
        let admin = admin.clone();
        Arc::new(move |issued: Issued| {
            let admin = admin.clone();
            Box::pin(async move { set_postgres_password(&admin, ROLE, &issued.token).await })
                as BoxFuture<'static, ()>
        }) as IssueHook
    });
    let principals = vec![StubPrincipal {
        client_id: "message-svc".into(),
        object_id: "oid-m".into(),
    }];
    let stub = StubIssuer::start(TENANT, "federated", principals, lifetime, hook).await;
    Setup {
        ca,
        postgres,
        admin,
        stub,
    }
}

async fn tokens(setup: &Setup) -> Tokens {
    let file = std::env::temp_dir().join(format!("push-store-{}.jwt", uuid::Uuid::now_v7()));
    std::fs::write(&file, "federated").unwrap();
    let azure = AzureConfig {
        client_id: Some("message-svc".into()),
        tenant_id: Some(TENANT.into()),
        federated_token_file: Some(file),
        authority_host: setup.stub.authority().into(),
    };
    Tokens::start(
        azure.source().unwrap(),
        push_store::POSTGRES_SCOPE,
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

fn database(setup: &Setup, ca: &TestCa) -> EntraDatabase {
    EntraDatabase {
        host: "127.0.0.1".into(),
        port: setup.postgres.port,
        name: "push_messaging".into(),
        user: ROLE.into(),
        ca_file: Some(ca.file().to_owned()),
    }
}

#[tokio::test]
async fn new_connections_use_the_newest_token_and_old_ones_keep_working() {
    let setup = setup(Duration::from_secs(2), true).await;
    let connector = Connector::entra(&database(&setup, &setup.ca), tokens(&setup).await);
    connector.migrate(&push_store::MIGRATOR, &[]).await.unwrap();
    let pool = connector
        .pool(&StoreOptions {
            max_connections: 2,
            ..StoreOptions::default()
        })
        .await
        .unwrap();
    let mut held = pool.acquire().await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await; // at least two rotations
    assert!(setup.stub.issued().len() >= 3);
    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    assert_eq!(one, 1, "an old connection keeps working");
    drop(held);
    sqlx::query(&format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = '{ROLE}'"
    ))
    .execute(&setup.admin)
    .await
    .unwrap();
    // Only the newest token is the password now: a new connection must use it.
    let mut fresh = None;
    for _ in 0..30 {
        if let Ok(value) = sqlx::query_scalar::<_, i32>("SELECT 2")
            .fetch_one(&pool)
            .await
        {
            fresh = Some(value);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(fresh, Some(2));
}

#[tokio::test]
async fn an_unknown_principal_and_an_untrusted_ca_are_permanent_errors() {
    let setup = setup(Duration::from_secs(3600), false).await;
    let connector = Connector::entra(&database(&setup, &setup.ca), tokens(&setup).await);
    let refused = connector.pool(&StoreOptions::default()).await.unwrap_err();
    assert!(is_permanent(&refused), "{refused:?}");
    let message = refused.to_string();
    assert!(
        !message.contains(setup.stub.issued()[0].token.as_str()),
        "{message}"
    );

    let other = TestCa::new();
    let untrusted = Connector::entra(&database(&setup, &other), tokens(&setup).await);
    let error = untrusted.pool(&StoreOptions::default()).await.unwrap_err();
    assert!(is_permanent(&error), "{error:?}");
}

#[tokio::test]
async fn migrations_stop_when_citext_is_not_allow_listed() {
    let setup = setup(Duration::from_secs(3600), true).await;
    sqlx::raw_sql("ALTER DATABASE push_messaging SET azure.extensions = 'PG_TRGM,PGCRYPTO'")
        .execute(&setup.admin)
        .await
        .unwrap();
    let connector = Connector::entra(&database(&setup, &setup.ca), tokens(&setup).await);
    let error = connector
        .migrate(&push_store::MIGRATOR, &["citext"])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("azure.extensions") && error.contains("CITEXT"),
        "{error}"
    );
    sqlx::raw_sql("ALTER DATABASE push_messaging SET azure.extensions = 'CITEXT,PG_TRGM'")
        .execute(&setup.admin)
        .await
        .unwrap();
    connector
        .migrate(&push_store::MIGRATOR, &["citext"])
        .await
        .unwrap();
}

/// With a migration identity, the runtime role owns nothing: the domain still works.
#[tokio::test]
async fn the_messaging_domain_runs_as_a_granted_non_owner_role() {
    let setup = setup(Duration::from_secs(3600), true).await;
    let connector = Connector::entra(&database(&setup, &setup.ca), tokens(&setup).await);
    connector.migrate(&push_store::MIGRATOR, &[]).await.unwrap();
    sqlx::raw_sql("CREATE ROLE \"push-runtime\" LOGIN PASSWORD 'runtime-password'")
        .execute(&setup.admin)
        .await
        .unwrap();
    let owner = connector.single().await.unwrap();
    grant_runtime_access(&owner, "messaging", "push-runtime")
        .await
        .unwrap();
    owner.close().await;
    let runtime = Connector::from_options(
        setup
            .postgres
            .options(&setup.ca, "push-runtime", "push_messaging")
            .password("runtime-password"),
    );
    let store = PgStore::new(runtime.pool(&StoreOptions::default()).await.unwrap());
    let messaging = Messaging::new(store, SystemClock, NoopNotifier);
    let (alice, bob) = (UserId::new(), UserId::new());
    let (phone, laptop) = (DeviceId::new(), DeviceId::new());
    messaging.device_registered(phone, alice).await.unwrap();
    messaging.device_registered(laptop, bob).await.unwrap();
    messaging.contact_request(alice, bob).await.unwrap();
    messaging.contact_accept(bob, alice).await.unwrap();
    let conversation = messaging.open_direct(alice, bob).await.unwrap();
    let sent = messaging
        .send_text(SendText {
            sender: alice,
            device: phone,
            conversation,
            client_msg_id: ClientMsgId::new(),
            body: b"hello".to_vec(),
            client_ts: None,
        })
        .await
        .unwrap();
    let batch = messaging.sync(laptop, 10).await.unwrap();
    assert!(!batch.entries.is_empty());
    messaging.ack(laptop, conversation, sent.seq).await.unwrap();
}
