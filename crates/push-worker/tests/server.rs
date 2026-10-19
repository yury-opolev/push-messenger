//! The worker as it runs (Docker must be running): auth-svc's and its own databases in
//! one Postgres, message-svc in this process; readiness, the relay, email through the log
//! sender into the DEV ONLY mailbox, a webhook, the cleanup jobs, and graceful shutdown.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use push_auth_core::outbox::{Email, Event};
use push_domain::{InMemoryStore, Messaging, RecordingNotifier, SystemClock};
use push_email::ResendWebhooks;
use push_message_svc::{AccountsApi, InternalToken, MessagingApi};
use push_proto::id_bytes;
use push_proto::internal::v1::IdentifyRequest;
use push_proto::internal::v1::accounts_server::AccountsServer;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_store::StoreOptions;
use push_worker::{Config, Sender, serve};
use serde_json::{Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
/// The shared token message-svc requires for its `Accounts` API.
const ACCOUNTS_TOKEN: &str = "accounts-token-for-the-server-tests";

fn config(database_url: String, auth_database_url: String, message_svc_url: String) -> Config {
    Config {
        database: push_store::DatabaseTarget::Url(database_url),
        auth_database: push_store::DatabaseTarget::Url(auth_database_url),
        azure: None,
        run_migrations: true,
        message_svc_url,
        accounts_token: ACCOUNTS_TOKEN.to_owned(),
        sender: Sender::Log,
        from: String::new(),
        product_name: "Push Messenger".to_owned(),
        webhook_secret: Some(SECRET.to_owned()),
        poll: Duration::from_millis(100),
        unverified_after: Duration::from_secs(7 * 24 * 3600),
        retention: Duration::from_secs(30 * 24 * 3600),
        http_addr: "127.0.0.1:0".parse().unwrap(),
        health_addr: "127.0.0.1:0".parse().unwrap(),
        store: StoreOptions::default(),
        dev_mailbox: true,
    }
}

/// message-svc's `Messaging` and `Accounts` APIs on an in-memory store; its URL.
async fn message_svc() -> String {
    let messaging = Arc::new(Messaging::new(
        InMemoryStore::default(),
        SystemClock,
        Arc::new(RecordingNotifier::default()),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(MessagingServer::new(MessagingApi::new(Arc::clone(
                &messaging,
            ))))
            .add_service(AccountsServer::with_interceptor(
                AccountsApi::new(messaging),
                InternalToken::accounts(Some(ACCOUNTS_TOKEN.to_owned())),
            ))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    url
}

/// A minimal HTTP/1.1 request: the status code and the body.
async fn request(addr: SocketAddr, head: &str, body: &[u8]) -> (u16, String) {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return (0, String::new());
    };
    let request = format!(
        "{head}\r\nHost: test\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    (status, body)
}

/// Polls `check` every 100 ms until it holds; fails after 15 s.
async fn eventually<F, Fut>(what: &str, check: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(15);
    while !check().await {
        assert!(Instant::now() < deadline, "{what} within 15 s");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn the_worker_relays_sends_takes_webhooks_and_stops_cleanly() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let server = format!("127.0.0.1:{port}");
    let admin = PgPool::connect(&format!("postgres://postgres:postgres@{server}/postgres"))
        .await
        .unwrap();
    for statement in [
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_auth",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement).execute(&admin).await.unwrap();
    }
    admin.close().await;
    // auth-svc migrates and grants the worker's role its access at start-up.
    let auth_url = format!("postgres://postgres:postgres@{server}/push_auth");
    push_auth_core::migrate_database(&auth_url).await.unwrap();
    push_auth_core::grant_worker_access(&auth_url, "push_worker")
        .await
        .unwrap();
    let auth = PgPool::connect(&auth_url).await.unwrap();
    let message_svc_url = message_svc().await;

    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (http_addr, health_addr) = (http.local_addr().unwrap(), health.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    let worker = tokio::spawn(serve(
        config(
            format!("postgres://push_worker:worker-secret@{server}/push_worker"),
            format!("postgres://push_worker:worker-secret@{server}/push_auth"),
            message_svc_url.clone(),
        ),
        http,
        health,
        shutdown.clone(),
    ));
    assert_eq!(
        request(health_addr, "GET /healthz HTTP/1.1", b"").await.0,
        200
    );
    eventually("the worker is ready", || async {
        request(health_addr, "GET /readyz HTTP/1.1", b"").await.0 == 200
    })
    .await;

    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
    for event in [
        Event::UserVerified { user_id: user },
        Event::DeviceRegistered {
            user_id: user,
            device_id: device,
        },
        Event::EmailSend {
            user_id: user,
            to: "alice@example.com".to_owned(),
            email: Email::VerifyEmail {
                link: "http://push.test/v1/auth/verify?token=t".to_owned(),
                handle: "alice".to_owned(),
            },
        },
    ] {
        sqlx::query("INSERT INTO auth.outbox (event_id, kind, payload) VALUES ($1, $2, $3)")
            .bind(Uuid::now_v7())
            .bind(event.kind())
            .bind(event.payload())
            .execute(&auth)
            .await
            .unwrap();
    }
    let messaging = MessagingClient::connect(message_svc_url).await.unwrap();
    eventually("message-svc knows the device", || {
        let mut messaging = messaging.clone();
        async move {
            messaging
                .identify(IdentifyRequest {
                    user_id: id_bytes(user),
                    device_id: id_bytes(device),
                    // No password reset in this test: any issue time passes.
                    issued_at: 0,
                })
                .await
                .is_ok()
        }
    })
    .await;
    eventually("the log sender delivered the email", || async {
        let (status, body) = request(
            http_addr,
            "GET /dev/mailbox?to=alice@example.com HTTP/1.1",
            b"",
        )
        .await;
        status == 200
            && serde_json::from_str::<Value>(&body).unwrap()["emails"][0]["template"]
                == "verify_email"
    })
    .await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.outbox")
        .fetch_one(&auth)
        .await
        .unwrap();
    assert_eq!(left, 0);

    let body = json!({
        "type": "email.complained",
        "created_at": "2026-10-13T10:00:00.000Z",
        "data": {"email_id": "re-1", "to": ["carol@example.com"]},
    })
    .to_string();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let signature = ResendWebhooks::new(SECRET)
        .unwrap()
        .sign("msg_1", timestamp, body.as_bytes());
    let head = format!(
        "POST /v1/webhooks/resend HTTP/1.1\r\nContent-Type: application/json\r\n\
         svix-id: msg_1\r\nsvix-timestamp: {timestamp}\r\nsvix-signature: {signature}"
    );
    assert_eq!(request(http_addr, &head, body.as_bytes()).await.0, 204);
    let unsigned = "POST /v1/webhooks/resend HTTP/1.1\r\nContent-Type: application/json";
    assert_eq!(request(http_addr, unsigned, body.as_bytes()).await.0, 401);

    let worker_db = PgPool::connect(&format!(
        "postgres://push_worker:worker-secret@{server}/push_worker"
    ))
    .await
    .unwrap();
    let suppressed: i64 = sqlx::query_scalar("SELECT count(*) FROM worker.email_suppression")
        .fetch_one(&worker_db)
        .await
        .unwrap();
    assert_eq!(suppressed, 1);
    eventually("the cleanups ran and wait for their next turn", || async {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM worker.job WHERE kind LIKE 'cleanup.%' \
             AND attempts = 0 AND dead_at IS NULL AND run_after > now() + interval '30 minutes'",
        )
        .fetch_one(&worker_db)
        .await
        .unwrap();
        waiting == 3
    })
    .await;

    let started = Instant::now();
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("the worker stops within 10 s")
        .unwrap()
        .expect("a clean stop");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn shutdown_is_honoured_while_the_databases_are_unreachable() {
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health_addr = health.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let worker = tokio::spawn(serve(
        config(
            "postgres://push:secret@127.0.0.1:1/push_worker".to_owned(),
            "postgres://push:secret@127.0.0.1:1/push_auth".to_owned(),
            "http://127.0.0.1:1".to_owned(),
        ),
        http,
        health,
        shutdown.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        request(health_addr, "GET /readyz HTTP/1.1", b"").await.0,
        503
    );

    let started = Instant::now();
    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("serve did not stop")
        .unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shutdown took {:?}",
        started.elapsed()
    );
}
