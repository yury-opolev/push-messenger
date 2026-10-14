//! The gateway over real WebSockets, in front of message-svc's API (on the in-memory
//! store) and a real Redis (Docker must be running).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use push_auth::tokens::AccessTokens;
use push_domain::{
    ConversationId, DeviceId, InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock, UserId,
};
use push_gateway::{Config, serve};
use push_message_svc::{MessagingApi, RedisNotifier};
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::messaging_server::{self, MessagingServer};
use push_proto::internal::v1::{
    AckRequest, AckResponse, DeleteRequest, EditRequest, IdentifyRequest, IdentifyResponse,
    ReadUpToRequest, ReadUpToResponse, RequestCall, SendRequest, SyncRequest, SyncResponse,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, AuthExpiring, ContactEvent, ContactState, ContentType, DirectReceipt, Identify, Ping,
    Pong, ReAuth, ReadPosition, Receipt, Send, SendAck, receipt,
};
use push_proto::{decode_frame, encode_frame, id, id_bytes};
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

    async fn edit(&self, request: Request<EditRequest>) -> Result<Response<SendAck>, Status> {
        self.api.edit(request).await
    }

    async fn delete(&self, request: Request<DeleteRequest>) -> Result<Response<SendAck>, Status> {
        self.api.delete(request).await
    }

    async fn read_up_to(
        &self,
        request: Request<ReadUpToRequest>,
    ) -> Result<Response<ReadUpToResponse>, Status> {
        self.api.read_up_to(request).await
    }

    async fn request(
        &self,
        request: Request<RequestCall>,
    ) -> Result<Response<push_proto::v1::Response>, Status> {
        self.api.request(request).await
    }
}

struct Stack {
    _redis: ContainerAsync<Redis>,
    /// message-svc's domain, to set up users, contacts and conversations directly.
    messaging: Arc<Messaging<InMemoryStore, SystemClock, RedisNotifier>>,
    message_svc: String,
    /// auth-svc's token issuer, whose key set the gateway fetches.
    issuer: AccessTokens,
    gateway: SocketAddr,
    shutdown: CancellationToken,
    server: JoinHandle<anyhow::Result<()>>,
}

/// The URL the gateway names itself by: the audience of its tokens.
const AUDIENCE: &str = "ws://push.test/v1/socket";

/// Redis, message-svc's API on the in-memory store, auth-svc's key set, and the gateway,
/// on free ports.
async fn stack() -> Stack {
    stack_with(0).await
}

