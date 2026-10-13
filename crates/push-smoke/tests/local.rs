//! The walking skeleton on this machine: Postgres and Redis in containers (Docker must be
//! running), auth-svc, message-svc and the gateway in this process, and the smoke test
//! against them over real HTTP, gRPC and WebSockets.

use std::net::SocketAddr;
use std::time::Duration;

use push_smoke::{Target, run};
use push_store::StoreOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn the_smoke_test_passes_against_the_whole_stack() {
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
    let shutdown = CancellationToken::new();

    // auth-svc has a database of its own in the same server, as in the chart.
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    sqlx::query("CREATE DATABASE push_auth")
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    let (server_url, _) = database_url.rsplit_once('/').unwrap();
    let auth_database_url = format!("{server_url}/push_auth");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket_url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
    let auth_http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth_health_addr = auth_health.local_addr().unwrap();
    let auth_url = format!("http://{}", auth_http.local_addr().unwrap());
    let auth_variables = [
        ("PUSH_DATABASE_URL", auth_database_url.as_str()),
        ("PUSH_REDIS_URL", redis_url.as_str()),
        ("PUSH_AUTH_PUBLIC_URL", "http://push.local"),
        ("PUSH_AUTH_TOKEN_AUDIENCE", socket_url.as_str()),
        // base64 of 32 bytes of 0x07 and of 0x09.
        (
            "PUSH_AUTH_SIGNING_KEY",
            "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=",
        ),
        (
            "PUSH_AUTH_LINK_KEY",
            "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=",
        ),
        ("PUSH_DEV_MAILBOX", "1"),
    ];
    let auth_config = push_auth::Config::from_lookup(|name| {
        auth_variables
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    })
    .unwrap();
    let auth = tokio::spawn(push_auth::serve(
        auth_config,
        auth_http,
        auth_health,
        shutdown.clone(),
    ));

    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_health_addr = message_svc_health.local_addr().unwrap();
    let message_svc_url = format!("http://{}", grpc.local_addr().unwrap());
    let message_svc = tokio::spawn(push_message_svc::serve(
        push_message_svc::Config {
            database_url,
            redis_url: redis_url.clone(),
            grpc_addr: grpc.local_addr().unwrap(),
            health_addr: message_svc_health.local_addr().unwrap(),
            store: StoreOptions::default(),
            dev_setup: true,
        },
        grpc,
        message_svc_health,
        shutdown.clone(),
    ));

    let gateway_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_health_addr = gateway_health.local_addr().unwrap();
    let gateway = tokio::spawn(push_gateway::serve(
        push_gateway::Config {
            message_svc_url: message_svc_url.clone(),
            redis_url,
            listen_addr: listener.local_addr().unwrap(),
            health_addr: gateway_health.local_addr().unwrap(),
            heartbeat: Duration::from_secs(30),
            dev_auth: true,
        },
        listener,
        gateway_health,
        shutdown.clone(),
    ));

    wait_until_ready("auth-svc", auth_health_addr, &auth).await;
    wait_until_ready("message-svc", message_svc_health_addr, &message_svc).await;
    wait_until_ready("the gateway", gateway_health_addr, &gateway).await;

    let target = Target {
        mailbox_url: auth_url.clone(),
        auth_url,
        setup_url: message_svc_url,
        socket_url,
        host: None,
    };
    tokio::time::timeout(Duration::from_secs(120), run(&target))
        .await
        .expect("the smoke test finishes within 120 s")
        .expect("the smoke test passes");

    shutdown.cancel();
    message_svc
        .await
        .unwrap()
        .expect("message-svc stops cleanly");
    gateway.await.unwrap().expect("the gateway stops cleanly");
    auth.await.unwrap().expect("auth-svc stops cleanly");
}

/// Polls `/readyz` of a server task until it answers 200; fails if the task stops first.
async fn wait_until_ready<T: std::fmt::Debug>(
    name: &str,
    health: SocketAddr,
    server: &JoinHandle<T>,
) {
    for _ in 0..240 {
        assert!(
            !server.is_finished(),
            "{name} stopped before it became ready"
        );
        let probe = tokio::time::timeout(Duration::from_secs(2), readyz_status(health));
        if matches!(probe.await, Ok(200)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{name} did not become ready within 60 s");
}

async fn readyz_status(addr: SocketAddr) -> u16 {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return 0;
    };
    let request = "GET /readyz HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n";
    if stream.write_all(request.as_bytes()).await.is_err() {
        return 0;
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).await.is_err() {
        return 0;
    }
    response
        .get(9..12)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}
