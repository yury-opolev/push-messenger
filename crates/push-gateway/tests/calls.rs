//! Calls across two gateway replicas, each in front of its own message-svc replica with
//! the call engine (on one shared in-memory store) and one real Redis (Docker must be
//! running): call state lives in Redis, so a call's devices may sit on either replica.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use push_auth_core::tokens::AccessTokens;
use push_domain::calls::Timing;
use push_domain::{
    DeviceId, InMemoryStore, Messaging, OutgoingPolicy, OutgoingPolicyChange, SystemClock, UserId,
};
use push_gateway::{Config, serve};
use push_message_svc::{
    CallService, CallsApi, InternalToken, MessagingApi, Metrics, RedisCalls, RedisNotifier,
};
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_proto::v1::frame::Body;
use push_proto::v1::{
    CallAccept, CallCancel, CallDecline, CallDirection, CallEndReason, CallError, CallHangup,
    CallIce, CallInvite, CallMedia, CallRenegotiate, CallSdpType, CallStatus, CallSync, Capability,
    ErrorCode, Identify,
};
use push_proto::{decode_frame, encode_frame, id_bytes, presence_conns_key};
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Shared = Arc<Messaging<InMemoryStore, SystemClock, RedisNotifier>>;

const AUDIENCE: &str = "ws://push.test/v1/socket";
const MESSAGING_TOKEN: &str = "messaging-token-for-the-gateway-tests";
/// Never to be seen in a log: every SDP of these tests carries it.
const MARKER: &str = "SDP-MARKER-7f3a";
/// Never to be seen in a log: every candidate of these tests names it.
const ADDRESS: &str = "203.0.113.77";
const OFFER: &str = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=SDP-MARKER-7f3a\r\nt=0 0\r\n\
    m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=x-marker:SDP-MARKER-7f3a\r\n";
const ANSWER: &str = "v=0\r\no=- 2 1 IN IP4 0.0.0.0\r\ns=SDP-MARKER-7f3a\r\nt=0 0\r\n\
    m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=x-marker:SDP-MARKER-7f3a\r\n";
const RING_MS: i64 = 2_000;
const GRACE_MS: i64 = 1_000;
const ATTEMPTS: u32 = 5;
/// How long a device must stay silent to count as not having received a frame.
const QUIET: Duration = Duration::from_millis(400);
/// Gateway replica A and replica B.
const A: usize = 0;
const B: usize = 1;

struct Stack {
    _redis: ContainerAsync<Redis>,
    redis_url: String,
    messaging: Shared,
    issuer: AccessTokens,
    auth_url: String,
    gateways: [SocketAddr; 2],
    shutdown: CancellationToken,
}

impl Drop for Stack {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// The log lines of every test of this binary, as JSON.
fn logs() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&buffer);
        // Everything at TRACE, through the gateway's own filter as `init_logging` uses it.
        let _ = tracing_subscriber::fmt()
            .json()
            .with_env_filter(push_gateway::log_filter(
                tracing_subscriber::EnvFilter::new("trace"),
            ))
            .with_writer(move || Captured(Arc::clone(&writer)))
            .try_init();
        buffer
    })
}

struct Captured(Arc<Mutex<Vec<u8>>>);

impl io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Redis, one store, two message-svc replicas with the call engine (each with its
/// sweeper; ring 2 s, grace 1 s), auth-svc's key set, and two gateways with calls
/// enabled: A in front of message-svc 1, B in front of message-svc 2.
async fn stack() -> Stack {
    logs();
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
    let messaging = Arc::new(Messaging::new(
        InMemoryStore::default(),
        SystemClock,
        notifier,
    ));

    let issuer = AccessTokens::from_seed(&[7; 32], AUDIENCE).unwrap();
    let jwks = issuer.jwks();
    let keys = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let auth_url = format!("http://{}", keys.local_addr().unwrap());
    let routes = axum::Router::new().route(
        "/v1/auth/jwks",
        axum::routing::get(move || async move { axum::Json(jwks) }),
    );
    tokio::spawn(axum::serve(keys, routes).into_future());

    let mut gateways = Vec::new();
    for _ in 0..2 {
        let message_svc = message_svc(&messaging, &redis_url, true, &shutdown).await;
        gateways.push(gateway(&redis_url, message_svc, &auth_url, &shutdown).await);
    }
    Stack {
        _redis: redis,
        redis_url,
        messaging,
        issuer,
        auth_url,
        gateways: [gateways[0], gateways[1]],
        shutdown,
    }
}

/// A message-svc replica on the shared store, with or without the call engine; returns
/// its URL.
async fn message_svc(
    messaging: &Shared,
    redis_url: &str,
    calls: bool,
    shutdown: &CancellationToken,
) -> String {
    let notifier = RedisNotifier::connect(redis_url).await.unwrap();
    let presence = Arc::new(notifier.presence());
    let mut api = MessagingApi::with_presence(Arc::clone(messaging), presence);
    if calls {
        let timing = Timing {
            ring_ms: RING_MS,
            grace_ms: GRACE_MS,
            liveness_ms: 500,
        };
        let service = Arc::new(CallService::new(
            Arc::clone(messaging),
            RedisCalls::connect(redis_url).await.unwrap(),
            notifier,
            timing,
            Arc::new(Metrics::new()),
        ));
        let sweeper = Arc::clone(&service);
        let sweeping = shutdown.clone();
        tokio::spawn(async move { sweeper.run_sweeper(sweeping).await });
        api = api.with_calls(service as Arc<dyn CallsApi>);
    }
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", grpc.local_addr().unwrap());
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
    url
}

/// A gateway with calls enabled (5 attempts per hour) in front of `message_svc`.
async fn gateway(
    redis_url: &str,
    message_svc: String,
    auth_url: &str,
    shutdown: &CancellationToken,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = Config {
        message_svc_url: message_svc,
        messaging_token: MESSAGING_TOKEN.to_owned(),
        redis: push_credentials::RedisTarget::Password {
            url: redis_url.to_owned(),
        },
        azure: None,
        auth_url: auth_url.to_owned(),
        token_audience: AUDIENCE.to_owned(),
        listen_addr: address,
        health_addr: health.local_addr().unwrap(),
        heartbeat: Duration::from_secs(30),
        groups: false,
        client_ip_header: None,
        presence_grace: Duration::from_millis(300),
        calls: true,
        call_attempts_per_hour: ATTEMPTS,
    };
    tokio::spawn(serve(config, listener, health, shutdown.clone()));
    address
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A user with one device, relayed to message-svc as the worker relays them.
async fn person(stack: &Stack) -> (Uuid, Uuid) {
    let user = Uuid::now_v7();
    (user, device(stack, user).await)
}

/// Another device of `user`.
async fn device(stack: &Stack, user: Uuid) -> Uuid {
    let device = Uuid::now_v7();
    stack
        .messaging
        .device_registered(DeviceId(device), UserId(user))
        .await
        .unwrap();
    device
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

/// The next frame, within 10 s.
async fn next(socket: &mut Socket) -> Body {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a frame within 10 s")
            .expect("the connection is open")
            .expect("a WebSocket message");
        match message {
            Message::Binary(bytes) => return decode_frame(&bytes).expect("a valid frame"),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("expected a frame, got {other:?}"),
        }
    }
}

fn is_call(frame: &Body) -> bool {
    matches!(
        frame,
        Body::CallIncoming(_)
            | Body::CallAccepted(_)
            | Body::CallTaken(_)
            | Body::CallEnded(_)
            | Body::CallError(_)
            | Body::CallSync(_)
            | Body::PeerCallIce(_)
            | Body::PeerCallRenegotiate(_)
    )
}

/// The next call frame, skipping presence and other frames.
async fn next_call(socket: &mut Socket) -> Body {
    loop {
        let frame = next(socket).await;
        if is_call(&frame) {
            return frame;
        }
    }
}

/// Asserts that no call frame reaches `socket` for a while.
async fn quiet(socket: &mut Socket) {
    let deadline = Instant::now() + QUIET;
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => return,
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                let frame = decode_frame(&bytes).expect("a valid frame");
                assert!(!is_call(&frame), "expected no call frame, got {frame:?}");
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            Ok(other) => panic!("expected an open connection, got {other:?}"),
        }
    }
}

