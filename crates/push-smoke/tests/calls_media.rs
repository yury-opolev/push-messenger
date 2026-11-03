//! The media helper's smoke test (docs/calls.md, "Media helper"): the whole stack in this
//! process with calls enabled, two SDK clients with `MediaCall` (feature `calls-media`)
//! that call each other and exchange audio: the caller sends a 440 Hz tone, the callee
//! an 880 Hz one, each checks the other's tone in what it hears, the caller hangs up and
//! both calls end. Three ways: `stun-only` (host candidates connect the two local
//! clients), and relay-only through the chart's pinned coturn in Docker, reached over UDP
//! and over TCP, with TURN REST credentials minted by auth-svc.
//!
//! Opt in with `PUSH_CALLS_MEDIA=1` (otherwise the tests pass without running): it needs
//! Docker. CI runs it in the `calls-media` job.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{Containers, Stack};
use push_load::accounts::Account as Credentials;
use push_sdk::media::{
    EndCause, FRAME_SAMPLES, MediaCall, MediaOptions, MediaState, PcmFrame, PcmStream, Role,
    SAMPLE_RATE,
};
use push_sdk::{
    CallEndReason, CallEvent, Client, Config, ContactState, Event, Events, MemorySecretStore,
    Platform, UserId,
};
use tokio::sync::mpsc;

const TURN_SECRET: &str = "the-coturn-shared-secret-of-the-media-test";
/// How long any one wait may take before the test fails.
const WAIT: Duration = Duration::from_secs(30);

fn enabled() -> bool {
    if std::env::var("PUSH_CALLS_MEDIA").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipped: set PUSH_CALLS_MEDIA=1 to run the media helper's smoke test");
    false
}

/// The stack with calls enabled and auth-svc's TURN settings `turn`.
async fn stack(turn: &[(&str, &str)]) -> (Stack, Containers) {
    let (mut datastores, containers) = common::calls_datastores().await;
    datastores
        .gateway
        .push(("PUSH_CALLS_ATTEMPTS_PER_HOUR".to_owned(), "1000".to_owned()));
    datastores.auth.extend(
        turn.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    (Stack::start(datastores).await, containers)
}

fn data_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("push-calls-media-{test}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One signed-in, connected client with calls, its call events split off its events.
struct Peer {
    name: &'static str,
    client: Client,
    user: UserId,
    calls: mpsc::UnboundedReceiver<CallEvent>,
}

impl Peer {
    async fn start(stack: &Stack, dir: &Path, name: &'static str, account: &Credentials) -> Self {
        let database = dir.join(format!("{name}.db"));
        let mut config = Config::new(
            &stack.auth_url,
            &stack.socket_url,
            &database.to_string_lossy(),
        );
        config.allow_insecure_transport = true;
        config.calls = true;
        let (client, mut events) =
            Client::open(config, Arc::new(MemorySecretStore::new())).unwrap();
        let signed_in = client
            .sign_in(
                &account.email,
                &account.password,
                &format!("{name}'s agent"),
                Platform::Desktop,
            )
            .await
            .unwrap_or_else(|error| panic!("{name} signs in: {error:?}"));
        client.connect().unwrap();
        // A device just registered is closed until the worker relays it: the SDK
        // reconnects by itself, and its first CallSync means the call registry knows it.
        wait(&mut events, name, "the CallSync", |event| {
            matches!(event, Event::Call(CallEvent::Synced { .. }))
        })
        .await;
        let (sender, calls) = mpsc::unbounded_channel();
        tokio::spawn(pump(events, sender));
        Self {
            name,
            client,
            user: signed_in.user_id,
            calls,
        }
    }

    async fn next_call_event(&mut self) -> CallEvent {
        tokio::time::timeout(WAIT, self.calls.recv())
            .await
            .unwrap_or_else(|_| panic!("{}: no call event within {WAIT:?}", self.name))
            .expect("the client is open")
    }
}

async fn wait(events: &mut Events, name: &str, what: &str, wanted: impl Fn(&Event) -> bool) {
    tokio::time::timeout(WAIT, async {
        while let Some(event) = events.next().await {
            if wanted(&event) {
                return;
            }
        }
        panic!("{name}: the client closed");
    })
    .await
    .unwrap_or_else(|_| panic!("{name}: no {what} within {WAIT:?}"));
}

/// The client's call events, in order, until it closes.
async fn pump(mut events: Events, calls: mpsc::UnboundedSender<CallEvent>) {
    while let Some(event) = events.next().await {
        if let Event::Call(call) = event {
            let _ = calls.send(call);
        }
    }
}

/// Every call event of `peer` to `call`, until the client closes.
fn forward(call: Arc<MediaCall>, mut events: mpsc::UnboundedReceiver<CallEvent>) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            call.handle(&event);
        }
    });
}

