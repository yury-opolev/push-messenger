//! The gateway over real WebSockets, in front of message-svc's API (on the in-memory
//! store) and a real Redis (Docker must be running).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use push_domain::{InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock};
use push_gateway::{Config, serve};
use push_message_svc::{DevSetupApi, MessagingApi, RedisNotifier};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::dev_setup_server::DevSetupServer;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::messaging_server::{self, MessagingServer};
use push_proto::internal::v1::{
    AckRequest, AckResponse, IdentifyRequest, IdentifyResponse, MakeContactsRequest,
    OpenDirectRequest, RegisterDeviceRequest, RegisterUserRequest, SendRequest, SyncRequest,
    SyncResponse,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, ContentType, Identify, Ping, Pong, Send, SendAck};
use push_proto::{decode_frame, encode_frame, id_bytes};
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Api = MessagingApi<InMemoryStore, SystemClock, RedisNotifier>;

/// message-svc's `Messaging` service, whose first `sync_failures` syncs fail as if the
/// service were restarting.
struct FlakySync {
    api: Api,
    sync_failures: Mutex<u32>,
}

#[tonic::async_trait]
impl messaging_server::Messaging for FlakySync {
    async fn identify(
        &self,
        request: Request<IdentifyRequest>,
    ) -> Result<Response<IdentifyResponse>, Status> {
        self.api.identify(request).await
    }

    async fn send(&self, request: Request<SendRequest>) -> Result<Response<SendAck>, Status> {
        self.api.send(request).await
    }

    async fn sync(&self, request: Request<SyncRequest>) -> Result<Response<SyncResponse>, Status> {
        let failing = {
            let mut left = self.sync_failures.lock().unwrap();
            let failing = *left > 0;
            *left = left.saturating_sub(1);
            failing
        };
        if failing {
            return Err(Status::unavailable("message-svc is restarting"));
        }
        self.api.sync(request).await
    }

    async fn ack(&self, request: Request<AckRequest>) -> Result<Response<AckResponse>, Status> {
        self.api.ack(request).await
    }
}

struct Stack {
    _redis: ContainerAsync<Redis>,
    message_svc: String,
    gateway: SocketAddr,
    shutdown: CancellationToken,
    server: JoinHandle<anyhow::Result<()>>,
}

/// Redis, message-svc's API on the in-memory store, and the gateway, on free ports.
async fn stack(dev_auth: bool) -> Stack {
    stack_with(dev_auth, 0).await
}

/// As [`stack`], with message-svc failing its first `sync_failures` syncs.
async fn stack_with(dev_auth: bool, sync_failures: u32) -> Stack {
    let redis = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start Redis (is Docker running?)");
    let redis_url = format!(
        "redis://127.0.0.1:{}",
        redis.get_host_port_ipv4(6379).await.unwrap()
    );

    let notifier = RedisNotifier::connect(&redis_url).await.unwrap();
    let messaging = Arc::new(Messaging::new(
        InMemoryStore::default(),
        SystemClock,
        notifier,
    ));
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc = format!("http://{}", grpc.local_addr().unwrap());
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(MessagingServer::new(FlakySync {
                api: MessagingApi::new(Arc::clone(&messaging)),
                sync_failures: Mutex::new(sync_failures),
            }))
            .add_service(DevSetupServer::new(DevSetupApi::new(messaging)))
            .serve_with_incoming(TcpListenerStream::new(grpc)),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = listener.local_addr().unwrap();
    let config = Config {
        message_svc_url: message_svc.clone(),
        redis_url,
        listen_addr: gateway,
        health_addr: health.local_addr().unwrap(),
        heartbeat: Duration::from_secs(30),
        dev_auth,
    };
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(config, listener, health, shutdown.clone()));
    Stack {
        _redis: redis,
        message_svc,
        gateway,
        shutdown,
        server,
    }
}