async fn connect(gateway: SocketAddr) -> Socket {
    let (socket, _) = connect_async(format!("ws://{gateway}/v1/socket"))
        .await
        .expect("connect to the gateway");
    socket
}

/// Connects and identifies advertising calls; returns once `Ready` offered calls, with
/// the device's `CallSync`.
async fn calls_sync(stack: &Stack, gateway: usize, user: Uuid, device: Uuid) -> (Socket, CallSync) {
    let mut socket = connect(stack.gateways[gateway]).await;
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
    (socket, sync)
}

/// As [`calls_sync`], for a device without calls.
async fn calls_ready(stack: &Stack, gateway: usize, user: Uuid, device: Uuid) -> Socket {
    let (socket, sync) = calls_sync(stack, gateway, user, device).await;
    assert!(sync.calls.is_empty());
    socket
}

/// Waits until `user` holds `leases` presence leases: the engine rings only live devices.
async fn wait_online(stack: &Stack, user: Uuid, leases: u64) {
    let mut redis = redis::Client::open(stack.redis_url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    for _ in 0..200 {
        let held: u64 = redis::cmd("ZCARD")
            .arg(presence_conns_key(user))
            .query_async(&mut redis)
            .await
            .unwrap();
        if held >= leases {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no {leases} presence leases within 10 s");
}

fn invite_frame(call: &[u8], callee: Uuid, offer: &str, media: Vec<CallMedia>) -> Body {
    Body::CallInvite(CallInvite {
        call_id: call.to_vec(),
        callee_user_id: id_bytes(callee),
        offer_sdp: offer.to_owned(),
        media: media.into_iter().map(i32::from).collect(),
    })
}

/// Sends an audio `CallInvite` to `callee`; returns the call's id.
async fn invite(socket: &mut Socket, callee: Uuid) -> Vec<u8> {
    let call = id_bytes(Uuid::now_v7());
    send(
        socket,
        invite_frame(&call, callee, OFFER, vec![CallMedia::Audio]),
    )
    .await;
    call
}

fn accept(call: &[u8]) -> Body {
    Body::CallAccept(CallAccept {
        call_id: call.to_vec(),
        answer_sdp: ANSWER.to_owned(),
    })
}

fn hangup(call: &[u8]) -> Body {
    Body::CallHangup(CallHangup {
        call_id: call.to_vec(),
        reason: None,
    })
}

fn candidate(n: u32) -> String {
    format!("candidate:{n} 1 udp 2122260223 {ADDRESS} 54321 typ host")
}

fn ice_frame(call: &[u8], n: u32) -> CallIce {
    CallIce {
        call_id: call.to_vec(),
        candidate: candidate(n),
        sdp_mid: Some("0".to_owned()),
        sdp_mline_index: Some(0),
        end_of_candidates: false,
        username_fragment: None,
    }
}

fn ice(call: &[u8], n: u32) -> Body {
    Body::CallIce(ice_frame(call, n))
}

fn renegotiate(call: &[u8], kind: CallSdpType) -> CallRenegotiate {
    CallRenegotiate {
        call_id: call.to_vec(),
        sdp: OFFER.to_owned(),
        r#type: kind.into(),
    }
}

/// Asserts that `socket` rings for `call` from `caller`.
async fn expect_incoming(socket: &mut Socket, call: &[u8], caller: Uuid) {
    let Body::CallIncoming(incoming) = next_call(socket).await else {
        panic!("expected CallIncoming");
    };
    assert_eq!(incoming.call_id, call);
    assert_eq!(incoming.caller_user_id, id_bytes(caller));
    assert_eq!(incoming.offer_sdp, OFFER);
    assert_eq!(incoming.media, vec![i32::from(CallMedia::Audio)]);
    assert!(incoming.expires_in_ms > 0 && i64::from(incoming.expires_in_ms) <= RING_MS);
}

/// Asserts that `socket` gets `CallAccepted` for `call`, answered by `device`.
async fn expect_accepted(socket: &mut Socket, call: &[u8], device: Uuid) {
    let Body::CallAccepted(accepted) = next_call(socket).await else {
        panic!("expected CallAccepted");
    };
    assert_eq!(accepted.call_id, call);
    assert_eq!(accepted.answer_sdp, ANSWER);
    assert_eq!(accepted.answered_device_id, id_bytes(device));
}

async fn expect_taken(socket: &mut Socket, call: &[u8]) {
    let Body::CallTaken(taken) = next_call(socket).await else {
        panic!("expected CallTaken");
    };
    assert_eq!(taken.call_id, call);
}

async fn expect_ended(socket: &mut Socket, call: &[u8], reason: CallEndReason) {
    let frame = next_call(socket).await;
    let Body::CallEnded(ended) = frame else {
        panic!("expected CallEnded {reason:?}, got {frame:?}");
    };
    assert_eq!(ended.call_id, call);
    assert_eq!(ended.reason(), reason);
}

async fn expect_error(socket: &mut Socket, call: &[u8], code: ErrorCode) -> CallError {
    let frame = next_call(socket).await;
    let Body::CallError(error) = frame else {
        panic!("expected CallError {code:?}, got {frame:?}");
    };
    assert_eq!(error.call_id, call);
    assert_eq!(error.code(), code);
    error
}

async fn expect_ice(socket: &mut Socket, call: &[u8], n: u32) {
    let frame = next_call(socket).await;
    let Body::PeerCallIce(relayed) = frame else {
        panic!("expected PeerCallIce {n}, got {frame:?}");
    };
    assert_eq!(relayed, ice_frame(call, n));
}

/// A caller on gateway A, and a contact of theirs with a phone on A and a laptop on B,
/// every device calls-active and online.
struct Scene {
    caller: Uuid,
    caller_socket: Socket,
    callee: Uuid,
    phone: Uuid,
    phone_socket: Socket,
    laptop: Uuid,
    laptop_socket: Socket,
}

async fn scene(stack: &Stack) -> Scene {
    let (caller, caller_device) = person(stack).await;
    let (callee, phone) = person(stack).await;
    let laptop = device(stack, callee).await;
    contacts(stack, caller, callee).await;
    let caller_socket = calls_ready(stack, A, caller, caller_device).await;
    let phone_socket = calls_ready(stack, A, callee, phone).await;
    let laptop_socket = calls_ready(stack, B, callee, laptop).await;
    wait_online(stack, caller, 1).await;
    wait_online(stack, callee, 2).await;
    Scene {
        caller,
        caller_socket,
        callee,
        phone,
        phone_socket,
        laptop,
        laptop_socket,
    }
}

impl Scene {
    /// The caller calls; both callee devices ring.
    async fn ring(&mut self) -> Vec<u8> {
        let call = invite(&mut self.caller_socket, self.callee).await;
        expect_incoming(&mut self.phone_socket, &call, self.caller).await;
        expect_incoming(&mut self.laptop_socket, &call, self.caller).await;
        call
    }

    /// The caller calls and the laptop answers.
    async fn connect(&mut self) -> Vec<u8> {
        let call = self.ring().await;
        send(&mut self.laptop_socket, accept(&call)).await;
        expect_accepted(&mut self.caller_socket, &call, self.laptop).await;
        expect_taken(&mut self.phone_socket, &call).await;
        call
    }
}

#[tokio::test]
async fn invite_rings_every_device_first_accept_wins_others_get_taken() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let call = scene.ring().await;

    send(&mut scene.laptop_socket, accept(&call)).await;
    expect_accepted(&mut scene.caller_socket, &call, scene.laptop).await;
    expect_taken(&mut scene.phone_socket, &call).await;

    send(&mut scene.phone_socket, accept(&call)).await;
    expect_error(&mut scene.phone_socket, &call, ErrorCode::AlreadyAnswered).await;
    quiet(&mut scene.caller_socket).await;
    quiet(&mut scene.laptop_socket).await;

    send(&mut scene.caller_socket, hangup(&call)).await;
    expect_ended(&mut scene.caller_socket, &call, CallEndReason::Completed).await;
    expect_ended(&mut scene.laptop_socket, &call, CallEndReason::Completed).await;
    quiet(&mut scene.phone_socket).await;
}

#[tokio::test]
async fn ringing_devices_never_get_caller_candidates() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let call = scene.ring().await;

    for n in 1..=3 {
        send(&mut scene.caller_socket, ice(&call, n)).await;
    }
    quiet(&mut scene.phone_socket).await;
    quiet(&mut scene.laptop_socket).await;

    send(&mut scene.laptop_socket, accept(&call)).await;
    for n in 1..=3 {
        expect_ice(&mut scene.laptop_socket, &call, n).await;
    }
    expect_accepted(&mut scene.caller_socket, &call, scene.laptop).await;
    expect_taken(&mut scene.phone_socket, &call).await;

    send(&mut scene.caller_socket, ice(&call, 4)).await;
    expect_ice(&mut scene.laptop_socket, &call, 4).await;
    quiet(&mut scene.phone_socket).await;
}

#[tokio::test]
async fn answer_and_ice_only_reach_the_party_device() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let other = device(&stack, scene.caller).await;
    let mut other_socket = calls_ready(&stack, B, scene.caller, other).await;
    wait_online(&stack, scene.caller, 2).await;

    let call = scene.connect().await;
    quiet(&mut other_socket).await;

    send(&mut scene.laptop_socket, ice(&call, 1)).await;
    expect_ice(&mut scene.caller_socket, &call, 1).await;
    send(&mut scene.caller_socket, ice(&call, 2)).await;
    expect_ice(&mut scene.laptop_socket, &call, 2).await;
    let offer = renegotiate(&call, CallSdpType::Offer);
    send(
        &mut scene.laptop_socket,
        Body::CallRenegotiate(offer.clone()),
    )
    .await;
    assert_eq!(
        next_call(&mut scene.caller_socket).await,
        Body::PeerCallRenegotiate(offer)
    );
    quiet(&mut other_socket).await;
    quiet(&mut scene.phone_socket).await;

    send(&mut scene.laptop_socket, hangup(&call)).await;
    expect_ended(&mut scene.caller_socket, &call, CallEndReason::Completed).await;
    expect_ended(&mut scene.laptop_socket, &call, CallEndReason::Completed).await;
    quiet(&mut other_socket).await;
    quiet(&mut scene.phone_socket).await;
}

