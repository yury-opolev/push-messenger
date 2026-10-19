//! `RedisNotifier` against a real Redis (Docker must be running).

use std::time::Duration;

use prost::Message;
use push_domain::{Event, Notifier, UserId};
use push_message_svc::RedisNotifier;
use push_proto::internal::v1::{EntriesAvailable, Notification, notification::Kind};
use push_proto::notify_channel;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio_stream::StreamExt;

async fn redis() -> (ContainerAsync<Redis>, String) {
    let container = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start a Redis container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("the Redis port");
    (container, format!("redis://127.0.0.1:{port}"))
}

#[tokio::test]
async fn a_published_event_reaches_the_users_channel() {
    let (_container, url) = redis().await;
    let user = UserId::new();
    let mut subscriber = redis::Client::open(url.as_str())
        .unwrap()
        .get_async_pubsub()
        .await
        .expect("connect a subscriber");
    subscriber
        .subscribe(notify_channel(user.0))
        .await
        .expect("subscribe");

    let notifier = RedisNotifier::connect(&url).await.expect("connect");
    notifier
        .publish(&[Event::EntriesAvailable { users: vec![user] }])
        .await;

    let mut messages = subscriber.on_message();
    let message = tokio::time::timeout(Duration::from_secs(5), messages.next())
        .await
        .expect("a notification within 5 s")
        .expect("the subscription is open");
    assert_eq!(message.get_channel_name(), notify_channel(user.0));
    let notification = Notification::decode(message.get_payload_bytes()).unwrap();
    assert_eq!(
        notification.kind,
        Some(Kind::EntriesAvailable(EntriesAvailable {}))
    );
}

#[tokio::test]
async fn publishing_without_redis_returns_without_failing() {
    let (container, url) = redis().await;
    let notifier = RedisNotifier::connect(&url).await.expect("connect");
    container.stop().await.expect("stop Redis");
    let events = [Event::EntriesAvailable {
        users: vec![UserId::new()],
    }];
    tokio::time::timeout(Duration::from_secs(30), notifier.publish(&events))
        .await
        .expect("a failed publish gives up instead of hanging");
}

#[tokio::test]
async fn the_publish_after_redis_closed_the_connection_still_rings() {
    let (_container, url) = redis().await;
    let user = UserId::new();
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut subscriber = client
        .get_async_pubsub()
        .await
        .expect("connect a subscriber");
    subscriber
        .subscribe(notify_channel(user.0))
        .await
        .expect("subscribe");
    let mut admin = client.get_multiplexed_async_connection().await.unwrap();
    let notifier = RedisNotifier::connect(&url).await.expect("connect");
    let events = [Event::EntriesAvailable { users: vec![user] }];
    notifier.publish(&events).await;
    let mut messages = subscriber.on_message();
    for pause in [None, Some(Duration::ZERO), Some(Duration::from_millis(200))] {
        if let Some(pause) = pause {
            // The subscriber is a pub/sub client: only the notifier's connection closes.
            let killed: i64 = redis::cmd("CLIENT")
                .arg("KILL")
                .arg("TYPE")
                .arg("normal")
                .arg("SKIPME")
                .arg("yes")
                .query_async(&mut admin)
                .await
                .unwrap();
            assert_eq!(killed, 1);
            tokio::time::sleep(pause).await;
            notifier.publish(&events).await;
        }
        let message = tokio::time::timeout(Duration::from_secs(5), messages.next())
            .await
            .unwrap_or_else(|_| panic!("a notification within 5 s ({pause:?})"))
            .expect("the subscription is open");
        assert_eq!(message.get_channel_name(), notify_channel(user.0));
    }
}
