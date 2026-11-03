//! push-sdk's calls API (docs/calls.md) against the whole stack in this process: SDK
//! clients sign in with verified password accounts, become contacts and call each other
//! through the gateway and message-svc's call engine, read their call history from
//! auth-svc, and fetch ICE servers in stun-only and TURN REST mode. Docker must be running.

mod common;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{Containers, Datastores, Stack};
use push_load::accounts::Account as Credentials;
use push_sdk::{
    CallDirection, CallEndReason, CallErrorCode, CallEvent, CallId, CallLogEntry, CallState,
    CallStatus, Client, Config, ConnectionState, ContactState, DeviceId, Event, Events,
    IceCandidate, MemorySecretStore, Platform, SdkError, SdpKind, UserId,
};

const OFFER: &str =
    "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
const ANSWER: &str =
    "v=0\r\no=- 2 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
const NEW_OFFER: &str =
    "v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
const NEW_ANSWER: &str =
    "v=0\r\no=- 2 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
const STUN_URL: &str = "stun:stun.push.test:3478";
const TURN_URLS: [&str; 2] = [
    "turn:turn.push.test:3478?transport=udp",
    "turns:turn.push.test:5349?transport=tcp",
];
const TURN_SECRET: &str = "a-turn-rest-shared-secret-for-the-sdk-test";
const TURN_TTL_SECONDS: i64 = 600;
/// How long any one wait may take before the test fails.
const WAIT: Duration = Duration::from_secs(30);

fn variables(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// The stack with calls enabled in message-svc and the gateway, and auth-svc's TURN
/// variables `turn`.
async fn start(turn: &[(&str, &str)]) -> (Stack, Containers) {
    let (mut datastores, containers): (Datastores, Containers) =
        common::password_datastores().await;
    datastores
        .message_svc
        .extend(variables(&[("PUSH_CALLS_ENABLED", "1")]));
    datastores.gateway.extend(variables(&[
        ("PUSH_CALLS_ENABLED", "1"),
        ("PUSH_CALLS_ATTEMPTS_PER_HOUR", "1000"),
    ]));
    datastores.auth.extend(variables(turn));
    (Stack::start(datastores).await, containers)
}

fn stun_only() -> Vec<(&'static str, &'static str)> {
    vec![
        ("PUSH_TURN_MODE", "stun-only"),
        ("PUSH_TURN_STUN_URLS", STUN_URL),
    ]
}

/// The SDK apps' databases, fresh for every test.
fn data_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("push-calls-sdk-{test}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn accounts(stack: &Stack, count: usize) -> Vec<Credentials> {
    push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), count)
        .await
        .expect("verified accounts")
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// One app profile: a client, its events and the events a wait passed over, in order.
struct App {
    name: String,
    client: Client,
    events: Events,
    backlog: VecDeque<Event>,
    user: UserId,
    device: DeviceId,
    calls: bool,
}

impl App {
    /// Opens a client (with `calls` advertised or not) and signs it in, registering a
    /// device of its own; not connected.
    async fn sign_in(
        stack: &Stack,
        dir: &Path,
        name: &str,
        credentials: &Credentials,
        calls: bool,
    ) -> Self {
        let database = dir.join(format!("{name}.db"));
        let mut config = Config::new(
            &stack.auth_url,
            &stack.socket_url,
            &database.to_string_lossy(),
        );
        config.allow_insecure_transport = true;
        config.calls = calls;
        let (client, events) = Client::open(config, Arc::new(MemorySecretStore::new())).unwrap();
        let account = client
            .sign_in(
                &credentials.email,
                &credentials.password,
                &format!("{name}'s laptop"),
                Platform::Desktop,
            )
            .await
            .unwrap_or_else(|error| panic!("{name} signs in: {error:?}"));
        Self {
            name: name.to_owned(),
            client,
            events,
            backlog: VecDeque::new(),
            user: account.user_id,
            device: account.device_id,
            calls,
        }
    }

