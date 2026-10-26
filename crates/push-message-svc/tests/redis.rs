//! `RedisNotifier` and `RedisPresence` against a real Redis (Docker must be running).

use std::time::Duration;

use prost::Message;
use push_domain::{Event, Notifier, PresenceView, UserId};
use push_message_svc::{PresenceStates, RedisNotifier};
use push_proto::internal::v1::{EntriesAvailable, Notification, notification::Kind};
use push_proto::{notify_channel, presence_conns_key, presence_state_key};
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

#[tokio::test]
async fn the_read_script_reports_online_offline_and_flips_once() {
    let (_container, url) = redis().await;
    let mut admin = redis::Client::open(url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let (online, gone, never) = (UserId::new(), UserId::new(), UserId::new());
    let now_ms = chrono::Utc::now().timestamp_millis();
    let minute = 1_760_000_040_i64;
    let _: () = redis::cmd("ZADD")
        .arg(presence_conns_key(online.0))
        .arg(now_ms + 60_000)
        .arg("connection-1")
        .query_async(&mut admin)
        .await
        .unwrap();
    let _: () = redis::cmd("ZADD")
        .arg(presence_conns_key(gone.0))
        .arg(now_ms - 60_000)
        .arg("connection-2")
        .query_async(&mut admin)
        .await
        .unwrap();
    let _: () = redis::cmd("HSET")
        .arg(presence_state_key(gone.0))
        .arg("on")
        .arg("1")
        .arg("seen")
        .arg(minute)
        .query_async(&mut admin)
        .await
        .unwrap();

    let presence = RedisNotifier::connect(&url)
        .await
        .expect("connect")
        .presence();
    let users = [online, gone, never];
    let last_seen = chrono::DateTime::from_timestamp(minute, 0);
    let (views, flipped) = presence.states(&users).await;
    assert_eq!(
        views,
        [
            PresenceView::Online,
            PresenceView::Offline { last_seen },
            PresenceView::Offline { last_seen: None },
        ]
    );
    assert_eq!(flipped, [gone]);

    let (views, flipped) = presence.states(&users).await;
    assert_eq!(views[1], PresenceView::Offline { last_seen });
    assert!(flipped.is_empty(), "a user flips only once");
    let expired: i64 = redis::cmd("ZCARD")
        .arg(presence_conns_key(gone.0))
        .query_async(&mut admin)
        .await
        .unwrap();
    assert_eq!(expired, 0, "the expired lease is pruned");

    // A Redis that lost its scripts (a restart or SCRIPT FLUSH) is loaded again.
    let _: () = redis::cmd("SCRIPT")
        .arg("FLUSH")
        .query_async(&mut admin)
        .await
        .unwrap();
    let (views, _) = presence.states(&[online]).await;
    assert_eq!(views, [PresenceView::Online]);
}

#[tokio::test]
async fn presence_reads_without_redis_answer_offline() {
    let (container, url) = redis().await;
    let presence = RedisNotifier::connect(&url)
        .await
        .expect("connect")
        .presence();
    container.stop().await.expect("stop Redis");
    let users = [UserId::new(), UserId::new()];
    let (views, flipped) = tokio::time::timeout(Duration::from_secs(30), presence.states(&users))
        .await
        .expect("a failed read gives up instead of hanging");
    assert_eq!(views, [PresenceView::Offline { last_seen: None }; 2]);
    assert!(flipped.is_empty());
}