/// Two accounts, contacts of each other, signed in and connected.
async fn peers(stack: &Stack, test: &str) -> (Peer, Peer) {
    let accounts = push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), 2)
        .await
        .expect("two verified accounts");
    let dir = data_dir(test);
    let alice = Peer::start(stack, &dir, "alice", &accounts[0]).await;
    let bob = Peer::start(stack, &dir, "bob", &accounts[1]).await;
    alice.client.request_contact(bob.user).await.unwrap();
    let accepted = bob.client.accept_contact(alice.user).await.unwrap();
    assert_eq!(accepted.state, ContactState::Accepted);
    (alice, bob)
}

/// Alice calls Bob with the media helper until it rings (a device's presence lease is
/// written alongside its call attach, so a first invite may end `Unreachable`), and Bob
/// answers with it. Each fetches its ICE servers, as an app does per call.
async fn place_call(alice: &mut Peer, bob: &mut Peer, relay: bool) -> (MediaCall, MediaCall) {
    for _ in 0..40 {
        let servers = alice.client.calls().ice_servers().await.unwrap();
        // With a relay the helper uses relay-only ICE: it has no other candidate.
        assert_eq!(servers.relay, relay);
        let caller = MediaCall::start(
            &alice.client.calls(),
            &servers,
            Role::Caller { peer: bob.user },
            MediaOptions::default(),
        )
        .await
        .expect("Alice's call starts");
        let rung = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    event = alice.next_call_event() => {
                        caller.handle(&event);
                        if let MediaState::Ended(ended) = caller.state() {
                            assert_eq!(ended.reason, CallEndReason::Unreachable, "{ended:?}");
                            return None;
                        }
                    }
                    event = bob.next_call_event() => match event {
                        CallEvent::Incoming { call_id, offer_sdp, .. } if call_id == caller.call_id() => {
                            return Some((call_id, offer_sdp));
                        }
                        _ => {}
                    }
                }
            }
        })
        .await;
        let Ok(Some((call_id, offer_sdp))) = rung else {
            drop(caller);
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        };
        let servers = bob.client.calls().ice_servers().await.unwrap();
        let callee = MediaCall::start(
            &bob.client.calls(),
            &servers,
            Role::Callee { call_id, offer_sdp },
            MediaOptions::default(),
        )
        .await
        .expect("Bob answers");
        return (caller, callee);
    }
    panic!("the call never rang");
}

fn tone(frequency: f64, index: usize) -> PcmFrame {
    let rate = f64::from(SAMPLE_RATE);
    let samples: Vec<i16> = (index * FRAME_SAMPLES..(index + 1) * FRAME_SAMPLES)
        .map(|n| {
            (8_000.0 * (2.0 * std::f64::consts::PI * frequency * n as f64 / rate).sin()) as i16
        })
        .collect();
    PcmFrame::from_slice(&samples).unwrap()
}

/// The amplitude of the `frequency` component of `signal` (Goertzel).
fn amplitude(signal: &[i16], frequency: f64) -> f64 {
    let omega = 2.0 * std::f64::consts::PI * frequency / f64::from(SAMPLE_RATE);
    let coefficient = 2.0 * omega.cos();
    let (mut previous, mut before) = (0.0f64, 0.0f64);
    for &sample in signal {
        let current = f64::from(sample) + coefficient * previous - before;
        before = previous;
        previous = current;
    }
    let power = previous * previous + before * before - coefficient * previous * before;
    2.0 * power.max(0.0).sqrt() / signal.len().max(1) as f64
}

/// 3 s of `frequency` into `call`'s sink (queued at once: the helper sends 20 ms per
/// 20 ms).
async fn speak(call: &MediaCall, frequency: f64) {
    let sink = call.outgoing();
    for index in 0..150 {
        assert!(
            sink.send(tone(frequency, index)).await,
            "the call is running"
        );
    }
}

/// 1.6 s of what arrives, after the first 0.4 s (the decoders settle).
async fn listen(stream: &mut PcmStream) -> Vec<i16> {
    let mut heard = Vec::new();
    for index in 0..100 {
        let frame = tokio::time::timeout(Duration::from_secs(10), stream.recv())
            .await
            .expect("audio arrives")
            .expect("the call is running");
        if index >= 20 {
            heard.extend_from_slice(frame.samples());
        }
    }
    heard
}

