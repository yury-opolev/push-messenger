//! The bridge end to end: the core with a test backend or the Coda backend over a scripted
//! `coda serve`, and a test messenger.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use push_bridge::backend::{
    Answer, Backend, BackendEvent, ErrorKind, PermissionMode, RequestKind, SteerOutcome, TurnSource,
};
use push_bridge::coda::CodaBackend;
use push_bridge::coda::testing::{FakeCoda, FakeLauncher};
use push_bridge::core::{Bridge, BridgeError, BridgeOptions, PRIVATE};
use push_bridge::folders;
use push_bridge::messenger::{Call, TestMessenger, param, template};
use push_bridge::state::StateFile;
use push_bridge::test_backend::{TestBackend, error};
use push_sdk::{ActivityState, ClientMsgId, UserId};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// A scratch folder with `root/work`, `root/other` and `outside`, removed on drop. Its
/// name is short so that masking leaves its paths alone.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = std::env::temp_dir().join(format!("pbt-{}", &id[..10]));
        for folder in ["root/work", "root/other", "outside"] {
            std::fs::create_dir_all(path.join(folder)).unwrap();
        }
        Self(path)
    }

    fn real(&self, folder: &str) -> PathBuf {
        folders::resolve(folder, &self.0, std::slice::from_ref(&self.0)).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    messenger: Arc<TestMessenger>,
    state: Arc<StateFile>,
    root: PathBuf,
    work: PathBuf,
    outside: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    run: Option<JoinHandle<Result<(), BridgeError>>>,
    _scratch: Scratch,
}

impl Harness {
    /// Runs the bridge over `backend`, which reports through `events`.
    fn run(backend: Arc<dyn Backend>, events: mpsc::UnboundedReceiver<BackendEvent>) -> Self {
        let scratch = Scratch::new();
        let (root, work, outside) = (
            scratch.real("root"),
            scratch.real("root/work"),
            scratch.real("outside"),
        );
        let state = Arc::new(StateFile::load(&scratch.0.join("state.json")).unwrap());
        let (messenger, messenger_events) = TestMessenger::new();
        let bridge = Bridge::new(
            Arc::clone(&messenger),
            backend,
            events,
            messenger_events,
            Arc::clone(&state),
            BridgeOptions {
                roots: vec![root.clone()],
                default_folder: work.clone(),
                default_mode: PermissionMode::Default,
            },
        );
        let (stop, stopped) = oneshot::channel::<()>();
        let run = tokio::spawn(bridge.run(async {
            let _ = stopped.await;
        }));
        Self {
            messenger,
            state,
            root,
            work,
            outside,
            stop: Some(stop),
            run: Some(run),
            _scratch: scratch,
        }
    }

    /// With a [`TestBackend`], once it started.
    async fn with_test_backend() -> (Self, Arc<TestBackend>) {
        let (backend, events) = TestBackend::channel();
        let harness = Self::run(Arc::clone(&backend) as Arc<dyn Backend>, events);
        backend.wait_for("start", 1).await;
        settle().await;
        (harness, backend)
    }

    async fn stop(&mut self) -> Result<(), BridgeError> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.run.take().expect("running").await.unwrap()
    }

    fn replies(&self) -> Vec<String> {
        self.messenger.reply_texts()
    }

    /// The cards sent: `(id, template)`.
    fn cards(&self) -> Vec<(ClientMsgId, String)> {
        self.messenger
            .sent_rich()
            .into_iter()
            .map(|(id, body)| (id, template(&body)))
            .filter(|(_, tpl)| tpl != "agent.reply@1")
            .collect()
    }

    /// The `count`th card, once sent.
    async fn card(&self, count: usize) -> (ClientMsgId, String) {
        self.messenger
            .wait_until("a card", |_| self.cards().len() >= count)
            .await;
        self.cards().swap_remove(count - 1)
    }

    /// The latest reply containing `text`, once sent.
    async fn reply_containing(&self, text: &str) -> String {
        self.messenger
            .wait_until(text, |_| {
                self.replies().iter().any(|reply| reply.contains(text))
            })
            .await;
        self.replies()
            .into_iter()
            .rfind(|reply| reply.contains(text))
            .unwrap()
    }

    async fn activity(&self, state: ActivityState) {
        self.messenger
            .wait_until("an activity", |_| {
                self.messenger
                    .activities()
                    .last()
                    .is_some_and(|(last, _)| *last == state)
            })
            .await;
    }

    /// The activity states sent, repeats folded.
    fn activity_states(&self) -> Vec<ActivityState> {
        let mut states: Vec<ActivityState> = Vec::new();
        for (state, _) in self.messenger.activities() {
            if states.last() != Some(&state) {
                states.push(state);
            }
        }
        states
    }

    /// The text of the last edit.
    fn last_edit(&self) -> String {
        let (_, body) = self.messenger.edits().pop().expect("an edit");
        param(&body, "text").unwrap()
    }
}

