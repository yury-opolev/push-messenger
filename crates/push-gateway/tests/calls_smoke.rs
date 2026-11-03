//! Calls through one gateway in front of message-svc's API with the call engine (on the
//! in-memory store) and a real Redis (Docker must be running): the wiring end to end.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use push_auth_core::tokens::AccessTokens;
use push_domain::calls::Timing;
use push_domain::{DeviceId, InMemoryStore, Messaging, SystemClock, UserId};
use push_gateway::{Config, serve};
use push_message_svc::{
    CallService, CallsApi, InternalToken, MessagingApi, Metrics, RedisCalls, RedisNotifier,
};
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_proto::v1::frame::Body;
use push_proto::v1::{
    CallAccept, CallEndReason, CallHangup, CallInvite, CallMedia, Capability, Identify,
};
use push_proto::{decode_frame, encode_frame, id_bytes, presence_conns_key};
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const AUDIENCE: &str = "ws://push.test/v1/socket";
const MESSAGING_TOKEN: &str = "messaging-token-for-the-gateway-tests";
const OFFER: &str =
    "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
const ANSWER: &str =
    "v=0\r\no=- 2 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";

struct Stack {
    _redis: ContainerAsync<Redis>,
    redis_url: String,
    messaging: Arc<Messaging<InMemoryStore, SystemClock, RedisNotifier>>,
    issuer: AccessTokens,
    gateway: SocketAddr,
    shutdown: CancellationToken,
}