/// A whole call between two media helpers: it connects, each hears the other's tone and
/// not its own, the caller hangs up, both calls end and both streams close.
async fn call_and_talk(stack: &Stack, test: &str, relay: bool) {
    let (mut alice, mut bob) = peers(stack, test).await;
    let (mut caller, mut callee) = place_call(&mut alice, &mut bob, relay).await;
    let (mut alice_hears, mut bob_hears) = (caller.incoming().unwrap(), callee.incoming().unwrap());
    let (caller, callee) = (Arc::new(caller), Arc::new(callee));
    let alice_events = std::mem::replace(&mut alice.calls, mpsc::unbounded_channel().1);
    let bob_events = std::mem::replace(&mut bob.calls, mpsc::unbounded_channel().1);
    forward(caller.clone(), alice_events);
    forward(callee.clone(), bob_events);
    tokio::time::timeout(WAIT, async {
        caller.connected().await.expect("Alice's media connects");
        callee.connected().await.expect("Bob's media connects");
    })
    .await
    .expect("the media connects in time");

    let ((), (), at_bob, at_alice) = tokio::join!(
        speak(&caller, 440.0),
        speak(&callee, 880.0),
        listen(&mut bob_hears),
        listen(&mut alice_hears),
    );
    let (bob_440, bob_880) = (amplitude(&at_bob, 440.0), amplitude(&at_bob, 880.0));
    eprintln!("Bob hears 440 Hz at {bob_440:.0}, 880 Hz at {bob_880:.0}");
    assert!(
        bob_440 > 2_000.0 && bob_880 < bob_440 / 10.0,
        "Bob hears Alice's tone: {bob_440:.0} {bob_880:.0}"
    );
    let (alice_880, alice_440) = (amplitude(&at_alice, 880.0), amplitude(&at_alice, 440.0));
    eprintln!("Alice hears 880 Hz at {alice_880:.0}, 440 Hz at {alice_440:.0}");
    assert!(
        alice_880 > 2_000.0 && alice_440 < alice_880 / 10.0,
        "Alice hears Bob's tone: {alice_880:.0} {alice_440:.0}"
    );

    let ended = caller.hangup().await;
    assert_eq!(
        (ended.reason, ended.cause),
        (CallEndReason::Completed, EndCause::Local)
    );
    let ended = tokio::time::timeout(WAIT, callee.on_ended())
        .await
        .expect("Bob's call ends");
    assert_eq!(
        (ended.reason, ended.cause),
        (CallEndReason::Completed, EndCause::Server)
    );
    assert!(
        ended.duration_s.is_some(),
        "an answered call has a duration"
    );
    assert!(bob_hears.recv().await.is_none(), "Bob's stream ends");
    assert!(alice_hears.recv().await.is_none(), "Alice's stream ends");

    drop((caller, callee));
    alice.client.close().await.unwrap();
    bob.client.close().await.unwrap();
}

#[tokio::test]
async fn media_helpers_talk_with_stun_only() {
    if !enabled() {
        return;
    }
    // No STUN server answers here: host candidates connect the two local clients.
    let (stack, _containers) = stack(&[
        ("PUSH_TURN_MODE", "stun-only"),
        ("PUSH_TURN_STUN_URLS", "stun:127.0.0.1:3478"),
    ])
    .await;
    tokio::time::timeout(
        Duration::from_secs(240),
        call_and_talk(&stack, "stun", false),
    )
    .await
    .expect("the call finishes within 240 s");
    stack.stop().await;
}

async fn through_coturn(transport: &str) {
    let coturn = common::coturn::start(TURN_SECRET).await;
    let port = match transport {
        "udp" => coturn.udp_port,
        _ => coturn.tcp_port,
    };
    let urls = format!("turn:127.0.0.1:{port}?transport={transport}");
    let (stack, _containers) = stack(&[
        ("PUSH_TURN_MODE", "internal"),
        ("PUSH_TURN_URLS", &urls),
        ("PUSH_TURN_SHARED_SECRET", TURN_SECRET),
    ])
    .await;
    tokio::time::timeout(
        Duration::from_secs(240),
        call_and_talk(&stack, transport, true),
    )
    .await
    .expect("the call finishes within 240 s");
    stack.stop().await;
}

/// Relay-only (the servers include a relay): each side's only candidate is its coturn
/// allocation, so the audio crosses coturn twice.
#[tokio::test]
async fn media_helpers_talk_through_coturn_over_udp() {
    if !enabled() {
        return;
    }
    through_coturn("udp").await;
}

#[tokio::test]
async fn media_helpers_talk_through_coturn_over_tcp() {
    if !enabled() {
        return;
    }
    through_coturn("tcp").await;
}