/// A user with one device, registered through the dev setup API.
async fn person(stack: &Stack) -> (Uuid, Uuid) {
    let mut setup = DevSetupClient::connect(stack.message_svc.clone())
        .await
        .unwrap();
    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
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
    (user, device)
}

async fn connect(stack: &Stack) -> Socket {
    let (socket, _) = connect_async(format!("ws://{}/v1/socket", stack.gateway))
        .await
        .expect("connect to the gateway");
    socket
}

async fn send(socket: &mut Socket, body: Body) {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .unwrap();
}

fn identify(user: Uuid, device: Uuid) -> Body {
    Body::Identify(Identify {
        token: String::new(),
        dev_user_id: id_bytes(user),
        dev_device_id: id_bytes(device),
    })
}

/// The next frame, within 5 s.
async fn next(socket: &mut Socket) -> Body {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("a frame within 5 s")
            .expect("the connection is open")
            .expect("a WebSocket message");
        match message {
            Message::Binary(bytes) => return decode_frame(&bytes).expect("a valid frame"),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("expected a frame, got {other:?}"),
        }
    }
}

/// The close code the gateway sends, within 10 s.
async fn close_code(socket: &mut Socket) -> u16 {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a close within 10 s");
        match message {
            Some(Ok(Message::Close(Some(frame)))) => return frame.code.into(),
            Some(Ok(_)) => {}
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_message_reaches_the_other_device_and_the_sender_gets_its_ack() {
    let stack = stack(true).await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    let mut setup = DevSetupClient::connect(stack.message_svc.clone())
        .await
        .unwrap();
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

    let mut bob_socket = connect(&stack).await;
    send(&mut bob_socket, identify(bob, bob_device)).await;
    let Body::Ready(ready) = next(&mut bob_socket).await else {
        panic!("expected Ready");
    };
    assert_eq!(ready.user_id, id_bytes(bob));
    assert_eq!(ready.device_id, id_bytes(bob_device));
    assert_eq!(ready.heartbeat_ms, 30_000);

    let mut alice_socket = connect(&stack).await;
    send(&mut alice_socket, identify(alice, alice_device)).await;
    assert!(matches!(next(&mut alice_socket).await, Body::Ready(_)));
    let client_msg_id = id_bytes(Uuid::now_v7());
    send(
        &mut alice_socket,
        Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.clone(),
            content_type: ContentType::Text.into(),
            body: b"hello Bob".to_vec(),
            client_ts: None,
        }),
    )
    .await;
    let ack = loop {
        // Alice's own device is also told about her message.
        match next(&mut alice_socket).await {
            Body::SendAck(ack) => break ack,
            Body::Deliver(_) => {}
            other => panic!("expected SendAck, got {other:?}"),
        }
    };
    assert_eq!(ack.client_msg_id, client_msg_id);
    assert_eq!(ack.seq, 1);

    let Body::Deliver(entry) = next(&mut bob_socket).await else {
        panic!("expected Deliver");
    };
    assert_eq!(entry.conversation_id, conversation);
    assert_eq!(entry.seq, 1);
    assert_eq!(entry.sender_id, id_bytes(alice));
    assert_eq!(entry.body.as_deref(), Some(b"hello Bob".as_slice()));

    send(
        &mut bob_socket,
        Body::Ack(Ack {
            conversation_id: conversation,
            seq: 1,
        }),
    )
    .await;
    // Frames are handled in order: a Pong after the Ack means the Ack succeeded.
    send(&mut bob_socket, Body::Ping(Ping {})).await;
    assert_eq!(next(&mut bob_socket).await, Body::Pong(Pong {}));
}

#[tokio::test]
async fn without_dev_auth_identify_is_closed_with_4001() {
    let stack = stack(false).await;
    let (user, device) = person(&stack).await;
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(user, device)).await;
    assert_eq!(close_code(&mut socket).await, 4001);
}

