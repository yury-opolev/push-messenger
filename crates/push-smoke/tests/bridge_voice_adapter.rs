//! The agent bridge's calls with speech in a voice adapter (docs/voice-adapter-protocol.md)
//! end to end: the stack, the owner and the agent as in `bridge_voice.rs`, but the bridge
//! runs `calls.adapter` as `push-agent-bridge run` does (the supervisor, calls advertised
//! while the adapter can listen) and the adapter is `push-voice-adapter-testkit`, the
//! scripted adapter hearing the fixtures' tones as their words and speaking a 600 Hz tone.
//!
//! - Spawned: the owner's call is answered and greeted, a recorded prompt reaches the
//!   backend, the reply is heard, the owner's words cut a long reply short (barge-in) and
//!   approve a card by voice; a final transcript becomes a `speak` within 100 ms (the
//!   adapter's events), the agent's voice arrives without gaps longer than
//!   [`MAX_GAP`]; the bridge's log holds none of the words.
//! - Connected (`calls.adapter.connect`): the adapter listens on a pipe (Windows) or a
//!   socket (Unix); the call works the same.
//! - An adapter crashing mid-call: the owner is told, the call stays, the restarted adapter
//!   takes it again and the owner's next words become a prompt.
//! - No adapter (a missing program) and an adapter refusing the bridge's audio format:
//!   calls are not advertised (the owner's call is `Unreachable`); the refusal is logged
//!   once as a warning (`unsupported_format`), its repeats at debug.
//!
//! The adapter's binary: the test builds it (`cargo build -p push-voice-adapter-testkit`,
//! a no-op when it is fresh; CI builds it first) into the target directory this test runs
//! from (`target/<profile>/`, the parent of the test's `deps/`): a dev-dependency's binary
//! is not built for a test of another package (`CARGO_BIN_EXE_*` is only set for the
//! package's own binaries).
//!
//! Opt in with `PUSH_BRIDGE_VOICE=1` (otherwise the tests pass without running): they need
//! Docker. CI runs them in the `bridge-voice` job.

mod bridge_voice_common;
mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bridge_voice_common::*;
use common::Stack;
use push_bridge::backend::{Answer, RequestKind, TurnSource};
use push_bridge::config::{AdapterConfig, ConnectConfig, SpawnConfig, StderrMode};
use push_bridge::logging;
use push_bridge::messenger::{param, template};
use push_bridge::voice::adapter::supervisor::AdapterState;
use push_bridge::voice::policy::CallPolicy;
use push_sdk::media::{MediaCall, MediaOptions, MediaState, Role};
use push_sdk::{CallEndReason, UserId};
use push_voice_adapter_testkit::{Script, Word};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing_subscriber::EnvFilter;

/// A final transcript becomes the bridge's `speak` within this (without the backend: the
/// bridge's own confirmation of a voice answer).
const MAX_FINAL_TO_SPEAK: Duration = Duration::from_millis(100);
/// The longest gap between the agent's frames the owner may hear while it speaks.
const MAX_GAP: Duration = Duration::from_millis(40);

// ---------------------------------------------------------------------------------------
// The bridge's log: JSON lines as `push-agent-bridge run` writes them (push_bridge at
// debug), kept in memory and in a file for a failing run.

struct Captured {
    lines: Mutex<Vec<u8>>,
    path: PathBuf,
}

struct Tee {
    captured: &'static Captured,
    file: std::fs::File,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.captured.lines.lock().unwrap().extend_from_slice(buf);
        self.file.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// The process's log, captured from the first call on.
fn bridge_log() -> &'static Captured {
    static CAPTURED: OnceLock<Captured> = OnceLock::new();
    static INSTALLED: OnceLock<()> = OnceLock::new();
    let captured = CAPTURED.get_or_init(|| Captured {
        lines: Mutex::new(Vec::new()),
        path: Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("bridge-voice-adapter-{}.log", uuid::Uuid::now_v7())),
    });
    INSTALLED.get_or_init(|| {
        let file = std::fs::File::create(&captured.path).unwrap();
        let writer = Mutex::new(Tee { captured, file });
        let subscriber = logging::subscriber(writer, EnvFilter::new("info,push_bridge=debug"));
        tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");
        step(&format!("the log: {}", captured.path.display()));
    });
    captured
}