#[tokio::test]
async fn decline_ends_for_everyone() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let call = scene.ring().await;

    send(
        &mut scene.phone_socket,
        Body::CallDecline(CallDecline {
            call_id: call.clone(),
            reason: None,
        }),
    )
    .await;
    for socket in [
        &mut scene.caller_socket,
        &mut scene.phone_socket,
        &mut scene.laptop_socket,
    ] {
        expect_ended(socket, &call, CallEndReason::Declined).await;
    }
}

#[tokio::test]
async fn decline_busy_reports_busy() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let call = scene.ring().await;

    send(
        &mut scene.laptop_socket,
        Body::CallDecline(CallDecline {
            call_id: call.clone(),
            reason: Some(CallEndReason::Busy.into()),
        }),
    )
    .await;
    for socket in [
        &mut scene.caller_socket,
        &mut scene.phone_socket,
        &mut scene.laptop_socket,
    ] {
        expect_ended(socket, &call, CallEndReason::Busy).await;
    }
}

#[tokio::test]
async fn cancel_ends_ringing() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let call = scene.ring().await;

    send(
        &mut scene.caller_socket,
        Body::CallCancel(CallCancel {
            call_id: call.clone(),
        }),
    )
    .await;
    for socket in [
        &mut scene.caller_socket,
        &mut scene.phone_socket,
        &mut scene.laptop_socket,
    ] {
        expect_ended(socket, &call, CallEndReason::Cancelled).await;
    }
}

