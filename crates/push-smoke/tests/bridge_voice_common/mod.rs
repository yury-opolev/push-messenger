//! What the bridge's voice smoke tests share (`bridge_voice.rs`, `bridge_voice_adapter.rs`):
//! the stack, the owner on an SDK client with push-sdk's media helper, the agent running
//! the bridge in this process with its speech in a voice adapter (the testkit's scripted
//! adapter, built here), and what the owner hears.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::common::{self, Containers, Stack};
use push_agent_key::software::SoftwareKey;
use push_bridge::backend::Backend;
use push_bridge::config::{AdapterConfig, SpawnConfig, StderrMode};
use push_bridge::core::{Bridge, BridgeError, BridgeOptions, CallFlag};
use push_bridge::folders;
use push_bridge::messenger::SdkMessenger;
use push_bridge::state::StateFile;
use push_bridge::test_backend::TestBackend;
use push_bridge::voice::VoiceSetup;
use push_bridge::voice::adapter::advertise::advertise;
use push_bridge::voice::adapter::report::log_totals;
use push_bridge::voice::adapter::router::AdapterSetup;
use push_bridge::voice::adapter::supervisor;
use push_bridge::voice::media::MediaPhone;
use push_bridge::voice::phone::{Line, Phone};
use push_bridge::voice::policy::CallPolicy;
use push_load::accounts::Account as Credentials;
use push_sdk::media::{
    FRAME_SAMPLES, MediaCall, MediaOptions, MediaState, PcmFrame, PcmStream, Role,
};
use push_sdk::{
    ActivityState, CallEndReason, CallEvent, CallId, Client, Config, Event, Events,
    MemorySecretStore, Message, Platform, SdkError, UserId,
};
use push_voice_adapter_proto::BridgeHello;
use push_voice_adapter_testkit::tones::{SPEAK_HZ, amplitude};
use push_voice_adapter_testkit::{Fixture, Script, Word, fixture as fixtures};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};

/// How long any one wait may take before the test fails.
pub const WAIT: Duration = Duration::from_secs(30);
/// The agent-key audience auth-svc checks.
pub const AUDIENCE: &str = "push-smoke-bridge-voice";
/// The amplitude of the agent's 600 Hz voice that counts as heard (it speaks at 6000)…
pub const HEARD: f64 = 1_500.0;
/// …and below which it is quiet.
pub const QUIET: f64 = 500.0;
/// One measuring window: 100 ms.
pub const WINDOW: usize = 5;

pub fn enabled() -> bool {
    if std::env::var("PUSH_BRIDGE_VOICE").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipped: set PUSH_BRIDGE_VOICE=1 to run the bridge's voice smoke test");
    false
}

pub fn step(text: &str) {
    eprintln!("[bridge-voice] {text}");
}

/// The stack with calls (STUN only: host candidates connect the local clients) and
/// agent-key sign-in, two agents per owner.
pub async fn stack() -> (Stack, Containers) {
    stack_for(2).await
}

/// [`stack`] with `agents` agents per owner.
pub async fn stack_for(agents: u32) -> (Stack, Containers) {
    let agents = agents.to_string();
    let (mut datastores, containers) = common::calls_datastores().await;
    datastores
        .gateway
        .push(("PUSH_CALLS_ATTEMPTS_PER_HOUR".to_owned(), "1000".to_owned()));
    for (name, value) in [
        ("PUSH_TURN_MODE", "stun-only"),
        ("PUSH_TURN_STUN_URLS", "stun:127.0.0.1:3478"),
        ("PUSH_AUTH_PROVIDERS", "password,agent-key"),
        ("PUSH_AUTH_AGENT_KEY_AUDIENCE", AUDIENCE),
        ("PUSH_AGENTS_PER_USER", agents.as_str()),
    ] {
        datastores.auth.push((name.to_owned(), value.to_owned()));
    }
    (Stack::start(datastores).await, containers)
}

pub fn scratch() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("push-bridge-voice-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(dir.join("root").join("work")).unwrap();
    dir
}