/// Where the log is now, to read what follows.
fn log_mark() -> usize {
    bridge_log().lines.lock().unwrap().len()
}

/// The bridge's log lines since `mark`.
fn log_since(mark: usize) -> Vec<Value> {
    let lines = bridge_log().lines.lock().unwrap();
    String::from_utf8_lossy(&lines[mark..])
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a JSON log line"))
        .filter(|line: &Value| {
            line["target"]
                .as_str()
                .is_some_and(|target| target.starts_with("push_bridge"))
        })
        .collect()
}

/// Content-free (design A15): the bridge's log since `mark` names none of `words`.
fn assert_content_free(mark: usize, words: &[&str]) {
    let lines = log_since(mark);
    assert!(!lines.is_empty(), "the bridge logged");
    for line in &lines {
        let text = line.to_string().to_lowercase();
        for word in words {
            assert!(
                !text.contains(&word.to_lowercase()),
                "the bridge's log has {word:?}: {line}"
            );
        }
    }
    step(&format!(
        "the bridge's {} log lines name none of the {} words or sentences",
        lines.len(),
        words.len()
    ));
}

/// `voice_adapter_failed` lines since `mark` whose reason has `code`: (warnings, debug).
fn failures(mark: usize, code: &str) -> (usize, usize) {
    let matching: Vec<String> = log_since(mark)
        .into_iter()
        .filter(|line| {
            line["fields"]["event"] == "voice_adapter_failed"
                && line["fields"]["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains(code))
        })
        .map(|line| line["level"].as_str().unwrap_or_default().to_owned())
        .collect();
    let warnings = matching.iter().filter(|level| *level == "WARN").count();
    let debug = matching.iter().filter(|level| *level == "DEBUG").count();
    (warnings, debug)
}

// ---------------------------------------------------------------------------------------
// The scripted adapter.

/// The testkit's binary, built once per run (see the module's documentation).
fn testkit() -> PathBuf {
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
                .args(["build", "-p", "push-voice-adapter-testkit", "--target-dir"])
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

/// The default script hearing every fixture's words. It sends its voice 100 ms ahead of
/// real time, as the protocol allows ("at most about 1 s ahead") and a synthesizing
/// adapter is: the gaps the owner hears are then the bridge's playback and the media
/// path's, not the testkit's own pacing (strictly real time, its timer's jitter passes
/// through the bridge, which plays what arrives: design §5, "gaps are the adapter's").
fn hearing() -> Script {
    Script {
        speak_lead_ms: 100,
        ..Script::with_words(
            all_words()
                .into_iter()
                .map(|(hz, text)| Word::new(hz, text))
                .collect(),
        )
    }
}

fn write_script(dir: &Path, script: &Script) -> PathBuf {
    let path = dir.join("script.json");
    std::fs::write(&path, serde_json::to_vec_pretty(script).unwrap()).unwrap();
    path
}

/// `calls.adapter.spawn` running the testkit with `script`, its events in `events`.
fn spawned(dir: &Path, script: &Script, events: &Path) -> AdapterConfig {
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

/// The testkit's events so far (JSON lines; `[]` before the first).
fn events(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| line.ends_with('}'))
        .map(|line| serde_json::from_str(line).expect("an event line"))
        .collect()
}

fn named<'a>(events: &'a [Value], name: &str) -> impl Iterator<Item = &'a Value> {
    events.iter().filter(move |event| event["event"] == name)
}

/// Waits until the testkit logged `n` events `name`.
async fn wait_events(path: &Path, name: &str, n: usize) -> Vec<Value> {
    until(&format!("{n} {name} events"), || {
        let events = events(path);
        (named(&events, name).count() >= n).then_some(events)
    })
    .await
}

/// A testkit listening on its own endpoint (connect mode), killed when dropped.
struct Listening {
    /// Killed when dropped.
    _child: tokio::process::Child,
    config: AdapterConfig,
}