#[tokio::test]
async fn cancel_after_accept_is_already_answered() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let call = scene.ring().await;
    send(&mut scene.phone_socket, accept(&call)).await;
    expect_accepted(&mut scene.caller_socket, &call, scene.phone).await;
    expect_taken(&mut scene.laptop_socket, &call).await;

    send(
        &mut scene.caller_socket,
        Body::CallCancel(CallCancel {
            call_id: call.clone(),
        }),
    )
    .await;
    expect_error(&mut scene.caller_socket, &call, ErrorCode::AlreadyAnswered).await;
    quiet(&mut scene.phone_socket).await;

    // Still connected: a hangup ends it.
    send(&mut scene.caller_socket, hangup(&call)).await;
    expect_ended(&mut scene.caller_socket, &call, CallEndReason::Completed).await;
    expect_ended(&mut scene.phone_socket, &call, CallEndReason::Completed).await;
    quiet(&mut scene.laptop_socket).await;
}

#[tokio::test]
async fn no_answer_is_missed_after_the_ring() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let started = Instant::now();
    let call = scene.ring().await;

    for socket in [
        &mut scene.caller_socket,
        &mut scene.phone_socket,
        &mut scene.laptop_socket,
    ] {
        expect_ended(socket, &call, CallEndReason::Missed).await;
    }
    assert!(started.elapsed() >= Duration::from_millis(RING_MS as u64 - 100));

    // Too late to answer.
    send(&mut scene.laptop_socket, accept(&call)).await;
    expect_error(&mut scene.laptop_socket, &call, ErrorCode::CallNotFound).await;
}

#[tokio::test]
async fn no_capable_device_is_unreachable_at_once() {
    let stack = stack().await;
    let (caller, caller_device) = person(&stack).await;
    let (plain, plain_device) = person(&stack).await;
    let (offline, _) = person(&stack).await;
    contacts(&stack, caller, plain).await;
    contacts(&stack, caller, offline).await;
    let mut caller_socket = calls_ready(&stack, A, caller, caller_device).await;

    // Online, without the calls capability.
    let mut plain_socket = connect(stack.gateways[B]).await;
    let (token, _) = stack.issuer.issue(plain, plain_device, now());
    send(
        &mut plain_socket,
        Body::Identify(Identify {
            token,
            capabilities: Vec::new(),
        }),
    )
    .await;
    let Body::Ready(_) = next(&mut plain_socket).await else {
        panic!("expected Ready");
    };
    wait_online(&stack, plain, 1).await;

    for callee in [plain, offline] {
        let started = Instant::now();
        let call = invite(&mut caller_socket, callee).await;
        expect_ended(&mut caller_socket, &call, CallEndReason::Unreachable).await;
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    quiet(&mut plain_socket).await;
}

#[tokio::test]
async fn a_callee_in_a_call_or_ringing_is_busy() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let (carol, carol_device) = person(&stack).await;
    contacts(&stack, carol, scene.callee).await;
    let mut carol_socket = calls_ready(&stack, B, carol, carol_device).await;
    wait_online(&stack, carol, 1).await;

    // Ringing.
    let call = scene.ring().await;
    let second = invite(&mut carol_socket, scene.callee).await;
    expect_ended(&mut carol_socket, &second, CallEndReason::Busy).await;
    quiet(&mut scene.phone_socket).await;
    quiet(&mut scene.laptop_socket).await;

    // In a call.
    send(&mut scene.laptop_socket, accept(&call)).await;
    expect_accepted(&mut scene.caller_socket, &call, scene.laptop).await;
    expect_taken(&mut scene.phone_socket, &call).await;
    let third = invite(&mut carol_socket, scene.callee).await;
    expect_ended(&mut carol_socket, &third, CallEndReason::Busy).await;
    quiet(&mut scene.phone_socket).await;
    quiet(&mut scene.laptop_socket).await;
}

