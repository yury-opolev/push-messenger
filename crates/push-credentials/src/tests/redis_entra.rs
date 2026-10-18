//! Redis with Entra tokens: TLS, a user from the token, re-authentication after
//! rotations, reconnection with a fresh token, and refusals.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use redis::aio::ConnectionManagerConfig;
use tokio_util::sync::CancellationToken;

use crate::testing::{
    IssueHook, Issued, StubIssuer, StubPrincipal, TestCa, TlsRedis, set_redis_password, tls_redis,
};
use crate::{
    AzureConfig, ConfigProblem, RedisConnector, RedisTarget, is_permanent_redis, is_refused,
};

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const OID: &str = "0a1b2c3d-0000-4000-8000-000000000001";

fn lookup(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
    move |name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    }
}

#[test]
fn targets_parse_and_entra_needs_tls_without_credentials() {
    let password = RedisTarget::from_lookup(&lookup(vec![(
        "PUSH_REDIS_URL",
        "redis://:hunter2@redis:6379".into(),
    )]))
    .unwrap();
    assert!(!format!("{password:?}").contains("hunter2"));
    assert_eq!(
        RedisTarget::from_lookup(&lookup(vec![])),
        Err(ConfigProblem::Missing("PUSH_REDIS_URL"))
    );
    let entra = |url: &str| {
        RedisTarget::from_lookup(&lookup(vec![
            ("PUSH_REDIS_AUTH", "azure-entra".into()),
            ("PUSH_REDIS_URL", url.into()),
        ]))
    };
    assert!(matches!(
        entra("rediss://cache.example.net:10000"),
        Ok(RedisTarget::Entra { .. })
    ));
    for bad in [
        "redis://cache:10000",
        "rediss://user:pw@cache:10000",
        "rediss://:pw@cache:10000",
    ] {
        assert!(
            matches!(
                entra(bad),
                Err(ConfigProblem::Invalid {
                    name: "PUSH_REDIS_URL",
                    ..
                })
            ),
            "{bad}"
        );
    }
}

struct Setup {
    _ca: TestCa,
    _server: TlsRedis,
    stub: StubIssuer,
    connector: RedisConnector,
    admin: redis::aio::MultiplexedConnection,
}

async fn azure(stub: &StubIssuer) -> AzureConfig {
    let file = std::env::temp_dir().join(format!("push-redis-{}.jwt", uuid::Uuid::now_v7()));
    std::fs::write(&file, "federated").unwrap();
    AzureConfig {
        client_id: Some("gateway".into()),
        tenant_id: Some(TENANT.into()),
        federated_token_file: Some(file),
        authority_host: stub.authority().into(),
    }
}

async fn setup(lifetime: Duration, register: bool, trust: Option<&TestCa>) -> Setup {
    crate::install_crypto_provider();
    let ca = TestCa::new();
    let server = tls_redis(&ca).await;
    let admin = server.admin(&ca).await;
    let hook: Option<IssueHook> = register.then(|| {
        let admin = admin.clone();
        Arc::new(move |issued: Issued| {
            let mut admin = admin.clone();
            Box::pin(async move {
                set_redis_password(&mut admin, &issued.object_id, &issued.token).await
            }) as BoxFuture<'static, ()>
        }) as IssueHook
    });
    let principals = vec![StubPrincipal {
        client_id: "gateway".into(),
        object_id: OID.into(),
    }];
    let stub = StubIssuer::start(TENANT, "federated", principals, lifetime, hook).await;
    let target = RedisTarget::Entra {
        url: server.url(),
        user: None,
        ca_file: Some(trust.unwrap_or(&ca).file().to_owned()),
    };
    let source = azure(&stub).await.source().unwrap();
    let connector = RedisConnector::open(&target, Some(&source), CancellationToken::new())
        .await
        .unwrap();
    Setup {
        _ca: ca,
        _server: server,
        stub,
        connector,
        admin,
    }
}

#[tokio::test]
async fn managed_connections_and_subscribers_survive_rotations_and_kills() {
    let mut setup = setup(Duration::from_secs(2), true, None).await;
    let mut manager = setup
        .connector
        .connection_manager(ConnectionManagerConfig::new())
        .await
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg("k")
        .arg("v")
        .query_async(&mut manager)
        .await
        .unwrap();
    // Three rotations: each token became the only password.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        setup.stub.issued().len() >= 3,
        "{}",
        setup.stub.issued().len()
    );
    let value: String = redis::cmd("GET")
        .arg("k")
        .query_async(&mut manager)
        .await
        .unwrap();
    assert_eq!(value, "v");
    // Azure closes connections whose token expired; the manager reconnects with the
    // current token.
    let _: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("USER")
        .arg(OID)
        .query_async(&mut setup.admin)
        .await
        .unwrap();
    let mut answered = None;
    for _ in 0..30 {
        if let Ok(value) = redis::cmd("GET")
            .arg("k")
            .query_async::<String>(&mut manager)
            .await
        {
            answered = Some(value);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(answered.as_deref(), Some("v"));
    let mut pubsub = setup.connector.pubsub().await.unwrap();
    pubsub.subscribe("ch").await.unwrap();
    let _: i64 = redis::cmd("PUBLISH")
        .arg("ch")
        .arg("hi")
        .query_async(&mut manager)
        .await
        .unwrap();
    let message = futures_util::StreamExt::next(&mut pubsub.on_message())
        .await
        .unwrap();
    assert_eq!(message.get_payload::<String>().unwrap(), "hi");
    assert!(setup.connector.rotations().is_some());
}

#[tokio::test]
async fn a_principal_without_a_redis_user_is_refused() {
    let setup = setup(Duration::from_secs(3600), false, None).await;
    let Err(error) = setup
        .connector
        .connection_manager(ConnectionManagerConfig::new())
        .await
    else {
        panic!("refused");
    };
    assert!(is_refused(&error), "{error:?}");
}

#[tokio::test]
async fn an_untrusted_server_certificate_is_permanent() {
    let other = TestCa::new();
    let setup = setup(Duration::from_secs(3600), true, Some(&other)).await;
    let Err(error) = setup
        .connector
        .connection_manager(ConnectionManagerConfig::new())
        .await
    else {
        panic!("refused");
    };
    assert!(is_permanent_redis(&error), "{error:?}");
    assert!(!is_refused(&error));
}