impl Listening {
    async fn start(dir: &Path, script: &Script, events: &Path) -> Self {
        let script = write_script(dir, script);
        let suffix = &uuid::Uuid::now_v7().simple().to_string()[24..];
        let (flag, endpoint, config) = endpoint(suffix);
        let mut child = tokio::process::Command::new(testkit())
            .arg("--script")
            .arg(&script)
            .arg("--events")
            .arg(events)
            .arg(flag)
            .arg(&endpoint)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the testkit starts");
        let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
        within("the testkit listens", async {
            while let Some(line) = stderr.next_line().await.unwrap() {
                step(&format!("testkit: {line}"));
                if line.starts_with("listening on") {
                    return;
                }
            }
            panic!("the testkit ended before listening");
        })
        .await;
        tokio::spawn(async move {
            while let Ok(Some(line)) = stderr.next_line().await {
                step(&format!("testkit: {line}"));
            }
        });
        Self {
            _child: child,
            config,
        }
    }
}

/// The testkit's flag, its endpoint and the bridge's `calls.adapter.connect`: a pipe on
/// Windows; on Unix a socket in `$XDG_RUNTIME_DIR` when set, else in the target's
/// temporary directory (a socket's path is at most ~100 bytes).
#[cfg(windows)]
fn endpoint(suffix: &str) -> (&'static str, String, AdapterConfig) {
    let name = format!("push-voice-e2e-{suffix}");
    let config = AdapterConfig::Connect(ConnectConfig::Pipe(name.clone()));
    ("--pipe", name, config)
}

#[cfg(unix)]
fn endpoint(suffix: &str) -> (&'static str, String, AdapterConfig) {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute() && dir.is_dir())
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")));
    let path = dir.join(format!("pve-{suffix}.sock"));
    let path = path.to_string_lossy().into_owned();
    assert!(path.len() < 100, "a socket path this long: {path}");
    let config = AdapterConfig::Connect(ConnectConfig::Socket(path.clone()));
    ("--socket", path, config)
}

// ---------------------------------------------------------------------------------------
// The scenarios.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bridge_speaks_through_a_spawned_adapter() {
    if !enabled() {
        return;
    }
    bridge_log();
    testkit();
    let started = Instant::now();
    let (stack, containers) = stack().await;
    step(&format!(
        "the stack is up in {} ms",
        started.elapsed().as_millis()
    ));
    tokio::time::timeout(Duration::from_secs(300), spawned_adapter(&stack))
        .await
        .expect("the scenario finishes within 300 s");
    stack.stop().await;
    drop(containers);
    step(&format!("done in {} ms", started.elapsed().as_millis()));
}

/// Creates an agent of the owner's: its enrollment code.
async fn new_agent(stack: &Stack, token: &str, handle: &str) -> String {
    let created = post(
        &reqwest::Client::new(),
        stack,
        "/v1/agents",
        Some(token),
        json!({"handle": handle, "display_name": "Voice agent"}),
    )
    .await;
    created["enrollment_code"].as_str().unwrap().to_owned()
}