#[tokio::test]
async fn a_caller_has_one_call_at_a_time() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let other = device(&stack, scene.caller).await;
    let mut other_socket = calls_ready(&stack, B, scene.caller, other).await;
    let (carol, carol_device) = person(&stack).await;
    contacts(&stack, scene.caller, carol).await;
    let mut carol_socket = calls_ready(&stack, B, carol, carol_device).await;
    wait_online(&stack, carol, 1).await;

    // Ringing: neither of the caller's devices may place another call.
    let call = scene.ring().await;
    let second = invite(&mut scene.caller_socket, carol).await;
    expect_error(&mut scene.caller_socket, &second, ErrorCode::InCall).await;
    let third = invite(&mut other_socket, carol).await;
    expect_error(&mut other_socket, &third, ErrorCode::InCall).await;
    quiet(&mut carol_socket).await;

    // Active.
    send(&mut scene.laptop_socket, accept(&call)).await;
    expect_accepted(&mut scene.caller_socket, &call, scene.laptop).await;
    let fourth = invite(&mut other_socket, carol).await;
    expect_error(&mut other_socket, &fourth, ErrorCode::InCall).await;
    quiet(&mut carol_socket).await;
}

#[tokio::test]
async fn disconnect_beyond_the_grace_fails_the_call() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let call = scene.connect().await;

    drop(scene.laptop_socket);
    let gone = Instant::now();
    let frame = next_call(&mut scene.caller_socket).await;
    let Body::CallEnded(ended) = frame else {
        panic!("expected CallEnded, got {frame:?}");
    };
    assert_eq!(ended.call_id, call);
    assert_eq!(ended.reason(), CallEndReason::Failed);
    assert!(ended.duration_s.is_some());
    assert!(gone.elapsed() >= Duration::from_millis(GRACE_MS as u64 - 100));

    // The answering device, back too late, has no call any more.
    let (mut laptop_socket, sync) = calls_sync(&stack, A, scene.callee, scene.laptop).await;
    assert!(sync.calls.is_empty(), "{sync:?}");
    send(&mut laptop_socket, ice(&call, 1)).await;
    expect_error(&mut laptop_socket, &call, ErrorCode::CallNotFound).await;
}

#[tokio::test]
async fn reconnect_within_the_grace_keeps_the_call() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let call = scene.connect().await;

    // The laptop (on B) drops and comes back on A at once.
    drop(scene.laptop_socket);
    let (mut laptop_socket, sync) = calls_sync(&stack, A, scene.callee, scene.laptop).await;
    assert_eq!(sync.calls.len(), 1, "{sync:?}");
    let state = &sync.calls[0];
    assert_eq!(state.call_id, call);
    assert_eq!(state.peer_user_id, id_bytes(scene.caller));
    assert_eq!(state.direction(), CallDirection::In);
    assert_eq!(state.status(), CallStatus::Active);
    assert!(state.this_device);
    assert!(state.answered_at.is_some());

    // An ICE restart, both ways.
    let offer = renegotiate(&call, CallSdpType::Offer);
    send(&mut laptop_socket, Body::CallRenegotiate(offer.clone())).await;
    assert_eq!(
        next_call(&mut scene.caller_socket).await,
        Body::PeerCallRenegotiate(offer)
    );
    let answer = renegotiate(&call, CallSdpType::Answer);
    send(
        &mut scene.caller_socket,
        Body::CallRenegotiate(answer.clone()),
    )
    .await;
    assert_eq!(
        next_call(&mut laptop_socket).await,
        Body::PeerCallRenegotiate(answer)
    );

    // Well past the grace, the call goes on.
    tokio::time::sleep(Duration::from_millis(3 * GRACE_MS as u64)).await;
    send(&mut scene.caller_socket, ice(&call, 1)).await;
    expect_ice(&mut laptop_socket, &call, 1).await;
    send(&mut laptop_socket, hangup(&call)).await;
    expect_ended(&mut scene.caller_socket, &call, CallEndReason::Completed).await;
    expect_ended(&mut laptop_socket, &call, CallEndReason::Completed).await;
}

#[tokio::test]
async fn a_stranger_or_blocked_user_cannot_call() {
    let stack = stack().await;
    let (caller, caller_device) = person(&stack).await;
    let (stranger, stranger_device) = person(&stack).await;
    let (pending, pending_device) = person(&stack).await;
    let (blocker, blocker_device) = person(&stack).await;
    let (blocked, blocked_device) = person(&stack).await;
    stack
        .messaging
        .contact_request(UserId(caller), UserId(pending))
        .await
        .unwrap();
    contacts(&stack, caller, blocker).await;
    stack
        .messaging
        .contact_block(UserId(blocker), UserId(caller))
        .await
        .unwrap();
    contacts(&stack, caller, blocked).await;
    stack
        .messaging
        .contact_block(UserId(caller), UserId(blocked))
        .await
        .unwrap();

    let mut caller_socket = calls_ready(&stack, A, caller, caller_device).await;
    let mut callees = Vec::new();
    for (index, (user, device)) in [
        (stranger, stranger_device),
        (pending, pending_device),
        (blocker, blocker_device),
        (blocked, blocked_device),
    ]
    .into_iter()
    .enumerate()
    {
        callees.push(calls_ready(&stack, index % 2, user, device).await);
        wait_online(&stack, user, 1).await;
    }
    wait_online(&stack, caller, 1).await;

    // Five invites: the hour's attempts, all refused alike.
    let unknown = Uuid::now_v7();
    for callee in [stranger, pending, blocker, blocked, unknown] {
        let call = invite(&mut caller_socket, callee).await;
        let error = expect_error(&mut caller_socket, &call, ErrorCode::NotContact).await;
        assert!(!error.retryable);
    }
    // Blocked either way: neither side may call the other.
    let [_, _, blocker_socket, blocked_socket] = &mut callees[..] else {
        unreachable!();
    };
    for socket in [blocker_socket, blocked_socket] {
        let call = invite(socket, caller).await;
        expect_error(socket, &call, ErrorCode::NotContact).await;
    }
    for socket in &mut callees {
        quiet(socket).await;
    }
    quiet(&mut caller_socket).await;
}