pub fn client(stack: &Stack, dir: &Path, name: &str) -> (Client, Events) {
    client_with(stack, dir, name, true)
}

/// A client that advertises calls from the start when `calls`.
pub fn client_with(stack: &Stack, dir: &Path, name: &str, calls: bool) -> (Client, Events) {
    let database = dir.join(format!("{name}.db"));
    let mut config = Config::new(
        &stack.auth_url,
        &stack.socket_url,
        &database.to_string_lossy(),
    );
    config.allow_insecure_transport = true;
    config.calls = calls;
    Client::open(config, Arc::new(MemorySecretStore::new())).unwrap()
}

pub async fn within<F: std::future::Future>(what: &str, future: F) -> F::Output {
    tokio::time::timeout(WAIT, future)
        .await
        .unwrap_or_else(|_| panic!("{what}: not within {WAIT:?}"))
}

/// Polls `check` every 20 ms until it gives a value.
pub async fn until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
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
pub struct Seen {
    pub edited: bool,
    pub message: Message,
}

impl Seen {
    pub fn body(&self) -> &str {
        self.message.body.as_deref().unwrap_or_default()
    }
}

/// What the owner's client reported about the agent: its messages and activity.
#[derive(Default)]
pub struct Log {
    pub messages: Vec<Seen>,
    pub activity: Vec<(Instant, ActivityState, Option<String>)>,
}

/// One signed-in, connected human with calls: its call events split off, and what it
/// sees of `watched` (the agent) logged.
pub struct Person {
    pub name: &'static str,
    pub client: Client,
    pub user: UserId,
    pub calls: Option<mpsc::UnboundedReceiver<CallEvent>>,
    pub log: Arc<Mutex<Log>>,
    pub watched: Arc<Mutex<Option<UserId>>>,
}