/// As [`stack`], with message-svc failing its first `sync_failures` syncs.
async fn stack_with(sync_failures: u32) -> Stack {
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
            .serve_with_incoming(TcpListenerStream::new(grpc)),
    );

    // auth-svc's key set, as `GET /v1/auth/jwks` serves it.
    let issuer = AccessTokens::from_seed(&[7; 32], AUDIENCE).unwrap();
    let jwks = issuer.jwks();
    let keys = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth_url = format!("http://{}", keys.local_addr().unwrap());
    let routes = axum::Router::new().route(
        "/v1/auth/jwks",
        axum::routing::get(move || async move { axum::Json(jwks) }),
    );
    tokio::spawn(axum::serve(keys, routes).into_future());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = listener.local_addr().unwrap();
    let config = Config {
        message_svc_url: message_svc.clone(),
        redis_url,
        auth_url,
        token_audience: AUDIENCE.to_owned(),
        listen_addr: gateway,
        health_addr: health.local_addr().unwrap(),
        heartbeat: Duration::from_secs(30),
    };
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(config, listener, health, shutdown.clone()));
    Stack {
        _redis: redis,
        messaging,
        message_svc,
        issuer,
        gateway,
        shutdown,
        server,
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A user with one device, relayed to message-svc as the worker relays them.
async fn person(stack: &Stack) -> (Uuid, Uuid) {
    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
    stack
        .messaging
        .device_registered(DeviceId(device), UserId(user))
        .await
        .unwrap();
    (user, device)
}

/// Makes two users contacts and opens their direct conversation.
async fn direct_chat(stack: &Stack, alice: Uuid, bob: Uuid) -> Vec<u8> {
    let (alice, bob) = (UserId(alice), UserId(bob));
    stack.messaging.contact_request(alice, bob).await.unwrap();
    stack.messaging.contact_accept(bob, alice).await.unwrap();
    let conversation = stack.messaging.open_direct(alice, bob).await.unwrap();
    id_bytes(conversation.0)
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

/// An `Identify` with an access token issued now.
fn identify(stack: &Stack, user: Uuid, device: Uuid) -> Body {
    identify_issued(stack, user, device, now())
}

/// An `Identify` with an access token issued at `issued_at` (seconds); it expires 15
/// minutes later.
fn identify_issued(stack: &Stack, user: Uuid, device: Uuid, issued_at: i64) -> Body {
    let (token, _) = stack.issuer.issue(user, device, issued_at);
    Body::Identify(Identify { token })
}

/// Connects and identifies with a token issued now; returns once `Ready` arrived.
async fn ready(stack: &Stack, user: Uuid, device: Uuid) -> Socket {
    let mut socket = connect(stack).await;
    send(&mut socket, identify(stack, user, device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
    socket
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
    let stack = stack().await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    let conversation = direct_chat(&stack, alice, bob).await;

    let mut bob_socket = connect(&stack).await;
    send(&mut bob_socket, identify(&stack, bob, bob_device)).await;
    let Body::Ready(ready_frame) = next(&mut bob_socket).await else {
        panic!("expected Ready");
    };
    assert_eq!(ready_frame.user_id, id_bytes(bob));
    assert_eq!(ready_frame.device_id, id_bytes(bob_device));
    assert_eq!(ready_frame.heartbeat_ms, 30_000);

    let mut alice_socket = ready(&stack, alice, alice_device).await;
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
async fn an_invalid_token_is_closed_with_4001_and_an_expired_one_with_4002() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    let elsewhere = AccessTokens::from_seed(&[7; 32], "wss://elsewhere.test/v1/socket").unwrap();
    let stranger = AccessTokens::from_seed(&[8; 32], AUDIENCE).unwrap();
    for token in [
        "not a token".to_owned(),
        String::new(),
        elsewhere.issue(user, device, now()).0,
    ] {
        let mut socket = connect(&stack).await;
        send(&mut socket, Body::Identify(Identify { token })).await;
        assert_eq!(close_code(&mut socket).await, 4001);
    }
    // A key the gateway does not hold even after fetching the set again, such as one
    // retired before it started: refresh, never a sign-out.
    let mut socket = connect(&stack).await;
    let token = stranger.issue(user, device, now()).0;
    send(&mut socket, Body::Identify(Identify { token })).await;
    assert_eq!(close_code(&mut socket).await, 4002);
    // Expired 90 s ago: past the 60 s leeway. The client refreshes, then reconnects.
    // Expired 45 s ago: inside the leeway, but past the 30 s a connection has to send
    // ReAuth, so refused at once rather than closed right after Ready.
    for expired in [90, 45] {
        let mut socket = connect(&stack).await;
        send(
            &mut socket,
            identify_issued(&stack, user, device, now() - 900 - expired),
        )
        .await;
        let first = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a message within 10 s");
        assert!(
            matches!(&first, Some(Ok(Message::Close(Some(frame)))) if u16::from(frame.code) == 4002),
            "{first:?}"
        );
    }
}

#[tokio::test]
async fn message_svc_decides_whether_the_tokens_device_may_connect() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;

    // Registered moments ago and not relayed yet: retry shortly, never a sign-out.
    let mut socket = connect(&stack).await;
    send(&mut socket, identify(&stack, user, Uuid::now_v7())).await;
    assert_eq!(close_code(&mut socket).await, 4006);

    // A token issued before the password reset: sign in again.
    let before = now() - 60;
    stack
        .messaging
        .password_reset(UserId(user), None)
        .await
        .unwrap();
    let mut socket = connect(&stack).await;
    send(&mut socket, identify_issued(&stack, user, device, before)).await;
    assert_eq!(close_code(&mut socket).await, 4001);
    let mut socket = connect(&stack).await;
    send(
        &mut socket,
        identify_issued(&stack, user, device, now() + 1),
    )
    .await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));

    // A revoked device: stop.
    stack
        .messaging
        .device_revoked(DeviceId(device), UserId(user))
        .await
        .unwrap();
    let mut socket = connect(&stack).await;
    send(
        &mut socket,
        identify_issued(&stack, user, device, now() + 1),
    )
    .await;
    assert_eq!(close_code(&mut socket).await, 4003);
}

#[tokio::test]
async fn a_connection_that_never_identifies_is_closed_after_5_seconds() {
    let stack = stack().await;
    let mut socket = connect(&stack).await;
    assert_eq!(close_code(&mut socket).await, 1002);
}

#[tokio::test]
async fn shutdown_closes_connections_with_4005() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    let mut socket = ready(&stack, user, device).await;

    stack.shutdown.cancel();
    assert_eq!(close_code(&mut socket).await, 4005);
    tokio::time::timeout(Duration::from_secs(15), stack.server)
        .await
        .expect("the gateway stops within 15 s")
        .unwrap()
        .expect("a clean stop");
}