#[tokio::test]
async fn agent_calls_only_its_owner() {
    let stack = stack().await;
    let (owner, owner_device) = person(&stack).await;
    let (agent, agent_device) = person(&stack).await;
    let (friend, friend_device) = person(&stack).await;
    stack
        .messaging
        .enroll_agent(UserId(owner), UserId(agent))
        .await
        .unwrap();
    // By default an agent's requests wait for its owner.
    stack
        .messaging
        .set_outgoing_policy(
            UserId(owner),
            UserId(agent),
            OutgoingPolicyChange {
                requests: Some(OutgoingPolicy::Allowed),
                ..OutgoingPolicyChange::default()
            },
        )
        .await
        .unwrap();
    contacts(&stack, agent, friend).await;
    contacts(&stack, owner, friend).await;
    let mut owner_socket = calls_ready(&stack, A, owner, owner_device).await;
    let mut agent_socket = calls_ready(&stack, B, agent, agent_device).await;
    let mut friend_socket = calls_ready(&stack, A, friend, friend_device).await;
    for user in [owner, agent, friend] {
        wait_online(&stack, user, 1).await;
    }

    // The agent calls a contact that is not its owner (its owner's call policy is off by
    // default), and that contact calls the agent.
    let call = invite(&mut agent_socket, friend).await;
    expect_error(&mut agent_socket, &call, ErrorCode::NotAllowedByOwner).await;
    // Under `ask` the call waits for the owner, named in the error.
    stack
        .messaging
        .set_outgoing_policy(
            UserId(owner),
            UserId(agent),
            OutgoingPolicyChange {
                calls: Some(OutgoingPolicy::Ask),
                ..OutgoingPolicyChange::default()
            },
        )
        .await
        .unwrap();
    let call = invite(&mut agent_socket, friend).await;
    let error = expect_error(&mut agent_socket, &call, ErrorCode::WaitingForOwner).await;
    assert_eq!(error.action_id.map(|id| id.len()), Some(16));
    let call = invite(&mut friend_socket, agent).await;
    expect_error(&mut friend_socket, &call, ErrorCode::NotContact).await;
    quiet(&mut agent_socket).await;
    quiet(&mut friend_socket).await;

    // The agent calls its owner.
    let call = invite(&mut agent_socket, owner).await;
    expect_incoming(&mut owner_socket, &call, agent).await;
    send(&mut owner_socket, accept(&call)).await;
    expect_accepted(&mut agent_socket, &call, owner_device).await;
    send(&mut agent_socket, hangup(&call)).await;
    expect_ended(&mut agent_socket, &call, CallEndReason::Completed).await;
    expect_ended(&mut owner_socket, &call, CallEndReason::Completed).await;

    // And the owner calls the agent.
    let call = invite(&mut owner_socket, agent).await;
    expect_incoming(&mut agent_socket, &call, owner).await;
    send(
        &mut owner_socket,
        Body::CallCancel(CallCancel {
            call_id: call.clone(),
        }),
    )
    .await;
    expect_ended(&mut owner_socket, &call, CallEndReason::Cancelled).await;
    expect_ended(&mut agent_socket, &call, CallEndReason::Cancelled).await;
}

#[tokio::test]
async fn another_users_device_cannot_touch_the_call() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let (mallory, mallory_device) = person(&stack).await;
    contacts(&stack, mallory, scene.caller).await;
    contacts(&stack, mallory, scene.callee).await;
    let mut mallory_socket = calls_ready(&stack, B, mallory, mallory_device).await;
    wait_online(&stack, mallory, 1).await;

    let call = scene.ring().await;
    let frames = |call: &[u8]| {
        vec![
            accept(call),
            Body::CallDecline(CallDecline {
                call_id: call.to_vec(),
                reason: None,
            }),
            Body::CallCancel(CallCancel {
                call_id: call.to_vec(),
            }),
            ice(call, 1),
            Body::CallRenegotiate(renegotiate(call, CallSdpType::Offer)),
            hangup(call),
        ]
    };
    for frame in frames(&call) {
        send(&mut mallory_socket, frame).await;
        expect_error(&mut mallory_socket, &call, ErrorCode::CallNotFound).await;
    }
    quiet(&mut scene.caller_socket).await;
    quiet(&mut scene.phone_socket).await;
    quiet(&mut scene.laptop_socket).await;

    // Untouched: it still rings, and once answered it is still out of reach.
    send(&mut scene.laptop_socket, accept(&call)).await;
    expect_accepted(&mut scene.caller_socket, &call, scene.laptop).await;
    expect_taken(&mut scene.phone_socket, &call).await;
    for frame in frames(&call) {
        send(&mut mallory_socket, frame).await;
        expect_error(&mut mallory_socket, &call, ErrorCode::CallNotFound).await;
    }
    quiet(&mut scene.caller_socket).await;
    quiet(&mut scene.laptop_socket).await;

    send(&mut scene.caller_socket, hangup(&call)).await;
    expect_ended(&mut scene.caller_socket, &call, CallEndReason::Completed).await;
    expect_ended(&mut scene.laptop_socket, &call, CallEndReason::Completed).await;
}

/// `OFFER` padded with an attribute line to exactly `bytes`.
fn padded_offer(bytes: usize) -> String {
    let mut offer = OFFER.to_owned();
    let room = bytes - offer.len() - "a=x-pad:\r\n".len();
    offer.push_str("a=x-pad:");
    offer.push_str(&"p".repeat(room));
    offer.push_str("\r\n");
    assert_eq!(offer.len(), bytes);
    offer
}