impl Person {
    pub async fn start(
        stack: &Stack,
        dir: &Path,
        name: &'static str,
        account: &Credentials,
    ) -> Self {
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

    pub fn watch(&self, agent: UserId) {
        *self.watched.lock().unwrap() = Some(agent);
    }

    /// The first of the agent's messages since `from` (an index into the log) that
    /// `wanted` accepts.
    pub async fn message(&self, what: &str, from: usize, wanted: impl Fn(&Seen) -> bool) -> Seen {
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

    pub fn message_count(&self) -> usize {
        self.log.lock().unwrap().messages.len()
    }
}

/// The client's call events, in order, and the watched user's messages and activity.
pub async fn pump(
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
pub async fn access_token(http: &reqwest::Client, stack: &Stack, account: &Credentials) -> String {
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

pub async fn post(
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

/// A [`MediaPhone`] that remembers the calls it answered, declined and placed.
pub struct WatchedPhone {
    pub phone: MediaPhone,
    pub answered: Mutex<Vec<CallId>>,
    pub declined: Mutex<Vec<CallId>>,
    pub dialed: Mutex<Vec<UserId>>,
}

impl WatchedPhone {
    pub fn answered(&self) -> Vec<CallId> {
        self.answered.lock().unwrap().clone()
    }

    pub fn declined(&self) -> Vec<CallId> {
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

    async fn call(&self, peer: UserId) -> Result<Line, SdkError> {
        self.dialed.lock().unwrap().push(peer);
        self.phone.call(peer).await
    }
}

/// The agent: enrolled with a software key, connected, the bridge running over it.
pub struct Agent {
    pub client: Client,
    pub user: UserId,
    pub backend: Arc<TestBackend>,
    pub phone: Arc<WatchedPhone>,
    pub stop: oneshot::Sender<()>,
    pub run: tokio::task::JoinHandle<Result<(), BridgeError>>,
    /// With a voice adapter: its supervisor and the router's counters.
    pub adapter: Option<AdapterSetup>,
    /// With a voice adapter: the task advertising calls while it can listen.
    pub advertiser: Option<tokio::task::JoinHandle<()>>,
}

/// Where the agent's speech runs.
pub enum Speech {
    /// A voice adapter (`calls.adapter`), supervised and advertised as
    /// `push-agent-bridge run` does.
    Adapter(AdapterConfig),
}

/// The testkit's recorded fixture `name`.
pub fn fixture(name: &str) -> Fixture {
    Fixture::named(name)
}

/// The words of every fixture: what the testkit's adapter hears.
pub fn all_words() -> Vec<Word> {
    fixtures::all_words()
}

/// The testkit's binary, built once per run (see the module's documentation).
pub fn testkit() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let test = std::env::current_exe().expect("the test's path");
            // target/<profile>/deps/<test> → target/<profile>
            let profile_dir = test
                .parent()
                .and_then(Path::parent)
                .expect("the test runs from target/<profile>/deps");
            let profile = profile_dir.file_name().and_then(|name| name.to_str());
            let target_dir = profile_dir.parent().expect("target/<profile>");
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let mut build = std::process::Command::new(cargo);
            build
                .args([
                    "build",
                    "--locked",
                    "-p",
                    "push-voice-adapter-testkit",
                    "--target-dir",
                ])
                .arg(target_dir)
                .current_dir(env!("CARGO_MANIFEST_DIR"));
            match profile {
                Some("debug") => {}
                Some("release") => {
                    build.arg("--release");
                }
                Some(other) => {
                    build.args(["--profile", other]);
                }
                None => panic!("the profile directory {}", profile_dir.display()),
            }
            let started = Instant::now();
            let status = build.status().expect("cargo runs");
            assert!(status.success(), "building push-voice-adapter-testkit");
            let binary = profile_dir.join(format!(
                "push-voice-adapter-testkit{}",
                std::env::consts::EXE_SUFFIX
            ));
            assert!(binary.is_file(), "{} exists", binary.display());
            step(&format!(
                "the testkit adapter is built in {} ms: {}",
                started.elapsed().as_millis(),
                binary.display()
            ));
            binary
        })
        .clone()
}

/// The default script hearing every fixture's words. It sends its voice strictly in real
/// time (no lead), its timer's jitter included: the bridge's playback pre-roll
/// (`router::PREROLL`) absorbs it, so the gaps the owner hears stay small.
pub fn hearing() -> Script {
    Script::with_words(all_words())
}

pub fn write_script(dir: &Path, script: &Script) -> PathBuf {
    let path = dir.join("script.json");
    std::fs::write(&path, serde_json::to_vec_pretty(script).unwrap()).unwrap();
    path
}

/// `calls.adapter.spawn` running the testkit with `script`, its events in `events`.
pub fn spawned(dir: &Path, script: &Script, events: &Path) -> AdapterConfig {
    let script = write_script(dir, script);
    AdapterConfig::Spawn(SpawnConfig {
        command: vec![
            testkit().to_string_lossy().into_owned(),
            "--script".to_owned(),
            script.to_string_lossy().into_owned(),
            "--events".to_owned(),
            events.to_string_lossy().into_owned(),
        ],
        stderr: StderrMode::Log,
        env_allow: Vec::new(),
    })
}

/// `calls.language`'s default.
pub const LANGUAGE: &str = "en-US";

impl Agent {
    /// Enrolls with `code` and runs the bridge as `push-agent-bridge run` does, answering
    /// and calling by `policy`, its speech in the testkit's adapter ([`hearing`], spawned,
    /// its events in `<dir>/adapter-events.jsonl`); returns once calls are advertised.
    pub async fn start(stack: &Stack, dir: &Path, code: &str, policy: CallPolicy) -> Self {
        let adapter = spawned(dir, &hearing(), &dir.join("adapter-events.jsonl"));
        let agent = Self::start_with(stack, dir, code, policy, Speech::Adapter(adapter)).await;
        advertised(&agent).await;
        agent
    }

    /// [`Agent::start`] with its speech in `speech`. With a voice adapter, calls are not
    /// advertised until it is ready with `stt` (as `push-agent-bridge run` does); its
    /// spawned adapter's stderr goes to `<dir>/logs`.
    pub async fn start_with(
        stack: &Stack,
        dir: &Path,
        code: &str,
        policy: CallPolicy,
        speech: Speech,
    ) -> Self {
        let (client, events) = client_with(stack, dir, "agent", false);
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
            dialed: Mutex::new(Vec::new()),
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
        );
        let (bridge, adapter, advertiser) = match speech {
            Speech::Adapter(config) => {
                let hello = BridgeHello::v1(env!("CARGO_PKG_VERSION"), LANGUAGE);
                let supervisor = supervisor::start(config, hello, dir.join("logs"));
                let setup = AdapterSetup::new(supervisor, LANGUAGE);
                let call_flag: CallFlag = Arc::new(watch::channel(false).0);
                let advertised = client.clone();
                let advertised_setup = setup.clone();
                let advertiser = tokio::spawn(advertise(
                    setup.supervisor.state(),
                    call_flag.subscribe(),
                    move |on| {
                        advertised_setup.set_advertised(on);
                        advertised.set_calls_advertised(on);
                    },
                ));
                let bridge = bridge
                    .with_voice(VoiceSetup {
                        phone: phone.clone(),
                        policy,
                        adapter: setup.clone(),
                    })
                    .with_call_flag(call_flag);
                (bridge, Some(setup), Some(advertiser))
            }
        };
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
            adapter,
            advertiser,
        }
    }

    /// Stops the bridge as `push-agent-bridge run` does at its end (calls are no longer
    /// advertised, the voice adapter is stopped, its totals logged) and closes the
    /// client.
    pub async fn shut_down(self) {
        let Agent {
            client,
            stop,
            run,
            phone,
            adapter,
            advertiser,
            ..
        } = self;
        let _ = stop.send(());
        within("the bridge stops", run)
            .await
            .expect("the bridge ran")
            .expect("the bridge stopped cleanly");
        stop_adapter(adapter, advertiser).await;
        // The phone holds a handle of the client: only the last one closes it.
        drop(phone);
        client.close().await.unwrap();
    }
}

/// Once the bridge stopped: stops advertising calls and the voice adapter (its totals
/// logged), as `push-agent-bridge run` does; both hold a handle of the agent's client.
pub async fn stop_adapter(
    adapter: Option<AdapterSetup>,
    advertiser: Option<tokio::task::JoinHandle<()>>,
) {
    if let Some(advertiser) = advertiser {
        advertiser.abort();
        let _ = advertiser.await;
    }
    if let Some(adapter) = adapter {
        within("the voice adapter stops", adapter.supervisor.shutdown()).await;
        log_totals(&adapter);
    }
}

/// Everything the owner heard, frame by frame with its arrival time.
#[derive(Clone, Default)]
pub struct Ear(pub Arc<Mutex<Vec<Heard>>>);

/// A frame and when it arrived.
pub type Heard = (Instant, [i16; FRAME_SAMPLES]);

impl Ear {
    pub fn listen(&self, mut stream: PcmStream) -> tokio::task::JoinHandle<()> {
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
    pub fn windows(&self, from: Instant, to: Instant) -> Vec<(Instant, f64)> {
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
                (chunk[0].0, amplitude(&samples, SPEAK_HZ))
            })
            .collect()
    }