/// Spec §12 test 7: ReAuth keeps a connection whose token expires open; without it, 4002.
#[tokio::test]
async fn an_expiring_token_is_announced_and_renewed_by_reauth() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    let (other, other_device) = person(&stack).await;
    // Expired 25 s ago, within the leeway: 5 s left to send ReAuth.
    let issued_at = now() - 900 - 25;
    let mut renewed = connect(&stack).await;
    let mut lapsed = connect(&stack).await;
    for (socket, user, device) in [
        (&mut renewed, user, device),
        (&mut lapsed, other, other_device),
    ] {
        send(socket, identify_issued(&stack, user, device, issued_at)).await;
        assert!(matches!(next(socket).await, Body::Ready(_)));
        assert_eq!(
            next(socket).await,
            Body::AuthExpiring(AuthExpiring { expires_in_s: 0 })
        );
    }
    let (token, _) = stack.issuer.issue(user, device, now());
    send(&mut renewed, Body::ReAuth(ReAuth { token })).await;
    assert_eq!(close_code(&mut lapsed).await, 4002);
    send(&mut renewed, Body::Ping(Ping {})).await;
    assert_eq!(next(&mut renewed).await, Body::Pong(Pong {}));

    // A token of another device cannot renew the connection.
    let (token, _) = stack.issuer.issue(user, Uuid::now_v7(), now());
    send(&mut renewed, Body::ReAuth(ReAuth { token })).await;
    assert_eq!(close_code(&mut renewed).await, 4001);
}

/// The first frame `wanted` accepts, skipping others, within 5 s each.
async fn wait_for(socket: &mut Socket, wanted: impl Fn(&Body) -> bool) -> Body {
    loop {
        let frame = next(socket).await;
        if wanted(&frame) {
            return frame;
        }
    }
}

/// Spec §12 test 6 and the owner's close codes: a revoked device is told to stop (4003),
/// a password reset to sign in again (4001).
#[tokio::test]
async fn a_revocation_and_a_password_reset_close_the_socket_at_once() {
    let stack = stack().await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    let mut alice_socket = ready(&stack, alice, alice_device).await;
    let mut bob_socket = ready(&stack, bob, bob_device).await;
    stack
        .messaging
        .device_revoked(DeviceId(alice_device), UserId(alice))
        .await
        .unwrap();
    assert_eq!(close_code(&mut alice_socket).await, 4003);

    // Bob's laptop signed in after the reset, which reaches the gateway later: its newer
    // token keeps it open, while the phone's session from before closes.
    let laptop = Uuid::now_v7();
    stack
        .messaging
        .device_registered(DeviceId(laptop), UserId(bob))
        .await
        .unwrap();
    let mut laptop_socket = connect(&stack).await;
    send(
        &mut laptop_socket,
        identify_issued(&stack, bob, laptop, now() + 2),
    )
    .await;
    assert!(matches!(next(&mut laptop_socket).await, Body::Ready(_)));
    stack
        .messaging
        .password_reset(UserId(bob), None)
        .await
        .unwrap();
    assert_eq!(close_code(&mut bob_socket).await, 4001);
    send(&mut laptop_socket, Body::Ping(Ping {})).await;
    assert_eq!(next(&mut laptop_socket).await, Body::Pong(Pong {}));
}

/// Spec §7.3: the newest connection of a device wins; the older one is told to stop.
#[tokio::test]
async fn a_newer_connection_of_the_device_takes_over() {
    let stack = stack().await;
    let (user, device) = person(&stack).await;
    let mut first = ready(&stack, user, device).await;
    let mut second = ready(&stack, user, device).await;
    assert_eq!(close_code(&mut first).await, 4003);
    send(&mut second, Body::Ping(Ping {})).await;
    assert_eq!(next(&mut second).await, Body::Pong(Pong {}));
}

