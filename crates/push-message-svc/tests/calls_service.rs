//! The call engine against a real Redis (Docker must be running) and an in-memory store.

use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use push_domain::calls::Timing;
use push_domain::{DeviceId, InMemoryStore, Messaging, RecordingNotifier, SystemClock, UserId};
use push_message_svc::{CallService, CallsApi, MessagingApi, Metrics, RedisCalls, RedisNotifier};
use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
use push_proto::internal::v1::{
    CallDeviceRequest, CallReply, CallRequest, CallSignal, IdentifyRequest, Notification,
    call_request, call_signal, notification::Kind,
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
    let messaging = Arc::new(Messaging::new(
        InMemoryStore::default(),
        SystemClock,
        Arc::new(RecordingNotifier::default()),
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
