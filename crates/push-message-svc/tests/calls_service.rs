//! The call engine against a real Redis (Docker must be running) and an in-memory store.

use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use push_domain::calls::{CallDirection, CallHistoryRow, EndReason, Timing, ended_rows};
use push_domain::{
    DeviceId, InMemoryStore, Messaging, OutgoingPolicy, OutgoingPolicyChange, RecordingNotifier,
    SystemClock, UserId,
};
use push_message_svc::{
    CallHistory, CallHistoryApi, CallService, CallsApi, MessagingApi, Metrics, RedisCalls,
    RedisNotifier,
};
use push_proto::internal::v1::calls_server::Calls as _;
use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
use push_proto::internal::v1::{
    CallDeviceRequest, CallHistoryCursor, CallHistoryEntry, CallHistoryRequest, CallReply,
    CallRequest, CallSignal, HideCallRequest, IdentifyRequest, Notification, call_request,
    call_signal, notification::Kind,
};
use push_proto::v1::{self, CallEndReason, CallMedia, Capability, ErrorCode};
use push_proto::{error_from_status, id_bytes, notify_channel, presence_conns_key};
use redis::aio::{MultiplexedConnection, PubSub};
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio_stream::StreamExt;
use tonic::{Code, Request, Status};
use uuid::Uuid;

const OFFER: &str =
    "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
const ANSWER: &str =
    "v=0\r\no=- 2 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";

type Service = CallService<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;

struct Env {
    _container: ContainerAsync<Redis>,
    url: String,
    messaging: Arc<Messaging<InMemoryStore, SystemClock, Arc<RecordingNotifier>>>,
    notifier: Arc<RecordingNotifier>,
    service: Arc<Service>,
    metrics: Arc<Metrics>,
    admin: MultiplexedConnection,
}

/// A user with one registered device.
#[derive(Clone, Copy)]
struct Party {
    user: UserId,
    device: DeviceId,
}