#[tokio::test]
async fn contact_events_receipts_and_read_positions_reach_the_sockets() {
    let stack = stack().await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    let mut alice_socket = ready(&stack, alice, alice_device).await;
    let mut bob_socket = ready(&stack, bob, bob_device).await;

    stack
        .messaging
        .contact_request(UserId(alice), UserId(bob))
        .await
        .unwrap();
    let event = |user: Uuid, state: ContactState| {
        Body::ContactEvent(ContactEvent {
            user_id: id_bytes(user),
            state: state.into(),
        })
    };
    assert_eq!(
        next(&mut bob_socket).await,
        event(alice, ContactState::RequestedIn)
    );
    assert_eq!(
        next(&mut alice_socket).await,
        event(bob, ContactState::RequestedOut)
    );
    stack
        .messaging
        .contact_accept(UserId(bob), UserId(alice))
        .await
        .unwrap();
    assert_eq!(
        next(&mut bob_socket).await,
        event(alice, ContactState::Accepted)
    );
    let conversation = stack
        .messaging
        .open_direct(UserId(alice), UserId(bob))
        .await
        .unwrap();
    let conversation = id_bytes(conversation.0);

    send(
        &mut alice_socket,
        Body::Send(Send {
            client_msg_id: id_bytes(Uuid::now_v7()),
            conversation_id: conversation.clone(),
            content_type: ContentType::Text.into(),
            body: b"hello".to_vec(),
            client_ts: None,
        }),
    )
    .await;
    wait_for(&mut bob_socket, |frame| matches!(frame, Body::Deliver(_))).await;
    send(
        &mut bob_socket,
        Body::Ack(Ack {
            conversation_id: conversation.clone(),
            seq: 1,
        }),
    )
    .await;
    let receipt = |delivered_seq, read_seq| {
        Body::Receipt(Receipt {
            conversation_id: conversation.clone(),
            positions: Some(receipt::Positions::Direct(DirectReceipt {
                user_id: id_bytes(bob),
                delivered_seq,
                read_seq,
            })),
        })
    };
    let delivered = receipt(1, 0);
    let got = wait_for(&mut alice_socket, |frame| matches!(frame, Body::Receipt(_))).await;
    assert_eq!(got, delivered);

    stack
        .messaging
        .read_up_to(UserId(bob), ConversationId(id(&conversation).unwrap()), 1)
        .await
        .unwrap();
    let position = wait_for(&mut bob_socket, |frame| {
        matches!(frame, Body::ReadPosition(_))
    })
    .await;
    assert_eq!(
        position,
        Body::ReadPosition(ReadPosition {
            conversation_id: conversation.clone(),
            read_seq: 1,
        })
    );
    let got = wait_for(&mut alice_socket, |frame| matches!(frame, Body::Receipt(_))).await;
    assert_eq!(got, receipt(1, 1));
}

/// Alice and Bob, one device each, as contacts with a direct conversation; and `count`
/// messages from Alice in it, sent through message-svc before anyone connects.
async fn chat_with_backlog(stack: &Stack, count: u32) -> ((Uuid, Uuid), (Uuid, Uuid), Vec<u8>) {
    let (alice, alice_device) = person(stack).await;
    let (bob, bob_device) = person(stack).await;
    let conversation = direct_chat(stack, alice, bob).await;

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
    let stack = stack().await;
    let backlog = MAX_SYNC_BATCH + 1;
    let (_alice, (bob, bob_device), conversation) = chat_with_backlog(&stack, backlog).await;

    let mut socket = connect(&stack).await;
    send(&mut socket, identify(&stack, bob, bob_device)).await;
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
    let stack = stack_with(1).await;
    let (_alice, (bob, bob_device), _conversation) = chat_with_backlog(&stack, 1).await;

    let mut socket = connect(&stack).await;
    send(&mut socket, identify(&stack, bob, bob_device)).await;
    assert!(matches!(next(&mut socket).await, Body::Ready(_)));
    // The sync after Ready fails; nothing rings Bob's channel, so only a retry delivers.
    let Body::Deliver(entry) = next(&mut socket).await else {
        panic!("expected Deliver");
    };
    assert_eq!(entry.seq, 1);
}