#[tokio::test]
async fn oversize_and_video_offers_are_refused() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;

    let video_sdp = format!("{OFFER}m=video 9 UDP/TLS/RTP/SAVPF 96\r\n");
    let refused = [
        (padded_offer(16_385), vec![CallMedia::Audio]),
        (video_sdp, vec![CallMedia::Audio]),
        (OFFER.to_owned(), vec![CallMedia::Audio, CallMedia::Video]),
    ];
    for (offer, media) in refused {
        let call = id_bytes(Uuid::now_v7());
        send(
            &mut scene.caller_socket,
            invite_frame(&call, scene.callee, &offer, media),
        )
        .await;
        let error = expect_error(&mut scene.caller_socket, &call, ErrorCode::Invalid).await;
        assert!(!error.retryable);
    }
    quiet(&mut scene.phone_socket).await;
    quiet(&mut scene.laptop_socket).await;

    // The largest offer rings.
    let largest = padded_offer(16_384);
    let call = id_bytes(Uuid::now_v7());
    send(
        &mut scene.caller_socket,
        invite_frame(&call, scene.callee, &largest, vec![CallMedia::Audio]),
    )
    .await;
    let Body::CallIncoming(incoming) = next_call(&mut scene.phone_socket).await else {
        panic!("expected CallIncoming");
    };
    assert_eq!(incoming.offer_sdp, largest);
}

/// Sends `frame` on a socket that already sent `sent` frames, keeping inside the
/// gateway's frame budget (bursts of 100, 20 a second).
async fn paced(socket: &mut Socket, sent: &mut u32, frame: Body) {
    if *sent >= 90 {
        tokio::time::sleep(Duration::from_millis(55)).await;
    }
    *sent += 1;
    send(socket, frame).await;
}

/// Asserts that `socket` gets `relayed` peer candidates, numbered from 1, in order, and
/// nothing more.
async fn drain_ice(socket: &mut Socket, call: &[u8], relayed: u32) {
    for n in 1..=relayed {
        expect_ice(socket, call, n).await;
    }
    quiet(socket).await;
}

#[tokio::test]
async fn ice_frames_are_capped_per_side() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let (mut caller_sent, mut laptop_sent) = (2, 1);
    let call = scene.ring().await;

    // Before the answer: 32 buffered, the 33rd refused.
    for n in 1..=32 {
        paced(&mut scene.caller_socket, &mut caller_sent, ice(&call, n)).await;
    }
    paced(&mut scene.caller_socket, &mut caller_sent, ice(&call, 33)).await;
    expect_error(&mut scene.caller_socket, &call, ErrorCode::CallLimit).await;

    paced(&mut scene.laptop_socket, &mut laptop_sent, accept(&call)).await;
    expect_accepted(&mut scene.caller_socket, &call, scene.laptop).await;

    // The caller's side: 200 in all (32 of them buffered), the 201st refused.
    for n in 33..=201 {
        paced(&mut scene.caller_socket, &mut caller_sent, ice(&call, n)).await;
    }
    drain_ice(&mut scene.laptop_socket, &call, 200).await;
    expect_error(&mut scene.caller_socket, &call, ErrorCode::CallLimit).await;

    // The callee's side: 200, the 201st refused.
    for n in 1..=201 {
        paced(&mut scene.laptop_socket, &mut laptop_sent, ice(&call, n)).await;
    }
    drain_ice(&mut scene.caller_socket, &call, 200).await;
    expect_error(&mut scene.laptop_socket, &call, ErrorCode::CallLimit).await;

    // The call itself goes on.
    send(&mut scene.caller_socket, hangup(&call)).await;
    expect_ended(&mut scene.caller_socket, &call, CallEndReason::Completed).await;
    expect_ended(&mut scene.laptop_socket, &call, CallEndReason::Completed).await;
}

#[tokio::test]
async fn attempts_per_hour_are_limited() {
    let stack = stack().await;
    let (caller, phone) = person(&stack).await;
    let laptop = device(&stack, caller).await;
    let (stranger, _) = person(&stack).await;
    let mut phone_socket = calls_ready(&stack, A, caller, phone).await;
    let mut laptop_socket = calls_ready(&stack, B, caller, laptop).await;

    // Refused invites count too, on every device and replica of the user.
    for attempt in 0..ATTEMPTS {
        let socket = if attempt % 2 == 0 {
            &mut phone_socket
        } else {
            &mut laptop_socket
        };
        let call = invite(socket, stranger).await;
        expect_error(socket, &call, ErrorCode::NotContact).await;
    }
    let call = invite(&mut laptop_socket, stranger).await;
    let error = expect_error(&mut laptop_socket, &call, ErrorCode::RateLimited).await;
    assert!(error.retryable);
    let retry_after_s = error.retry_after_s.expect("retry_after_s");
    assert!((1..=3_600).contains(&retry_after_s), "{retry_after_s}");
    let call = invite(&mut phone_socket, stranger).await;
    expect_error(&mut phone_socket, &call, ErrorCode::RateLimited).await;
}

#[tokio::test]
async fn calls_are_not_offered_when_message_svc_has_them_off() {
    let stack = stack().await;
    let message_svc = message_svc(&stack.messaging, &stack.redis_url, false, &stack.shutdown).await;
    let old = gateway(
        &stack.redis_url,
        message_svc,
        &stack.auth_url,
        &stack.shutdown,
    )
    .await;
    let (caller, caller_device) = person(&stack).await;
    let (callee, callee_device) = person(&stack).await;
    contacts(&stack, caller, callee).await;
    let _callee_socket = calls_ready(&stack, A, callee, callee_device).await;
    wait_online(&stack, callee, 1).await;

    let mut socket = connect(old).await;
    let (token, _) = stack.issuer.issue(caller, caller_device, now());
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
    assert!(ready.capabilities.is_empty(), "{ready:?}");

    let call = invite(&mut socket, callee).await;
    let error = expect_error(&mut socket, &call, ErrorCode::NotAvailable).await;
    assert!(!error.retryable);
    send(&mut socket, hangup(&call)).await;
    expect_error(&mut socket, &call, ErrorCode::NotAvailable).await;
    quiet(&mut socket).await;
}