async fn env(timing: Timing) -> Env {
    let container = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start a Redis container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("the Redis port");
    let url = format!("redis://127.0.0.1:{port}");
    let notifier = Arc::new(RecordingNotifier::default());
    let messaging = Arc::new(Messaging::new(
        InMemoryStore::default(),
        SystemClock,
        Arc::clone(&notifier),
    ));
    let metrics = Arc::new(Metrics::new());
    let service = Arc::new(CallService::new(
        Arc::clone(&messaging),
        RedisCalls::connect(&url).await.expect("connect"),
        RedisNotifier::connect(&url).await.expect("connect"),
        timing,
        Arc::clone(&metrics),
    ));
    let admin = redis::Client::open(url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    Env {
        _container: container,
        url,
        messaging,
        notifier,
        service,
        metrics,
        admin,
    }
}

impl Env {
    async fn user(&self) -> Party {
        let party = Party {
            user: UserId::new(),
            device: DeviceId::new(),
        };
        self.messaging
            .device_registered(party.device, party.user)
            .await
            .unwrap();
        party
    }

    async fn contacts(&self, one: Party, other: Party) {
        self.messaging
            .contact_request(one.user, other.user)
            .await
            .unwrap();
        self.messaging
            .contact_accept(other.user, one.user)
            .await
            .unwrap();
    }

    /// Attaches `device` of `user` on a new connection with a live presence lease.
    async fn online(&mut self, user: UserId, device: DeviceId) -> (Uuid, v1::CallSync) {
        let connection = Uuid::now_v7();
        let now = self.service.redis().now().await.unwrap();
        let _: () = redis::cmd("ZADD")
            .arg(presence_conns_key(user.0))
            .arg(now + 90_000)
            .arg(connection.to_string())
            .query_async(&mut self.admin)
            .await
            .unwrap();
        let sync = self
            .service
            .attach(device_request(user, device, connection))
            .await
            .unwrap();
        (connection, sync)
    }

    async fn subscribe(&self, users: &[UserId]) -> PubSub {
        let mut subscriber = redis::Client::open(self.url.as_str())
            .unwrap()
            .get_async_pubsub()
            .await
            .unwrap();
        for user in users {
            subscriber.subscribe(notify_channel(user.0)).await.unwrap();
        }
        subscriber
    }

    async fn send(&self, from: Party, frame: call_request::Frame) -> Result<CallReply, Status> {
        self.service
            .call(CallRequest {
                user_id: id_bytes(from.user.0),
                device_id: id_bytes(from.device.0),
                frame: Some(frame),
            })
            .await
    }

    async fn invite(&self, from: Party, to: UserId, call: Uuid) -> Result<CallReply, Status> {
        let invite = v1::CallInvite {
            call_id: id_bytes(call),
            callee_user_id: id_bytes(to.0),
            offer_sdp: OFFER.to_owned(),
            media: vec![CallMedia::Audio.into()],
        };
        self.send(from, call_request::Frame::Invite(invite)).await
    }
}

fn device_request(user: UserId, device: DeviceId, connection: Uuid) -> CallDeviceRequest {
    CallDeviceRequest {
        user_id: id_bytes(user.0),
        device_id: id_bytes(device.0),
        connection_id: id_bytes(connection),
    }
}

fn devices(devices: &[DeviceId]) -> Vec<Vec<u8>> {
    let mut ids: Vec<Vec<u8>> = devices.iter().map(|device| id_bytes(device.0)).collect();
    ids.sort();
    ids
}

/// The next call signal on `subscriber`: its user's channel, sorted devices and frame.
async fn next_signal(subscriber: &mut PubSub) -> (String, Vec<Vec<u8>>, call_signal::Frame) {
    let message = tokio::time::timeout(Duration::from_secs(5), subscriber.on_message().next())
        .await
        .expect("a call signal within 5 s")
        .expect("the subscription is open");
    let notification = Notification::decode(message.get_payload_bytes()).unwrap();
    let Some(Kind::Call(CallSignal {
        mut device_ids,
        frame,
    })) = notification.kind
    else {
        panic!("not a call signal");
    };
    device_ids.sort();
    (
        message.get_channel_name().to_owned(),
        device_ids,
        frame.unwrap(),
    )
}

async fn nothing_more(subscriber: &mut PubSub) {
    let next =
        tokio::time::timeout(Duration::from_millis(300), subscriber.on_message().next()).await;
    assert!(next.is_err(), "no further signal");
}

fn code(status: &Status) -> ErrorCode {
    error_from_status(status).code()
}

fn ended(call: Uuid, reason: CallEndReason) -> v1::CallEnded {
    v1::CallEnded {
        call_id: id_bytes(call),
        reason: reason.into(),
        duration_s: None,
    }
}

#[tokio::test]
async fn an_invite_rings_and_an_accept_connects() {
    let mut env = env(Timing::default()).await;
    let alice = env.user().await;
    let bob = env.user().await;
    let bob_laptop = DeviceId::new();
    env.messaging
        .device_registered(bob_laptop, bob.user)
        .await
        .unwrap();
    let stranger = env.user().await;
    env.contacts(alice, bob).await;
    let (_, sync) = env.online(alice.user, alice.device).await;
    assert!(sync.calls.is_empty());
    let (bob_phone_conn, _) = env.online(bob.user, bob.device).await;
    env.online(bob.user, bob_laptop).await;
    let mut signals = env.subscribe(&[alice.user, bob.user]).await;

    let call = Uuid::now_v7();
    let reply = env.invite(alice, bob.user, call).await.unwrap();
    assert_eq!(reply.ended, None);
    let (channel, to, frame) = next_signal(&mut signals).await;
    assert_eq!(channel, notify_channel(bob.user.0));
    assert_eq!(to, devices(&[bob.device, bob_laptop]));
    let call_signal::Frame::Incoming(incoming) = frame else {
        panic!("not CallIncoming");
    };
    assert_eq!(incoming.call_id, id_bytes(call));
    assert_eq!(incoming.caller_user_id, id_bytes(alice.user.0));
    assert_eq!(incoming.offer_sdp, OFFER);
    assert!((44_000..=45_000).contains(&incoming.expires_in_ms));

    // The invite sent again (its reply was lost) is accepted without effect.
    let reply = env.invite(alice, bob.user, call).await.unwrap();
    assert_eq!(reply.ended, None);

    // A reconnecting ringing device finds the call in its CallSync.
    let sync = env
        .service
        .attach(device_request(bob.user, bob.device, bob_phone_conn))
        .await
        .unwrap();
    assert_eq!(sync.calls.len(), 1);
    let state = &sync.calls[0];
    assert_eq!(state.call_id, id_bytes(call));
    assert_eq!(state.status(), v1::CallStatus::Ringing);
    assert_eq!(state.direction(), v1::CallDirection::In);
    assert_eq!(state.offer_sdp.as_deref(), Some(OFFER));
    assert!(state.this_device);

    // The caller's candidate waits for the answer.
    let ice = v1::CallIce {
        call_id: id_bytes(call),
        candidate: "candidate:1 1 udp 2122260223 192.0.2.1 54400 typ host".to_owned(),
        sdp_mid: Some("0".to_owned()),
        ..v1::CallIce::default()
    };
    env.send(alice, call_request::Frame::Ice(ice.clone()))
        .await
        .unwrap();
    // Nobody else can touch the call.
    let hangup = v1::CallHangup {
        call_id: id_bytes(call),
        reason: None,
    };
    let status = env
        .send(stranger, call_request::Frame::Hangup(hangup.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        (status.code(), code(&status)),
        (Code::NotFound, ErrorCode::CallNotFound)
    );
    nothing_more(&mut signals).await;

    let accept = v1::CallAccept {
        call_id: id_bytes(call),
        answer_sdp: ANSWER.to_owned(),
    };
    env.send(bob, call_request::Frame::Accept(accept.clone()))
        .await
        .unwrap();
    let (channel, to, frame) = next_signal(&mut signals).await;
    assert_eq!(
        (channel, to),
        (notify_channel(alice.user.0), devices(&[alice.device]))
    );
    assert_eq!(
        frame,
        call_signal::Frame::Accepted(v1::CallAccepted {
            call_id: id_bytes(call),
            answer_sdp: ANSWER.to_owned(),
            answered_device_id: id_bytes(bob.device.0),
        })
    );
    let (channel, to, frame) = next_signal(&mut signals).await;
    assert_eq!(
        (channel, to),
        (notify_channel(bob.user.0), devices(&[bob_laptop]))
    );
    assert!(matches!(frame, call_signal::Frame::Taken(_)));
    let (_, to, frame) = next_signal(&mut signals).await;
    assert_eq!(to, devices(&[bob.device]));
    assert_eq!(frame, call_signal::Frame::Ice(ice));

    // The accept sent again (its reply was lost) only repeats `CallAccepted`.
    env.send(bob, call_request::Frame::Accept(accept.clone()))
        .await
        .unwrap();
    let (channel, to, frame) = next_signal(&mut signals).await;
    assert_eq!(
        (channel, to),
        (notify_channel(alice.user.0), devices(&[alice.device]))
    );
    assert!(matches!(frame, call_signal::Frame::Accepted(_)));
    nothing_more(&mut signals).await;

    let late = Party {
        user: bob.user,
        device: bob_laptop,
    };
    let status = env
        .send(late, call_request::Frame::Accept(accept))
        .await
        .unwrap_err();
    assert_eq!(
        (status.code(), code(&status)),
        (Code::FailedPrecondition, ErrorCode::AlreadyAnswered)
    );

    env.send(alice, call_request::Frame::Hangup(hangup))
        .await
        .unwrap();
    let mut endings = Vec::new();
    for _ in 0..2 {
        let (channel, to, frame) = next_signal(&mut signals).await;
        let call_signal::Frame::Ended(ended) = frame else {
            panic!("not CallEnded");
        };
        assert_eq!(ended.reason(), CallEndReason::Completed);
        assert_eq!(ended.duration_s, Some(0));
        endings.push((channel, to));
    }
    assert_eq!(
        endings,
        [
            (notify_channel(alice.user.0), devices(&[alice.device])),
            (notify_channel(bob.user.0), devices(&[bob.device])),
        ]
    );
    assert_eq!(
        env.service.redis().user_call(alice.user).await.unwrap(),
        None
    );
    assert_eq!(env.service.redis().user_call(bob.user).await.unwrap(), None);

    let text = env.metrics.render(None);
    assert!(text.contains("push_calls_started_total 1\n"), "{text}");
    assert!(text.contains("push_calls_answered_total 1\n"), "{text}");
    assert!(text.contains("push_calls_ended_total{reason=\"completed\"} 1\n"));
    assert!(text.contains("push_call_frames_refused_total{code=\"call_not_found\"} 1\n"));
    assert!(text.contains("push_call_frames_refused_total{code=\"already_answered\"} 1\n"));
    assert!(text.contains("push_call_setup_seconds_count 1\n"));
    assert!(text.contains("push_call_signal_seconds_count{frame=\"invite\"} 2\n"));
}

#[tokio::test]
async fn a_busy_callee_gets_busy_reply() {
    let mut env = env(Timing::default()).await;
    let (alice, bob, carol, dave) = (
        env.user().await,
        env.user().await,
        env.user().await,
        env.user().await,
    );
    env.contacts(alice, bob).await;
    env.contacts(carol, bob).await;
    env.contacts(alice, carol).await;
    env.online(alice.user, alice.device).await;
    env.online(carol.user, carol.device).await;

    // Bob has no calls-active device yet.
    let unreachable = Uuid::now_v7();
    let reply = env.invite(alice, bob.user, unreachable).await.unwrap();
    assert_eq!(
        reply.ended,
        Some(ended(unreachable, CallEndReason::Unreachable))
    );

    env.online(bob.user, bob.device).await;
    let call = Uuid::now_v7();
    assert_eq!(env.invite(alice, bob.user, call).await.unwrap().ended, None);
    let busy = Uuid::now_v7();
    let reply = env.invite(carol, bob.user, busy).await.unwrap();
    assert_eq!(reply.ended, Some(ended(busy, CallEndReason::Busy)));

    let status = env
        .invite(alice, carol.user, Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(
        (status.code(), code(&status)),
        (Code::FailedPrecondition, ErrorCode::InCall)
    );
    let status = env
        .invite(dave, bob.user, Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(
        (status.code(), code(&status)),
        (Code::FailedPrecondition, ErrorCode::NotContact)
    );
    let status = env
        .invite(dave, UserId::new(), Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(code(&status), ErrorCode::NotContact);
    let status = env
        .invite(alice, alice.user, Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(
        (status.code(), code(&status)),
        (Code::InvalidArgument, ErrorCode::Invalid)
    );
    let status = env.invite(carol, alice.user, call).await.unwrap_err();
    assert_eq!(
        error_from_status(&status).message,
        "call_id is already used"
    );
    // Only the device that sent it may send an invite again.
    let alice_tablet = Party {
        user: alice.user,
        device: DeviceId::new(),
    };
    let status = env.invite(alice_tablet, bob.user, call).await.unwrap_err();
    assert_eq!(
        error_from_status(&status).message,
        "call_id is already used"
    );

    let text = env.metrics.render(None);
    assert!(text.contains("push_calls_ended_total{reason=\"busy\"} 1\n"));
    assert!(text.contains("push_calls_ended_total{reason=\"unreachable\"} 1\n"));
    assert!(text.contains("push_calls_started_total 1\n"));
}

#[tokio::test]
async fn the_sweeper_ends_a_missed_call() {
    let timing = Timing {
        ring_ms: 1_000,
        ..Timing::default()
    };
    let mut env = env(timing).await;
    let (alice, bob) = (env.user().await, env.user().await);
    env.contacts(alice, bob).await;
    env.online(alice.user, alice.device).await;
    env.online(bob.user, bob.device).await;
    let mut signals = env.subscribe(&[alice.user, bob.user]).await;
    let call = Uuid::now_v7();
    env.invite(alice, bob.user, call).await.unwrap();
    assert!(matches!(
        next_signal(&mut signals).await.2,
        call_signal::Frame::Incoming(_)
    ));

    assert_eq!(env.service.sweep_once().await.unwrap(), 0, "not due yet");
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert_eq!(env.service.sweep_once().await.unwrap(), 1);
    for user in [alice, bob] {
        let (channel, to, frame) = next_signal(&mut signals).await;
        assert_eq!(
            (channel, to),
            (notify_channel(user.user.0), devices(&[user.device]))
        );
        assert_eq!(
            frame,
            call_signal::Frame::Ended(ended(call, CallEndReason::Missed))
        );
    }
    assert_eq!(env.service.redis().live_count().await.unwrap(), 0);
    assert_eq!(env.service.redis().user_call(bob.user).await.unwrap(), None);
    assert!(
        env.metrics
            .render(Some(0))
            .contains("push_calls_ended_total{reason=\"missed\"} 1\n")
    );

    // The sweeper task stops on shutdown.
    let shutdown = tokio_util::sync::CancellationToken::new();
    let sweeper = {
        let service = Arc::clone(&env.service);
        let shutdown = shutdown.clone();
        tokio::spawn(async move { service.run_sweeper(shutdown).await })
    };
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), sweeper)
        .await
        .expect("the sweeper stops")
        .unwrap();
}

#[tokio::test]
async fn one_sweep_ticks_every_due_call() {
    const CALLS: usize = 200;
    let timing = Timing {
        ring_ms: 1_000,
        ..Timing::default()
    };
    let mut env = env(timing).await;
    for _ in 0..CALLS {
        let (alice, bob) = (env.user().await, env.user().await);
        env.contacts(alice, bob).await;
        env.online(bob.user, bob.device).await;
        env.invite(alice, bob.user, Uuid::now_v7()).await.unwrap();
    }
    assert_eq!(
        env.service.redis().live_count().await.unwrap(),
        CALLS as u64
    );
    tokio::time::sleep(Duration::from_millis(1_100)).await;

    // Far more than one claim's worth: the sweep claims again while claims come back full.
    let started = std::time::Instant::now();
    assert_eq!(env.service.sweep_once().await.unwrap(), CALLS);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(env.service.redis().live_count().await.unwrap(), 0);
    assert!(env.metrics.render(Some(0)).contains(&format!(
        "push_calls_ended_total{{reason=\"missed\"}} {CALLS}\n"
    )));
}

#[tokio::test]
async fn a_party_device_gone_past_the_grace_fails_the_call_and_a_reattach_resumes() {
    let timing = Timing {
        ring_ms: 45_000,
        grace_ms: 1_000,
        liveness_ms: 200,
    };
    let mut env = env(timing).await;
    let (alice, bob) = (env.user().await, env.user().await);
    env.contacts(alice, bob).await;
    env.online(alice.user, alice.device).await;
    let (bob_conn, _) = env.online(bob.user, bob.device).await;
    let call = Uuid::now_v7();
    env.invite(alice, bob.user, call).await.unwrap();
    let accept = v1::CallAccept {
        call_id: id_bytes(call),
        answer_sdp: ANSWER.to_owned(),
    };
    env.send(bob, call_request::Frame::Accept(accept))
        .await
        .unwrap();

    // A detach and a reattach within the grace resume the call.
    env.service
        .detach(device_request(bob.user, bob.device, bob_conn))
        .await
        .unwrap();
    let (_, sync) = env.online(bob.user, bob.device).await;
    assert_eq!(sync.calls.len(), 1);
    assert_eq!(sync.calls[0].status(), v1::CallStatus::Active);
    assert!(sync.calls[0].this_device);
    assert!(sync.calls[0].answered_at.is_some());

    // Bob's connection loses its presence lease (its gateway died).
    let _: () = redis::cmd("DEL")
        .arg(presence_conns_key(bob.user.0))
        .query_async(&mut env.admin)
        .await
        .unwrap();
    let mut signals = env.subscribe(&[alice.user, bob.user]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    env.service.sweep_once().await.unwrap();
    nothing_more(&mut signals).await;
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    env.service.sweep_once().await.unwrap();
    let (channel, _, frame) = next_signal(&mut signals).await;
    assert_eq!(channel, notify_channel(alice.user.0));
    let call_signal::Frame::Ended(ended) = frame else {
        panic!("not CallEnded");
    };
    assert_eq!(ended.reason(), CallEndReason::Failed);
    assert_eq!(
        env.service.redis().user_call(alice.user).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn a_tick_that_finds_an_away_device_live_again_keeps_the_call() {
    let timing = Timing {
        ring_ms: 45_000,
        grace_ms: 1_000,
        liveness_ms: 200,
    };
    let mut env = env(timing).await;
    let (alice, bob) = (env.user().await, env.user().await);
    env.contacts(alice, bob).await;
    env.online(alice.user, alice.device).await;
    let (bob_conn, _) = env.online(bob.user, bob.device).await;
    let call = Uuid::now_v7();
    env.invite(alice, bob.user, call).await.unwrap();
    let accept = v1::CallAccept {
        call_id: id_bytes(call),
        answer_sdp: ANSWER.to_owned(),
    };
    env.send(bob, call_request::Frame::Accept(accept))
        .await
        .unwrap();

    // An attach of a connected party device changes nothing but still bumps the version,
    // so a tick that read the registry before it re-reads.
    let (before, _) = env.service.redis().load(call).await.unwrap();
    env.service
        .attach(device_request(bob.user, bob.device, bob_conn))
        .await
        .unwrap();
    let (after, _) = env.service.redis().load(call).await.unwrap();
    let ((before_version, before_record), (after_version, after_record)) =
        (before.unwrap(), after.unwrap());
    assert_eq!(after_version, before_version + 1);
    assert_eq!(after_record, before_record);

    // A liveness tick finds Bob's lease gone and marks him away...
    let _: () = redis::cmd("DEL")
        .arg(presence_conns_key(bob.user.0))
        .query_async(&mut env.admin)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    env.service.sweep_once().await.unwrap();
    let (stored, _) = env.service.redis().load(call).await.unwrap();
    assert!(!stored.unwrap().1.contains("\"callee_away_ms\":null"));
    // ...but his connection is live again without an attach clearing it (the attach lost
    // the race to that tick): the next tick clears it and the grace end passes.
    let now = env.service.redis().now().await.unwrap();
    let _: () = redis::cmd("ZADD")
        .arg(presence_conns_key(bob.user.0))
        .arg(now + 90_000)
        .arg(bob_conn.to_string())
        .query_async(&mut env.admin)
        .await
        .unwrap();
    let mut signals = env.subscribe(&[alice.user, bob.user]).await;
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        env.service.sweep_once().await.unwrap();
    }
    nothing_more(&mut signals).await;
    assert_eq!(
        env.service.redis().user_call(alice.user).await.unwrap(),
        Some(call)
    );
    let (stored, _) = env.service.redis().load(call).await.unwrap();
    assert!(stored.unwrap().1.contains("\"callee_away_ms\":null"));
}

#[tokio::test]
async fn identify_offers_calls_only_with_the_engine() {
    let env = env(Timing::default()).await;
    let alice = env.user().await;
    let identify = || {
        Request::new(IdentifyRequest {
            user_id: id_bytes(alice.user.0),
            device_id: id_bytes(alice.device.0),
            issued_at: chrono::Utc::now().timestamp(),
        })
    };
    let plain = MessagingApi::new(Arc::clone(&env.messaging));
    let answer = plain.identify(identify()).await.unwrap().into_inner();
    assert!(answer.capabilities.is_empty());
    let request = CallRequest {
        user_id: id_bytes(alice.user.0),
        device_id: id_bytes(alice.device.0),
        frame: None,
    };
    let status = plain.call(Request::new(request.clone())).await.unwrap_err();
    assert_eq!(
        (status.code(), code(&status)),
        (Code::FailedPrecondition, ErrorCode::NotAvailable)
    );

    let calls: Arc<dyn CallsApi> = env.service.clone();
    let api = MessagingApi::new(Arc::clone(&env.messaging)).with_calls(calls);
    let answer = api.identify(identify()).await.unwrap().into_inner();
    assert_eq!(answer.capabilities, [i32::from(Capability::Calls)]);
    let status = api.call(Request::new(request)).await.unwrap_err();
    assert_eq!(code(&status), ErrorCode::Invalid, "a frame is required");
    let sync = api
        .call_attach(Request::new(device_request(
            alice.user,
            alice.device,
            Uuid::new_v4(),
        )))
        .await
        .unwrap()
        .into_inner();
    assert!(sync.calls.is_empty());
}

/// The next signal must be `CallHistoryChanged`: its channel, sorted devices and
/// `latest_ended_at`.
async fn next_history_changed(subscriber: &mut PubSub) -> (String, Vec<Vec<u8>>, Option<i64>) {
    let (channel, to, frame) = next_signal(subscriber).await;
    let call_signal::Frame::HistoryChanged(changed) = frame else {
        panic!("not CallHistoryChanged: {frame:?}");
    };
    (channel, to, changed.latest_ended_at)
}

async fn history_of(env: &Env, user: UserId) -> Vec<CallHistoryRow> {
    env.messaging
        .call_history(user, None, None, 0)
        .await
        .unwrap()
}

/// `user`'s history once it holds `len` calls: rows are written in the background.
async fn history_with(env: &Env, user: UserId, len: usize) -> Vec<CallHistoryRow> {
    for _ in 0..100 {
        let rows = history_of(env, user).await;
        if rows.len() == len {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no {len} calls in the history within 5 s");
}

fn history_api(
    env: &Env,
    notifier: RedisNotifier,
) -> (
    Arc<History>,
    CallHistoryApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>,
) {
    let history = Arc::new(CallHistory::new(
        Arc::clone(&env.messaging),
        notifier,
        Arc::clone(&env.metrics),
    ));
    (Arc::clone(&history), CallHistoryApi::new(history))
}

type History = CallHistory<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;

#[tokio::test]
async fn an_ended_call_is_in_both_histories_and_announced_to_their_devices() {
    let mut env = env(Timing::default()).await;
    let (alice, bob, carol) = (env.user().await, env.user().await, env.user().await);
    let bob_laptop = DeviceId::new();
    env.messaging
        .device_registered(bob_laptop, bob.user)
        .await
        .unwrap();
    // Alice's tablet is registered but not calls-active: it gets nothing.
    env.messaging
        .device_registered(DeviceId::new(), alice.user)
        .await
        .unwrap();
    env.contacts(alice, bob).await;
    env.online(alice.user, alice.device).await;
    env.online(bob.user, bob.device).await;
    env.online(bob.user, bob_laptop).await;
    let mut signals = env.subscribe(&[alice.user, bob.user, carol.user]).await;

    let call = Uuid::now_v7();
    env.invite(alice, bob.user, call).await.unwrap();
    assert!(matches!(
        next_signal(&mut signals).await.2,
        call_signal::Frame::Incoming(_)
    ));
    let accept = v1::CallAccept {
        call_id: id_bytes(call),
        answer_sdp: ANSWER.to_owned(),
    };
    env.send(bob, call_request::Frame::Accept(accept))
        .await
        .unwrap();
    for _ in 0..2 {
        next_signal(&mut signals).await;
    }
    // Nothing is written while the call lives.
    assert!(history_of(&env, alice.user).await.is_empty());
    let hangup = v1::CallHangup {
        call_id: id_bytes(call),
        reason: None,
    };
    env.send(bob, call_request::Frame::Hangup(hangup))
        .await
        .unwrap();
    for _ in 0..2 {
        assert!(matches!(
            next_signal(&mut signals).await.2,
            call_signal::Frame::Ended(_)
        ));
    }
    let (channel, to, alice_latest) = next_history_changed(&mut signals).await;
    assert_eq!(
        (channel, to),
        (notify_channel(alice.user.0), devices(&[alice.device]))
    );
    let (channel, to, bob_latest) = next_history_changed(&mut signals).await;
    assert_eq!(
        (channel, to),
        (
            notify_channel(bob.user.0),
            devices(&[bob.device, bob_laptop])
        )
    );
    nothing_more(&mut signals).await;

    let mine = history_of(&env, alice.user).await;
    assert_eq!(mine.len(), 1);
    let row = &mine[0];
    assert_eq!(row.call_id, call);
    assert_eq!((row.user, row.peer), (alice.user, bob.user));
    assert_eq!(row.direction, CallDirection::Out);
    assert_eq!(row.outcome, EndReason::Completed);
    assert!(row.started_at <= row.answered_at.unwrap());
    assert!(row.answered_at.unwrap() <= row.ended_at);
    assert_eq!(row.duration_s, 0);
    assert_eq!(alice_latest, Some(row.ended_at.timestamp_micros()));
    assert_eq!(bob_latest, alice_latest);
    let theirs = history_of(&env, bob.user).await;
    assert_eq!(theirs.len(), 1);
    assert_eq!(
        (theirs[0].peer, theirs[0].direction),
        (alice.user, CallDirection::In)
    );
    assert_eq!(theirs[0].ended_at, row.ended_at);
    assert!(history_of(&env, carol.user).await.is_empty());
}

#[tokio::test]
async fn calls_that_never_rang_are_in_the_history() {
    let mut env = env(Timing::default()).await;
    let (alice, bob, carol) = (env.user().await, env.user().await, env.user().await);
    env.contacts(alice, bob).await;
    env.contacts(carol, bob).await;
    env.online(alice.user, alice.device).await;
    env.online(carol.user, carol.device).await;

    // Unreachable: only the caller's row; the callee never saw the call.
    let unreachable = Uuid::now_v7();
    env.invite(alice, bob.user, unreachable).await.unwrap();
    let mine = history_with(&env, alice.user, 1).await;
    assert_eq!(mine.len(), 1);
    assert_eq!(
        (mine[0].call_id, mine[0].outcome),
        (unreachable, EndReason::Unreachable)
    );
    assert_eq!(mine[0].started_at, mine[0].ended_at);
    assert!(history_of(&env, bob.user).await.is_empty());

    env.online(bob.user, bob.device).await;
    let mut signals = env.subscribe(&[bob.user, carol.user]).await;
    let call = Uuid::now_v7();
    env.invite(alice, bob.user, call).await.unwrap();
    assert!(matches!(
        next_signal(&mut signals).await.2,
        call_signal::Frame::Incoming(_)
    ));
    // Busy: both rows, and both users' devices are told.
    let busy = Uuid::now_v7();
    env.invite(carol, bob.user, busy).await.unwrap();
    let (channel, to, _) = next_history_changed(&mut signals).await;
    assert_eq!(
        (channel, to),
        (notify_channel(carol.user.0), devices(&[carol.device]))
    );
    let (channel, to, _) = next_history_changed(&mut signals).await;
    assert_eq!(
        (channel, to),
        (notify_channel(bob.user.0), devices(&[bob.device]))
    );
    let theirs = history_of(&env, bob.user).await;
    assert_eq!(theirs.len(), 1);
    assert_eq!(
        (
            theirs[0].call_id,
            theirs[0].peer,
            theirs[0].direction,
            theirs[0].outcome
        ),
        (busy, carol.user, CallDirection::In, EndReason::Busy)
    );
    assert_eq!(
        history_of(&env, carol.user).await[0].direction,
        CallDirection::Out
    );

    // The ringing call is cancelled: newest first.
    let cancel = v1::CallCancel {
        call_id: id_bytes(call),
    };
    env.send(alice, call_request::Frame::Cancel(cancel))
        .await
        .unwrap();
    let theirs = history_with(&env, bob.user, 2).await;
    assert_eq!(
        theirs
            .iter()
            .map(|row| (row.call_id, row.outcome))
            .collect::<Vec<_>>(),
        [(call, EndReason::Cancelled), (busy, EndReason::Busy)]
    );
    let mine = history_with(&env, alice.user, 2).await;
    assert_eq!(mine[0].outcome, EndReason::Cancelled);
}

#[tokio::test]
async fn calls_an_agent_starts_are_marked_in_both_histories() {
    let mut env = env(Timing::default()).await;
    let (owner, friend) = (env.user().await, env.user().await);
    let agent = Party {
        user: UserId::new(),
        device: DeviceId::new(),
    };
    env.messaging
        .enroll_agent(owner.user, agent.user)
        .await
        .unwrap();
    env.messaging
        .device_registered(agent.device, agent.user)
        .await
        .unwrap();
    env.online(agent.user, agent.device).await;
    env.online(owner.user, owner.device).await;

    // To the owner: always allowed, marked on both rows.
    let call = Uuid::now_v7();
    assert_eq!(
        env.invite(agent, owner.user, call).await.unwrap().ended,
        None
    );
    let cancel = v1::CallCancel {
        call_id: id_bytes(call),
    };
    env.send(agent, call_request::Frame::Cancel(cancel))
        .await
        .unwrap();
    let mine = history_with(&env, agent.user, 1).await;
    let theirs = history_with(&env, owner.user, 1).await;
    assert!(mine[0].started_by_agent && theirs[0].started_by_agent);

    // The owner's call to the agent is not.
    let call = Uuid::now_v7();
    env.invite(owner, agent.user, call).await.unwrap();
    let cancel = v1::CallCancel {
        call_id: id_bytes(call),
    };
    env.send(owner, call_request::Frame::Cancel(cancel))
        .await
        .unwrap();
    let theirs = history_with(&env, owner.user, 2).await;
    assert_eq!(theirs[0].call_id, call);
    assert!(!theirs[0].started_by_agent);

    // To a contact under `allowed`, not reachable: the caller's row is marked.
    env.messaging
        .set_outgoing_policy(
            owner.user,
            agent.user,
            OutgoingPolicyChange {
                requests: Some(OutgoingPolicy::Allowed),
                calls: Some(OutgoingPolicy::Allowed),
                ..OutgoingPolicyChange::default()
            },
        )
        .await
        .unwrap();
    env.contacts(agent, friend).await;
    let call = Uuid::now_v7();
    assert_eq!(
        env.invite(agent, friend.user, call).await.unwrap().ended,
        Some(ended(call, CallEndReason::Unreachable))
    );
    let mine = history_with(&env, agent.user, 3).await;
    assert_eq!(mine[0].call_id, call);
    assert!(mine[0].started_by_agent);
}

#[tokio::test]
async fn an_agents_call_to_a_contact_follows_its_owners_call_policy() {
    let env = env(Timing::default()).await;
    let (owner, friend) = (env.user().await, env.user().await);
    let agent = Party {
        user: UserId::new(),
        device: DeviceId::new(),
    };
    env.messaging
        .enroll_agent(owner.user, agent.user)
        .await
        .unwrap();
    env.messaging
        .device_registered(agent.device, agent.user)
        .await
        .unwrap();
    let policy = |calls| OutgoingPolicyChange {
        requests: Some(OutgoingPolicy::Allowed),
        calls: Some(calls),
        ..OutgoingPolicyChange::default()
    };
    env.messaging
        .set_outgoing_policy(owner.user, agent.user, policy(OutgoingPolicy::Off))
        .await
        .unwrap();
    env.contacts(agent, friend).await;

    let status = env
        .invite(agent, friend.user, Uuid::now_v7())
        .await
        .unwrap_err();
    let error = error_from_status(&status);
    assert_eq!(
        (status.code(), error.code(), error.action_id),
        (Code::FailedPrecondition, ErrorCode::NotAllowedByOwner, None)
    );

    env.messaging
        .set_outgoing_policy(owner.user, agent.user, policy(OutgoingPolicy::Ask))
        .await
        .unwrap();
    let status = env
        .invite(agent, friend.user, Uuid::now_v7())
        .await
        .unwrap_err();
    let error = error_from_status(&status);
    assert_eq!(
        (status.code(), error.code()),
        (Code::FailedPrecondition, ErrorCode::WaitingForOwner)
    );
    let waiting = env
        .messaging
        .waiting_outgoing(owner.user, agent.user)
        .await
        .unwrap();
    assert_eq!(error.action_id, Some(id_bytes(waiting[0].action_id)));
    // Approved: the agent may ring that person once.
    env.messaging
        .approve_outgoing(owner.user, agent.user, waiting[0].action_id)
        .await
        .unwrap();
    let call = Uuid::now_v7();
    assert_eq!(
        env.invite(agent, friend.user, call).await.unwrap().ended,
        Some(ended(call, CallEndReason::Unreachable))
    );
    let status = env
        .invite(agent, friend.user, Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(code(&status), ErrorCode::WaitingForOwner);

    env.messaging
        .set_outgoing_policy(owner.user, agent.user, policy(OutgoingPolicy::Allowed))
        .await
        .unwrap();
    // Ten calls a day; the approved one and the queued one counted already.
    let mut refused = None;
    for _ in 0..10 {
        if let Err(status) = env.invite(agent, friend.user, Uuid::now_v7()).await {
            refused = Some(status);
            break;
        }
    }
    let status = refused.expect("the eleventh call of the day is refused");
    let error = error_from_status(&status);
    assert_eq!(
        (status.code(), error.code()),
        (Code::ResourceExhausted, ErrorCode::RateLimited)
    );
    assert!(error.retry_after_s.is_some_and(|seconds| seconds > 0));
}

/// An agent of a new owner, enrolled with one device, a contact of `friend`.
async fn agent_of(env: &Env, owner: Party, friend: Party, calls: OutgoingPolicy) -> Party {
    let agent = Party {
        user: UserId::new(),
        device: DeviceId::new(),
    };
    env.messaging
        .enroll_agent(owner.user, agent.user)
        .await
        .unwrap();
    env.messaging
        .device_registered(agent.device, agent.user)
        .await
        .unwrap();
    env.messaging
        .set_outgoing_policy(
            owner.user,
            agent.user,
            OutgoingPolicyChange {
                requests: Some(OutgoingPolicy::Allowed),
                calls: Some(calls),
                ..OutgoingPolicyChange::default()
            },
        )
        .await
        .unwrap();
    env.contacts(agent, friend).await;
    agent
}

#[tokio::test]
async fn a_resent_invite_of_an_approved_agent_call_is_not_gated_again() {
    let mut env = env(Timing::default()).await;
    let (owner, friend) = (env.user().await, env.user().await);
    let agent = agent_of(&env, owner, friend, OutgoingPolicy::Ask).await;
    env.online(agent.user, agent.device).await;
    env.online(friend.user, friend.device).await;
    let status = env
        .invite(agent, friend.user, Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(code(&status), ErrorCode::WaitingForOwner);
    let waiting = env
        .messaging
        .waiting_outgoing(owner.user, agent.user)
        .await
        .unwrap();
    env.messaging
        .approve_outgoing(owner.user, agent.user, waiting[0].action_id)
        .await
        .unwrap();
    let call = Uuid::now_v7();
    assert_eq!(
        env.invite(agent, friend.user, call).await.unwrap().ended,
        None
    );
    env.notifier.take();

    // Its reply was lost: the same invite again is accepted without effect.
    assert_eq!(
        env.invite(agent, friend.user, call).await.unwrap(),
        CallReply::default()
    );
    assert!(
        env.messaging
            .waiting_outgoing(owner.user, agent.user)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(env.notifier.take().is_empty(), "the owner hears nothing");
}

#[tokio::test]
async fn a_resent_invite_of_an_allowed_agent_call_counts_once() {
    let mut env = env(Timing::default()).await;
    let (owner, friend) = (env.user().await, env.user().await);
    let agent = agent_of(&env, owner, friend, OutgoingPolicy::Allowed).await;
    env.online(agent.user, agent.device).await;
    env.online(friend.user, friend.device).await;
    let cancel = |call| {
        call_request::Frame::Cancel(v1::CallCancel {
            call_id: id_bytes(call),
        })
    };

    let call = Uuid::now_v7();
    assert_eq!(
        env.invite(agent, friend.user, call).await.unwrap().ended,
        None
    );
    for _ in 0..3 {
        assert_eq!(
            env.invite(agent, friend.user, call).await.unwrap(),
            CallReply::default()
        );
    }
    env.send(agent, cancel(call)).await.unwrap();

    // Ten calls a day: the resent one counted once, nine more ring.
    for _ in 0..9 {
        let call = Uuid::now_v7();
        assert_eq!(
            env.invite(agent, friend.user, call).await.unwrap().ended,
            None
        );
        env.send(agent, cancel(call)).await.unwrap();
    }
    let status = env
        .invite(agent, friend.user, Uuid::now_v7())
        .await
        .unwrap_err();
    assert_eq!(code(&status), ErrorCode::RateLimited);
}

#[tokio::test]
async fn the_history_api_shows_own_calls_only_and_hides_per_side() {
    let env = env(Timing::default()).await;
    let (alice, bob, carol) = (env.user().await, env.user().await, env.user().await);
    let (_, api) = history_api(&env, RedisNotifier::connect(&env.url).await.unwrap());
    let ended_ms = chrono::Utc::now().timestamp_millis();
    let mut calls = Vec::new();
    for offset in [3_000, 2_000, 1_000] {
        let call = Uuid::now_v7();
        let rows = ended_rows(
            call,
            alice.user,
            bob.user,
            ended_ms - offset - 20_000,
            Some(ended_ms - offset - 10_000),
            ended_ms - offset,
            EndReason::Completed,
            offset == 1_000,
        );
        env.messaging.record_call(&rows).await.unwrap();
        calls.push(call);
    }
    calls.reverse();
    let request = |user: UserId, before: Option<CallHistoryCursor>, since, limit| {
        Request::new(CallHistoryRequest {
            user_id: id_bytes(user.0),
            before,
            since,
            limit,
        })
    };
    let page = api
        .history(request(alice.user, None, None, 0))
        .await
        .unwrap()
        .into_inner();
    let ids: Vec<Vec<u8>> = page
        .calls
        .iter()
        .map(|entry| entry.call_id.clone())
        .collect();
    assert_eq!(
        ids,
        calls.iter().map(|call| id_bytes(*call)).collect::<Vec<_>>()
    );
    let newest = &page.calls[0];
    assert_eq!(newest.peer_user_id, id_bytes(bob.user.0));
    assert_eq!(newest.direction(), v1::CallDirection::Out);
    assert_eq!(newest.outcome(), CallEndReason::Completed);
    assert_eq!(newest.duration_s, 10);
    assert_eq!(newest.ended_at, (ended_ms - 1_000) * 1000);
    assert_eq!(newest.answered_at, Some((ended_ms - 11_000) * 1000));
    assert!(newest.started_by_agent);
    assert!(!page.calls[1].started_by_agent);
    assert_eq!(newest.started_at, (ended_ms - 21_000) * 1000);
    let cursor = |entry: &CallHistoryEntry| CallHistoryCursor {
        ended_at: entry.ended_at,
        call_id: entry.call_id.clone(),
    };
    let older = api
        .history(request(alice.user, Some(cursor(newest)), None, 1))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(older.calls, page.calls[1..2]);
    let newer = api
        .history(request(alice.user, None, Some(cursor(&page.calls[2])), 0))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(newer.calls, page.calls[..2]);
    let status = api
        .history(request(
            alice.user,
            Some(CallHistoryCursor {
                ended_at: 0,
                call_id: vec![1, 2, 3],
            }),
            None,
            0,
        ))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        api.history(request(carol.user, None, None, 0))
            .await
            .unwrap()
            .into_inner()
            .calls
            .is_empty()
    );

    // Hiding: carol cannot hide alice's call; alice hides only her own row.
    let mut env = env;
    env.online(alice.user, alice.device).await;
    env.online(bob.user, bob.device).await;
    let mut signals = env.subscribe(&[alice.user, bob.user]).await;
    let hide = |user: UserId, call: Uuid| {
        Request::new(HideCallRequest {
            user_id: id_bytes(user.0),
            call_id: id_bytes(call),
        })
    };
    api.hide(hide(carol.user, calls[0])).await.unwrap();
    api.hide(hide(alice.user, Uuid::now_v7())).await.unwrap();
    nothing_more(&mut signals).await;
    assert_eq!(history_of(&env, alice.user).await.len(), 3);
    api.hide(hide(alice.user, calls[0])).await.unwrap();
    let (channel, to, latest) = next_history_changed(&mut signals).await;
    assert_eq!(
        (channel, to, latest),
        (notify_channel(alice.user.0), devices(&[alice.device]), None)
    );
    // Hiding again changes nothing and tells nobody.
    api.hide(hide(alice.user, calls[0])).await.unwrap();
    nothing_more(&mut signals).await;
    let mine: Vec<Uuid> = history_of(&env, alice.user)
        .await
        .iter()
        .map(|row| row.call_id)
        .collect();
    assert_eq!(mine, calls[1..]);
    assert_eq!(history_of(&env, bob.user).await.len(), 3);
}

#[tokio::test]
async fn the_purge_deletes_only_rows_past_the_retention() {
    let env = env(Timing::default()).await;
    let (alice, bob) = (env.user().await, env.user().await);
    let (history, _) = history_api(&env, RedisNotifier::connect(&env.url).await.unwrap());
    let now_ms = chrono::Utc::now().timestamp_millis();
    let day_ms = 24 * 60 * 60 * 1000;
    let old = Uuid::now_v7();
    let kept = Uuid::now_v7();
    for (call, age_days) in [(old, 91), (kept, 89)] {
        let ended = now_ms - age_days * day_ms;
        let rows = ended_rows(
            call,
            alice.user,
            bob.user,
            ended,
            None,
            ended,
            EndReason::Missed,
            false,
        );
        env.messaging.record_call(&rows).await.unwrap();
    }
    env.messaging.hide_call(bob.user, old).await.unwrap();

    assert_eq!(history.purge(90).await.unwrap(), 2);
    for user in [alice.user, bob.user] {
        let rows = history_of(&env, user).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_id, kept);
    }
    assert_eq!(history.purge(90).await.unwrap(), 0);
    assert!(
        env.metrics
            .render_history()
            .contains("push_call_history_rows_deleted_total 2\n")
    );
    assert_eq!(history.purge(1).await.unwrap(), 2);
    assert!(history_of(&env, alice.user).await.is_empty());
}
