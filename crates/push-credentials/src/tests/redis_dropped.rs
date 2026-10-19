//! A connection the server closes: RESP2 managers fail the next command (redis 1.2),
//! [`retry_if_dropped`] sends it again, and the connector's RESP3 managers reconnect at
//! once.

use std::time::Duration;

use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

use crate::{RedisConnector, is_dropped_connection, retry_if_dropped};

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

/// Closes every client connection but `admin`'s, then gives the clients time to see it.
async fn kill_clients(admin: &mut redis::aio::MultiplexedConnection) {
    let killed: i64 = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg("normal")
        .arg("SKIPME")
        .arg("yes")
        .query_async(admin)
        .await
        .unwrap();
    assert!(killed >= 1, "{killed}");
    tokio::time::sleep(Duration::from_millis(200)).await;
}

async fn ping(manager: &redis::aio::ConnectionManager) -> redis::RedisResult<String> {
    redis::cmd("PING").query_async(&mut manager.clone()).await
}

#[tokio::test]
async fn a_dropped_connection_fails_one_resp2_command_and_a_retry_succeeds() {
    let (_container, url) = redis().await;
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut admin = client.get_multiplexed_async_connection().await.unwrap();
    let manager = client.get_connection_manager().await.unwrap();
    assert_eq!(ping(&manager).await.unwrap(), "PONG");

    kill_clients(&mut admin).await;
    let error = ping(&manager)
        .await
        .expect_err("RESP2 finds out on the next command");
    assert!(is_dropped_connection(&error), "{error:?}");
    assert_eq!(ping(&manager).await.unwrap(), "PONG", "reconnected");

    kill_clients(&mut admin).await;
    let mut calls = 0;
    let answer = retry_if_dropped(|| {
        calls += 1;
        ping(&manager)
    })
    .await;
    assert_eq!(answer.unwrap(), "PONG");
    assert_eq!(calls, 2);
}

#[tokio::test]
async fn other_errors_are_not_retried() {
    let (_container, url) = redis().await;
    let manager = redis::Client::open(url.as_str())
        .unwrap()
        .get_connection_manager()
        .await
        .unwrap();
    let mut calls = 0;
    let wrong_type = retry_if_dropped(|| {
        calls += 1;
        let mut manager = manager.clone();
        async move {
            let _: () = redis::cmd("SET")
                .arg("k")
                .arg("v")
                .query_async(&mut manager)
                .await?;
            redis::cmd("INCR")
                .arg("k")
                .query_async::<i64>(&mut manager)
                .await
        }
    })
    .await;
    assert!(wrong_type.is_err());
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn the_connectors_managers_reconnect_as_soon_as_the_server_closes_them() {
    let (_container, url) = redis().await;
    let mut admin = redis::Client::open(url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let connector = RedisConnector::password(&url).unwrap();
    let manager = connector
        .connection_manager(redis::aio::ConnectionManagerConfig::new())
        .await
        .unwrap();
    let lazy = connector
        .connection_manager_lazy(redis::aio::ConnectionManagerConfig::new())
        .unwrap();
    assert_eq!(ping(&lazy).await.unwrap(), "PONG");
    let protocol: std::collections::HashMap<String, redis::Value> = redis::cmd("HELLO")
        .query_async(&mut manager.clone())
        .await
        .unwrap();
    assert_eq!(protocol.get("proto"), Some(&redis::Value::Int(3)));

    kill_clients(&mut admin).await;
    // No retry: RESP3 managers hear the close and have reconnected already.
    assert_eq!(ping(&manager).await.unwrap(), "PONG");
    assert_eq!(ping(&lazy).await.unwrap(), "PONG");
}