#[tokio::test]
async fn no_sdp_or_candidate_in_logs_or_audit() {
    let stack = stack().await;
    let mut scene = scene(&stack).await;
    let call = scene.ring().await;
    send(&mut scene.caller_socket, ice(&call, 1)).await;
    send(&mut scene.laptop_socket, accept(&call)).await;
    expect_ice(&mut scene.laptop_socket, &call, 1).await;
    expect_accepted(&mut scene.caller_socket, &call, scene.laptop).await;
    send(&mut scene.laptop_socket, ice(&call, 2)).await;
    expect_ice(&mut scene.caller_socket, &call, 2).await;
    let offer = renegotiate(&call, CallSdpType::Offer);
    send(&mut scene.caller_socket, Body::CallRenegotiate(offer)).await;
    let Body::PeerCallRenegotiate(_) = next_call(&mut scene.laptop_socket).await else {
        panic!("expected PeerCallRenegotiate");
    };
    send(&mut scene.caller_socket, hangup(&call)).await;
    expect_ended(&mut scene.caller_socket, &call, CallEndReason::Completed).await;
    expect_ended(&mut scene.laptop_socket, &call, CallEndReason::Completed).await;
    // A refused frame too: errors never echo what was sent.
    send(&mut scene.caller_socket, ice(&call, 3)).await;
    expect_error(&mut scene.caller_socket, &call, ErrorCode::CallNotFound).await;

    let call_id = Uuid::from_slice(&call).unwrap().to_string();
    let logs = String::from_utf8_lossy(&logs().lock().unwrap()).into_owned();
    let ours: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains(&call_id))
        .collect();
    assert!(
        ours.iter().any(|line| line.contains("call.started")),
        "no call.started for {call_id}"
    );
    assert!(
        ours.iter().any(|line| line.contains("call.ended")),
        "no call.ended for {call_id}"
    );
    for secret in [MARKER, ADDRESS, "candidate:"] {
        let leaks: Vec<&str> = logs.lines().filter(|line| line.contains(secret)).collect();
        assert!(leaks.is_empty(), "{secret} logged: {leaks:#?}");
    }
}

/// The next `CallHistoryChanged`, skipping every other frame: its `latest_ended_at`.
async fn next_history_changed(socket: &mut Socket) -> Option<i64> {
    loop {
        if let Body::CallHistoryChanged(changed) = next(socket).await {
            return changed.latest_ended_at;
        }
    }
}

/// Asserts that no `CallHistoryChanged` reaches `socket` for a while.
async fn no_history_changed(socket: &mut Socket) {
    let deadline = Instant::now() + QUIET;
    loop {
        match tokio::time::timeout_at(deadline, socket.next()).await {
            Err(_) => return,
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                let frame = decode_frame(&bytes).expect("a valid frame");
                assert!(
                    !matches!(frame, Body::CallHistoryChanged(_)),
                    "expected no CallHistoryChanged, got {frame:?}"
                );
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            Ok(other) => panic!("expected an open connection, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn history_lists_both_sides_and_hiding_is_per_side() {
    use push_message_svc::{CallHistory, CallHistoryApi};
    use push_proto::internal::v1::calls_server::Calls as _;
    use push_proto::internal::v1::{CallHistoryRequest, HideCallRequest};

    let stack = stack().await;
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    contacts(&stack, alice, bob).await;
    let mut alice_socket = calls_ready(&stack, A, alice, alice_device).await;
    let mut bob_socket = calls_ready(&stack, B, bob, bob_device).await;
    wait_online(&stack, alice, 1).await;
    wait_online(&stack, bob, 1).await;

    let call = invite(&mut alice_socket, bob).await;
    expect_incoming(&mut bob_socket, &call, alice).await;
    send(&mut bob_socket, accept(&call)).await;
    expect_accepted(&mut alice_socket, &call, bob_device).await;
    send(&mut alice_socket, hangup(&call)).await;
    expect_ended(&mut alice_socket, &call, CallEndReason::Completed).await;
    expect_ended(&mut bob_socket, &call, CallEndReason::Completed).await;
    let alice_latest = next_history_changed(&mut alice_socket).await;
    let bob_latest = next_history_changed(&mut bob_socket).await;
    assert!(alice_latest.is_some());
    assert_eq!(alice_latest, bob_latest);

    let history = Arc::new(CallHistory::new(
        Arc::clone(&stack.messaging),
        RedisNotifier::connect(&stack.redis_url).await.unwrap(),
        Arc::new(Metrics::new()),
    ));
    let api = CallHistoryApi::new(history);
    let list = |user: Uuid| {
        tonic::Request::new(CallHistoryRequest {
            user_id: id_bytes(user),
            before: None,
            since: None,
            limit: 0,
        })
    };
    let mine = api.history(list(alice)).await.unwrap().into_inner().calls;
    assert_eq!(mine.len(), 1);
    assert_eq!(
        (
            mine[0].call_id.clone(),
            mine[0].peer_user_id.clone(),
            mine[0].direction(),
            mine[0].outcome(),
            Some(mine[0].ended_at),
        ),
        (
            call.clone(),
            id_bytes(bob),
            CallDirection::Out,
            CallEndReason::Completed,
            alice_latest,
        )
    );
    assert!(mine[0].answered_at.is_some());
    let theirs = api.history(list(bob)).await.unwrap().into_inner().calls;
    assert_eq!(theirs.len(), 1);
    assert_eq!(
        (theirs[0].peer_user_id.clone(), theirs[0].direction()),
        (id_bytes(alice), CallDirection::In)
    );

    api.hide(tonic::Request::new(HideCallRequest {
        user_id: id_bytes(alice),
        call_id: call.clone(),
    }))
    .await
    .unwrap();
    assert_eq!(next_history_changed(&mut alice_socket).await, None);
    no_history_changed(&mut bob_socket).await;
    assert!(
        api.history(list(alice))
            .await
            .unwrap()
            .into_inner()
            .calls
            .is_empty()
    );
    assert_eq!(
        api.history(list(bob)).await.unwrap().into_inner().calls,
        theirs
    );
}
