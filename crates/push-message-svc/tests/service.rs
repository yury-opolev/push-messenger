//! The whole service on real Postgres and Redis (Docker must be running): readiness, the
//! dev setup calls, the worker's account events (a revocation kicks the device on Redis),
//! a send that rings the recipient's Redis channel, sync, ack, and graceful shutdown.

use std::net::SocketAddr;
use std::time::Duration;

use prost::Message;
use push_message_svc::{Config, serve};
use push_proto::internal::v1::accounts_client::AccountsClient;
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AccountDevice, AckRequest, EditRequest, IdentifyRequest, Kick, KickReason, MakeContactsRequest,
    Notification, OpenDirectRequest, ReadUpToRequest, RegisterDeviceRequest, RegisterUserRequest,
    RequestCall, SendRequest, SyncRequest, notification::Kind,
};
use push_proto::v1::{
    self, Ack, ContactAccept, ContactRequest, ContentType, CreateGroup, CreateInvite, Edit,
    FetchHistory, GroupReceipts, ListConversations, ReadUpTo, RedeemInvite, Send,
    conversation_summary, edit, request, response,
};
use push_proto::{id_bytes, kick_channel, notify_channel};
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
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use uuid::Uuid;

/// The worker's token for the `Accounts` API.
const ACCOUNTS_TOKEN: &str = "accounts-token-for-the-service-tests";

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
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
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
        accounts_token: Some(ACCOUNTS_TOKEN.to_owned()),
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
    for _ in 0..240 {
        if running.server.is_finished() {
            let outcome = running.server.await;
            panic!("the service stopped before it became ready: {outcome:?}");
        }
        let probe = tokio::time::timeout(Duration::from_secs(2), get(running.health, "/readyz"));
        if matches!(probe.await, Ok((200, _))) {
            return running;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("the service did not become ready within 60 s");
}

#[tokio::test]
async fn shutdown_is_honoured_while_the_database_is_unreachable() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        database_url: "postgres://postgres:postgres@127.0.0.1:1/postgres".to_owned(),
        redis_url: "redis://127.0.0.1:1".to_owned(),
        grpc_addr: grpc.local_addr().unwrap(),
        health_addr: health.local_addr().unwrap(),
        store: StoreOptions::default(),
        accounts_token: None,
        dev_setup: false,
    };
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(config, grpc, health, shutdown.clone()));
    tokio::time::sleep(Duration::from_millis(500)).await;

    let started = std::time::Instant::now();
    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_secs(10), server)
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
async fn accounts_are_served_and_dev_setup_is_absent_unless_enabled() {
    let running = start(false).await;
    let url = format!("http://{}", running.grpc);
    let channel = Channel::from_shared(url.clone())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut messaging = MessagingClient::connect(url.clone()).await.unwrap();
    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
    let event = AccountDevice {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
    };
    let identify = IdentifyRequest {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
    };
    // Not relayed yet: worth retrying, unlike a revoked device.
    let status = messaging.identify(identify.clone()).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unavailable);

    // Without the worker's token (the gateway has none) the API refuses.
    let mut stranger = AccountsClient::new(channel.clone());
    let status = stranger.device_registered(event.clone()).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);

    let bearer: MetadataValue<_> = format!("Bearer {ACCOUNTS_TOKEN}").parse().unwrap();
    let mut accounts = AccountsClient::with_interceptor(channel, move |mut request: Request<()>| {
        request
            .metadata_mut()
            .insert("authorization", bearer.clone());
        Ok(request)
    });
    accounts.device_registered(event.clone()).await.unwrap();
    messaging.identify(identify.clone()).await.unwrap();

    // A revocation kicks the device through its Redis channel.
    let mut bell = redis::Client::open(running.redis_url.as_str())
        .unwrap()
        .get_async_pubsub()
        .await
        .unwrap();
    bell.subscribe(kick_channel(device)).await.unwrap();
    accounts.device_revoked(event).await.unwrap();
    let kicked = tokio::time::timeout(Duration::from_secs(5), bell.on_message().next())
        .await
        .expect("the device's channel rings within 5 s")
        .unwrap();
    let kick = Kick::decode(kicked.get_payload_bytes()).unwrap();
    assert_eq!(kick.reason(), KickReason::DeviceRevoked);
    let status = messaging.identify(identify).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::PermissionDenied);

    let mut setup = DevSetupClient::connect(url).await.unwrap();
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