/// Waits until the agent advertises calls (its adapter has listened for 2 s).
async fn advertised(agent: &Agent) {
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

async fn spawned_adapter(stack: &Stack) {
    let mark = log_mark();
    let dir = scratch();
    let accounts = push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), 1)
        .await
        .expect("a verified account");
    let mut owner = Person::start(stack, &dir, "owner", &accounts[0]).await;
    let token = access_token(&reqwest::Client::new(), stack, &accounts[0]).await;
    let code = new_agent(stack, &token, "spawned_voice").await;
    let events_path = dir.join("adapter-events.jsonl");
    let adapter = spawned(&dir, &hearing(), &events_path);
    let agent = Agent::start_with(
        stack,
        &dir,
        &code,
        CallPolicy::default(),
        Speech::Adapter(adapter),
    )
    .await;
    owner.watch(agent.user);
    within("the backend starts", agent.backend.wait_for("start", 1)).await;
    advertised(&agent).await;
    let hello = wait_events(&events_path, "hello_received", 1).await;
    assert_eq!(
        named(&hello, "hello_received").next().unwrap()["protocols"],
        json!(["push-voice-adapter/1"])
    );

    // (a) The owner calls; the bridge answers, starts the call in the adapter and greets.
    let placed = Instant::now();
    let (owner_call, incoming) = call(&mut owner, agent.user).await.expect("the call starts");
    let ear = Ear::default();
    let listening = ear.listen(incoming);
    within("the owner's media connects", owner_call.connected())
        .await
        .expect("the call connects");
    let started = wait_events(&events_path, "call_started", 1).await;
    let call_id = named(&started, "call_started").next().unwrap()["call_id"].clone();
    ear.voice("the greeting", placed).await;
    ear.quiet("the end of the greeting", placed).await;
    assert_eq!(agent.phone.answered(), [owner_call.call_id()]);

    // (b) The owner asks: its audio flows to the adapter, whose final transcript becomes
    // the transcript and a prompt.
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
    assert_eq!(
        param(transcript.body(), "text").as_deref(),
        Some("run the tests")
    );
    let so_far = events(&events_path);
    let audio = named(&so_far, "audio").count();
    assert!(
        audio >= 50,
        "the owner's audio reached the adapter: {audio}"
    );
    assert!(
        named(&so_far, "audio").all(|event| event["call_id"] == call_id),
        "of the call"
    );
    assert!(
        named(&so_far, "transcript_sent").any(|event| event["final"] == true),
        "a final transcript"
    );
    step(&format!(
        "the prompt reached the backend; the adapter got {audio} frames of the owner's"
    ));

    // (c) The reply is spoken by the adapter and heard.
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

    // (d) Barge-in: the owner talks over a long reply; the adapter is told to stop and
    // the words steer. Meanwhile: the gaps in the agent's voice.
    let stops = named(&events(&events_path), "stop_speaking").count();
    let long = Instant::now();
    agent.backend.turn_started(TurnSource::Prompt);
    let sentence = "This sentence is one of many in a long reply that keeps going.";
    agent
        .backend
        .text(&format!("{}\n", vec![sentence; 30].join(" ")));
    let voiced = ear.voice("the long reply", long).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let barge_in = Instant::now();
    let gaps = ear.gaps(voiced + Duration::from_millis(200), barge_in);
    say(&owner_call, &fixture("barge_in").audio()).await;
    within(
        "the steer",
        agent.backend.wait_for("steer stop, use the other file", 1),
    )
    .await;
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
    let so_far = events(&events_path);
    assert!(
        named(&so_far, "stop_speaking").count() > stops,
        "the adapter was told to stop"
    );
    assert!(
        named(&so_far, "speak_done_sent").any(|event| event["interrupted"] == true),
        "a speak was cut short"
    );
    agent.backend.turn_ended(false);
    let longest = gaps.first().copied().unwrap_or_default();
    let p99 = gaps.get(gaps.len() / 100).copied().unwrap_or_default();
    let over = gaps.iter().filter(|gap| **gap > MAX_GAP).count();
    step(&format!(
        "while the agent spoke: {} frames, the longest gaps {:?} ms, p99 {} ms, {over} over {} ms",
        gaps.len() + 1,
        gaps.iter()
            .take(5)
            .map(Duration::as_millis)
            .collect::<Vec<_>>(),
        p99.as_millis(),
        MAX_GAP.as_millis()
    ));
    assert!(gaps.len() >= 100, "the agent spoke 2 s at least: {gaps:?}");
    assert!(longest <= MAX_GAP, "the longest gap: {longest:?}");

    // (e) A permission card, approved by voice: the bridge's confirmation is a `speak`
    // right after the final transcript (no backend in between).
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
    let before = events(&events_path).len();
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
    agent.backend.turn_ended(false);
    ear.voice("the confirmation", approved).await;
    ear.quiet("the end of the confirmation", approved).await;
    let after: Vec<Value> = events(&events_path).into_iter().skip(before).collect();
    let final_at = after
        .iter()
        .position(|event| event["event"] == "transcript_sent" && event["final"] == true)
        .expect("the answer's final transcript");
    let speak = after[final_at..]
        .iter()
        .find(|event| event["event"] == "speak")
        .expect("the confirmation's speak");
    let final_to_speak = Duration::from_micros(
        speak["t_us"].as_u64().unwrap() - after[final_at]["t_us"].as_u64().unwrap(),
    );
    step(&format!(
        "final transcript → speak: {:.1} ms (the adapter's clock)",
        final_to_speak.as_secs_f64() * 1000.0
    ));
    assert!(
        final_to_speak < MAX_FINAL_TO_SPEAK,
        "final transcript → speak: {final_to_speak:?}"
    );

    // (f) The owner hangs up: the adapter hears the call's end.
    let ended = within("the owner's hang-up", owner_call.hangup()).await;
    assert_eq!(ended.reason, CallEndReason::Completed);
    within("the owner's stream ends", listening)
        .await
        .expect("the listener ran");
    let ended = wait_events(&events_path, "call_ended", 1).await;
    assert_eq!(
        named(&ended, "call_ended").next().unwrap()["call_id"],
        call_id
    );
    let metrics = agent
        .adapter
        .as_ref()
        .expect("the adapter's setup")
        .metrics
        .snapshot();
    step(&format!(
        "the router: {} call(s), {} frames to the adapter, {} from it, {} dropped, {} speaks; \
         final → speak (backend included) last {} ms, max {} ms; speak → audio max {} ms",
        metrics.calls,
        metrics.frames_to_adapter,
        metrics.frames_from_adapter,
        metrics.frames_dropped,
        metrics.speaks,
        metrics.final_to_speak.last.as_millis(),
        metrics.final_to_speak.max.as_millis(),
        metrics.speak_to_audio.max.as_millis(),
    ));
    assert_eq!(metrics.calls, 1);
    assert!(metrics.frames_from_adapter > 0 && metrics.speaks > 0);

    let backend = agent.backend.clone();
    agent.shut_down().await;
    assert!(backend.calls().iter().any(|call| call == "stop"));
    wait_events(&events_path, "closed", 1).await;
    assert!(
        log_since(mark)
            .iter()
            .any(|line| line["fields"]["event"] == "adapter_metrics_total"),
        "the totals are logged at the end"
    );
    assert_content_free(
        mark,
        &[
            "run the tests",
            "stop, use the other file",
            "All tests pass",
            "keeps going",
            "cargo test",
            "Approved.",
        ],
    );
    drop(owner_call);
    owner.client.close().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bridge_connects_recovers_and_falls_back() {
    if !enabled() {
        return;
    }
    bridge_log();
    testkit();
    let started = Instant::now();
    let (stack, containers) = stack_for(6).await;
    step(&format!(
        "the stack is up in {} ms",
        started.elapsed().as_millis()
    ));
    tokio::time::timeout(Duration::from_secs(420), recovery(&stack))
        .await
        .expect("the scenario finishes within 420 s");
    stack.stop().await;
    drop(containers);
    step(&format!("done in {} ms", started.elapsed().as_millis()));
}