    /// The gaps between the arrivals of consecutive frames in `from..to`, longest first.
    pub fn gaps(&self, from: Instant, to: Instant) -> Vec<Duration> {
        let heard = self.0.lock().unwrap();
        let arrivals: Vec<Instant> = heard
            .iter()
            .map(|(at, _)| *at)
            .filter(|at| *at >= from && *at < to)
            .collect();
        let mut gaps: Vec<Duration> = arrivals
            .windows(2)
            .map(|pair| pair[1].duration_since(pair[0]))
            .collect();
        gaps.sort_unstable_by(|a, b| b.cmp(a));
        gaps
    }

    /// Waits until the agent's voice is heard after `from`; when it was.
    pub async fn voice(&self, what: &str, from: Instant) -> Instant {
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
    pub async fn quiet(&self, what: &str, from: Instant) {
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
pub async fn say(call: &MediaCall, samples: &[i16]) {
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
pub fn forward(call: Arc<MediaCall>, mut events: mpsc::UnboundedReceiver<CallEvent>) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            call.handle(&event);
        }
    });
}

/// `caller` calls `peer` with the media helper; retries while the first invites end
/// `Unreachable` (a device's presence lease is written alongside its call attach) until
/// one rings or ends otherwise. The caller's call events go to the call; also its stream.
pub async fn call(
    caller: &mut Person,
    peer: UserId,
) -> Result<(Arc<MediaCall>, PcmStream), SdkError> {
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

/// The owner's side of calls one after the other: every call event goes to the current
/// call (events that come before it is set are replayed to it), and the bridge's
/// invitations are reported as they ring.
pub struct Line2 {
    pub current: Arc<Mutex<Option<Arc<MediaCall>>>>,
    pub pending: Arc<Mutex<Vec<CallEvent>>>,
    pub incoming: mpsc::UnboundedReceiver<(CallId, String)>,
}

impl Line2 {
    pub fn new(mut events: mpsc::UnboundedReceiver<CallEvent>) -> Self {
        let current: Arc<Mutex<Option<Arc<MediaCall>>>> = Arc::default();
        let pending: Arc<Mutex<Vec<CallEvent>>> = Arc::default();
        let (sender, incoming) = mpsc::unbounded_channel();
        let (to, kept) = (current.clone(), pending.clone());
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if let CallEvent::Incoming {
                    call_id, offer_sdp, ..
                } = &event
                {
                    let _ = sender.send((*call_id, offer_sdp.clone()));
                    continue;
                }
                let call = to.lock().unwrap().clone();
                match call {
                    Some(call) => {
                        call.handle(&event);
                    }
                    None => kept.lock().unwrap().push(event),
                }
            }
        });
        Self {
            current,
            pending,
            incoming,
        }
    }

    /// `call` gets the call events from now on, and those that came meanwhile.
    pub fn set(&self, call: Arc<MediaCall>) {
        let mut current = self.current.lock().unwrap();
        for event in self.pending.lock().unwrap().drain(..) {
            call.handle(&event);
        }
        *current = Some(call);
    }

    pub fn clear(&self) {
        *self.current.lock().unwrap() = None;
        self.pending.lock().unwrap().clear();
    }

    /// The next call the bridge places to the owner: its id and offer.
    pub async fn ringing(&mut self, what: &str) -> (CallId, String) {
        within(what, self.incoming.recv())
            .await
            .expect("the owner's events go on")
    }
}

