//! The whole system on this machine: Postgres and Redis in containers (Docker must be
//! running), auth-svc, the worker, message-svc and the gateway in this process, wired as
//! the chart wires them (each internal API behind its own token), and the smoke test
//! against them as a client sees them: auth-svc's HTTP API and the gateway's WebSocket.

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

    // auth-svc and the worker have databases of their own in the same server, as in the
    // chart; the worker's role may also drain auth-svc's outbox, which auth-svc grants.
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    for statement in [
        "CREATE DATABASE push_auth",
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement).execute(&admin).await.unwrap();
    }
    admin.close().await;
    let (server_url, _) = database_url.rsplit_once('/').unwrap();
    let auth_database_url = format!("{server_url}/push_auth");
    let worker_server = format!(
        "postgres://push_worker:worker-secret@127.0.0.1:{}",
        postgres.get_host_port_ipv4(5432).await.unwrap()
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket_url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
    let auth_http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth_health_addr = auth_health.local_addr().unwrap();
    let auth_url = format!("http://{}", auth_http.local_addr().unwrap());
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_url = format!("http://{}", grpc.local_addr().unwrap());
    // auth-svc's token for message-svc's Directory API, as the chart's Secret holds it.
    // message-svc refuses it equal to the Accounts token.
    let directory_token = "directory-token-for-the-local-smoke-test";
    // base64 of 32 bytes of 0x07: auth-svc's signing key, which the smoke test also gets
    // to sign tokens the gateway must refuse.
    let signing_key = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";
    let auth_variables = [
        ("PUSH_DATABASE_URL", auth_database_url.as_str()),
        ("PUSH_REDIS_URL", redis_url.as_str()),
        ("PUSH_AUTH_PUBLIC_URL", "http://push.local"),
        ("PUSH_AUTH_TOKEN_AUDIENCE", socket_url.as_str()),
        ("PUSH_AUTH_SIGNING_KEY", signing_key),
        // base64 of 32 bytes of 0x09.
        (
            "PUSH_AUTH_LINK_KEY",
            "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=",
        ),
        ("PUSH_AUTH_WORKER_ROLE", "push_worker"),
        ("PUSH_MESSAGE_SVC_URL", message_svc_url.as_str()),
        ("PUSH_DIRECTORY_TOKEN", directory_token),
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

    let message_svc_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_health_addr = message_svc_health.local_addr().unwrap();
    // The worker's token for message-svc's Accounts API, as the chart's Secret holds it.
    let accounts_token = "accounts-token-for-the-local-smoke-test".to_owned();
    let message_svc = tokio::spawn(push_message_svc::serve(
        push_message_svc::Config {
            database_url,
            redis_url: redis_url.clone(),
            grpc_addr: grpc.local_addr().unwrap(),
            health_addr: message_svc_health.local_addr().unwrap(),
            store: StoreOptions::default(),
            accounts_token: Some(accounts_token.clone()),
            directory_token: Some(directory_token.to_owned()),
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
            redis_url: redis_url.clone(),
            auth_url: auth_url.clone(),
            token_audience: socket_url.clone(),
            listen_addr: listener.local_addr().unwrap(),
            health_addr: gateway_health.local_addr().unwrap(),
            heartbeat: Duration::from_secs(30),
            groups: false,
            client_ip_header: None,
        },
        listener,
        gateway_health,
        shutdown.clone(),
    ));

    let worker_http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker_health_addr = worker_health.local_addr().unwrap();
    let worker_url = format!("http://{}", worker_http.local_addr().unwrap());
    let worker = tokio::spawn(push_worker::serve(
        push_worker::Config {
            database_url: format!("{worker_server}/push_worker"),
            auth_database_url: format!("{worker_server}/push_auth"),
            message_svc_url: message_svc_url.clone(),
            accounts_token,
            sender: push_worker::Sender::Log,
            from: String::new(),
            product_name: "Push Messenger".to_owned(),
            webhook_secret: None,
            poll: Duration::from_millis(250),
            unverified_after: Duration::from_secs(7 * 24 * 3600),
            retention: Duration::from_secs(30 * 24 * 3600),
            http_addr: worker_http.local_addr().unwrap(),
            health_addr: worker_health_addr,
            store: StoreOptions::default(),
            dev_mailbox: true,
        },
        worker_http,
        worker_health,
        shutdown.clone(),
    ));

    wait_until_ready("auth-svc", auth_health_addr, &auth).await;
    wait_until_ready("the worker", worker_health_addr, &worker).await;
    wait_until_ready("message-svc", message_svc_health_addr, &message_svc).await;
    wait_until_ready("the gateway", gateway_health_addr, &gateway).await;

    let target = Target {
        mailbox_url: worker_url,
        auth_url,
        socket_url,
        host: None,
        signing_key: signing_key.to_owned(),
    };
    tokio::time::timeout(Duration::from_secs(180), run(&target))
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
    worker.await.unwrap().expect("the worker stops cleanly");
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