/// Lets the bridge handle everything waiting (paused time moves on only when every
/// task is idle).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

async fn within<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("timed out")
}

fn permission() -> RequestKind {
    RequestKind::Permission {
        tool: "Bash".into(),
        preview: "cargo test".into(),
    }
}

fn start_call(folder: &Path, session: &str, mode: &str) -> String {
    format!(
        "start folder={} session={session} mode={mode}",
        folder.display()
    )
}

fn count(calls: &[String], prefix: &str) -> usize {
    calls.iter().filter(|call| call.starts_with(prefix)).count()
}

/// 1. A prompt gives a reply that is edited; a permission request gives an approval card
/// the owner answers; the turn ends idle. An engine-started turn and an agent message
/// are shown.
#[tokio::test(start_paused = true)]
async fn a_turn_with_an_approval_end_to_end() {
    let (h, backend) = Harness::with_test_backend().await;
    h.messenger.owner_says("fix the bug");
    backend.wait_for("prompt fix the bug", 1).await;

    backend.turn_started(TurnSource::Prompt);
    backend.emit(BackendEvent::ThinkingDelta("hmm".into()));
    backend.text("Looking");
    h.messenger
        .wait_until("the reply", |_| h.messenger.sent_rich().len() == 1)
        .await;
    backend.emit(BackendEvent::ToolStarted {
        id: "c1".into(),
        tool: "read_file".into(),
        target: "src/main.rs".into(),
        task: None,
    });
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!h.messenger.edits().is_empty(), "edited after 2 s");

    let answer = backend.request(permission());
    let (card, tpl) = h.card(1).await;
    assert_eq!(tpl, "agent.approval@1");
    h.activity(ActivityState::Waiting).await;
    h.messenger.answer(card, &["approve"]);
    assert_eq!(answer.await, Ok(Answer::Permission(true)));
    h.activity(ActivityState::Working).await;

    backend.emit(BackendEvent::ToolFinished {
        id: "c1".into(),
        failed: false,
    });
    backend.text(" — fixed.");
    backend.turn_ended(false);
    h.activity(ActivityState::Idle).await;
    assert_eq!(h.last_edit(), "Looking — fixed.");
    assert_eq!(
        h.activity_states(),
        [
            ActivityState::Working,
            ActivityState::Waiting,
            ActivityState::Working,
            ActivityState::Idle
        ]
    );
    assert!(
        h.messenger
            .activities()
            .contains(&(ActivityState::Working, Some("read_file src/main.rs".into())))
    );

    backend.turn_started(TurnSource::Engine);
    backend.text("The background build passed.");
    backend.turn_ended(false);
    h.messenger
        .wait_until("the engine's reply", |_| h.messenger.sent_rich().len() == 3)
        .await;
    h.activity(ActivityState::Idle).await;
    assert_eq!(h.last_edit(), "The background build passed.");

    backend.emit(BackendEvent::AgentMessage {
        cursor: 7,
        label: "Deploy".into(),
        text: "Deployed to staging.".into(),
    });
    h.reply_containing("Deployed to staging.").await;
    let (_, body) = h.messenger.sent_rich().pop().unwrap();
    assert_eq!(param(&body, "title").as_deref(), Some("Deploy"));
    assert_eq!(h.state.get().agent_message_cursor, Some(7));
    assert_eq!(count(&backend.calls(), "prompt"), 1);
}