    /// Signs in and connects.
    async fn start(
        stack: &Stack,
        dir: &Path,
        name: &str,
        credentials: &Credentials,
        calls: bool,
    ) -> Self {
        let mut app = Self::sign_in(stack, dir, name, credentials, calls).await;
        app.connect().await;
        app
    }

    /// Connects and waits until the connection serves; with calls, until the call
    /// registry knows it (its `CallSync`, after which it rings), returning that sync.
    /// A device just registered is closed 4006 until the worker relays it: the SDK
    /// reconnects by itself.
    async fn connect(&mut self) -> Option<(Vec<CallState>, Vec<CallId>)> {
        self.client.connect().unwrap();
        if !self.calls {
            self.wait("Connected", |event| {
                matches!(
                    event,
                    Event::ConnectionStateChanged(ConnectionState::Connected)
                )
            })
            .await;
            assert!(!self.client.calls().available());
            return None;
        }
        let CallEvent::Synced { calls, gone } = self
            .wait_call("the CallSync", |event| {
                matches!(event, CallEvent::Synced { .. })
            })
            .await
        else {
            unreachable!("the wait accepts only Synced");
        };
        assert!(
            self.client.calls().available(),
            "{}: calls available",
            self.name
        );
        Some((calls, gone))
    }

    /// The first event `wanted` takes, from those passed over before or new ones.
    async fn wait(&mut self, what: &str, wanted: impl Fn(&Event) -> bool) -> Event {
        if let Some(index) = self.backlog.iter().position(&wanted) {
            return self.backlog.remove(index).unwrap();
        }
        let (events, backlog) = (&mut self.events, &mut self.backlog);
        tokio::time::timeout(WAIT, async {
            loop {
                let event = events.next().await.expect("the client is open");
                if wanted(&event) {
                    return event;
                }
                backlog.push_back(event);
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{}: no {what} within {WAIT:?}", self.name))
    }

    async fn wait_call(&mut self, what: &str, wanted: impl Fn(&CallEvent) -> bool) -> CallEvent {
        match self
            .wait(
                what,
                |event| matches!(event, Event::Call(call) if wanted(call)),
            )
            .await
        {
            Event::Call(call) => call,
            _ => unreachable!("the wait accepts only Call"),
        }
    }

    /// How `call` ended here, and its duration.
    async fn ended(&mut self, call: CallId) -> (CallEndReason, Option<u32>) {
        match self
            .wait_call(
                "the call's end",
                |event| matches!(event, CallEvent::Ended { call_id, .. } if *call_id == call),
            )
            .await
        {
            CallEvent::Ended {
                reason, duration_s, ..
            } => (reason, duration_s),
            _ => unreachable!(),
        }
    }

    /// The refusal of a frame for `call`.
    async fn refused(&mut self, call: CallId) -> (CallErrorCode, SdkError) {
        match self
            .wait_call(
                "the call's error",
                |event| matches!(event, CallEvent::Error { call_id, .. } if *call_id == call),
            )
            .await
        {
            CallEvent::Error { code, error, .. } => (code, error),
            _ => unreachable!(),
        }
    }

    /// The next peer candidate for `call`.
    async fn peer_ice(&mut self, call: CallId) -> IceCandidate {
        match self
            .wait_call(
                "a peer candidate",
                |event| matches!(event, CallEvent::PeerIce { call_id, .. } if *call_id == call),
            )
            .await
        {
            CallEvent::PeerIce { candidate, .. } => candidate,
            _ => unreachable!(),
        }
    }

    /// The next peer renegotiation for `call`.
    async fn peer_sdp(&mut self, call: CallId) -> (String, SdpKind) {
        match self
            .wait_call(
                "a peer renegotiation",
                |event| matches!(event, CallEvent::PeerRenegotiate { call_id, .. } if *call_id == call),
            )
            .await
        {
            CallEvent::PeerRenegotiate { sdp, kind, .. } => (sdp, kind),
            _ => unreachable!(),
        }
    }

    /// The call events that arrived so far, and within `quiet`.
    async fn call_events_within(&mut self, quiet: Duration) -> Vec<CallEvent> {
        let deadline = tokio::time::Instant::now() + quiet;
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.events.next()).await {
            self.backlog.push_back(event);
        }
        self.backlog
            .iter()
            .filter_map(|event| match event {
                Event::Call(call) => Some(call.clone()),
                _ => None,
            })
            .collect()
    }

    /// The history entry of `call` once it is recorded.
    async fn history_entry(&self, call: CallId) -> CallLogEntry {
        let deadline = Instant::now() + WAIT;
        loop {
            let page = self.client.calls().history(None, None, None).await.unwrap();
            if let Some(entry) = page.calls.into_iter().find(|entry| entry.call_id == call) {
                return entry;
            }
            assert!(
                Instant::now() < deadline,
                "{}: the call is not in the history",
                self.name
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn lists(&self, call: CallId) -> bool {
        let page = self.client.calls().history(None, None, None).await.unwrap();
        page.calls.iter().any(|entry| entry.call_id == call)
    }

    async fn close(self) {
        self.client.close().await.unwrap();
    }
}

/// `a` and `b` become accepted contacts.
async fn befriend(a: &mut App, b: &mut App) {
    a.client.request_contact(b.user).await.unwrap();
    let accepted = b.client.accept_contact(a.user).await.unwrap();
    assert_eq!(accepted.state, ContactState::Accepted);
    let peer = b.user;
    a.wait("the accepted contact", |event| {
        matches!(event, Event::ContactChanged(contact)
            if contact.user_id == peer && contact.state == ContactState::Accepted)
    })
    .await;
}

/// `caller` calls `callee` until it rings on `callee`'s device: a device whose
/// connection the call engine does not know yet makes the invite end `Unreachable`.
async fn ring(caller: &mut App, callee: &mut App) -> CallId {
    let (caller_user, callee_user) = (caller.user, callee.user);
    for _ in 0..40 {
        let call = caller
            .client
            .calls()
            .invite(callee_user, OFFER)
            .await
            .unwrap();
        let ringing_at = now_ms();
        tokio::select! {
            (reason, _) = caller.ended(call) => {
                assert_eq!(reason, CallEndReason::Unreachable, "the invite ended {reason:?}");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            incoming = callee.wait_call(
                "the incoming call",
                |event| matches!(event, CallEvent::Incoming { call_id, .. } if *call_id == call),
            ) => {
                assert_incoming(&incoming, call, caller_user, ringing_at);
                return call;
            }
        }
    }
    panic!("the call never rang");
}

fn assert_incoming(incoming: &CallEvent, call: CallId, caller: UserId, ringing_at: i64) {
    let CallEvent::Incoming {
        call_id,
        caller_id,
        offer_sdp,
        expires_at_ms,
    } = incoming
    else {
        panic!("not an incoming call: {incoming:?}");
    };
    assert_eq!(*call_id, call);
    assert_eq!(*caller_id, caller);
    assert_eq!(offer_sdp, OFFER, "the offer arrives intact");
    assert!(
        *expires_at_ms > ringing_at,
        "it rings for a while: {expires_at_ms} after {ringing_at}"
    );
}

fn candidate(n: u32) -> IceCandidate {
    IceCandidate {
        candidate: format!("candidate:{n} 1 udp 2122260223 192.0.2.{n} 5432{n} typ host"),
        sdp_mid: Some("0".to_owned()),
        sdp_mline_index: Some(0),
        username_fragment: Some(format!("frag{n}")),
        end_of_candidates: false,
    }
}

/// `caller`'s ringing call is answered by `callee`, and `caller` gets the answer.
async fn answer(caller: &mut App, callee: &mut App, call: CallId) {
    callee.client.calls().accept(call, ANSWER).await.unwrap();
    let accepted = caller
        .wait_call(
            "the answer",
            |event| matches!(event, CallEvent::Accepted { call_id, .. } if *call_id == call),
        )
        .await;
    let CallEvent::Accepted {
        answer_sdp,
        answered_device_id,
        ..
    } = accepted
    else {
        unreachable!();
    };
    assert_eq!(answer_sdp, ANSWER, "the answer arrives intact");
    assert_eq!(answered_device_id, callee.device);
}

/// `app` waits for a `HistoryChanged` naming a call that ended at `ended_at_ms` or later
/// (`Some`), or a hidden call (`None`).
async fn history_changed(app: &mut App, ended_at_ms: Option<i64>) {
    app.wait_call("the history change", |event| match (event, ended_at_ms) {
        (
            CallEvent::HistoryChanged {
                latest_ended_at_ms: Some(latest),
            },
            Some(ended),
        ) => *latest + 1 >= ended,
        (
            CallEvent::HistoryChanged {
                latest_ended_at_ms: None,
            },
            None,
        ) => true,
        _ => false,
    })
    .await;
}

fn assert_not_available(result: Result<(), SdkError>, what: &str) {
    match result {
        Err(SdkError::Server { code, .. }) if code == "not_available" => {}
        other => panic!("{what}: {other:?}"),
    }
}

/// Alice calls Bob, who has two devices: both ring, Bob's first device answers and his
/// second stops ringing, candidates trickle both ways (Alice's buffered while ringing),
/// both sides renegotiate, Alice hangs up. Then the call is in both histories; Bob hides
/// it from his.
#[tokio::test]
async fn two_sdk_clients_complete_a_call() {
    let started = Instant::now();
    let (stack, _containers) = start(&stun_only()).await;
    let dir = data_dir("complete");
    let credentials = accounts(&stack, 2).await;
    let mut alice = App::start(&stack, &dir, "alice", &credentials[0], true).await;
    let mut bob1 = App::start(&stack, &dir, "bob1", &credentials[1], true).await;
    let mut bob2 = App::start(&stack, &dir, "bob2", &credentials[1], true).await;
    assert_eq!(bob1.user, bob2.user);
    assert_ne!(bob1.device, bob2.device);
    befriend(&mut alice, &mut bob1).await;

    let servers = alice.client.calls().ice_servers().await.unwrap();
    assert!(!servers.relay, "stun-only has no relay");
    assert_eq!(servers.servers.len(), 1, "{servers:?}");
    assert_eq!(servers.servers[0].urls, [STUN_URL]);
    assert_eq!(servers.servers[0].username, None);
    assert_eq!(servers.servers[0].credential, None);

    let ringing_at = now_ms();
    let call = ring(&mut alice, &mut bob1).await;
    let incoming = bob2
        .wait_call(
            "the incoming call",
            |event| matches!(event, CallEvent::Incoming { call_id, .. } if *call_id == call),
        )
        .await;
    assert_incoming(&incoming, call, alice.user, ringing_at);

    // Alice trickles while it rings: the server buffers her candidates for the answerer.
    let alice_candidates: Vec<IceCandidate> = (1..=3).map(candidate).collect();
    for candidate in &alice_candidates {
        alice
            .client
            .calls()
            .send_ice(call, candidate.clone())
            .await
            .unwrap();
    }

    answer(&mut alice, &mut bob1, call).await;
    bob2.wait_call(
        "the call taken",
        |event| matches!(event, CallEvent::Taken { call_id } if *call_id == call),
    )
    .await;
    for sent in &alice_candidates {
        assert_eq!(&bob1.peer_ice(call).await, sent, "in order, intact");
    }

    let mut bob_candidates: Vec<IceCandidate> = (4..=5).map(candidate).collect();
    bob_candidates.push(IceCandidate::end_of_candidates());
    for candidate in &bob_candidates {
        bob1.client
            .calls()
            .send_ice(call, candidate.clone())
            .await
            .unwrap();
    }
    for sent in &bob_candidates {
        assert_eq!(&alice.peer_ice(call).await, sent, "in order, intact");
    }

    alice
        .client
        .calls()
        .renegotiate(call, NEW_OFFER, SdpKind::Offer)
        .await
        .unwrap();
    assert_eq!(
        bob1.peer_sdp(call).await,
        (NEW_OFFER.to_owned(), SdpKind::Offer)
    );
    bob1.client
        .calls()
        .renegotiate(call, NEW_ANSWER, SdpKind::Answer)
        .await
        .unwrap();
    assert_eq!(
        alice.peer_sdp(call).await,
        (NEW_ANSWER.to_owned(), SdpKind::Answer)
    );

    alice.client.calls().hangup(call, false).await.unwrap();
    for app in [&mut alice, &mut bob1] {
        let (reason, duration) = app.ended(call).await;
        assert_eq!(reason, CallEndReason::Completed, "{}", app.name);
        assert!(
            duration.is_some(),
            "{}: an answered call has a duration",
            app.name
        );
    }
    let bob2_events = bob2.call_events_within(Duration::ZERO).await;
    assert!(
        !bob2_events
            .iter()
            .any(|event| matches!(event, CallEvent::Ended { call_id, .. } if *call_id == call)),
        "the call was taken from Bob's second device: {bob2_events:?}"
    );

    let outgoing = alice.history_entry(call).await;
    let incoming = bob1.history_entry(call).await;
    for (entry, direction, peer) in [
        (&outgoing, CallDirection::Outgoing, bob1.user),
        (&incoming, CallDirection::Incoming, alice.user),
    ] {
        assert_eq!(entry.direction, direction);
        assert_eq!(entry.peer_user_id, peer);
        assert_eq!(entry.outcome, CallEndReason::Completed);
        assert!(entry.answered_at_ms.is_some());
        assert!(entry.started_at_ms <= entry.ended_at_ms);
    }
    for app in [&mut alice, &mut bob1, &mut bob2] {
        history_changed(app, Some(outgoing.ended_at_ms)).await;
    }

    bob1.client.calls().hide(call).await.unwrap();
    assert!(!bob1.lists(call).await, "Bob hid the call");
    assert!(!bob2.lists(call).await, "hidden on all Bob's devices");
    assert!(alice.lists(call).await, "Alice still sees it");
    history_changed(&mut bob2, None).await;

    for app in [alice, bob1, bob2] {
        app.close().await;
    }
    stack.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("two_sdk_clients_complete_a_call: {:?}", started.elapsed());
}

/// Every way a call ends without being completed, and the refusals: declined, declined
/// as busy, cancelled, busy, unreachable (the callee's only client takes no calls), not a
/// contact, already in a call; and a client without calls has none.
#[tokio::test]
async fn declined_cancelled_busy_and_unreachable_calls_end_as_such() {
    let started = Instant::now();
    let (stack, _containers) = start(&stun_only()).await;
    let dir = data_dir("endings");
    let credentials = accounts(&stack, 4).await;
    let mut alice = App::start(&stack, &dir, "alice", &credentials[0], true).await;
    let mut bob = App::start(&stack, &dir, "bob", &credentials[1], true).await;
    let mut carol = App::start(&stack, &dir, "carol", &credentials[2], true).await;
    let mut dave = App::start(&stack, &dir, "dave", &credentials[3], false).await;
    befriend(&mut alice, &mut bob).await;
    befriend(&mut alice, &mut carol).await;
    befriend(&mut alice, &mut dave).await;
    befriend(&mut carol, &mut bob).await;

    for (busy, expected) in [
        (false, CallEndReason::Declined),
        (true, CallEndReason::Busy),
    ] {
        let call = ring(&mut alice, &mut bob).await;
        bob.client.calls().decline(call, busy).await.unwrap();
        assert_eq!(alice.ended(call).await, (expected, None));
        assert_eq!(bob.ended(call).await, (expected, None));
    }

    let call = ring(&mut alice, &mut bob).await;
    alice.client.calls().cancel(call).await.unwrap();
    assert_eq!(alice.ended(call).await, (CallEndReason::Cancelled, None));
    assert_eq!(bob.ended(call).await, (CallEndReason::Cancelled, None));

    // Alice, ringing Bob, cannot call anyone else.
    let call = ring(&mut alice, &mut bob).await;
    let second = alice
        .client
        .calls()
        .invite(carol.user, OFFER)
        .await
        .unwrap();
    let (code, error) = alice.refused(second).await;
    assert_eq!(code, CallErrorCode::InCall, "{error:?}");
    assert!(
        matches!(&error, SdkError::Server { code, .. } if code == "in_call"),
        "{error:?}"
    );
    // Bob, in a call with Alice, is busy for Carol.
    answer(&mut alice, &mut bob, call).await;
    let busy = carol.client.calls().invite(bob.user, OFFER).await.unwrap();
    assert_eq!(carol.ended(busy).await, (CallEndReason::Busy, None));
    alice.client.calls().hangup(call, false).await.unwrap();
    assert_eq!(alice.ended(call).await.0, CallEndReason::Completed);
    assert_eq!(bob.ended(call).await.0, CallEndReason::Completed);

    // Dave's only client takes no calls: unreachable at once, and he hears nothing.
    let unreachable = alice.client.calls().invite(dave.user, OFFER).await.unwrap();
    assert_eq!(
        alice.ended(unreachable).await,
        (CallEndReason::Unreachable, None)
    );
    let heard = dave.call_events_within(Duration::from_secs(1)).await;
    assert!(heard.is_empty(), "dave got call events: {heard:?}");

    // Carol and Dave are not contacts.
    let stranger = carol.client.calls().invite(dave.user, OFFER).await.unwrap();
    let (code, error) = carol.refused(stranger).await;
    assert_eq!(code, CallErrorCode::NotContact, "{error:?}");

    // Without calls advertised, the API is there but unavailable.
    let calls = dave.client.calls();
    assert!(!calls.available());
    match calls.invite(alice.user, OFFER).await {
        Err(SdkError::Server { code, .. }) if code == "not_available" => {}
        other => panic!("invite: {other:?}"),
    }
    assert_not_available(calls.accept(call, ANSWER).await, "accept");
    assert_not_available(calls.decline(call, false).await, "decline");
    assert_not_available(calls.cancel(call).await, "cancel");
    assert_not_available(calls.hangup(call, false).await, "hangup");
    assert_not_available(calls.send_ice(call, candidate(1)).await, "send_ice");
    assert_not_available(
        calls.renegotiate(call, NEW_OFFER, SdpKind::Offer).await,
        "renegotiate",
    );
    drop(calls);

    for app in [alice, bob, carol, dave] {
        app.close().await;
    }
    stack.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!(
        "declined_cancelled_busy_and_unreachable_calls_end_as_such: {:?}",
        started.elapsed()
    );
}

/// Bob's device drops out of an active call and comes back within the disconnect grace:
/// the sync lists the call as his, active, to restart ICE on; the call goes on.
#[tokio::test]
async fn call_sync_after_a_reconnect_lists_the_active_call() {
    let started = Instant::now();
    let (stack, _containers) = start(&stun_only()).await;
    let dir = data_dir("sync");
    let credentials = accounts(&stack, 2).await;
    let mut alice = App::start(&stack, &dir, "alice", &credentials[0], true).await;
    let mut bob = App::start(&stack, &dir, "bob", &credentials[1], true).await;
    befriend(&mut alice, &mut bob).await;

    let call = ring(&mut alice, &mut bob).await;
    answer(&mut alice, &mut bob, call).await;

    bob.client.disconnect().unwrap();
    bob.wait("Offline", |event| {
        matches!(
            event,
            Event::ConnectionStateChanged(ConnectionState::Offline)
        )
    })
    .await;
    assert!(!bob.client.calls().available());
    let (calls, gone) = bob.connect().await.expect("a CallSync");
    assert!(gone.is_empty(), "{gone:?}");
    assert_eq!(calls.len(), 1, "{calls:?}");
    let state = &calls[0];
    assert_eq!(state.call_id, call);
    assert_eq!(state.peer_user_id, alice.user);
    assert_eq!(state.direction, CallDirection::Incoming);
    assert_eq!(state.status, CallStatus::Active);
    assert!(state.this_device);
    assert!(state.needs_ice_restart());
    assert!(state.answered_at_ms.is_some());
    assert_eq!(state.offer_sdp, None);

    // The ICE restart reaches Alice: the call goes on.
    bob.client
        .calls()
        .renegotiate(call, NEW_OFFER, SdpKind::Offer)
        .await
        .unwrap();
    assert_eq!(
        alice.peer_sdp(call).await,
        (NEW_OFFER.to_owned(), SdpKind::Offer)
    );
    alice.client.calls().hangup(call, false).await.unwrap();
    for app in [&mut alice, &mut bob] {
        let (reason, duration) = app.ended(call).await;
        assert_eq!(reason, CallEndReason::Completed, "{}", app.name);
        assert!(duration.is_some());
    }

    for app in [alice, bob] {
        app.close().await;
    }
    stack.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!(
        "call_sync_after_a_reconnect_lists_the_active_call: {:?}",
        started.elapsed()
    );
}

/// TURN REST mode: the relay's credentials are `<expiry>:<user id>` and
/// `base64(HMAC-SHA1(secret, username))`, valid for the configured lifetime.
#[tokio::test]
async fn ice_servers_hand_out_turn_rest_credentials() {
    use base64::Engine as _;
    use hmac::{Hmac, Mac};

    let started = Instant::now();
    let turn_urls = TURN_URLS.join(",");
    let ttl = TURN_TTL_SECONDS.to_string();
    let (stack, _containers) = start(&[
        ("PUSH_TURN_MODE", "internal"),
        ("PUSH_TURN_STUN_URLS", STUN_URL),
        ("PUSH_TURN_URLS", &turn_urls),
        ("PUSH_TURN_SHARED_SECRET", TURN_SECRET),
        ("PUSH_TURN_TTL_SECONDS", &ttl),
    ])
    .await;
    let dir = data_dir("turn");
    let credentials = accounts(&stack, 1).await;
    // Only a signed-in client: no connection needed.
    let alice = App::sign_in(&stack, &dir, "alice", &credentials[0], true).await;

    let fetched_at = now_ms();
    let servers = alice.client.calls().ice_servers().await.unwrap();
    assert!(servers.relay);
    let expected_expiry = fetched_at + TURN_TTL_SECONDS * 1000;
    assert!(
        (servers.expires_at_ms - expected_expiry).abs() < 30_000,
        "expires at {} rather than about {expected_expiry}",
        servers.expires_at_ms
    );
    let stun = servers
        .servers
        .iter()
        .find(|server| server.urls == [STUN_URL])
        .expect("the STUN entry");
    assert_eq!((&stun.username, &stun.credential), (&None, &None));
    let turn = servers
        .servers
        .iter()
        .find(|server| server.urls == TURN_URLS)
        .unwrap_or_else(|| panic!("the TURN entry: {servers:?}"));
    let username = turn.username.clone().expect("a TURN username");
    let credential = turn.credential.clone().expect("a TURN credential");
    let (expiry, user) = username.split_once(':').expect("<expiry>:<user id>");
    assert_eq!(user, alice.user.0.to_string());
    let expiry: i64 = expiry.parse().expect("unix seconds");
    assert_eq!(expiry, servers.expires_at_ms / 1000);
    assert_eq!(servers.expires_at_ms % 1000, 0, "whole seconds");
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(TURN_SECRET.as_bytes()).unwrap();
    mac.update(username.as_bytes());
    let expected = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    assert_eq!(credential, expected);

    let again = alice.client.calls().ice_servers().await.unwrap();
    let again_turn = again
        .servers
        .iter()
        .find(|server| server.urls == TURN_URLS)
        .expect("the TURN entry again");
    let again_username = again_turn.username.as_deref().unwrap();
    assert!(again_username.ends_with(&format!(":{}", alice.user.0)));
    assert!(again.expires_at_ms >= servers.expires_at_ms);

    alice.close().await;
    stack.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!(
        "ice_servers_hand_out_turn_rest_credentials: {:?}",
        started.elapsed()
    );
}