/// The owner calls `peer` with the media helper (retrying while the first invites end
/// `Unreachable`, as [`call`] does).
pub async fn call_with(caller: &Person, line: &Line2, peer: UserId) -> (Arc<MediaCall>, PcmStream) {
    for _ in 0..40 {
        let servers = caller.client.calls().ice_servers().await.unwrap();
        let mut call = MediaCall::start(
            &caller.client.calls(),
            &servers,
            Role::Caller { peer },
            MediaOptions::default(),
        )
        .await
        .expect("the call starts");
        let stream = call.incoming().expect("the caller's stream");
        let call = Arc::new(call);
        line.set(call.clone());
        let unreachable = tokio::time::timeout(Duration::from_secs(3), call.on_ended())
            .await
            .is_ok_and(|ended| ended.reason == CallEndReason::Unreachable);
        if unreachable {
            line.clear();
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        return (call, stream);
    }
    panic!("{}: the call never rang", caller.name);
}

/// Answers card `card` (as the owner's client saw it) with `values`.
pub fn answer_card(person: &Person, card: &Seen, values: &[&str]) {
    let body = json!({
        "card": card.message.message_id.expect("a delivered card").to_string(),
        "values": values,
        "fallback": values.join(", "),
    });
    person
        .client
        .send_answer(card.message.conversation_id, &body.to_string())
        .expect("the answer is queued");
}

/// Waits until the agent advertises calls (its adapter has listened for 2 s).
pub async fn advertised(agent: &Agent) {
    let since = Instant::now();
    until("calls advertised", || {
        agent.client.calls_advertised().then_some(())
    })
    .await;
    step(&format!(
        "calls advertised {} ms after the bridge started",
        since.elapsed().as_millis()
    ));
}