/// 2. The same flow against the Coda backend and a scripted `coda serve`.
#[tokio::test]
async fn a_turn_with_an_approval_through_coda() {
    let (launcher, mut fakes) = FakeLauncher::new();
    let (sender, events) = mpsc::unbounded_channel();
    let backend = Arc::new(CodaBackend::with_launcher(launcher.clone(), sender));
    let mut h = Harness::run(backend, events);
    let mut fake: FakeCoda = within(fakes.recv()).await.unwrap();
    within(fake.handshake("s-1")).await;
    assert_eq!(
        launcher.spawns(),
        [(h.work.clone(), PermissionMode::Default)]
    );

    h.messenger.owner_says("fix the bug");
    let (prompt, params) = within(fake.expect("session/prompt")).await;
    assert_eq!(params, json!({"text": "fix the bug"}));
    fake.notify(
        "event/activity",
        json!({"turnId": "t1", "phase": "preparing", "phaseSince": "x"}),
    )
    .await;
    fake.notify("event/assistantText", json!({"delta": "Looking"}))
        .await;
    within(
        h.messenger
            .wait_until("the reply", |_| h.messenger.sent_rich().len() == 1),
    )
    .await;

    let request = fake
        .send_request(
            "request/permission",
            json!({"toolName": "run_command", "inputPreview": "cargo test", "requestId": "r1"}),
        )
        .await;
    let (card, tpl) = within(h.card(1)).await;
    assert_eq!(tpl, "agent.approval@1");
    within(h.activity(ActivityState::Waiting)).await;
    h.messenger.answer(card, &["approve"]);
    let response = within(fake.response(&request)).await;
    assert_eq!(response["result"], json!({"allow": true}));

    fake.notify(
        "event/turnEnded",
        json!({"turnId": "t1", "interrupted": false}),
    )
    .await;
    fake.respond(&prompt, json!({"ok": true, "interrupted": false}))
        .await;
    within(h.activity(ActivityState::Idle)).await;
    assert_eq!(h.last_edit(), "Looking");
    assert_eq!(h.state.get().sessions.values().collect::<Vec<_>>(), ["s-1"]);
    h.run.take().unwrap().abort();
}

/// 3. A stranger hears "This agent is private." at most once an hour; nothing reaches
/// the backend.
#[tokio::test(start_paused = true)]
async fn a_stranger_is_told_once_an_hour() {
    let (h, backend) = Harness::with_test_backend().await;
    let stranger = UserId::new();
    let conversation = h.messenger.conversation_with(stranger);
    h.messenger.says(stranger, "run rm -rf /");
    h.messenger
        .wait_until("the reply", |_| h.messenger.texts().len() == 1)
        .await;
    h.messenger.says(stranger, "please");
    h.messenger.owner_says("ping");
    backend.wait_for("prompt ping", 1).await;
    assert_eq!(h.messenger.texts(), [(conversation, PRIVATE.to_owned())]);
    tokio::time::sleep(Duration::from_secs(61 * 60)).await;
    h.messenger.says(stranger, "again");
    h.messenger
        .wait_until("the second reply", |_| h.messenger.texts().len() == 2)
        .await;
    let calls = backend.calls();
    assert_eq!(count(&calls, "prompt") + count(&calls, "steer"), 1);
}

/// 4. Unanswered cards time out after 10 minutes: a permission gets `{allow:false}`, a
/// question is declined with -32800, and the owner reads "No answer; skipped".
#[tokio::test(start_paused = true)]
async fn unanswered_cards_time_out() {
    let (launcher, mut fakes) = FakeLauncher::new();
    let (sender, events) = mpsc::unbounded_channel();
    let backend = Arc::new(CodaBackend::with_launcher(launcher, sender));
    let mut h = Harness::run(backend, events);
    let mut fake = fakes.recv().await.unwrap();
    fake.handshake("s-1").await;

    let permission = fake
        .send_request(
            "request/permission",
            json!({"toolName": "write_file", "inputPreview": "a.txt"}),
        )
        .await;
    h.card(1).await;
    let shown = tokio::time::Instant::now();
    let response = fake.response(&permission).await;
    assert_eq!(response["result"], json!({"allow": false}));
    assert!(shown.elapsed() >= Duration::from_secs(599));
    h.reply_containing("No answer; skipped").await;

    let question = fake
        .send_request(
            "request/question",
            json!({"question": "Which?", "options": ["A", "B"], "multiSelect": false, "allowFreeText": true}),
        )
        .await;
    h.card(2).await;
    let response = fake.response(&question).await;
    assert_eq!(response["error"]["code"], -32800);
    h.messenger
        .wait_until("two skips", |_| {
            h.replies()
                .iter()
                .filter(|reply| reply.as_str() == "No answer; skipped")
                .count()
                == 2
        })
        .await;
    h.activity(ActivityState::Idle).await;
    h.run.take().unwrap().abort();
}

/// 5. Text during a turn steers it; a busy prompt steers too; a steer refused because the
/// turn is ending is sent as a prompt once the turn ended.
#[tokio::test(start_paused = true)]
async fn busy_text_steers_or_waits_for_the_turn_to_end() {
    let (h, backend) = Harness::with_test_backend().await;
    backend.turn_started(TurnSource::Prompt);
    h.messenger.owner_says("also check the tests");
    backend.wait_for("steer also check the tests", 1).await;

    backend.turn_ended(false);
    settle().await;
    backend.next_prompt(Err(error("session/prompt", ErrorKind::Busy)));
    backend.next_steer(Ok(SteerOutcome::TurnEnding));
    h.messenger.owner_says("and the docs");
    backend.wait_for("steer and the docs", 1).await;
    backend.turn_started(TurnSource::Engine);
    settle().await;
    assert_eq!(
        count(&backend.calls(), "prompt and the docs"),
        1,
        "waits for the turn"
    );
    backend.turn_ended(false);
    backend.wait_for("prompt and the docs", 2).await;
    assert_eq!(
        backend.calls()[1..],
        [
            "steer also check the tests",
            "prompt and the docs",
            "steer and the docs",
            "prompt and the docs"
        ]
    );
}