async fn recovery(stack: &Stack) {
    let mark = log_mark();
    let dir = scratch();
    let accounts = push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), 1)
        .await
        .expect("a verified account");
    let mut owner = Person::start(stack, &dir, "owner", &accounts[0]).await;
    let mut line = Line2::new(owner.calls.take().expect("the call events are free"));
    let token = access_token(&reqwest::Client::new(), stack, &accounts[0]).await;

    connected(stack, &token, &owner, &line).await;
    crash(stack, &token, &owner, &line).await;
    no_adapter(stack, &token, &owner, &mut line).await;
    refused_format(stack, &token, &owner, &mut line).await;
    assert_content_free(mark, &["run the tests", "All tests pass"]);

    drop(line);
    owner.client.close().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// An agent of the owner's in a directory of its own.
async fn agent(stack: &Stack, token: &str, owner: &Person, handle: &str, speech: Speech) -> Agent {
    let code = new_agent(stack, token, handle).await;
    let dir = scratch();
    let agent = Agent::start_with(stack, &dir, &code, CallPolicy::default(), speech).await;
    owner.watch(agent.user);
    within("the backend starts", agent.backend.wait_for("start", 1)).await;
    agent
}

/// The owner calls the agent and hears the greeting.
async fn call_and_greet(owner: &Person, line: &Line2, agent: UserId) -> (Arc<MediaCall>, Ear) {
    let placed = Instant::now();
    let (owner_call, incoming) = call_with(owner, line, agent).await;
    let ear = Ear::default();
    ear.listen(incoming);
    within("the owner's media connects", owner_call.connected())
        .await
        .expect("the call connects");
    ear.voice("the greeting", placed).await;
    ear.quiet("the end of the greeting", placed).await;
    (owner_call, ear)
}

