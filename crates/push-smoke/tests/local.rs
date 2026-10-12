//! The walking skeleton on this machine: Postgres and Redis in containers (Docker must be
//! running), message-svc and the gateway in this process, and the smoke test against
//! them over real gRPC and WebSockets.

use std::time::Duration;

use push_smoke::{Target, run};
use push_store::StoreOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::net::TcpListener;
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

    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
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

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket_url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
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

    // message-svc migrates the database before it serves; the smoke test's first call
    // waits for it.
    let target = Target {
        setup_url: message_svc_url,
        socket_url,
        host: None,
    };
    let mut result = run(&target).await;
    for _ in 0..20 {
        if result.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        result = run(&target).await;
    }
    result.expect("the smoke test passes");

    shutdown.cancel();
    message_svc
        .await
        .unwrap()
        .expect("message-svc stops cleanly");
    gateway.await.unwrap().expect("the gateway stops cleanly");
}