/// 6. The backend exits: the reply says so, open cards are declined, and it restarts with
/// backoff (1, 2, 4 s) resuming the stored session, then says what was lost.
#[tokio::test(start_paused = true)]
async fn an_exited_backend_restarts_with_backoff() {
    let (h, backend) = Harness::with_test_backend().await;
    backend.turn_started(TurnSource::Prompt);
    backend.text("Working on it");
    let answer = backend.request(permission());
    h.card(1).await;
    backend.fail_next_start(ErrorKind::NotRunning);
    backend.fail_next_start(ErrorKind::NotRunning);
    let exited_at = tokio::time::Instant::now();
    backend.emit(BackendEvent::Exited {
        reason: "exit code 1".into(),
    });
    h.reply_containing("Declined: Test stopped.").await;
    assert_eq!(h.last_edit(), "Working on it\n(Test stopped; restarting)");
    drop(answer);

    h.reply_containing(
        "Test restarted. Undelivered background messages and in-memory schedules were lost.",
    )
    .await;
    let starts: Vec<_> = backend
        .timed_calls()
        .into_iter()
        .filter(|(_, call)| call.starts_with("start"))
        .collect();
    assert_eq!(starts.len(), 4);
    let offsets: Vec<u128> = starts[1..]
        .iter()
        .map(|(at, _)| (*at - exited_at).as_millis())
        .collect();
    for (offset, expected) in offsets.iter().zip([1_000, 3_000, 7_000]) {
        assert!((expected..expected + 100).contains(offset), "{offsets:?}");
    }
    for (_, call) in &starts[1..] {
        assert_eq!(*call, start_call(&h.work, "session-1", "default"));
    }
    h.activity(ActivityState::Idle).await;
}

/// 7. `/cd` outside the roots is refused with the roots listed; `/mode bypass` needs its
/// card, shows in `/status`, and `/new` resets it.
#[tokio::test(start_paused = true)]
async fn folder_and_approval_commands() {
    let (mut h, backend) = Harness::with_test_backend().await;
    h.messenger
        .owner_says(&format!("/cd {}", h.outside.display()));
    let refused = h.reply_containing("Cannot switch to").await;
    assert!(refused.contains("outside the allowed roots"), "{refused}");
    assert!(
        refused.contains(&format!("Allowed roots: {}", h.root.display())),
        "{refused}"
    );
    h.messenger.owner_says("/cd ../../outside");
    h.messenger
        .wait_until("a second refusal", |_| {
            h.replies()
                .iter()
                .filter(|reply| reply.contains("Cannot switch to"))
                .count()
                == 2
        })
        .await;
    assert_eq!(count(&backend.calls(), "start"), 1);

    h.messenger.owner_says("/mode bypass");
    let (card, _) = h.card(1).await;
    let body = h.messenger.sent_rich().pop().unwrap().1;
    assert_eq!(
        param(&body, "title").as_deref(),
        Some("Turn off approvals?")
    );
    h.messenger.answer(card, &["reject"]);
    h.reply_containing("Approvals stay on.").await;
    assert_eq!(count(&backend.calls(), "command"), 0);

    h.messenger.owner_says("/mode bypass");
    let (card, _) = h.card(2).await;
    h.messenger.answer(card, &["approve"]);
    h.reply_containing("Approvals are off until /new.").await;
    assert!(
        backend
            .calls()
            .contains(&"command SetPermissionMode(bypassPermissions)".to_owned())
    );
    h.messenger.owner_says("/status");
    let status = h.reply_containing("Permission mode:").await;
    assert!(
        status.contains("Permission mode: bypassPermissions (approvals are OFF until /new)"),
        "{status}"
    );

    h.messenger.owner_says("/new");
    h.reply_containing("New session session-2").await;
    assert!(
        backend
            .calls()
            .contains(&start_call(&h.work, "None", "default"))
    );
    h.messenger.owner_says("/status");
    h.reply_containing("Permission mode: default").await;
    assert_eq!(
        h.state.get().sessions.values().last().map(String::as_str),
        Some("session-2")
    );
    assert!(
        h.messenger
            .calls()
            .iter()
            .all(|call| !matches!(call, Call::SendText { .. }))
    );
    assert_eq!(h.stop().await, Ok(()));
}
