//! The hub on Redis with Entra tokens: rotations replace the subscriber without a gap,
//! and a killed subscriber comes back with a fresh token.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use prost::Message;
use push_credentials::testing::{
    IssueHook, Issued, StubIssuer, StubPrincipal, TestCa, TlsRedis, set_redis_password, tls_redis,
};
use push_credentials::{AzureConfig, RedisConnector, RedisTarget};
use push_gateway::{Hub, Signal};
use push_proto::internal::v1::{
    EntriesAvailable, Kick, KickReason, Notification, notification::Kind,
};
use push_proto::{kick_channel, notify_channel};
use redis::AsyncCommands;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const OID: &str = "0a1b2c3d-0000-4000-8000-0000000000aa";

struct Setup {
    _ca: TestCa,
    _server: TlsRedis,
    admin: redis::aio::MultiplexedConnection,
    stub: StubIssuer,
    connector: RedisConnector,
}

async fn setup() -> Setup {
    push_credentials::install_crypto_provider();
    let ca = TestCa::new();
    let server = tls_redis(&ca).await;
    let admin = server.admin(&ca).await;
    let hook: IssueHook = {
        let admin = admin.clone();
        Arc::new(move |issued: Issued| {
            let mut admin = admin.clone();
            Box::pin(async move {
                set_redis_password(&mut admin, &issued.object_id, &issued.token).await
            }) as BoxFuture<'static, ()>
        })
    };
    let principals = vec![StubPrincipal {
        client_id: "gateway".into(),
        object_id: OID.into(),
    }];
    let stub = StubIssuer::start(
        TENANT,
        "federated",
        principals,
        Duration::from_secs(2),
        Some(hook),
    )
    .await;
    let file = std::env::temp_dir().join(format!("push-hub-{}.jwt", Uuid::now_v7()));
    std::fs::write(&file, "federated").unwrap();
    let azure = AzureConfig {
        client_id: Some("gateway".into()),
        tenant_id: Some(TENANT.into()),
        federated_token_file: Some(file),
        authority_host: stub.authority().into(),
    };
    let target = RedisTarget::Entra {
        url: server.url(),
        user: None,
        ca_file: Some(ca.file().to_owned()),
    };
    let connector = RedisConnector::open(
        &target,
        Some(&azure.source().unwrap()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    Setup {
        _ca: ca,
        _server: server,
        admin,
        stub,
        connector,
    }
}

fn ring_payload() -> Vec<u8> {
    Notification {
        kind: Some(Kind::EntriesAvailable(EntriesAvailable {})),
    }
    .encode_to_vec()
}

async fn connected(hub: &Hub) {
    for _ in 0..100 {
        if hub.is_connected() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the hub did not connect within 5 s");
}

async fn subscribers(admin: &mut redis::aio::MultiplexedConnection, channel: &str) -> i64 {
    let counts: Vec<(String, i64)> = redis::cmd("PUBSUB")
        .arg("NUMSUB")
        .arg(channel)
        .query_async(admin)
        .await
        .unwrap();
    counts[0].1
}

/// The ids of the hub's pub/sub connections to Redis.
async fn subscriber_ids(admin: &mut redis::aio::MultiplexedConnection) -> Vec<String> {
    let list: String = redis::cmd("CLIENT")
        .arg("LIST")
        .arg("TYPE")
        .arg("pubsub")
        .query_async(admin)
        .await
        .unwrap();
    list.lines()
        .filter(|line| line.split(' ').any(|field| field == format!("user={OID}")))
        .filter_map(|line| line.split(' ').find_map(|field| field.strip_prefix("id=")))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn rotations_replace_the_subscriber_without_losing_a_notification() {
    let mut setup = setup().await;
    let shutdown = CancellationToken::new();
    let (hub, subscriber) = Hub::start(setup.connector.clone(), shutdown.clone());
    connected(&hub).await;
    // One session per kick: a kick is never coalesced, so each lost message shows.
    let mut sessions = Vec::new();
    for _ in 0..35 {
        let device = Uuid::now_v7();
        sessions.push((device, hub.watch_device(device, Uuid::now_v7(), 0).await));
    }
    let issued_before = setup.stub.issued().len();
    let first = subscriber_ids(&mut setup.admin).await;
    assert_eq!(first.len(), 1, "{first:?}");
    // A publisher at 5 a second, independent of the receiving side, for about 7 s: three
    // or more rotations happen while kicks keep coming.
    let publisher = {
        let mut admin = setup.admin.clone();
        let devices: Vec<Uuid> = sessions.iter().map(|(device, _)| *device).collect();
        tokio::spawn(async move {
            for device in devices {
                let kick = Kick {
                    reason: KickReason::DeviceRevoked.into(),
                    ..Default::default()
                };
                let _: i64 = admin
                    .publish(kick_channel(device), kick.encode_to_vec())
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
    };
    for (device, session) in &mut sessions {
        let signal = tokio::time::timeout(Duration::from_secs(10), session.next())
            .await
            .unwrap_or_else(|_| panic!("the kick of {device} was lost"));
        assert_eq!(signal, Signal::Kick(KickReason::DeviceRevoked));
        assert!(hub.is_connected(), "a rotation must not disconnect the hub");
    }
    publisher.await.unwrap();
    assert!(
        setup.stub.issued().len() >= issued_before + 3,
        "at least three rotations happened"
    );
    // Stop the rotations, then the replaced subscribers must all be gone.
    setup.stub.set_failing(true);
    let channel = kick_channel(sessions[0].0);
    let mut count = 0;
    for _ in 0..50 {
        count = subscribers(&mut setup.admin, &channel).await;
        if count == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(count, 1, "the replaced subscribers are gone");
    let last = subscriber_ids(&mut setup.admin).await;
    assert!(
        last.len() == 1 && last != first,
        "the subscriber was replaced: {first:?} then {last:?}"
    );
    shutdown.cancel();
    subscriber.await.unwrap();
}

#[tokio::test]
async fn a_killed_subscriber_returns_with_a_fresh_token_and_its_channels() {
    let mut setup = setup().await;
    let shutdown = CancellationToken::new();
    let (hub, subscriber) = Hub::start(setup.connector.clone(), shutdown.clone());
    connected(&hub).await;
    let user = Uuid::now_v7();
    let mut session = hub.watch_device(Uuid::now_v7(), Uuid::now_v7(), 0).await;
    session.join_user(user).await;
    // Let the token it connected with be replaced, then cut every connection of the user.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("USER")
        .arg(OID)
        .query_async(&mut setup.admin)
        .await
        .unwrap();
    let ring = tokio::time::timeout(Duration::from_secs(10), session.next())
        .await
        .expect("the reconnect rings");
    assert_eq!(ring, Signal::Ring);
    connected(&hub).await;
    let _: i64 = setup
        .admin
        .publish(notify_channel(user), ring_payload())
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), session.next())
            .await
            .unwrap(),
        Signal::Ring
    );
    shutdown.cancel();
    subscriber.await.unwrap();
}
