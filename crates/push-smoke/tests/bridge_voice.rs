//! The agent bridge answers its owner's call end to end (docs/agent-bridge.md, "Voice
//! calls"): the whole stack in this process with calls and agent-key sign-in, the owner on
//! an SDK client placing the call with push-sdk's media helper, and the agent enrolled with
//! a software key running the bridge as `push-agent-bridge run` does (an `SdkMessenger`,
//! `MediaPhone`), with a scripted backend and the tone-based speech fakes. Over real Opus
//! media the owner hears the greeting and the spoken replies, its recorded fixtures become
//! a prompt, a steer that cuts the reply short (barge-in) and a voice approval; a stranger's
//! call never reaches the bridge, and the owner hangs up.
//!
//! Opt in with `PUSH_BRIDGE_VOICE=1` (otherwise the test passes without running): it needs
//! Docker. CI runs it in the `bridge-voice` job.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Containers, Stack};
use push_agent_key::software::SoftwareKey;
use push_bridge::backend::{Answer, Backend, RequestKind, TurnSource};
use push_bridge::core::{Bridge, BridgeError, BridgeOptions};
use push_bridge::folders;
use push_bridge::messenger::{SdkMessenger, param, template};
use push_bridge::state::StateFile;
use push_bridge::test_backend::TestBackend;
use push_bridge::voice::VoiceSetup;
use push_bridge::voice::fake::{FakeStt, FakeTts, Fixture, TTS_HZ, amplitude};
use push_bridge::voice::media::MediaPhone;
use push_bridge::voice::phone::{Line, Phone};
use push_bridge::voice::vad::VadConfig;
use push_load::accounts::Account as Credentials;
use push_sdk::media::{
    EndCause, FRAME_SAMPLES, MediaCall, MediaOptions, MediaState, PcmFrame, PcmStream, Role,
};
use push_sdk::{
    ActivityState, CallEndReason, CallErrorCode, CallEvent, CallId, Client, Config, ContactState,
    Event, Events, MemorySecretStore, Message, MessageKind, Platform, SdkError, UserId,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

/// How long any one wait may take before the test fails.
const WAIT: Duration = Duration::from_secs(30);
/// The agent-key audience auth-svc checks.
const AUDIENCE: &str = "push-smoke-bridge-voice";
/// The amplitude of the agent's 600 Hz voice that counts as heard (it speaks at 6000)…
const HEARD: f64 = 1_500.0;
/// …and below which it is quiet.
const QUIET: f64 = 500.0;
/// One measuring window: 100 ms.
const WINDOW: usize = 5;

fn enabled() -> bool {
    if std::env::var("PUSH_BRIDGE_VOICE").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipped: set PUSH_BRIDGE_VOICE=1 to run the bridge's voice smoke test");
    false
}

fn step(text: &str) {
    eprintln!("[bridge-voice] {text}");
}

/// The stack with calls (STUN only: host candidates connect the local clients) and
/// agent-key sign-in, one agent per owner at least.
async fn stack() -> (Stack, Containers) {
    let (mut datastores, containers) = common::calls_datastores().await;
    datastores
        .gateway
        .push(("PUSH_CALLS_ATTEMPTS_PER_HOUR".to_owned(), "1000".to_owned()));
    for (name, value) in [
        ("PUSH_TURN_MODE", "stun-only"),
        ("PUSH_TURN_STUN_URLS", "stun:127.0.0.1:3478"),
        ("PUSH_AUTH_PROVIDERS", "password,agent-key"),
        ("PUSH_AUTH_AGENT_KEY_AUDIENCE", AUDIENCE),
        ("PUSH_AGENTS_PER_USER", "2"),
    ] {
        datastores.auth.push((name.to_owned(), value.to_owned()));
    }
    (Stack::start(datastores).await, containers)
}

fn scratch() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("push-bridge-voice-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(dir.join("root").join("work")).unwrap();
    dir
}

fn client(stack: &Stack, dir: &Path, name: &str) -> (Client, Events) {
    let database = dir.join(format!("{name}.db"));
    let mut config = Config::new(
        &stack.auth_url,
        &stack.socket_url,
        &database.to_string_lossy(),
    );
    config.allow_insecure_transport = true;
    config.calls = true;
    Client::open(config, Arc::new(MemorySecretStore::new())).unwrap()
}

async fn within<F: std::future::Future>(what: &str, future: F) -> F::Output {
    tokio::time::timeout(WAIT, future)
        .await
        .unwrap_or_else(|_| panic!("{what}: not within {WAIT:?}"))
}

/// Polls `check` every 20 ms until it gives a value.
async fn until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    within(what, async {
        loop {
            if let Some(value) = check() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
}

/// A message of the agent's, as the owner's client reported it.
#[derive(Clone)]
struct Seen {
    edited: bool,
    message: Message,
}

impl Seen {
    fn body(&self) -> &str {
        self.message.body.as_deref().unwrap_or_default()
    }
}

/// What the owner's client reported about the agent: its messages and activity.
#[derive(Default)]
struct Log {
    messages: Vec<Seen>,
    activity: Vec<(Instant, ActivityState, Option<String>)>,
}

/// One signed-in, connected human with calls: its call events split off, and what it
/// sees of `watched` (the agent) logged.
struct Person {
    name: &'static str,
    client: Client,
    user: UserId,
    calls: Option<mpsc::UnboundedReceiver<CallEvent>>,
    log: Arc<Mutex<Log>>,
    watched: Arc<Mutex<Option<UserId>>>,
}

impl Person {
    async fn start(stack: &Stack, dir: &Path, name: &'static str, account: &Credentials) -> Self {
        let (client, mut events) = client(stack, dir, name);
        let signed_in = client
            .sign_in(
                &account.email,
                &account.password,
                &format!("{name}'s phone"),
                Platform::Desktop,
            )
            .await
            .unwrap_or_else(|error| panic!("{name} signs in: {error:?}"));
        client.connect().unwrap();
        // A device just registered is closed until the worker relays it: the SDK
        // reconnects by itself, and its first CallSync means the call registry knows it.
        within(&format!("{name}'s first CallSync"), async {
            while let Some(event) = events.next().await {
                if matches!(event, Event::Call(CallEvent::Synced { .. })) {
                    return;
                }
            }
            panic!("{name}: the client closed");
        })
        .await;
        let (sender, calls) = mpsc::unbounded_channel();
        let log = Arc::new(Mutex::new(Log::default()));
        let watched = Arc::new(Mutex::new(None));
        tokio::spawn(pump(events, sender, log.clone(), watched.clone()));
        Self {
            name,
            client,
            user: signed_in.user_id,
            calls: Some(calls),
            log,
            watched,
        }
    }

    fn watch(&self, agent: UserId) {
        *self.watched.lock().unwrap() = Some(agent);
    }

    /// The first of the agent's messages since `from` (an index into the log) that
    /// `wanted` accepts.
    async fn message(&self, what: &str, from: usize, wanted: impl Fn(&Seen) -> bool) -> Seen {
        until(&format!("{}: {what}", self.name), || {
            let log = self.log.lock().unwrap();
            log.messages
                .iter()
                .skip(from)
                .find(|seen| wanted(seen))
                .cloned()
        })
        .await
    }

    fn message_count(&self) -> usize {
        self.log.lock().unwrap().messages.len()
    }
}

/// The client's call events, in order, and the watched user's messages and activity.
async fn pump(
    mut events: Events,
    calls: mpsc::UnboundedSender<CallEvent>,
    log: Arc<Mutex<Log>>,
    watched: Arc<Mutex<Option<UserId>>>,
) {
    while let Some(event) = events.next().await {
        let agent = *watched.lock().unwrap();
        match event {
            Event::Call(call) => {
                let _ = calls.send(call);
            }
            Event::MessageAdded(message) | Event::MessageSent(message)
                if Some(message.sender_id) == agent =>
            {
                log.lock().unwrap().messages.push(Seen {
                    edited: false,
                    message,
                });
            }
            Event::MessageEdited(message) if Some(message.sender_id) == agent => {
                log.lock().unwrap().messages.push(Seen {
                    edited: true,
                    message,
                });
            }
            Event::ActivityChanged(activity) if Some(activity.user_id) == agent => {
                log.lock()
                    .unwrap()
                    .activity
                    .push((Instant::now(), activity.state, activity.label));
            }
            _ => {}
        }
    }
}

/// The owner's device access token, from a sign-in over HTTP (a device of its own).
async fn access_token(http: &reqwest::Client, stack: &Stack, account: &Credentials) -> String {
    let login = post(
        http,
        stack,
        "/v1/auth/login",
        None,
        json!({"email": account.email, "password": account.password}),
    )
    .await;
    let registration = login["registration_token"].as_str().unwrap().to_owned();
    let device = post(
        http,
        stack,
        "/v1/devices",
        Some(&registration),
        json!({
            "name": "the owner's agents page",
            "platform": "other",
            "registration_id": uuid::Uuid::now_v7(),
        }),
    )
    .await;
    device["access_token"].as_str().unwrap().to_owned()
}

async fn post(
    http: &reqwest::Client,
    stack: &Stack,
    path: &str,
    bearer: Option<&str>,
    body: Value,
) -> Value {
    let mut request = http.post(format!("{}{path}", stack.auth_url)).json(&body);
    if let Some(bearer) = bearer {
        request = request.bearer_auth(bearer);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{path}: {status} {body}");
    body
}

/// A [`MediaPhone`] that remembers the calls it answered and declined.
struct WatchedPhone {
    phone: MediaPhone,
    answered: Mutex<Vec<CallId>>,
    declined: Mutex<Vec<CallId>>,
}

impl WatchedPhone {
    fn answered(&self) -> Vec<CallId> {
        self.answered.lock().unwrap().clone()
    }

    fn declined(&self) -> Vec<CallId> {
        self.declined.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Phone for WatchedPhone {
    async fn answer(&self, call_id: CallId, offer_sdp: &str) -> Result<Line, SdkError> {
        self.answered.lock().unwrap().push(call_id);
        self.phone.answer(call_id, offer_sdp).await
    }

    async fn decline(&self, call_id: CallId, busy: bool) -> Result<(), SdkError> {
        self.declined.lock().unwrap().push(call_id);
        self.phone.decline(call_id, busy).await
    }
}

/// The agent: enrolled with a software key, connected, the bridge running over it.
struct Agent {
    client: Client,
    user: UserId,
    backend: Arc<TestBackend>,
    phone: Arc<WatchedPhone>,
    stop: oneshot::Sender<()>,
    run: tokio::task::JoinHandle<Result<(), BridgeError>>,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../push-bridge/tests/fixtures/voice")
}

fn fixture(name: &str) -> Fixture {
    Fixture::load(&fixtures_dir().join(format!("{name}.json"))).unwrap()
}

/// The words of every fixture: what the agent's fake speech-to-text hears.
fn all_words() -> Vec<(f64, String)> {
    ["prompt", "barge_in", "approve", "option_two", "ambiguous"]
        .into_iter()
        .flat_map(|name| fixture(name).words)
        .map(|word| (word.hz, word.text))
        .collect()
}

impl Agent {
    /// Enrolls with `code` and runs the bridge as `push-agent-bridge run` does.
    async fn start(stack: &Stack, dir: &Path, code: &str) -> Self {
        let (client, events) = client(stack, dir, "agent");
        let key = SoftwareKey::generate().unwrap();
        let account = client
            .enroll_with_agent_key(
                Arc::new(key),
                Some(AUDIENCE),
                code,
                "voice bridge",
                Platform::Desktop,
            )
            .await
            .expect("the agent enrolls");
        let messenger_events = SdkMessenger::forward(events);
        client.connect().unwrap();
        let messenger = Arc::new(SdkMessenger::new(client.clone()).unwrap());
        let (sender, backend_events) = mpsc::unbounded_channel();
        let backend = Arc::new(TestBackend::new("Test", sender));
        let scratch = [dir.to_path_buf()];
        let root = folders::resolve("root", dir, &scratch).unwrap();
        let work = folders::resolve("root/work", dir, &scratch).unwrap();
        let state = Arc::new(StateFile::load(&dir.join("state.json")).unwrap());
        let phone = Arc::new(WatchedPhone {
            phone: MediaPhone::new(&client),
            answered: Mutex::new(Vec::new()),
            declined: Mutex::new(Vec::new()),
        });
        let bridge = Bridge::new(
            messenger,
            Arc::clone(&backend) as Arc<dyn Backend>,
            backend_events,
            messenger_events,
            state,
            BridgeOptions {
                roots: vec![root],
                default_folder: work,
                default_mode: push_bridge::backend::PermissionMode::Default,
            },
        )
        .with_voice(VoiceSetup {
            phone: phone.clone(),
            stt: Some(Arc::new(FakeStt::new(all_words()))),
            tts: Some(Arc::new(FakeTts::new())),
            vad: VadConfig::default(),
        });
        let (stop, stopped) = oneshot::channel::<()>();
        let run = tokio::spawn(bridge.run(async {
            let _ = stopped.await;
        }));
        Self {
            client,
            user: account.user_id,
            backend,
            phone,
            stop,
            run,
        }
    }
}

/// Everything the owner heard, frame by frame with its arrival time.
#[derive(Clone, Default)]
struct Ear(Arc<Mutex<Vec<Heard>>>);

/// A frame and when it arrived.
type Heard = (Instant, [i16; FRAME_SAMPLES]);

impl Ear {
    fn listen(&self, mut stream: PcmStream) -> tokio::task::JoinHandle<()> {
        let heard = self.0.clone();
        tokio::spawn(async move {
            while let Some(frame) = stream.recv().await {
                heard
                    .lock()
                    .unwrap()
                    .push((Instant::now(), *frame.samples()));
            }
        })
    }

    /// The 600 Hz amplitude of each 100 ms window of what arrived in `from..to`, with
    /// the window's start.
    fn windows(&self, from: Instant, to: Instant) -> Vec<(Instant, f64)> {
        let heard = self.0.lock().unwrap();
        let frames: Vec<_> = heard
            .iter()
            .filter(|(at, _)| *at >= from && *at < to)
            .collect();
        frames
            .chunks(WINDOW)
            .filter(|chunk| chunk.len() == WINDOW)
            .map(|chunk| {
                let samples: Vec<i16> = chunk.iter().flat_map(|(_, frame)| *frame).collect();
                (chunk[0].0, amplitude(&samples, TTS_HZ))
            })
            .collect()
    }

    /// Waits until the agent's voice is heard after `from`; when it was.
    async fn voice(&self, what: &str, from: Instant) -> Instant {
        let at = until(what, || {
            self.windows(from, Instant::now() + WAIT)
                .into_iter()
                .find(|(_, level)| *level > HEARD)
        })
        .await;
        step(&format!(
            "the owner hears {what} ({:.0} at 600 Hz) {} ms in",
            at.1,
            at.0.duration_since(from).as_millis()
        ));
        at.0
    }

    /// Waits until the agent has been quiet for a second after `from` (the helper sends
    /// nothing while it has nothing to say: no frames are quiet too).
    async fn quiet(&self, what: &str, from: Instant) {
        until(what, || {
            let now = Instant::now();
            let start = now.checked_sub(Duration::from_secs(1))?;
            (start >= from
                && self
                    .windows(start, now)
                    .iter()
                    .all(|(_, level)| *level < QUIET))
            .then_some(())
        })
        .await;
    }
}

/// Queues `samples` into the owner's microphone (the helper sends 20 ms per 20 ms).
async fn say(call: &MediaCall, samples: &[i16]) {
    let sink = call.outgoing();
    for chunk in samples.chunks(FRAME_SAMPLES) {
        let mut frame = [0i16; FRAME_SAMPLES];
        frame[..chunk.len()].copy_from_slice(chunk);
        assert!(
            sink.send(PcmFrame::from_slice(&frame).unwrap()).await,
            "the call is running"
        );
    }
}

/// Every call event of `events` to `call`, until the client closes.
fn forward(call: Arc<MediaCall>, mut events: mpsc::UnboundedReceiver<CallEvent>) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            call.handle(&event);
        }
    });
}

/// `caller` calls `peer` with the media helper; retries while the first invites end
/// `Unreachable` (a device's presence lease is written alongside its call attach) until
/// one rings or ends otherwise. The caller's call events go to the call; also its stream.
async fn call(caller: &mut Person, peer: UserId) -> Result<(Arc<MediaCall>, PcmStream), SdkError> {
    let mut events = caller.calls.take().expect("the call events are free");
    for _ in 0..40 {
        let servers = caller.client.calls().ice_servers().await?;
        let mut call = MediaCall::start(
            &caller.client.calls(),
            &servers,
            Role::Caller { peer },
            MediaOptions::default(),
        )
        .await?;
        let unreachable = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let Some(event) = events.recv().await else {
                    return false;
                };
                call.handle(&event);
                match call.state() {
                    MediaState::Ended(ended) => {
                        return ended.reason == CallEndReason::Unreachable;
                    }
                    MediaState::Connected => return false,
                    MediaState::Connecting => {}
                }
            }
        })
        .await
        .unwrap_or(false);
        if unreachable {
            drop(call);
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        let stream = call.incoming().expect("the caller's stream");
        let call = Arc::new(call);
        forward(call.clone(), events);
        return Ok((call, stream));
    }
    panic!("{}: the call never rang", caller.name);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bridge_answers_its_owners_call() {
    if !enabled() {
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();
    let started = Instant::now();
    let (stack, containers) = stack().await;
    step(&format!(
        "the stack is up in {} ms",
        started.elapsed().as_millis()
    ));
    tokio::time::timeout(Duration::from_secs(300), scenario(&stack))
        .await
        .expect("the scenario finishes within 300 s");
    stack.stop().await;
    drop(containers);
    step(&format!("done in {} ms", started.elapsed().as_millis()));
}

async fn scenario(stack: &Stack) {
    let dir = scratch();
    let accounts = push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), 2)
        .await
        .expect("two verified accounts");
    let mut owner = Person::start(stack, &dir, "owner", &accounts[0]).await;
    let mut stranger = Person::start(stack, &dir, "stranger", &accounts[1]).await;
    owner.client.request_contact(stranger.user).await.unwrap();
    let accepted = stranger.client.accept_contact(owner.user).await.unwrap();
    assert_eq!(accepted.state, ContactState::Accepted);

    let http = reqwest::Client::new();
    let token = access_token(&http, stack, &accounts[0]).await;
    let created = post(
        &http,
        stack,
        "/v1/agents",
        Some(&token),
        json!({"handle": "voice_agent", "display_name": "Voice agent"}),
    )
    .await;
    let code = created["enrollment_code"].as_str().unwrap();
    let agent = Agent::start(stack, &dir, code).await;
    owner.watch(agent.user);
    stranger.watch(agent.user);
    within("the backend starts", agent.backend.wait_for("start", 1)).await;
    step("the agent enrolled and its bridge runs");

    // (a) The owner calls; the bridge answers and greets.
    let placed = Instant::now();
    let (owner_call, incoming) = call(&mut owner, agent.user).await.expect("the call starts");
    let ear = Ear::default();
    let listening = ear.listen(incoming);
    within("the owner's media connects", owner_call.connected())
        .await
        .expect("the call connects");
    step(&format!(
        "the bridge answered; media connected {} ms after the call",
        placed.elapsed().as_millis()
    ));
    ear.voice("the greeting", placed).await;
    ear.quiet("the end of the greeting", placed).await;
    assert_eq!(agent.phone.answered(), [owner_call.call_id()]);

    // (b) The owner asks: a transcript, and a prompt.
    let seen = owner.message_count();
    say(&owner_call, &fixture("prompt").audio()).await;
    within(
        "the prompt",
        agent.backend.wait_for("prompt run the tests", 1),
    )
    .await;
    let transcript = owner
        .message("the transcript", seen, |seen| {
            template(seen.body()) == "agent.transcript@1"
        })
        .await;
    assert_eq!(transcript.message.kind, MessageKind::Rich);
    assert_eq!(
        param(transcript.body(), "text").as_deref(),
        Some("run the tests")
    );
    step("the prompt reached the backend; the owner got the transcript");

    // (c) The reply is spoken and posted.
    let seen = owner.message_count();
    let replied = Instant::now();
    agent.backend.turn_started(TurnSource::Prompt);
    agent.backend.text("All tests pass.\n");
    agent.backend.turn_ended(false);
    ear.voice("the reply", replied).await;
    owner
        .message("the reply", seen, |seen| {
            template(seen.body()) == "agent.reply@1"
                && param(seen.body(), "text").is_some_and(|text| text.contains("All tests pass."))
        })
        .await;
    ear.quiet("the end of the reply", replied).await;

    // (d) Barge-in: the owner talks over a long reply, which stops; the words steer.
    let long = Instant::now();
    agent.backend.turn_started(TurnSource::Prompt);
    let sentence = "This sentence is one of many in a long reply that keeps going.";
    agent
        .backend
        .text(&format!("{}\n", vec![sentence; 30].join(" ")));
    ear.voice("the long reply", long).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let barge_in = Instant::now();
    say(&owner_call, &fixture("barge_in").audio()).await;
    within(
        "the steer",
        agent.backend.wait_for("steer stop, use the other file", 1),
    )
    .await;
    let before = ear.windows(barge_in - Duration::from_millis(500), barge_in);
    assert!(
        before.iter().any(|(_, level)| *level > HEARD),
        "the agent spoke right before the barge-in: {before:?}"
    );
    // The owner's words start 200 ms in; the agent stops within 1.5 s of them.
    let cut = barge_in + Duration::from_millis(1_700);
    let quiet_until = barge_in + Duration::from_secs(4);
    tokio::time::sleep_until(tokio::time::Instant::from_std(quiet_until)).await;
    let after = ear.windows(cut, quiet_until);
    let ended = ear
        .windows(barge_in, quiet_until)
        .into_iter()
        .rfind(|(_, level)| *level > QUIET)
        .map_or(0, |(at, _)| {
            (at.duration_since(barge_in) + Duration::from_millis(100)).as_millis()
        });
    step(&format!(
        "barge-in: the agent's voice ended {ended} ms after the owner's audio started (its words 200 ms in)"
    ));
    assert!(
        after.iter().all(|(_, level)| *level < QUIET),
        "the agent stopped speaking: {after:?}"
    );
    agent.backend.turn_ended(false);

    // (e) A permission card, approved by voice.
    let seen = owner.message_count();
    let asked = Instant::now();
    agent.backend.turn_started(TurnSource::Prompt);
    let answer = agent.backend.request(RequestKind::Permission {
        tool: "Bash".into(),
        preview: "cargo test".into(),
    });
    let card = owner
        .message("the card", seen, |seen| {
            !seen.edited && template(seen.body()) == "agent.approval@1"
        })
        .await;
    ear.voice("the card's question", asked).await;
    ear.quiet("the end of the question", asked).await;
    let approved = Instant::now();
    say(&owner_call, &fixture("approve").audio()).await;
    let answer = within("the voice answer", answer).await;
    assert_eq!(answer, Ok(Answer::Permission(true)));
    let card_id = card.message.client_msg_id;
    let edit = owner
        .message("the card's edit", seen, |seen| {
            seen.edited && seen.message.client_msg_id == card_id
        })
        .await;
    assert_eq!(
        param(edit.body(), "text").as_deref(),
        Some("🎤 Answered by voice: Approve.")
    );
    step("the card was approved by voice and edited");
    agent.backend.turn_ended(false);
    ear.voice("the confirmation", approved).await;
    ear.quiet("the end of the confirmation", approved).await;

    // (f) A stranger (the owner's contact, not the agent's) is refused by the server.
    let refused = match call(&mut stranger, agent.user).await {
        Err(error) => {
            step(&format!("the stranger's call was refused at once: {error}"));
            None
        }
        Ok((stranger_call, _)) => {
            let ended = within("the stranger's call ends", stranger_call.on_ended()).await;
            step(&format!("the stranger's call ended: {ended:?}"));
            assert_eq!(ended.cause, EndCause::Refused(CallErrorCode::NotContact));
            Some(stranger_call.call_id())
        }
    };
    assert_eq!(agent.phone.answered(), [owner_call.call_id()]);
    assert_eq!(agent.phone.declined(), []);
    if let Some(stranger_call) = refused {
        assert_ne!(stranger_call, owner_call.call_id());
    }

    // (g) The owner hangs up: both sides end.
    let hung_up = Instant::now();
    let ended = within("the owner's hang-up", owner_call.hangup()).await;
    assert_eq!(
        (ended.reason, ended.cause),
        (CallEndReason::Completed, EndCause::Local)
    );
    within("the owner's stream ends", listening)
        .await
        .expect("the listener ran");
    until("the agent is idle again", || {
        let log = owner.log.lock().unwrap();
        log.activity
            .iter()
            .any(|(at, state, _)| *at >= hung_up && *state == ActivityState::Idle)
            .then_some(())
    })
    .await;
    step("the owner hung up; the bridge's call ended");

    let Agent {
        client,
        stop,
        run,
        backend,
        phone,
        ..
    } = agent;
    let _ = stop.send(());
    within("the bridge stops", run)
        .await
        .expect("the bridge ran")
        .expect("the bridge stopped cleanly");
    assert!(backend.calls().iter().any(|call| call == "stop"));
    // The phone holds a handle of the client: only the last one closes it.
    drop((owner_call, phone));
    client.close().await.unwrap();
    owner.client.close().await.unwrap();
    stranger.client.close().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
