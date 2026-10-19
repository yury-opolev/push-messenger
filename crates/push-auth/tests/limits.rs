//! The Redis rate-limit counters against a real Redis (Docker must be running).

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use push_auth::AuthError;
use push_auth::limits::{Action, Counter, Limit, Limits, Outcome, RedisCounter};
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio_util::sync::CancellationToken;

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
async fn a_counter_allows_up_to_its_limit_and_then_names_the_wait() {
    let (_container, url) = redis().await;
    let counter = RedisCounter::connect(&url, CancellationToken::new())
        .await
        .unwrap();
    let limit = Limit {
        max: 2,
        window: Duration::from_secs(60),
    };
    assert_eq!(counter.hit("rl:test", limit).await, Ok(Outcome::Allowed));
    assert_eq!(counter.hit("rl:test", limit).await, Ok(Outcome::Allowed));
    match counter.hit("rl:test", limit).await {
        Ok(Outcome::Limited { retry_after_s }) => {
            assert!((1..=60).contains(&retry_after_s), "{retry_after_s}")
        }
        other => panic!("expected Limited, got {other:?}"),
    }
    let mut connection = redis::Client::open(url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let ttl: i64 = redis::cmd("TTL")
        .arg("rl:test")
        .query_async(&mut connection)
        .await
        .unwrap();
    assert!((1..=60).contains(&ttl), "the window expires: {ttl}");
}

#[tokio::test]
async fn redis_never_sees_an_email_or_an_ip() {
    let (_container, url) = redis().await;
    let counter = RedisCounter::connect(&url, CancellationToken::new())
        .await
        .unwrap();
    let limits = Limits::new(Arc::new(counter));
    let ip: IpAddr = "203.0.113.7".parse().unwrap();
    limits
        .check(Action::Login, Some(ip), Some("alice@example.com"))
        .await
        .unwrap();
    let mut connection = redis::Client::open(url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let mut keys: Vec<String> = redis::cmd("KEYS")
        .arg("*")
        .query_async(&mut connection)
        .await
        .unwrap();
    keys.sort();
    assert_eq!(keys.len(), 3, "{keys:?}");
    assert!(keys[0].starts_with("rl:login:account:"), "{keys:?}");
    assert!(keys[1].starts_with("rl:login:account_ip:"), "{keys:?}");
    assert!(keys[2].starts_with("rl:login:ip:"), "{keys:?}");
    for key in keys {
        assert!(!key.contains("alice") && !key.contains("203.0.113.7"));
    }
}

#[tokio::test]
async fn without_redis_requests_are_refused_quickly_and_may_be_retried() {
    let (container, url) = redis().await;
    let counter = RedisCounter::connect(&url, CancellationToken::new())
        .await
        .unwrap();
    container.stop().await.expect("stop Redis");
    let limits = Limits::new(Arc::new(counter));
    let started = Instant::now();
    let refused = limits
        .check(Action::Login, None, Some("alice@example.com"))
        .await;
    assert_eq!(refused, Err(AuthError::Unavailable));
    assert!(refused.unwrap_err().retryable());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_check_after_redis_closed_the_connection_still_counts() {
    let (_container, url) = redis().await;
    let counter = RedisCounter::connect(&url, CancellationToken::new())
        .await
        .unwrap();
    let limit = Limit {
        max: 5,
        window: Duration::from_secs(60),
    };
    assert_eq!(counter.hit("rl:kill", limit).await, Ok(Outcome::Allowed));
    let mut admin = redis::Client::open(url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    for pause in [Duration::ZERO, Duration::from_millis(200)] {
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
        assert_eq!(
            counter.hit("rl:kill", limit).await,
            Ok(Outcome::Allowed),
            "after {pause:?}"
        );
    }
}