#[tokio::test]
async fn a_device_of_another_user_is_closed_with_4001() {
    let stack = stack(true).await;
    let (alice, _) = person(&stack).await;
    let (_, bob_device) = person(&stack).await;
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(alice, bob_device)).await;
    assert_eq!(close_code(&mut socket).await, 4001);

    // A device message-svc has not heard of yet (not relayed) is not refused for good:
    // message-svc answers UNAVAILABLE, and the client retries (Plan 8 gives it its own code).
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(alice, Uuid::now_v7())).await;
    assert_eq!(close_code(&mut socket).await, 1011);
}

#[tokio::test]
async fn a_connection_that_never_identifies_is_closed_after_5_seconds() {
    let stack = stack(true).await;
    let mut socket = connect(&stack).await;
    assert_eq!(close_code(&mut socket).await, 4001);
}

#[tokio::test]
async fn shutdown_closes_connections_with_4005() {
    let stack = stack(true).await;
    let (user, device) = person(&stack).await;
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(user, device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));

    stack.shutdown.cancel();
    assert_eq!(close_code(&mut socket).await, 4005);
    tokio::time::timeout(Duration::from_secs(15), stack.server)
        .await
        .expect("the gateway stops within 15 s")
        .unwrap()
        .expect("a clean stop");
}

/// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
/// messages from Alice in it, sent through message-svc before anyone connects.
async fn chat_with_backlog(stack: &Stack, count: u32) -> ((Uuid, Uuid), (Uuid, Uuid), Vec<u8>) {
    let (alice, alice_device) = person(stack).await;
    let (bob, bob_device) = person(stack).await;
    let mut setup = DevSetupClient::connect(stack.message_svc.clone())
        .await
        .unwrap();
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
    let mut messaging = MessagingClient::connect(stack.message_svc.clone())
        .await
        .unwrap();
    for n in 0..count {
        messaging
            .send(SendRequest {
                user_id: id_bytes(alice),
                device_id: id_bytes(alice_device),
                send: Some(Send {
                    client_msg_id: id_bytes(Uuid::now_v7()),
                    conversation_id: conversation.clone(),
                    content_type: ContentType::Text.into(),
                    body: format!("message {n}").into_bytes(),
                    client_ts: None,
                }),
            })
            .await
            .unwrap();
    }
    ((alice, alice_device), (bob, bob_device), conversation)
}

#[tokio::test]
async fn a_backlog_larger_than_one_sync_batch_is_delivered_as_the_device_acks() {
    let stack = stack(true).await;
    let backlog = MAX_SYNC_BATCH + 1;
    let (_alice, (bob, bob_device), conversation) = chat_with_backlog(&stack, backlog).await;

    let mut socket = connect(&stack).await;
    send(&mut socket, identify(bob, bob_device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
    for expected in 1..=i64::from(MAX_SYNC_BATCH) {
        let Body::Deliver(entry) = next(&mut socket).await else {
            panic!("expected Deliver {expected}");
        };
        assert_eq!(entry.seq, expected);
    }

    // Nothing rings Bob's channel again: only his ack can bring the rest.
    send(
        &mut socket,
        Body::Ack(Ack {
            conversation_id: conversation,
            seq: i64::from(MAX_SYNC_BATCH),
        }),
    )
    .await;
    let Body::Deliver(entry) = next(&mut socket).await else {
        panic!("expected the last Deliver");
    };
    assert_eq!(entry.seq, i64::from(backlog));
}

#[tokio::test]
async fn a_failed_sync_is_retried() {
    let stack = stack_with(true, 1).await;
    let (_alice, (bob, bob_device), _conversation) = chat_with_backlog(&stack, 1).await;

    let mut socket = connect(&stack).await;
    send(&mut socket, identify(bob, bob_device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
    // The sync after Ready fails; nothing rings Bob's channel, so only a retry delivers.
    let Body::Deliver(entry) = next(&mut socket).await else {
        panic!("expected Deliver");
    };
    assert_eq!(entry.seq, 1);
}