impl Drop for Stack {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Redis, message-svc's API with calls enabled (and its sweeper), auth-svc's key set, and
/// a gateway with calls enabled, on free ports.
async fn stack() -> Stack {
    let redis = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start Redis (is Docker running?)");
    let redis_url = format!(
        "redis://127.0.0.1:{}",
        redis.get_host_port_ipv4(6379).await.unwrap()
    );
    let shutdown = CancellationToken::new();

    let notifier = RedisNotifier::connect(&redis_url).await.unwrap();
    let presence = Arc::new(notifier.presence());
    let messaging = Arc::new(Messaging::new(
        InMemoryStore::default(),
        SystemClock,
        notifier.clone(),
    ));
    let timing = Timing {
        ring_ms: 10_000,
        grace_ms: 2_000,
        ..Timing::default()
    };
    let calls = Arc::new(CallService::new(
        Arc::clone(&messaging),
        RedisCalls::connect(&redis_url).await.unwrap(),
        notifier,
        timing,
        Arc::new(Metrics::new()),
    ));
    let sweeper = Arc::clone(&calls);
    let sweeping = shutdown.clone();
    tokio::spawn(async move { sweeper.run_sweeper(sweeping).await });
    let api = MessagingApi::with_presence(Arc::clone(&messaging), presence)
        .with_calls(calls as Arc<dyn CallsApi>);
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc = format!("http://{}", grpc.local_addr().unwrap());
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(MessagingServer::with_interceptor(
                api,
                InternalToken::messaging(Some(MESSAGING_TOKEN.to_owned())),
            ))
            .serve_with_incoming_shutdown(
                TcpListenerStream::new(grpc),
                shutdown.clone().cancelled_owned(),
            ),
    );

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
        message_svc_url: message_svc,
        messaging_token: MESSAGING_TOKEN.to_owned(),
        redis: push_credentials::RedisTarget::Password {
            url: redis_url.clone(),
        },
        azure: None,
        auth_url,
        token_audience: AUDIENCE.to_owned(),
        listen_addr: gateway,
        health_addr: health.local_addr().unwrap(),
        heartbeat: Duration::from_secs(30),
        groups: false,
        client_ip_header: None,
        presence_grace: Duration::from_millis(300),
        calls: true,
        call_attempts_per_hour: 30,
    };
    tokio::spawn(serve(config, listener, health, shutdown.clone()));
    Stack {
        _redis: redis,
        redis_url,
        messaging,
        issuer,
        gateway,
        shutdown,
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

async fn contacts(stack: &Stack, one: Uuid, other: Uuid) {
    let (one, other) = (UserId(one), UserId(other));
    stack.messaging.contact_request(one, other).await.unwrap();
    stack.messaging.contact_accept(other, one).await.unwrap();
}

async fn send(socket: &mut Socket, body: Body) {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .unwrap();
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

/// The next call frame, skipping presence and other frames.
async fn next_call(socket: &mut Socket) -> Body {
    loop {
        let frame = next(socket).await;
        if matches!(
            frame,
            Body::CallIncoming(_)
                | Body::CallAccepted(_)
                | Body::CallTaken(_)
                | Body::CallEnded(_)
                | Body::CallError(_)
                | Body::CallSync(_)
                | Body::PeerCallIce(_)
                | Body::PeerCallRenegotiate(_)
        ) {
            return frame;
        }
    }
}

/// Connects and identifies advertising calls; returns once `Ready` offered calls and the
/// (empty) `CallSync` arrived.
async fn calls_ready(stack: &Stack, user: Uuid, device: Uuid) -> Socket {
    let (mut socket, _) = connect_async(format!("ws://{}/v1/socket", stack.gateway))
        .await
        .expect("connect to the gateway");
    let (token, _) = stack.issuer.issue(user, device, now());
    send(
        &mut socket,
        Body::Identify(Identify {
            token,
            capabilities: vec![Capability::Calls.into()],
        }),
    )
    .await;
    let Body::Ready(ready) = next(&mut socket).await else {
        panic!("expected Ready");
    };
    assert_eq!(ready.capabilities, vec![i32::from(Capability::Calls)]);
    let Body::CallSync(sync) = next_call(&mut socket).await else {
        panic!("expected CallSync");
    };
    assert!(sync.calls.is_empty());
    socket
}

/// Waits until `user` holds a presence lease: the engine rings only live devices.
async fn wait_online(stack: &Stack, user: Uuid) {
    let mut redis = redis::Client::open(stack.redis_url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    for _ in 0..100 {
        let leases: u64 = redis::cmd("ZCARD")
            .arg(presence_conns_key(user))
            .query_async(&mut redis)
            .await
            .unwrap();
        if leases > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no presence lease within 5 s");
}

#[tokio::test]
async fn a_call_rings_connects_and_ends_through_the_gateway() {
    let stack = stack().await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    contacts(&stack, alice, bob).await;
    let mut alice_socket = calls_ready(&stack, alice, alice_device).await;
    let mut bob_socket = calls_ready(&stack, bob, bob_device).await;
    wait_online(&stack, bob).await;

    let call = id_bytes(Uuid::now_v7());
    send(
        &mut alice_socket,
        Body::CallInvite(CallInvite {
            call_id: call.clone(),
            callee_user_id: id_bytes(bob),
            offer_sdp: OFFER.to_owned(),
            media: vec![CallMedia::Audio.into()],
        }),
    )
    .await;
    let Body::CallIncoming(incoming) = next_call(&mut bob_socket).await else {
        panic!("expected CallIncoming");
    };
    assert_eq!(incoming.call_id, call);
    assert_eq!(incoming.caller_user_id, id_bytes(alice));
    assert_eq!(incoming.offer_sdp, OFFER);

    send(
        &mut bob_socket,
        Body::CallAccept(CallAccept {
            call_id: call.clone(),
            answer_sdp: ANSWER.to_owned(),
        }),
    )
    .await;
    let Body::CallAccepted(accepted) = next_call(&mut alice_socket).await else {
        panic!("expected CallAccepted");
    };
    assert_eq!(accepted.call_id, call);
    assert_eq!(accepted.answer_sdp, ANSWER);
    assert_eq!(accepted.answered_device_id, id_bytes(bob_device));

    send(
        &mut alice_socket,
        Body::CallHangup(CallHangup {
            call_id: call.clone(),
            reason: None,
        }),
    )
    .await;
    for socket in [&mut alice_socket, &mut bob_socket] {
        let Body::CallEnded(ended) = next_call(socket).await else {
            panic!("expected CallEnded");
        };
        assert_eq!(ended.call_id, call);
        assert_eq!(ended.reason(), CallEndReason::Completed);
    }
}