/// The Accounts client the worker uses, with the internal token.
async fn accounts_client(
    url: String,
) -> AccountsClient<InterceptedService<Channel, impl Interceptor>> {
    let channel = Channel::from_shared(url).unwrap().connect().await.unwrap();
    let bearer: MetadataValue<_> = format!("Bearer {ACCOUNTS_TOKEN}").parse().unwrap();
    AccountsClient::with_interceptor(channel, move |mut request: Request<()>| {
        request
            .metadata_mut()
            .insert("authorization", bearer.clone());
        Ok(request)
    })
}

/// A request frame as `user`.
fn call(user: Uuid, body: request::Body) -> RequestCall {
    RequestCall {
        user_id: id_bytes(user),
        request: Some(v1::Request {
            request_id: 1,
            body: Some(body),
        }),
    }
}

#[tokio::test]
async fn the_client_requests_run_on_postgres() {
    let running = start(false).await;
    let url = format!("http://{}", running.grpc);
    let mut accounts = accounts_client(url.clone()).await;
    let mut messaging = MessagingClient::connect(url).await.unwrap();
    let (alice, bob, carol) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let devices = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
    for (user, device) in [alice, bob, carol].into_iter().zip(devices) {
        accounts
            .device_registered(AccountDevice {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
            })
            .await
            .unwrap();
    }

    // Alice invites Bob; Carol asks Alice, who accepts.
    let create = request::Body::CreateInvite(CreateInvite {
        expires_in_s: 600,
        max_uses: 1,
    });
    let Some(response::Body::Invite(invite)) = messaging
        .request(call(alice, create))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not an invite");
    };
    let redeem = request::Body::RedeemInvite(RedeemInvite { code: invite.code });
    messaging.request(call(bob, redeem)).await.unwrap();
    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(alice),
    });
    messaging.request(call(carol, ask)).await.unwrap();
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(carol),
    });
    messaging.request(call(alice, accept)).await.unwrap();

    let group = request::Body::CreateGroup(CreateGroup {
        title: "Trip".to_owned(),
        member_ids: vec![id_bytes(bob), id_bytes(carol)],
    });
    let Some(response::Body::GroupCreated(created)) = messaging
        .request(call(alice, group))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not a created group");
    };
    let sent = messaging
        .send(SendRequest {
            user_id: id_bytes(alice),
            device_id: id_bytes(devices[0]),
            send: Some(Send {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: created.conversation_id.clone(),
                content_type: ContentType::Text.into(),
                body: b"helo".to_vec(),
                client_ts: None,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    messaging
        .edit(EditRequest {
            user_id: id_bytes(alice),
            device_id: id_bytes(devices[0]),
            edit: Some(Edit {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: created.conversation_id.clone(),
                target: Some(edit::Target::TargetMessageId(sent.message_id)),
                body: b"hello".to_vec(),
                client_ts: None,
            }),
        })
        .await
        .unwrap();
    let read = messaging
        .read_up_to(ReadUpToRequest {
            user_id: id_bytes(bob),
            read: Some(ReadUpTo {
                conversation_id: created.conversation_id.clone(),
                seq: sent.seq,
            }),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(read.moved);

    let fetch = request::Body::FetchHistory(FetchHistory {
        conversation_id: created.conversation_id.clone(),
        before_seq: None,
        limit: 0,
    });
    let Some(response::Body::History(page)) = messaging
        .request(call(carol, fetch))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not a history page");
    };
    let bodies: Vec<Option<&[u8]>> = page
        .entries
        .iter()
        .map(|entry| entry.body.as_deref())
        .collect();
    assert_eq!(bodies.len(), 2, "the created entry and the message");
    assert_eq!(bodies[1], Some(b"hello".as_slice()));

    let list = request::Body::ListConversations(ListConversations::default());
    let Some(response::Body::Conversations(listed)) = messaging
        .request(call(alice, list))
        .await
        .unwrap()
        .into_inner()
        .body
    else {
        panic!("not a conversation list");
    };
    assert_eq!(listed.conversations.len(), 1);
    assert_eq!(listed.conversations[0].title.as_deref(), Some("Trip"));
    assert_eq!(
        listed.conversations[0].receipts,
        Some(conversation_summary::Receipts::Group(GroupReceipts {
            all_delivered_seq: 1,
            all_read_seq: 1,
        })),
        "Bob read the message, Carol did not: only the created entry counts as read by all"
    );

    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
}
