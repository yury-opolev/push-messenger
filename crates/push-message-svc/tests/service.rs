//! The whole service on real Postgres and Redis (Docker must be running): readiness, the
//! dev setup calls, a send that rings the recipient's Redis channel, sync, ack, and
//! graceful shutdown.

use std::net::SocketAddr;
use std::time::Duration;

use prost::Message;
use push_message_svc::{Config, serve};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AckRequest, MakeContactsRequest, Notification, OpenDirectRequest, RegisterDeviceRequest,
    RegisterUserRequest, SendRequest, SyncRequest, notification::Kind,
};
use push_proto::v1::{Ack, ContentType, Send};
use push_proto::{id_bytes, notify_channel};
use push_store::StoreOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Running {
    _postgres: ContainerAsync<Postgres>,
    _redis: ContainerAsync<Redis>,
    redis_url: String,
    grpc: SocketAddr,
    health: SocketAddr,
    shutdown: CancellationToken,
    server: JoinHandle<anyhow::Result<()>>,
}

/// Starts Postgres, Redis and the service on free local ports.
async fn start(dev_setup: bool) -> Running {
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
    let postgres_port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();
    let redis_url = format!("redis://127.0.0.1:{redis_port}");

    let grpc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (grpc, health) = (
        grpc_listener.local_addr().unwrap(),
        health_listener.local_addr().unwrap(),
    );
    let config = Config {
        database_url: format!("postgres://postgres:postgres@127.0.0.1:{postgres_port}/postgres"),
        redis_url: redis_url.clone(),
        grpc_addr: grpc,
        health_addr: health,
        store: StoreOptions::default(),
        dev_setup,
    };
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        config,
        grpc_listener,
        health_listener,
        shutdown.clone(),
    ));
    let running = Running {
        _postgres: postgres,
        _redis: redis,
        redis_url,
        grpc,
        health,
        shutdown,
        server,
    };
    for _ in 0..120 {
        if get(running.health, "/readyz").await.0 == 200 {
            return running;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("the service did not become ready within 30 s");
}

/// A minimal HTTP/1.1 GET: the status code and the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return (0, String::new());
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    (status, body)
}

#[tokio::test]
async fn a_message_goes_from_send_to_ack_and_rings_the_recipient() {
    let running = start(true).await;
    assert_eq!(
        get(running.health, "/healthz").await,
        (200, "ok".to_owned())
    );
    assert_eq!(
        get(running.health, "/readyz").await,
        (200, "ready".to_owned())
    );

    let url = format!("http://{}", running.grpc);
    let mut setup = DevSetupClient::connect(url.clone()).await.unwrap();
    let mut messaging = MessagingClient::connect(url).await.unwrap();
    let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
    let (alice_device, bob_device) = (Uuid::now_v7(), Uuid::now_v7());
    for (user, device) in [(alice, alice_device), (bob, bob_device)] {
        setup
            .register_user(RegisterUserRequest {
                user_id: id_bytes(user),
            })
            .await
            .unwrap();
        setup
            .register_device(RegisterDeviceRequest {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
            })
            .await
            .unwrap();
    }
    setup
        .make_contacts(MakeContactsRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap();
    let conversation = setup
        .open_direct(OpenDirectRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap()
        .into_inner()
        .conversation_id;

    let mut bell = redis::Client::open(running.redis_url.as_str())
        .unwrap()
        .get_async_pubsub()
        .await
        .unwrap();
    bell.subscribe(notify_channel(bob)).await.unwrap();

    let ack = messaging
        .send(SendRequest {
            user_id: id_bytes(alice),
            device_id: id_bytes(alice_device),
            send: Some(Send {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: conversation.clone(),
                content_type: ContentType::Text.into(),
                body: b"hello".to_vec(),
                client_ts: None,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ack.seq, 1);

    let rung = tokio::time::timeout(Duration::from_secs(5), bell.on_message().next())
        .await
        .expect("Bob's channel rings within 5 s")
        .unwrap();
    let notification = Notification::decode(rung.get_payload_bytes()).unwrap();
    assert!(matches!(notification.kind, Some(Kind::EntriesAvailable(_))));

    let batch = messaging
        .sync(SyncRequest {
            device_id: id_bytes(bob_device),
            limit: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(batch.entries.len(), 1);
    assert_eq!(batch.entries[0].body.as_deref(), Some(b"hello".as_slice()));
    let acked = messaging
        .ack(AckRequest {
            device_id: id_bytes(bob_device),
            ack: Some(Ack {
                conversation_id: conversation,
                seq: 1,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(acked.moved);

    running.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), running.server)
        .await
        .expect("the service stops within 10 s")
        .unwrap()
        .expect("a clean stop");
}

#[tokio::test]
async fn dev_setup_is_absent_unless_enabled() {
    let running = start(false).await;
    let mut setup = DevSetupClient::connect(format!("http://{}", running.grpc))
        .await
        .unwrap();
    let status = setup
        .register_user(RegisterUserRequest {
            user_id: id_bytes(Uuid::now_v7()),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
}
