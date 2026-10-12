//! The hub's Redis subscriber against a real Redis (Docker must be running).

use std::time::Duration;

use prost::Message;
use push_gateway::Hub;
use push_proto::internal::v1::{EntriesAvailable, Notification, notification::Kind};
use push_proto::notify_channel;
use redis::AsyncCommands;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn redis() -> (ContainerAsync<Redis>, redis::Client) {
    let container = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start a Redis container (is Docker running?)");
    let port = container.get_host_port_ipv4(6379).await.unwrap();
    let client = redis::Client::open(format!("redis://127.0.0.1:{port}")).unwrap();
    (container, client)
}

async fn connected(hub: &Hub) {
    for _ in 0..100 {
        if hub.is_connected() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the hub did not connect to Redis within 5 s");
}

async fn subscribers(client: &redis::Client, channel: &str) -> i64 {
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
    let counts: Vec<(String, i64)> = redis::cmd("PUBSUB")
        .arg("NUMSUB")
        .arg(channel)
        .query_async(&mut connection)
        .await
        .unwrap();
    counts[0].1
}

#[tokio::test]
async fn a_notification_published_on_redis_rings_the_users_session() {
    let (_container, client) = redis().await;
    let shutdown = CancellationToken::new();
    let (hub, subscriber) = Hub::start(client.clone(), shutdown.clone());
    connected(&hub).await;

    let user = Uuid::now_v7();
    let mut session = hub.join(user).await;
    let payload = Notification {
        kind: Some(Kind::EntriesAvailable(EntriesAvailable {})),
    }
    .encode_to_vec();
    let mut publisher = client.get_multiplexed_async_connection().await.unwrap();
    let _: i64 = publisher
        .publish(notify_channel(user), payload)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), session.rung())
        .await
        .expect("the session is rung within 5 s");

    shutdown.cancel();
    subscriber.await.unwrap();
}

#[tokio::test]
async fn a_session_is_rung_again_after_the_subscription_is_reestablished() {
    let (_container, client) = redis().await;
    let shutdown = CancellationToken::new();
    let (hub, subscriber) = Hub::start(client.clone(), shutdown.clone());
    connected(&hub).await;

    let user = Uuid::now_v7();
    let channel = notify_channel(user);
    let mut session = hub.join(user).await;
    assert_eq!(subscribers(&client, &channel).await, 1);

    let mut admin = client.get_multiplexed_async_connection().await.unwrap();
    let killed: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg("pubsub")
        .query_async(&mut admin)
        .await
        .unwrap();
    assert!(killed >= 1, "the hub's pubsub connection was killed");

    // The reconnect rings every session, since notifications may have been lost.
    tokio::time::timeout(Duration::from_secs(10), session.rung())
        .await
        .expect("the session is rung after the reconnect within 10 s");
    connected(&hub).await;
    assert_eq!(subscribers(&client, &channel).await, 1);

    let payload = Notification {
        kind: Some(Kind::EntriesAvailable(EntriesAvailable {})),
    }
    .encode_to_vec();
    let _: i64 = admin.publish(&channel, payload).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), session.rung())
        .await
        .expect("a fresh publish rings the session within 5 s");

    shutdown.cancel();
    subscriber.await.unwrap();
}

#[tokio::test]
async fn the_channel_is_unsubscribed_when_the_last_session_leaves() {
    let (_container, client) = redis().await;
    let shutdown = CancellationToken::new();
    let (hub, _subscriber) = Hub::start(client.clone(), shutdown.clone());
    connected(&hub).await;

    let user = Uuid::now_v7();
    let channel = notify_channel(user);
    let session = hub.join(user).await;
    assert_eq!(subscribers(&client, &channel).await, 1);
    drop(session);
    for _ in 0..100 {
        if subscribers(&client, &channel).await == 0 {
            shutdown.cancel();
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the channel is still subscribed 5 s after the last session left");
}