/// The owner asks; the prompt (the `n`th) reaches the backend; the reply is heard.
async fn ask(owner_call: &MediaCall, ear: &Ear, agent: &Agent, n: usize) {
    say(owner_call, &fixture("prompt").audio()).await;
    within(
        "the prompt",
        agent.backend.wait_for("prompt run the tests", n),
    )
    .await;
    let replied = Instant::now();
    agent.backend.turn_started(TurnSource::Prompt);
    agent.backend.text("All tests pass.\n");
    agent.backend.turn_ended(false);
    ear.voice("the reply", replied).await;
    ear.quiet("the end of the reply", replied).await;
}

async fn hang_up(owner_call: Arc<MediaCall>, line: &Line2) {
    within("the owner's hang-up", owner_call.hangup()).await;
    line.clear();
}

/// `calls.adapter.connect`: the testkit already listens; the bridge connects to it.
async fn connected(stack: &Stack, token: &str, owner: &Person, line: &Line2) {
    let dir = scratch();
    let events_path = dir.join("adapter-events.jsonl");
    let testkit = Listening::start(&dir, &hearing(), &events_path).await;
    let agent = agent(
        stack,
        token,
        owner,
        "connected_voice",
        Speech::Adapter(testkit.config.clone()),
    )
    .await;
    advertised(&agent).await;
    wait_events(&events_path, "connected", 1).await;
    let (owner_call, ear) = call_and_greet(owner, line, agent.user).await;
    wait_events(&events_path, "call_started", 1).await;
    ask(&owner_call, &ear, &agent, 1).await;
    step("connect mode: the prompt reached the backend and the reply was heard");
    hang_up(owner_call, line).await;
    wait_events(&events_path, "call_ended", 1).await;
    agent.shut_down().await;
    wait_events(&events_path, "closed", 1).await;
    drop(testkit);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The adapter crashes mid-call; the supervisor starts it again, which takes the call.
async fn crash(stack: &Stack, token: &str, owner: &Person, line: &Line2) {
    let dir = scratch();
    let events_path = dir.join("adapter-events.jsonl");
    let script = Script {
        crash_after_frames: Some(25),
        crash_once_marker: Some(dir.join("crashed")),
        ..hearing()
    };
    let adapter = spawned(&dir, &script, &events_path);
    let agent = agent(
        stack,
        token,
        owner,
        "crashing_voice",
        Speech::Adapter(adapter),
    )
    .await;
    advertised(&agent).await;
    let seen = owner.message_count();
    let (owner_call, ear) = call_and_greet(owner, line, agent.user).await;
    // The owner's first words reach the adapter, which crashes on them.
    say(&owner_call, &fixture("prompt").audio()).await;
    let crashed = wait_events(&events_path, "crashed", 1).await;
    assert_eq!(named(&crashed, "crashed").next().unwrap()["frames"], 25);
    owner
        .message("the adapter's loss", seen, |seen| {
            param(seen.body(), "text").as_deref() == Some("Voice adapter stopped; reconnecting…")
        })
        .await;
    step("the adapter crashed mid-call; the owner was told");
    owner
        .message("the adapter's return", seen, |seen| {
            param(seen.body(), "text").as_deref() == Some("Voice adapter is back.")
        })
        .await;
    assert_eq!(owner_call.state(), MediaState::Connected, "the call stays");
    let started = wait_events(&events_path, "call_started", 2).await;
    let ids: Vec<&Value> = named(&started, "call_started")
        .map(|event| &event["call_id"])
        .collect();
    assert_eq!(ids[0], ids[1], "the same call, started again");
    step("the restarted adapter took the call again");
    assert!(
        !agent
            .backend
            .calls()
            .iter()
            .any(|call| call.starts_with("prompt")),
        "the words the adapter crashed on were lost"
    );
    ask(&owner_call, &ear, &agent, 1).await;
    step("after the restart the owner's words became a prompt");
    let restarts = agent.adapter.as_ref().unwrap().supervisor.restarts();
    assert_eq!(restarts, 1);
    hang_up(owner_call, line).await;
    agent.shut_down().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// The owner calls once and the call ends `Unreachable` (calls are not advertised).
async fn unreachable(owner: &Person, line: &mut Line2, agent: &Agent) {
    let servers = owner.client.calls().ice_servers().await.unwrap();
    let call = MediaCall::start(
        &owner.client.calls(),
        &servers,
        Role::Caller { peer: agent.user },
        MediaOptions::default(),
    )
    .await
    .expect("the call starts");
    let call = Arc::new(call);
    line.set(call.clone());
    let ended = within("the call ends", call.on_ended()).await;
    assert_eq!(ended.reason, CallEndReason::Unreachable);
    assert!(agent.phone.answered().is_empty(), "never answered");
    line.clear();
}

/// The adapter's state is a backoff; its reason.
async fn backing_off(agent: &Agent) -> String {
    let mut state = agent.adapter.as_ref().unwrap().supervisor.state();
    let state = within(
        "a backoff",
        state.wait_for(|state| matches!(state, AdapterState::Backoff { .. })),
    )
    .await
    .expect("the supervisor runs")
    .clone();
    match state {
        AdapterState::Backoff { reason, .. } => reason,
        _ => unreachable!(),
    }
}

/// `calls.adapter` names a program that does not exist: calls are never advertised.
async fn no_adapter(stack: &Stack, token: &str, owner: &Person, line: &mut Line2) {
    let dir = scratch();
    let missing = dir.join(format!(
        "no-such-voice-adapter{}",
        std::env::consts::EXE_SUFFIX
    ));
    let adapter = AdapterConfig::Spawn(SpawnConfig {
        command: vec![missing.to_string_lossy().into_owned()],
        stderr: StderrMode::Discard,
        env_allow: Vec::new(),
    });
    let mark = log_mark();
    let agent = agent(
        stack,
        token,
        owner,
        "missing_voice",
        Speech::Adapter(adapter),
    )
    .await;
    let reason = backing_off(&agent).await;
    step(&format!("no adapter: backing off ({reason})"));
    // Longer than an adapter must listen before calls are advertised.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!agent.client.calls_advertised());
    unreachable(owner, line, &agent).await;
    step("no adapter: the owner's call was unreachable");
    assert!(
        log_since(mark)
            .iter()
            .any(|line| line["fields"]["event"] == "voice_adapter_failed"),
        "the failure is logged"
    );
    agent.shut_down().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// The adapter refuses the bridge's audio format at every start: one warning, then the
/// retries at debug; calls are never advertised.
async fn refused_format(stack: &Stack, token: &str, owner: &Person, line: &mut Line2) {
    let dir = scratch();
    let events_path = dir.join("adapter-events.jsonl");
    let script = Script {
        refuse_format: true,
        ..hearing()
    };
    let adapter = spawned(&dir, &script, &events_path);
    let mark = log_mark();
    let agent = agent(
        stack,
        token,
        owner,
        "refusing_voice",
        Speech::Adapter(adapter),
    )
    .await;
    let reason = backing_off(&agent).await;
    assert!(reason.contains("unsupported_format"), "{reason}");
    // Starts at 0 s, 1 s and 3 s (the backoff doubles from 1 s).
    wait_events(&events_path, "error_sent", 3).await;
    until("the third failure logged", || {
        let (warnings, debug) = failures(mark, "unsupported_format");
        (warnings + debug >= 3).then_some(())
    })
    .await;
    let (warnings, debug) = failures(mark, "unsupported_format");
    step(&format!(
        "refused format: {warnings} warning(s), {debug} debug line(s) for unsupported_format"
    ));
    assert_eq!(warnings, 1, "one warning for the repeated reason");
    assert!(!agent.client.calls_advertised());
    unreachable(owner, line, &agent).await;
    step("refused format: the owner's call was unreachable");
    agent.shut_down().await;
    let _ = std::fs::remove_dir_all(&dir);
}
