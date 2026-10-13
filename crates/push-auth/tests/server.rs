//! The service as a whole: start-up, readiness, the API on a real socket, and shutdown
//! (Docker must be running).

use std::net::SocketAddr;
use std::time::Duration;

use push_auth::{Config, serve};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// base64 of 32 bytes of 0x07 and of 0x09.
const SIGNING_KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";
const LINK_KEY: &str = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=";

fn config(database_url: &str, redis_url: &str) -> Config {
    let variables = [
        ("PUSH_DATABASE_URL", database_url),
        ("PUSH_REDIS_URL", redis_url),
        ("PUSH_AUTH_PUBLIC_URL", "http://push.local"),
        ("PUSH_AUTH_TOKEN_AUDIENCE", "ws://push.local/v1/socket"),
        ("PUSH_AUTH_SIGNING_KEY", SIGNING_KEY),
        ("PUSH_AUTH_LINK_KEY", LINK_KEY),
    ];
    Config::from_lookup(|name| {
        variables
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    })
    .unwrap()
}

/// `GET path` over a plain socket: the status code and the body, or `(0, "")` after 5 s.
async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    tokio::time::timeout(std::time::Duration::from_secs(5), get_once(addr, path))
        .await
        .unwrap_or((0, String::new()))
}

async fn get_once(addr: SocketAddr, path: &str) -> (u16, String) {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return (0, String::new());
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return (0, String::new());
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).await.is_err() {
        return (0, String::new());
    }
    let status = response
        .get(9..12)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
}

async fn listeners() -> (TcpListener, TcpListener, SocketAddr, SocketAddr) {
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (http_addr, health_addr) = (http.local_addr().unwrap(), health.local_addr().unwrap());
    (http, health, http_addr, health_addr)
}

#[tokio::test]
async fn it_becomes_ready_serves_the_jwks_and_stops_cleanly() {
    let postgres = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start Postgres (is Docker running?)");
    let redis = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start Redis");
    let database_url = format!(
        "postgres://postgres:postgres@127.0.0.1:{}/postgres",
        postgres.get_host_port_ipv4(5432).await.unwrap()
    );
    let redis_url = format!(
        "redis://127.0.0.1:{}",
        redis.get_host_port_ipv4(6379).await.unwrap()
    );
    let (http, health, http_addr, health_addr) = listeners().await;
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        config(&database_url, &redis_url),
        http,
        health,
        shutdown.clone(),
    ));

    let mut ready = false;
    for _ in 0..240 {
        assert!(
            !server.is_finished(),
            "the service stopped before it became ready"
        );
        if get(health_addr, "/readyz").await.0 == 200 {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(ready, "not ready within 60 s");
    assert_eq!(get(health_addr, "/healthz").await, (200, "ok".to_owned()));
    let (status, body) = get(http_addr, "/v1/auth/jwks").await;
    assert_eq!(status, 200);
    assert!(body.contains(r#""crv":"Ed25519""#), "{body}");
    assert_eq!(get(http_addr, "/dev/mailbox?to=a@example.com").await.0, 404);

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("stops within 10 s")
        .unwrap()
        .expect("stops cleanly");
}

#[tokio::test]
async fn without_a_database_it_is_not_ready_and_still_stops_when_asked() {
    // Nothing listens on this port.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let (http, health, _http_addr, health_addr) = listeners().await;
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        config(
            &format!("postgres://postgres:postgres@127.0.0.1:{port}/push_auth"),
            "redis://127.0.0.1:1",
        ),
        http,
        health,
        shutdown.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        get(health_addr, "/readyz").await,
        (503, "starting".to_owned())
    );
    assert_eq!(get(health_addr, "/healthz").await.0, 200);
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the start-up retries stop on shutdown")
        .unwrap()
        .expect("stops cleanly");
}
